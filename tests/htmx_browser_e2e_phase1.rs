//! Phase 1 (per-row settings deletes) browser e2e tests.
//!
//! Each test seeds real DB rows, navigates the browser to the real
//! `/settings` page (not a fixture), exercises the row's delete form,
//! and asserts on the post-action DOM. This covers the same surface
//! as `tests/htmx_settings_delete.rs` but at the layer where
//! template-attribute drift, the `htmx:confirm` bridge in `base.js`,
//! and the actual swap behavior all matter.
//!
//! Skips gracefully when the WebDriver endpoint is unreachable —
//! see `tests/htmx_browser_e2e.rs` for the run instructions and
//! the planned removal post-migration.
//!
//! Test plan covered here (Phase 1.0):
//!
//! - Indexers: confirm-then-delete removes the row from the DOM.
//! - Indexers: cancel-on-confirm leaves the row alone (regression guard
//!   for the "cancel modal silently submitted" bug fixed by the
//!   htmx:confirm bridge in `base.js`).
//! - Download clients: deleting the default auto-promotes the next-
//!   lowest-id row to default — verifies the `was_default` + `MIN(id)`
//!   promotion path in `models::download_clients::delete` end-to-end
//!   (the "Default" badge appears on the surviving row after reload).
//! - Custom formats: deleting the *last* CF triggers `HX-Refresh` so
//!   the empty-state CTA appears (per-row swap can't inject an empty-
//!   state since the empty-state lives outside the table loop).
//! - Groups: row delete (no confirm modal — direct htmx swap).

use std::net::SocketAddr;
use std::time::Duration;

use fantoccini::ClientBuilder;
use fantoccini::Locator;
use ryokan::models::custom_formats as cf_model;
use ryokan::models::download_clients::{DownloadClientForm, insert as insert_download_client};
use ryokan::models::group_source_map;
use ryokan::models::indexers::{IndexerForm, insert as insert_indexer};
use ryokan::services::source::Source;
use ryokan::test_support::{build_test_app_state, e2e_browser_app, in_memory_pool};
use sqlx::SqlitePool;

// ─── Harness (mirrors the boilerplate in htmx_browser_e2e.rs) ──────

async fn spawn_app(state: ryokan::AppState) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind random port");
    let addr = listener.local_addr().expect("local_addr");
    let app = e2e_browser_app(state);
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    addr
}

async fn try_connect_browser() -> Result<fantoccini::Client, String> {
    let url = std::env::var("RYOKAN_WEBDRIVER_URL")
        .unwrap_or_else(|_| "http://localhost:4444".to_string());

    let mut caps = serde_json::Map::new();
    let headless = std::env::var("RYOKAN_BROWSER_HEADLESS")
        .map(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
        .unwrap_or(true);
    let mut firefox_opts = serde_json::Map::new();
    if headless {
        firefox_opts.insert("args".to_string(), serde_json::json!(["-headless"]));
    }
    if let Some(bin) = resolve_browser_binary() {
        firefox_opts.insert("binary".to_string(), serde_json::json!(bin));
    }
    caps.insert(
        "moz:firefoxOptions".to_string(),
        serde_json::Value::Object(firefox_opts),
    );

    ClientBuilder::native()
        .capabilities(caps)
        .connect(&url)
        .await
        .map_err(|e| format!("WebDriver at {url} unavailable: {e}"))
}

fn resolve_browser_binary() -> Option<String> {
    if let Ok(explicit) = std::env::var("RYOKAN_BROWSER_BIN")
        && !explicit.is_empty()
    {
        return Some(explicit);
    }
    for candidate in ["firefox", "firefox-esr"] {
        if let Some(path) = which(candidate) {
            return Some(path);
        }
    }
    if let Some(librewolf) = which("librewolf") {
        return Some(librewolf_shim(&librewolf));
    }
    None
}

fn librewolf_shim(librewolf_path: &str) -> String {
    let wrapper_dir = std::env::temp_dir().join("ryokan-librewolf-shim");
    std::fs::create_dir_all(&wrapper_dir).expect("create shim dir");
    let wrapper_path = wrapper_dir.join("firefox-shim.sh");
    let body = format!(
        "#!/bin/sh\n\
         if [ \"$1\" = \"--version\" ] || [ \"$1\" = \"-version\" ]; then\n\
         \techo \"Mozilla Firefox 149.0\"\n\
         \texit 0\n\
         fi\n\
         exec {} \"$@\"\n",
        shell_quote(librewolf_path),
    );
    std::fs::write(&wrapper_path, body).expect("write shim");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&wrapper_path)
            .expect("stat shim")
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&wrapper_path, perms).expect("chmod shim");
    }
    wrapper_path.to_string_lossy().into_owned()
}

fn shell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

fn which(bin: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(bin);
        if candidate.is_file() {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    None
}

/// Plant the test session cookie + load `/settings?tab=<tab>` so the
/// browser is sitting on the page with the row(s) we want to drive.
async fn open_settings(
    client: &fantoccini::Client,
    addr: SocketAddr,
    session_token: &str,
    tab: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let base = format!("http://{addr}");
    client.goto(&format!("{base}/login")).await?;
    let raw = format!("session={session_token}; Path=/; SameSite=Lax");
    let cookie = fantoccini::cookies::Cookie::parse(raw)?;
    client.add_cookie(cookie).await?;
    client.goto(&format!("{base}/settings?tab={tab}")).await?;
    Ok(())
}

/// Assert `window.htmx` is defined — i.e. the vendored script
/// actually loaded. Use this on tests where the htmx-handled action
/// legitimately changes the URL (e.g. HX-Refresh), so the
/// stay-in-place URL check in `assert_htmx_handled_in_place` doesn't
/// apply.
async fn assert_htmx_loaded(client: &fantoccini::Client) -> Result<(), Box<dyn std::error::Error>> {
    let htmx_loaded: bool = client
        .execute(
            r#"return typeof window.htmx === 'object' && !!window.htmx;"#,
            vec![],
        )
        .await?
        .as_bool()
        .unwrap_or(false);
    if !htmx_loaded {
        return Err("window.htmx is undefined — vendored script failed to load".into());
    }
    Ok(())
}

/// Assert htmx is actually loaded and the last form action did NOT
/// redirect-navigate (the form-POST fallback path appends `?msg=...`
/// to the URL on success). Without this check, every Phase 1 row-
/// delete test would silently pass under "htmx failed to load" — the
/// form's `action="..."` + `method="post"` fallback gets a 303
/// redirect → page reloads → row is gone → test sees "row vanished"
/// and exits green. Caught during a mutation-testing audit.
async fn assert_htmx_handled_in_place(
    client: &fantoccini::Client,
    expected_url_prefix: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let htmx_loaded: bool = client
        .execute(
            r#"return typeof window.htmx === 'object' && !!window.htmx;"#,
            vec![],
        )
        .await?
        .as_bool()
        .unwrap_or(false);
    if !htmx_loaded {
        return Err(
            "window.htmx is undefined — vendored script failed to load and the form-POST \
             fallback handled the request"
                .into(),
        );
    }
    let url = client.current_url().await?;
    let url_str = url.as_str();
    if url_str.contains("msg=") || url_str.contains("err=") {
        return Err(format!(
            "URL contains a flash query param ({url_str}) — the form-POST fallback redirected \
             instead of htmx swapping in place; expected to stay at {expected_url_prefix}"
        )
        .into());
    }
    if !url_str.starts_with(expected_url_prefix) {
        return Err(format!(
            "URL changed from {expected_url_prefix} → {url_str} — page navigated when it should \
             have swapped in place"
        )
        .into());
    }
    Ok(())
}

/// Assert that the confirm modal's title or body contains the given
/// substring. `slot` is "title" or "body" — maps to the
/// `#ryokan-confirm-title` and `#ryokan-confirm-body` elements in
/// `templates/base.html`. Used to verify that the row form's
/// `data-ryokan-confirm-*` attrs round-trip into the modal copy.
async fn assert_modal_text(
    client: &fantoccini::Client,
    slot: &str,
    expected_substring: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let id = format!("ryokan-confirm-{slot}");
    let actual: String = client
        .execute(
            r#"
            const id = arguments[0];
            const el = document.getElementById(id);
            return el ? (el.textContent || '') : '';
            "#,
            vec![serde_json::json!(id)],
        )
        .await?
        .as_str()
        .unwrap_or("")
        .to_string();
    if !actual.contains(expected_substring) {
        return Err(format!(
            "modal {slot} did not contain `{expected_substring}` — got `{actual}`"
        )
        .into());
    }
    Ok(())
}

/// Assert that some node in the DOM contains `marker` as part of its
/// text content. Companion to `wait_for_row_removed` for the survivor-
/// row check pattern: after a delete, the deleted row should be gone
/// AND adjacent rows should still be there. Without this, a stray
/// `hx-target="closest div"` (which would swap the entire containing
/// div) silently passes — the deleted marker disappears but so do
/// all its siblings.
async fn assert_dom_contains(
    client: &fantoccini::Client,
    marker: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let present: bool = client
        .execute(
            r#"
            const marker = arguments[0];
            return document.body.textContent.includes(marker);
            "#,
            vec![serde_json::json!(marker)],
        )
        .await?
        .as_bool()
        .unwrap_or(false);
    if !present {
        return Err(format!(
            "expected DOM to still contain `{marker}` — over-broad swap target swallowed it?"
        )
        .into());
    }
    Ok(())
}

/// Wait until the row that contains `unique_marker` text disappears.
/// Returns Err on timeout instead of panicking — panics inside the
/// inner async block kill the task before `client.close()` runs,
/// leaving the geckodriver session orphaned and breaking subsequent
/// tests with "Session is already started."
async fn wait_for_row_removed(
    client: &fantoccini::Client,
    unique_marker: &str,
    timeout: Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        // JS query: scan all <tr> for one containing the marker.
        // WebDriver's text-based locators don't support arbitrary
        // substring matching across descendants reliably.
        let still_present: bool = client
            .execute(
                r#"
                const marker = arguments[0];
                return Array.from(document.querySelectorAll('tr'))
                    .some(tr => tr.textContent.includes(marker));
                "#,
                vec![serde_json::json!(unique_marker)],
            )
            .await?
            .as_bool()
            .unwrap_or(false);
        if !still_present {
            return Ok(());
        }
        if std::time::Instant::now() > deadline {
            return Err(format!(
                "row containing `{unique_marker}` was not removed within {timeout:?}"
            )
            .into());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Wait for the confirm modal (`#ryokan-confirm-modal` in
/// `templates/base.html`) to become visible. Toggled via
/// `style.display`, so check computed visibility.
async fn wait_for_confirm_modal(
    client: &fantoccini::Client,
    timeout: Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let visible: bool = client
            .execute(
                r#"
                const m = document.getElementById('ryokan-confirm-modal');
                if (!m) return false;
                return getComputedStyle(m).display !== 'none';
                "#,
                vec![],
            )
            .await?
            .as_bool()
            .unwrap_or(false);
        if visible {
            return Ok(());
        }
        if std::time::Instant::now() > deadline {
            return Err(format!("confirm modal did not appear within {timeout:?}").into());
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}

/// Find a `<form>` whose `data-ryokan-confirm-body` attribute (or any
/// child text) contains the unique marker, and click its submit
/// button. This avoids depending on row indices, which break when
/// other tests run in parallel against shared seed data.
async fn click_delete_for(
    client: &fantoccini::Client,
    unique_marker: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    client
        .execute(
            r#"
            const marker = arguments[0];
            const forms = Array.from(document.querySelectorAll('form'));
            const target = forms.find(f =>
                (f.getAttribute('data-ryokan-confirm-body') || '').includes(marker)
                || f.closest('tr')?.textContent.includes(marker));
            if (!target) throw new Error('no delete form found for marker: ' + marker);
            const btn = target.querySelector('button[type="submit"]');
            if (!btn) throw new Error('delete form has no submit button');
            btn.click();
            "#,
            vec![serde_json::json!(unique_marker)],
        )
        .await?;
    Ok(())
}

// ─── Seed helpers ──────────────────────────────────────────────────

async fn seed_indexer(db: &SqlitePool, name: &str) -> i64 {
    insert_indexer(
        db,
        IndexerForm {
            name,
            kind: "torznab",
            url: "https://example.com/torznab",
            api_key: "abc",
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

async fn seed_dl_client(db: &SqlitePool, name: &str, is_default: bool) -> i64 {
    insert_download_client(
        db,
        DownloadClientForm {
            name,
            kind: "qbittorrent",
            url: "http://qbit.local",
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

async fn seed_cf(db: &SqlitePool, name: &str) -> i64 {
    cf_model::insert(db, name, None, "{}", 0, "manual")
        .await
        .expect("seed custom format")
}

async fn seed_group(db: &SqlitePool, name: &str) {
    group_source_map::upsert_user_edit(db, name, Source::BluRay, 1.0, "test seed")
        .await
        .expect("seed group");
}

async fn seed_user_session(db: &SqlitePool) -> String {
    let user_id = ryokan::models::user::create_user(db, "phase1-user", "hunter2-test-password")
        .await
        .expect("create user");
    ryokan::models::session::create_session(db, user_id)
        .await
        .expect("create session")
}

// ─── Tests ─────────────────────────────────────────────────────────

#[tokio::test]
async fn indexers_delete_confirm_removes_row() {
    // Two indexers seeded so the test can also assert the survivor
    // row stays in the DOM. Without that, an over-broad `hx-target`
    // (e.g. `closest div` swapping the whole table) passes silently.
    let db = in_memory_pool().await;
    let _doomed = seed_indexer(&db, "Phase1Test-IndexerA").await;
    let _survivor = seed_indexer(&db, "Phase1Test-IndexerSurvivor").await;
    let token = seed_user_session(&db).await;
    let state = build_test_app_state(db.clone(), None);
    let addr = spawn_app(state).await;

    let client = match try_connect_browser().await {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("[skip] {msg}");
            return;
        }
    };

    let result = async {
        open_settings(&client, addr, &token, "indexers").await?;
        client
            .find(Locator::Css("button[type=\"submit\"].btn-danger"))
            .await?;
        click_delete_for(&client, "Phase1Test-IndexerA").await?;
        wait_for_confirm_modal(&client, Duration::from_secs(2)).await?;
        // Modal-copy regression guard: the row form's
        // `data-ryokan-confirm-*` attrs flow through `base.js`'s
        // `ryokanConfirmFromAttrs` → modal title/body. If that
        // pipeline regresses (e.g. wrong attr name, modal element
        // ID typo), the modal would render with default copy
        // ("Confirm" / "Are you sure?") instead of the indexer-
        // specific text. Pin to the indexer name in the body so
        // a future copy edit only requires updating the substring.
        assert_modal_text(&client, "title", "Delete indexer?").await?;
        assert_modal_text(&client, "body", "Phase1Test-IndexerA").await?;
        // Confirm: click the "Yes" button in the modal.
        client
            .find(Locator::Id("ryokan-confirm-yes"))
            .await?
            .click()
            .await?;
        wait_for_row_removed(&client, "Phase1Test-IndexerA", Duration::from_secs(3)).await?;
        // Survivor must still be in the DOM — guards against an
        // over-broad swap target swallowing siblings.
        assert_dom_contains(&client, "Phase1Test-IndexerSurvivor").await?;
        // Discrimination: the row vanish above passes equally well
        // under the form-POST fallback (htmx not loaded → 303 →
        // navigate → reload → row gone). This check distinguishes
        // an htmx in-place swap from a fallback redirect.
        assert_htmx_handled_in_place(&client, &format!("http://{addr}/settings?tab=indexers"))
            .await?;
        Ok::<(), Box<dyn std::error::Error>>(())
    }
    .await;

    let _ = client.close().await;
    result.expect("indexers delete confirm");

    // DB-side sanity: only the doomed row is gone.
    let remaining = ryokan::models::indexers::list_all(&db)
        .await
        .expect("list indexers");
    assert_eq!(
        remaining.len(),
        1,
        "exactly one indexer (the survivor) must remain; got {remaining:?}"
    );
    assert_eq!(
        remaining[0].name, "Phase1Test-IndexerSurvivor",
        "wrong indexer survived"
    );
    let _ = remaining; // shadow legacy assert
    let remaining = ryokan::models::indexers::list_all(&db)
        .await
        .expect("list indexers");
    assert!(
        remaining.iter().all(|r| r.name != "Phase1Test-IndexerA"),
        "indexer row should be deleted; found {remaining:?}"
    );
}

#[tokio::test]
async fn indexers_delete_cancel_keeps_row() {
    // Regression guard for the "cancel modal silently submitted" bug.
    // Earlier shape: htmx's submit listener fired before base.js's,
    // so the AJAX was already in flight by the time `preventDefault()`
    // ran. Fixed by switching to the `htmx:confirm` event bridge.
    // If that bridge regresses, this test catches it: clicking Cancel
    // would still delete the row.
    let db = in_memory_pool().await;
    let _id = seed_indexer(&db, "Phase1Test-IndexerB").await;
    let token = seed_user_session(&db).await;
    let state = build_test_app_state(db.clone(), None);
    let addr = spawn_app(state).await;

    let client = match try_connect_browser().await {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("[skip] {msg}");
            return;
        }
    };

    let result = async {
        open_settings(&client, addr, &token, "indexers").await?;
        // Sanity: htmx must be loaded before we drive the cancel
        // path — without htmx, there's no modal to cancel and the
        // form would submit natively, deleting the row.
        assert_htmx_handled_in_place(&client, &format!("http://{addr}/settings?tab=indexers"))
            .await?;
        click_delete_for(&client, "Phase1Test-IndexerB").await?;
        wait_for_confirm_modal(&client, Duration::from_secs(2)).await?;
        // Cancel: click the "No" button in the modal.
        client
            .find(Locator::Id("ryokan-confirm-no"))
            .await?
            .click()
            .await?;
        // Give htmx a beat to misbehave if the bridge is broken.
        tokio::time::sleep(Duration::from_millis(300)).await;
        // Row must still be present in the DOM.
        let still_present: bool = client
            .execute(
                r#"
                return Array.from(document.querySelectorAll('tr'))
                    .some(tr => tr.textContent.includes('Phase1Test-IndexerB'));
                "#,
                vec![],
            )
            .await?
            .as_bool()
            .unwrap_or(false);
        if !still_present {
            return Err("cancel must leave the row in the DOM; modal-bridge regression?".into());
        }
        Ok::<(), Box<dyn std::error::Error>>(())
    }
    .await;

    let _ = client.close().await;
    result.expect("indexers delete cancel");

    // DB-side sanity: the row is still there.
    let remaining = ryokan::models::indexers::list_all(&db)
        .await
        .expect("list indexers");
    assert_eq!(remaining.len(), 1, "cancel must NOT delete the indexer row");
}

#[tokio::test]
async fn download_clients_delete_default_auto_promotes_next() {
    // Two clients: A (default) and B (not default). Delete A; expect
    // B to inherit the default badge after the page re-renders.
    let db = in_memory_pool().await;
    let id_a = seed_dl_client(&db, "Phase1Test-DcA", true).await;
    let _id_b = seed_dl_client(&db, "Phase1Test-DcB", false).await;
    let token = seed_user_session(&db).await;
    let state = build_test_app_state(db.clone(), None);
    let addr = spawn_app(state).await;

    let client = match try_connect_browser().await {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("[skip] {msg}");
            return;
        }
    };

    let result = async {
        open_settings(&client, addr, &token, "integrations").await?;
        click_delete_for(&client, "Phase1Test-DcA").await?;
        wait_for_confirm_modal(&client, Duration::from_secs(2)).await?;
        client
            .find(Locator::Id("ryokan-confirm-yes"))
            .await?
            .click()
            .await?;
        wait_for_row_removed(&client, "Phase1Test-DcA", Duration::from_secs(3)).await?;
        // Survivor row stays in the DOM (over-broad swap target catch).
        assert_dom_contains(&client, "Phase1Test-DcB").await?;
        assert_htmx_handled_in_place(&client, &format!("http://{addr}/settings?tab=integrations"))
            .await?;
        // Reload to re-render the default badges (the row swap alone
        // doesn't repaint sibling rows; auto-promotion is observable
        // in the DOM only after a fresh page load).
        let base = format!("http://{addr}");
        client
            .goto(&format!("{base}/settings?tab=integrations"))
            .await?;
        // DOM-side verification of the auto-promote (the DB-side
        // assertion below confirms B's `is_default = 1`, but a
        // template regression that doesn't re-render the badge would
        // pass the DB check and silently break the UI).
        let badge_on_survivor: bool = client
            .execute(
                r#"
                const tr = Array.from(document.querySelectorAll('tr'))
                    .find(r => r.textContent.includes('Phase1Test-DcB'));
                if (!tr) return false;
                return tr.textContent.toLowerCase().includes('default');
                "#,
                vec![],
            )
            .await?
            .as_bool()
            .unwrap_or(false);
        if !badge_on_survivor {
            return Err(
                "surviving DC row does not show the `default` badge after auto-promote — \
                 template render path didn't reflect the DB change?"
                    .into(),
            );
        }
        Ok::<(), Box<dyn std::error::Error>>(())
    }
    .await;

    let _ = client.close().await;
    result.expect("download clients delete-default + auto-promote");

    // DB-side: A is gone, B is default.
    let surviving = ryokan::models::download_clients::list_all(&db)
        .await
        .expect("list download clients");
    assert_eq!(surviving.len(), 1, "deleted A should leave only B");
    assert_eq!(surviving[0].name, "Phase1Test-DcB");
    assert!(
        surviving[0].is_default,
        "B should have been auto-promoted to default after A's delete; got {surviving:?}"
    );
    let _ = id_a; // silence unused
}

#[tokio::test]
async fn custom_formats_delete_last_triggers_hx_refresh() {
    // Deleting the only CF should send `HX-Refresh: true` so the
    // empty-state CTA ("Install bundled defaults") renders.
    // Otherwise per-row swap leaves an empty <tbody> with no CTA.
    let db = in_memory_pool().await;
    let _id = seed_cf(&db, "Phase1Test-CfSolo").await;
    let token = seed_user_session(&db).await;
    let state = build_test_app_state(db.clone(), None);
    let addr = spawn_app(state).await;

    let client = match try_connect_browser().await {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("[skip] {msg}");
            return;
        }
    };

    let result = async {
        open_settings(&client, addr, &token, "custom_formats").await?;
        // Sanity before the click: htmx must be present, otherwise
        // the form-POST fallback's 303 redirect lands on
        // /settings?tab=custom_formats&msg=... and the empty-state
        // appears for non-htmx reasons. Test passes for the wrong
        // reason. (URL discrimination doesn't work here because
        // HX-Refresh legitimately navigates.)
        assert_htmx_loaded(&client).await?;
        click_delete_for(&client, "Phase1Test-CfSolo").await?;
        wait_for_confirm_modal(&client, Duration::from_secs(2)).await?;
        client
            .find(Locator::Id("ryokan-confirm-yes"))
            .await?
            .click()
            .await?;
        // After HX-Refresh the URL stays put but the page re-renders
        // top-to-bottom — wait for the row to be gone AND for the
        // empty-state CTA text to appear.
        wait_for_row_removed(&client, "Phase1Test-CfSolo", Duration::from_secs(5)).await?;
        // Look for the empty-state CTA. The visible button reads
        // "Install Bundled Defaults" (title-case); use a case-
        // insensitive substring match so we don't pin to the exact
        // capitalization in the template.
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            let cta_present: bool = client
                .execute(
                    r#"
                    return document.body.textContent.toLowerCase()
                        .includes('install bundled defaults');
                    "#,
                    vec![],
                )
                .await?
                .as_bool()
                .unwrap_or(false);
            if cta_present {
                break;
            }
            if std::time::Instant::now() > deadline {
                return Err("empty-state CTA did not appear after HX-Refresh".into());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Ok::<(), Box<dyn std::error::Error>>(())
    }
    .await;

    let _ = client.close().await;
    result.expect("custom formats delete last + HX-Refresh");
}

#[tokio::test]
async fn groups_delete_removes_row_directly() {
    // Groups deliberately have NO confirm-modal wiring on the row's
    // delete form (the row is cheap to recreate; modal would be
    // friction). Click the button → htmx fires the POST directly →
    // row swap removes the row. If a future change adds a modal, this
    // test will need a `wait_for_confirm_modal` + Yes click.
    //
    // Two groups seeded so the test can assert the survivor stays
    // (over-broad swap target catch).
    let db = in_memory_pool().await;
    seed_group(&db, "Phase1Test-GroupDoomed").await;
    seed_group(&db, "Phase1Test-GroupSurvivor").await;
    let token = seed_user_session(&db).await;
    let state = build_test_app_state(db.clone(), None);
    let addr = spawn_app(state).await;

    let client = match try_connect_browser().await {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("[skip] {msg}");
            return;
        }
    };

    let result = async {
        open_settings(&client, addr, &token, "groups").await?;
        click_delete_for(&client, "Phase1Test-GroupDoomed").await?;
        wait_for_row_removed(&client, "Phase1Test-GroupDoomed", Duration::from_secs(3)).await?;
        assert_dom_contains(&client, "Phase1Test-GroupSurvivor").await?;
        assert_htmx_handled_in_place(&client, &format!("http://{addr}/settings?tab=groups"))
            .await?;
        Ok::<(), Box<dyn std::error::Error>>(())
    }
    .await;

    let _ = client.close().await;
    result.expect("groups delete");

    // DB-side: doomed gone, survivor remains. `get(name)` returns
    // Option, so we query each by name rather than diffing list_all
    // (the seed table contributes a lot of noise).
    let doomed = ryokan::models::group_source_map::get(&db, "Phase1Test-GroupDoomed")
        .await
        .expect("query doomed group");
    assert!(doomed.is_none(), "doomed group must be removed from DB");
    let survivor = ryokan::models::group_source_map::get(&db, "Phase1Test-GroupSurvivor")
        .await
        .expect("query survivor group");
    assert!(survivor.is_some(), "survivor group must still be in DB");
}
