use askama::Template;
use axum::{
    extract::{Query, State},
    response::{Html, Json},
    Form,
};
use serde::Deserialize;

use crate::services::{nyaa, logger};
use crate::models::log::LogCategory;
use crate::AppState;

#[derive(Template)]
#[template(path = "search.html")]
struct SearchTemplate {
    page: String,
    results: Vec<nyaa::SearchResult>,
    query: String,
    searched: bool,
    has_next: bool,
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

#[derive(Deserialize)]
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

#[derive(Deserialize)]
pub struct GrabForm {
    url: String,
}

/// Helper to build SearchOptions from config.
async fn build_opts(state: &AppState, query: String, category: String, filter: String, user: String) -> nyaa::SearchOptions {
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

    let prefer_subs = config
        .as_ref()
        .map(|c| c.prefer_subs)
        .unwrap_or(true);

    nyaa::SearchOptions {
        query,
        category: if category.is_empty() { "1_0".to_string() } else { category },
        filter: if filter.is_empty() { "0".to_string() } else { filter },
        user,
        preferred_groups,
        preferred_resolution: preferred_res,
        prefer_subs,
    }
}

pub async fn search_page(State(_state): State<AppState>) -> Html<String> {
    let template = SearchTemplate {
        page: "search".to_string(),
        results: Vec::new(),
        query: String::new(),
        searched: false,
        has_next: false,
    };
    Html(template.render().unwrap_or_default())
}

pub async fn search_submit(
    State(state): State<AppState>,
    Form(form): Form<SearchForm>,
) -> Html<String> {
    let opts = build_opts(&state, form.query.clone(), form.category, form.filter, form.user).await;

    let response = match nyaa::search(&opts, 1).await {
        Ok(resp) => {
            logger::debug(&state.db, LogCategory::Search, &format!("Search: '{}' — {} results", form.query, resp.results.len()), "").await;
            resp
        }
        Err(e) => {
            logger::error(&state.db, LogCategory::Nyaa, &format!("Search failed: '{}'", form.query), &e).await;
            nyaa::SearchResponse { results: Vec::new(), page: 1, has_next: false }
        }
    };

    let template = SearchTemplate {
        page: "search".to_string(),
        results: response.results,
        query: form.query,
        searched: true,
        has_next: response.has_next,
    };
    Html(template.render().unwrap_or_default())
}

/// JSON API endpoint for loading additional pages.
pub async fn search_page_api(
    State(state): State<AppState>,
    Query(params): Query<PageQuery>,
) -> Result<Json<nyaa::SearchResponse>, (axum::http::StatusCode, String)> {
    let opts = build_opts(&state, params.query, params.category, params.filter, params.user).await;

    let response = nyaa::search(&opts, params.p)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e))?;

    Ok(Json(response))
}

pub async fn grab_release(
    State(state): State<AppState>,
    Json(form): Json<GrabForm>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let client = {
        let qbit = state.qbit.read().await;
        qbit.as_ref()
            .ok_or((axum::http::StatusCode::BAD_REQUEST, "qBittorrent not configured".to_string()))?
            .clone()
    };

    client
        .add_torrent(&form.url)
        .await
        .map_err(|e| {
            // Fire-and-forget log — don't block on it in the error path.
            let db = state.db.clone();
            let err_msg = e.clone();
            tokio::spawn(async move {
                logger::error(&db, LogCategory::QBit, "Manual grab failed", &err_msg).await;
            });
            (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e)
        })?;

    logger::info(&state.db, LogCategory::Grab, "Manual grab sent to qBittorrent", &form.url).await;

    Ok(Json(serde_json::json!({"ok": true})))
}

pub async fn get_torrents(
    State(state): State<AppState>,
) -> Result<Json<Vec<crate::services::qbit::Torrent>>, (axum::http::StatusCode, String)> {
    let client = {
        let qbit = state.qbit.read().await;
        qbit.as_ref()
            .ok_or((axum::http::StatusCode::BAD_REQUEST, "qBittorrent not configured".to_string()))?
            .clone()
    };

    let torrents = client
        .get_torrents()
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e))?;

    Ok(Json(torrents))
}
