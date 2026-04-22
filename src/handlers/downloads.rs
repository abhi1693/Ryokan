use askama::Template;
use axum::{
    extract::Query,
    extract::State,
    response::{Html, Json},
};
use serde::Deserialize;

use crate::AppState;
use crate::models::grabbed_torrents;

struct QueueTorrentView {
    hash: String,
    name: String,
    size_display: String,
    progress_pct: String,
    speed_display: String,
    eta_display: String,
    state_label: String,
    state_badge_class: String,
    is_paused: bool,
}

fn format_size(bytes: i64) -> String {
    if bytes <= 0 {
        return "0 B".to_string();
    }
    let units = ["B", "KB", "MB", "GB", "TB"];
    let i = ((bytes as f64).ln() / 1024f64.ln()).floor() as usize;
    let i = i.min(units.len() - 1);
    let val = bytes as f64 / 1024f64.powi(i as i32);
    if i == 0 {
        format!("{} {}", val as i64, units[i])
    } else {
        format!("{:.1} {}", val, units[i])
    }
}

fn format_speed(bps: i64) -> String {
    if bps <= 0 {
        return String::new();
    }
    format!("{}/s", format_size(bps))
}

fn format_eta(seconds: i64) -> String {
    if seconds <= 0 || seconds >= 8640000 {
        return String::new();
    }
    let h = seconds / 3600;
    let m = (seconds % 3600) / 60;
    let s = seconds % 60;
    if h > 0 {
        format!("{}h {}m", h, m)
    } else if m > 0 {
        format!("{}m {}s", m, s)
    } else {
        format!("{}s", s)
    }
}

fn state_label(state: &str) -> &str {
    match state {
        "uploading" | "stalledUP" | "forcedUP" => "Seeding",
        "downloading" | "forcedDL" => "Downloading",
        "stalledDL" => "Stalled",
        "pausedDL" | "pausedUP" => "Paused",
        "queuedDL" | "queuedUP" => "Queued",
        "checkingDL" | "checkingUP" => "Checking",
        "error" => "Error",
        "missingFiles" => "Missing Files",
        "moving" => "Moving",
        "metaDL" => "Fetching metadata",
        "allocating" => "Allocating",
        _ => state,
    }
}

fn state_badge_class(state: &str) -> &str {
    match state {
        "uploading" | "stalledUP" | "forcedUP" | "pausedUP" => "log-badge-info",
        "downloading" | "forcedDL" => "log-badge-debug",
        "pausedDL" | "queuedDL" | "queuedUP" | "stalledDL" => "log-badge-warn",
        "error" | "missingFiles" => "log-badge-error",
        _ => "",
    }
}

fn torrent_to_view(t: &crate::services::download_client::DownloadItem) -> QueueTorrentView {
    QueueTorrentView {
        hash: t.hash.clone(),
        name: t.name.clone(),
        size_display: format_size(t.size),
        progress_pct: format!("{:.1}", t.progress * 100.0),
        speed_display: format_speed(t.dlspeed),
        eta_display: format_eta(t.eta),
        state_label: state_label(&t.state).to_string(),
        state_badge_class: state_badge_class(&t.state).to_string(),
        is_paused: t.state.starts_with("paused"),
    }
}

#[derive(Template)]
#[template(path = "downloads.html")]
struct DownloadsTemplate {
    page: String,
    tab: String,
    queue: Vec<QueueTorrentView>,
    queue_error: String,
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

    // Load once up-front so history/blocklist queries can honor the
    // user's title_language preference. Queue doesn't need it — the
    // torrent client reports the release filename, not the series.
    let title_language = crate::models::config::get_config(&state.db)
        .await
        .ok()
        .flatten()
        .map(|c| c.title_language)
        .unwrap_or_else(|| "english".to_string());

    let (queue, queue_error) = if tab == "queue" {
        let client = state.download_client.read().await.clone();
        match client {
            Some(c) => match c.list_scoped().await {
                Ok(mut torrents) => {
                    // Sort: downloading first, then by progress descending.
                    torrents.sort_by(|a, b| {
                        let a_down = if a.state.contains("DL") || a.state == "downloading" {
                            0
                        } else {
                            1
                        };
                        let b_down = if b.state.contains("DL") || b.state == "downloading" {
                            0
                        } else {
                            1
                        };
                        a_down.cmp(&b_down).then(
                            b.progress
                                .partial_cmp(&a.progress)
                                .unwrap_or(std::cmp::Ordering::Equal),
                        )
                    });
                    let views = torrents.iter().map(torrent_to_view).collect();
                    (views, String::new())
                }
                Err(e) => (Vec::new(), format!("Could not load queue: {}", e)),
            },
            None => (Vec::new(), "Download client is not configured.".to_string()),
        }
    } else {
        (Vec::new(), String::new())
    };

    let history = if tab == "history" {
        grabbed_torrents::get_all_with_series(&state.db, 500, &title_language)
            .await
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    let blocklist = if tab == "blocklist" {
        grabbed_torrents::get_blocked(&state.db, &title_language)
            .await
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    let template = DownloadsTemplate {
        page: "downloads".to_string(),
        tab,
        queue,
        queue_error,
        history,
        blocklist,
    };
    Html(template.render().unwrap_or_default())
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct TorrentActionForm {
    hash: String,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct TorrentDeleteForm {
    hash: String,
    #[serde(default)]
    delete_files: bool,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct BlocklistRemoveForm {
    id: i64,
}

#[utoipa::path(
    post,
    path = "/api/downloads/pause",
    tag = "Downloads",
    summary = "Pause a torrent",
    description = "Pause an active torrent download in qBittorrent.",
    request_body = TorrentActionForm,
    responses(
        (status = 200, description = "Torrent paused", body = serde_json::Value),
        (status = 400, description = "qBittorrent not configured"),
    ),
)]
pub async fn api_pause_torrent(
    State(state): State<AppState>,
    Json(form): Json<TorrentActionForm>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let client = {
        let guard = state.download_client.read().await;
        guard
            .as_ref()
            .ok_or((
                axum::http::StatusCode::BAD_REQUEST,
                "Download client not configured".to_string(),
            ))?
            .clone()
    };
    client
        .pause(&form.hash)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(serde_json::json!({"ok": true})))
}

#[utoipa::path(
    post,
    path = "/api/downloads/resume",
    tag = "Downloads",
    summary = "Resume a torrent",
    description = "Resume a paused torrent download in qBittorrent.",
    request_body = TorrentActionForm,
    responses(
        (status = 200, description = "Torrent resumed", body = serde_json::Value),
        (status = 400, description = "qBittorrent not configured"),
    ),
)]
pub async fn api_resume_torrent(
    State(state): State<AppState>,
    Json(form): Json<TorrentActionForm>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let client = {
        let guard = state.download_client.read().await;
        guard
            .as_ref()
            .ok_or((
                axum::http::StatusCode::BAD_REQUEST,
                "Download client not configured".to_string(),
            ))?
            .clone()
    };
    client
        .resume(&form.hash)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(serde_json::json!({"ok": true})))
}

#[utoipa::path(
    post,
    path = "/api/downloads/delete",
    tag = "Downloads",
    summary = "Delete a torrent",
    description = "Remove a torrent from qBittorrent. Optionally delete downloaded files.",
    request_body = TorrentDeleteForm,
    responses(
        (status = 200, description = "Torrent deleted", body = serde_json::Value),
        (status = 400, description = "qBittorrent not configured"),
    ),
)]
pub async fn api_delete_torrent(
    State(state): State<AppState>,
    Json(form): Json<TorrentDeleteForm>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let client = {
        let guard = state.download_client.read().await;
        guard
            .as_ref()
            .ok_or((
                axum::http::StatusCode::BAD_REQUEST,
                "Download client not configured".to_string(),
            ))?
            .clone()
    };
    client
        .delete(&form.hash, form.delete_files)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(serde_json::json!({"ok": true})))
}

#[utoipa::path(
    post,
    path = "/api/downloads/blocklist/remove",
    tag = "Downloads",
    summary = "Remove from blocklist",
    description = "Remove a grabbed torrent entry from the blocklist by its database ID.",
    request_body = BlocklistRemoveForm,
    responses(
        (status = 200, description = "Entry removed", body = serde_json::Value),
        (status = 500, description = "Database error"),
    ),
)]
pub async fn api_blocklist_remove(
    State(state): State<AppState>,
    Json(form): Json<BlocklistRemoveForm>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    grabbed_torrents::remove(&state.db, form.id)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::json!({"ok": true})))
}
