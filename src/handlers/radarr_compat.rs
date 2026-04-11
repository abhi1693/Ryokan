//! Radarr v3 API compatibility layer for Seerr integration (anime movies).
//!
//! Implements the subset of Radarr's API that Seerr calls, translating
//! requests into Ryokan's internal data model. Mounted under `/radarr/api/v3/`.

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
/// `?apikey=` query parameter against the configured Radarr API key.
pub async fn require_api_key(
    State(state): State<AppState>,
    req: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let cfg = match config::get_config(&state.db).await {
        Ok(Some(c)) => c,
        _ => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };

    if !cfg.radarr_enabled || cfg.radarr_api_key.is_empty() {
        return (StatusCode::SERVICE_UNAVAILABLE, "Radarr API compatibility layer is disabled").into_response();
    }

    let api_key = req
        .headers()
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .or_else(|| {
            let query_str = req.uri().query().unwrap_or("");
            query_str.split('&').find_map(|pair| {
                let (key, val) = pair.split_once('=')?;
                if key == "apikey" { Some(val.to_string()) } else { None }
            })
        });

    match api_key {
        Some(key) if key == cfg.radarr_api_key => next.run(req).await,
        _ => (StatusCode::UNAUTHORIZED, "Invalid or missing API key").into_response(),
    }
}

// ── Radarr-compatible types ───────────────────────────────────────────────

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct RadarrMovie {
    pub id: i64,
    pub title: String,
    pub sort_title: String,
    pub status: String,
    pub overview: String,
    pub images: Vec<RadarrImage>,
    pub remote_poster: String,
    pub year: i32,
    pub path: String,
    pub quality_profile_id: i32,
    pub monitored: bool,
    pub minimum_availability: String,
    pub runtime: i32,
    pub tmdb_id: i64,
    pub imdb_id: String,
    pub title_slug: String,
    pub certification: String,
    pub genres: Vec<String>,
    pub tags: Vec<i32>,
    pub added: String,
    pub ratings: RadarrRatings,
    pub has_file: bool,
    pub is_available: bool,
    pub folder_name: String,
    pub clean_title: String,
    pub root_folder_path: String,
    pub movie_file: Option<RadarrMovieFile>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct RadarrImage {
    pub cover_type: String,
    pub url: String,
    pub remote_url: String,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct RadarrRatings {
    pub imdb: RadarrRatingValue,
    pub tmdb: RadarrRatingValue,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct RadarrRatingValue {
    pub votes: i32,
    pub value: f64,
    #[serde(rename = "type")]
    pub rating_type: String,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct RadarrMovieFile {
    pub id: i64,
    pub relative_path: String,
    pub size: i64,
    pub quality: serde_json::Value,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RadarrQualityProfile {
    id: i32,
    name: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RadarrRootFolder {
    id: i32,
    path: String,
    free_space: i64,
    accessible: bool,
    unmapped_folders: Vec<()>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RadarrTag {
    id: i32,
    label: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RadarrSystemStatus {
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
pub struct MovieLookupQuery {
    term: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct AddMovieBody {
    pub tmdb_id: Option<i64>,
    pub title: Option<String>,
    pub quality_profile_id: Option<i32>,
    pub monitored: Option<bool>,
    pub root_folder_path: Option<String>,
    pub minimum_availability: Option<String>,
    pub tags: Option<Vec<i32>>,
    pub add_options: Option<AddMovieOptions>,
    pub title_slug: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct AddMovieOptions {
    pub search_for_movie: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct UpdateMovieBody {
    pub id: i64,
    pub monitored: Option<bool>,
    pub tags: Option<Vec<i32>>,
    #[serde(flatten)]
    _extra: HashMap<String, serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RadarrCommandBody {
    pub name: Option<String>,
    pub movie_ids: Option<Vec<i64>>,
}

#[derive(Deserialize)]
pub struct RadarrTagBody {
    pub label: Option<String>,
}

// ── Handlers ───────────────────────────────────────────────────────────────

/// GET /radarr/api/v3/system/status
pub async fn system_status() -> Json<RadarrSystemStatus> {
    Json(RadarrSystemStatus {
        version: "5.2.6.8376".to_string(),
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

/// GET /radarr/api/v3/qualityprofile
pub async fn quality_profiles() -> Json<Vec<RadarrQualityProfile>> {
    Json(vec![RadarrQualityProfile {
        id: 1,
        name: "Default".to_string(),
    }])
}

/// GET /radarr/api/v3/rootfolder
pub async fn root_folders(
    State(state): State<AppState>,
) -> Json<Vec<RadarrRootFolder>> {
    let media_root = config::get_config(&state.db)
        .await
        .ok()
        .flatten()
        .map(|c| c.media_root)
        .unwrap_or_default();

    let path = if media_root.is_empty() { "/media".to_string() } else { media_root };

    Json(vec![RadarrRootFolder {
        id: 1,
        path,
        free_space: 0,
        accessible: true,
        unmapped_folders: vec![],
    }])
}

/// GET /radarr/api/v3/tag
pub async fn list_tags() -> Json<Vec<RadarrTag>> {
    Json(vec![])
}

/// POST /radarr/api/v3/tag
pub async fn create_tag(
    Json(body): Json<RadarrTagBody>,
) -> Json<RadarrTag> {
    Json(RadarrTag {
        id: 1,
        label: body.label.unwrap_or_default(),
    })
}

/// GET /radarr/api/v3/movie/lookup?term=tmdb:12345
///
/// Seerr sends `term=tmdb:12345` for TMDB ID lookup.
pub async fn movie_lookup(
    State(state): State<AppState>,
    Query(params): Query<MovieLookupQuery>,
) -> Result<Json<Vec<RadarrMovie>>, (StatusCode, String)> {
    let cfg = config::get_config(&state.db)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .unwrap_or_default();

    if let Some(tmdb_id_str) = params.term.strip_prefix("tmdb:") {
        let tmdb_id: i64 = tmdb_id_str
            .trim()
            .parse()
            .map_err(|_| (StatusCode::BAD_REQUEST, "Invalid TMDB ID".to_string()))?;

        return lookup_by_tmdb_id(&state, &cfg, tmdb_id).await;
    }

    // Title search fallback.
    let results = anilist::search_anime(&params.term)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e))?;

    let mut movies = Vec::new();
    for r in results {
        let db_series = series::get_by_anilist_id(&state.db, r.id)
            .await
            .ok()
            .flatten();

        let tmdb_id = resolve_tmdb_id(r.id, r.id_mal).await;
        let title = if !r.title_english.is_empty() { &r.title_english } else { &r.title_romaji };

        movies.push(build_radarr_movie_from_search(
            &r, title, tmdb_id, db_series.as_ref(), &cfg,
        ));
    }

    Ok(Json(movies))
}

/// GET /radarr/api/v3/movie — list all tracked series as movies.
pub async fn list_movies(
    State(state): State<AppState>,
) -> Result<Json<Vec<RadarrMovie>>, (StatusCode, String)> {
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
        let tmdb_id = resolve_tmdb_id(s.anilist_id, s.mal_id).await;
        results.push(build_radarr_movie_from_tracked(s, tmdb_id, &cfg));
    }

    Ok(Json(results))
}

/// GET /radarr/api/v3/movie/{id}
pub async fn get_movie(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<RadarrMovie>, (StatusCode, String)> {
    anibridge::ensure_loaded().await;
    let cfg = config::get_config(&state.db)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .unwrap_or_default();

    let s = series::get_by_id(&state.db, id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "Movie not found".to_string()))?;

    let tmdb_id = resolve_tmdb_id(s.anilist_id, s.mal_id).await;
    Ok(Json(build_radarr_movie_from_tracked(&s, tmdb_id, &cfg)))
}

/// POST /radarr/api/v3/movie — add a new movie (anime).
pub async fn add_movie(
    State(state): State<AppState>,
    Json(body): Json<AddMovieBody>,
) -> Result<Json<RadarrMovie>, (StatusCode, String)> {
    let tmdb_id = body.tmdb_id.unwrap_or(0);

    anibridge::ensure_loaded().await;
    let anime_ids = anibridge::lookup_by_tmdb(tmdb_id).await;

    let detail = if let Some(ids) = anime_ids.first().filter(|a| a.anilist_id.is_some() || a.mal_id.is_some()) {
        if let Some(al_id) = ids.anilist_id {
            match anilist::get_anime_detail(al_id).await {
                Ok(d) => d,
                Err(_) if ids.mal_id.is_some() => {
                    crate::services::jikan::get_anime_detail_cached(ids.mal_id.unwrap())
                        .await
                        .map_err(|e| (StatusCode::BAD_GATEWAY, e))?
                }
                Err(e) => return Err((StatusCode::BAD_GATEWAY, e)),
            }
        } else {
            crate::services::jikan::get_anime_detail_cached(ids.mal_id.unwrap())
                .await
                .map_err(|e| (StatusCode::BAD_GATEWAY, e))?
        }
    } else {
        // No anibridge mapping — fall back to AniList title search.
        let search_title = body.title.as_deref().unwrap_or("");
        if search_title.is_empty() {
            return Err((StatusCode::BAD_REQUEST, format!("No mapping for TMDB ID {} and no title provided", tmdb_id)));
        }
        tracing::info!("No anibridge mapping for TMDB {}; searching AniList for '{}'", tmdb_id, search_title);

        let results = anilist::search_anime(search_title)
            .await
            .map_err(|e| (StatusCode::BAD_GATEWAY, e))?;

        let best = results.first()
            .ok_or((StatusCode::NOT_FOUND, format!("No AniList results for '{}'", search_title)))?;

        anilist::get_anime_detail(best.id)
            .await
            .map_err(|e| (StatusCode::BAD_GATEWAY, e))?
    };

    let title = if !detail.title_english.is_empty() { &detail.title_english } else { &detail.title_romaji };

    let (id, _created) = series::upsert(
        &state.db,
        detail.id,
        detail.id_mal,
        title,
        &detail.title_romaji,
        &detail.title_english,
        &detail.title_native,
        &detail.cover_url,
        &detail.format,
        &detail.status,
        detail.episodes,
    )
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let monitor_mode = if body.monitored.unwrap_or(true) {
        monitoring::MonitorMode::All
    } else {
        monitoring::MonitorMode::None
    };
    let _ = monitoring_service::apply_monitor_mode(&state.db, id, monitor_mode).await;

    logger::info(
        &state.db,
        LogCategory::Library,
        &format!("Added via Seerr (Radarr): {}", title),
        &format!("tmdb_id={}, provider_id={}, id={}", tmdb_id, detail.id, id),
    ).await;

    // Auto-search if requested.
    if body.add_options
        .as_ref()
        .and_then(|o| o.search_for_movie)
        .unwrap_or(false)
    {
        let state_clone = state.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            let _ = super::library::auto_search_series(
                axum::extract::State(state_clone),
                axum::extract::Path(id),
            ).await;
        });
    }

    let cfg = config::get_config(&state.db)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();

    let s = series::get_by_id(&state.db, id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::INTERNAL_SERVER_ERROR, "Movie not found after insert".to_string()))?;

    Ok(Json(build_radarr_movie_from_tracked(&s, tmdb_id, &cfg)))
}

/// PUT /radarr/api/v3/movie — update an existing movie.
pub async fn update_movie(
    State(state): State<AppState>,
    Json(body): Json<UpdateMovieBody>,
) -> Result<Json<RadarrMovie>, (StatusCode, String)> {
    anibridge::ensure_loaded().await;
    let s = series::get_by_id(&state.db, body.id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "Movie not found".to_string()))?;

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
        &format!("Updated via Seerr (Radarr): {}", s.title),
        &format!("id={}, monitored={:?}", s.id, body.monitored),
    ).await;

    let cfg = config::get_config(&state.db)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();

    let tmdb_id = resolve_tmdb_id(s.anilist_id, s.mal_id).await;
    Ok(Json(build_radarr_movie_from_tracked(&s, tmdb_id, &cfg)))
}

/// POST /radarr/api/v3/command — execute a command. Seerr sends MoviesSearch.
pub async fn execute_command(
    State(state): State<AppState>,
    Json(body): Json<RadarrCommandBody>,
) -> Json<serde_json::Value> {
    let name = body.name.unwrap_or_default();

    if name == "MoviesSearch" {
        if let Some(movie_ids) = body.movie_ids {
            for movie_id in movie_ids {
                let state_clone = state.clone();
                tokio::spawn(async move {
                    let _ = super::library::auto_search_series(
                        axum::extract::State(state_clone),
                        axum::extract::Path(movie_id),
                    ).await;
                });
            }
        }
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

/// Resolve TMDB ID from either AniList ID or MAL ID.
async fn resolve_tmdb_id(anilist_id: i64, mal_id: impl Into<Option<i64>>) -> i64 {
    if let Some(tmdb) = anibridge::lookup_tmdb_by_anilist(anilist_id).await {
        return tmdb;
    }
    if let Some(mid) = mal_id.into() {
        if mid > 0 {
            if let Some(tmdb) = anibridge::lookup_tmdb_by_mal(mid).await {
                return tmdb;
            }
        }
    }
    0
}

async fn lookup_by_tmdb_id(
    state: &AppState,
    cfg: &config::Config,
    tmdb_id: i64,
) -> Result<Json<Vec<RadarrMovie>>, (StatusCode, String)> {
    anibridge::ensure_loaded().await;
    let anime_ids = anibridge::lookup_by_tmdb(tmdb_id).await;

    if anime_ids.is_empty() {
        tracing::warn!("No anibridge mapping for TMDB ID {}; returning stub movie for Seerr", tmdb_id);
        return Ok(Json(vec![build_stub_movie(tmdb_id, cfg)]));
    }

    let mut results = Vec::new();
    for ids in &anime_ids {
        let detail = if let Some(al_id) = ids.anilist_id {
            match anilist::get_anime_detail(al_id).await {
                Ok(d) => d,
                Err(_) if ids.mal_id.is_some() => {
                    match crate::services::jikan::get_anime_detail_cached(ids.mal_id.unwrap()).await {
                        Ok(d) => d,
                        Err(_) => continue,
                    }
                }
                Err(_) => continue,
            }
        } else if let Some(mal_id) = ids.mal_id {
            match crate::services::jikan::get_anime_detail_cached(mal_id).await {
                Ok(d) => d,
                Err(_) => continue,
            }
        } else {
            continue;
        };

        let db_series = if detail.id > 0 {
            series::get_by_anilist_id(&state.db, detail.id).await.ok().flatten()
        } else {
            None
        };

        let title = if !detail.title_english.is_empty() { &detail.title_english } else { &detail.title_romaji };

        let search_result = anilist::AnimeEntry {
            id: detail.id,
            id_mal: detail.id_mal,
            title_romaji: detail.title_romaji.clone(),
            title_english: detail.title_english.clone(),
            title_native: detail.title_native.clone(),
            cover_url: detail.cover_url.clone(),
            format: detail.format.clone(),
            status: detail.status.clone(),
            status_display: String::new(),
            episodes: detail.episodes,
            season_year: detail.season_year,
            source: if detail.id > 0 { "anilist" } else { "mal" }.to_string(),
        };

        results.push(build_radarr_movie_from_search(
            &search_result, title, tmdb_id, db_series.as_ref(), cfg,
        ));
    }

    Ok(Json(results))
}

fn build_radarr_movie_from_search(
    r: &anilist::AnimeEntry,
    title: &str,
    tmdb_id: i64,
    db_series: Option<&series::Series>,
    cfg: &config::Config,
) -> RadarrMovie {
    let is_in_library = db_series.is_some();
    let internal_id = db_series.map(|s| s.id).unwrap_or(0);

    let folder_name = db_series
        .map(|s| s.folder_name.clone())
        .unwrap_or_else(|| media::sanitize_folder_name(title));

    let has_file = if is_in_library {
        !media::scan_series_folder(&cfg.media_root, &folder_name).is_empty()
    } else {
        false
    };

    let path = if cfg.media_root.is_empty() {
        format!("/media/{}", folder_name)
    } else {
        format!("{}/{}", cfg.media_root, folder_name)
    };

    let monitored = db_series
        .map(|s| s.monitor_mode_enum() != monitoring::MonitorMode::None)
        .unwrap_or(true);

    RadarrMovie {
        id: internal_id,
        title: title.to_string(),
        sort_title: title.to_lowercase(),
        status: map_status(&r.status),
        overview: String::new(),
        images: vec![RadarrImage {
            cover_type: "poster".to_string(),
            url: r.cover_url.clone(),
            remote_url: r.cover_url.clone(),
        }],
        remote_poster: r.cover_url.clone(),
        year: r.season_year.unwrap_or(0),
        path,
        quality_profile_id: 1,
        monitored,
        minimum_availability: "released".to_string(),
        runtime: 24,
        tmdb_id,
        imdb_id: String::new(),
        title_slug: format!("ryokan-{}", r.id),
        certification: String::new(),
        genres: vec!["Anime".to_string()],
        tags: vec![],
        added: if is_in_library { "2024-01-01T00:00:00Z".to_string() } else { String::new() },
        ratings: default_ratings(),
        has_file,
        is_available: true,
        folder_name: folder_name.clone(),
        clean_title: title.to_lowercase().replace(|c: char| !c.is_alphanumeric(), ""),
        root_folder_path: cfg.media_root.clone(),
        movie_file: None,
    }
}

fn build_radarr_movie_from_tracked(
    s: &series::Series,
    tmdb_id: i64,
    cfg: &config::Config,
) -> RadarrMovie {
    let disk_files = media::scan_series_folder(&cfg.media_root, &s.folder_name);
    let has_file = !disk_files.is_empty();
    let monitored = s.monitor_mode_enum() != monitoring::MonitorMode::None;

    let path = if cfg.media_root.is_empty() {
        format!("/media/{}", s.folder_name)
    } else {
        format!("{}/{}", cfg.media_root, s.folder_name)
    };

    let title = if !s.title.is_empty() { &s.title } else { &s.title_romaji };

    RadarrMovie {
        id: s.id,
        title: title.to_string(),
        sort_title: title.to_lowercase(),
        status: map_status(&s.status),
        overview: String::new(),
        images: vec![RadarrImage {
            cover_type: "poster".to_string(),
            url: s.cover_url.clone(),
            remote_url: s.cover_url.clone(),
        }],
        remote_poster: s.cover_url.clone(),
        year: 0,
        path,
        quality_profile_id: 1,
        monitored,
        minimum_availability: "released".to_string(),
        runtime: 24,
        tmdb_id,
        imdb_id: String::new(),
        title_slug: format!("ryokan-{}", s.anilist_id),
        certification: String::new(),
        genres: vec!["Anime".to_string()],
        tags: vec![],
        added: "2024-01-01T00:00:00Z".to_string(),
        ratings: default_ratings(),
        has_file,
        is_available: true,
        folder_name: s.folder_name.clone(),
        clean_title: title.to_lowercase().replace(|c: char| !c.is_alphanumeric(), ""),
        root_folder_path: cfg.media_root.clone(),
        movie_file: None,
    }
}

fn map_status(anilist_status: &str) -> String {
    match anilist_status.to_uppercase().as_str() {
        "RELEASING" | "NOT_YET_RELEASED" => "announced".to_string(),
        "FINISHED" | "FINISHED_AIRING" | "CANCELLED" => "released".to_string(),
        _ => "released".to_string(),
    }
}

fn default_ratings() -> RadarrRatings {
    RadarrRatings {
        imdb: RadarrRatingValue { votes: 0, value: 0.0, rating_type: "user".to_string() },
        tmdb: RadarrRatingValue { votes: 0, value: 0.0, rating_type: "user".to_string() },
    }
}

fn build_stub_movie(tmdb_id: i64, cfg: &config::Config) -> RadarrMovie {
    let path = if cfg.media_root.is_empty() {
        "/media/Unknown".to_string()
    } else {
        format!("{}/Unknown", cfg.media_root)
    };

    RadarrMovie {
        id: 0,
        title: format!("TMDB:{}", tmdb_id),
        sort_title: format!("tmdb:{}", tmdb_id),
        status: "released".to_string(),
        overview: String::new(),
        images: vec![],
        remote_poster: String::new(),
        year: 0,
        path,
        quality_profile_id: 1,
        monitored: true,
        minimum_availability: "released".to_string(),
        runtime: 0,
        tmdb_id,
        imdb_id: String::new(),
        title_slug: format!("tmdb-{}", tmdb_id),
        certification: String::new(),
        genres: vec!["Anime".to_string()],
        tags: vec![],
        added: String::new(),
        ratings: default_ratings(),
        has_file: false,
        is_available: true,
        folder_name: String::new(),
        clean_title: String::new(),
        root_folder_path: cfg.media_root.clone(),
        movie_file: None,
    }
}
