use std::collections::{HashMap, HashSet};

use chrono::{NaiveDate, Utc};
use sqlx::SqlitePool;

use crate::{
    models::{config, local_metadata, monitoring::{self, EpisodeMonitorState, MonitorMode}, series},
    services::{jikan, media},
};

#[derive(Debug, Clone)]
pub struct MonitoringSummary {
    pub mode: MonitorMode,
    pub monitored_count: usize,
    pub total_count: usize,
}

pub async fn apply_monitor_mode(db: &SqlitePool, series_id: i64, mode: MonitorMode) -> Result<MonitoringSummary, String> {
    series::update_monitor_mode(db, series_id, mode.as_str())
        .await
        .map_err(|e| e.to_string())?;
    recompute_series_monitoring(db, series_id).await
}

pub async fn recompute_series_monitoring(db: &SqlitePool, series_id: i64) -> Result<MonitoringSummary, String> {
    let Some(row) = series::get_by_id(db, series_id).await.map_err(|e| e.to_string())? else {
        return Err("Series not found".to_string());
    };

    let mode = row.monitor_mode_enum();
    let total = row.episodes.unwrap_or(0).max(0);
    let episode_numbers: Vec<i32> = (1..=total).collect();

    if episode_numbers.is_empty() {
        monitoring::replace_series_states(db, row.id, &[])
            .await
            .map_err(|e| e.to_string())?;
        return Ok(MonitoringSummary { mode, monitored_count: 0, total_count: 0 });
    }

    let cfg = config::get_config(db)
        .await
        .ok()
        .flatten()
        .unwrap_or(crate::models::config::Config {
            qbit_url: String::new(),
            qbit_user: String::new(),
            qbit_pass: String::new(),
            qbit_category: String::new(),
            qbit_download_path: String::new(),
            jellyfin_url: String::new(),
            jellyfin_api_key: String::new(),
            preferred_groups: String::new(),
            blocked_groups: String::new(),
            preferred_resolution: "1080".to_string(),
            quality_profile: "web_1080".to_string(),
            quality_cutoff: "bd_1080".to_string(),
            finished_series_quality: "prefer_bd".to_string(),
            media_root: String::new(),
            title_language: "english".to_string(),
            force_mal_fallback: false,
            rss_enabled: false,
            rss_interval_minutes: 5,
            force_kitsu_fallback: false,
            post_processing_enabled: false,
            post_processing_mode: "hardlink".to_string(),
            auto_grab_on_add: true,
            prefer_subs: true,
            allow_non_english: false,
        });

    let disk_files = media::scan_series_folder(&cfg.media_root, &row.folder_name);
    let existing_eps: HashSet<i32> = disk_files.iter().map(|f| f.episode_number).collect();
    let episode_info = load_episode_info(db, &row).await;
    let monitored_eps = resolve_monitored_episodes(&row, &episode_numbers, &existing_eps, &episode_info, mode);

    let states: Vec<EpisodeMonitorState> = episode_numbers
        .iter()
        .map(|ep| EpisodeMonitorState {
            episode_number: *ep,
            monitored: monitored_eps.contains(ep),
        })
        .collect();

    monitoring::replace_series_states(db, row.id, &states)
        .await
        .map_err(|e| e.to_string())?;

    Ok(MonitoringSummary {
        mode,
        monitored_count: monitored_eps.len(),
        total_count: episode_numbers.len(),
    })
}

pub async fn ensure_series_monitoring_rows(db: &SqlitePool, tracked: &series::Series) -> Result<MonitoringSummary, String> {
    let total = tracked.episodes.unwrap_or(0).max(0);
    let episode_numbers: Vec<i32> = (1..=total).collect();
    let existing = monitoring::get_series_states(db, tracked.id)
        .await
        .map_err(|e| e.to_string())?;

    if existing.len() != episode_numbers.len() {
        monitoring::ensure_series_rows(db, tracked.id, &episode_numbers)
            .await
            .map_err(|e| e.to_string())?;
        return recompute_series_monitoring(db, tracked.id).await;
    }

    let monitored_count = existing.iter().filter(|s| s.monitored).count();
    Ok(MonitoringSummary {
        mode: tracked.monitor_mode_enum(),
        monitored_count,
        total_count: episode_numbers.len(),
    })
}

async fn load_episode_info(db: &SqlitePool, row: &series::Series) -> HashMap<i32, jikan::EpisodeInfo> {
    if let Ok(cached) = local_metadata::get_episode_map_for_series(db, row.id).await {
        if !cached.is_empty() {
            return cached
                .into_iter()
                .map(|(num, ep)| (num, jikan::EpisodeInfo { title: ep.title, aired: ep.aired }))
                .collect();
        }
    }
    let Some(mal_id) = row.mal_id else {
        return HashMap::new();
    };
    jikan::fetch_episode_titles(db, mal_id).await
}

fn resolve_monitored_episodes(
    row: &series::Series,
    episode_numbers: &[i32],
    existing_eps: &HashSet<i32>,
    episode_info: &HashMap<i32, jikan::EpisodeInfo>,
    mode: MonitorMode,
) -> HashSet<i32> {
    let today = Utc::now().date_naive();
    let mut latest_aired_known = 0;

    for ep in episode_numbers {
        if let Some(info) = episode_info.get(ep) {
            if let Some(aired) = parse_aired_date(&info.aired) {
                if aired <= today {
                    latest_aired_known = latest_aired_known.max(*ep);
                }
            }
        }
    }

    let max_existing = existing_eps.iter().copied().max().unwrap_or(0);
    let is_finished = matches!(row.status.trim().to_ascii_uppercase().as_str(), "FINISHED" | "FINISHED_AIRING" | "CANCELLED");

    episode_numbers
        .iter()
        .copied()
        .filter(|ep| match mode {
            MonitorMode::All => true,
            MonitorMode::None => false,
            MonitorMode::Existing => existing_eps.contains(ep),
            MonitorMode::Missing => {
                if existing_eps.contains(ep) {
                    return false;
                }
                if is_finished {
                    return true;
                }
                if let Some(info) = episode_info.get(ep) {
                    if let Some(aired) = parse_aired_date(&info.aired) {
                        return aired <= today;
                    }
                }
                *ep <= latest_aired_known && latest_aired_known > 0
            }
            MonitorMode::Future => {
                if existing_eps.contains(ep) {
                    return false;
                }
                if let Some(info) = episode_info.get(ep) {
                    if let Some(aired) = parse_aired_date(&info.aired) {
                        return aired > today;
                    }
                }
                if latest_aired_known > 0 {
                    *ep > latest_aired_known
                } else {
                    *ep > max_existing
                }
            }
        })
        .collect()
}

fn parse_aired_date(value: &str) -> Option<NaiveDate> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let date_part = trimmed.split('T').next().unwrap_or(trimmed);
    NaiveDate::parse_from_str(date_part, "%Y-%m-%d").ok()
}
