//! Wiremock-driven coverage for `services::metadata_sync`. Pre-this-file
//! the only direct unit coverage was the pure helpers
//! (`is_authoritative_detail`, `title_candidates_for_series`,
//! `episode_needs_kitsu_backfill`) plus an empty-DB sweep contract.
//! These tests stand up a wiremock AL endpoint and exercise the full
//! `refresh_series_metadata` orchestration: AL fetch → series row
//! refresh → metadata_cache upsert → relations + episode cache merge.
//!
//! Mirrors the shape of `tests/external_sync_e2e.rs`, which uses the
//! same `RYOKAN_ANILIST_API_BASE` seam. A shared `tokio::sync::Mutex`
//! serializer prevents tests in this binary from racing on the
//! process-wide env var; tests in *other* binaries get their own
//! process so the seam doesn't leak across test crates.
//!
//! Each scenario is tuned to keep the wiremock self-contained:
//!   * MOVIE format + `episodes: 1` → `services::metadata_sync::
//!     build_episode_cache` early-returns without hitting Jikan, so we
//!     don't need a JIKAN_API_BASE override.
//!   * Empty `relations.edges` → `hydrate_relation_tree` fans out zero
//!     follow-up requests.
//!   * `idMal: null` + empty title fields → the AL-failure paths
//!     (5xx, rate-limit) don't fall back to Jikan / Kitsu.

use ryokan::models::{metadata_cache, series};
use ryokan::services::{anilist, metadata_sync};
use ryokan::test_support::in_memory_pool;
use serde_json::json;
use sqlx::SqlitePool;
use std::sync::LazyLock;
use tokio::sync::Mutex;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// One-at-a-time gate around `RYOKAN_ANILIST_API_BASE` writes so
/// tokio's parallel test scheduler can't race two tests on the
/// process-wide env var.
static ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

/// Detail response for AL `Media(id: $id, idMal: $idMal)`. MOVIE
/// format + 1 episode keeps the downstream `build_episode_cache` from
/// reaching Jikan — see the module header.
fn media_detail_response(id: i64) -> serde_json::Value {
    json!({
        "data": {
            "Media": {
                "id": id,
                "idMal": null,
                "title": {
                    "romaji": "Test Movie",
                    "english": "Test Movie EN",
                    "native": "テスト"
                },
                "synonyms": [],
                "coverImage": {
                    "large": "https://example/cover.jpg",
                    "extraLarge": "https://example/cover-xl.jpg"
                },
                "bannerImage": "https://example/banner.jpg",
                "format": "MOVIE",
                "status": "FINISHED",
                "episodes": 1,
                "duration": 120,
                "season": null,
                "seasonYear": 2020,
                "endDate": { "year": 2020 },
                "description": "Self-contained AL fixture for metadata_sync.",
                "genres": ["Action", "Drama"],
                "averageScore": 75,
                "nextAiringEpisode": null,
                "streamingEpisodes": [],
                "relations": { "edges": [] }
            }
        }
    })
}

/// Seed a single tracked series row and return its id. Title fields
/// are deliberately blank so the failure-path fallback to Kitsu's
/// title-fuzz search no-ops (no candidates → final AL retry → same
/// Err returned).
async fn seed_minimal_series(db: &SqlitePool, anilist_id: i64) -> i64 {
    sqlx::query(
        "INSERT INTO series (anilist_id, title, title_romaji, folder_name, status, format) \
         VALUES (?, '', '', '', 'FINISHED', 'MOVIE')",
    )
    .bind(anilist_id)
    .execute(db)
    .await
    .unwrap();
    sqlx::query_scalar::<_, i64>("SELECT id FROM series WHERE anilist_id = ?")
        .bind(anilist_id)
        .fetch_one(db)
        .await
        .unwrap()
}

#[tokio::test]
async fn refresh_series_metadata_writes_cache_on_happy_al_response() {
    let _gate = ENV_LOCK.lock().await;
    anilist::reset_state_for_tests();

    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .and(body_string_contains("Media(id"))
        .respond_with(ResponseTemplate::new(200).set_body_json(media_detail_response(2026)))
        .mount(&mock)
        .await;

    // SAFETY: serialized via ENV_LOCK; no concurrent reader/writer
    // within this binary, and other test binaries get their own
    // process.
    unsafe {
        std::env::set_var("RYOKAN_ANILIST_API_BASE", mock.uri());
    }

    let db = in_memory_pool().await;
    let series_id = seed_minimal_series(&db, 2026).await;
    let tracked = series::get_by_id(&db, series_id)
        .await
        .unwrap()
        .expect("seeded series exists");

    let detail = metadata_sync::refresh_series_metadata(&db, &tracked, false)
        .await
        .expect("refresh should succeed against the wiremock fixture");

    assert_eq!(detail.id, 2026);
    assert_eq!(detail.title_english, "Test Movie EN");
    assert_eq!(detail.format, "MOVIE");

    // metadata_cache row written inline by refresh_series_metadata_inner.
    let cached = metadata_cache::get_by_series_id(&db, series_id)
        .await
        .unwrap()
        .expect("metadata_cache row should exist");
    assert_eq!(cached.detail.id, 2026);
    assert_eq!(cached.detail.title_english, "Test Movie EN");

    // series_genres side table populated from detail.genres.
    let genres: Vec<String> =
        sqlx::query_scalar("SELECT genre FROM series_genres WHERE series_id = ? ORDER BY genre")
            .bind(series_id)
            .fetch_all(&db)
            .await
            .unwrap();
    assert_eq!(genres, vec!["Action".to_string(), "Drama".to_string()]);

    // Series core columns refreshed from the AL detail.
    let refreshed = series::get_by_id(&db, series_id)
        .await
        .unwrap()
        .expect("series row");
    assert_eq!(refreshed.title_english, "Test Movie EN");
    assert_eq!(refreshed.season_year, Some(2020));
    assert_eq!(refreshed.end_year, Some(2020));
    assert_eq!(refreshed.episodes, Some(1));

    unsafe {
        std::env::remove_var("RYOKAN_ANILIST_API_BASE");
    }
    anilist::reset_state_for_tests();
}

#[tokio::test]
async fn refresh_series_metadata_returns_error_on_al_5xx_with_no_fallback_signal() {
    // 5xx is the "AL is down" case — `fetch_live_detail` would
    // normally fall back to MAL/Kitsu, but a series with no mal_id
    // and no title candidates has nowhere to fall back to. The final
    // arm re-calls AL, hits the same 5xx, and returns Err.
    let _gate = ENV_LOCK.lock().await;
    anilist::reset_state_for_tests();

    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&mock)
        .await;
    unsafe {
        std::env::set_var("RYOKAN_ANILIST_API_BASE", mock.uri());
    }

    let db = in_memory_pool().await;
    let series_id = seed_minimal_series(&db, 7777).await;
    let tracked = series::get_by_id(&db, series_id).await.unwrap().unwrap();

    let result = metadata_sync::refresh_series_metadata(&db, &tracked, false).await;
    assert!(result.is_err(), "5xx without fallback signal must Err");

    // No cache row was written when the fetch failed.
    let cached = metadata_cache::get_by_series_id(&db, series_id)
        .await
        .unwrap();
    assert!(cached.is_none(), "5xx must not poison metadata_cache");

    unsafe {
        std::env::remove_var("RYOKAN_ANILIST_API_BASE");
    }
    anilist::reset_state_for_tests();
}

#[tokio::test]
async fn refresh_series_metadata_returns_rate_limit_error_on_al_429() {
    // 429 is the load-bearing case the AL state machine guards
    // against — a rate-limit must NOT silently substitute MAL data
    // (would burn through Jikan's 3 req/s budget on every refresh
    // sweep). Caller gets back an `is_rate_limit_error`-tagged Err
    // and the metadata refresh defers.
    let _gate = ENV_LOCK.lock().await;
    anilist::reset_state_for_tests();

    let mock = MockServer::start().await;
    // AL signals the throttle via X-RateLimit-Remaining=0 +
    // X-RateLimit-Reset=<epoch> on 429. The state machine reads those
    // headers and computes the cooldown.
    let reset_at = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 60) as i64;
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("X-RateLimit-Limit", "30")
                .insert_header("X-RateLimit-Remaining", "0")
                .insert_header("X-RateLimit-Reset", reset_at.to_string()),
        )
        .mount(&mock)
        .await;
    unsafe {
        std::env::set_var("RYOKAN_ANILIST_API_BASE", mock.uri());
    }

    let db = in_memory_pool().await;
    let series_id = seed_minimal_series(&db, 8888).await;
    let tracked = series::get_by_id(&db, series_id).await.unwrap().unwrap();

    let result = metadata_sync::refresh_series_metadata(&db, &tracked, false).await;
    let err = result.expect_err("429 must surface as Err");
    assert!(
        anilist::is_rate_limit_error(&err),
        "expected rate-limit-tagged error, got: {err}"
    );

    unsafe {
        std::env::remove_var("RYOKAN_ANILIST_API_BASE");
    }
    anilist::reset_state_for_tests();
}

#[tokio::test]
async fn refresh_all_series_metadata_skips_when_anilist_is_unreachable() {
    // Drives `run_metadata_sweep` end-to-end with one tracked series
    // whose AL fetch fails. `run_metadata_sweep` MUST NOT panic on a
    // failing series; it counts the failure and continues. The
    // sweep's (refreshed, failed) tuple is part of the
    // `system::api_metadata_refresh` handler contract.
    let _gate = ENV_LOCK.lock().await;
    anilist::reset_state_for_tests();

    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&mock)
        .await;
    unsafe {
        std::env::set_var("RYOKAN_ANILIST_API_BASE", mock.uri());
    }

    let db = in_memory_pool().await;
    seed_minimal_series(&db, 9999).await;

    let (refreshed, failed) = metadata_sync::refresh_all_series_metadata(&db).await;
    assert_eq!(refreshed, 0);
    assert_eq!(failed, 1);

    unsafe {
        std::env::remove_var("RYOKAN_ANILIST_API_BASE");
    }
    anilist::reset_state_for_tests();
}
