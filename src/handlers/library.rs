use askama::Template;
use axum::{
    extract::{Path, Query, State},
    response::{Html, Json},
};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::collections::HashMap;

use crate::models::{config, episode_tags, grabbed_torrents, local_metadata, metadata_cache, monitoring, series};
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
#[template(path = "needs_review.html")]
struct NeedsReviewTemplate {
    page: String,
    entries: Vec<episode_tags::NeedsReviewEntry>,
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
    title_language: String,
    relation_groups: Vec<RelationGroup>,
    external_url: String,
    external_label: String,
    monitor_mode: String,
    monitor_mode_label: String,
    monitored_count: i32,
    all_monitored: bool,
    /// Phase 4: series-level upgrade opt-in. Rendered as a checkbox on the
    /// series detail page; toggled via POST /api/library/allow-upgrades.
    allow_upgrades: bool,
}

#[derive(Template)]
#[template(path = "error.html")]
struct ErrorTemplate {
    page: String,
    title: String,
    message: String,
    detail: String,
}

#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct Episode {
    pub number: i32,
    pub title: String,
    pub title_romaji: String,
    pub title_english: String,
    pub title_native: String,
    pub aired: String,
    pub on_disk: bool,
    pub quality: String,
    pub quality_state: String,  // "disk", "grabbed", "failed", or ""
    pub size_display: String,
    pub filename: String,
    pub can_auto_search: bool,
    pub monitored: bool,
    /// Phase 4 classification columns — exposed to the template so the
    /// manual override picker can pre-select the current values. The
    /// override dropdown's composite key (e.g. "bluray_remux", "web_dl")
    /// is derived from this quartet in the template JS.
    pub class_source: String,
    pub class_resolution: String,
    pub class_is_remux: bool,
    pub class_is_bdmv: bool,
    pub class_web_kind: String,
    pub manual_override: bool,
    pub needs_review: bool,
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

#[derive(Deserialize, utoipa::IntoParams)]
pub struct AnilistSearchQuery {
    q: String,
}

#[derive(Deserialize, utoipa::ToSchema)]
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
    season_year: Option<i32>,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct RemoveSeriesForm {
    id: i64,
    /// When true (the default for the "Remove from Library" button), the
    /// handler also tells qBittorrent to drop every torrent associated
    /// with the series and removes the series media folder from disk.
    /// Settable to false from API consumers (e.g. the Sonarr compat shim)
    /// that want to delete *only* the database tracking row.
    #[serde(default)]
    delete_files: Option<bool>,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct SetFolderForm {
    series_id: i64,
    folder_name: String,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct SetMonitoringForm {
    series_id: i64,
    monitor_mode: String,
    auto_grab: Option<bool>,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct SetEpisodeMonitoringForm {
    series_id: i64,
    episode_number: i32,
    monitored: bool,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct SetAllowUpgradesForm {
    series_id: i64,
    allow: bool,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct SetManualOverrideForm {
    series_id: i64,
    episode_number: i32,
    /// Empty string clears the override and reverts to classifier output.
    source: String,
    resolution: String,
    #[serde(default)]
    is_remux: bool,
    /// Sonarr-parity: BD-Raw / BDMV release flag, distinct from `is_remux`.
    /// Mutually exclusive at the label level — when both are set, BDMV wins.
    #[serde(default)]
    is_bdmv: bool,
    /// Sonarr-parity: WEB-DL vs WEBRip variant when `source == "Web"`.
    /// Empty string for legacy bare-WEB rows or non-Web sources.
    #[serde(default)]
    web_kind: String,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct MarkEpisodeFailedForm {
    history_id: i64,
    #[serde(default)]
    blocklist: bool,
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
    } else if let Some(row) = series::get_by_anilist_id(db, request_id).await? {
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

    let primary_title = if !matched.title_english.is_empty() {
        matched.title_english.clone()
    } else {
        matched.title_romaji.clone()
    };
    if series::upsert(
        db,
        series::SeriesCore {
            anilist_id: matched.id,
            mal_id: matched.id_mal,
            title: &primary_title,
            title_romaji: &matched.title_romaji,
            title_english: &matched.title_english,
            title_native: &matched.title_native,
            cover_url: &matched.cover_url,
            format: &matched.format,
            status: &matched.status,
            episodes: matched.episodes,
            season_year: matched.season_year,
            end_year: detail.end_year,
        },
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
                    logger::warn(db, LogCategory::AniList, &fallback_msg, &e).await;
                    if let Some(ref tracked) = db_series {
                        if let Ok(Some(cached)) = metadata_cache::get_by_series_id(db, tracked.id).await {
                            logger::info(
                                db,
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
                                    logger::warn(db, LogCategory::AniList, "AniList and MAL detail failed; using Kitsu fallback", &tracked.title).await;
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
                                db,
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
                            logger::warn(db, LogCategory::AniList, "AniList and MAL detail failed; using Kitsu fallback", &tracked.title).await;
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

/// Phase 4 cross-library "needs review" page. Lists every episode the
/// classifier couldn't land a confident verdict on, with a deep link back
/// to the series detail page so the user can open the override modal.
pub async fn needs_review_page(State(state): State<AppState>) -> Html<String> {
    let mut entries = episode_tags::get_needs_review(&state.db).await.unwrap_or_default();
    for entry in entries.iter_mut() {
        entry.cover_url = artwork::cached_or_source_url(
            &state.db,
            &format!("series-{}-cover", entry.series_id),
            &entry.cover_url,
        ).await;
    }
    let template = NeedsReviewTemplate {
        page: "library".to_string(),
        entries,
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
    let ep_total = detail.effective_episode_count();
    if let Some(series_id) = db_series.as_ref().map(|s| s.id) {
        detail.cover_url = artwork::cached_or_source_url(&state.db, &format!("series-{}-cover", series_id), &detail.cover_url).await;
        detail.banner_url = artwork::cached_or_source_url(&state.db, &format!("series-{}-banner", series_id), &detail.banner_url).await;
    } else if detail.id != 0 {
        detail.cover_url = artwork::first_cached_url(&state.db, &[artwork::provider_cover_key(detail.id, detail.id_mal), format!("provider-{}-cover", detail.id)], &detail.cover_url).await;
        detail.banner_url = artwork::first_cached_url(&state.db, &[artwork::provider_banner_key(detail.id, detail.id_mal), format!("provider-{}-banner", detail.id)], &detail.banner_url).await;
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

    let all_monitored = ep_total > 0 && monitored_count >= ep_total;
    let allow_upgrades = db_series.as_ref().map(|s| s.allow_upgrades).unwrap_or(true);
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
        title_language,
        relation_groups,
        external_url,
        external_label,
        monitor_mode,
        monitor_mode_label,
        monitored_count,
        all_monitored,
        allow_upgrades,
    };
    Html(template.render().unwrap_or_default())
}

/// Maximum number of missing trailing Jikan episodes we'll tolerate before
/// falling back to Kitsu. MAL typically lags AniList's airing schedule by 1-2
/// episodes for long-running series (One Piece being the canonical case).
/// Without this tolerance, every One Piece page load re-runs the Kitsu title
/// search (`best_candidate` hits the Kitsu HTTP API before checking the
/// episode cache) to backfill 1-2 trailing episodes. And for long-running
/// shows Kitsu over-counts anyway — it lists episodes past the actual aired
/// count — so falling back here wouldn't even give us accurate titles.
const JIKAN_MAL_LAG_TOLERANCE: i32 = 10;

/// Build the episode list for a single series (no chain walking).
fn episode_needs_kitsu_backfill<F>(ep_count: i32, mut has_jikan_title: F) -> bool
where
    F: FnMut(i32) -> bool,
{
    if ep_count <= 1 {
        return false;
    }

    let missing = (1..=ep_count).filter(|ep_num| !has_jikan_title(*ep_num)).count() as i32;
    missing > JIKAN_MAL_LAG_TOLERANCE
}

async fn build_episodes(
    db: &SqlitePool,
    detail: &anilist::AnimeDetail,
    db_id: Option<i64>,
    folder_name: &str,
    media_root: &str,
) -> (Vec<Episode>, i32, String, i32) {
    let ep_count = detail.effective_episode_count();
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
            &[detail.title_english.clone(), detail.title_romaji.clone(), detail.title_native.clone()],
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

    let quality_tags = if let Some(id) = db_id {
        episode_tags::get_for_series(db, id).await.unwrap_or_default()
    } else {
        std::collections::HashMap::new()
    };

    let is_tracked = db_id.is_some();

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

        // Quality display: disk file quality takes precedence; fall back to grab tag.
        let (display_quality, quality_state) = if !quality.is_empty() {
            (quality.clone(), "disk".to_string())
        } else if let Some(tag) = quality_tags.get(&ep_num) {
            (tag.quality_tag.clone(), tag.state.clone())
        } else {
            (String::new(), String::new())
        };

        let tag = quality_tags.get(&ep_num);
        let class_source = tag.map(|t| t.source.clone()).unwrap_or_default();
        let class_resolution = tag.map(|t| t.resolution.clone()).unwrap_or_default();
        let class_is_remux = tag.map(|t| t.is_remux).unwrap_or(false);
        let class_is_bdmv = tag.map(|t| t.is_bdmv).unwrap_or(false);
        let class_web_kind = tag.map(|t| t.web_kind.clone()).unwrap_or_default();
        let needs_review = tag.map(|t| t.needs_review).unwrap_or(false);
        let manual_override = tag.map(|t| t.manual_override).unwrap_or(false);

        episodes.push(Episode {
            number: ep_num,
            title: ep_title,
            title_romaji: ep_title_romaji,
            title_english: ep_title_english,
            title_native: ep_title_native,
            aired: ep_aired,
            on_disk,
            quality: display_quality,
            quality_state,
            size_display,
            filename,
            can_auto_search: is_tracked,
            monitored,
            class_source,
            class_resolution,
            class_is_remux,
            class_is_bdmv,
            class_web_kind,
            manual_override,
            needs_review,
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
            let (display_quality, quality_state) = if !f.quality.is_empty() {
                (f.quality.clone(), "disk".to_string())
            } else if let Some(tag) = quality_tags.get(&f.episode_number) {
                (tag.quality_tag.clone(), tag.state.clone())
            } else {
                (String::new(), String::new())
            };
            let tag = quality_tags.get(&f.episode_number);
            let class_source = tag.map(|t| t.source.clone()).unwrap_or_default();
            let class_resolution = tag.map(|t| t.resolution.clone()).unwrap_or_default();
            let class_is_remux = tag.map(|t| t.is_remux).unwrap_or(false);
            let class_is_bdmv = tag.map(|t| t.is_bdmv).unwrap_or(false);
            let class_web_kind = tag.map(|t| t.web_kind.clone()).unwrap_or_default();
            let needs_review = tag.map(|t| t.needs_review).unwrap_or(false);
            let manual_override = tag.map(|t| t.manual_override).unwrap_or(false);
            episodes.push(Episode {
                number: f.episode_number,
                title: String::new(),
                title_romaji: String::new(),
                title_english: String::new(),
                title_native: String::new(),
                aired: String::new(),
                on_disk: true,
                quality: display_quality,
                quality_state,
                size_display: f.size_display.clone(),
                filename: f.filename.clone(),
                can_auto_search: is_tracked,
                monitored,
                class_source,
                class_resolution,
                class_is_remux,
                class_is_bdmv,
                class_web_kind,
                manual_override,
                needs_review,
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
                &[artwork::series_relation_cover_key(series_id, related.id, related.id_mal),
                    format!("series-{}-relation-{}-cover", series_id, related.id),
                    artwork::provider_cover_key(related.id, related.id_mal),
                    format!("provider-{}-cover", related.id)],
                &related.cover_url,
            ).await
        } else if related.id != 0 || related.id_mal.is_some() {
            artwork::first_cached_url(
                db,
                &[artwork::provider_cover_key(related.id, related.id_mal),
                    format!("provider-{}-cover", related.id)],
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

#[utoipa::path(
    get,
    path = "/api/anilist/search",
    tag = "Library",
    summary = "Search AniList for anime",
    description = "Search for anime by title. Uses AniList as primary source with MAL/Jikan and Kitsu as fallbacks.",
    params(AnilistSearchQuery),
    responses(
        (status = 200, description = "Search results", body = Vec<anilist::AnimeEntry>),
        (status = 500, description = "Search failed"),
    ),
)]
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

#[utoipa::path(
    get,
    path = "/api/series/{anilist_id}",
    tag = "Library",
    summary = "Get series detail",
    description = "Returns full metadata for a series by its AniList ID or internal database ID.",
    params(
        ("anilist_id" = i64, Path, description = "AniList ID or internal series ID"),
    ),
    responses(
        (status = 200, description = "Series detail", body = anilist::AnimeDetail),
        (status = 500, description = "Failed to fetch detail"),
    ),
)]
pub async fn api_series_detail(
    State(state): State<AppState>,
    Path(request_id): Path<i64>,
) -> Result<Json<anilist::AnimeDetail>, (axum::http::StatusCode, String)> {
    let (_, _, detail) = resolve_series_context(&state.db, request_id)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(detail))
}

#[utoipa::path(
    post,
    path = "/api/library/add",
    tag = "Library",
    summary = "Add series to library",
    description = "Add an anime series to the tracked library. If it already exists, updates the existing entry.",
    request_body = AddSeriesForm,
    responses(
        (status = 200, description = "Series added/updated", body = serde_json::Value),
        (status = 500, description = "Database error"),
    ),
)]
pub async fn add_series(
    State(state): State<AppState>,
    Json(form): Json<AddSeriesForm>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let (id, created) = series::upsert(
        &state.db,
        series::SeriesCore {
            anilist_id: form.anilist_id,
            mal_id: form.mal_id,
            title: &form.title,
            title_romaji: &form.title_romaji,
            title_english: &form.title_english,
            title_native: &form.title_native,
            cover_url: &form.cover_url,
            format: &form.format,
            status: &form.status,
            episodes: form.episodes,
            season_year: form.season_year,
            // AddSeriesForm comes from the search result card which doesn't
            // carry an end date — leave null and let the metadata sync pass
            // populate it via refresh_core_metadata when the full detail
            // fetch lands.
            end_year: None,
        },
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

#[utoipa::path(
    post,
    path = "/api/library/reconcile-fallbacks",
    tag = "Library",
    summary = "Reconcile fallback entries",
    description = "Attempt to upgrade MAL/Jikan-sourced library entries to AniList IDs.",
    responses(
        (status = 200, description = "Reconciliation report", body = serde_json::Value),
    ),
)]
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

#[utoipa::path(
    post,
    path = "/api/library/remove",
    tag = "Library",
    summary = "Remove series from library",
    description = "Remove a tracked series from the library by its internal database ID.",
    request_body = RemoveSeriesForm,
    responses(
        (status = 200, description = "Series removed", body = serde_json::Value),
        (status = 500, description = "Database error"),
    ),
)]
pub async fn remove_series(
    State(state): State<AppState>,
    Json(form): Json<RemoveSeriesForm>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let series_id = form.id;
    let delete_files = form.delete_files.unwrap_or(true);

    // Look up the series row up front so we have folder_name to delete on
    // disk and a useful title for the log line. A missing row isn't fatal
    // — the DB delete below is idempotent — but we have no folder/torrent
    // cleanup work to do in that case.
    let tracked = series::get_by_id(&state.db, series_id)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let mut torrents_removed: u64 = 0;
    let mut torrent_failures: Vec<String> = Vec::new();
    let mut folder_status: &'static str = "skipped";
    let mut folder_detail: String = String::new();

    if delete_files {
        if let Some(ref tracked) = tracked {
            // 1. Tell qBittorrent to drop every torrent (with files) we ever
            //    grabbed for this series. A failure on one torrent is logged
            //    but doesn't abort the rest of the cleanup — we'd rather end
            //    in a partially-clean state than orphan the DB row.
            let hashes = grabbed_torrents::get_all_for_series(&state.db, series_id)
                .await
                .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

            if !hashes.is_empty() {
                let qbit_opt = state.qbit.read().await.clone();
                if let Some(qbit) = qbit_opt {
                    for (_id, hash) in &hashes {
                        if hash.is_empty() {
                            continue;
                        }
                        match qbit.delete_torrent(hash, true).await {
                            Ok(()) => torrents_removed += 1,
                            Err(err) => torrent_failures.push(format!("{}: {}", hash, err)),
                        }
                    }
                } else {
                    torrent_failures.push("qBittorrent client not configured".to_string());
                }
            }

            // 2. Drop the grabbed_torrents rows for this series so the table
            //    doesn't accumulate stale references to hashes qBit just
            //    forgot about. Best-effort — failure is logged but doesn't
            //    block the rest of cleanup.
            if let Err(err) = grabbed_torrents::delete_all_for_series(&state.db, series_id).await {
                torrent_failures.push(format!("clear grabbed_torrents: {}", err));
            }

            // 3. Delete the series media folder. Canonicalize the candidate
            //    path and assert it still resolves under the configured
            //    media root before recursing — a renamed/relocated
            //    folder_name pointed at a symlink to /etc must not
            //    accidentally remove anything outside the library. Async
            //    fs to keep the runtime worker free on slow mounts.
            let cfg_opt = config::get_config(&state.db).await.ok().flatten();
            if let Some(cfg) = cfg_opt {
                if !tracked.folder_name.trim().is_empty() && !cfg.media_root.trim().is_empty() {
                    let series_dir = std::path::Path::new(&cfg.media_root).join(&tracked.folder_name);
                    match tokio::fs::canonicalize(&cfg.media_root).await {
                        Ok(media_root_canon) => {
                            match tokio::fs::canonicalize(&series_dir).await {
                                Ok(series_canon) if series_canon.starts_with(&media_root_canon) => {
                                    match tokio::fs::remove_dir_all(&series_canon).await {
                                        Ok(()) => {
                                            folder_status = "removed";
                                            folder_detail = series_canon.display().to_string();
                                        }
                                        Err(err) => {
                                            folder_status = "error";
                                            folder_detail = format!("{}: {}", series_canon.display(), err);
                                        }
                                    }
                                }
                                Ok(other) => {
                                    folder_status = "refused";
                                    folder_detail = format!(
                                        "resolves outside media root: {} -> {}",
                                        series_dir.display(),
                                        other.display()
                                    );
                                }
                                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                                    folder_status = "missing";
                                    folder_detail = series_dir.display().to_string();
                                }
                                Err(err) => {
                                    folder_status = "error";
                                    folder_detail = format!("{}: {}", series_dir.display(), err);
                                }
                            }
                        }
                        Err(err) => {
                            folder_status = "error";
                            folder_detail = format!("media_root canonicalize: {}", err);
                        }
                    }
                }
            }
        }
    }

    // 4. Remove the DB tracking rows. This is the irreversible step, so
    //    do it last — if filesystem cleanup blew up the operator can
    //    still inspect the half-cleaned state via the Library page.
    series::remove(&state.db, series_id)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // 5. Nudge Jellyfin to rescan so the now-deleted folder disappears
    //    from its UI without waiting for the next scheduled sweep. Best
    //    effort — Jellyfin not being configured or being unreachable is
    //    not a reason to fail the removal.
    let mut jellyfin_status: &'static str = "skipped";
    if delete_files {
        let jellyfin_opt = state.jellyfin.read().await.clone();
        if let Some(jelly) = jellyfin_opt {
            jellyfin_status = match jelly.refresh_library().await {
                Ok(()) => "refreshed",
                Err(_) => "error",
            };
        }
    }

    // Scrub any user-controlled strings that go into the log line —
    // series titles come from metadata providers and folder paths come
    // from user input, both of which could contain newlines or control
    // chars that would corrupt the log format. `sanitize_for_log` strips
    // control chars and caps length.
    let series_label = tracked
        .as_ref()
        .map(|t| crate::handlers::auth::sanitize_for_log(&t.title))
        .unwrap_or_else(|| format!("id={}", series_id));
    let safe_folder_detail = crate::handlers::auth::sanitize_for_log(&folder_detail);
    let safe_torrent_failures: Vec<String> = torrent_failures
        .iter()
        .map(|e| crate::handlers::auth::sanitize_for_log(e))
        .collect();
    logger::info(
        &state.db,
        LogCategory::Library,
        &format!("Removed from library: {}", series_label),
        &format!(
            "id={}, delete_files={}, torrents_removed={}, folder={}{}{}, jellyfin={}",
            series_id,
            delete_files,
            torrents_removed,
            folder_status,
            if safe_folder_detail.is_empty() { String::new() } else { format!(" ({})", safe_folder_detail) },
            if safe_torrent_failures.is_empty() {
                String::new()
            } else {
                format!(", torrent_errors=[{}]", safe_torrent_failures.join("; "))
            },
            jellyfin_status,
        ),
    )
    .await;

    Ok(Json(serde_json::json!({
        "ok": true,
        "torrents_removed": torrents_removed,
        "torrent_errors": torrent_failures,
        "folder": folder_status,
        "jellyfin": jellyfin_status,
    })))
}

#[utoipa::path(
    post,
    path = "/api/library/folder",
    tag = "Library",
    summary = "Set series folder name",
    description = "Set the media library folder name for a tracked series.",
    request_body = SetFolderForm,
    responses(
        (status = 200, description = "Folder updated", body = serde_json::Value),
        (status = 500, description = "Database error"),
    ),
)]
pub async fn set_folder(
    State(state): State<AppState>,
    Json(form): Json<SetFolderForm>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    // Validate the folder name before touching the DB or the filesystem.
    // `sanitize_folder_name` strips path-traversal characters, control
    // chars, and trims surrounding whitespace/dots. If the result is
    // empty or differs from the input, the client sent something unsafe —
    // reject it with 400 rather than silently renaming to the sanitized
    // form (which would mask the attempt and still succeed).
    let sanitized = crate::services::media::sanitize_folder_name(&form.folder_name);
    if sanitized.is_empty() || sanitized != form.folder_name {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            "Invalid folder name".to_string(),
        ));
    }

    series::update_folder(&state.db, form.series_id, &sanitized)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let _ = monitoring_service::recompute_series_monitoring(&state.db, form.series_id).await;
    Ok(Json(serde_json::json!({"ok": true})))
}

#[utoipa::path(
    post,
    path = "/api/library/monitoring",
    tag = "Library",
    summary = "Set series monitoring mode",
    description = "Update the monitoring mode (all, future, none, etc.) for a tracked series.",
    request_body = SetMonitoringForm,
    responses(
        (status = 200, description = "Monitoring updated", body = serde_json::Value),
        (status = 500, description = "Database error"),
    ),
)]
pub async fn set_monitoring(
    State(state): State<AppState>,
    Json(form): Json<SetMonitoringForm>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let mode = monitoring::MonitorMode::from_str(&form.monitor_mode);
    let series_id = form.series_id;
    let summary = monitoring_service::apply_monitor_mode(&state.db, series_id, mode)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e))?;

    logger::info(
        &state.db,
        LogCategory::Library,
        &format!("Updated monitoring for series {}", series_id),
        &format!("mode={}, monitored={}/{}", summary.mode.as_str(), summary.monitored_count, summary.total_count),
    ).await;

    // Auto-grab monitored episodes if requested (e.g. after initial add).
    if form.auto_grab.unwrap_or(false)
        && mode != monitoring::MonitorMode::None
        && summary.monitored_count > 0
        && state.qbit.read().await.is_some()
    {
        let auto_grab_on_add = config::get_config(&state.db)
            .await
            .ok()
            .flatten()
            .map(|c| c.auto_grab_on_add)
            .unwrap_or(true);

        if auto_grab_on_add {
            let state_clone = state.clone();
            tokio::spawn(async move {
                // Small delay to let metadata hydration finish.
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                let _ = auto_search_series(
                    axum::extract::State(state_clone),
                    axum::extract::Path(series_id),
                ).await;
            });
        }
    }

    Ok(Json(serde_json::json!({
        "ok": true,
        "monitor_mode": summary.mode.as_str(),
        "monitor_mode_label": summary.mode.label(),
        "monitored_count": summary.monitored_count,
        "total_count": summary.total_count,
    })))
}

#[utoipa::path(
    post,
    path = "/api/library/episode-monitoring",
    tag = "Library",
    summary = "Set episode monitoring",
    description = "Toggle monitoring on or off for a specific episode of a tracked series.",
    request_body = SetEpisodeMonitoringForm,
    responses(
        (status = 200, description = "Episode monitoring updated", body = serde_json::Value),
        (status = 500, description = "Database error"),
    ),
)]
pub async fn set_episode_monitoring(
    State(state): State<AppState>,
    Json(form): Json<SetEpisodeMonitoringForm>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    monitoring::set_episode_monitored(&state.db, form.series_id, form.episode_number, form.monitored)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::json!({
        "ok": true,
        "episode_number": form.episode_number,
        "monitored": form.monitored,
    })))
}

/// Toggle the per-series upgrade opt-in. Phase 4 feature — when the user
/// turns this off for a series, the upgrade scanner skips it entirely.
#[utoipa::path(
    post,
    path = "/api/library/allow-upgrades",
    tag = "Library",
    summary = "Toggle series upgrade opt-in",
    description = "Enable or disable automated upgrades for a single tracked series.",
    request_body = SetAllowUpgradesForm,
    responses(
        (status = 200, description = "Allow-upgrades toggled", body = serde_json::Value),
        (status = 500, description = "Database error"),
    ),
)]
pub async fn set_allow_upgrades(
    State(state): State<AppState>,
    Json(form): Json<SetAllowUpgradesForm>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    series::update_allow_upgrades(&state.db, form.series_id, form.allow)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    logger::info(
        &state.db,
        LogCategory::Library,
        &format!("Upgrade opt-in for series {} set to {}", form.series_id, form.allow),
        "",
    ).await;
    Ok(Json(serde_json::json!({
        "ok": true,
        "series_id": form.series_id,
        "allow_upgrades": form.allow,
    })))
}

/// Apply (or clear) a user's manual source/resolution override for a single
/// episode. Phase 4 feature — pins the classification so future re-classifies
/// (post-download or library scan) won't overwrite it. Passing an empty
/// `source` clears the override and re-enables automatic re-classification.
///
/// Feeds the Phase 4 feedback loop: every non-clearing override emits a
/// signal that can be aggregated into group_source_map suggestions by the
/// background job.
#[utoipa::path(
    post,
    path = "/api/library/manual-override",
    tag = "Library",
    summary = "Set manual source override on an episode",
    description = "Force a specific source/resolution classification on an episode, or clear an existing override.",
    request_body = SetManualOverrideForm,
    responses(
        (status = 200, description = "Override applied", body = serde_json::Value),
        (status = 500, description = "Database error"),
    ),
)]
pub async fn set_manual_override(
    State(state): State<AppState>,
    Json(form): Json<SetManualOverrideForm>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    episode_tags::set_manual_override(
        &state.db,
        form.series_id,
        form.episode_number,
        &form.source,
        &form.resolution,
        form.is_remux,
        form.is_bdmv,
        &form.web_kind,
    )
    .await
    .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let action = if form.source.is_empty() {
        "cleared".to_string()
    } else {
        format!("{} {}", form.source, form.resolution)
    };
    logger::info(
        &state.db,
        LogCategory::Library,
        &format!("Manual override {} for series {} ep {}", action, form.series_id, form.episode_number),
        "",
    ).await;

    Ok(Json(serde_json::json!({
        "ok": true,
        "series_id": form.series_id,
        "episode_number": form.episode_number,
        "source": form.source,
        "resolution": form.resolution,
        "is_remux": form.is_remux,
    })))
}

#[utoipa::path(
    get,
    path = "/api/library/folders",
    tag = "Library",
    summary = "List media folders",
    description = "List existing folder names under the configured media root directory.",
    responses(
        (status = 200, description = "Folder list", body = Vec<String>),
    ),
)]
pub async fn list_folders(
    State(state): State<AppState>,
) -> Result<Json<Vec<String>>, (axum::http::StatusCode, String)> {
    let cfg = config::get_config(&state.db).await.ok().flatten();
    let media_root = cfg.map(|c| c.media_root).unwrap_or_default();
    let folders = media::list_media_folders(&media_root);
    Ok(Json(folders))
}

/// Phase 2: sibling auto-expansion for multi-series batch releases.
///
/// Called after a successful grab when the user targeted a series
/// that belongs to a franchise. Detects sibling entries
/// (sequels, prequels, side stories, etc.) in the torrent's file
/// list, upserts each detected sibling into the tracked series
/// table, and writes `grabbed_torrent_series` route rows so
/// post-processing can move each file into the correct sibling's
/// media folder.
///
/// Gated on AniList provenance (enforced inside
/// [`auto_search::detect_sibling_entries_in_pack`]) — Jikan-sourced
/// details are skipped to avoid duplicate library rows from MAL's
/// finer-grained splits (e.g. JoJo Stone Ocean, which MAL splits
/// into 3 cours but AL keeps as a single entry). When AL is down at
/// grab time the grab itself still succeeds; sibling detection just
/// returns an empty vec and the 12h metadata refresh can
/// retroactively run detection later if needed.
///
/// Errors here are soft: the grab has already completed and
/// post-processing's legacy fallback (no route rows → route
/// everything to `parent_series_id`) keeps the grab functional even
/// when auto-expand fails partway through. Any failures get logged
/// at warn level.
///
/// Resolve the list of episode numbers a batch release should be
/// recorded against at grab time. Parses explicit episode ranges
/// from the release title (e.g. "01-12", "E01-E24") via
/// [`auto_search::parse_release_numbers`], and when the title carries
/// no explicit numbers falls back to the series' known episode count
/// so a title like "Jellyfish Can't Swim in the Night" still spawns
/// a row per episode.
///
/// Used by the two batch grab handlers (`search_batch_releases` and
/// `grab_batch_result`). Without this, batch grabs passed an empty
/// episode list to `grabbed_torrents::record_grab` and skipped
/// `episode_tags::record_grab` entirely, which meant the series page
/// showed every episode as UNKNOWN until post-processing ran — and
/// if the user had post-processing disabled, the rows never got
/// created at all.
///
/// Capped at 1000 episodes as a safety rail against a garbage
/// AniList record reporting an absurd episode count.
fn batch_episode_numbers(title: &str, detail: &anilist::AnimeDetail) -> Vec<i32> {
    let mut ep_nums: Vec<i32> = auto_search::parse_release_numbers(title)
        .into_iter()
        .collect();
    if ep_nums.is_empty() {
        if let Some(total) = detail.episodes {
            if total > 0 && total <= 1000 {
                ep_nums = (1..=total).collect();
            }
        }
    }
    ep_nums.sort_unstable();
    ep_nums
}

/// Returns the number of siblings *newly added* to the library
/// (upserts that hit an existing row don't count).
#[allow(clippy::too_many_arguments)]
async fn auto_expand_library_from_pack(
    db: &SqlitePool,
    qbit: &crate::services::qbit::QbitClient,
    info_hash: &str,
    parent_detail: &anilist::AnimeDetail,
    parent_series_id: i64,
    parent_episode_numbers: &[i32],
    grab_id: i64,
    torrent_title: &str,
) -> usize {
    if parent_detail.id <= 0 || info_hash.is_empty() {
        return 0;
    }

    // Wait for qBit metadata before asking for the file list. Fresh
    // grabs via `add_torrent` don't block on metadata discovery, so
    // a naive `get_torrent_files` right after add returns empty.
    // We use a generous 60s ceiling here (rather than the 10s used
    // by the interactive selective-narrowing path) because this runs
    // inside a `tokio::spawn` — blocking up to a minute in the
    // background is fine, the HTTP handler has already returned.
    let files = match qbit
        .wait_for_metadata(info_hash, std::time::Duration::from_secs(60))
        .await
    {
        Ok(files) => files,
        Err(e) => {
            logger::warn(
                db,
                LogCategory::Library,
                &format!(
                    "auto-expand: metadata wait failed for '{}', skipping sibling detection (fallback: all files will route to parent series_id={})",
                    torrent_title, parent_series_id
                ),
                &e,
            )
            .await;
            return 0;
        }
    };
    let filenames: Vec<String> = files.iter().map(|f| f.name.clone()).collect();

    auto_expand_library_from_pack_with_files(
        db,
        &filenames,
        parent_detail,
        parent_series_id,
        parent_episode_numbers,
        grab_id,
        torrent_title,
    )
    .await
}

/// Pure inner fn — takes a pre-fetched file list instead of a qBit
/// client so the test suite can exercise the sibling detection, series
/// upsert, and route-writing logic without spinning up qBittorrent.
/// The outer [`auto_expand_library_from_pack`] handles the metadata-
/// wait dance; everything else lives here.
#[allow(clippy::too_many_arguments)]
async fn auto_expand_library_from_pack_with_files(
    db: &SqlitePool,
    filenames: &[String],
    parent_detail: &anilist::AnimeDetail,
    parent_series_id: i64,
    parent_episode_numbers: &[i32],
    grab_id: i64,
    torrent_title: &str,
) -> usize {
    let parent_title = if !parent_detail.title_english.is_empty() {
        parent_detail.title_english.as_str()
    } else {
        parent_detail.title_romaji.as_str()
    };

    if parent_detail.id <= 0 {
        logger::debug(
            db,
            LogCategory::Library,
            "Auto-expand: skipping sibling detection, parent has no AniList id",
            &format!("parent_series_id={}, torrent='{}'", parent_series_id, torrent_title),
        )
        .await;
        return 0;
    }

    logger::debug(
        db,
        LogCategory::Library,
        &format!(
            "Auto-expand: scanning {} file(s) for siblings of '{}'",
            filenames.len(),
            parent_title
        ),
        &format!(
            "parent_anilist_id={}, torrent='{}'",
            parent_detail.id, torrent_title
        ),
    )
    .await;

    let siblings = auto_search::detect_sibling_entries_in_pack(filenames, parent_detail);
    if siblings.is_empty() {
        logger::info(
            db,
            LogCategory::Library,
            &format!(
                "Auto-expand: no siblings detected in pack '{}'",
                torrent_title
            ),
            &format!(
                "parent='{}', parent_anilist_id={}, files={}",
                parent_title,
                parent_detail.id,
                filenames.len()
            ),
        )
        .await;
        return 0;
    }

    let siblings_considered = siblings.len();
    let mut added = 0_usize;
    let mut claimed: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let mut routes: Vec<grabbed_torrents::GrabSeriesRoute> = Vec::new();

    for sibling in siblings {
        let primary_title = if !sibling.title_english.is_empty() {
            sibling.title_english.clone()
        } else {
            sibling.title_romaji.clone()
        };

        // Upsert dedups by mal_id then anilist_id, so reconciled
        // entries that already have both IDs populated update
        // in place instead of duplicating.
        let upsert_result = series::upsert(
            db,
            series::SeriesCore {
                anilist_id: sibling.anilist_id,
                mal_id: sibling.mal_id,
                title: &primary_title,
                title_romaji: &sibling.title_romaji,
                title_english: &sibling.title_english,
                title_native: &sibling.title_native,
                cover_url: &sibling.cover_url,
                format: &sibling.format,
                status: &sibling.status,
                episodes: sibling.episodes,
                season_year: sibling.season_year,
                // Relation cards don't carry end_year — the
                // background metadata refresh populates it.
                end_year: None,
            },
        )
        .await;
        let (sibling_id, created) = match upsert_result {
            Ok(pair) => pair,
            Err(e) => {
                logger::warn(
                    db,
                    LogCategory::Library,
                    &format!("auto-expand: failed to upsert sibling '{}'", primary_title),
                    &e.to_string(),
                )
                .await;
                continue;
            }
        };

        if created {
            added += 1;
            logger::info(
                db,
                LogCategory::Library,
                &format!(
                    "Auto-expand: added sibling '{}' from batch '{}'",
                    primary_title, torrent_title
                ),
                &format!(
                    "anilist_id={}, matched_subtitle={:?}, files={}",
                    sibling.anilist_id,
                    sibling.matched_subtitle,
                    sibling.file_indices.len()
                ),
            )
            .await;

            // Kick off a background metadata refresh so the full
            // detail (description, artwork, end_year, etc.) gets
            // hydrated for the UI. Fire-and-forget — the route is
            // already recorded below either way.
            let db_clone = db.clone();
            tokio::spawn(async move {
                if let Ok(Some(tracked)) = series::get_by_id(&db_clone, sibling_id).await {
                    let force_fallback = config::get_config(&db_clone)
                        .await
                        .ok()
                        .flatten()
                        .map(|c| c.force_mal_fallback)
                        .unwrap_or(false);
                    let _ = metadata_sync::refresh_series_metadata(
                        &db_clone,
                        &tracked,
                        force_fallback,
                    )
                    .await;
                }
            });
        }

        // Derive episode numbers per sibling so
        // find_imported_for_episode can locate this route when
        // an upgrade later targets one of the sibling's episodes.
        //
        // The stored ep_nums are *effective* (post-offset) numbers so
        // an upgrade searching by episode 1 of Owari S2 finds a route
        // whose files were originally numbered E14 on disk. Skip
        // rows that would resolve to a non-positive effective number
        // (shouldn't happen — detection sets offset conservatively —
        // but guards against a bad route/file pairing).
        let mut ep_nums: Vec<i32> = Vec::new();
        for &file_idx in &sibling.file_indices {
            if let Some(name) = filenames.get(file_idx) {
                if let Some((_, raw)) = media::parse_episode_number(&name.to_lowercase()) {
                    let effective = raw - sibling.episode_offset;
                    if effective > 0 {
                        ep_nums.push(effective);
                    }
                }
            }
        }
        ep_nums.sort_unstable();
        ep_nums.dedup();

        for &i in &sibling.file_indices {
            claimed.insert(i);
        }
        routes.push(grabbed_torrents::GrabSeriesRoute {
            grab_id,
            series_id: sibling_id,
            file_indices: sibling.file_indices,
            episode_numbers: ep_nums,
            matched_subtitle: sibling.matched_subtitle,
            episode_offset: sibling.episode_offset,
        });
    }

    // Parent route: every media file not claimed by a sibling
    // routes to the parent series. Unclaimed files are expected for
    // franchise-root grabs (JoJo S1 in a S1-S5 pack won't match any
    // sibling subtitle) but can also indicate extras or a missed
    // sibling — log a warn either way so the operator can spot
    // regressions.
    let parent_file_indices: Vec<usize> = (0..filenames.len())
        .filter(|i| {
            filenames
                .get(*i)
                .map(|n| auto_search::is_media_filename(n))
                .unwrap_or(false)
                && !claimed.contains(i)
        })
        .collect();

    if !routes.is_empty() && !parent_file_indices.is_empty() {
        logger::warn(
            db,
            LogCategory::Library,
            &format!(
                "Auto-expand: {} unclaimed file(s) in batch '{}' routed to parent series",
                parent_file_indices.len(),
                torrent_title,
            ),
            &format!(
                "parent_id={}, siblings_added={}, unclaimed_count={}",
                parent_series_id,
                added,
                parent_file_indices.len()
            ),
        )
        .await;

        routes.push(grabbed_torrents::GrabSeriesRoute {
            grab_id,
            series_id: parent_series_id,
            file_indices: parent_file_indices,
            episode_numbers: parent_episode_numbers.to_vec(),
            matched_subtitle: String::new(),
            // Parent-route files always use their own arc-local
            // numbering — no offset ever needed here.
            episode_offset: 0,
        });
    }

    if !routes.is_empty() {
        if let Err(e) = grabbed_torrents::record_grab_series_routes(db, &routes).await {
            logger::warn(
                db,
                LogCategory::Library,
                &format!(
                    "auto-expand: failed to write route rows for '{}'",
                    torrent_title
                ),
                &e.to_string(),
            )
            .await;
        }
    }

    logger::info(
        db,
        LogCategory::Library,
        &format!(
            "Auto-expand: finished batch '{}' — {} sibling(s) added",
            torrent_title, added
        ),
        &format!(
            "parent='{}', siblings_considered={}, routes_written={}",
            parent_title,
            siblings_considered,
            routes.len()
        ),
    )
    .await;

    added
}

async fn run_auto_search_targets(
    state: &AppState,
    request_id: i64,
    targets: Vec<auto_search::SearchTarget>,
    allow_batch: bool,
    series_id: Option<i64>,
) -> Result<auto_search::AutoSearchReport, (axum::http::StatusCode, String)> {
    run_auto_search_targets_with_upgrades(state, request_id, targets, allow_batch, series_id, std::collections::HashMap::new()).await
}

async fn run_auto_search_targets_with_upgrades(
    state: &AppState,
    request_id: i64,
    targets: Vec<auto_search::SearchTarget>,
    allow_batch: bool,
    series_id: Option<i64>,
    upgrade_classifications: std::collections::HashMap<i32, crate::services::source::ClassificationResult>,
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
        .unwrap_or_default();

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

    // Clone the compiled-CF Arc out from under the read lock so the
    // scoring loop below runs without holding it.
    let cfs = state.custom_formats.read().await.clone();

    let mut grabbed = Vec::new();
    let mut skipped = Vec::new();
    for target in targets {
        let label = auto_search::target_label(&target);
        let is_upgrade = matches!(&target, auto_search::SearchTarget::Episode(n) if upgrade_classifications.contains_key(n));
        match auto_search::find_best_for_target(&state.db, &detail, &cfg, &target, allow_batch, is_upgrade, &cfs).await {
            Some(result) => {
                // Classify up front so both upgrade-verification and log labels
                // read the same result.
                let incoming_classification = crate::services::source::classify_release(
                    &state.db,
                    &result.title,
                    Some(&result.resolution),
                    Some(crate::services::source::NyaaContext {
                        info_hash: &result.info_hash,
                        view_url: &result.link,
                        is_batch: result.is_batch,
                    }),
                    Some(crate::services::source::SeriesContext {
                        status: &detail.status,
                        season_year: detail.season_year,
                        end_year: detail.end_year,
                    }),
                ).await;

                // For upgrade targets, verify the found release is actually
                // better quality than what's already on disk.
                if let auto_search::SearchTarget::Episode(ep_num) = &target {
                    if let Some(existing) = upgrade_classifications.get(ep_num) {
                        if incoming_classification.rank() <= existing.rank() {
                            logger::debug(&state.db, LogCategory::AutoSearch, &format!("{}: skipped upgrade (incoming {} not better than existing {})", label, incoming_classification.label(), existing.label()), &result.title).await;
                            skipped.push(format!("{}: no quality upgrade available", label));
                            continue;
                        }
                        logger::info(&state.db, LogCategory::AutoSearch, &format!("{}: upgrading from {} to {}", label, existing.label(), incoming_classification.label()), &result.title).await;
                    }
                }
                // For selective downloads, prefer the `.torrent` URL
                // over the magnet: qBit can parse metadata straight
                // from the file instead of waiting on DHT/trackers.
                let wants_selective = !result.info_hash.is_empty()
                    && auto_search::has_selective_discriminator(&detail);
                let url = if wants_selective && !result.torrent.is_empty() {
                    result.torrent.clone()
                } else if !result.magnet.is_empty() {
                    result.magnet.clone()
                } else {
                    result.torrent.clone()
                };
                if url.is_empty() {
                    logger::warn(&state.db, LogCategory::AutoSearch, &format!("{}: no magnet/torrent URL", label), &result.title).await;
                    skipped.push(format!("{}: no magnet/torrent URL", label));
                    continue;
                }
                // Selective-file path for multi-part / multi-season
                // packs. `pick_wanted_file_indices` narrows by part
                // number (Kizumonogatari II in a Monogatari megapack)
                // or positive subtitle match (Stardust Crusaders in a
                // JoJo S1–S5 pack). The gate only runs this branch
                // when the detail has an actual discriminator to try,
                // so single-entry series fall through to the plain
                // add. Franchise roots without their own subtitle
                // (JoJo S1) also fall through — they're handled by
                // the multi-series auto-expand path below, which
                // downloads the full pack and routes each sibling's
                // files to its own library entry. On filter error,
                // fall back to a full add rather than dropping the
                // grab entirely.
                let selective_outcome: Result<Option<Vec<usize>>, String> = if wants_selective {
                    let detail_clone = detail.clone();
                    let info_hash_clone = result.info_hash.clone();
                    match qbit
                        .add_torrent_with_file_filter(&url, &info_hash_clone, move |files| {
                            auto_search::pick_wanted_file_indices(files, &detail_clone)
                        })
                        .await
                    {
                        Ok(crate::services::qbit::SelectiveOutcome::Filtered(kept)) => Ok(Some(kept)),
                        Ok(crate::services::qbit::SelectiveOutcome::FullDownload) => Ok(None),
                        Err(e) => {
                            logger::warn(
                                &state.db,
                                LogCategory::Grab,
                                &format!("{}: selective download failed, falling back to full grab", label),
                                &e,
                            )
                            .await;
                            qbit.add_torrent(&url).await.map(|_| None)
                        }
                    }
                } else {
                    qbit.add_torrent(&url).await.map(|_| None)
                };
                match selective_outcome {
                    Ok(kept) => {
                        let selective_suffix = match (&kept, wants_selective) {
                            (Some(ids), _) => format!(", selective={}", ids.len()),
                            (None, true) => ", selective=full(timeout)".to_string(),
                            (None, false) => String::new(),
                        };
                        logger::info(
                            &state.db,
                            LogCategory::Grab,
                            &format!("Grabbed: {}", result.title),
                            &format!("target={}, group={}, score={}, tier={}, batch={}{}", label, result.group, result.score, incoming_classification.label(), result.is_batch, selective_suffix),
                        ).await;
                        // Record for post-processing and episode quality tags.
                        if let Some(sid) = series_id {
                            let mut ep_nums: Vec<i32> = match &target {
                                auto_search::SearchTarget::Episode(n) => vec![*n],
                                auto_search::SearchTarget::Single => vec![1],
                            };
                            // For batch releases, parse all episode numbers from
                            // the title so every covered episode gets a grab tag.
                            if result.is_batch {
                                let parsed = auto_search::parse_release_numbers(&result.title);
                                if !parsed.is_empty() {
                                    ep_nums = parsed.into_iter().collect();
                                    ep_nums.sort_unstable();
                                }
                            }
                            let grab_id = crate::models::grabbed_torrents::record_grab(
                                &state.db,
                                &result.info_hash,
                                &result.title,
                                sid,
                                &ep_nums,
                                result.is_batch,
                            ).await.ok().flatten();
                            for ep_num in &ep_nums {
                                let _ = episode_tags::record_grab(
                                    &state.db,
                                    sid,
                                    *ep_num,
                                    &incoming_classification,
                                    &result.title,
                                    &result.group,
                                    result.size_bytes,
                                    result.is_batch,
                                ).await;
                            }
                            // Phase 2 sibling auto-expand: when the
                            // grab is a batch covering a franchise
                            // (e.g. JoJo S1-S5), detect sibling
                            // entries in the file list and add them
                            // to the library so post-processing can
                            // route each file to the correct series.
                            // Only runs on fresh grabs (existing
                            // grab_id skips the route-write path).
                            //
                            // Skip auto-expand when selective narrowing
                            // successfully applied — the user is
                            // explicitly targeting one sibling inside a
                            // megapack (e.g. Stardust Crusaders in a
                            // JoJo pack), so the other siblings' files
                            // are marked priority=0 in qBit and will
                            // never land. Creating ghost library rows
                            // for them would leave dangling entries
                            // with no imported files. The
                            // `kept.is_none()` fallback path
                            // (selective filter timed out → full
                            // download) still auto-expands because the
                            // whole pack is actually downloading.
                            let selective_narrowed = wants_selective && kept.is_some();
                            if result.is_batch && !selective_narrowed {
                                if let Some(grab_id) = grab_id {
                                    // Fire-and-forget so the HTTP handler
                                    // doesn't block up to ~60s waiting on
                                    // qBit to discover metadata (see the
                                    // `wait_for_metadata` call inside
                                    // `auto_expand_library_from_pack`).
                                    // Failures here only affect post-
                                    // processing routing, which already
                                    // falls back to the parent series.
                                    let db_task = state.db.clone();
                                    let qbit_task = qbit.clone();
                                    let info_hash_task = result.info_hash.clone();
                                    let detail_task = detail.clone();
                                    let ep_nums_task = ep_nums.clone();
                                    let title_task = result.title.clone();
                                    tokio::spawn(async move {
                                        auto_expand_library_from_pack(
                                            &db_task,
                                            &qbit_task,
                                            &info_hash_task,
                                            &detail_task,
                                            sid,
                                            &ep_nums_task,
                                            grab_id,
                                            &title_task,
                                        ).await;
                                    });
                                }
                            }
                        }
                    }
                    Err(e) => {
                        logger::error(&state.db, LogCategory::QBit, &format!("Failed to add torrent for {}", label), &e).await;
                        return Err((axum::http::StatusCode::BAD_GATEWAY, e));
                    }
                }
                let queued_batch = result.is_batch;
                grabbed.push(auto_search::AutoSearchHit {
                    target_label: label.clone(),
                    release_title: result.title,
                    release_group: result.group,
                    quality_tier: incoming_classification.label(),
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

#[utoipa::path(
    post,
    path = "/api/series/{anilist_id}/auto-search",
    tag = "Library",
    summary = "Auto-search all episodes",
    description = "Automatically search and grab the best release for every monitored episode of a series.",
    params(
        ("anilist_id" = i64, Path, description = "AniList ID or internal series ID"),
    ),
    responses(
        (status = 200, description = "Auto-search report", body = auto_search::AutoSearchReport),
        (status = 502, description = "Metadata fetch failed"),
    ),
)]
pub async fn auto_search_series(
    State(state): State<AppState>,
    Path(request_id): Path<i64>,
) -> Result<Json<auto_search::AutoSearchReport>, (axum::http::StatusCode, String)> {
    let cfg = config::get_config(&state.db)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .unwrap_or_default();

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

    let mut targets = if tracked.is_some() {
        auto_search::build_monitored_targets(&detail, &existing_eps, &monitored_eps)
    } else {
        auto_search::build_missing_targets(&detail, &existing_eps)
    };

    // Also include upgrade targets: episodes on disk below the quality cutoff.
    let (cutoff_source, cutoff_is_remux, cutoff_is_bdmv) =
        crate::services::source::parse_cutoff_source(&cfg.cutoff_source);
    let cutoff_resolution = crate::services::source::Resolution::from_str(&cfg.cutoff_resolution);
    let quality_tags = if let Some(ref t) = tracked {
        episode_tags::get_for_series(&state.db, t.id).await.unwrap_or_default()
    } else {
        std::collections::HashMap::new()
    };
    let upgrade_targets = auto_search::build_upgrade_targets(
        &existing_files,
        &monitored_eps,
        cutoff_source,
        cutoff_resolution,
        cutoff_is_remux,
        cutoff_is_bdmv,
        &quality_tags,
    );
    // Merge upgrade targets (avoid duplicates with missing targets).
    let existing_target_eps: std::collections::HashSet<i32> = targets
        .iter()
        .filter_map(|t| match t {
            auto_search::SearchTarget::Episode(n) => Some(*n),
            _ => None,
        })
        .collect();
    for (target, _) in &upgrade_targets {
        if let auto_search::SearchTarget::Episode(n) = target {
            if !existing_target_eps.contains(n) {
                targets.push(target.clone());
            }
        }
    }

    let target_summary = if targets.len() <= 5 {
        targets.iter().map(auto_search::target_label).collect::<Vec<_>>().join(", ")
    } else {
        format!("{} targets", targets.len())
    };
    let upgrade_count = upgrade_targets.len();
    let title_for_log = if !detail.title_english.is_empty() { &detail.title_english } else { &detail.title_romaji };
    logger::debug(
        &state.db,
        LogCategory::AutoSearch,
        &format!("Missing targets for {}: {}", title_for_log, target_summary),
        &format!("on_disk={}, monitored={}, upgradeable={}, total={:?}", existing_eps.len(), monitored_eps.len(), upgrade_count, detail.episodes),
    ).await;
    let series_id_for_grab = tracked.as_ref().map(|s| s.id);
    // Build a map of existing episode classifications for upgrade verification in the search task.
    let upgrade_classifications: std::collections::HashMap<i32, crate::services::source::ClassificationResult> = upgrade_targets
        .into_iter()
        .filter_map(|(t, classification)| match t {
            auto_search::SearchTarget::Episode(n) => Some((n, classification)),
            _ => None,
        })
        .collect();
    // Spawn as an independent task so the grab completes even if the client disconnects.
    let state_clone = state.clone();
    let handle = tokio::spawn(async move {
        run_auto_search_targets_with_upgrades(&state_clone, request_id, targets, true, series_id_for_grab, upgrade_classifications).await
    });
    let report = handle.await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, format!("Search task failed: {}", e)))??;
    Ok(Json(report))
}

#[utoipa::path(
    post,
    path = "/api/series/{anilist_id}/auto-search/{episode_number}",
    tag = "Library",
    summary = "Auto-search single episode",
    description = "Automatically search and grab the best release for a specific episode.",
    params(
        ("anilist_id" = i64, Path, description = "AniList ID or internal series ID"),
        ("episode_number" = i32, Path, description = "Episode number"),
    ),
    responses(
        (status = 200, description = "Auto-search report", body = auto_search::AutoSearchReport),
        (status = 400, description = "Invalid episode for media type"),
        (status = 502, description = "Metadata fetch failed"),
    ),
)]
pub async fn auto_search_episode(
    State(state): State<AppState>,
    Path((request_id, episode_number)): Path<(i64, i32)>,
) -> Result<Json<auto_search::AutoSearchReport>, (axum::http::StatusCode, String)> {
    let (tracked_row, _, detail) = resolve_series_context(&state.db, request_id)
        .await
        .map_err(|e| (axum::http::StatusCode::BAD_GATEWAY, e))?;

    let series_id_for_grab: Option<i64> = tracked_row.as_ref().map(|s| s.id);

    if let Some(_tracked) = tracked_row {
        // Monitoring status does not block manual episode searches.
    } else if matches!(detail.format.as_str(), "MOVIE" | "SPECIAL" | "OVA" | "ONA") && episode_number != 1 {
        return Err((axum::http::StatusCode::BAD_REQUEST, "Single-entry media can only search episode 1".to_string()));
    }

    logger::debug(
        &state.db,
        LogCategory::AutoSearch,
        &format!("Episode search: series_ref={}, episode={}", request_id, episode_number),
        "allow_batch=false",
    ).await;
    // Collapse to Single for single-entry media so movie/OVA/special
    // release titles (which don't carry episode numbers) aren't filtered
    // out by the Episode(n) matching rules.
    let target = auto_search::SearchTarget::for_episode(&detail, episode_number);

    // Spawn as an independent task so the grab completes even if the client disconnects.
    let state_clone = state.clone();
    let handle = tokio::spawn(async move {
        run_auto_search_targets(
            &state_clone,
            request_id,
            vec![target],
            false,
            series_id_for_grab,
        )
        .await
    });
    let report = handle.await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, format!("Search task failed: {}", e)))??;
    Ok(Json(report))
}

/// Search batch releases only for a series (no single-episode grabs).
#[utoipa::path(
    post,
    path = "/api/series/{anilist_id}/search-batch",
    tag = "Library",
    summary = "Search for batch releases",
    description = "Search for batch/complete-series torrent releases and grab the best match.",
    params(
        ("anilist_id" = i64, Path, description = "AniList ID or internal series ID"),
    ),
    responses(
        (status = 200, description = "Batch search report", body = auto_search::AutoSearchReport),
        (status = 502, description = "Metadata fetch failed"),
    ),
)]
pub async fn search_batch_releases(
    State(state): State<AppState>,
    Path(request_id): Path<i64>,
) -> Result<Json<auto_search::AutoSearchReport>, (axum::http::StatusCode, String)> {
    let (tracked_row, _, detail) = resolve_series_context(&state.db, request_id)
        .await
        .map_err(|e| (axum::http::StatusCode::BAD_GATEWAY, e))?;

    let series_id_for_grab = tracked_row.as_ref().map(|s| s.id);

    let cfg = config::get_config(&state.db)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .unwrap_or_default();

    let cfs = state.custom_formats.read().await.clone();

    // Pick the best *batch* — filtering to is_batch pre-selection instead
    // of post-selection. The old code called find_best_for_target and
    // post-filtered, which returned None whenever the top-scored result
    // was a single-episode weekly release (i.e. almost every popular show
    // with active weekly seeders).
    let best = auto_search::find_best_batch_for_target(
        &state.db,
        &detail,
        &cfg,
        &auto_search::SearchTarget::Single,
        &cfs,
    ).await;

    let qbit = {
        let qbit = state.qbit.read().await;
        qbit.as_ref()
            .ok_or((axum::http::StatusCode::BAD_REQUEST, "qBittorrent not configured".to_string()))?
            .clone()
    };

    match best {
        None => Err((axum::http::StatusCode::NOT_FOUND, "No batch release found".to_string())),
        Some(result) => {
            let url = if !result.magnet.is_empty() { result.magnet.clone() } else { result.torrent.clone() };
            if url.is_empty() {
                return Err((axum::http::StatusCode::BAD_GATEWAY, "No magnet/torrent URL for batch release".to_string()));
            }
            qbit.add_torrent(&url).await
                .map_err(|e| (axum::http::StatusCode::BAD_GATEWAY, e))?;
            let classification = crate::services::source::classify_release(
                &state.db,
                &result.title,
                Some(&result.resolution),
                Some(crate::services::source::NyaaContext {
                    info_hash: &result.info_hash,
                    view_url: &result.link,
                    is_batch: result.is_batch,
                }),
                Some(crate::services::source::SeriesContext {
                    status: &detail.status,
                    season_year: detail.season_year,
                    end_year: detail.end_year,
                }),
            ).await;
            let tier_label = classification.label();
            logger::info(
                &state.db,
                LogCategory::Grab,
                &format!("Grabbed batch: {}", result.title),
                &format!("group={}, score={}, tier={}", result.group, result.score, tier_label),
            ).await;
            if let Some(sid) = series_id_for_grab {
                // Parse episode list from the batch title so every covered
                // episode gets a per-episode `episode_quality_tags` row at
                // grab time, not just at post-processing time. Without
                // this the UI shows UNKNOWN for every episode of a
                // freshly-grabbed batch — and if the user has
                // post-processing disabled the rows never get created at
                // all. Mirrors the auto-search-path logic at
                // `run_auto_search_targets_with_upgrades` (look for
                // `parse_release_numbers` above).
                //
                // Fallback when the title carries no explicit range
                // (e.g. "Jellyfish Can't Swim in the Night" with no
                // "01-12" suffix): use the series' known episode count.
                // Capped at 1000 so a garbage AniList record can't
                // spawn a million rows.
                let ep_nums = batch_episode_numbers(&result.title, &detail);
                let _ = crate::models::grabbed_torrents::record_grab(
                    &state.db,
                    &result.info_hash,
                    &result.title,
                    sid,
                    &ep_nums,
                    result.is_batch,
                ).await;
                for ep_num in &ep_nums {
                    let _ = episode_tags::record_grab(
                        &state.db,
                        sid,
                        *ep_num,
                        &classification,
                        &result.title,
                        &result.group,
                        result.size_bytes,
                        result.is_batch,
                    ).await;
                }
            }
            Ok(Json(auto_search::AutoSearchReport {
                grabbed: vec![auto_search::AutoSearchHit {
                    target_label: "Batch".to_string(),
                    release_title: result.title,
                    release_group: result.group,
                    quality_tier: tier_label,
                    url,
                    score: result.score,
                }],
                skipped: vec![],
                quality_profile: cfg.quality_profile,
            }))
        }
    }
}

/// Interactive search: return all scored candidates for an episode without grabbing.
#[utoipa::path(
    get,
    path = "/api/series/{anilist_id}/interactive-search/{episode_number}",
    tag = "Library",
    summary = "Interactive episode search",
    description = "Search Nyaa for all available releases of a specific episode, returning scored results for manual selection.",
    params(
        ("anilist_id" = i64, Path, description = "AniList ID or internal series ID"),
        ("episode_number" = i32, Path, description = "Episode number to search for"),
    ),
    responses(
        (status = 200, description = "Search results", body = Vec<crate::services::nyaa::SearchResult>),
        (status = 502, description = "Metadata fetch failed"),
    ),
)]
pub async fn interactive_search_episode(
    State(state): State<AppState>,
    Path((request_id, episode_number)): Path<(i64, i32)>,
) -> Result<Json<Vec<crate::services::nyaa::SearchResult>>, (axum::http::StatusCode, String)> {
    let (_, _, detail) = resolve_series_context(&state.db, request_id)
        .await
        .map_err(|e| (axum::http::StatusCode::BAD_GATEWAY, e))?;

    let cfg = config::get_config(&state.db)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .unwrap_or_default();

    let cfs = state.custom_formats.read().await.clone();

    // Same single-entry collapse as auto_search_episode — the interactive
    // picker otherwise returns zero results for movies.
    let target = auto_search::SearchTarget::for_episode(&detail, episode_number);
    let results = auto_search::find_all_for_target(
        &state.db,
        &detail,
        &cfg,
        &target,
        false,
        &cfs,
    ).await;

    Ok(Json(results))
}

/// Interactive batch search: return all scored batch candidates so the user
/// can pick one. Uses the same query sweep as the auto batch search
/// (`find_best_batch_for_target`) so the interactive and auto paths surface
/// the same candidate pool — the only difference is that this returns every
/// hit instead of picking the top.
#[utoipa::path(
    get,
    path = "/api/series/{anilist_id}/interactive-search-batch",
    tag = "Library",
    summary = "Interactive batch search",
    description = "Search Nyaa for batch/complete releases of a series, returning scored results for manual selection.",
    params(
        ("anilist_id" = i64, Path, description = "AniList ID or internal series ID"),
    ),
    responses(
        (status = 200, description = "Batch search results", body = Vec<crate::services::nyaa::SearchResult>),
        (status = 502, description = "Metadata fetch failed"),
    ),
)]
pub async fn interactive_search_batches(
    State(state): State<AppState>,
    Path(request_id): Path<i64>,
) -> Result<Json<Vec<crate::services::nyaa::SearchResult>>, (axum::http::StatusCode, String)> {
    let (_, _, detail) = resolve_series_context(&state.db, request_id)
        .await
        .map_err(|e| (axum::http::StatusCode::BAD_GATEWAY, e))?;

    let cfg = config::get_config(&state.db)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .unwrap_or_default();

    let cfs = state.custom_formats.read().await.clone();

    let results = auto_search::collect_scored_batches_for_target(
        &state.db,
        &detail,
        &cfg,
        &auto_search::SearchTarget::Single,
        &cfs,
    ).await;

    Ok(Json(results))
}

/// Grab a specific batch release chosen from interactive batch search.
///
/// Mirrors `grab_interactive_result` but without an episode number —
/// batches cover a range of episodes — the episode list is resolved
/// from the release title via [`batch_episode_numbers`] at grab time
/// so per-episode `episode_tags::record_grab` writes land immediately
/// and the UI shows the batch's quality tier without waiting on
/// post-processing.
#[utoipa::path(
    post,
    path = "/api/series/{anilist_id}/grab-batch",
    tag = "Library",
    summary = "Grab a specific batch release",
    description = "Send a specific batch torrent release (chosen from interactive batch search) to qBittorrent for download.",
    params(
        ("anilist_id" = i64, Path, description = "AniList ID or internal series ID"),
    ),
    request_body = serde_json::Value,
    responses(
        (status = 200, description = "Batch grabbed", body = serde_json::Value),
        (status = 400, description = "No URL provided or qBittorrent not configured"),
        (status = 502, description = "Metadata fetch failed"),
    ),
)]
pub async fn grab_batch_result(
    State(state): State<AppState>,
    Path(request_id): Path<i64>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let (tracked_row, _, detail) = resolve_series_context(&state.db, request_id)
        .await
        .map_err(|e| (axum::http::StatusCode::BAD_GATEWAY, e))?;

    let series_id = tracked_row.as_ref().map(|s| s.id);
    let url = body["url"].as_str().unwrap_or("").to_string();
    let title = body["title"].as_str().unwrap_or("").to_string();
    let group = body["group"].as_str().unwrap_or("").to_string();
    let resolution = body["resolution"].as_str().unwrap_or("").to_string();
    let info_hash = body["info_hash"].as_str().unwrap_or("").to_string();
    let size_bytes = body["size_bytes"].as_i64().unwrap_or(0);

    if url.is_empty() {
        return Err((axum::http::StatusCode::BAD_REQUEST, "No URL provided".to_string()));
    }

    let qbit = {
        let qbit = state.qbit.read().await;
        qbit.as_ref()
            .ok_or((axum::http::StatusCode::BAD_REQUEST, "qBittorrent not configured".to_string()))?
            .clone()
    };

    // Same selective-file path as `grab_interactive_result`: narrow
    // a megapack to just the target if it has its own subtitle or
    // part number. Franchise roots (JoJo S1) deliberately fall
    // through so the multi-series auto-expand path below can route
    // each sibling's files into its own library entry instead.
    let wants_selective = !info_hash.is_empty()
        && auto_search::has_selective_discriminator(&detail);
    let selective_outcome: Option<Vec<usize>> = if wants_selective {
        let detail_clone = detail.clone();
        match qbit
            .add_torrent_with_file_filter(&url, &info_hash, move |files| {
                auto_search::pick_wanted_file_indices(files, &detail_clone)
            })
            .await
        {
            Ok(crate::services::qbit::SelectiveOutcome::Filtered(kept)) => Some(kept),
            Ok(crate::services::qbit::SelectiveOutcome::FullDownload) => None,
            Err(e) => {
                logger::warn(
                    &state.db,
                    LogCategory::Grab,
                    &format!("Selective batch download failed, falling back to full grab: {}", title),
                    &e,
                )
                .await;
                qbit.add_torrent(&url)
                    .await
                    .map_err(|e| (axum::http::StatusCode::BAD_GATEWAY, e))?;
                None
            }
        }
    } else {
        qbit.add_torrent(&url)
            .await
            .map_err(|e| (axum::http::StatusCode::BAD_GATEWAY, e))?;
        None
    };

    // Classify so the log line carries the actual quality tier. Pass the
    // chosen release as a batch in NyaaContext (Layer 4 uses this for the
    // finished-series BluRay rule).
    let classification = crate::services::source::classify_release(
        &state.db,
        &title,
        Some(&resolution),
        Some(crate::services::source::NyaaContext {
            info_hash: &info_hash,
            view_url: "",
            is_batch: true,
        }),
        Some(crate::services::source::SeriesContext {
            status: &detail.status,
            season_year: detail.season_year,
            end_year: detail.end_year,
        }),
    ).await;

    let selective_suffix = match (&selective_outcome, wants_selective) {
        (Some(kept), _) => format!(", selective={}", kept.len()),
        (None, true) => ", selective=full(timeout)".to_string(),
        (None, false) => String::new(),
    };
    logger::info(
        &state.db,
        LogCategory::Grab,
        &format!("Grabbed batch (interactive): {}", title),
        &format!("group={}, tier={}{}", group, classification.label(), selective_suffix),
    ).await;

    if let Some(sid) = series_id {
        // Parse episode list from the batch title so every covered
        // episode gets a per-episode `episode_quality_tags` row at
        // grab time. Same reasoning as in `search_batch_releases` —
        // without this, batch grabs leave every episode showing
        // UNKNOWN in the UI, and with post-processing disabled the
        // rows never get created at all.
        let ep_nums = batch_episode_numbers(&title, &detail);
        let grab_id = crate::models::grabbed_torrents::record_grab(
            &state.db, &info_hash, &title, sid, &ep_nums, true,
        ).await.ok().flatten();
        for ep_num in &ep_nums {
            let _ = episode_tags::record_grab(
                &state.db,
                sid,
                *ep_num,
                &classification,
                &title,
                &group,
                size_bytes,
                true,
            ).await;
        }
        // Phase 2 sibling auto-expand. Skip when selective narrowing
        // successfully applied — the user picked a specific sibling
        // (e.g. Stardust Crusaders) out of a megapack and the other
        // siblings' files are marked priority=0 in qBit and won't
        // land. Creating library entries for them would leave ghost
        // rows with no imported files. The `selective_outcome.is_none()
        // && wants_selective` fallback (filter timed out → full
        // download) still auto-expands because the whole pack is
        // actually downloading.
        let selective_narrowed = wants_selective && selective_outcome.is_some();
        if !selective_narrowed {
            if let Some(grab_id) = grab_id {
                // Fire-and-forget so the HTTP handler doesn't block
                // up to ~60s on qBit metadata discovery. See the
                // matching spawn in `run_auto_search_targets_with_upgrades`.
                let db_task = state.db.clone();
                let qbit_task = qbit.clone();
                let info_hash_task = info_hash.clone();
                let detail_task = detail.clone();
                let title_task = title.clone();
                let ep_nums_task = ep_nums.clone();
                tokio::spawn(async move {
                    auto_expand_library_from_pack(
                        &db_task,
                        &qbit_task,
                        &info_hash_task,
                        &detail_task,
                        sid,
                        &ep_nums_task,
                        grab_id,
                        &title_task,
                    ).await;
                });
            }
        }
    }

    Ok(Json(serde_json::json!({
        "ok": true,
        "title": title,
        "tier": classification.label(),
        "selective_files": selective_outcome,
    })))
}

/// Grab a specific release chosen from the interactive search.
#[utoipa::path(
    post,
    path = "/api/series/{anilist_id}/grab/{episode_number}",
    tag = "Library",
    summary = "Grab a specific release",
    description = "Send a specific torrent release (chosen from interactive search) to qBittorrent for download.",
    params(
        ("anilist_id" = i64, Path, description = "AniList ID or internal series ID"),
        ("episode_number" = i32, Path, description = "Episode number"),
    ),
    request_body = serde_json::Value,
    responses(
        (status = 200, description = "Release grabbed", body = serde_json::Value),
        (status = 400, description = "No URL provided or qBittorrent not configured"),
        (status = 502, description = "Metadata fetch failed"),
    ),
)]
pub async fn grab_interactive_result(
    State(state): State<AppState>,
    Path((request_id, episode_number)): Path<(i64, i32)>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let (tracked_row, _, detail) = resolve_series_context(&state.db, request_id)
        .await
        .map_err(|e| (axum::http::StatusCode::BAD_GATEWAY, e))?;

    let series_id = tracked_row.as_ref().map(|s| s.id);
    let url = body["url"].as_str().unwrap_or("").to_string();
    let title = body["title"].as_str().unwrap_or("").to_string();
    let group = body["group"].as_str().unwrap_or("").to_string();
    let resolution = body["resolution"].as_str().unwrap_or("").to_string();
    let info_hash = body["info_hash"].as_str().unwrap_or("").to_string();
    let size_bytes = body["size_bytes"].as_i64().unwrap_or(0);

    if url.is_empty() {
        return Err((axum::http::StatusCode::BAD_REQUEST, "No URL provided".to_string()));
    }

    let qbit = {
        let qbit = state.qbit.read().await;
        qbit.as_ref()
            .ok_or((axum::http::StatusCode::BAD_REQUEST, "qBittorrent not configured".to_string()))?
            .clone()
    };

    // If the target is a multi-part entry ("Kizumonogatari II") OR a
    // subtitled season of a franchise ("Stardust Crusaders"), try the
    // selective-file download path so a megapack release only pulls
    // the files the user is tracking. Franchise roots without their
    // own subtitle return `false` here and fall through to the plain
    // `add_torrent` path — interactive single-episode grabs don't
    // auto-expand the library (that's `grab_batch_result`'s job).
    let wants_selective = !info_hash.is_empty()
        && auto_search::has_selective_discriminator(&detail);
    let selective_outcome: Option<Vec<usize>> = if wants_selective {
        let detail_clone = detail.clone();
        match qbit
            .add_torrent_with_file_filter(&url, &info_hash, move |files| {
                auto_search::pick_wanted_file_indices(files, &detail_clone)
            })
            .await
        {
            Ok(crate::services::qbit::SelectiveOutcome::Filtered(kept)) => Some(kept),
            Ok(crate::services::qbit::SelectiveOutcome::FullDownload) => None,
            Err(e) => {
                logger::warn(
                    &state.db,
                    LogCategory::Grab,
                    &format!("Selective download failed, falling back to full grab: {}", title),
                    &e,
                )
                .await;
                qbit.add_torrent(&url)
                    .await
                    .map_err(|e| (axum::http::StatusCode::BAD_GATEWAY, e))?;
                None
            }
        }
    } else {
        qbit.add_torrent(&url)
            .await
            .map_err(|e| (axum::http::StatusCode::BAD_GATEWAY, e))?;
        None
    };

    // Interactive grab: the frontend doesn't currently round-trip the Nyaa
    // view URL, so Layer 2 is skipped here. These grabs are user-initiated
    // and rarely land on the ambiguous tail that Layer 2 targets anyway.
    // Layer 4 still runs when we have a tracked series — it's a pure
    // function with no round-trip cost.
    let series_ctx = tracked_row
        .as_ref()
        .map(|s| crate::services::source::SeriesContext {
            status: &s.status,
            season_year: s.season_year,
            end_year: s.end_year,
        });
    let classification = crate::services::source::classify_release(
        &state.db,
        &title,
        Some(&resolution),
        None,
        series_ctx,
    ).await;
    let selective_suffix = match (&selective_outcome, wants_selective) {
        (Some(kept), _) => format!(", selective={}", kept.len()),
        (None, true) => ", selective=full(timeout)".to_string(),
        (None, false) => String::new(),
    };
    logger::info(
        &state.db,
        LogCategory::Grab,
        &format!("Interactive grab: {}", title),
        &format!(
            "episode={}, group={}, tier={}{}",
            episode_number, group, classification.label(), selective_suffix
        ),
    ).await;

    if let Some(sid) = series_id {
        // Interactive single-episode grab — not a batch by definition.
        let _ = crate::models::grabbed_torrents::record_grab(
            &state.db, &info_hash, &title, sid, &[episode_number], false,
        ).await;
        let _ = episode_tags::record_grab(
            &state.db, sid, episode_number, &classification, &title, &group, size_bytes, false,
        ).await;
    }

    Ok(Json(serde_json::json!({
        "ok": true,
        "selective_files": selective_outcome,
    })))
}

/// Delete an episode file from disk.
#[utoipa::path(
    post,
    path = "/api/series/{anilist_id}/delete-file/{episode_number}",
    tag = "Library",
    summary = "Delete episode file",
    description = "Delete the on-disk media file for a specific episode.",
    params(
        ("anilist_id" = i64, Path, description = "AniList ID or internal series ID"),
        ("episode_number" = i32, Path, description = "Episode number"),
    ),
    responses(
        (status = 200, description = "File deleted", body = serde_json::Value),
        (status = 400, description = "Series not in library or no file found"),
        (status = 502, description = "Metadata fetch failed"),
    ),
)]
pub async fn delete_episode_file(
    State(state): State<AppState>,
    Path((request_id, episode_number)): Path<(i64, i32)>,
) -> (axum::http::StatusCode, Json<serde_json::Value>) {
    let json_err = |status: axum::http::StatusCode, msg: &str| {
        (status, Json(serde_json::json!({"ok": false, "message": msg})))
    };

    let (tracked_row, _, _detail) = match resolve_series_context(&state.db, request_id).await {
        Ok(v) => v,
        Err(e) => return json_err(axum::http::StatusCode::BAD_GATEWAY, &e),
    };

    let tracked = match tracked_row {
        Some(t) => t,
        None => return json_err(axum::http::StatusCode::BAD_REQUEST, "Series not in library"),
    };

    let cfg = match config::get_config(&state.db).await.ok().flatten() {
        Some(c) => c,
        None => return json_err(axum::http::StatusCode::INTERNAL_SERVER_ERROR, "No config"),
    };

    let files = media::scan_series_folder(&cfg.media_root, &tracked.folder_name);
    let target = files.iter().find(|f| f.episode_number == episode_number);

    match target {
        None => json_err(axum::http::StatusCode::NOT_FOUND, "Episode file not found on disk"),
        Some(file) => {
            let series_dir = std::path::Path::new(&cfg.media_root).join(&tracked.folder_name);
            let full_path = series_dir.join(&file.filename);

            // Canonicalize and verify the resolved path is still inside
            // the configured media root. `media::scan_series_folder` uses
            // `std::fs::read_dir`, which silently follows directory
            // symlinks — so a symlink inside a tracked series folder
            // pointing at, say, `/etc` would show up in the scan, and a
            // DELETE on that episode would then unlink a file outside
            // the media root. Canonicalizing `media_root` and asserting
            // that the canonicalized target sits under it closes that
            // gap without breaking legitimate symlinks that still
            // resolve inside the library.
            // Use tokio::fs::canonicalize so the runtime worker isn't
            // stalled while symlinks are walked on a potentially slow
            // network mount (NFS/SMB). Doing the std::fs version here
            // re-introduces the blocking-on-runtime class of bug the rest
            // of this module was rewritten to avoid.
            let media_root_canon = match tokio::fs::canonicalize(&cfg.media_root).await {
                Ok(p) => p,
                Err(e) => return json_err(
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    &format!("Failed to resolve media root: {}", e),
                ),
            };
            let full_path_canon = match tokio::fs::canonicalize(&full_path).await {
                Ok(p) => p,
                Err(e) => return json_err(
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    &format!("Failed to resolve file: {}", e),
                ),
            };
            if !full_path_canon.starts_with(&media_root_canon) {
                logger::warn(
                    &state.db,
                    LogCategory::Library,
                    "Refused to delete file outside media root",
                    &format!(
                        "series_id={}, requested={}, resolved={}, media_root={}",
                        tracked.id,
                        full_path.display(),
                        full_path_canon.display(),
                        media_root_canon.display()
                    ),
                ).await;
                return json_err(
                    axum::http::StatusCode::BAD_REQUEST,
                    "File resolves outside media root",
                );
            }

            // tokio::fs::remove_file so the handler doesn't block a
            // runtime worker on the (typically fast, but latency-sensitive
            // on network mounts) unlink call. `exists()` is still sync but
            // that's a cheap stat call.
            if let Err(e) = tokio::fs::remove_file(&full_path_canon).await {
                return json_err(
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    &format!("Failed to delete file: {}", e),
                );
            }

            // Also remove the accompanying .nfo file if it exists. Same
            // canonicalize-and-check dance so a sibling symlink can't
            // route the delete outside the media root either. Use the
            // async variant so the stat/readlink walk doesn't block the
            // Tokio worker on a slow mount.
            let nfo_path = full_path_canon.with_extension("nfo");
            if let Ok(nfo_canon) = tokio::fs::canonicalize(&nfo_path).await {
                if nfo_canon.starts_with(&media_root_canon) {
                    let _ = tokio::fs::remove_file(&nfo_canon).await;
                }
            }

            // Clear the episode quality tag so it shows as missing again.
            let _ = episode_tags::clear_episode_tag(&state.db, tracked.id, episode_number).await;

            logger::info(
                &state.db,
                LogCategory::Library,
                &format!("Deleted episode {} file: {}", episode_number, file.filename),
                &format!("series_id={}, path={}", tracked.id, full_path_canon.display()),
            ).await;

            (axum::http::StatusCode::OK, Json(serde_json::json!({"ok": true, "deleted": file.filename})))
        }
    }
}

/// Get grab history for a specific episode.
#[utoipa::path(
    get,
    path = "/api/series/{anilist_id}/grab-history/{episode_number}",
    tag = "Library",
    summary = "Get episode grab history",
    description = "Returns the grab history for a specific episode, including quality tags and release info.",
    params(
        ("anilist_id" = i64, Path, description = "AniList ID or internal series ID"),
        ("episode_number" = i32, Path, description = "Episode number"),
    ),
    responses(
        (status = 200, description = "Grab history entries", body = Vec<episode_tags::GrabHistoryEntry>),
        (status = 400, description = "Series not in library"),
    ),
)]
pub async fn get_episode_grab_history(
    State(state): State<AppState>,
    Path((request_id, episode_number)): Path<(i64, i32)>,
) -> Result<Json<Vec<episode_tags::GrabHistoryEntry>>, (axum::http::StatusCode, String)> {
    let (tracked_row, _, _) = resolve_series_context(&state.db, request_id)
        .await
        .map_err(|e| (axum::http::StatusCode::BAD_GATEWAY, e))?;

    let series_id = tracked_row
        .ok_or((axum::http::StatusCode::BAD_REQUEST, "Series not in library".to_string()))?
        .id;

    let history = episode_tags::get_grab_history(&state.db, series_id, episode_number)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(history))
}

/// Mark a grab as failed and re-trigger auto-search for the episode.
#[utoipa::path(
    post,
    path = "/api/series/{anilist_id}/mark-failed/{episode_number}",
    tag = "Library",
    summary = "Mark episode grab as failed",
    description = "Mark a grabbed episode as failed and optionally blocklist it, then re-search for a replacement.",
    params(
        ("anilist_id" = i64, Path, description = "AniList ID or internal series ID"),
        ("episode_number" = i32, Path, description = "Episode number"),
    ),
    request_body = MarkEpisodeFailedForm,
    responses(
        (status = 200, description = "Re-search report", body = auto_search::AutoSearchReport),
        (status = 400, description = "Series not in library"),
    ),
)]
pub async fn mark_episode_failed(
    State(state): State<AppState>,
    Path((request_id, episode_number)): Path<(i64, i32)>,
    Json(form): Json<MarkEpisodeFailedForm>,
) -> Result<Json<auto_search::AutoSearchReport>, (axum::http::StatusCode, String)> {
    let (tracked_row, _, detail) = resolve_series_context(&state.db, request_id)
        .await
        .map_err(|e| (axum::http::StatusCode::BAD_GATEWAY, e))?;

    let series_id = tracked_row
        .ok_or((axum::http::StatusCode::BAD_REQUEST, "Series not in library".to_string()))?
        .id;

    let (_sid, _ep, release_title) = episode_tags::mark_grab_failed(&state.db, form.history_id)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    if form.blocklist && !release_title.is_empty() {
        let _ = grabbed_torrents::mark_failed_by_name(&state.db, series_id, &release_title).await;
    }

    // Tell qBittorrent to drop the previously-imported torrent for this
    // episode (if any) before we re-search. Without this, qBit keeps the
    // old torrent seeding from its downloads-folder copy, and any new grab
    // we hand it can hash-collide or stall at 0% because qBit thinks the
    // file is already present. delete_files=true is safe because
    // post-processing imports as a hardlink/copy/move — the library copy
    // lives on a separate inode (or path) from the downloads-folder source.
    // We deliberately leave the `grabbed_torrents` row in 'imported' state
    // so post-processing's upgrade detection can still find it, remove the
    // stale library file, and import the replacement cleanly.
    if let Ok(old_grabs) =
        grabbed_torrents::find_imported_for_episode(&state.db, series_id, episode_number).await
    {
        if !old_grabs.is_empty() {
            let qbit = { state.qbit.read().await.as_ref().cloned() };
            if let Some(qbit) = qbit {
                for old in &old_grabs {
                    if !old.hash.is_empty() {
                        if let Err(e) = qbit.delete_torrent(&old.hash, true).await {
                            crate::services::logger::warn(
                                &state.db,
                                crate::models::log::LogCategory::QBit,
                                &format!(
                                    "Failed to remove old torrent for S?E{:02} replacement: '{}'",
                                    episode_number, old.torrent_name
                                ),
                                &e,
                            )
                            .await;
                        }
                    }
                }
            }
        }
    }

    // Re-trigger auto-search for this episode in the background so it completes
    // even if the client disconnects. Collapse to Single for single-entry
    // media so the retry doesn't get stuck in the same Episode(1)
    // filter-rejects-everything trap.
    let target = auto_search::SearchTarget::for_episode(&detail, episode_number);
    let state_clone = state.clone();
    let handle = tokio::spawn(async move {
        run_auto_search_targets(
            &state_clone,
            request_id,
            vec![target],
            false,
            Some(series_id),
        ).await
    });
    let report = handle.await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, format!("Search task failed: {}", e)))??;

    Ok(Json(report))
}

/// Returns download progress for episodes of a series that are currently downloading.
#[utoipa::path(
    get,
    path = "/api/series/{anilist_id}/download-progress",
    tag = "Library",
    summary = "Episode download progress",
    description = "Returns download progress for all actively downloading episodes of a series.",
    params(
        ("anilist_id" = i64, Path, description = "AniList ID or internal series ID"),
    ),
    responses(
        (status = 200, description = "Download progress per episode", body = Vec<EpisodeProgress>),
        (status = 400, description = "Series not in library"),
    ),
)]
pub async fn episode_download_progress(
    State(state): State<AppState>,
    Path(request_id): Path<i64>,
) -> Result<Json<Vec<EpisodeProgress>>, (axum::http::StatusCode, String)> {
    let (tracked_row, _, _) = resolve_series_context(&state.db, request_id)
        .await
        .map_err(|e| (axum::http::StatusCode::BAD_GATEWAY, e))?;

    let tracked = tracked_row.ok_or((axum::http::StatusCode::BAD_REQUEST, "Series not in library".to_string()))?;

    let pending = crate::models::grabbed_torrents::get_all_pending(&state.db)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let series_pending: Vec<_> = pending.iter().filter(|g| g.series_id == tracked.id).collect();
    if series_pending.is_empty() {
        return Ok(Json(Vec::new()));
    }

    let qbit = {
        let guard = state.qbit.read().await;
        match guard.as_ref() {
            Some(c) => c.clone(),
            None => return Ok(Json(Vec::new())),
        }
    };

    let torrents = qbit.get_torrents().await.unwrap_or_default();
    let by_hash: HashMap<String, &crate::services::qbit::Torrent> = torrents
        .iter()
        .map(|t| (t.hash.to_lowercase(), t))
        .collect();

    let mut results = Vec::new();
    for grab in &series_pending {
        let torrent = if !grab.hash.is_empty() {
            by_hash.get(&grab.hash.to_lowercase()).copied()
        } else {
            None
        };

        let Some(t) = torrent else {
            // Torrent not in qBittorrent — skip it so the UI clears the stale
            // progress bar. The post-processing tick will mark old orphans as failed.
            continue;
        };

        for ep in &grab.episode_numbers {
            results.push(EpisodeProgress {
                episode: *ep,
                progress: t.progress,
                speed: t.dlspeed,
                state: t.state.clone(),
            });
        }
    }

    Ok(Json(results))
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct EpisodeProgress {
    pub episode: i32,
    pub progress: f64,
    pub speed: i64,
    pub state: String,
}

/// Returns the current episode state for a series as JSON.
///
/// Used by the series page's download-progress poller: when a torrent
/// disappears from the progress response (meaning the download completed and
/// the post-processing tick has moved the file into the library), the client
/// fetches this endpoint and patches the affected row in-place so the user
/// sees the new on-disk file without a full page refresh.
#[utoipa::path(
    get,
    path = "/api/series/{anilist_id}/episodes",
    tag = "Library",
    summary = "Episode state snapshot",
    description = "Returns the current list of episodes for a series, reflecting on-disk state.",
    params(
        ("anilist_id" = i64, Path, description = "AniList ID or internal series ID"),
    ),
    responses(
        (status = 200, description = "Episode state", body = Vec<Episode>),
        (status = 502, description = "Metadata fetch failed"),
    ),
)]
pub async fn series_episodes_json(
    State(state): State<AppState>,
    Path(request_id): Path<i64>,
) -> Result<Json<Vec<Episode>>, (axum::http::StatusCode, String)> {
    let (db_series, _, detail) = resolve_series_context(&state.db, request_id)
        .await
        .map_err(|e| (axum::http::StatusCode::BAD_GATEWAY, e))?;

    let db_id = db_series.as_ref().map(|s| s.id);
    let folder_name = db_series.as_ref().map(|s| s.folder_name.clone()).unwrap_or_default();

    let cfg = config::get_config(&state.db).await.ok().flatten();
    let media_root = cfg.as_ref().map(|c| c.media_root.clone()).unwrap_or_default();

    let (episodes, _, _, _) =
        build_episodes(&state.db, &detail, db_id, &folder_name, &media_root).await;

    Ok(Json(episodes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_anime_detail(id: i64, title_english: &str) -> anilist::AnimeDetail {
        anilist::AnimeDetail {
            id,
            id_mal: None,
            title_romaji: title_english.to_string(),
            title_english: title_english.to_string(),
            title_native: String::new(),
            cover_url: String::new(),
            banner_url: String::new(),
            format: "TV".to_string(),
            status: "FINISHED".to_string(),
            status_display: "Finished".to_string(),
            episodes: Some(26),
            duration: Some(24),
            season: String::new(),
            season_year: Some(2012),
            end_year: Some(2013),
            description: String::new(),
            genres: Vec::new(),
            average_score: None,
            average_score_display: None,
            score_is_ten_point: false,
            score_class: String::new(),
            next_airing_episode: None,
            next_airing_at: None,
            synonyms: Vec::new(),
            streaming_episodes: Vec::new(),
            relations: Vec::new(),
        }
    }

    fn related_entry(
        id: i64,
        title_english: &str,
        episodes: Option<i32>,
    ) -> anilist::RelatedEntry {
        anilist::RelatedEntry {
            id,
            id_mal: None,
            title_romaji: title_english.to_string(),
            title_english: title_english.to_string(),
            title_native: String::new(),
            cover_url: String::new(),
            format: "TV".to_string(),
            status: "FINISHED".to_string(),
            status_display: "Finished".to_string(),
            episodes,
            relation_type: "SIDE_STORY".to_string(),
            season_year: Some(2014),
            media_type: "ANIME".to_string(),
        }
    }

    /// End-to-end exercise of the Phase 2 auto-expand route writer.
    /// Mirrors the real JoJo S1-S3 megapack case: the parent entry
    /// ("JoJo's Bizarre Adventure") owns the Phantom Blood / Battle
    /// Tendency files and a sibling relation ("Stardust Crusaders")
    /// owns the S3 files. After the pure inner fn runs, we expect two
    /// route rows to land in `grabbed_torrent_series` — one per series
    /// — with the unclaimed (parent) files routing to the franchise
    /// root.
    ///
    /// The fn is split into outer (qBit metadata wait) + inner
    /// (`_with_files`) precisely so this test can feed synthetic
    /// filenames without spinning up qBittorrent.
    #[tokio::test]
    async fn auto_expand_routes_sibling_and_parent_files() {
        let db = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite");
        crate::models::migrate(&db).await.expect("migrate");

        // Seed the parent series row. The real grab path calls
        // series::upsert first, so this matches production flow.
        let (parent_id, _) = series::upsert(
            &db,
            series::SeriesCore {
                anilist_id: 801,
                mal_id: None,
                title: "JoJo's Bizarre Adventure",
                title_romaji: "JoJo no Kimyou na Bouken",
                title_english: "JoJo's Bizarre Adventure",
                title_native: "",
                cover_url: "",
                format: "TV",
                status: "FINISHED",
                episodes: Some(26),
                season_year: Some(2012),
                end_year: Some(2013),
            },
        )
        .await
        .expect("parent upsert");

        // Record a grab row so there's a grab_id for the routes to
        // attach to. `record_grab` returns Ok(Some(id)) on fresh insert.
        let grab_id = grabbed_torrents::record_grab(
            &db,
            "dummyhash0000000000000000000000000000000",
            "[Group] JoJo Megapack (BD 1080p)",
            parent_id,
            &[],
            true,
        )
        .await
        .expect("record_grab")
        .expect("grab row inserted");

        // Construct a parent AnimeDetail with one sibling relation.
        // The sibling title must carry an extractable trailing
        // subtitle ("Stardust Crusaders") for detect_sibling_entries
        // to find a needle to match.
        let mut parent_detail = empty_anime_detail(801, "JoJo's Bizarre Adventure");
        parent_detail.relations.push(related_entry(
            802,
            "JoJo's Bizarre Adventure: Stardust Crusaders",
            Some(24),
        ));

        // Two sibling files (match the Stardust Crusaders needle) and
        // two parent files (bare franchise title, no sibling subtitle).
        let filenames = vec![
            "[Group] JoJo no Kimyou na Bouken - Stardust Crusaders - 01 [BD 1080p].mkv".to_string(),
            "[Group] JoJo no Kimyou na Bouken - Stardust Crusaders - 02 [BD 1080p].mkv".to_string(),
            "[Group] JoJo no Kimyou na Bouken - 01 [BD 1080p].mkv".to_string(),
            "[Group] JoJo no Kimyou na Bouken - 02 [BD 1080p].mkv".to_string(),
        ];

        let added = auto_expand_library_from_pack_with_files(
            &db,
            &filenames,
            &parent_detail,
            parent_id,
            &[1, 2],
            grab_id,
            "[Group] JoJo Megapack (BD 1080p)",
        )
        .await;

        assert_eq!(added, 1, "one new sibling (Stardust Crusaders) expected");

        let routes = grabbed_torrents::get_series_routes(&db, grab_id)
            .await
            .expect("get_series_routes");
        assert_eq!(routes.len(), 2, "sibling route + parent route expected");

        // The sibling route: claims file indices 0 and 1, its series_id
        // differs from the parent, and the matched subtitle is the one
        // trailing_subtitle_of extracted from the relation title.
        let sibling_route = routes
            .iter()
            .find(|r| r.series_id != parent_id)
            .expect("sibling route present");
        assert_eq!(sibling_route.file_indices, vec![0, 1]);
        assert_eq!(sibling_route.matched_subtitle, "Stardust Crusaders");
        // Arc-local numbering (files E01, E02) → min_ep=1 ≤
        // parent_cap=26 → offset=0, and stored episode_numbers
        // equal the raw parsed values.
        assert_eq!(sibling_route.episode_offset, 0);
        assert_eq!(sibling_route.episode_numbers, vec![1, 2]);

        // The parent route: claims the unclaimed media files (2 and 3)
        // and reuses the caller-supplied episode numbers verbatim.
        let parent_route = routes
            .iter()
            .find(|r| r.series_id == parent_id)
            .expect("parent route present");
        assert_eq!(parent_route.file_indices, vec![2, 3]);
        assert_eq!(parent_route.episode_numbers, vec![1, 2]);
        // Parent routes always carry offset 0.
        assert_eq!(parent_route.episode_offset, 0);
    }

    /// Smol Monogatari-style batch: absolute episode numbering runs
    /// across parent + sibling (E13 = last parent ep, E14 = first
    /// sibling ep). The fallback path detects Owarimonogatari Second
    /// Season via title-prefix matching AND the per-sibling offset
    /// pass sets offset=13 so the route row's episode_numbers store
    /// the effective (arc-local) 1..=7 instead of the raw 14..=20.
    #[tokio::test]
    async fn auto_expand_persists_episode_offset_for_absolute_numbered_batch() {
        let db = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite");
        crate::models::migrate(&db).await.expect("migrate");

        let (parent_id, _) = series::upsert(
            &db,
            series::SeriesCore {
                anilist_id: 21320,
                mal_id: None,
                title: "Owarimonogatari",
                title_romaji: "Owarimonogatari",
                title_english: "Owarimonogatari",
                title_native: "",
                cover_url: "",
                format: "TV",
                status: "FINISHED",
                episodes: Some(13),
                season_year: Some(2015),
                end_year: Some(2015),
            },
        )
        .await
        .expect("parent upsert");

        let grab_id = grabbed_torrents::record_grab(
            &db,
            "owarismolhash00000000000000000000000000",
            "[smol] Monogatari - S07 [BD 1080p HEVC Opus]",
            parent_id,
            &[],
            true,
        )
        .await
        .expect("record_grab")
        .expect("grab inserted");

        // Parent AnimeDetail with a continuation relation. Use the
        // real title "Owarimonogatari Second Season" — no delimiter,
        // no 2-token trailing subtitle — so the subtitle path cannot
        // match and the fallback path's title-prefix rule must fire.
        let mut parent_detail = empty_anime_detail(21320, "Owarimonogatari");
        parent_detail.episodes = Some(13);
        parent_detail
            .relations
            .push(related_entry(21860, "Owarimonogatari Second Season", Some(7)));

        // 13 parent files (S07E01..E13) + 7 sibling files (S07E14..E20).
        let mut filenames: Vec<String> = Vec::new();
        for n in 1..=13 {
            filenames.push(format!(
                "[smol] Monogatari - S07E{:02} - Owarimonogatari (BD 1080p).mkv",
                n
            ));
        }
        for n in 14..=20 {
            filenames.push(format!(
                "[smol] Monogatari - S07E{:02} - Owarimonogatari Second Season (Ge) (BD 1080p).mkv",
                n
            ));
        }
        let parent_episode_numbers: Vec<i32> = (1..=13).collect();

        let added = auto_expand_library_from_pack_with_files(
            &db,
            &filenames,
            &parent_detail,
            parent_id,
            &parent_episode_numbers,
            grab_id,
            "[smol] Monogatari - S07 [BD 1080p HEVC Opus]",
        )
        .await;

        assert_eq!(added, 1, "one new sibling (Owari S2) expected");

        let routes = grabbed_torrents::get_series_routes(&db, grab_id)
            .await
            .expect("get_series_routes");
        assert_eq!(routes.len(), 2, "sibling route + parent route expected");

        let sibling_route = routes
            .iter()
            .find(|r| r.series_id != parent_id)
            .expect("sibling route present");
        // Files 13..=19 (0-based indices) correspond to S07E14..E20.
        assert_eq!(sibling_route.file_indices, vec![13, 14, 15, 16, 17, 18, 19]);
        // The matched subtitle records the detection method for
        // operator inspection.
        assert!(sibling_route
            .matched_subtitle
            .starts_with("episode-range fallback"));
        // Absolute numbering → offset = parent_cap = 13.
        assert_eq!(sibling_route.episode_offset, 13);
        // Stored episode_numbers are effective (post-offset) values,
        // so a later `find_imported_for_episode(sibling, 1)` upgrade
        // query hits this route row correctly.
        assert_eq!(sibling_route.episode_numbers, vec![1, 2, 3, 4, 5, 6, 7]);

        let parent_route = routes
            .iter()
            .find(|r| r.series_id == parent_id)
            .expect("parent route present");
        assert_eq!(parent_route.file_indices, vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
        assert_eq!(parent_route.episode_offset, 0);
    }

    /// When the file list has no sibling matches, the inner fn is a
    /// no-op: no sibling series get upserted and no route rows get
    /// written. This exercises the early-return after
    /// `detect_sibling_entries_in_pack` returns an empty vec — the
    /// production path relies on that branch to avoid polluting the
    /// library with ghost rows for regular single-series grabs.
    #[tokio::test]
    async fn auto_expand_noop_when_no_siblings_detected() {
        let db = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite");
        crate::models::migrate(&db).await.expect("migrate");

        let (parent_id, _) = series::upsert(
            &db,
            series::SeriesCore {
                anilist_id: 901,
                mal_id: None,
                title: "Sono Bisque Doll wa Koi wo Suru",
                title_romaji: "Sono Bisque Doll wa Koi wo Suru",
                title_english: "My Dress-Up Darling",
                title_native: "",
                cover_url: "",
                format: "TV",
                status: "FINISHED",
                episodes: Some(12),
                season_year: Some(2022),
                end_year: Some(2022),
            },
        )
        .await
        .expect("parent upsert");

        let grab_id = grabbed_torrents::record_grab(
            &db,
            "dummyhash1111111111111111111111111111111",
            "[Group] My Dress-Up Darling S01 (BD 1080p)",
            parent_id,
            &[],
            true,
        )
        .await
        .expect("record_grab")
        .expect("grab row inserted");

        // No relations on the parent detail → no sibling candidates
        // even though the file list is full of media files.
        let parent_detail = empty_anime_detail(901, "My Dress-Up Darling");
        let filenames = vec![
            "[Group] My Dress-Up Darling - 01 [BD 1080p].mkv".to_string(),
            "[Group] My Dress-Up Darling - 02 [BD 1080p].mkv".to_string(),
        ];

        let added = auto_expand_library_from_pack_with_files(
            &db,
            &filenames,
            &parent_detail,
            parent_id,
            &[1, 2],
            grab_id,
            "[Group] My Dress-Up Darling S01 (BD 1080p)",
        )
        .await;

        assert_eq!(added, 0);
        let routes = grabbed_torrents::get_series_routes(&db, grab_id)
            .await
            .expect("get_series_routes");
        assert!(
            routes.is_empty(),
            "no sibling → no routes, post-processing falls back to grab.series_id"
        );
    }
}
