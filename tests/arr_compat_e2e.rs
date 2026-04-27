//! Wiremock-driven coverage for the Sonarr/Radarr shim's resource-tier
//! `add_series` / `add_movie` endpoints. Inline tests in
//! `handlers/{sonarr,radarr}_compat/tests/{series,movie}.rs` cover the
//! rest of the shim surface (lookup stub, list, get, update, command);
//! this file fills in the most-hit Seerr path: `POST /series` and
//! `POST /movie` with a valid TVDB/TMDB id mapped through anibridge.
//!
//! Self-contained orchestration: anibridge's TVDB/TMDB index is seeded
//! via `anibridge::seed_external_mappings_for_tests` (no on-disk
//! cache fetch, no network), then AL's GraphQL detail endpoint is
//! redirected to a wiremock server via `RYOKAN_ANILIST_API_BASE`.
//! This is the same shape as `tests/external_sync_e2e.rs` and
//! `tests/metadata_sync_e2e.rs` — three integration-test crates each
//! get their own process so the env-var override doesn't bleed across
//! workspace tests.
//!
//! Tests within this binary share an env-var serializer to keep the
//! `RYOKAN_ANILIST_API_BASE` write race-free.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use ryokan::models::series;
use ryokan::services::anibridge;
use ryokan::services::anilist;
use ryokan::test_support::{
    build_test_app_state, in_memory_pool, radarr_router_with_movie, seed_radarr_enabled,
    seed_sonarr_enabled, sonarr_router_with_series,
};
use serde_json::json;
use sqlx::SqlitePool;
use std::sync::LazyLock;
use tokio::sync::Mutex;
use tower::ServiceExt;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const SONARR_KEY: &str = "test-sonarr-key-arr-e2e-1";
const RADARR_KEY: &str = "test-radarr-key-arr-e2e-1";

static ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

/// AL Media-detail response. Self-contained shape (MOVIE format,
/// `episodes: 1`, empty relations) so `metadata_sync::build_episode_cache`
/// and `hydrate_relation_tree` don't fan out to Jikan or to extra AL
/// queries.
fn media_detail_response(id: i64, title: &str) -> serde_json::Value {
    json!({
        "data": {
            "Media": {
                "id": id,
                "idMal": null,
                "title": {
                    "romaji": title,
                    "english": title,
                    "native": title,
                },
                "synonyms": [],
                "coverImage": {
                    "large": "https://example/cover.jpg",
                    "extraLarge": "https://example/cover-xl.jpg",
                },
                "bannerImage": "https://example/banner.jpg",
                "format": "MOVIE",
                "status": "FINISHED",
                "episodes": 1,
                "duration": 120,
                "season": null,
                "seasonYear": 2022,
                "endDate": { "year": 2022 },
                "description": "Wiremock fixture.",
                "genres": [],
                "averageScore": null,
                "nextAiringEpisode": null,
                "streamingEpisodes": [],
                "relations": { "edges": [] }
            }
        }
    })
}

async fn seed_sonarr_state(db: &SqlitePool) {
    seed_sonarr_enabled(db, SONARR_KEY).await;
}

async fn seed_radarr_state(db: &SqlitePool) {
    seed_radarr_enabled(db, RADARR_KEY).await;
}

async fn post_json(
    app: axum::Router,
    uri: &str,
    api_key: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("x-api-key", api_key)
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

// ─── Sonarr add_series ──────────────────────────────────────────────

#[tokio::test]
async fn sonarr_add_series_creates_db_row_and_returns_payload() {
    let _gate = ENV_LOCK.lock().await;
    anilist::reset_state_for_tests();
    anibridge::clear_cache_for_tests().await;

    // Seed anibridge: TVDB 4242 season 0 → AL 88888. Season 0 is the
    // unscoped catch-all; the handler's `requested_season` is None
    // when no monitored season is in the body.
    anibridge::seed_external_mappings_for_tests(
        &[(4242, 0, Some(88888), None)],
        &[(4242, 0, Some(88888), None)],
    )
    .await;

    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .and(body_string_contains("Media(id"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(media_detail_response(88888, "Seerr Add")),
        )
        .mount(&mock)
        .await;

    // SAFETY: ENV_LOCK serializes env-var access within this binary;
    // each `tests/*.rs` file is its own process, so cross-binary
    // races aren't possible.
    unsafe {
        std::env::set_var("RYOKAN_ANILIST_API_BASE", mock.uri());
    }

    let db = in_memory_pool().await;
    seed_sonarr_state(&db).await;
    let state = build_test_app_state(db.clone(), None);
    let app = sonarr_router_with_series(state);

    let (status, body) = post_json(
        app,
        "/api/v3/series",
        SONARR_KEY,
        json!({
            "tvdbId": 4242,
            "title": "Seerr Add",
            "seasons": [{"seasonNumber": 1, "monitored": true}],
            "monitored": true,
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    // Response carries the Sonarr-shape payload built from the
    // newly-inserted series row.
    assert_eq!(body["title"], "Seerr Add");
    assert_eq!(body["seriesType"], "anime");

    // DB-side: the `series` row landed with the AL id from the mapping.
    let row = series::get_by_anilist_id(&db, 88888)
        .await
        .unwrap()
        .expect("series row should exist after add_series");
    assert_eq!(row.title_english, "Seerr Add");
    assert_eq!(row.format, "MOVIE");
    // Monitor mode pinned to "all" because seasons[0].monitored=true.
    assert_eq!(row.monitor_mode, "all");

    unsafe {
        std::env::remove_var("RYOKAN_ANILIST_API_BASE");
    }
    anilist::reset_state_for_tests();
    anibridge::clear_cache_for_tests().await;
}

#[tokio::test]
async fn sonarr_add_series_returns_400_when_no_mapping_and_no_title() {
    let _gate = ENV_LOCK.lock().await;
    anibridge::clear_cache_for_tests().await;
    // Seed empty anibridge so lookup_by_tvdb / lookup_by_tmdb both
    // return empty Vecs — the title-fallback branch fires, but with
    // an empty `title` field the handler returns 400 before any AL
    // call.
    anibridge::seed_external_mappings_for_tests(&[], &[]).await;

    let db = in_memory_pool().await;
    seed_sonarr_state(&db).await;
    let state = build_test_app_state(db, None);
    let app = sonarr_router_with_series(state);

    let (status, _body) = post_json(
        app,
        "/api/v3/series",
        SONARR_KEY,
        json!({
            "tvdbId": 9999,
            "title": "",
            "seasons": [],
        }),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);

    anibridge::clear_cache_for_tests().await;
}

#[tokio::test]
async fn sonarr_add_series_pins_monitor_mode_to_none_when_seerr_unmonitors() {
    // Seerr can send `seasons: [{monitored: false}]` to add but not
    // monitor. The handler maps that to MonitorMode::None on the
    // upserted row.
    let _gate = ENV_LOCK.lock().await;
    anilist::reset_state_for_tests();
    anibridge::clear_cache_for_tests().await;
    anibridge::seed_external_mappings_for_tests(
        &[(5555, 0, Some(77777), None)],
        &[(5555, 0, Some(77777), None)],
    )
    .await;

    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .and(body_string_contains("Media(id"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(media_detail_response(77777, "Unmonitored Add")),
        )
        .mount(&mock)
        .await;
    unsafe {
        std::env::set_var("RYOKAN_ANILIST_API_BASE", mock.uri());
    }

    let db = in_memory_pool().await;
    seed_sonarr_state(&db).await;
    let state = build_test_app_state(db.clone(), None);
    let app = sonarr_router_with_series(state);

    let (status, _body) = post_json(
        app,
        "/api/v3/series",
        SONARR_KEY,
        json!({
            "tvdbId": 5555,
            "title": "Unmonitored Add",
            "seasons": [{"seasonNumber": 1, "monitored": false}],
            "monitored": false,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let row = series::get_by_anilist_id(&db, 77777)
        .await
        .unwrap()
        .expect("series row");
    assert_eq!(row.monitor_mode, "none");

    unsafe {
        std::env::remove_var("RYOKAN_ANILIST_API_BASE");
    }
    anilist::reset_state_for_tests();
    anibridge::clear_cache_for_tests().await;
}

// ─── Radarr add_movie ──────────────────────────────────────────────

#[tokio::test]
async fn radarr_add_movie_creates_db_row_and_returns_payload() {
    let _gate = ENV_LOCK.lock().await;
    anilist::reset_state_for_tests();
    anibridge::clear_cache_for_tests().await;
    anibridge::seed_external_mappings_for_tests(&[], &[(6060, 0, Some(11111), None)]).await;

    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .and(body_string_contains("Media(id"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(media_detail_response(11111, "Seerr Movie")),
        )
        .mount(&mock)
        .await;
    unsafe {
        std::env::set_var("RYOKAN_ANILIST_API_BASE", mock.uri());
    }

    let db = in_memory_pool().await;
    seed_radarr_state(&db).await;
    let state = build_test_app_state(db.clone(), None);
    let app = radarr_router_with_movie(state);

    let (status, body) = post_json(
        app,
        "/radarr/api/v3/movie",
        RADARR_KEY,
        json!({
            "tmdbId": 6060,
            "title": "Seerr Movie",
            "monitored": true,
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["title"], "Seerr Movie");
    // Both rating slots present (Seerr renders whichever it reads).
    assert!(body["ratings"]["imdb"]["value"].is_f64());
    assert!(body["ratings"]["tmdb"]["value"].is_f64());

    let row = series::get_by_anilist_id(&db, 11111)
        .await
        .unwrap()
        .expect("series row should exist after add_movie");
    assert_eq!(row.title_english, "Seerr Movie");
    assert_eq!(row.format, "MOVIE");
    assert_eq!(row.monitor_mode, "all");

    unsafe {
        std::env::remove_var("RYOKAN_ANILIST_API_BASE");
    }
    anilist::reset_state_for_tests();
    anibridge::clear_cache_for_tests().await;
}

#[tokio::test]
async fn radarr_add_movie_returns_400_when_no_mapping_and_no_title() {
    let _gate = ENV_LOCK.lock().await;
    anibridge::clear_cache_for_tests().await;
    anibridge::seed_external_mappings_for_tests(&[], &[]).await;

    let db = in_memory_pool().await;
    seed_radarr_state(&db).await;
    let state = build_test_app_state(db, None);
    let app = radarr_router_with_movie(state);

    let (status, _body) = post_json(
        app,
        "/radarr/api/v3/movie",
        RADARR_KEY,
        json!({
            "tmdbId": 9999,
            "title": "",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    anibridge::clear_cache_for_tests().await;
}

#[tokio::test]
async fn radarr_add_movie_pins_monitor_mode_to_none_when_seerr_unmonitors() {
    let _gate = ENV_LOCK.lock().await;
    anilist::reset_state_for_tests();
    anibridge::clear_cache_for_tests().await;
    anibridge::seed_external_mappings_for_tests(&[], &[(7070, 0, Some(22222), None)]).await;

    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .and(body_string_contains("Media(id"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(media_detail_response(22222, "Unmonitored Movie")),
        )
        .mount(&mock)
        .await;
    unsafe {
        std::env::set_var("RYOKAN_ANILIST_API_BASE", mock.uri());
    }

    let db = in_memory_pool().await;
    seed_radarr_state(&db).await;
    let state = build_test_app_state(db.clone(), None);
    let app = radarr_router_with_movie(state);

    let (status, _body) = post_json(
        app,
        "/radarr/api/v3/movie",
        RADARR_KEY,
        json!({
            "tmdbId": 7070,
            "title": "Unmonitored Movie",
            "monitored": false,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let row = series::get_by_anilist_id(&db, 22222)
        .await
        .unwrap()
        .expect("series row");
    assert_eq!(row.monitor_mode, "none");

    unsafe {
        std::env::remove_var("RYOKAN_ANILIST_API_BASE");
    }
    anilist::reset_state_for_tests();
    anibridge::clear_cache_for_tests().await;
}
