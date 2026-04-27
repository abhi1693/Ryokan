//! Resource-tier endpoints (`/series`, `/series/{id}`, `/series/lookup`,
//! `/command`). The system-tier tests in `system.rs` only cover the
//! connection-test surface Seerr hits before adding anything; these
//! cover what Seerr does once it tries to actually request a series.
//!
//! The lookup-by-title path goes out to AniList and is not covered
//! here — the wiremock-driven tests for that live in
//! `tests/sonarr_compat_e2e.rs` (or a future sibling). What we *can*
//! cover without a network mock:
//!
//! * `/series/lookup?term=tvdb:N` — `lookup_by_external_id` falls
//!   through to the stub when anibridge has no mapping (tests don't
//!   load the real mappings table).
//! * `/series` (GET) — tracked-series list, DB-only.
//! * `/series/{id}` — single-series GET, DB-only.
//! * `/series` (PUT) — monitor-mode flip, DB-only.
//! * `/command` — fire-and-forget canned response.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;

use crate::test_support::{
    build_test_app_state, in_memory_pool, seed_series, seed_sonarr_enabled,
    sonarr_router_with_series,
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
        "GET {uri} should return 200"
    );
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).expect("JSON")
}

async fn get_status(app: axum::Router, uri: &str) -> StatusCode {
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .header("x-api-key", KEY)
        .body(Body::empty())
        .unwrap();
    app.oneshot(req).await.unwrap().status()
}

async fn post_json(
    app: axum::Router,
    uri: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("x-api-key", KEY)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let parsed = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, parsed)
}

async fn put_json(
    app: axum::Router,
    uri: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .method("PUT")
        .uri(uri)
        .header("x-api-key", KEY)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let parsed = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, parsed)
}

// ─── /series/lookup ─────────────────────────────────────────────────

#[tokio::test]
async fn series_lookup_by_tvdb_returns_stub_when_anibridge_has_no_mapping() {
    // Seerr's connection-test path: it sends a tvdb-prefixed term
    // before any real search. Empty anibridge in the test env means
    // `lookup_by_external_id` returns the `TVDB:<id>` stub so Seerr's
    // add-step can still proceed to the title-search fallback.
    let db = in_memory_pool().await;
    seed_sonarr_enabled(&db, KEY).await;
    let state = build_test_app_state(db, None);
    let app = sonarr_router_with_series(state);
    let body = get_json(app, "/api/v3/series/lookup?term=tvdb:0").await;
    assert!(body.is_array());
    assert_eq!(body.as_array().unwrap().len(), 1);
    assert_eq!(body[0]["title"], "TVDB:0");
    assert_eq!(body[0]["titleSlug"], "tvdb-0");
}

#[tokio::test]
async fn series_lookup_by_tvdb_with_non_numeric_id_returns_400() {
    // `tvdb:abc` is malformed — the parse fails and the handler maps
    // to BAD_REQUEST. Seerr's UI surfaces a clean error rather than
    // a 500.
    let db = in_memory_pool().await;
    seed_sonarr_enabled(&db, KEY).await;
    let state = build_test_app_state(db, None);
    let app = sonarr_router_with_series(state);
    let status = get_status(app, "/api/v3/series/lookup?term=tvdb:notanumber").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ─── /series (GET) ──────────────────────────────────────────────────

#[tokio::test]
async fn list_series_returns_empty_array_for_empty_library() {
    let db = in_memory_pool().await;
    seed_sonarr_enabled(&db, KEY).await;
    let state = build_test_app_state(db, None);
    let app = sonarr_router_with_series(state);
    let body = get_json(app, "/api/v3/series").await;
    assert_eq!(body, json!([]));
}

#[tokio::test]
async fn list_series_returns_one_entry_per_tracked_row() {
    let db = in_memory_pool().await;
    seed_sonarr_enabled(&db, KEY).await;
    seed_series(&db, 1001, "Show A").await;
    seed_series(&db, 1002, "Show B").await;
    let state = build_test_app_state(db, None);
    let app = sonarr_router_with_series(state);
    let body = get_json(app, "/api/v3/series").await;
    let arr = body.as_array().expect("array");
    assert_eq!(arr.len(), 2);
    let titles: Vec<&str> = arr.iter().map(|e| e["title"].as_str().unwrap()).collect();
    assert!(titles.contains(&"Show A"));
    assert!(titles.contains(&"Show B"));
}

// ─── /series/{id} ───────────────────────────────────────────────────

#[tokio::test]
async fn get_series_returns_payload_for_tracked_id() {
    let db = in_memory_pool().await;
    seed_sonarr_enabled(&db, KEY).await;
    let series_id = seed_series(&db, 2001, "Solo Show").await;
    let state = build_test_app_state(db, None);
    let app = sonarr_router_with_series(state);

    let body = get_json(app, &format!("/api/v3/series/{series_id}")).await;
    assert_eq!(body["title"], "Solo Show");
    assert_eq!(body["id"], series_id);
    // Sonarr-shape invariants Seerr's validator depends on:
    assert!(body["seasons"].is_array());
    assert_eq!(body["seriesType"], "anime");
    assert!(body["titleSlug"].as_str().unwrap().starts_with("ryokan-"));
}

#[tokio::test]
async fn get_series_returns_404_for_unknown_id() {
    let db = in_memory_pool().await;
    seed_sonarr_enabled(&db, KEY).await;
    let state = build_test_app_state(db, None);
    let app = sonarr_router_with_series(state);

    let status = get_status(app, "/api/v3/series/9999").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ─── /series (PUT) ──────────────────────────────────────────────────

#[tokio::test]
async fn update_series_flips_monitor_mode_to_none_when_unmonitored() {
    let db = in_memory_pool().await;
    seed_sonarr_enabled(&db, KEY).await;
    let series_id = seed_series(&db, 3001, "Show").await;
    let state = build_test_app_state(db.clone(), None);
    let app = sonarr_router_with_series(state);

    let (status, _body) = put_json(
        app,
        "/api/v3/series",
        json!({
            "id": series_id,
            "monitored": false,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // DB-side: monitor_mode flipped to "none". The response body
    // is built from the pre-update row by design (the handler
    // captures `s` before calling apply_monitor_mode), so the
    // response's `monitored` field reflects the prior state — we
    // assert against the canonical DB column instead.
    let mode: String = sqlx::query_scalar("SELECT monitor_mode FROM series WHERE id = ?")
        .bind(series_id)
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(mode, "none");
}

#[tokio::test]
async fn update_series_flips_monitor_mode_to_all_when_monitored_true() {
    let db = in_memory_pool().await;
    seed_sonarr_enabled(&db, KEY).await;
    let series_id = seed_series(&db, 3002, "Show").await;
    sqlx::query("UPDATE series SET monitor_mode = 'none' WHERE id = ?")
        .bind(series_id)
        .execute(&db)
        .await
        .unwrap();
    let state = build_test_app_state(db.clone(), None);
    let app = sonarr_router_with_series(state);

    let (status, _body) = put_json(
        app,
        "/api/v3/series",
        json!({
            "id": series_id,
            "monitored": true,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let mode: String = sqlx::query_scalar("SELECT monitor_mode FROM series WHERE id = ?")
        .bind(series_id)
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(mode, "all");
}

#[tokio::test]
async fn update_series_returns_404_for_unknown_id() {
    let db = in_memory_pool().await;
    seed_sonarr_enabled(&db, KEY).await;
    let state = build_test_app_state(db, None);
    let app = sonarr_router_with_series(state);

    let (status, _body) = put_json(
        app,
        "/api/v3/series",
        json!({
            "id": 99999,
            "monitored": false,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn update_series_with_no_monitored_field_is_a_noop_on_state() {
    // Seerr sometimes sends an UpdateSeriesBody without the
    // `monitored` field (just tag changes etc.). The handler must
    // tolerate that and not flip monitoring as a side effect.
    let db = in_memory_pool().await;
    seed_sonarr_enabled(&db, KEY).await;
    let series_id = seed_series(&db, 3003, "Show").await;
    let state = build_test_app_state(db.clone(), None);
    let app = sonarr_router_with_series(state);

    let (status, _body) = put_json(
        app,
        "/api/v3/series",
        json!({
            "id": series_id,
            "tags": [],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let mode: String = sqlx::query_scalar("SELECT monitor_mode FROM series WHERE id = ?")
        .bind(series_id)
        .fetch_one(&db)
        .await
        .unwrap();
    // Unchanged — `monitor_mode` schema default is "future" (per
    // models/migrations: ALTER TABLE series ADD COLUMN monitor_mode
    // ... DEFAULT 'future'). Pin against the schema default rather
    // than a hardcoded "all" so a future migration tweak surfaces
    // here as a clean test failure.
    assert_eq!(mode, "future");
}

// ─── /command ───────────────────────────────────────────────────────

#[tokio::test]
async fn command_series_search_returns_queued_envelope() {
    // Seerr fires a SeriesSearch command after add. The handler
    // synthesizes a `queued` envelope that Sonarr-clients accept,
    // and spawns the auto-search in the background. We can't easily
    // verify the spawn without a Nyaa mock; pin the response shape
    // and the fact that the handler returns immediately.
    let db = in_memory_pool().await;
    seed_sonarr_enabled(&db, KEY).await;
    let state = build_test_app_state(db, None);
    let app = sonarr_router_with_series(state);

    let (status, body) = post_json(
        app,
        "/api/v3/command",
        json!({
            "name": "SeriesSearch",
            "seriesId": 42,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "SeriesSearch");
    assert_eq!(body["commandName"], "SeriesSearch");
    assert_eq!(body["status"], "queued");
    assert!(
        body["queued"].is_string(),
        "queued must be an RFC3339 string"
    );
    assert_eq!(body["id"], 1);
}

#[tokio::test]
async fn command_with_no_name_returns_envelope_with_empty_name() {
    // Defensive: Seerr versions vary in what they send. Missing
    // `name` field defaults to an empty string in the response;
    // handler must not 500.
    let db = in_memory_pool().await;
    seed_sonarr_enabled(&db, KEY).await;
    let state = build_test_app_state(db, None);
    let app = sonarr_router_with_series(state);

    let (status, body) = post_json(app, "/api/v3/command", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "");
    assert_eq!(body["status"], "queued");
}
