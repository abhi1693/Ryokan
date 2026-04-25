//! End-to-end watch-list sync test (issue #62 PR B).
//!
//! Stands up a wiremock server that pretends to be AL's GraphQL
//! endpoint, points Ryokan at it via `RYOKAN_ANILIST_API_BASE`, seeds
//! an encrypted `external_accounts` row, and runs `tick_once`. The
//! assertion is a real series row landing in the in-memory DB with
//! the right monitor mode + cached metadata — i.e. all the seams
//! line up: token decrypt → AL fetch → entries normalize → detail
//! batch → series upsert → metadata_cache write → monitor_mode write.
//!
//! Why wiremock instead of a `live_smoke`-style env-gated test
//! against real AL: live smokes need a maintained AL OAuth token +
//! a stable test list. Wiremock against a captured response shape
//! gives us regression coverage without operational debt.

use ryokan::models::external_accounts::{LinkRequest, PROVIDER_ANILIST};
use ryokan::models::monitoring::MonitorMode;
use ryokan::models::{external_accounts, series};
use ryokan::services::anilist;
use ryokan::services::external_sync;
use ryokan::test_support::{build_test_app_state, in_memory_pool};
use serde_json::json;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// AL `MediaListCollection` response: one entry, status CURRENT
/// (Watching), AL id 999, no custom lists.
fn media_list_collection_response() -> serde_json::Value {
    json!({
        "data": {
            "MediaListCollection": {
                "lists": [
                    {
                        "isCustomList": false,
                        "entries": [
                            {
                                "mediaId": 999,
                                "status": "CURRENT",
                                "progress": 4,
                                "score": 8.5,
                                "updatedAt": 1_700_000_000_i64,
                                "notes": "",
                                "customLists": {}
                            }
                        ]
                    }
                ]
            }
        }
    })
}

/// AL detail-batch response shape for `Page.media`. Only the fields
/// AnimeDetail's deserializer requires; everything else falls through
/// to the `#[serde(default)]` arms.
fn page_media_response() -> serde_json::Value {
    json!({
        "data": {
            "Page": {
                "media": [
                    {
                        "id": 999,
                        "idMal": 12345,
                        "title": {
                            "romaji": "Test Series",
                            "english": "Test Series",
                            "native": "テストシリーズ"
                        },
                        "synonyms": [],
                        "coverImage": {
                            "large": "https://example/cover.jpg",
                            "extraLarge": "https://example/cover-xl.jpg"
                        },
                        "bannerImage": "https://example/banner.jpg",
                        "format": "TV",
                        "status": "RELEASING",
                        "episodes": 12,
                        "duration": 24,
                        "season": "SPRING",
                        "seasonYear": 2024,
                        "endDate": { "year": null },
                        "description": "An example series for the e2e test.",
                        "genres": ["Action"],
                        "averageScore": 80,
                        "nextAiringEpisode": null,
                        "streamingEpisodes": [],
                        "relations": { "edges": [] }
                    }
                ]
            }
        }
    })
}

#[tokio::test]
async fn watch_list_sync_imports_series_with_resolved_monitor_mode() {
    // SAFETY: setting env vars in tests is single-threaded within
    // this test (no other test sets RYOKAN_ANILIST_API_BASE) and the
    // override is the entire point of the seam — no race risk in this
    // crate's test layout.
    anilist::reset_state_for_tests();

    let mock = MockServer::start().await;

    // Two POSTs land at the wiremock root: the watch-list fetch
    // (matched on `MediaListCollection` in the body) and the detail-
    // batch fetch (matched on `Page(perPage`). Each gets its own
    // response shape; wiremock dispatches by body match.
    Mock::given(method("POST"))
        .and(path("/"))
        .and(body_string_contains("MediaListCollection"))
        .respond_with(ResponseTemplate::new(200).set_body_json(media_list_collection_response()))
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/"))
        .and(body_string_contains("Page(perPage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page_media_response()))
        .mount(&mock)
        .await;

    unsafe {
        std::env::set_var("RYOKAN_ANILIST_API_BASE", mock.uri());
    }

    let db = in_memory_pool().await;
    let state = build_test_app_state(db.clone(), None);

    // Seed an external_account with the AL provider, fake plaintext
    // tokens (link() encrypts them), and the AL user id we expect
    // back. provider_user_id is read by sync_anilist as the user we
    // fetch the list for.
    external_accounts::link(
        &db,
        LinkRequest {
            provider: PROVIDER_ANILIST.to_string(),
            provider_user_id: "42".to_string(),
            username: "e2e_user".to_string(),
            access_token: "fake-al-access-token".to_string(),
            refresh_token: String::new(),
            access_token_expires_at: None,
            score_format: "POINT_10".to_string(),
        },
    )
    .await
    .expect("seed external account");

    // Run one full sync iteration. Returns the supervised-loop
    // summary string on success.
    let summary = external_sync::tick_once(&state)
        .await
        .expect("tick_once should succeed against the wiremock fixture");
    assert!(
        summary.contains("created 1"),
        "summary should report one created series, got: {summary}"
    );

    // Series row landed with the AL detail data merged in.
    let row = series::get_by_anilist_id(&db, 999)
        .await
        .expect("series query")
        .expect("series row should exist after sync");
    assert_eq!(row.anilist_id, 999);
    assert_eq!(row.mal_id, Some(12345));
    assert_eq!(row.title_english, "Test Series");
    assert_eq!(row.format, "TV");
    assert_eq!(row.status, "RELEASING");
    assert_eq!(row.episodes, Some(12));

    // CURRENT + skip_already_watched=false (the seed default) →
    // monitor_mode = "all". Plan decision #6.
    assert_eq!(
        row.monitor_mode,
        MonitorMode::All.as_str(),
        "Watching status without skip-already-watched must map to All"
    );

    // metadata_cache row written inline by the merge step.
    let cached = ryokan::models::metadata_cache::get_by_series_id(&db, row.id)
        .await
        .expect("metadata_cache query")
        .expect("metadata_cache row should exist");
    assert_eq!(cached.detail.id, 999);
    assert_eq!(cached.detail.title_english, "Test Series");

    // Cursor stamped on success — the next tick filters by this
    // timestamp and won't refetch the same entries.
    let acct = external_accounts::get_current(&db)
        .await
        .expect("get_current")
        .expect("linked account should still exist");
    assert!(
        acct.list_last_synced_at.unwrap_or(0) > 0,
        "list_last_synced_at must advance on a successful tick"
    );
    assert!(
        acct.list_full_resync_at.unwrap_or(0) > 0,
        "first sync is a full sync; list_full_resync_at must also advance"
    );

    // Cleanup: clear the override so other tests in the same process
    // (sequential within a `cargo test` invocation but separate test
    // crates run in parallel) don't pick up our wiremock host.
    unsafe {
        std::env::remove_var("RYOKAN_ANILIST_API_BASE");
    }
    anilist::reset_state_for_tests();
}
