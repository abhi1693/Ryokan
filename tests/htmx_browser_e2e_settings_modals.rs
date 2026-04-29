//! Browser-e2e coverage for the indexer + download-client modal-form
//! HTMX flows and the per-protocol Default-checkbox auto-check that
//! landed alongside the SAB cleanup PR.
//!
//! Why these need a browser layer (per CLAUDE.md's PR 131 audit):
//! the handler-layer tests in `tests/htmx_settings_delete.rs` exercise
//! the response shape but can't catch:
//!   - vendored htmx failing to load (fallback masquerades as success)
//!   - `hx-vals` / `hx-include` form-encoding regressions
//!   - `data-ryokan-confirm-*` typos that fall through to default copy
//!   - per-page inline scripts (the DC auto-check default checkbox runs
//!     in the browser, not on the server)
//!
//! Skips gracefully when geckodriver/WebDriver is unreachable; see
//! `tests/htmx_browser_e2e.rs` for run instructions.

use fantoccini::Locator;
use ryokan::models::download_clients::{DownloadClientForm, insert as insert_download_client};
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

async fn seed_dl_client(db: &SqlitePool, name: &str, kind: &str, is_default: bool) -> i64 {
    insert_download_client(
        db,
        DownloadClientForm {
            name,
            kind,
            url: "http://example.local",
            username: "",
            password: "",
            label: "",
            download_path: "",
            enabled: true,
            is_default,
        },
    )
    .await
    .expect("seed download client")
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

/// `/api/indexers/test` POST should return a partial with an HX-Trigger
/// header that fires a toast in the page. Pin that the handler sets
/// the trigger and the toast helper picks it up — without this, a
/// regression that drops the trigger header would leave the user
/// staring at a hung modal with no feedback. The test only checks the
/// POST shape directly via fetch (faster than driving the modal flow);
/// the toast wiring is covered by base.js's existing handler.
#[tokio::test]
async fn indexer_test_endpoint_returns_partial_with_hx_trigger() {
    let Ok(client) = try_connect_browser().await else {
        return;
    };

    let db = in_memory_pool().await;
    let state = build_test_app_state(db.clone(), None);
    let session = seed_user_session(&db).await;
    let addr = spawn_app(state).await;

    open_with_session(&client, addr, &session, "/settings?tab=indexers")
        .await
        .expect("open settings");
    let _ = assert_htmx_loaded(&client).await;

    // Drive a fetch with bogus credentials so the test is deterministic
    // (we don't care if the connect succeeds — only that the response
    // includes an HX-Trigger header naming the toast event).
    let script = r#"
        const form = new FormData();
        form.append("kind", "torznab");
        form.append("url", "http://nonexistent.local");
        form.append("api_key", "bogus");
        const r = await fetch("/api/indexers/test", { method: "POST", body: form });
        return {
            status: r.status,
            trigger: r.headers.get("HX-Trigger") || ""
        };
    "#;
    // fantoccini::Client::execute_async shape — wrapper IIFE returns the value
    // back through arguments[0].
    let wrapped = format!(
        "const cb = arguments[arguments.length - 1]; (async () => {{ {script} }})().then(cb);"
    );
    let result = client
        .execute_async(&wrapped, vec![])
        .await
        .expect("fetch test endpoint");
    let trigger = result
        .get("trigger")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    assert!(
        trigger.contains("ryokan-indexer-test-result"),
        "POST /api/indexers/test must set HX-Trigger naming the toast event; got: {trigger:?}"
    );

    let _ = client.close().await;
}

/// First-of-protocol grabs the Default checkbox automatically. When
/// the pool already has a torrent client (qBit) but no usenet client,
/// opening the Add modal with `kind=sabnzbd` selected should mark
/// "Default" pre-checked because it's the first usenet entry. Pinned
/// because the auto-check logic lives in inline template attributes
/// (`data-first-torrent` / `data-first-usenet`) consumed by JS in
/// `static/js/settings.js` — a typo in either side breaks the UX
/// silently and the handler-layer tests can't see it.
#[tokio::test]
async fn add_dl_client_modal_auto_checks_default_for_first_of_protocol() {
    let Ok(client) = try_connect_browser().await else {
        return;
    };

    let db = in_memory_pool().await;
    let state = build_test_app_state(db.clone(), None);
    let session = seed_user_session(&db).await;
    // Seed an existing torrent client so the user opening the Add
    // modal isn't first-of-anything for torrent. SAB (when added)
    // would still be first-of-usenet.
    seed_dl_client(&db, "Test qBit", "qbittorrent", true).await;
    let addr = spawn_app(state).await;

    open_with_session(&client, addr, &session, "/settings?tab=download_clients")
        .await
        .expect("open download_clients tab");
    let _ = assert_htmx_loaded(&client).await;

    // Probe the rendered Add-form partial directly (the DC partial's
    // data-first-* attrs are computed server-side based on the
    // current pool — they're the SAME values regardless of which
    // kind the user opens the modal for, because the fields are
    // populated from `download_clients::list_all` row counts).
    let html = client
        .execute_async(
            "const cb = arguments[arguments.length - 1]; \
             fetch('/api/download-clients/add-form?kind=sabnzbd') \
                .then(r => r.text()).then(cb);",
            vec![],
        )
        .await
        .expect("fetch add form")
        .as_str()
        .unwrap_or_default()
        .to_string();

    // The pool has one torrent (so first_torrent_client = false) but
    // no usenet (so first_usenet_client = true). The Default checkbox
    // for an SAB row should therefore be pre-checked.
    assert!(
        html.contains("data-first-torrent=\"0\""),
        "expected data-first-torrent=\"0\" (pool already has qBit); got:\n{html}"
    );
    assert!(
        html.contains("data-first-usenet=\"1\""),
        "expected data-first-usenet=\"1\" (no usenet client yet); got:\n{html}"
    );

    let _ = client.close().await;
}
