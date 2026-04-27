//! Small-surface Radarr endpoints.

use axum::{Json, extract::State};

use crate::AppState;
use crate::handlers::arr_shared::{
    DownloadClientEntry, QualityProfile, SystemStatus, Tag, TagBody,
};
use crate::models::config;

use super::types::RadarrRootFolder;

// ── Handlers ───────────────────────────────────────────────────────────────

/// GET /radarr/api/v3/system/status
pub async fn system_status(State(state): State<AppState>) -> Json<SystemStatus> {
    // Radarr reports a different version string (6.x line) and the url_base
    // is the `/radarr/` prefix this shim is mounted under; everything else
    // matches the shared default.
    let mut s = SystemStatus::default_with_name("Ryokan", state.start_time);
    s.version = "6.1.1.10360".to_string();
    s.url_base = "/radarr".to_string();
    Json(s)
}

/// GET /radarr/api/v3/qualityprofile
pub async fn quality_profiles() -> Json<Vec<QualityProfile>> {
    Json(vec![QualityProfile {
        id: 1,
        name: "Default".to_string(),
    }])
}

/// GET /radarr/api/v3/rootfolder
pub async fn root_folders(State(state): State<AppState>) -> Json<Vec<RadarrRootFolder>> {
    let media_root = config::get_config(&state.db)
        .await
        .ok()
        .flatten()
        .map(|c| c.media_root)
        .unwrap_or_default();

    let path = if media_root.is_empty() {
        "/media".to_string()
    } else {
        media_root
    };

    Json(vec![RadarrRootFolder {
        id: 1,
        path,
        free_space: 0,
        accessible: true,
        unmapped_folders: vec![],
    }])
}

/// GET /radarr/api/v3/tag
pub async fn list_tags() -> Json<Vec<Tag>> {
    Json(vec![])
}

/// POST /radarr/api/v3/tag
pub async fn create_tag(Json(body): Json<TagBody>) -> Json<Tag> {
    Json(Tag {
        id: 1,
        label: body.label.unwrap_or_default(),
    })
}

/// GET /radarr/api/v3/downloadclient — Radarr-shim equivalent of the
/// Sonarr endpoint. See `sonarr_compat::list_download_clients` for the
/// motivation. Radarr's canonical response shape matches Sonarr's for
/// this particular endpoint.
pub async fn list_download_clients(
    State(state): State<AppState>,
) -> Json<Vec<DownloadClientEntry>> {
    let client = state.default_download_client().await;
    let Some(client) = client else {
        return Json(vec![]);
    };
    let impl_name = client.sonarr_impl_name();
    Json(vec![DownloadClientEntry {
        id: 1,
        name: "Ryokan".to_string(),
        enable: true,
        // PR 112 review #2 — derive from live client; see Sonarr
        // counterpart in `sonarr_compat/system.rs`.
        protocol: client.protocol().to_string(),
        implementation: impl_name.to_string(),
        config_contract: format!("{impl_name}Settings"),
        priority: 1,
    }])
}
