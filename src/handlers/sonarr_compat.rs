//! Sonarr v3 API compatibility layer for Seerr integration.
//!
//! Implements the subset of Sonarr's API that Seerr calls, translating
//! requests into Ryokan's internal data model.

use axum::{
    extract::{Path, Query, State},
    http::{Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::models::{config, monitoring, series};
use crate::services::{anibridge, anilist, logger, media, monitoring as monitoring_service};
use crate::models::log::LogCategory;
use crate::AppState;

// ── Authentication middleware ──────────────────────────────────────────────

/// Middleware that validates the API key from the `X-Api-Key` header or
/// `?apikey=` query parameter against the configured Sonarr API key.
/// Returns 401 if missing/invalid or if the Sonarr compat layer is disabled.
pub async fn require_api_key(
    State(state): State<AppState>,
    req: Request<axum::body::Body>,
    next: Next,
) -> Response {
    // 503 (with Retry-After) for transient config-load failures and
    // for "config row missing" (fresh install, user hasn't saved
    // settings yet). Returning 500 here would have Seerr mark the
    // indexer broken and back off for a long window — 503 advertises
    // "try again soon" instead. The 401 (UNAUTHORIZED) path stays
    // for "key mismatch" so a real auth failure is still visible.
    let cfg = match config::get_config(&state.db).await {
        Ok(Some(c)) => c,
        Ok(None) | Err(_) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                [(axum::http::header::RETRY_AFTER, "5")],
                "Ryokan config not yet available",
            )
                .into_response();
        }
    };

    if !cfg.sonarr_enabled || cfg.sonarr_api_key.is_empty() {
        return (StatusCode::SERVICE_UNAVAILABLE, "Sonarr API compatibility layer is disabled").into_response();
    }

    // Check X-Api-Key header first, then fall back to ?apikey= query param.
    // Query-string values are percent-decoded — Seerr URL-encodes apikey
    // values that contain `+`, `=`, `&`, or `%` (all legal in API keys
    // and not restricted by the settings UI), so a raw string compare
    // would silently reject every Seerr request whose key contained any
    // of those characters.
    let api_key = req
        .headers()
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .or_else(|| {
            let query_str = req.uri().query().unwrap_or("");
            query_str.split('&').find_map(|pair| {
                let (key, val) = pair.split_once('=')?;
                if key == "apikey" {
                    Some(urlencoding::decode(val).ok()?.into_owned())
                } else {
                    None
                }
            })
        });

    // Constant-time compare so the equality check itself never becomes a
    // timing oracle. The threat is largely theoretical over the network,
    // but it costs nothing to remove.
    let valid = match &api_key {
        Some(key) => bool::from(subtle::ConstantTimeEq::ct_eq(
            key.as_bytes(),
            cfg.sonarr_api_key.as_bytes(),
        )),
        None => false,
    };
    if valid {
        next.run(req).await
    } else {
        (StatusCode::UNAUTHORIZED, "Invalid or missing API key").into_response()
    }
}

// ── Sonarr-compatible types ────────────────────────────────────────────────

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SonarrSeries {
    pub id: i64,
    pub title: String,
    pub sort_title: String,
    pub status: String,
    pub overview: String,
    pub network: String,
    pub air_time: String,
    pub images: Vec<SonarrImage>,
    pub remote_poster: String,
    pub seasons: Vec<SonarrSeason>,
    pub year: i32,
    pub path: String,
    pub profile_id: i32,
    pub language_profile_id: i32,
    pub season_folder: bool,
    pub monitored: bool,
    pub use_scene_numbering: bool,
    pub runtime: i32,
    pub tvdb_id: i64,
    pub tv_rage_id: i64,
    pub tv_maze_id: i64,
    pub first_aired: String,
    pub series_type: String,
    pub clean_title: String,
    pub imdb_id: String,
    pub title_slug: String,
    pub certification: String,
    pub genres: Vec<String>,
    pub tags: Vec<i32>,
    pub added: String,
    pub ratings: SonarrRatings,
    pub quality_profile_id: i32,
    pub root_folder_path: String,
    pub statistics: SonarrStatistics,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SonarrImage {
    pub cover_type: String,
    pub url: String,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SonarrSeason {
    pub season_number: i32,
    pub monitored: bool,
    pub statistics: SonarrSeasonStats,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SonarrSeasonStats {
    pub episode_file_count: i32,
    pub episode_count: i32,
    pub total_episode_count: i32,
    pub size_on_disk: i64,
    pub percent_of_episodes: f64,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SonarrRatings {
    pub votes: i32,
    pub value: f64,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SonarrStatistics {
    pub season_count: i32,
    pub episode_file_count: i32,
    pub episode_count: i32,
    pub total_episode_count: i32,
    pub size_on_disk: i64,
    pub percent_of_episodes: f64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QualityProfile {
    id: i32,
    name: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RootFolder {
    id: i32,
    path: String,
    free_space: i64,
    total_space: i64,
    unmapped_folders: Vec<()>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LanguageProfile {
    id: i32,
    name: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Tag {
    id: i32,
    label: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemStatus {
    version: String,
    build_time: String,
    is_debug: bool,
    is_production: bool,
    is_admin: bool,
    is_user_interactive: bool,
    startup_path: String,
    app_data: String,
    os_name: String,
    os_version: String,
    is_net_core: bool,
    is_mono: bool,
    is_linux: bool,
    is_osx: bool,
    is_windows: bool,
    is_docker: bool,
    mode: String,
    branch: String,
    authentication: String,
    sqlite_version: String,
    migration_version: i32,
    url_base: String,
    runtime_version: String,
    runtime_name: String,
    start_time: String,
    package_update_mechanism: String,
    app_name: String,
}

// ── Request types ──────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct SeriesLookupQuery {
    term: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct AddSeriesBody {
    pub tvdb_id: Option<i64>,
    pub title: Option<String>,
    pub quality_profile_id: Option<i32>,
    pub language_profile_id: Option<i32>,
    pub seasons: Option<Vec<SeasonInput>>,
    pub tags: Option<Vec<i32>>,
    pub season_folder: Option<bool>,
    pub monitored: Option<bool>,
    pub root_folder_path: Option<String>,
    pub series_type: Option<String>,
    pub add_options: Option<AddOptions>,
    // Extra fields Seerr may send — we preserve them on passthrough.
    pub title_slug: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SeasonInput {
    pub season_number: i32,
    pub monitored: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct AddOptions {
    pub ignore_episodes_with_files: Option<bool>,
    pub search_for_missing_episodes: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct UpdateSeriesBody {
    pub id: i64,
    pub monitored: Option<bool>,
    pub seasons: Option<Vec<SeasonInput>>,
    pub tags: Option<Vec<i32>>,
    // Accept and ignore all other fields Seerr passes through.
    #[serde(flatten)]
    _extra: HashMap<String, serde_json::Value>,
}

#[derive(Deserialize)]
pub struct CommandBody {
    pub name: Option<String>,
    #[serde(rename = "seriesId")]
    pub series_id: Option<i64>,
}

#[derive(Deserialize)]
pub struct TagBody {
    pub label: Option<String>,
}

// ── Handlers ───────────────────────────────────────────────────────────────

/// GET /api/v3/system/status
pub async fn system_status() -> Json<SystemStatus> {
    Json(SystemStatus {
        version: "3.0.9.1549".to_string(),
        build_time: "2024-01-01T00:00:00Z".to_string(),
        is_debug: false,
        is_production: true,
        is_admin: false,
        is_user_interactive: false,
        startup_path: String::new(),
        app_data: String::new(),
        os_name: "linux".to_string(),
        os_version: String::new(),
        is_net_core: true,
        is_mono: false,
        is_linux: true,
        is_osx: false,
        is_windows: false,
        is_docker: false,
        mode: "default".to_string(),
        branch: "main".to_string(),
        authentication: "none".to_string(),
        sqlite_version: String::new(),
        migration_version: 0,
        url_base: String::new(),
        runtime_version: String::new(),
        runtime_name: String::new(),
        start_time: "2024-01-01T00:00:00Z".to_string(),
        package_update_mechanism: "builtIn".to_string(),
        app_name: "Ryokan".to_string(),
    })
}

/// GET /api/v3/qualityprofile
pub async fn quality_profiles() -> Json<Vec<QualityProfile>> {
    Json(vec![QualityProfile {
        id: 1,
        name: "Default".to_string(),
    }])
}

/// GET /api/v3/rootfolder
pub async fn root_folders(
    State(state): State<AppState>,
) -> Json<Vec<RootFolder>> {
    let media_root = config::get_config(&state.db)
        .await
        .ok()
        .flatten()
        .map(|c| c.media_root)
        .unwrap_or_default();

    let path = if media_root.is_empty() { "/media".to_string() } else { media_root };

    Json(vec![RootFolder {
        id: 1,
        path,
        free_space: 0,
        total_space: 0,
        unmapped_folders: vec![],
    }])
}

/// GET /api/v3/languageprofile
pub async fn language_profiles() -> Json<Vec<LanguageProfile>> {
    Json(vec![LanguageProfile {
        id: 1,
        name: "English".to_string(),
    }])
}

/// GET /api/v3/tag
pub async fn list_tags() -> Json<Vec<Tag>> {
    Json(vec![])
}

/// POST /api/v3/tag
pub async fn create_tag(
    Json(body): Json<TagBody>,
) -> Json<Tag> {
    Json(Tag {
        id: 1,
        label: body.label.unwrap_or_default(),
    })
}

/// GET /api/v3/series/lookup?term=...
///
/// Seerr sends either `term=tvdb:12345` for TVDB ID lookup or `term=Title` for title search.
/// The ID in the `tvdb:` prefix is a real TVDB ID (Sonarr natively uses TVDB).
pub async fn series_lookup(
    State(state): State<AppState>,
    Query(params): Query<SeriesLookupQuery>,
) -> Result<Json<Vec<SonarrSeries>>, (StatusCode, String)> {
    anibridge::ensure_loaded().await;
    let cfg = config::get_config(&state.db)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .unwrap_or_default();

    if let Some(tvdb_id_str) = params.term.strip_prefix("tvdb:") {
        // TVDB ID lookup — try anibridge TVDB index first, then TMDB as fallback.
        let tvdb_id: i64 = tvdb_id_str
            .trim()
            .parse()
            .map_err(|_| (StatusCode::BAD_REQUEST, "Invalid ID".to_string()))?;

        return lookup_by_external_id(&state, &cfg, tvdb_id).await;
    }

    // Title search — search AniList and return results in Sonarr format.
    let results = anilist::search_anime(&params.term)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e))?;

    let mut sonarr_results = Vec::new();
    for r in results {
        let db_series = series::get_by_anilist_id(&state.db, r.id)
            .await
            .ok()
            .flatten();

        let tmdb_id = anibridge::resolve_tmdb_id(r.id, r.id_mal).await;
        let title = if !r.title_english.is_empty() { &r.title_english } else { &r.title_romaji };

        sonarr_results.push(build_sonarr_series_from_search(
            &r,
            title,
            tmdb_id,
            db_series.as_ref(),
            &cfg,
        ));
    }

    Ok(Json(sonarr_results))
}

/// GET /api/v3/series — list all tracked series.
pub async fn list_series(
    State(state): State<AppState>,
) -> Result<Json<Vec<SonarrSeries>>, (StatusCode, String)> {
    anibridge::ensure_loaded().await;
    let cfg = config::get_config(&state.db)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .unwrap_or_default();

    let tracked = series::get_all(&state.db)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let mut results = Vec::new();
    for s in &tracked {
        let tmdb_id = anibridge::resolve_tmdb_id(s.anilist_id, s.mal_id).await;
        results.push(build_sonarr_series_from_tracked(s, tmdb_id, &cfg));
    }

    Ok(Json(results))
}

/// GET /api/v3/series/{id} — get a single tracked series by Ryokan's internal ID.
pub async fn get_series(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<SonarrSeries>, (StatusCode, String)> {
    anibridge::ensure_loaded().await;
    let cfg = config::get_config(&state.db)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .unwrap_or_default();

    let s = series::get_by_id(&state.db, id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "Series not found".to_string()))?;

    let tmdb_id = anibridge::resolve_tmdb_id(s.anilist_id, s.mal_id).await;
    Ok(Json(build_sonarr_series_from_tracked(&s, tmdb_id, &cfg)))
}

/// POST /api/v3/series — add a new series.
///
/// Seerr sends tvdbId which is a real TVDB ID. We map it to AniList/MAL via
/// anibridge, add the series to Ryokan's library, and return a Sonarr-format response.
pub async fn add_series(
    State(state): State<AppState>,
    Json(body): Json<AddSeriesBody>,
) -> Result<Json<SonarrSeries>, (StatusCode, String)> {
    let tvdb_id = body.tvdb_id.unwrap_or(0);

    // Extract which season Seerr is requesting. Seerr marks exactly one season as
    // monitored per request, so .max() effectively picks the single monitored season.
    let requested_season = body.seasons.as_ref().and_then(|seasons| {
        seasons.iter()
            .filter(|s| s.monitored && s.season_number > 0)
            .map(|s| s.season_number)
            .max()
    });

    tracing::info!(
        "Seerr add_series: tvdb_id={}, title={:?}, requested_season={:?}, seasons={:?}",
        tvdb_id, body.title, requested_season, body.seasons,
    );

    // Resolve TVDB + season → AniList/MAL IDs via anibridge.
    anibridge::ensure_loaded().await;
    let mut anime_ids = anibridge::lookup_by_tvdb(tvdb_id, requested_season).await;
    if anime_ids.is_empty() {
        anime_ids = anibridge::lookup_by_tmdb(tvdb_id, requested_season).await;
    }

    // #26 — TMDB often models multi-cour anime as one flat season
    // (JJK, Bleach TYBW, Demon Slayer) so Seerr requests "season 1"
    // even when it covers 2–3 AL entries. Sort by AL ID ascending so
    // the earliest-aired cour is the "primary" we return to Seerr —
    // AL IDs are assigned chronologically at entry-creation time, so
    // this picks the right cour as the face of the request even though
    // all siblings get added below.
    anime_ids.sort_by_key(|a| a.anilist_id.unwrap_or(i64::MAX));

    tracing::info!(
        "Anibridge resolved TVDB {} season {:?} → {} entries: {:?}",
        tvdb_id, requested_season, anime_ids.len(), anime_ids,
    );

    // #26 — Squashed-merge detection. >1 AL entry means TMDB collapsed
    // multiple cours into a single season; we fan out one Ryokan series
    // per AL entry so a single Seerr request seeds all the relevant
    // cours at once. Single-entry adds skip the fan-out path and behave
    // exactly as before.
    let is_squashed_merge = anime_ids.len() > 1;

    // Squashed-merge regrab shortcut: if every AL entry the anibridge
    // mapping returned is already present in Ryokan's DB, skip all
    // AL/Jikan detail fetches — the user has already added this
    // franchise once, Seerr is just re-asking. We still re-apply
    // monitoring and re-trigger auto-search below so a regrab from the
    // Seerr UI actually does something.
    let existing_siblings: Vec<Option<series::Series>> = {
        let mut v = Vec::with_capacity(anime_ids.len());
        for ids in &anime_ids {
            let row = if let Some(al) = ids.anilist_id {
                series::get_by_anilist_id(&state.db, al)
                    .await
                    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
            } else if let Some(mal) = ids.mal_id {
                series::get_by_mal_id(&state.db, mal)
                    .await
                    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
            } else {
                None
            };
            v.push(row);
        }
        v
    };
    let all_exist = !anime_ids.is_empty() && existing_siblings.iter().all(|o| o.is_some());

    // Gather the series rows we ultimately apply monitoring + auto-search
    // to. For the regrab path this is populated from `existing_siblings`
    // directly. For the fresh (or partial-fresh) path each missing entry
    // takes the AL/Jikan fetch + upsert branch below.
    let mut processed: Vec<series::Series> = Vec::new();
    // Used purely for the "Added via Seerr: <title>" log line so a regrab
    // doesn't claim to have "added" anything.
    let mut newly_added = 0_usize;

    if all_exist {
        for row in existing_siblings.iter().flatten() {
            processed.push(row.clone());
        }
        if is_squashed_merge {
            logger::info(
                &state.db,
                LogCategory::Library,
                &format!(
                    "Seerr regrab: TVDB {} season {:?} already mapped to {} existing series — skipping AL fetch",
                    tvdb_id, requested_season, processed.len()
                ),
                &format!("series_ids={:?}", processed.iter().map(|s| s.id).collect::<Vec<_>>()),
            ).await;
        }
    } else if !anime_ids.is_empty()
        && anime_ids.iter().any(|a| a.anilist_id.is_some() || a.mal_id.is_some())
    {
        if is_squashed_merge {
            let al_ids: Vec<i64> = anime_ids.iter().filter_map(|a| a.anilist_id).collect();
            logger::info(
                &state.db,
                LogCategory::Library,
                &format!(
                    "Seerr add: TVDB {} season {:?} maps to {} AniList entries — fanning out",
                    tvdb_id, requested_season, anime_ids.len()
                ),
                &format!("al_ids_sorted={:?}", al_ids),
            ).await;
        }

        // Pre-batch every still-missing AniList id into one
        // `Page(media(id_in:[]))` call so the per-entry loop below
        // hits DETAIL_CACHE instead of issuing N sequential GraphQL
        // queries. A 7-entry fan-out becomes 1 round-trip + 7 cache
        // hits instead of 7 throttled-serial round-trips.
        let prefetch_ids: Vec<i64> = anime_ids
            .iter()
            .zip(existing_siblings.iter())
            .filter_map(|(ids, existing)| {
                if existing.is_some() {
                    None
                } else {
                    ids.anilist_id.filter(|id| *id > 0)
                }
            })
            .collect();
        if !prefetch_ids.is_empty() {
            if let Err(e) = anilist::get_anime_details_batch(&prefetch_ids).await {
                tracing::debug!(
                    "Seerr add: AL batch prefetch failed (per-id loop will retry): {e}"
                );
            }
        }

        for (ids, existing) in anime_ids.iter().zip(existing_siblings.into_iter()) {
            // Skip entries that already exist — regrab-within-fresh case.
            // A partial fan-out (user added JJK S1 manually last month, now
            // Seerr adds JJK) shouldn't re-fetch S1's detail.
            if let Some(row) = existing {
                processed.push(row);
                continue;
            }
            if ids.anilist_id.is_none() && ids.mal_id.is_none() {
                continue;
            }

            let detail_result = if let Some(al_id) = ids.anilist_id {
                match anilist::get_anime_detail(al_id).await {
                    Ok(d) => Ok(d),
                    Err(_) if ids.mal_id.is_some() => {
                        crate::services::jikan::get_anime_detail_cached(ids.mal_id.unwrap()).await
                    }
                    Err(e) => Err(e),
                }
            } else if let Some(mal_id) = ids.mal_id {
                crate::services::jikan::get_anime_detail_cached(mal_id).await
            } else {
                unreachable!("checked above");
            };

            let detail = match detail_result {
                Ok(d) => d,
                Err(e) => {
                    // In a squashed fan-out, a single sibling's detail
                    // fetch failing shouldn't nuke the whole add — log
                    // it and keep going. For a single-entry add this
                    // leaves `processed` empty and we fall through to
                    // the error return below.
                    logger::warn(
                        &state.db,
                        LogCategory::Library,
                        &format!(
                            "Seerr add: skipped AL entry {:?} / MAL {:?} after detail fetch failure",
                            ids.anilist_id, ids.mal_id
                        ),
                        &e,
                    ).await;
                    continue;
                }
            };

            let title = if !detail.title_english.is_empty() { &detail.title_english } else { &detail.title_romaji };
            let (id, _created) = series::upsert(
                &state.db,
                series::SeriesCore {
                    anilist_id: detail.id,
                    mal_id: detail.id_mal,
                    title,
                    title_romaji: &detail.title_romaji,
                    title_english: &detail.title_english,
                    title_native: &detail.title_native,
                    cover_url: &detail.cover_url,
                    format: &detail.format,
                    status: &detail.status,
                    episodes: detail.episodes,
                    season_year: detail.season_year,
                    end_year: detail.end_year,
                },
            )
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

            let s = series::get_by_id(&state.db, id)
                .await
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
                .ok_or((StatusCode::INTERNAL_SERVER_ERROR, "Series not found after insert".to_string()))?;
            processed.push(s);
            newly_added += 1;
        }
    } else {
        // No anibridge mapping — fall back to AniList title search.
        // Single-path only: we have nothing to fan out over.
        let search_title = body.title.as_deref().unwrap_or("");
        if search_title.is_empty() {
            return Err((StatusCode::BAD_REQUEST, format!("No mapping for TVDB ID {} and no title provided", tvdb_id)));
        }
        tracing::info!("No anibridge mapping for TVDB {}; searching AniList for '{}'", tvdb_id, search_title);

        let results = anilist::search_anime(search_title)
            .await
            .map_err(|e| (StatusCode::BAD_GATEWAY, e))?;

        let best = results.first()
            .ok_or((StatusCode::NOT_FOUND, format!("No AniList results for '{}'", search_title)))?;

        let detail = anilist::get_anime_detail(best.id)
            .await
            .map_err(|e| (StatusCode::BAD_GATEWAY, e))?;

        let title = if !detail.title_english.is_empty() { &detail.title_english } else { &detail.title_romaji };
        let (id, _created) = series::upsert(
            &state.db,
            series::SeriesCore {
                anilist_id: detail.id,
                mal_id: detail.id_mal,
                title,
                title_romaji: &detail.title_romaji,
                title_english: &detail.title_english,
                title_native: &detail.title_native,
                cover_url: &detail.cover_url,
                format: &detail.format,
                status: &detail.status,
                episodes: detail.episodes,
                season_year: detail.season_year,
                end_year: detail.end_year,
            },
        )
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

        let s = series::get_by_id(&state.db, id)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
            .ok_or((StatusCode::INTERNAL_SERVER_ERROR, "Series not found after insert".to_string()))?;
        processed.push(s);
        newly_added += 1;
    }

    if processed.is_empty() {
        return Err((
            StatusCode::BAD_GATEWAY,
            format!("No usable AniList/MAL mapping for TVDB ID {}", tvdb_id),
        ));
    }

    // Set monitoring based on what Seerr requested. Applied to every
    // sibling in a fan-out — Seerr's "monitor this season" intent covers
    // the whole squashed merge, so each fanned-out cour inherits it.
    let should_monitor = if let Some(ref seasons) = body.seasons {
        if let Some(req_s) = requested_season {
            seasons.iter().any(|s| s.season_number == req_s && s.monitored)
        } else {
            body.monitored.unwrap_or(true)
        }
    } else {
        body.monitored.unwrap_or(true)
    };
    let monitor_mode = if should_monitor {
        monitoring::MonitorMode::All
    } else {
        monitoring::MonitorMode::None
    };
    for s in &processed {
        let _ = monitoring_service::apply_monitor_mode(&state.db, s.id, monitor_mode).await;
    }

    if newly_added > 0 {
        let primary_title = &processed[0].title;
        let added_ids: Vec<i64> = processed.iter().map(|s| s.id).collect();
        logger::info(
            &state.db,
            LogCategory::Library,
            &format!(
                "Added via Seerr: {}{}",
                primary_title,
                if is_squashed_merge {
                    format!(" (+{} sibling cour{})", processed.len().saturating_sub(1), if processed.len() == 2 { "" } else { "s" })
                } else {
                    String::new()
                }
            ),
            &format!("tvdb_id={}, series_ids={:?}, newly_added={}", tvdb_id, added_ids, newly_added),
        ).await;
    }

    // Auto-search if requested. Each sibling in a fan-out gets its own
    // spawned auto-search — the grab-time hydration hook inside
    // `auto_search_series` will lazily walk each series' PREQUEL chain
    // and set `cumulative_prior_episodes` on first run so absolute-
    // numbered releases route to the right cour.
    if body.add_options
        .as_ref()
        .and_then(|o| o.search_for_missing_episodes)
        .unwrap_or(false)
    {
        for s in &processed {
            let state_clone = state.clone();
            let id = s.id;
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                let _ = super::library::auto_search_series(
                    axum::extract::State(state_clone),
                    axum::extract::Path(id),
                    axum::extract::Query(super::library::AutoSearchQuery::default()),
                ).await;
            });
        }
    }

    let cfg = config::get_config(&state.db)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();

    // Return the primary (earliest AL ID) as the Sonarr payload. Seerr
    // keys off tvdb_id, not the Ryokan series id, so a single response
    // representing the whole request is what it expects — the sibling
    // cours exist in Ryokan's DB but don't need to be reflected in the
    // Sonarr response shape.
    let primary = &processed[0];
    Ok(Json(build_sonarr_series_from_tracked(primary, tvdb_id, &cfg)))
}

/// PUT /api/v3/series — update an existing series.
pub async fn update_series(
    State(state): State<AppState>,
    Json(body): Json<UpdateSeriesBody>,
) -> Result<Json<SonarrSeries>, (StatusCode, String)> {
    anibridge::ensure_loaded().await;
    let s = series::get_by_id(&state.db, body.id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "Series not found".to_string()))?;

    // Update monitoring.
    if let Some(monitored) = body.monitored {
        let mode = if monitored {
            monitoring::MonitorMode::All
        } else {
            monitoring::MonitorMode::None
        };
        let _ = monitoring_service::apply_monitor_mode(&state.db, s.id, mode).await;
    }

    logger::info(
        &state.db,
        LogCategory::Library,
        &format!("Updated via Seerr: {}", s.title),
        &format!("id={}, monitored={:?}", s.id, body.monitored),
    ).await;

    let cfg = config::get_config(&state.db)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();

    let tmdb_id = anibridge::resolve_tmdb_id(s.anilist_id, s.mal_id).await;
    Ok(Json(build_sonarr_series_from_tracked(&s, tmdb_id, &cfg)))
}

/// POST /api/v3/command — execute a command. Seerr sends SeriesSearch.
pub async fn execute_command(
    State(state): State<AppState>,
    Json(body): Json<CommandBody>,
) -> Json<serde_json::Value> {
    let name = body.name.unwrap_or_default();

    if name == "SeriesSearch"
        && let Some(series_id) = body.series_id {
            let state_clone = state.clone();
            tokio::spawn(async move {
                let _ = super::library::auto_search_series(
                    axum::extract::State(state_clone),
                    axum::extract::Path(series_id),
                    axum::extract::Query(super::library::AutoSearchQuery::default()),
                ).await;
            });
        }

    Json(serde_json::json!({
        "id": 1,
        "name": name,
        "commandName": name,
        "status": "queued",
        "queued": chrono::Utc::now().to_rfc3339(),
    }))
}

// ── Helpers ────────────────────────────────────────────────────────────────

/// Look up anime by external ID (TVDB or TMDB). Tries TVDB index first since
/// Sonarr/Seerr sends real TVDB IDs, then falls back to TMDB index.
///
/// Returns a SINGLE series with multiple seasons (one per AniList entry) when
/// the TVDB ID maps to multiple anibridge seasons. This matches how real Sonarr
/// returns multi-season shows, allowing Seerr to request specific seasons.
async fn lookup_by_external_id(
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
        tracing::warn!("No anibridge mapping for TVDB ID {}; returning stub for Seerr", tvdb_id);
        return Ok(Json(vec![build_stub_series(tvdb_id, cfg)]));
    }

    // Fetch metadata for the first entry to use as the "show-level" info.
    // Note: for multi-season shows (e.g. JoJo), this uses season 1's AniList entry
    // for the title and cover art. This is fine since Seerr keys on tvdb_id, not
    // the title — but a Jikan fallback may return a part-specific title here.
    let first_ids = &season_entries[0].1;
    let show_detail = fetch_anime_detail(first_ids).await;
    let show_title = show_detail.as_ref().map(|d| {
        if !d.title_english.is_empty() { d.title_english.clone() } else { d.title_romaji.clone() }
    }).unwrap_or_else(|| format!("TVDB:{}", tvdb_id));
    let show_cover = show_detail.as_ref().map(|d| d.cover_url.clone()).unwrap_or_default();

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
    let year = show_detail.as_ref().and_then(|d| d.season_year).unwrap_or(0);

    let path = if cfg.media_root.is_empty() {
        format!("/media/{}", media::sanitize_folder_name(&show_title))
    } else {
        format!("{}/{}", cfg.media_root, media::sanitize_folder_name(&show_title))
    };

    let result = SonarrSeries {
        id: 0,
        title: show_title.clone(),
        sort_title: show_title.to_lowercase(),
        status: show_detail.as_ref().map(|d| map_status(&d.status)).unwrap_or_else(|| "continuing".to_string()),
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
        clean_title: show_title.to_lowercase().replace(|c: char| !c.is_alphanumeric(), ""),
        imdb_id: String::new(),
        title_slug: format!("ryokan-tvdb-{}", tvdb_id),
        certification: String::new(),
        genres: vec!["Anime".to_string()],
        tags: vec![],
        added: String::new(),
        ratings: SonarrRatings { votes: 0, value: 0.0 },
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
        tvdb_id, season_count,
    );

    Ok(Json(vec![result]))
}

/// Fetch anime detail from AniList or Jikan, returning None on failure.
async fn fetch_anime_detail(ids: &anibridge::AnimeIds) -> Option<anilist::AnimeDetail> {
    if let Some(al_id) = ids.anilist_id
        && let Ok(d) = anilist::get_anime_detail(al_id).await {
            return Some(d);
        }
    if let Some(mal_id) = ids.mal_id
        && let Ok(d) = crate::services::jikan::get_anime_detail_cached(mal_id).await {
            return Some(d);
        }
    None
}

fn build_sonarr_series_from_search(
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
                percent_of_episodes: if total_eps > 0 { (on_disk as f64 / total_eps as f64) * 100.0 } else { 0.0 },
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
        clean_title: title.to_lowercase().replace(|c: char| !c.is_alphanumeric(), ""),
        imdb_id: String::new(),
        title_slug: format!("ryokan-{}", r.id),
        certification: String::new(),
        genres: vec!["Anime".to_string()],
        tags: vec![],
        added: String::new(),
        ratings: SonarrRatings { votes: 0, value: 0.0 },
        quality_profile_id: 1,
        root_folder_path: cfg.media_root.clone(),
        statistics: SonarrStatistics {
            season_count: 1,
            episode_file_count: on_disk,
            episode_count: total_eps,
            total_episode_count: total_eps,
            size_on_disk: 0,
            percent_of_episodes: if total_eps > 0 { (on_disk as f64 / total_eps as f64) * 100.0 } else { 0.0 },
        },
    }
}

fn build_sonarr_series_from_tracked(
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

    let title = if !s.title.is_empty() { &s.title } else { &s.title_romaji };

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
                percent_of_episodes: if total_eps > 0 { (on_disk as f64 / total_eps as f64) * 100.0 } else { 0.0 },
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
        clean_title: title.to_lowercase().replace(|c: char| !c.is_alphanumeric(), ""),
        imdb_id: String::new(),
        title_slug: format!("ryokan-{}", s.anilist_id),
        certification: String::new(),
        genres: vec!["Anime".to_string()],
        tags: vec![],
        added: String::new(),
        ratings: SonarrRatings { votes: 0, value: 0.0 },
        quality_profile_id: 1,
        root_folder_path: cfg.media_root.clone(),
        statistics: SonarrStatistics {
            season_count: 1,
            episode_file_count: on_disk,
            episode_count: total_eps,
            total_episode_count: total_eps,
            size_on_disk: 0,
            percent_of_episodes: if total_eps > 0 { (on_disk as f64 / total_eps as f64) * 100.0 } else { 0.0 },
        },
    }
}

fn map_status(anilist_status: &str) -> String {
    match anilist_status.to_uppercase().as_str() {
        "RELEASING" | "NOT_YET_RELEASED" => "continuing".to_string(),
        "FINISHED" | "FINISHED_AIRING" | "CANCELLED" => "ended".to_string(),
        _ => "continuing".to_string(),
    }
}

/// Build a minimal stub SonarrSeries for TVDB IDs that anibridge can't resolve.
/// This lets Seerr proceed to the add step, where we resolve via AniList title search.
fn build_stub_series(tvdb_id: i64, cfg: &config::Config) -> SonarrSeries {
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
        ratings: SonarrRatings { votes: 0, value: 0.0 },
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

