use askama::Template;
use axum::{
    Form,
    extract::{Query, State},
    response::{Html, Json},
};
use serde::Deserialize;

use crate::AppState;
use crate::models::log::LogCategory;
use crate::services::{logger, nyaa, scoring};

#[derive(Template)]
#[template(path = "search.html")]
struct SearchTemplate {
    page: String,
    results: Vec<nyaa::SearchResult>,
    query: String,
    searched: bool,
    has_next: bool,
    /// Issue #83 — `batches_only` (default) or `never`. Threaded
    /// through to search.js via window.searchState so the Grab button
    /// can bypass the modal when the user's set it to `never`.
    grab_preview_mode: String,
    title_language: String,
}

async fn load_grab_preview_mode(state: &AppState) -> String {
    crate::models::config::get_config(&state.db)
        .await
        .ok()
        .flatten()
        .map(|c| c.grab_preview_mode)
        .unwrap_or_else(|| "batches_only".to_string())
}

#[derive(Deserialize)]
pub struct SearchForm {
    query: String,
    #[serde(default)]
    category: String,
    #[serde(default)]
    filter: String,
    #[serde(default)]
    user: String,
}

#[derive(Deserialize, utoipa::IntoParams)]
pub struct PageQuery {
    query: String,
    #[serde(default)]
    category: String,
    #[serde(default)]
    filter: String,
    #[serde(default)]
    user: String,
    #[serde(default = "default_page")]
    p: i32,
}

fn default_page() -> i32 {
    1
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct GrabForm {
    url: String,
    /// Optional release title — used for library linkage (#1.3.0 plan
    /// item 6d). When supplied, the grab handler tries to match it
    /// against an existing library series; on match, the grab lands
    /// in `grabbed_torrents` linked to that series and (for batches)
    /// auto_expand runs for sibling-series detection. Empty / absent
    /// = behave like the original grab endpoint (fire-and-forget).
    #[serde(default)]
    title: Option<String>,
    /// Optional info_hash from the frontend. Used both to key the
    /// download-client add and (when matched) as the grabbed_torrents
    /// primary key. Frontend sends it when known.
    #[serde(default)]
    info_hash: Option<String>,
    /// Whether the release was flagged as a batch by the search UI.
    /// Gates auto_expand at grab time.
    #[serde(default)]
    is_batch: Option<bool>,
}

/// Helper to build SearchOptions from config.
async fn build_opts(
    state: &AppState,
    query: String,
    category: String,
    filter: String,
    user: String,
) -> nyaa::SearchOptions {
    let config = crate::models::config::get_config(&state.db)
        .await
        .ok()
        .flatten();

    let preferred_groups = config
        .as_ref()
        .map(|c| {
            c.preferred_groups
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();

    let preferred_res = config
        .as_ref()
        .map(|c| c.preferred_resolution.clone())
        .unwrap_or_else(|| "1080".to_string());

    let prefer_subs = config.as_ref().map(|c| c.prefer_subs).unwrap_or(true);

    nyaa::SearchOptions {
        query,
        category: if category.is_empty() {
            "1_0".to_string()
        } else {
            category
        },
        filter: if filter.is_empty() {
            "0".to_string()
        } else {
            filter
        },
        user,
        preferred_groups,
        preferred_resolution: preferred_res,
        prefer_subs,
    }
}

pub async fn search_page(State(state): State<AppState>) -> Html<String> {
    let (grab_preview_mode, title_language) = tokio::join!(
        load_grab_preview_mode(&state),
        crate::models::config::get_title_language(&state.db),
    );
    let template = SearchTemplate {
        page: "search".to_string(),
        results: Vec::new(),
        query: String::new(),
        searched: false,
        has_next: false,
        grab_preview_mode,
        title_language,
    };
    Html(template.render().unwrap_or_default())
}

pub async fn search_submit(
    State(state): State<AppState>,
    Form(form): Form<SearchForm>,
) -> Html<String> {
    let opts = build_opts(
        &state,
        form.query.clone(),
        form.category,
        form.filter,
        form.user,
    )
    .await;

    let mut response = match nyaa::search(&opts, 1).await {
        Ok(resp) => {
            logger::debug(
                &state.db,
                LogCategory::Search,
                &format!("Search: '{}' — {} results", form.query, resp.results.len()),
                "",
            )
            .await;
            resp
        }
        Err(e) => {
            logger::error(
                &state.db,
                LogCategory::Nyaa,
                &format!("Search failed: '{}'", form.query),
                &e,
            )
            .await;
            nyaa::SearchResponse {
                results: Vec::new(),
                page: 1,
                has_next: false,
            }
        }
    };

    // #1.3.0 — augment the base-score breakdown with Custom Format
    // contributions so the search-page expander shows both the base
    // rules and the CF deltas. SeaDex specs never fire here (no
    // series context = empty hash set), which is deliberate: the
    // manual search page is a generic Nyaa search surface, not a
    // per-series auto-grab path.
    let cfs = state.custom_formats.read().await.clone();
    scoring::apply_cf_breakdown(
        &mut response.results,
        &cfs,
        &std::collections::HashSet::new(),
    );

    let (grab_preview_mode, title_language) = tokio::join!(
        load_grab_preview_mode(&state),
        crate::models::config::get_title_language(&state.db),
    );
    let template = SearchTemplate {
        page: "search".to_string(),
        results: response.results,
        query: form.query,
        searched: true,
        has_next: response.has_next,
        grab_preview_mode,
        title_language,
    };
    Html(template.render().unwrap_or_default())
}

/// JSON API endpoint for loading additional pages.
#[utoipa::path(
    get,
    path = "/api/search/page",
    tag = "Search",
    summary = "Search Nyaa torrents",
    description = "Search Nyaa.si for anime torrents with pagination and filtering options.",
    params(PageQuery),
    responses(
        (status = 200, description = "Paginated search results", body = nyaa::SearchResponse),
        (status = 500, description = "Search failed"),
    ),
)]
pub async fn search_page_api(
    State(state): State<AppState>,
    Query(params): Query<PageQuery>,
) -> Result<Json<nyaa::SearchResponse>, (axum::http::StatusCode, String)> {
    let opts = build_opts(
        &state,
        params.query,
        params.category,
        params.filter,
        params.user,
    )
    .await;

    let mut response = nyaa::search(&opts, params.p)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e))?;

    // Mirror search_submit — keep the expander/scores consistent across
    // page 1 (server-rendered) and page 2+ (JSON-appended via loadMore).
    let cfs = state.custom_formats.read().await.clone();
    scoring::apply_cf_breakdown(
        &mut response.results,
        &cfs,
        &std::collections::HashSet::new(),
    );

    Ok(Json(response))
}

#[utoipa::path(
    post,
    path = "/api/grab",
    tag = "Search",
    summary = "Grab a torrent",
    description = "Send a torrent URL to qBittorrent for download.",
    request_body = GrabForm,
    responses(
        (status = 200, description = "Torrent added", body = serde_json::Value),
        (status = 400, description = "qBittorrent not configured"),
        (status = 500, description = "Failed to add torrent"),
    ),
)]
pub async fn grab_release(
    State(state): State<AppState>,
    Json(form): Json<GrabForm>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let client = state.default_download_client().await.ok_or((
        axum::http::StatusCode::BAD_REQUEST,
        "Download client not configured".to_string(),
    ))?;

    let form_hash = form.info_hash.clone().unwrap_or_default();
    let info_hash = if !form_hash.is_empty() {
        form_hash
    } else {
        crate::services::nyaa::extract_hash(&form.url)
    };
    client
        .add_torrent(&form.url, &info_hash)
        .await
        .map_err(|e| {
            let db = state.db.clone();
            let err_msg = e.clone();
            tokio::spawn(async move {
                logger::error(&db, LogCategory::QBit, "Manual grab failed", &err_msg).await;
            });
            (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e)
        })?;

    logger::info(
        &state.db,
        LogCategory::Grab,
        "Manual grab sent to download client",
        &form.url,
    )
    .await;

    // Library linkage (#6d) — fire-and-forget so the user sees
    // "grabbed" immediately while the library bookkeeping runs in the
    // background. Only runs when the frontend passed a title (so we
    // have something to match) and info_hash (so grabbed_torrents has
    // a stable primary key). Matching against the existing library
    // reuses the RSS matcher — no AniList calls, no HTTP, just the
    // alias/fuzzy-match pass.
    if let (Some(title), hash) = (form.title.clone(), info_hash.clone())
        && !title.is_empty()
        && !hash.is_empty()
    {
        let is_batch = form.is_batch.unwrap_or(false);
        let state_task = state.clone();
        let client_task = client.clone();
        tokio::spawn(async move {
            let Some((series, eps)) =
                crate::services::rss::match_library_title(&state_task.db, &title, is_batch).await
            else {
                // No library match. Grab succeeds without series
                // attribution. Auto-adding the series via AniList is
                // scoped out for this pass — it needs care around
                // rate limits, duplicate detection, and provider
                // fallbacks.
                return;
            };
            // Record the grab against the matched series. Episode
            // numbers come from the RSS matcher's resolved_eps
            // (absolute or season-relative, whichever fired).
            let grab_id = crate::models::grabbed_torrents::record_grab(
                &state_task.db,
                &hash,
                &title,
                series.id,
                &eps,
                is_batch,
            )
            .await
            .ok()
            .flatten();

            // Populate episode_quality_tags so the series page shows
            // each grabbed episode in 'grabbed' state right away.
            let classification = crate::services::source::classify_release(
                &state_task.db,
                &title,
                None,
                Some(crate::services::source::NyaaContext {
                    info_hash: &hash,
                    view_url: "",
                    is_batch,
                }),
                Some(crate::services::source::SeriesContext {
                    status: &series.status,
                    season_year: series.season_year,
                    end_year: series.end_year,
                }),
            )
            .await;
            for ep in &eps {
                let _ = crate::models::episode_tags::record_grab(
                    &state_task.db,
                    series.id,
                    *ep,
                    &classification,
                    &title,
                    "",
                    0,
                    is_batch,
                )
                .await;
            }

            logger::info(
                &state_task.db,
                LogCategory::Grab,
                &format!(
                    "Manual grab linked to series: {} ({} ep{})",
                    series.title,
                    eps.len(),
                    if eps.len() == 1 { "" } else { "s" }
                ),
                &title,
            )
            .await;

            // Batch grabs get sibling-series detection via auto_expand
            // at metadata-available time — same path RSS + auto-search
            // use. Skipped when the series's provider_id is negative
            // (Jikan-fallback sentinel, no AL graph to walk).
            if is_batch
                && series.anilist_id > 0
                && let Some(grab_id) = grab_id
            {
                let db_expand = state_task.db.clone();
                let client_expand = client_task.clone();
                let hash_expand = hash.clone();
                let title_expand = title.clone();
                let series_id_expand = series.id;
                let provider_id_expand = series.anilist_id;
                let ep_list_expand = eps.clone();
                let classification_expand = classification.clone();
                tokio::spawn(async move {
                    let detail = match crate::models::metadata_cache::get_by_provider_id(
                        &db_expand,
                        provider_id_expand,
                    )
                    .await
                    {
                        Ok(Some(row)) => row.detail,
                        _ => return,
                    };
                    let files = match crate::services::download_client::wait_for_files(
                        &*client_expand,
                        &hash_expand,
                        std::time::Duration::from_secs(180),
                    )
                    .await
                    {
                        Ok(files) => files,
                        Err(_) => return,
                    };
                    let filenames: Vec<String> = files.into_iter().map(|f| f.name).collect();
                    let ctx = crate::services::auto_expand::AutoExpandGrabContext {
                        classification: classification_expand,
                        release_group: String::new(),
                        size_bytes: 0,
                    };
                    crate::services::auto_expand::expand_from_files(
                        &db_expand,
                        &filenames,
                        &detail,
                        series_id_expand,
                        &ep_list_expand,
                        grab_id,
                        &title_expand,
                        &ctx,
                    )
                    .await;
                });
            }
        });
    }

    Ok(Json(serde_json::json!({"ok": true})))
}

#[utoipa::path(
    get,
    path = "/api/torrents",
    tag = "Downloads",
    summary = "List active torrents",
    description = "Returns all torrents currently in the download client's queue.",
    responses(
        (status = 200, description = "Torrent list", body = Vec<crate::services::download_client::DownloadItem>),
        (status = 400, description = "Download client not configured"),
    ),
)]
pub async fn get_torrents(
    State(state): State<AppState>,
) -> Result<
    Json<Vec<crate::services::download_client::DownloadItem>>,
    (axum::http::StatusCode, String),
> {
    let client = state.default_download_client().await.ok_or((
        axum::http::StatusCode::BAD_REQUEST,
        "Download client not configured".to_string(),
    ))?;

    let torrents = client
        .list_scoped()
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e))?;

    Ok(Json(torrents))
}
