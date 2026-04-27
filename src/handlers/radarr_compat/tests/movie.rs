//! Resource-tier endpoints for the Radarr shim (`/movie`,
//! `/movie/{id}`, `/movie/lookup`, `/command`). Mirrors the Sonarr
//! `series.rs` test layout — auth is exercised in `auth.rs`,
//! system-tier shapes in `system.rs`, this file covers the
//! Seerr-add-and-update flow.
//!
//! The lookup-by-title path goes out to AniList; covered (or not)
//! at the integration level. What's covered here without network:
//!
//! * `/movie/lookup?term=tmdb:N` — empty anibridge → stub.
//! * `/movie` (GET) — DB-only.
//! * `/movie/{id}` — DB-only.
//! * `/movie` (PUT) — monitor flip, DB-only.
//! * `/command` — fire-and-forget canned response.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;

use crate::test_support::{
    build_test_app_state, in_memory_pool, radarr_router_with_movie, seed_radarr_enabled,
    seed_series,
};

const KEY: &str = "test-radarr-key-01234567";

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

// ─── /movie/lookup ──────────────────────────────────────────────────

#[tokio::test]
async fn movie_lookup_by_tmdb_returns_stub_when_anibridge_has_no_mapping() {
    let db = in_memory_pool().await;
    seed_radarr_enabled(&db, KEY).await;
    let state = build_test_app_state(db, None);
    let app = radarr_router_with_movie(state);
    let body = get_json(app, "/radarr/api/v3/movie/lookup?term=tmdb:0").await;
    assert!(body.is_array());
    assert_eq!(body.as_array().unwrap().len(), 1);
    assert_eq!(body[0]["title"], "TMDB:0");
    assert_eq!(body[0]["titleSlug"], "tmdb-0");
}

#[tokio::test]
async fn movie_lookup_by_tmdb_with_non_numeric_id_returns_400() {
    let db = in_memory_pool().await;
    seed_radarr_enabled(&db, KEY).await;
    let state = build_test_app_state(db, None);
    let app = radarr_router_with_movie(state);
    let status = get_status(app, "/radarr/api/v3/movie/lookup?term=tmdb:notanumber").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ─── /movie (GET) ───────────────────────────────────────────────────

#[tokio::test]
async fn list_movies_returns_empty_array_for_empty_library() {
    let db = in_memory_pool().await;
    seed_radarr_enabled(&db, KEY).await;
    let state = build_test_app_state(db, None);
    let app = radarr_router_with_movie(state);
    let body = get_json(app, "/radarr/api/v3/movie").await;
    assert_eq!(body, json!([]));
}

#[tokio::test]
async fn list_movies_returns_one_entry_per_tracked_row() {
    let db = in_memory_pool().await;
    seed_radarr_enabled(&db, KEY).await;
    seed_series(&db, 1101, "Movie A").await;
    seed_series(&db, 1102, "Movie B").await;
    let state = build_test_app_state(db, None);
    let app = radarr_router_with_movie(state);
    let body = get_json(app, "/radarr/api/v3/movie").await;
    let arr = body.as_array().expect("array");
    assert_eq!(arr.len(), 2);
    let titles: Vec<&str> = arr.iter().map(|e| e["title"].as_str().unwrap()).collect();
    assert!(titles.contains(&"Movie A"));
    assert!(titles.contains(&"Movie B"));
}

// ─── /movie/{id} ───────────────────────────────────────────────────

#[tokio::test]
async fn get_movie_returns_payload_for_tracked_id() {
    let db = in_memory_pool().await;
    seed_radarr_enabled(&db, KEY).await;
    let series_id = seed_series(&db, 2101, "Solo Movie").await;
    let state = build_test_app_state(db, None);
    let app = radarr_router_with_movie(state);

    let body = get_json(app, &format!("/radarr/api/v3/movie/{series_id}")).await;
    assert_eq!(body["title"], "Solo Movie");
    assert_eq!(body["id"], series_id);
    // Radarr-shape invariants: both rating slots present, isAvailable
    // populated, titleSlug uses the ryokan-{anilist} format.
    assert!(body["ratings"]["imdb"]["value"].is_f64());
    assert!(body["ratings"]["tmdb"]["value"].is_f64());
    assert!(body["isAvailable"].is_boolean());
    assert!(body["titleSlug"].as_str().unwrap().starts_with("ryokan-"));
}

#[tokio::test]
async fn get_movie_returns_404_for_unknown_id() {
    let db = in_memory_pool().await;
    seed_radarr_enabled(&db, KEY).await;
    let state = build_test_app_state(db, None);
    let app = radarr_router_with_movie(state);

    let status = get_status(app, "/radarr/api/v3/movie/9999").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ─── /movie (PUT) ───────────────────────────────────────────────────

#[tokio::test]
async fn update_movie_flips_monitor_mode_to_none_when_unmonitored() {
    let db = in_memory_pool().await;
    seed_radarr_enabled(&db, KEY).await;
    let series_id = seed_series(&db, 3101, "Movie").await;
    let state = build_test_app_state(db.clone(), None);
    let app = radarr_router_with_movie(state);

    let (status, _body) = put_json(
        app,
        "/radarr/api/v3/movie",
        json!({
            "id": series_id,
            "monitored": false,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // The handler captures `s` before apply_monitor_mode runs, so
    // the response reflects the prior state. The DB column is the
    // ground truth — pin against it.
    let mode: String = sqlx::query_scalar("SELECT monitor_mode FROM series WHERE id = ?")
        .bind(series_id)
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(mode, "none");
}

#[tokio::test]
async fn update_movie_flips_monitor_mode_to_all_when_monitored_true() {
    let db = in_memory_pool().await;
    seed_radarr_enabled(&db, KEY).await;
    let series_id = seed_series(&db, 3102, "Movie").await;
    sqlx::query("UPDATE series SET monitor_mode = 'none' WHERE id = ?")
        .bind(series_id)
        .execute(&db)
        .await
        .unwrap();
    let state = build_test_app_state(db.clone(), None);
    let app = radarr_router_with_movie(state);

    let (status, _body) = put_json(
        app,
        "/radarr/api/v3/movie",
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
async fn update_movie_returns_404_for_unknown_id() {
    let db = in_memory_pool().await;
    seed_radarr_enabled(&db, KEY).await;
    let state = build_test_app_state(db, None);
    let app = radarr_router_with_movie(state);

    let (status, _body) = put_json(
        app,
        "/radarr/api/v3/movie",
        json!({
            "id": 99999,
            "monitored": false,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ─── /command ───────────────────────────────────────────────────────

#[tokio::test]
async fn command_movies_search_returns_queued_envelope() {
    // Seerr fires `MoviesSearch` (note the plural) with a `movieIds`
    // array; the handler spawns one auto-search per id and returns
    // the queued envelope immediately.
    let db = in_memory_pool().await;
    seed_radarr_enabled(&db, KEY).await;
    let state = build_test_app_state(db, None);
    let app = radarr_router_with_movie(state);

    let (status, body) = post_json(
        app,
        "/radarr/api/v3/command",
        json!({
            "name": "MoviesSearch",
            "movieIds": [42, 43],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "MoviesSearch");
    assert_eq!(body["commandName"], "MoviesSearch");
    assert_eq!(body["status"], "queued");
    assert!(body["queued"].is_string());
    assert_eq!(body["id"], 1);
}

#[tokio::test]
async fn command_with_no_name_returns_envelope_with_empty_name() {
    let db = in_memory_pool().await;
    seed_radarr_enabled(&db, KEY).await;
    let state = build_test_app_state(db, None);
    let app = radarr_router_with_movie(state);

    let (status, body) = post_json(app, "/radarr/api/v3/command", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "");
    assert_eq!(body["status"], "queued");
}
