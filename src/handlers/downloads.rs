use askama::Template;
use axum::{
    extract::State,
    extract::Query,
    response::{Html, Json},
};
use serde::Deserialize;

use crate::models::grabbed_torrents;
use crate::AppState;

#[derive(Template)]
#[template(path = "downloads.html")]
struct DownloadsTemplate {
    page: String,
    tab: String,
    history: Vec<grabbed_torrents::GrabbedTorrentWithSeries>,
    blocklist: Vec<grabbed_torrents::GrabbedTorrentWithSeries>,
}

#[derive(Deserialize)]
pub struct DownloadsQuery {
    tab: Option<String>,
}

fn normalize_tab(tab: Option<String>) -> String {
    match tab.as_deref() {
        Some("history") => "history".to_string(),
        Some("blocklist") => "blocklist".to_string(),
        _ => "queue".to_string(),
    }
}

pub async fn downloads_page(
    State(state): State<AppState>,
    Query(params): Query<DownloadsQuery>,
) -> Html<String> {
    let tab = normalize_tab(params.tab);

    let history = if tab == "history" {
        grabbed_torrents::get_all_with_series(&state.db, 500).await.unwrap_or_default()
    } else {
        Vec::new()
    };

    let blocklist = if tab == "blocklist" {
        grabbed_torrents::get_blocked(&state.db).await.unwrap_or_default()
    } else {
        Vec::new()
    };

    let template = DownloadsTemplate {
        page: "downloads".to_string(),
        tab,
        history,
        blocklist,
    };
    Html(template.render().unwrap_or_default())
}

#[derive(Deserialize)]
pub struct TorrentActionForm {
    hash: String,
}

#[derive(Deserialize)]
pub struct TorrentDeleteForm {
    hash: String,
    #[serde(default)]
    delete_files: bool,
}

#[derive(Deserialize)]
pub struct BlocklistRemoveForm {
    id: i64,
}

pub async fn api_pause_torrent(
    State(state): State<AppState>,
    Json(form): Json<TorrentActionForm>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let client = {
        let qbit = state.qbit.read().await;
        qbit.as_ref()
            .ok_or((axum::http::StatusCode::BAD_REQUEST, "qBittorrent not configured".to_string()))?
            .clone()
    };
    client.pause_torrent(&form.hash).await.map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(serde_json::json!({"ok": true})))
}

pub async fn api_resume_torrent(
    State(state): State<AppState>,
    Json(form): Json<TorrentActionForm>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let client = {
        let qbit = state.qbit.read().await;
        qbit.as_ref()
            .ok_or((axum::http::StatusCode::BAD_REQUEST, "qBittorrent not configured".to_string()))?
            .clone()
    };
    client.resume_torrent(&form.hash).await.map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(serde_json::json!({"ok": true})))
}

pub async fn api_delete_torrent(
    State(state): State<AppState>,
    Json(form): Json<TorrentDeleteForm>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let client = {
        let qbit = state.qbit.read().await;
        qbit.as_ref()
            .ok_or((axum::http::StatusCode::BAD_REQUEST, "qBittorrent not configured".to_string()))?
            .clone()
    };
    client.delete_torrent(&form.hash, form.delete_files).await.map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(serde_json::json!({"ok": true})))
}

pub async fn api_blocklist_remove(
    State(state): State<AppState>,
    Json(form): Json<BlocklistRemoveForm>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    grabbed_torrents::remove(&state.db, form.id)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::json!({"ok": true})))
}
