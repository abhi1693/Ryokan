//! Browser-e2e coverage for the indexer Edit-card → modal flow.
//! That flow is JS-driven (`openIndexerEditModal` calls `htmx.ajax`),
//! so the browser layer adds value: a typo in the JS, a missing data
//! attribute on the card, or htmx itself failing to load would all
//! break this in production but pass at the handler layer.
//!
//! The two non-JS-interactive checks that were here (indexer Test
//! endpoint HX-Trigger header + DC add-form data-first-* attributes)
//! moved to `tests/htmx_settings_modal_handlers.rs` since they validate
//! server-rendered output and don't benefit from a real browser. The
//! browser-e2e variants were observably flaky against geckodriver
//! (in-page `fetch` + `goto`-then-`source` both intermittently
//! returned empty bodies under parallel test execution); the
//! handler-direct variant is deterministic.
//!
//! Skips gracefully when geckodriver/WebDriver is unreachable; see
//! `tests/htmx_browser_e2e.rs` for run instructions.

use fantoccini::Locator;
use ryokan::models::indexers::{IndexerForm, insert as insert_indexer};
use ryokan::test_support::{build_test_app_state, in_memory_pool};
use sqlx::SqlitePool;
use std::time::Duration;

#[path = "common/browser_e2e.rs"]
mod browser_e2e;
use browser_e2e::{
    assert_htmx_loaded, open_with_session, seed_user_session, spawn_app, try_connect_browser,
};

async fn seed_indexer(db: &SqlitePool, name: &str) -> i64 {
    insert_indexer(
        db,
        IndexerForm {
            name,
            kind: "torznab",
            url: "https://example.com/torznab",
            api_key: "k",
            priority: 25,
            enabled: true,
            is_private_tracker: false,
            seed_ratio: None,
            seed_time_minutes: None,
            min_seeders: 1,
            request_timeout_secs: None,
            download_client_id: None,
            rss_enabled: false,
        },
    )
    .await
    .expect("seed indexer")
}

/// Clicking the Edit button on an indexer card fires an
/// `hx-get="/settings/indexers/{id}/edit-form"` that swaps the modal
/// body. The handler must return a populated form (Name, URL, API
/// Key, Priority, etc.) bound to the row's existing values, not a
/// blank shell. Pin the wire shape so a regex-replace refactor of the
/// partial doesn't silently nullify the populated values.
#[tokio::test]
async fn indexer_edit_modal_loads_populated_form() {
    let Ok(client) = try_connect_browser().await else {
        return; // WebDriver unreachable — skip.
    };

    let db = in_memory_pool().await;
    let state = build_test_app_state(db.clone(), None);
    let session = seed_user_session(&db).await;
    let indexer_id = seed_indexer(&db, "Test Indexer Alpha").await;
    let addr = spawn_app(state).await;

    open_with_session(&client, addr, &session, "/settings?tab=indexers")
        .await
        .expect("open settings");
    let _ = assert_htmx_loaded(&client).await;

    // Click the indexer card body — the whole card is a click
    // target (`role="button"` on the body div with `data-indexer-id`),
    // and `openIndexerEditModal` fetches the edit-form partial via
    // `htmx.ajax` into `#indexer-modal-body`.
    let card_body = client
        .wait()
        .at_most(Duration::from_secs(5))
        .for_element(Locator::Css(&format!("[data-indexer-id=\"{indexer_id}\"]")))
        .await
        .expect("indexer card body");
    card_body.click().await.expect("click card");

    // Modal body should populate with the indexer's name pre-filled.
    let name_input = client
        .wait()
        .at_most(Duration::from_secs(5))
        .for_element(Locator::Css("#indexer-modal-body input[name=\"name\"]"))
        .await
        .expect("name input present");
    let name_value = name_input.prop("value").await.expect("read value");
    assert_eq!(
        name_value.as_deref(),
        Some("Test Indexer Alpha"),
        "edit modal must populate Name from the indexer row, not render blank"
    );

    let _ = client.close().await;
}
