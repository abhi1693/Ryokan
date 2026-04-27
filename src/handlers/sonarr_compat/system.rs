//! Small-surface Sonarr endpoints.

use axum::{Json, extract::State};

use crate::AppState;
use crate::handlers::arr_shared::{
    DownloadClientEntry, QualityProfile, SystemStatus, Tag, TagBody,
};
use crate::models::config;

use super::types::{LanguageProfile, RootFolder};

// ── Handlers ───────────────────────────────────────────────────────────────

/// GET /api/v3/system/status
pub async fn system_status(State(state): State<AppState>) -> Json<SystemStatus> {
    Json(SystemStatus::default_with_name("Ryokan", state.start_time))
}

/// GET /api/v3/qualityprofile
pub async fn quality_profiles() -> Json<Vec<QualityProfile>> {
    Json(vec![QualityProfile {
        id: 1,
        name: "Default".to_string(),
    }])
}

/// GET /api/v3/rootfolder
pub async fn root_folders(State(state): State<AppState>) -> Json<Vec<RootFolder>> {
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

/// GET /api/v3/downloadclient — fake Sonarr-shaped response reflecting
/// Ryokan's active download client. Seerr doesn't strictly require this
/// endpoint today, but filing it alongside the #63 pluggable-client
/// refactor keeps the shim consistent with the rest of Ryokan: the
/// `implementation` field varies with the active client rather than
/// being hardcoded to `QBittorrent`. Returns an empty list when no
/// client is configured — matches Sonarr's behavior for an unset
/// download client slot.
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
        // PR 112 review #2 — derive the protocol from the live
        // client (`"torrent"` for BT, `"usenet"` for SAB) instead
        // of hardcoding `"torrent"`. Sonarr's own shim emits
        // `"usenet"` for a SAB default-client install; matching
        // here keeps Seerr's "show me the user's clients" view
        // accurate.
        protocol: client.protocol().to_string(),
        implementation: impl_name.to_string(),
        config_contract: format!("{impl_name}Settings"),
        priority: 1,
    }])
}

/// POST /api/v3/tag
pub async fn create_tag(Json(body): Json<TagBody>) -> Json<Tag> {
    Json(Tag {
        id: 1,
        label: body.label.unwrap_or_default(),
    })
}
