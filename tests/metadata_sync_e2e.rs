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

use ryokan::models::{local_metadata, metadata_cache, series};
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

/// TV variant of the seed helper. Format = TV so build_episode_cache's
/// `episodic_format` gate fires and the function reaches the
/// jikan/kitsu episode-titles fetch (the surface this file's TV
/// fixture wants to exercise).
async fn seed_minimal_series_tv(db: &SqlitePool, anilist_id: i64) -> i64 {
    sqlx::query(
        "INSERT INTO series (anilist_id, title, title_romaji, folder_name, status, format) \
         VALUES (?, '', '', '', 'FINISHED', 'TV')",
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

/// MOVIE-format AL response with caller-controlled title fields. The
/// existing `media_detail_response` hard-codes "Test Movie" / "Test
/// Movie EN" / "テスト" — handy for the happy-path test, but unable
/// to exercise the `if !detail.title_english.trim().is_empty() ...
/// else if !detail.title_romaji ...` fallback chain in
/// `build_episode_cache`. This lets a test populate any subset of
/// the three title slots.
fn movie_detail_response_with_titles(
    id: i64,
    romaji: &str,
    english: &str,
    native: &str,
) -> serde_json::Value {
    json!({
        "data": {
            "Media": {
                "id": id,
                "idMal": null,
                "title": {
                    "romaji": romaji,
                    "english": english,
                    "native": native,
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
                "description": "Title-fallback fixture for build_episode_cache.",
                "genres": [],
                "averageScore": null,
                "nextAiringEpisode": null,
                "streamingEpisodes": [],
                "relations": { "edges": [] }
            }
        }
    })
}

/// AL fixture for a TV-format show that build_episode_cache will
/// follow up on with a Jikan episodes fetch. `idMal` is set so the
/// `should_fetch_jikan` branch fires; `episodes: 3` keeps the response
/// payload + downstream loops short.
fn tv_media_detail_response(id: i64, mal_id: i64) -> serde_json::Value {
    json!({
        "data": {
            "Media": {
                "id": id,
                "idMal": mal_id,
                "title": {
                    "romaji": "Test TV Show",
                    "english": "Test TV Show EN",
                    "native": "テスト"
                },
                "synonyms": [],
                "coverImage": {
                    "large": "https://example/cover.jpg",
                    "extraLarge": "https://example/cover-xl.jpg"
                },
                "bannerImage": "https://example/banner.jpg",
                "format": "TV",
                "status": "FINISHED",
                "episodes": 3,
                "duration": 24,
                "season": "WINTER",
                "seasonYear": 2024,
                "endDate": { "year": 2024 },
                "description": "TV fixture for the build_episode_cache path.",
                "genres": ["Action"],
                "averageScore": 80,
                "nextAiringEpisode": null,
                "streamingEpisodes": [],
                "relations": { "edges": [] }
            }
        }
    })
}

#[tokio::test]
async fn refresh_series_metadata_tv_format_merges_jikan_episode_titles() {
    // Drives the build_episode_cache path that the existing happy-path
    // test (MOVIE format) deliberately skips. With format=TV +
    // idMal=Some(...), `should_fetch_jikan` fires and the function
    // hits Jikan for the per-episode titles. Pins the
    //
    //     jikan_eps.remove(&ep_num).map(...).or_else(...)
    //
    // merge ladder at line ~304: the Jikan title takes precedence
    // when present, the fallback chain only fires when Jikan is empty.
    let _gate = ENV_LOCK.lock().await;
    anilist::reset_state_for_tests();

    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .and(body_string_contains("Media(id"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(tv_media_detail_response(5555, 99999)),
        )
        .mount(&mock)
        .await;

    // Jikan episodes fixture — 3 episodes with distinct titles so the
    // assertions can verify the per-episode title round-trips.
    Mock::given(method("GET"))
        .and(path("/anime/99999/episodes"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [
                { "mal_id": 1, "episode_id": 1, "title": "Pilot Episode",
                  "aired": "2024-01-01T00:00:00+00:00" },
                { "mal_id": 2, "episode_id": 2, "title": "Second Steps",
                  "aired": "2024-01-08T00:00:00+00:00" },
                { "mal_id": 3, "episode_id": 3, "title": "Resolution",
                  "aired": "2024-01-15T00:00:00+00:00" },
            ]
        })))
        .mount(&mock)
        .await;

    unsafe {
        std::env::set_var("RYOKAN_ANILIST_API_BASE", mock.uri());
        std::env::set_var("JIKAN_API_BASE", mock.uri());
    }

    let db = in_memory_pool().await;
    let series_id = seed_minimal_series_tv(&db, 5555).await;
    let tracked = series::get_by_id(&db, series_id).await.unwrap().unwrap();

    let detail = metadata_sync::refresh_series_metadata(&db, &tracked, false)
        .await
        .expect("TV refresh should succeed against the wiremock fixture");
    assert_eq!(detail.id, 5555);
    assert_eq!(detail.format, "TV");
    assert_eq!(detail.id_mal, Some(99999));

    // The episode cache must round-trip the Jikan titles.
    let episodes = local_metadata::get_episode_map_for_series(&db, series_id)
        .await
        .expect("episode-map fetch");
    assert_eq!(episodes.len(), 3, "all 3 episodes must be cached");
    assert_eq!(
        episodes.get(&1).map(|e| e.title.as_str()),
        Some("Pilot Episode")
    );
    assert_eq!(
        episodes.get(&2).map(|e| e.title.as_str()),
        Some("Second Steps")
    );
    assert_eq!(
        episodes.get(&3).map(|e| e.title.as_str()),
        Some("Resolution")
    );
    // Source label must be `jikan` since that's where the title came
    // from. Pins the per-episode source-attribution column the UI
    // uses to badge "from MAL/Jikan" vs "from AL/Kitsu/series".
    assert_eq!(
        episodes.get(&1).map(|e| e.source.as_str()),
        Some("jikan"),
        "Jikan-sourced episodes must be tagged accordingly"
    );

    unsafe {
        std::env::remove_var("RYOKAN_ANILIST_API_BASE");
        std::env::remove_var("JIKAN_API_BASE");
    }
    anilist::reset_state_for_tests();
}

#[tokio::test]
async fn refresh_series_metadata_tv_format_falls_back_to_series_title_when_jikan_empty() {
    // When Jikan returns no episodes (e.g. the show isn't indexed in
    // MAL despite an idMal being set), build_episode_cache's per-
    // episode loop falls through to the empty `local` branch and the
    // fallback_title kicks in. For TV format with episodes > 1, the
    // fallback_title is empty (line ~272: `String::new()` because the
    // ep_count > 1 branch doesn't compute one). This pins the empty-
    // title default — episodes still get cached, but with empty
    // titles instead of Jikan ones.
    //
    // The mutation surface here is the `if ep_count <= 1` guard at
    // line 264 of metadata_sync.rs and the fallback ladder around it.
    let _gate = ENV_LOCK.lock().await;
    anilist::reset_state_for_tests();

    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .and(body_string_contains("Media(id"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(tv_media_detail_response(6666, 88888)),
        )
        .mount(&mock)
        .await;
    // Jikan returns empty data — no episodes cached upstream.
    Mock::given(method("GET"))
        .and(path("/anime/88888/episodes"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []})))
        .mount(&mock)
        .await;
    // Kitsu also returns no candidates — without this, `best_candidate`
    // hits real kitsu.io and may match something for "Test TV Show",
    // which would feed the kitsu_eps merge ladder with "Episode N"
    // synthetic titles (kitsu.rs:594) and break the fallback assertion.
    Mock::given(method("GET"))
        .and(path("/anime"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []})))
        .mount(&mock)
        .await;

    unsafe {
        std::env::set_var("RYOKAN_ANILIST_API_BASE", mock.uri());
        std::env::set_var("JIKAN_API_BASE", mock.uri());
        std::env::set_var("RYOKAN_KITSU_API_BASE", mock.uri());
    }

    let db = in_memory_pool().await;
    let series_id = seed_minimal_series_tv(&db, 6666).await;
    let tracked = series::get_by_id(&db, series_id).await.unwrap().unwrap();

    metadata_sync::refresh_series_metadata(&db, &tracked, false)
        .await
        .expect("TV refresh succeeds even when Jikan has no episodes");

    let episodes = local_metadata::get_episode_map_for_series(&db, series_id)
        .await
        .expect("episode-map fetch");
    assert_eq!(episodes.len(), 3, "episode rows must still be created");
    // Empty titles, source tagged as the series-level fallback.
    for ep_num in 1..=3 {
        let ep = episodes.get(&ep_num).expect("episode row present");
        assert!(
            ep.title.is_empty(),
            "TV episode without Jikan title must be empty (got {:?})",
            ep.title
        );
        assert_eq!(
            ep.source.as_str(),
            "series",
            "fallback rows must be tagged 'series'"
        );
    }

    unsafe {
        std::env::remove_var("RYOKAN_ANILIST_API_BASE");
        std::env::remove_var("JIKAN_API_BASE");
        std::env::remove_var("RYOKAN_KITSU_API_BASE");
    }
    anilist::reset_state_for_tests();
}

#[tokio::test]
async fn refresh_series_metadata_movie_falls_back_to_romaji_when_english_empty() {
    // Pin line 265's `if !detail.title_english.trim().is_empty()` guard
    // in build_episode_cache. With title_english = "" and a non-empty
    // title_romaji, the original code falls through to the romaji
    // branch (line 267) and writes "Test Romaji" as the episode
    // title. A `delete !` mutation flips the guard to "fire when
    // title_english IS empty," which would write the empty string.
    let _gate = ENV_LOCK.lock().await;
    anilist::reset_state_for_tests();

    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .and(body_string_contains("Media(id"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(movie_detail_response_with_titles(
                7001,
                "Test Romaji",
                "",
                "テスト",
            )),
        )
        .mount(&mock)
        .await;
    unsafe {
        std::env::set_var("RYOKAN_ANILIST_API_BASE", mock.uri());
    }

    let db = in_memory_pool().await;
    let series_id = seed_minimal_series(&db, 7001).await;
    let tracked = series::get_by_id(&db, series_id).await.unwrap().unwrap();
    metadata_sync::refresh_series_metadata(&db, &tracked, false)
        .await
        .expect("refresh");

    let episodes = local_metadata::get_episode_map_for_series(&db, series_id)
        .await
        .expect("episode-map fetch");
    let ep1 = episodes.get(&1).expect("episode 1 present");
    assert_eq!(
        ep1.title, "Test Romaji",
        "empty title_english must fall through to title_romaji"
    );

    unsafe {
        std::env::remove_var("RYOKAN_ANILIST_API_BASE");
    }
    anilist::reset_state_for_tests();
}

#[tokio::test]
async fn refresh_series_metadata_movie_falls_back_to_native_when_english_and_romaji_empty() {
    // Pin line 267's `if !detail.title_romaji.trim().is_empty()` guard.
    // With both english and romaji empty, the chain falls through to
    // the title_native else-arm (line 270). A `delete !` mutation on
    // line 267 would prefer the empty title_romaji over the populated
    // title_native.
    let _gate = ENV_LOCK.lock().await;
    anilist::reset_state_for_tests();

    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .and(body_string_contains("Media(id"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(movie_detail_response_with_titles(
                7002,
                "",
                "",
                "テスト",
            )),
        )
        .mount(&mock)
        .await;
    unsafe {
        std::env::set_var("RYOKAN_ANILIST_API_BASE", mock.uri());
    }

    let db = in_memory_pool().await;
    let series_id = seed_minimal_series(&db, 7002).await;
    let tracked = series::get_by_id(&db, series_id).await.unwrap().unwrap();
    metadata_sync::refresh_series_metadata(&db, &tracked, false)
        .await
        .expect("refresh");

    let episodes = local_metadata::get_episode_map_for_series(&db, series_id)
        .await
        .expect("episode-map fetch");
    let ep1 = episodes.get(&1).expect("episode 1 present");
    assert_eq!(
        ep1.title, "テスト",
        "both english and romaji empty must fall through to title_native"
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
