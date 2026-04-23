//! Sonarr shim response-shape snapshots. Seerr depends on the
//! exact JSON structure of every response — a silently-dropped
//! field or renamed key breaks its indexer detection and the only
//! signal is "indexer offline" in Seerr's UI. `insta` snapshots
//! pin the full shape; `cargo insta review` is the explicit
//! update path when we intentionally change the schema.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use crate::test_support::{
    build_test_app_state, in_memory_pool, seed_sonarr_enabled, sonarr_router,
};

const KEY: &str = "test-sonarr-key-01234567";

async fn get_json(app: axum::Router, uri: &str) -> serde_json::Value {
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .header("x-api-key", KEY)
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "endpoint should return 200 under a valid key"
    );
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).expect("response body is JSON")
}

// ─── system/status ─────────────────────────────────────────────────

#[tokio::test]
async fn system_status_shape_matches_seerr_expectations() {
    let db = in_memory_pool().await;
    seed_sonarr_enabled(&db, KEY).await;
    let state = build_test_app_state(db, None);
    let app = sonarr_router(state);
    let body = get_json(app, "/api/v3/system/status").await;
    // Full snapshot — every field matters for Seerr's validator.
    // Any drift lands as a snapshot diff in review.
    insta::assert_json_snapshot!("sonarr_system_status", body);
}

#[tokio::test]
async fn system_status_reports_app_name_sonarr() {
    let db = in_memory_pool().await;
    seed_sonarr_enabled(&db, KEY).await;
    let state = build_test_app_state(db, None);
    let app = sonarr_router(state);
    let body = get_json(app, "/api/v3/system/status").await;
    // `appName` is the field Seerr uses for the indicator pill in
    // its UI. Pinning here so a rename of the shim doesn't silently
    // break the UX.
    assert_eq!(body["appName"], "Ryokan");
}

#[tokio::test]
async fn system_status_reports_linux_os_for_seerr_path_separator() {
    // Seerr picks the path separator for `rootFolderPath` validation
    // from `osName` — if we reported "Windows" here, Seerr would
    // backslash-split linux paths and every rootfolder check would
    // fail. The shim returns "linux" as a fixed string because
    // Docker bind-mounts behave like linux paths regardless of host.
    let db = in_memory_pool().await;
    seed_sonarr_enabled(&db, KEY).await;
    let state = build_test_app_state(db, None);
    let app = sonarr_router(state);
    let body = get_json(app, "/api/v3/system/status").await;
    assert_eq!(body["osName"], "linux");
    assert_eq!(body["isLinux"], true);
    assert_eq!(body["isWindows"], false);
    assert_eq!(body["isOsx"], false);
}

// ─── qualityprofile / qualityProfile ───────────────────────────────

#[tokio::test]
async fn quality_profile_list_snapshot() {
    let db = in_memory_pool().await;
    seed_sonarr_enabled(&db, KEY).await;
    let state = build_test_app_state(db, None);
    let app = sonarr_router(state);
    let body = get_json(app, "/api/v3/qualityprofile").await;
    insta::assert_json_snapshot!("sonarr_quality_profile", body);
}

// ─── rootfolder ───────────────────────────────────────────────────

#[tokio::test]
async fn root_folder_uses_default_path_when_media_root_unset() {
    // `rootFolders` reads `config.media_root`; on a fresh config
    // that field is empty and the shim substitutes `/media`.
    let db = in_memory_pool().await;
    seed_sonarr_enabled(&db, KEY).await;
    let state = build_test_app_state(db, None);
    let app = sonarr_router(state);
    let body = get_json(app, "/api/v3/rootfolder").await;
    assert!(body.is_array());
    let first = &body[0];
    assert_eq!(first["path"], "/media");
    assert_eq!(first["id"], 1);
}

#[tokio::test]
async fn root_folder_reflects_user_media_root_when_set() {
    let db = in_memory_pool().await;
    let cfg = crate::models::config::Config {
        sonarr_enabled: true,
        sonarr_api_key: KEY.to_string(),
        media_root: "/srv/anime".to_string(),
        ..crate::models::config::Config::default()
    };
    crate::models::config::save_config(&db, &cfg).await.unwrap();
    let state = build_test_app_state(db, None);
    let app = sonarr_router(state);
    let body = get_json(app, "/api/v3/rootfolder").await;
    assert_eq!(body[0]["path"], "/srv/anime");
}

// ─── languageprofile ──────────────────────────────────────────────

#[tokio::test]
async fn language_profile_list_snapshot() {
    let db = in_memory_pool().await;
    seed_sonarr_enabled(&db, KEY).await;
    let state = build_test_app_state(db, None);
    let app = sonarr_router(state);
    let body = get_json(app, "/api/v3/languageprofile").await;
    insta::assert_json_snapshot!("sonarr_language_profile", body);
}

// ─── tag ───────────────────────────────────────────────────────────

#[tokio::test]
async fn tag_list_returns_empty_array() {
    let db = in_memory_pool().await;
    seed_sonarr_enabled(&db, KEY).await;
    let state = build_test_app_state(db, None);
    let app = sonarr_router(state);
    let body = get_json(app, "/api/v3/tag").await;
    assert_eq!(body, serde_json::json!([]));
}

// ─── downloadclient ───────────────────────────────────────────────

#[tokio::test]
async fn download_client_returns_empty_when_no_client_configured() {
    // No client in `state.download_client` → the shim returns an
    // empty list (Sonarr's behavior for an unset client slot).
    let db = in_memory_pool().await;
    seed_sonarr_enabled(&db, KEY).await;
    let state = build_test_app_state(db, None);
    let app = sonarr_router(state);
    let body = get_json(app, "/api/v3/downloadclient").await;
    assert_eq!(body, serde_json::json!([]));
}
