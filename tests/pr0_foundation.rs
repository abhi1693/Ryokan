//! Foundation-PR pilot tests (issue test-coverage-expansion PR 0).
//!
//! Proves the three pieces of new infrastructure land correctly:
//!
//!   1. **`tests/` directory discovers the library crate via
//!      `ryokan::...`** — this file is the first one under `tests/`
//!      so if the lib+bin refactor is wrong `cargo test` stops here
//!      with a link error, not halfway through a 2-minute run.
//!   2. **`test-support` feature gates the helper surface correctly**
//!      — `ryokan::test_support` only compiles when the feature is on;
//!      `tests/` files opt in via `--features test-support` in CI.
//!   3. **`handler_router` + `oneshot` exercises real middleware** —
//!      the pilot drives both the public-routes (no auth) and
//!      protected-routes (with session cookie) paths so later waves
//!      can clone the same shape.
//!
//! Also includes a `rstest`-parameterized sanity check on the
//! classification pipeline so the new dev-dep's macro integration is
//! verified without a separate test file.
//!
//! Not exhaustive — the goal is "smoke test the scaffolding," not
//! "cover the behavior." Subsequent PRs layer the real assertions on.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use ryokan::services::source::Source;
use ryokan::services::source_filename::classify_filename;
use ryokan::test_support::{
    build_test_app_state, handler_router, in_memory_pool, logged_in_session,
};

/// The public `GET /login` route renders a 200 HTML page without
/// auth — the simplest possible signal that `handler_router` wired
/// the public-routes middleware correctly. Needs a user to exist
/// (otherwise `login_page` redirects to `/setup` by design); the
/// `logged_in_session` helper creates one as a side-effect, which
/// is enough to flip `has_users` without actually sending a cookie.
#[tokio::test]
async fn handler_router_serves_public_login_page() {
    let db = in_memory_pool().await;
    let (state, _cookie) = logged_in_session(&db).await;
    let app = handler_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/login")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("oneshot");

    assert_eq!(response.status(), StatusCode::OK);
    let content_type = response
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        content_type.starts_with("text/html"),
        "login page should render HTML, got content-type: {content_type}"
    );
}

/// Without a session cookie, `require_auth` redirects the protected
/// `/api/health` endpoint to `/login` (303 See Other). Proves the
/// middleware is actually running, not accidentally skipped.
#[tokio::test]
async fn protected_endpoint_redirects_anonymous_user() {
    let db = in_memory_pool().await;
    let state = build_test_app_state(db, None);
    let app = handler_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("oneshot");

    // `require_auth` uses 303 for browser-facing redirects; API
    // endpoints that shouldn't redirect use a different path. /api/health
    // follows the browser-redirect convention per the existing handler.
    let status = response.status();
    assert!(
        status == StatusCode::SEE_OTHER || status == StatusCode::FOUND,
        "anonymous hit to protected route should redirect, got {status}"
    );
}

/// `logged_in_session` produces a cookie that satisfies `require_auth`.
/// Positive counterpart of the anonymous-redirect test above.
#[tokio::test]
async fn logged_in_session_cookie_reaches_protected_endpoint() {
    let db = in_memory_pool().await;
    let (state, cookie) = logged_in_session(&db).await;
    let app = handler_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/health")
                .header(axum::http::header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("oneshot");

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "authenticated hit to /api/health should succeed"
    );
}

/// `insta` snapshot pilot — locks the `/api/health` response shape
/// (sans dynamic fields) so later provider/client integration PRs
/// have a working snapshot example to copy from. The health endpoint
/// returns JSON, and its happy-path shape is stable enough to pin.
#[tokio::test]
async fn api_health_response_shape_snapshot() {
    let db = in_memory_pool().await;
    let (state, cookie) = logged_in_session(&db).await;
    let app = handler_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/health")
                .header(axum::http::header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("oneshot");

    assert_eq!(response.status(), StatusCode::OK);
    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let body: serde_json::Value = serde_json::from_slice(&body_bytes).expect("parse json");

    // Pin the top-level key set rather than a full snapshot — the
    // values change depending on config (e.g. download client
    // configured or not), but the key shape is the thing a Seerr-
    // style consumer depends on. Keys sorted so ordering changes
    // don't flake the snapshot.
    let mut keys: Vec<String> = body
        .as_object()
        .expect("health response is a JSON object")
        .keys()
        .cloned()
        .collect();
    keys.sort();
    insta::assert_json_snapshot!("api_health_top_level_keys", keys);
}

/// `rstest` pilot — confirms the parameterized-test macro integrates
/// cleanly with the existing classification pipeline. Three cases
/// cover the three sources with unambiguous filename-layer markers.
/// SubsPlease-style releases don't carry filename-layer source
/// evidence (the group-map layer does that job) so the WEB case
/// uses an explicit `WEB-DL` tag instead.
#[rstest::rstest]
#[case("[MTBB] Show - 01 [BD 1080p][FLAC][x265].mkv", Source::BluRay)]
#[case("[Group] Show S01E01 WEB-DL 1080p x264.mkv", Source::Web)]
#[case("[Group] Show - 01 [HDTV 720p].mkv", Source::Hdtv)]
fn rstest_classifies_filename_source(#[case] input: &str, #[case] expected: Source) {
    let classification = classify_filename(input);
    // Highest-confidence evidence wins — mirrors the aggregator in
    // `services/source/mod.rs`. In a pilot test we just check the
    // strongest claim matches; f32 comparison via partial_cmp.
    let top = classification
        .evidence
        .iter()
        .max_by(|a, b| a.confidence.partial_cmp(&b.confidence).unwrap())
        .map(|c| c.source);
    assert_eq!(top, Some(expected), "input: {input}");
}
