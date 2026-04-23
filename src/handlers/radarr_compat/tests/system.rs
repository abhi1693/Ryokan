//! Radarr shim response-shape snapshots. Parallel to the Sonarr
//! system tests — same philosophy: pin the JSON shape with `insta`
//! so a silent drift breaks CI instead of Seerr.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use crate::test_support::{
    build_test_app_state, in_memory_pool, radarr_router, seed_radarr_enabled,
};

const KEY: &str = "test-radarr-key-89abcdef";

async fn get_json(app: axum::Router, uri: &str) -> serde_json::Value {
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .header("x-api-key", KEY)
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).expect("response body is JSON")
}

#[tokio::test]
async fn system_status_shape_matches_seerr_expectations() {
    let db = in_memory_pool().await;
    seed_radarr_enabled(&db, KEY).await;
    let state = build_test_app_state(db, None);
    let app = radarr_router(state);
    let body = get_json(app, "/radarr/api/v3/system/status").await;
    insta::assert_json_snapshot!("radarr_system_status", body);
}

#[tokio::test]
async fn system_status_reports_app_name_radarr() {
    let db = in_memory_pool().await;
    seed_radarr_enabled(&db, KEY).await;
    let state = build_test_app_state(db, None);
    let app = radarr_router(state);
    let body = get_json(app, "/radarr/api/v3/system/status").await;
    assert_eq!(body["appName"], "Ryokan");
}

#[tokio::test]
async fn quality_profile_list_snapshot() {
    let db = in_memory_pool().await;
    seed_radarr_enabled(&db, KEY).await;
    let state = build_test_app_state(db, None);
    let app = radarr_router(state);
    let body = get_json(app, "/radarr/api/v3/qualityprofile").await;
    insta::assert_json_snapshot!("radarr_quality_profile", body);
}

#[tokio::test]
async fn root_folder_list_snapshot_with_default_media_root() {
    let db = in_memory_pool().await;
    seed_radarr_enabled(&db, KEY).await;
    let state = build_test_app_state(db, None);
    let app = radarr_router(state);
    let body = get_json(app, "/radarr/api/v3/rootfolder").await;
    // Radarr's RootFolder has a DIFFERENT shape than Sonarr's
    // (accessible: bool instead of totalSpace etc.). Snapshotting
    // here fences that distinction — a refactor that accidentally
    // unified the two would break Seerr's Radarr integration.
    insta::assert_json_snapshot!("radarr_root_folder", body);
}

#[tokio::test]
async fn tag_list_returns_empty_array() {
    let db = in_memory_pool().await;
    seed_radarr_enabled(&db, KEY).await;
    let state = build_test_app_state(db, None);
    let app = radarr_router(state);
    let body = get_json(app, "/radarr/api/v3/tag").await;
    assert_eq!(body, serde_json::json!([]));
}

#[tokio::test]
async fn download_client_returns_empty_when_no_client_configured() {
    let db = in_memory_pool().await;
    seed_radarr_enabled(&db, KEY).await;
    let state = build_test_app_state(db, None);
    let app = radarr_router(state);
    let body = get_json(app, "/radarr/api/v3/downloadclient").await;
    assert_eq!(body, serde_json::json!([]));
}
