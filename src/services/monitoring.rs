use std::collections::{HashMap, HashSet};

use chrono::{NaiveDate, Utc};
use sqlx::SqlitePool;

use crate::{
    models::{config, local_metadata, metadata_cache, monitoring::{self, EpisodeMonitorState, MonitorMode}, series},
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
    let total = effective_episode_count(db, &row).await;
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
        .unwrap_or_default();

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
    let total = effective_episode_count(db, tracked).await;
    let episode_numbers: Vec<i32> = (1..=total).collect();
    let existing = monitoring::get_series_states(db, tracked.id)
        .await
        .map_err(|e| e.to_string())?;

    // Recompute from scratch whenever the row count diverges from the
    // effective episode count. `recompute_series_monitoring` uses a single
    // transaction to DELETE + INSERT, which avoids the 1157-round-trip cost
    // that a naive insert loop would pay for something like One Piece.
    if existing.len() != episode_numbers.len() {
        return recompute_series_monitoring(db, tracked.id).await;
    }

    let monitored_count = existing.iter().filter(|s| s.monitored).count();
    Ok(MonitoringSummary {
        mode: tracked.monitor_mode_enum(),
        monitored_count,
        total_count: episode_numbers.len(),
    })
}

/// Returns the effective episode count for a tracked series, preferring the
/// AniList-reported `episodes` field and falling back through cached
/// metadata (`next_airing_episode - 1`) and the cached episode map. This
/// matters for currently-airing long-runners like One Piece where AniList
/// reports `episodes: null` — without the fallback, monitoring recomputes
/// against zero episodes and the per-episode Monitor buttons have nothing
/// to toggle.
async fn effective_episode_count(db: &SqlitePool, row: &series::Series) -> i32 {
    if let Some(n) = row.episodes {
        if n > 0 {
            return n;
        }
    }
    if let Ok(Some(cached)) = metadata_cache::get_by_series_id(db, row.id).await {
        let n = cached.detail.effective_episode_count();
        if n > 0 {
            return n;
        }
    }
    if let Ok(map) = local_metadata::get_episode_map_for_series(db, row.id).await {
        if let Some(max) = map.keys().copied().max() {
            return max;
        }
    }
    0
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
