use askama::Template;
use axum::{
    extract::Query,
    extract::State,
    response::{Html, Json},
};
use serde::Deserialize;

use crate::AppState;
use crate::models::grabbed_torrents;
use crate::services::download_client::DownloadItemState;

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

fn state_label(kind: DownloadItemState) -> &'static str {
    match kind {
        DownloadItemState::Downloading => "Downloading",
        DownloadItemState::DownloadingStalled => "Stalled",
        DownloadItemState::DownloadingQueued => "Queued",
        DownloadItemState::CheckingDownload => "Checking",
        DownloadItemState::Seeding | DownloadItemState::SeedingStalled => "Seeding",
        DownloadItemState::SeedingQueued => "Queued",
        DownloadItemState::CheckingSeed => "Checking",
        DownloadItemState::Paused | DownloadItemState::PausedComplete => "Paused",
        DownloadItemState::Errored => "Error",
    }
}

fn state_badge_class(kind: DownloadItemState) -> &'static str {
    match kind {
        DownloadItemState::Downloading => "log-badge-debug",
        DownloadItemState::DownloadingStalled
        | DownloadItemState::DownloadingQueued
        | DownloadItemState::SeedingQueued
        | DownloadItemState::Paused => "log-badge-warn",
        DownloadItemState::Seeding
        | DownloadItemState::SeedingStalled
        | DownloadItemState::PausedComplete => "log-badge-info",
        DownloadItemState::Errored => "log-badge-error",
        DownloadItemState::CheckingDownload | DownloadItemState::CheckingSeed => "",
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
        state_label: state_label(t.state_kind).to_string(),
        state_badge_class: state_badge_class(t.state_kind).to_string(),
        is_paused: matches!(
            t.state_kind,
            DownloadItemState::Paused | DownloadItemState::PausedComplete
        ),
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
                    let is_downloading = |k: DownloadItemState| {
                        matches!(
                            k,
                            DownloadItemState::Downloading
                                | DownloadItemState::DownloadingStalled
                                | DownloadItemState::DownloadingQueued
                                | DownloadItemState::CheckingDownload
                        )
                    };
                    torrents.sort_by(|a, b| {
                        let a_down = if is_downloading(a.state_kind) { 0 } else { 1 };
                        let b_down = if is_downloading(b.state_kind) { 0 } else { 1 };
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
    description = "Pause an active torrent download in the configured download client.",
    request_body = TorrentActionForm,
    responses(
        (status = 200, description = "Torrent paused", body = serde_json::Value),
        (status = 400, description = "Download client not configured"),
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
    description = "Resume a paused torrent download in the configured download client.",
    request_body = TorrentActionForm,
    responses(
        (status = 200, description = "Torrent resumed", body = serde_json::Value),
        (status = 400, description = "Download client not configured"),
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
    description = "Remove a torrent from the configured download client. Optionally delete downloaded files.",
    request_body = TorrentDeleteForm,
    responses(
        (status = 200, description = "Torrent deleted", body = serde_json::Value),
        (status = 400, description = "Download client not configured"),
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

#[cfg(test)]
mod tests {
    use super::*;

    // Mirror of the exhaustive-match pattern in
    // `services::download_client::tests::all_variants_with_slugs` —
    // a new enum variant has to be added to the inner match before
    // the test compiles, which forces both state_label and
    // state_badge_class to get an explicit mapping instead of
    // falling through to a fallback arm.
    fn all_variants_with_expected() -> Vec<(DownloadItemState, &'static str, &'static str, bool)> {
        // (variant, expected_label, expected_badge_class, expected_is_paused)
        use DownloadItemState::*;
        fn _exhaustive(v: DownloadItemState) {
            match v {
                Downloading | DownloadingStalled | DownloadingQueued | CheckingDownload => {}
                Seeding | SeedingStalled | SeedingQueued | CheckingSeed => {}
                Paused | PausedComplete => {}
                Errored => {}
            }
        }
        vec![
            (Downloading, "Downloading", "log-badge-debug", false),
            (DownloadingStalled, "Stalled", "log-badge-warn", false),
            (DownloadingQueued, "Queued", "log-badge-warn", false),
            (CheckingDownload, "Checking", "", false),
            (Seeding, "Seeding", "log-badge-info", false),
            (SeedingStalled, "Seeding", "log-badge-info", false),
            (SeedingQueued, "Queued", "log-badge-warn", false),
            (CheckingSeed, "Checking", "", false),
            (Paused, "Paused", "log-badge-warn", true),
            (PausedComplete, "Paused", "log-badge-info", true),
            (Errored, "Error", "log-badge-error", false),
        ]
    }

    #[test]
    fn state_label_covers_every_variant() {
        for (v, label, _, _) in all_variants_with_expected() {
            assert_eq!(state_label(v), label, "label mismatch for {v:?}");
        }
    }

    #[test]
    fn state_badge_class_covers_every_variant() {
        for (v, _, badge, _) in all_variants_with_expected() {
            assert_eq!(state_badge_class(v), badge, "badge mismatch for {v:?}");
        }
    }

    #[test]
    fn torrent_view_is_paused_flag_matches_enum() {
        // Drives the pause/resume button on the queue row. Reading
        // a client-native "paused" prefix from the legacy
        // `state.starts_with("paused")` path would silently break
        // for Transmission (numeric states) and rtorrent (computed
        // strings) — the derivation has to go through state_kind.
        for (v, _, _, expected_paused) in all_variants_with_expected() {
            let item = crate::services::download_client::DownloadItem {
                hash: "a".repeat(40),
                name: "Release".to_string(),
                size: 0,
                progress: 0.0,
                dlspeed: 0,
                state: String::new(),
                category: String::new(),
                eta: 0,
                save_path: String::new(),
                content_path: String::new(),
                state_kind: v,
            };
            let view = torrent_to_view(&item);
            assert_eq!(view.is_paused, expected_paused, "is_paused for {v:?}");
        }
    }
}
