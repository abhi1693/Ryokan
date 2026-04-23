//! Radarr shim API-key middleware coverage. Parallel to the
//! Sonarr auth tests, but asserts the Radarr-specific paths are
//! independently gated — enabling the Sonarr shim doesn't
//! accidentally enable Radarr and vice versa.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use crate::test_support::{
    build_test_app_state, in_memory_pool, radarr_router, seed_radarr_enabled, seed_sonarr_enabled,
};

const KEY: &str = "test-radarr-key-89abcdef";

async fn get(app: axum::Router, uri: &str, api_key: Option<&str>) -> axum::http::Response<Body> {
    let mut builder = Request::builder().method("GET").uri(uri);
    if let Some(k) = api_key {
        builder = builder.header("x-api-key", k);
    }
    app.oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

#[tokio::test]
async fn returns_503_when_config_row_missing() {
    let db = in_memory_pool().await;
    let state = build_test_app_state(db, None);
    let app = radarr_router(state);
    let response = get(app, "/radarr/api/v3/system/status", Some(KEY)).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn returns_503_when_radarr_shim_disabled() {
    let db = in_memory_pool().await;
    crate::models::config::save_config(&db, &crate::models::config::Config::default())
        .await
        .unwrap();
    let state = build_test_app_state(db, None);
    let app = radarr_router(state);
    let response = get(app, "/radarr/api/v3/system/status", Some(KEY)).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn returns_401_on_mismatched_key() {
    let db = in_memory_pool().await;
    seed_radarr_enabled(&db, KEY).await;
    let state = build_test_app_state(db, None);
    let app = radarr_router(state);
    let response = get(app, "/radarr/api/v3/system/status", Some("wrong")).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn accepts_valid_radarr_key() {
    let db = in_memory_pool().await;
    seed_radarr_enabled(&db, KEY).await;
    let state = build_test_app_state(db, None);
    let app = radarr_router(state);
    let response = get(app, "/radarr/api/v3/system/status", Some(KEY)).await;
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn sonarr_key_does_not_unlock_radarr_shim() {
    // Property under test: the two shims are independently gated
    // by their own `enabled` + `api_key` fields. Enabling Sonarr
    // with its own key must not open the Radarr gate, even when
    // the caller attacks with the Sonarr key.
    let db = in_memory_pool().await;
    let sonarr_key = "sonarr-only-key";
    seed_sonarr_enabled(&db, sonarr_key).await;
    // Radarr remains disabled with an empty key.
    let state = build_test_app_state(db, None);
    let app = radarr_router(state);
    let response = get(app, "/radarr/api/v3/system/status", Some(sonarr_key)).await;
    assert_eq!(
        response.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "Radarr should stay disabled when only Sonarr is configured"
    );
}

#[tokio::test]
async fn case_variant_quality_profile_route_accepts_same_key() {
    // Parallel to Sonarr's alias test — Seerr ships both casings
    // and the Radarr shim handles them identically.
    let db = in_memory_pool().await;
    seed_radarr_enabled(&db, KEY).await;
    let state = build_test_app_state(db, None);
    let app = radarr_router(state);
    let lower = get(app.clone(), "/radarr/api/v3/qualityprofile", Some(KEY)).await;
    let upper = get(app, "/radarr/api/v3/qualityProfile", Some(KEY)).await;
    assert_eq!(lower.status(), StatusCode::OK);
    assert_eq!(upper.status(), StatusCode::OK);
}
