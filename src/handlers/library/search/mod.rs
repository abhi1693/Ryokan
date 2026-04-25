//! Search/grab/auto-expand handlers for the library section.
//!
//! Split from a single 2596-line `search.rs` into a directory in
//! v1.5: see `auto_search`, `interactive`, `grab` siblings + the
//! `tests/` topic-split. This `mod.rs` retains the two cheap API
//! endpoints (`anilist_search`, `api_series_detail`) and the public
//! re-export surface that `main.rs`'s router declarations call.
//!
//! - `auto_search` — auto-search pipeline (`auto_search_series`,
//!   `auto_search_episode`, the inner `run_auto_search_targets_with_upgrades`
//!   loop) **and** the auto-expand sibling-pack detector that
//!   `auto_search_targets_with_upgrades` calls back into. The two
//!   directions of the call edge are too tightly coupled to live
//!   apart, so they share a file.
//! - `interactive` — user-driven search variants (`search_batch_releases`,
//!   `interactive_search_episode`, `interactive_search_batches`).
//! - `grab` — `grab_batch_result` + `grab_interactive_result`.
//! - `tests` — the auto-expand cumulative-offset / sibling-routing
//!   suite.

use axum::{
    extract::{Path, Query, State},
    response::Json,
};

use crate::AppState;
use crate::models::log::LogCategory;
use crate::services::{anilist, logger};

use super::AnilistSearchQuery;
use super::reconcile::{force_mal_fallback_enabled, resolve_series_context};

mod auto_search;
mod grab;
mod interactive;

#[cfg(test)]
mod tests;

pub use auto_search::{
    __path_auto_search_episode, __path_auto_search_series, AutoSearchQuery, auto_search_episode,
    auto_search_series, run_auto_search_targets,
};
pub use grab::{
    __path_grab_batch_result, __path_grab_interactive_result, grab_batch_result,
    grab_interactive_result,
};
pub use interactive::{
    __path_interactive_search_batches, __path_interactive_search_episode,
    __path_search_batch_releases, interactive_search_batches, interactive_search_episode,
    search_batch_releases,
};

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
    // Per-search override (?source=al|mal) takes precedence over the
    // ambient config flag. Only `al`, `mal`, or omitted are valid —
    // surface a 400 on anything else so a client with a typo in its
    // query param doesn't silently drop into the config default and
    // look like the toggle is broken.
    let force_fallback = match params.source.as_deref() {
        Some("mal") => true,
        Some("al") => false,
        None => force_mal_fallback_enabled(&state.db).await,
        Some("") => force_mal_fallback_enabled(&state.db).await,
        Some(other) => {
            return Err((
                axum::http::StatusCode::BAD_REQUEST,
                format!(
                    "invalid source override: {:?} (expected \"al\", \"mal\", or omit)",
                    other
                ),
            ));
        }
    };
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
        &format!(
            "results={}, source={}, forced_fallback={}, requested={}",
            results.len(),
            source,
            force_fallback,
            params.source.as_deref().unwrap_or("(config default)"),
        ),
    )
    .await;

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
