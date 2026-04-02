use askama::Template;
use axum::{
    extract::{Path, Query, State},
    response::{Html, Json},
};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::collections::HashMap;

use crate::models::{config, local_metadata, metadata_cache, monitoring, series};
use crate::models::log::LogCategory;
use crate::services::{anilist, artwork, auto_search, jikan, kitsu, logger, media, metadata_sync, monitoring as monitoring_service};
use crate::AppState;

#[derive(Template)]
#[template(path = "index.html")]
struct IndexTemplate {
    page: String,
    library: Vec<series::Series>,
    title_language: String,
}

#[derive(Template)]
#[template(path = "series.html")]
struct SeriesTemplate {
    page: String,
    route_id: i64,
    detail: anilist::AnimeDetail,
    is_tracked: bool,
    db_id: Option<i64>,
    folder_name: String,
    media_root: String,
    episodes: Vec<Episode>,
    ep_total: i32,
    on_disk_count: i32,
    size_display: String,
    available_folders: Vec<String>,
    title_language: String,
    relation_groups: Vec<RelationGroup>,
    external_url: String,
    external_label: String,
    monitor_mode: String,
    monitor_mode_label: String,
    monitored_count: i32,
}

#[derive(Template)]
#[template(path = "error.html")]
struct ErrorTemplate {
    page: String,
    title: String,
    message: String,
    detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Episode {
    pub number: i32,
    pub title: String,
    pub title_romaji: String,
    pub title_english: String,
    pub title_native: String,
    pub aired: String,
    pub on_disk: bool,
    pub quality: String,
    pub size_display: String,
    pub filename: String,
    pub can_auto_search: bool,
    pub monitored: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct RelationGroup {
    pub relation_type: String,
    pub label: String,
    pub entries: Vec<RelationCard>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RelationCard {
    pub id: i64,
    pub title: String,
    pub title_romaji: String,
    pub title_english: String,
    pub title_native: String,
    pub cover_url: String,
    pub format: String,
    pub status: String,
    pub episodes: Option<i32>,
}

#[derive(Deserialize)]
pub struct AnilistSearchQuery {
    q: String,
}

#[derive(Deserialize)]
pub struct AddSeriesForm {
    anilist_id: i64,
    mal_id: Option<i64>,
    title: String,
    title_romaji: String,
    title_english: String,
    title_native: String,
    cover_url: String,
    format: String,
    status: String,
    episodes: Option<i32>,
}

#[derive(Deserialize)]
pub struct RemoveSeriesForm {
    id: i64,
}

#[derive(Deserialize)]
pub struct SetFolderForm {
    series_id: i64,
    folder_name: String,
}

#[derive(Deserialize)]
pub struct SetMonitoringForm {
    series_id: i64,
    monitor_mode: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReconcileReport {
    pub checked: usize,
    pub upgraded: usize,
    pub failed: usize,
}

async fn force_mal_fallback_enabled(db: &SqlitePool) -> bool {
    config::get_config(db)
        .await
        .ok()
        .flatten()
        .map(|c| c.force_mal_fallback)
        .unwrap_or(false)
}

async fn force_kitsu_fallback_enabled(db: &SqlitePool) -> bool {
    config::get_config(db)
        .await
        .ok()
        .flatten()
        .map(|c| c.force_kitsu_fallback)
        .unwrap_or(false)
}

async fn resolve_series_request(db: &SqlitePool, request_id: i64) -> Result<(Option<series::Series>, i64), sqlx::Error> {
    if let Some(row) = series::get_by_id(db, request_id).await? {
        Ok((Some(row.clone()), row.anilist_id))
    } else {
        Ok((None, request_id))
    }
}

async fn maybe_reconcile_mal_entry(
    db: &SqlitePool,
    db_series: Option<series::Series>,
) -> Option<(series::Series, anilist::AnimeDetail)> {
    let existing = db_series?;
    let mal_id = existing.mal_id?;
    if existing.anilist_id > 0 {
        return None;
    }

    let matched = match anilist::find_anime_by_mal_id(mal_id).await {
        Ok(Some(entry)) => entry,
        _ => return None,
    };

    let detail = match anilist::get_anime_detail(matched.id).await {
        Ok(detail) => detail,
        Err(_) => return None,
    };

    if series::upsert(
        db,
        matched.id,
        matched.id_mal,
        &(if !matched.title_english.is_empty() { matched.title_english.clone() } else { matched.title_romaji.clone() }),
        &matched.title_romaji,
        &matched.title_english,
        &matched.title_native,
        &matched.cover_url,
        &matched.format,
        &matched.status,
        matched.episodes,
    ).await.is_err() {
        return None;
    }

    let refreshed = match series::get_by_id(db, existing.id).await {
        Ok(Some(row)) => row,
        _ => return None,
    };
    Some((refreshed, detail))
}

async fn resolve_series_context(
    db: &SqlitePool,
    request_id: i64,
) -> Result<(Option<series::Series>, i64, anilist::AnimeDetail), String> {
    let force_fallback = force_mal_fallback_enabled(db).await;
    let (resolved_row, mut provider_id) = resolve_series_request(db, request_id)
        .await
        .map_err(|e| e.to_string())?;
    let mut db_series = resolved_row.clone();

    if let Some(ref tracked) = db_series {
        if let Ok(Some(cached)) = metadata_cache::get_by_series_id(db, tracked.id).await {
            if !cached.is_fresh {
                let db_clone = db.clone();
                let tracked_clone = tracked.clone();
                tokio::spawn(async move {
                    let force_fallback = crate::models::config::get_config(&db_clone)
                        .await
                        .ok()
                        .flatten()
                        .map(|c| c.force_mal_fallback)
                        .unwrap_or(false);
                    let _ = metadata_sync::refresh_series_metadata(&db_clone, &tracked_clone, force_fallback).await;
                });
            }
            return Ok((db_series, cached.provider_id, cached.detail));
        }
        if tracked.anilist_id > 0 {
            if let Ok(Some(cached)) = metadata_cache::get_by_provider_id(db, tracked.anilist_id).await {
                return Ok((db_series, cached.provider_id, cached.detail));
            }
        } else if tracked.anilist_id < 0 {
            // MAL-sourced entry: check provider cache with the negative ID.
            if let Ok(Some(cached)) = metadata_cache::get_by_provider_id(db, tracked.anilist_id).await {
                return Ok((db_series, cached.provider_id, cached.detail));
            }
        }
    } else if provider_id != 0 {
        if let Ok(Some(cached)) = metadata_cache::get_by_provider_id(db, provider_id).await {
            return Ok((db_series, cached.provider_id, cached.detail));
        }
    }

    let mal_hint = db_series.as_ref().and_then(|s| s.mal_id);
    let mut detail = match anilist::get_anime_detail_with_options(provider_id, mal_hint, force_fallback).await {
        Ok(d) => d,
        Err(e) => {
            if let Some((reconciled, upgraded_detail)) = maybe_reconcile_mal_entry(db, db_series.clone()).await {
                provider_id = reconciled.anilist_id;
                db_series = Some(reconciled);
                upgraded_detail
            } else {
                let fallback_mal_id = mal_hint
                    .or_else(|| db_series.as_ref().and_then(|s| s.mal_id));
                if let Some(mid) = fallback_mal_id {
                    let fallback_msg = format!(
                        "AniList detail failed for id={}; falling back to Jikan (mal_id={})",
                        provider_id, mid
                    );
                    logger::warn(&db, LogCategory::AniList, &fallback_msg, &e).await;
                    if let Some(ref tracked) = db_series {
                        if let Ok(Some(cached)) = metadata_cache::get_by_series_id(db, tracked.id).await {
                            logger::info(
                                &db,
                                LogCategory::AniList,
                                &format!("Using cached metadata for {}", tracked.title),
                                &format!("cached_at={}", cached.cached_at),
                            ).await;
                            return Ok((db_series, cached.provider_id, cached.detail));
                        }
                    }
                    match jikan::get_anime_detail_cached(mid).await {
                        Ok(detail) => detail,
                        Err(je) => {
                            if let Some(ref tracked) = db_series {
                                let kitsu_titles = vec![
                                    tracked.title.clone(),
                                    tracked.title_romaji.clone(),
                                    tracked.title_english.clone(),
                                    tracked.title_native.clone(),
                                ];
                                if let Ok(kitsu_detail) = kitsu::get_anime_detail_by_titles(&kitsu_titles, None, tracked.episodes).await {
                                    logger::warn(&db, LogCategory::AniList, "AniList and MAL detail failed; using Kitsu fallback", &tracked.title).await;
                                    return Ok((db_series, kitsu_detail.id, kitsu_detail));
                                }
                            }
                            return Err(format!("{} (Jikan fallback also failed: {})", e, je));
                        }
                    }
                } else {
                    if let Some(ref tracked) = db_series {
                        if let Ok(Some(cached)) = metadata_cache::get_by_series_id(db, tracked.id).await {
                            logger::info(
                                &db,
                                LogCategory::AniList,
                                &format!("Using cached metadata for {}", tracked.title),
                                &format!("cached_at={}", cached.cached_at),
                            ).await;
                            return Ok((db_series, cached.provider_id, cached.detail));
                        }
                        let kitsu_titles = vec![
                            tracked.title.clone(),
                            tracked.title_romaji.clone(),
                            tracked.title_english.clone(),
                            tracked.title_native.clone(),
                        ];
                        if let Ok(kitsu_detail) = kitsu::get_anime_detail_by_titles(&kitsu_titles, None, tracked.episodes).await {
                            logger::warn(&db, LogCategory::AniList, "AniList and MAL detail failed; using Kitsu fallback", &tracked.title).await;
                            return Ok((db_series, kitsu_detail.id, kitsu_detail));
                        }
                    }
                    return Err(e);
                }
            }
        }
    };

    if !force_fallback {
        if let Some((reconciled, upgraded_detail)) = maybe_reconcile_mal_entry(db, db_series.clone()).await {
            provider_id = reconciled.anilist_id;
            db_series = Some(reconciled);
            detail = upgraded_detail;
        }
    }

    if db_series.is_none() {
        db_series = if let Some(mid) = detail.id_mal {
            series::get_by_mal_id(db, mid).await.ok().flatten()
        } else {
            series::get_by_anilist_id(db, detail.id).await.ok().flatten()
        };
    }

    if detail.id != 0 {
        let _ = metadata_cache::upsert_provider(db, detail.id, detail.id_mal, &detail).await;
    }
    if let Some(ref tracked) = db_series {
        if should_persist_detail_cache(tracked, &detail) {
            let _ = metadata_cache::upsert(db, tracked.id, detail.id, detail.id_mal, &detail).await;
        }
        if detail.id_mal.is_some() {
            let _ = jikan::fetch_episode_titles_for_detail(db, &detail).await;
        }
    }

    Ok((db_series, provider_id, detail))
}

async fn reconcile_all_fallback_entries(db: &SqlitePool) -> ReconcileReport {
    let rows = match series::get_unreconciled_fallbacks(db).await {
        Ok(rows) => rows,
        Err(_) => {
            return ReconcileReport { checked: 0, upgraded: 0, failed: 1 };
        }
    };

    let mut report = ReconcileReport { checked: rows.len(), upgraded: 0, failed: 0 };
    for row in rows {
        if maybe_reconcile_mal_entry(db, Some(row)).await.is_some() {
            report.upgraded += 1;
        } else {
            report.failed += 1;
        }
    }
    report
}

pub async fn index(State(state): State<AppState>) -> Html<String> {
    let mut library = series::get_all(&state.db).await.unwrap_or_default();
    for item in library.iter_mut() {
        item.cover_url = artwork::cached_or_source_url(&state.db, &format!("series-{}-cover", item.id), &item.cover_url).await;
    }
    let cfg = config::get_config(&state.db).await.ok().flatten();
    let template = IndexTemplate {
        page: "library".to_string(),
        library,
        title_language: cfg.map(|c| c.title_language).unwrap_or_else(|| "english".to_string()),
    };
    Html(template.render().unwrap_or_default())
}

pub async fn series_detail(
    State(state): State<AppState>,
    Path(request_id): Path<i64>,
) -> Html<String> {
    let (db_series, provider_id, mut detail) = match resolve_series_context(&state.db, request_id).await {
        Ok(v) => v,
        Err(e) => {
            logger::error(&state.db, LogCategory::AniList, &format!("Failed to fetch detail for {}", request_id), &e).await;
            let (title, message, tech_detail) = if e.contains("403") {
                (
                    "Metadata Provider Unavailable".to_string(),
                    "The metadata API is temporarily unavailable. This usually resolves itself within a few hours. Try again later.".to_string(),
                    e,
                )
            } else if e.contains("not found") || e.contains("Not Found") {
                (
                    "Series Not Found".to_string(),
                    format!("Could not find a series with ID {}. It may have been removed from the metadata provider.", request_id),
                    e,
                )
            } else {
                (
                    "Something Went Wrong".to_string(),
                    "An error occurred while loading this series. Please try again.".to_string(),
                    e,
                )
            };
            let template = ErrorTemplate {
                page: "library".to_string(),
                title,
                message,
                detail: tech_detail,
            };
            return Html(template.render().unwrap_or_default());
        }
    };
    let is_tracked = db_series.is_some();
    let db_id = db_series.as_ref().map(|s| s.id);
    let folder_name = db_series.as_ref().map(|s| s.folder_name.clone()).unwrap_or_default();

    let cfg = config::get_config(&state.db).await.ok().flatten();
    let media_root = cfg.as_ref().map(|c| c.media_root.clone()).unwrap_or_default();
    let title_language = cfg
        .as_ref()
        .map(|c| c.title_language.clone())
        .unwrap_or_else(|| "english".to_string());

    let mut monitor_mode = "future".to_string();
    let mut monitor_mode_label = monitoring::MonitorMode::Future.label().to_string();
    if let Some(ref tracked) = db_series {
        if let Ok(summary) = monitoring_service::ensure_series_monitoring_rows(&state.db, tracked).await {
            monitor_mode = summary.mode.as_str().to_string();
            monitor_mode_label = summary.mode.label().to_string();
        } else {
            monitor_mode = tracked.monitor_mode.clone();
            monitor_mode_label = tracked.monitor_mode_enum().label().to_string();
        }
    }
    let (episodes, on_disk_count, size_display, monitored_count) =
        build_episodes(&state.db, &detail, db_id, &folder_name, &media_root).await;
    let ep_total = detail.episodes.unwrap_or(0);
    let available_folders = media::list_media_folders(&media_root);
    if let Some(series_id) = db_series.as_ref().map(|s| s.id) {
        detail.cover_url = artwork::cached_or_source_url(&state.db, &format!("series-{}-cover", series_id), &detail.cover_url).await;
        detail.banner_url = artwork::cached_or_source_url(&state.db, &format!("series-{}-banner", series_id), &detail.banner_url).await;
    } else if detail.id != 0 {
        detail.cover_url = artwork::first_cached_url(&state.db, &vec![artwork::provider_cover_key(detail.id, detail.id_mal), format!("provider-{}-cover", detail.id)], &detail.cover_url).await;
        detail.banner_url = artwork::first_cached_url(&state.db, &vec![artwork::provider_banner_key(detail.id, detail.id_mal), format!("provider-{}-banner", detail.id)], &detail.banner_url).await;
    }

    let relation_groups = build_relation_groups(&state.db, db_series.as_ref().map(|s| s.id), &detail).await;
    let (external_url, external_label) = if detail.id < 0 {
        (
            detail.id_mal
                .map(|id| format!("https://myanimelist.net/anime/{}", id))
                .unwrap_or_default(),
            "MyAnimeList".to_string(),
        )
    } else {
        (
            format!("https://anilist.co/anime/{}", detail.id),
            "AniList".to_string(),
        )
    };

    let template = SeriesTemplate {
        page: "library".to_string(),
        route_id: db_id.unwrap_or(provider_id),
        detail,
        is_tracked,
        db_id,
        folder_name,
        media_root,
        episodes,
        ep_total,
        on_disk_count,
        size_display,
        available_folders,
        title_language,
        relation_groups,
        external_url,
        external_label,
        monitor_mode,
        monitor_mode_label,
        monitored_count,
    };
    Html(template.render().unwrap_or_default())
}

/// Build the episode list for a single series (no chain walking).

fn episode_needs_kitsu_backfill<F>(ep_count: i32, mut has_jikan_title: F) -> bool
where
    F: FnMut(i32) -> bool,
{
    if ep_count <= 1 {
        return false;
    }

    (1..=ep_count).any(|ep_num| !has_jikan_title(ep_num))
}

async fn build_episodes(
    db: &SqlitePool,
    detail: &anilist::AnimeDetail,
    db_id: Option<i64>,
    folder_name: &str,
    media_root: &str,
) -> (Vec<Episode>, i32, String, i32) {
    let ep_count = detail.episodes.unwrap_or(0);
    let disk_files = media::scan_series_folder(media_root, folder_name);

    let cached_eps = if let Some(sid) = db_id {
        let rows = local_metadata::get_episode_map_for_series(db, sid).await.unwrap_or_default();
        if rows.is_empty() && detail.id != 0 {
            local_metadata::get_episode_map_for_provider(db, detail.id).await.unwrap_or_default()
        } else {
            rows
        }
    } else if detail.id != 0 {
        local_metadata::get_episode_map_for_provider(db, detail.id).await.unwrap_or_default()
    } else {
        HashMap::new()
    };
    let cached_matches_force = !force_kitsu_fallback_enabled(db).await
        || cached_eps.values().any(|ep| ep.source == "kitsu");
    let use_cached_eps = !cached_eps.is_empty() && cached_matches_force;

    let episodic_format = !matches!(detail.format.as_str(), "MOVIE" | "SPECIAL" | "OVA" | "ONA");
    let should_fetch_jikan = !use_cached_eps && detail.id_mal.is_some() && (episodic_format || ep_count > 1);
    let jikan_eps = if should_fetch_jikan {
        jikan::fetch_episode_titles_for_detail(db, detail).await
    } else {
        HashMap::new()
    };

    let force_kitsu_fallback = force_kitsu_fallback_enabled(db).await;
    let should_try_kitsu = !use_cached_eps
        && ep_count > 1
        && (force_kitsu_fallback
            || episode_needs_kitsu_backfill(ep_count.max(0), |ep_num| {
                jikan_eps
                    .get(&ep_num)
                    .map(|info| !info.title.trim().is_empty())
                    .unwrap_or(false)
            }));
    let kitsu_eps: HashMap<i32, kitsu::EpisodeInfo> = if should_try_kitsu {
        kitsu::fetch_episode_titles_fallback(
            db,
            &vec![detail.title_english.clone(), detail.title_romaji.clone(), detail.title_native.clone()],
            detail.season_year,
            detail.episodes,
        ).await
    } else {
        HashMap::new()
    };

    let monitored_lookup: std::collections::HashSet<i32> = if let Some(id) = db_id {
        monitoring::get_monitored_episode_numbers(db, id)
            .await
            .unwrap_or_default()
            .into_iter()
            .collect()
    } else {
        std::collections::HashSet::new()
    };

    let mut episodes = Vec::new();
    let mut on_disk_count = 0i32;
    let mut total_size: u64 = 0;
    let mut monitored_count = 0i32;

    for ep_num in 1..=ep_count.max(0) {
        let disk_match = disk_files.iter().find(|f| {
            if let Some(s) = f.season_number {
                s == 1 && f.episode_number == ep_num
            } else {
                f.episode_number == ep_num
            }
        });

        let (on_disk, quality, size_display, filename) = match disk_match {
            Some(f) => (true, f.quality.clone(), f.size_display.clone(), f.filename.clone()),
            None => (false, String::new(), String::new(), String::new()),
        };

        if on_disk {
            on_disk_count += 1;
            if let Some(f) = disk_match {
                total_size += f.size_bytes;
            }
        }

        let use_series_fallback = ep_count <= 1;
        let fallback_title = if use_series_fallback {
            preferred_title(&detail.title_english, &detail.title_romaji, &detail.title_native)
        } else {
            String::new()
        };
        let fallback_romaji = if use_series_fallback {
            non_empty_or(&detail.title_romaji, &fallback_title)
        } else {
            String::new()
        };
        let fallback_english = if use_series_fallback {
            non_empty_or(&detail.title_english, &fallback_title)
        } else {
            String::new()
        };
        let fallback_native = if use_series_fallback {
            non_empty_or(&detail.title_native, &fallback_title)
        } else {
            String::new()
        };

        let (ep_title, ep_title_romaji, ep_title_english, ep_title_native, ep_aired) =
            if use_cached_eps {
                if let Some(info) = cached_eps.get(&ep_num) {
                    (
                        non_empty_or(&info.title, &fallback_title),
                        non_empty_or(&info.title_romaji, &fallback_romaji),
                        non_empty_or(&info.title_english, &fallback_english),
                        non_empty_or(&info.title_native, &fallback_native),
                        info.aired.clone(),
                    )
                } else {
                    (
                        fallback_title.clone(),
                        fallback_romaji.clone(),
                        fallback_english.clone(),
                        fallback_native.clone(),
                        String::new(),
                    )
                }
            } else if force_kitsu_fallback {
                if let Some(kitsu_info) = kitsu_eps.get(&ep_num) {
                    let t = if !kitsu_info.title.trim().is_empty() {
                        kitsu_info.title.clone()
                    } else {
                        fallback_title.clone()
                    };
                    (
                        t.clone(),
                        t.clone(),
                        t.clone(),
                        t,
                        kitsu_info.aired.clone(),
                    )
                } else {
                    match jikan_eps.get(&ep_num) {
                        Some(info) if !info.title.trim().is_empty() => (
                            info.title.clone(),
                            info.title.clone(),
                            info.title.clone(),
                            info.title.clone(),
                            info.aired.clone(),
                        ),
                        Some(info) => (
                            fallback_title.clone(),
                            fallback_romaji.clone(),
                            fallback_english.clone(),
                            fallback_native.clone(),
                            info.aired.clone(),
                        ),
                        None => (
                            fallback_title,
                            fallback_romaji,
                            fallback_english,
                            fallback_native,
                            String::new(),
                        ),
                    }
                }
            } else {
                match jikan_eps.get(&ep_num) {
                    Some(info) if !info.title.trim().is_empty() => (
                        info.title.clone(),
                        info.title.clone(),
                        info.title.clone(),
                        info.title.clone(),
                        info.aired.clone(),
                    ),
                    Some(info) => (
                        fallback_title.clone(),
                        fallback_romaji.clone(),
                        fallback_english.clone(),
                        fallback_native.clone(),
                        info.aired.clone(),
                    ),
                    None => {
                        // Try Kitsu fallback for episode title/air date.
                        if let Some(kitsu_info) = kitsu_eps.get(&ep_num) {
                            let t = if !kitsu_info.title.trim().is_empty() {
                                kitsu_info.title.clone()
                            } else {
                                fallback_title.clone()
                            };
                            (
                                t.clone(),
                                t.clone(),
                                t.clone(),
                                t,
                                kitsu_info.aired.clone(),
                            )
                        } else {
                            (
                                fallback_title,
                                fallback_romaji,
                                fallback_english,
                                fallback_native,
                                String::new(),
                            )
                        }
                    }
                }
            };

        let monitored = monitored_lookup.contains(&ep_num);
        if monitored {
            monitored_count += 1;
        }

        episodes.push(Episode {
            number: ep_num,
            title: ep_title,
            title_romaji: ep_title_romaji,
            title_english: ep_title_english,
            title_native: ep_title_native,
            aired: ep_aired,
            on_disk,
            quality,
            size_display,
            filename,
            can_auto_search: !on_disk && monitored,
            monitored,
        });
    }

    if ep_count == 0 && !disk_files.is_empty() {
        for f in &disk_files {
            on_disk_count += 1;
            total_size += f.size_bytes;
            let monitored = monitored_lookup.contains(&f.episode_number);
            if monitored {
                monitored_count += 1;
            }
            episodes.push(Episode {
                number: f.episode_number,
                title: String::new(),
                title_romaji: String::new(),
                title_english: String::new(),
                title_native: String::new(),
                aired: String::new(),
                on_disk: true,
                quality: f.quality.clone(),
                size_display: f.size_display.clone(),
                filename: f.filename.clone(),
                can_auto_search: false,
                monitored,
            });
        }
    }

    episodes.sort_by(|a, b| b.number.cmp(&a.number));

    let size_display = format_size(total_size);
    (episodes, on_disk_count, size_display, monitored_count)
}

fn relation_identity_key(provider_id: i64, mal_id: Option<i64>) -> String {
    if let Some(mal_id) = mal_id {
        format!("mal:{mal_id}")
    } else {
        format!("provider:{provider_id}")
    }
}

/// Resolve the best link ID for a relation card.  If the related entry is
/// tracked in the library (by AniList ID or MAL ID), return the DB series ID
/// so the link always navigates to `/series/<db_id>`.  Otherwise fall back to
/// the provider ID (which may be negative for MAL-sourced entries, but the
/// detail resolver in `resolve_series_context` knows how to handle that).
async fn resolve_relation_card_id(db: &SqlitePool, provider_id: i64, mal_id: Option<i64>) -> i64 {
    // Try AniList ID first (positive IDs).
    if provider_id > 0 {
        if let Ok(Some(row)) = series::get_by_anilist_id(db, provider_id).await {
            return row.id;
        }
    }
    // Try MAL ID.
    if let Some(mid) = mal_id {
        if let Ok(Some(row)) = series::get_by_mal_id(db, mid).await {
            return row.id;
        }
    }
    // For MAL-sourced entries, the anilist_id column stores -mal_id.
    if provider_id < 0 {
        if let Ok(Some(row)) = series::get_by_anilist_id(db, provider_id).await {
            return row.id;
        }
    }
    provider_id
}

fn relation_richness(rel: &anilist::RelatedEntry) -> i32 {
    let mut score = 0;
    if !rel.cover_url.trim().is_empty() { score += 4; }
    if !rel.format.trim().is_empty() && rel.format != "TBA" { score += 2; }
    if !rel.status.trim().is_empty() && rel.status != "TBA" { score += 2; }
    if rel.episodes.unwrap_or(0) > 0 { score += 1; }
    if !preferred_title(&rel.title_english, &rel.title_romaji, &rel.title_native).trim().is_empty() { score += 1; }
    score
}

fn merge_relation_metadata(primary: &anilist::RelatedEntry, fallback: &anilist::RelatedEntry) -> anilist::RelatedEntry {
    let mut merged = primary.clone();

    if merged.title_romaji.trim().is_empty() { merged.title_romaji = fallback.title_romaji.clone(); }
    if merged.title_english.trim().is_empty() { merged.title_english = fallback.title_english.clone(); }
    if merged.title_native.trim().is_empty() { merged.title_native = fallback.title_native.clone(); }
    if merged.cover_url.trim().is_empty() { merged.cover_url = fallback.cover_url.clone(); }
    if merged.format.trim().is_empty() || merged.format == "TBA" { merged.format = fallback.format.clone(); }
    if merged.status.trim().is_empty() || merged.status == "TBA" {
        merged.status = fallback.status.clone();
        merged.status_display = fallback.status_display.clone();
    }
    if merged.episodes.is_none() || merged.episodes == Some(0) { merged.episodes = fallback.episodes; }
    if merged.season_year.is_none() { merged.season_year = fallback.season_year; }
    if merged.id_mal.is_none() { merged.id_mal = fallback.id_mal; }
    if merged.media_type.trim().is_empty() { merged.media_type = fallback.media_type.clone(); }

    merged
}

/// Group the detail's relations by type for display as cards.
async fn build_relation_groups(
    db: &SqlitePool,
    db_id: Option<i64>,
    detail: &anilist::AnimeDetail,
) -> Vec<RelationGroup> {
    let cached_relations = if let Some(series_id) = db_id {
        let rows = local_metadata::get_relations_for_series(db, series_id).await.unwrap_or_default();
        if rows.is_empty() && detail.id != 0 {
            local_metadata::get_relations_for_provider(db, detail.id).await.unwrap_or_default()
        } else {
            rows
        }
    } else if detail.id != 0 {
        local_metadata::get_relations_for_provider(db, detail.id).await.unwrap_or_default()
    } else {
        Vec::new()
    };

    // Treat the current AniList detail payload as the canonical relation graph whenever it is
    // available. Cached relation rows can be stale from older MAL/Jikan hydration passes, which is
    // how the same title ends up rendered twice under two different relation tags.
    let has_authoritative_relations = !detail.relations.is_empty();
    let mut relations = if has_authoritative_relations {
        detail.relations.clone()
    } else {
        cached_relations.clone()
    };

    if has_authoritative_relations {
        let by_identity: HashMap<String, usize> = relations
            .iter()
            .enumerate()
            .filter(|(_, r)| matches!(r.media_type.as_str(), "ANIME" | "MUSIC"))
            .map(|(idx, r)| (relation_identity_key(r.id, r.id_mal), idx))
            .collect();

        for cached in cached_relations {
            if !matches!(cached.media_type.as_str(), "ANIME" | "MUSIC") {
                continue;
            }
            let key = relation_identity_key(cached.id, cached.id_mal);
            let Some(idx) = by_identity.get(&key).copied() else {
                continue;
            };
            let merged = merge_relation_metadata(&relations[idx], &cached);
            relations[idx] = merged;
        }
    }

    if !has_authoritative_relations && (detail.id != 0 || detail.id_mal.is_some()) {
        let existing_relation_keys: std::collections::HashSet<String> = relations
            .iter()
            .filter(|r| matches!(r.media_type.as_str(), "ANIME" | "MUSIC"))
            .map(|r| relation_identity_key(r.id, r.id_mal))
            .collect();
        let incoming = local_metadata::get_incoming_relations_for_provider(db, detail.id, detail.id_mal)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|r| !existing_relation_keys.contains(&relation_identity_key(r.id, r.id_mal)))
            .collect::<Vec<_>>();
        relations.extend(incoming);
    }

    // Build identity key for the current series so we can filter self-references.
    let self_key = relation_identity_key(detail.id, detail.id_mal);

    let mut deduped: Vec<anilist::RelatedEntry> = Vec::new();
    let mut deduped_index: HashMap<(String, String), usize> = HashMap::new();
    for related in relations {
        if !matches!(related.media_type.as_str(), "ANIME" | "MUSIC") {
            continue;
        }
        // Skip self-references: relations that point back to the current series.
        let related_key = relation_identity_key(related.id, related.id_mal);
        if related_key == self_key {
            continue;
        }
        let normalized_type = local_metadata::normalize_relation_type(&related.relation_type).to_string();
        let key = (related_key, normalized_type);
        if let Some(idx) = deduped_index.get(&key).copied() {
            if relation_richness(&deduped[idx]) < relation_richness(&related) {
                deduped[idx] = related;
            }
        } else {
            deduped_index.insert(key, deduped.len());
            deduped.push(related);
        }
    }
    let relations = deduped;

    let type_order = [
        "PREQUEL", "SEQUEL", "SIDE_STORY", "ALTERNATIVE",
        "SUMMARY", "FULL_STORY", "SPIN_OFF", "OTHER", "CHARACTER", "PARENT", "ADAPTATION",
    ];

    let mut groups: HashMap<String, Vec<RelationCard>> = HashMap::new();

    for related in &relations {
        if !matches!(related.media_type.as_str(), "ANIME" | "MUSIC") {
            continue;
        }

        // Resolve the card's link ID: prefer the DB series ID if this entry is tracked,
        // so clicking the card navigates to /series/<db_id> which always resolves correctly.
        // Without this, MAL-sourced entries link to /series/<negative_mal_id> which can
        // resolve to a different series if the provider cache is stale.
        let card_id = resolve_relation_card_id(db, related.id, related.id_mal).await;

        let normalized_relation_type = local_metadata::normalize_relation_type(&related.relation_type).to_string();
        let cards = groups.entry(normalized_relation_type).or_default();
        let cover_url = if let Some(series_id) = db_id {
            artwork::first_cached_url(
                db,
                &vec![
                    artwork::series_relation_cover_key(series_id, related.id, related.id_mal),
                    format!("series-{}-relation-{}-cover", series_id, related.id),
                    artwork::provider_cover_key(related.id, related.id_mal),
                    format!("provider-{}-cover", related.id),
                ],
                &related.cover_url,
            ).await
        } else if related.id != 0 || related.id_mal.is_some() {
            artwork::first_cached_url(
                db,
                &vec![
                    artwork::provider_cover_key(related.id, related.id_mal),
                    format!("provider-{}-cover", related.id),
                ],
                &related.cover_url,
            ).await
        } else {
            related.cover_url.clone()
        };

        cards.push(RelationCard {
            id: card_id,
            title: preferred_title(&related.title_english, &related.title_romaji, &related.title_native),
            title_romaji: related.title_romaji.clone(),
            title_english: related.title_english.clone(),
            title_native: related.title_native.clone(),
            cover_url,
            format: related.format.clone(),
            status: related.status.clone(),
            episodes: related.episodes,
        });
    }

    let mut result: Vec<RelationGroup> = groups
        .into_iter()
        .map(|(rel_type, mut entries)| {
            entries.sort_by(|a, b| {
                let a_title = a.title.to_ascii_lowercase();
                let b_title = b.title.to_ascii_lowercase();
                a_title
                    .cmp(&b_title)
                    .then_with(|| a.title_romaji.to_ascii_lowercase().cmp(&b.title_romaji.to_ascii_lowercase()))
                    .then_with(|| a.id.cmp(&b.id))
            });
            let label = format_relation_label(&rel_type);
            RelationGroup { relation_type: rel_type, label, entries }
        })
        .collect();

    result.sort_by_key(|g| type_order.iter().position(|t| *t == g.relation_type).unwrap_or(99));
    result
}

fn format_relation_label(rel_type: &str) -> String {
    match rel_type {
        "PREQUEL" => "Prequel".to_string(),
        "SEQUEL" => "Sequel".to_string(),
        "SIDE_STORY" => "Side Story".to_string(),
        "ALTERNATIVE" => "Alternative".to_string(),
        "SUMMARY" => "Summary".to_string(),
        "FULL_STORY" => "Full Story".to_string(),
        "SPIN_OFF" => "Spin Off".to_string(),
        "OTHER" => "Other".to_string(),
        "CHARACTER" => "Character".to_string(),
        "PARENT" => "Parent".to_string(),
        "ADAPTATION" => "Adaptation".to_string(),
        other => other.replace('_', " "),
    }
}

fn non_empty_or(value: &str, fallback: &str) -> String {
    if !value.trim().is_empty() {
        value.to_string()
    } else {
        fallback.to_string()
    }
}

fn preferred_title(english: &str, romaji: &str, native: &str) -> String {
    if !english.is_empty() {
        english.to_string()
    } else if !romaji.is_empty() {
        romaji.to_string()
    } else {
        native.to_string()
    }
}


fn should_persist_detail_cache(tracked: &series::Series, detail: &anilist::AnimeDetail) -> bool {
    if tracked.anilist_id <= 0 {
        return true;
    }
    detail.id > 0 && detail.id == tracked.anilist_id
}

fn format_size(bytes: u64) -> String {
    if bytes == 0 {
        return String::new();
    }
    let gb = bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    if gb >= 1.0 {
        format!("{:.1} GiB", gb)
    } else {
        format!("{:.0} MiB", bytes as f64 / (1024.0 * 1024.0))
    }
}

pub async fn anilist_search(
    State(state): State<AppState>,
    Query(params): Query<AnilistSearchQuery>,
) -> Result<Json<Vec<anilist::AnimeEntry>>, (axum::http::StatusCode, String)> {
    let force_fallback = force_mal_fallback_enabled(&state.db).await;
    let results = anilist::search_anime_with_options(&params.q, force_fallback)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e))?;

    let source = if results.iter().any(|r| r.source == "mal") {
        "MAL/Jikan fallback"
    } else {
        "AniList"
    };
    logger::info(
        &state.db,
        LogCategory::AniList,
        &format!("Title search: '{}'", params.q),
        &format!("results={}, source={}, forced_fallback={}", results.len(), source, force_fallback),
    ).await;

    Ok(Json(results))
}

pub async fn api_series_detail(
    State(state): State<AppState>,
    Path(request_id): Path<i64>,
) -> Result<Json<anilist::AnimeDetail>, (axum::http::StatusCode, String)> {
    let (_, _, detail) = resolve_series_context(&state.db, request_id)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(detail))
}

pub async fn add_series(
    State(state): State<AppState>,
    Json(form): Json<AddSeriesForm>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let (id, created) = series::upsert(
        &state.db,
        form.anilist_id,
        form.mal_id,
        &form.title,
        &form.title_romaji,
        &form.title_english,
        &form.title_native,
        &form.cover_url,
        &form.format,
        &form.status,
        form.episodes,
    )
    .await
    .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    logger::info(
        &state.db,
        LogCategory::Library,
        &format!("{} library entry: {}", if created { "Added" } else { "Updated" }, form.title),
        &format!("id={}, anilist_id={}, mal_id={:?}, format={}, episodes={:?}", id, form.anilist_id, form.mal_id, form.format, form.episodes),
    ).await;

    if let Ok(Some(tracked)) = series::get_by_id(&state.db, id).await {
        let db_clone = state.db.clone();
        let tracked_clone = tracked.clone();
        tokio::spawn(async move {
            let force_fallback = crate::models::config::get_config(&db_clone)
                .await
                .ok()
                .flatten()
                .map(|c| c.force_mal_fallback)
                .unwrap_or(false);
            match metadata_sync::refresh_series_metadata(&db_clone, &tracked_clone, force_fallback).await {
                Ok(detail) => {
                    logger::info(
                        &db_clone,
                        LogCategory::AniList,
                        &format!("Hydrated local metadata for {}", tracked_clone.title),
                        &format!("provider_id={}, mal_id={:?}, episodes={:?}", detail.id, detail.id_mal, detail.episodes),
                    ).await;
                }
                Err(err) => {
                    logger::warn(
                        &db_clone,
                        LogCategory::AniList,
                        &format!("Failed to hydrate local metadata for {}", tracked_clone.title),
                        &err,
                    ).await;
                }
            }
        });
    }

    let monitor = monitoring_service::recompute_series_monitoring(&state.db, id).await.ok();

    Ok(Json(serde_json::json!({
        "ok": true,
        "id": id,
        "created": created,
        "monitor_mode": monitor.as_ref().map(|m| m.mode.as_str()).unwrap_or("future"),
        "monitored_count": monitor.as_ref().map(|m| m.monitored_count).unwrap_or(0),
        "total_count": monitor.as_ref().map(|m| m.total_count).unwrap_or(0),
        "hydrating": true
    })))
}

pub async fn reconcile_fallbacks(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let report = reconcile_all_fallback_entries(&state.db).await;
    logger::info(
        &state.db,
        LogCategory::AniList,
        "Fallback reconciliation complete",
        &format!("checked={}, upgraded={}, failed={}", report.checked, report.upgraded, report.failed),
    ).await;
    Ok(Json(serde_json::json!({
        "ok": true,
        "checked": report.checked,
        "upgraded": report.upgraded,
        "failed": report.failed,
        "message": format!("Checked {}, upgraded {}, failed {}", report.checked, report.upgraded, report.failed),
    })))
}

pub async fn remove_series(
    State(state): State<AppState>,
    Json(form): Json<RemoveSeriesForm>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    logger::info(&state.db, LogCategory::Library, &format!("Removed from library: id={}", form.id), "").await;
    series::remove(&state.db, form.id)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::json!({"ok": true})))
}

pub async fn set_folder(
    State(state): State<AppState>,
    Json(form): Json<SetFolderForm>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    series::update_folder(&state.db, form.series_id, &form.folder_name)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let _ = monitoring_service::recompute_series_monitoring(&state.db, form.series_id).await;
    Ok(Json(serde_json::json!({"ok": true})))
}

pub async fn set_monitoring(
    State(state): State<AppState>,
    Json(form): Json<SetMonitoringForm>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let mode = monitoring::MonitorMode::from_str(&form.monitor_mode);
    let summary = monitoring_service::apply_monitor_mode(&state.db, form.series_id, mode)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e))?;

    logger::info(
        &state.db,
        LogCategory::Library,
        &format!("Updated monitoring for series {}", form.series_id),
        &format!("mode={}, monitored={}/{}", summary.mode.as_str(), summary.monitored_count, summary.total_count),
    ).await;

    Ok(Json(serde_json::json!({
        "ok": true,
        "monitor_mode": summary.mode.as_str(),
        "monitor_mode_label": summary.mode.label(),
        "monitored_count": summary.monitored_count,
        "total_count": summary.total_count,
    })))
}

pub async fn list_folders(
    State(state): State<AppState>,
) -> Result<Json<Vec<String>>, (axum::http::StatusCode, String)> {
    let cfg = config::get_config(&state.db).await.ok().flatten();
    let media_root = cfg.map(|c| c.media_root).unwrap_or_default();
    let folders = media::list_media_folders(&media_root);
    Ok(Json(folders))
}

async fn run_auto_search_targets(
    state: &AppState,
    request_id: i64,
    targets: Vec<auto_search::SearchTarget>,
    allow_batch: bool,
) -> Result<auto_search::AutoSearchReport, (axum::http::StatusCode, String)> {
    let qbit = {
        let qbit = state.qbit.read().await;
        qbit.as_ref()
            .ok_or((axum::http::StatusCode::BAD_REQUEST, "qBittorrent not configured".to_string()))?
            .clone()
    };

    let cfg = config::get_config(&state.db)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .unwrap_or(crate::models::config::Config {
            qbit_url: String::new(),
            qbit_user: String::new(),
            qbit_pass: String::new(),
            qbit_category: String::new(),
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
        });

    let (_, _, detail) = resolve_series_context(&state.db, request_id)
        .await
        .map_err(|e| (axum::http::StatusCode::BAD_GATEWAY, e))?;

    let title = if !detail.title_english.is_empty() {
        &detail.title_english
    } else {
        &detail.title_romaji
    };
    logger::info(
        &state.db,
        LogCategory::AutoSearch,
        &format!("Auto search started for {}", title),
        &format!("{} target(s), allow_batch={}", targets.len(), allow_batch),
    ).await;

    let mut grabbed = Vec::new();
    let mut skipped = Vec::new();
    for target in targets {
        let label = auto_search::target_label(&target);
        match auto_search::find_best_for_target(&detail, &cfg, &target, allow_batch).await {
            Some(result) => {
                let url = if !result.magnet.is_empty() { result.magnet.clone() } else { result.torrent.clone() };
                if url.is_empty() {
                    logger::warn(&state.db, LogCategory::AutoSearch, &format!("{}: no magnet/torrent URL", label), &result.title).await;
                    skipped.push(format!("{}: no magnet/torrent URL", label));
                    continue;
                }
                match qbit.add_torrent(&url).await {
                    Ok(_) => {
                        let tier = crate::services::quality::detect_tier(&result.title, &result.resolution);
                        logger::info(
                            &state.db,
                            LogCategory::Grab,
                            &format!("Grabbed: {}", result.title),
                            &format!("target={}, group={}, score={}, tier={}, batch={}", label, result.group, result.score, tier.label(), result.is_batch),
                        ).await;
                    }
                    Err(e) => {
                        logger::error(&state.db, LogCategory::QBit, &format!("Failed to add torrent for {}", label), &e).await;
                        return Err((axum::http::StatusCode::BAD_GATEWAY, e));
                    }
                }
                let queued_batch = result.is_batch;
                let detected_tier = crate::services::quality::detect_tier(&result.title, &result.resolution);
                grabbed.push(auto_search::AutoSearchHit {
                    target_label: label.clone(),
                    release_title: result.title,
                    release_group: result.group,
                    quality_tier: detected_tier.label().to_string(),
                    url,
                    score: result.score,
                });
                if queued_batch && allow_batch {
                    logger::info(&state.db, LogCategory::AutoSearch, "Season pack queued; stopping episode search", "").await;
                    skipped.push("Season pack queued; skipped additional episode searches".to_string());
                    break;
                }
            }
            None => {
                logger::debug(&state.db, LogCategory::AutoSearch, &format!("{}: no matching release found", label), "").await;
                skipped.push(format!("{}: no matching release found", label));
            }
        }
    }

    logger::info(
        &state.db,
        LogCategory::AutoSearch,
        &format!("Auto search complete: {} grabbed, {} skipped", grabbed.len(), skipped.len()),
        &format!("profile={}", cfg.quality_profile),
    ).await;

    Ok(auto_search::AutoSearchReport {
        grabbed,
        skipped,
        quality_profile: cfg.quality_profile,
    })
}

pub async fn auto_search_series(
    State(state): State<AppState>,
    Path(request_id): Path<i64>,
) -> Result<Json<auto_search::AutoSearchReport>, (axum::http::StatusCode, String)> {
    let cfg = config::get_config(&state.db)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .unwrap_or(crate::models::config::Config {
            qbit_url: String::new(),
            qbit_user: String::new(),
            qbit_pass: String::new(),
            qbit_category: String::new(),
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
        });

    let (tracked_row, provider_id, detail) = resolve_series_context(&state.db, request_id)
        .await
        .map_err(|e| (axum::http::StatusCode::BAD_GATEWAY, e))?;

    let tracked = if let Some(row) = tracked_row {
        Some(row)
    } else if let Some(mid) = detail.id_mal {
        series::get_by_mal_id(&state.db, mid)
            .await
            .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    } else {
        series::get_by_anilist_id(&state.db, provider_id)
            .await
            .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    };
    let folder_name = tracked.as_ref().map(|s| s.folder_name.clone()).unwrap_or_default();
    let existing_files = media::scan_series_folder(&cfg.media_root, &folder_name);
    let existing_eps: Vec<i32> = existing_files.iter().map(|f| f.episode_number).collect();

    let monitored_eps = if let Some(ref tracked_series) = tracked {
        monitoring::get_monitored_episode_numbers(&state.db, tracked_series.id)
            .await
            .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    } else {
        Vec::new()
    };

    let targets = if tracked.is_some() {
        auto_search::build_monitored_targets(&detail, &existing_eps, &monitored_eps)
    } else {
        auto_search::build_missing_targets(&detail, &existing_eps)
    };
    let target_summary = if targets.len() <= 5 {
        targets.iter().map(|t| auto_search::target_label(t)).collect::<Vec<_>>().join(", ")
    } else {
        format!("{} targets", targets.len())
    };
    let title_for_log = if !detail.title_english.is_empty() { &detail.title_english } else { &detail.title_romaji };
    logger::debug(
        &state.db,
        LogCategory::AutoSearch,
        &format!("Missing targets for {}: {}", title_for_log, target_summary),
        &format!("on_disk={}, monitored={}, total={:?}", existing_eps.len(), monitored_eps.len(), detail.episodes),
    ).await;
    let report = run_auto_search_targets(&state, request_id, targets, true).await?;
    Ok(Json(report))
}

pub async fn auto_search_episode(
    State(state): State<AppState>,
    Path((request_id, episode_number)): Path<(i64, i32)>,
) -> Result<Json<auto_search::AutoSearchReport>, (axum::http::StatusCode, String)> {
    let (tracked_row, _, detail) = resolve_series_context(&state.db, request_id)
        .await
        .map_err(|e| (axum::http::StatusCode::BAD_GATEWAY, e))?;

    if let Some(tracked) = tracked_row {
        let monitored_eps = monitoring::get_monitored_episode_numbers(&state.db, tracked.id)
            .await
            .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        if !monitored_eps.contains(&episode_number) {
            return Err((axum::http::StatusCode::BAD_REQUEST, format!("Episode {} is not monitored ({})", episode_number, tracked.monitor_mode_enum().label())));
        }
    } else if matches!(detail.format.as_str(), "MOVIE" | "SPECIAL" | "OVA" | "ONA") && episode_number != 1 {
        return Err((axum::http::StatusCode::BAD_REQUEST, "Single-entry media can only search episode 1".to_string()));
    }

    logger::debug(
        &state.db,
        LogCategory::AutoSearch,
        &format!("Episode search: series_ref={}, episode={}", request_id, episode_number),
        "allow_batch=false",
    ).await;
    let report = run_auto_search_targets(
        &state,
        request_id,
        vec![auto_search::SearchTarget::Episode(episode_number)],
        false,
    )
    .await?;
    Ok(Json(report))
}
