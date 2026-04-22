//! Construction helpers for SonarrSeries payloads: resolving an
//! AniList entry from a TVDB ID, fetching AL detail, and building
//! SonarrSeries DTOs from either a live search result or a tracked
//! series row.

use axum::{Json, http::StatusCode};

use crate::AppState;
use crate::models::{config, monitoring, series};
use crate::services::{anibridge, anilist, media};

use super::types::{
    SonarrImage, SonarrRatings, SonarrSeason, SonarrSeasonStats, SonarrSeries, SonarrStatistics,
};

// ── Helpers ────────────────────────────────────────────────────────────────

/// Look up anime by external ID (TVDB or TMDB). Tries TVDB index first since
/// Sonarr/Seerr sends real TVDB IDs, then falls back to TMDB index.
///
/// Returns a SINGLE series with multiple seasons (one per AniList entry) when
/// the TVDB ID maps to multiple anibridge seasons. This matches how real Sonarr
/// returns multi-season shows, allowing Seerr to request specific seasons.
pub(super) async fn lookup_by_external_id(
    _state: &AppState,
    cfg: &config::Config,
    tvdb_id: i64,
) -> Result<Json<Vec<SonarrSeries>>, (StatusCode, String)> {
    anibridge::ensure_loaded().await;

    let mut season_entries = anibridge::lookup_tvdb_seasons(tvdb_id).await;
    if season_entries.is_empty() {
        season_entries = anibridge::lookup_tmdb_seasons(tvdb_id).await;
    }

    if season_entries.is_empty() {
        tracing::warn!(
            "No anibridge mapping for TVDB ID {}; returning stub for Seerr",
            tvdb_id
        );
        return Ok(Json(vec![build_stub_series(tvdb_id, cfg)]));
    }

    // Fetch metadata for the first entry to use as the "show-level" info.
    // Note: for multi-season shows (e.g. JoJo), this uses season 1's AniList entry
    // for the title and cover art. This is fine since Seerr keys on tvdb_id, not
    // the title — but a Jikan fallback may return a part-specific title here.
    let first_ids = &season_entries[0].1;
    let show_detail = fetch_anime_detail(first_ids).await;
    let show_title = show_detail
        .as_ref()
        .map(|d| {
            if !d.title_english.is_empty() {
                d.title_english.clone()
            } else {
                d.title_romaji.clone()
            }
        })
        .unwrap_or_else(|| format!("TVDB:{}", tvdb_id));
    let show_cover = show_detail
        .as_ref()
        .map(|d| d.cover_url.clone())
        .unwrap_or_default();

    // Build a seasons array — one season per anibridge entry.
    let mut seasons = Vec::new();
    for (season_num, _ids) in &season_entries {
        let sn = if *season_num == 0 { 1 } else { *season_num };
        if seasons.iter().any(|s: &SonarrSeason| s.season_number == sn) {
            continue;
        }
        seasons.push(SonarrSeason {
            season_number: sn,
            monitored: false,
            statistics: SonarrSeasonStats {
                episode_file_count: 0,
                episode_count: 0,
                total_episode_count: 0,
                size_on_disk: 0,
                percent_of_episodes: 0.0,
            },
        });
    }

    let season_count = seasons.len() as i32;
    let year = show_detail
        .as_ref()
        .and_then(|d| d.season_year)
        .unwrap_or(0);

    let path = if cfg.media_root.is_empty() {
        format!("/media/{}", media::sanitize_folder_name(&show_title))
    } else {
        format!(
            "{}/{}",
            cfg.media_root,
            media::sanitize_folder_name(&show_title)
        )
    };

    let result = SonarrSeries {
        id: 0,
        title: show_title.clone(),
        sort_title: show_title.to_lowercase(),
        status: show_detail
            .as_ref()
            .map(|d| map_status(&d.status))
            .unwrap_or_else(|| "continuing".to_string()),
        overview: String::new(),
        network: String::new(),
        air_time: String::new(),
        images: vec![SonarrImage {
            cover_type: "poster".to_string(),
            url: show_cover.clone(),
        }],
        remote_poster: show_cover,
        seasons,
        year,
        path,
        profile_id: 1,
        language_profile_id: 1,
        season_folder: true,
        monitored: false,
        use_scene_numbering: false,
        runtime: 24,
        tvdb_id,
        tv_rage_id: 0,
        tv_maze_id: 0,
        first_aired: String::new(),
        series_type: "anime".to_string(),
        clean_title: show_title
            .to_lowercase()
            .replace(|c: char| !c.is_alphanumeric(), ""),
        imdb_id: String::new(),
        title_slug: format!("ryokan-tvdb-{}", tvdb_id),
        certification: String::new(),
        genres: vec!["Anime".to_string()],
        tags: vec![],
        added: String::new(),
        ratings: SonarrRatings {
            votes: 0,
            value: 0.0,
        },
        quality_profile_id: 1,
        root_folder_path: cfg.media_root.clone(),
        statistics: SonarrStatistics {
            season_count,
            episode_file_count: 0,
            episode_count: 0,
            total_episode_count: 0,
            size_on_disk: 0,
            percent_of_episodes: 0.0,
        },
    };

    tracing::info!(
        "series_lookup TVDB {}: returning 1 series with {} seasons",
        tvdb_id,
        season_count,
    );

    Ok(Json(vec![result]))
}

/// Fetch anime detail from AniList or Jikan, returning None on failure.
pub(super) async fn fetch_anime_detail(ids: &anibridge::AnimeIds) -> Option<anilist::AnimeDetail> {
    if let Some(al_id) = ids.anilist_id
        && let Ok(d) = anilist::get_anime_detail(al_id).await
    {
        return Some(d);
    }
    if let Some(mal_id) = ids.mal_id
        && let Ok(d) = crate::services::jikan::get_anime_detail_cached(mal_id).await
    {
        return Some(d);
    }
    None
}

pub(super) fn build_sonarr_series_from_search(
    r: &anilist::AnimeEntry,
    title: &str,
    tmdb_id: i64,
    db_series: Option<&series::Series>,
    cfg: &config::Config,
) -> SonarrSeries {
    let total_eps = r.episodes.unwrap_or(0).max(0);
    let is_in_library = db_series.is_some();
    let internal_id = db_series.map(|s| s.id).unwrap_or(0);

    let folder_name = db_series
        .map(|s| s.folder_name.clone())
        .unwrap_or_else(|| media::sanitize_folder_name(title));

    let disk_files = if is_in_library {
        media::scan_series_folder(&cfg.media_root, &folder_name)
    } else {
        Vec::new()
    };
    let on_disk = disk_files.len() as i32;

    let path = if cfg.media_root.is_empty() {
        format!("/media/{}", folder_name)
    } else {
        format!("{}/{}", cfg.media_root, folder_name)
    };

    let monitored = db_series
        .map(|s| s.monitor_mode_enum() != monitoring::MonitorMode::None)
        .unwrap_or(true);

    SonarrSeries {
        id: internal_id,
        title: title.to_string(),
        sort_title: title.to_lowercase(),
        status: map_status(&r.status),
        overview: String::new(),
        network: String::new(),
        air_time: String::new(),
        images: vec![SonarrImage {
            cover_type: "poster".to_string(),
            url: r.cover_url.clone(),
        }],
        remote_poster: r.cover_url.clone(),
        seasons: vec![SonarrSeason {
            season_number: 1,
            monitored,
            statistics: SonarrSeasonStats {
                episode_file_count: on_disk,
                episode_count: total_eps,
                total_episode_count: total_eps,
                size_on_disk: 0,
                percent_of_episodes: if total_eps > 0 {
                    (on_disk as f64 / total_eps as f64) * 100.0
                } else {
                    0.0
                },
            },
        }],
        year: r.season_year.unwrap_or(0),
        path,
        profile_id: 1,
        language_profile_id: 1,
        season_folder: true,
        monitored,
        use_scene_numbering: false,
        runtime: 24,
        tvdb_id: tmdb_id,
        tv_rage_id: 0,
        tv_maze_id: 0,
        first_aired: String::new(),
        series_type: "anime".to_string(),
        clean_title: title
            .to_lowercase()
            .replace(|c: char| !c.is_alphanumeric(), ""),
        imdb_id: String::new(),
        title_slug: format!("ryokan-{}", r.id),
        certification: String::new(),
        genres: vec!["Anime".to_string()],
        tags: vec![],
        added: String::new(),
        ratings: SonarrRatings {
            votes: 0,
            value: 0.0,
        },
        quality_profile_id: 1,
        root_folder_path: cfg.media_root.clone(),
        statistics: SonarrStatistics {
            season_count: 1,
            episode_file_count: on_disk,
            episode_count: total_eps,
            total_episode_count: total_eps,
            size_on_disk: 0,
            percent_of_episodes: if total_eps > 0 {
                (on_disk as f64 / total_eps as f64) * 100.0
            } else {
                0.0
            },
        },
    }
}

pub(super) fn build_sonarr_series_from_tracked(
    s: &series::Series,
    tmdb_id: i64,
    cfg: &config::Config,
) -> SonarrSeries {
    let total_eps = s.episodes.unwrap_or(0).max(0);
    let disk_files = media::scan_series_folder(&cfg.media_root, &s.folder_name);
    let on_disk = disk_files.len() as i32;
    let monitored = s.monitor_mode_enum() != monitoring::MonitorMode::None;

    let path = if cfg.media_root.is_empty() {
        format!("/media/{}", s.folder_name)
    } else {
        format!("{}/{}", cfg.media_root, s.folder_name)
    };

    let title = if !s.title.is_empty() {
        &s.title
    } else {
        &s.title_romaji
    };

    SonarrSeries {
        id: s.id,
        title: title.to_string(),
        sort_title: title.to_lowercase(),
        status: map_status(&s.status),
        overview: String::new(),
        network: String::new(),
        air_time: String::new(),
        images: vec![SonarrImage {
            cover_type: "poster".to_string(),
            url: s.cover_url.clone(),
        }],
        remote_poster: s.cover_url.clone(),
        seasons: vec![SonarrSeason {
            season_number: 1,
            monitored,
            statistics: SonarrSeasonStats {
                episode_file_count: on_disk,
                episode_count: total_eps,
                total_episode_count: total_eps,
                size_on_disk: 0,
                percent_of_episodes: if total_eps > 0 {
                    (on_disk as f64 / total_eps as f64) * 100.0
                } else {
                    0.0
                },
            },
        }],
        year: s.season_year.unwrap_or(0),
        path,
        profile_id: 1,
        language_profile_id: 1,
        season_folder: true,
        monitored,
        use_scene_numbering: false,
        runtime: 24,
        tvdb_id: tmdb_id,
        tv_rage_id: 0,
        tv_maze_id: 0,
        first_aired: String::new(),
        series_type: "anime".to_string(),
        clean_title: title
            .to_lowercase()
            .replace(|c: char| !c.is_alphanumeric(), ""),
        imdb_id: String::new(),
        title_slug: format!("ryokan-{}", s.anilist_id),
        certification: String::new(),
        genres: vec!["Anime".to_string()],
        tags: vec![],
        added: String::new(),
        ratings: SonarrRatings {
            votes: 0,
            value: 0.0,
        },
        quality_profile_id: 1,
        root_folder_path: cfg.media_root.clone(),
        statistics: SonarrStatistics {
            season_count: 1,
            episode_file_count: on_disk,
            episode_count: total_eps,
            total_episode_count: total_eps,
            size_on_disk: 0,
            percent_of_episodes: if total_eps > 0 {
                (on_disk as f64 / total_eps as f64) * 100.0
            } else {
                0.0
            },
        },
    }
}

pub(super) fn map_status(anilist_status: &str) -> String {
    match anilist_status.to_uppercase().as_str() {
        "RELEASING" | "NOT_YET_RELEASED" => "continuing".to_string(),
        "FINISHED" | "FINISHED_AIRING" | "CANCELLED" => "ended".to_string(),
        _ => "continuing".to_string(),
    }
}

/// Build a minimal stub SonarrSeries for TVDB IDs that anibridge can't resolve.
/// This lets Seerr proceed to the add step, where we resolve via AniList title search.
pub(super) fn build_stub_series(tvdb_id: i64, cfg: &config::Config) -> SonarrSeries {
    let path = if cfg.media_root.is_empty() {
        "/media/Unknown".to_string()
    } else {
        format!("{}/Unknown", cfg.media_root)
    };

    SonarrSeries {
        id: 0,
        title: format!("TVDB:{}", tvdb_id),
        sort_title: format!("tvdb:{}", tvdb_id),
        status: "continuing".to_string(),
        overview: String::new(),
        network: String::new(),
        air_time: String::new(),
        images: vec![],
        remote_poster: String::new(),
        seasons: vec![SonarrSeason {
            season_number: 1,
            monitored: true,
            statistics: SonarrSeasonStats {
                episode_file_count: 0,
                episode_count: 0,
                total_episode_count: 0,
                size_on_disk: 0,
                percent_of_episodes: 0.0,
            },
        }],
        year: 0,
        path,
        profile_id: 1,
        language_profile_id: 1,
        season_folder: true,
        monitored: true,
        use_scene_numbering: false,
        runtime: 24,
        tvdb_id,
        tv_rage_id: 0,
        tv_maze_id: 0,
        first_aired: String::new(),
        series_type: "anime".to_string(),
        clean_title: String::new(),
        imdb_id: String::new(),
        title_slug: format!("tvdb-{}", tvdb_id),
        certification: String::new(),
        genres: vec!["Anime".to_string()],
        tags: vec![],
        added: String::new(),
        ratings: SonarrRatings {
            votes: 0,
            value: 0.0,
        },
        quality_profile_id: 1,
        root_folder_path: cfg.media_root.clone(),
        statistics: SonarrStatistics {
            season_count: 1,
            episode_file_count: 0,
            episode_count: 0,
            total_episode_count: 0,
            size_on_disk: 0,
            percent_of_episodes: 0.0,
        },
    }
}
