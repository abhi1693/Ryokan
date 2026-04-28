//! Phase 1.5 grab-bag (issue #129) browser e2e tests.
//!
//! Two patterns under test:
//!
//! - **Row removal** (blocklist-remove): same shape as Phase 1
//!   settings deletes — confirm modal → click Yes → row stripped.
//!   Drives the real `/downloads?tab=blocklist` page so any drift
//!   between the row template and the production handler shows up.
//! - **Inline-result swap** (jellyfin/test, jellyfin/refresh,
//!   download-clients/test): button click → htmx posts the form →
//!   handler returns a small HTML fragment → swap into the result
//!   span. Drives a fixture page in `test_support::e2e` that mirrors
//!   the production button shape (see the fixture for the exact
//!   `hx-include` / `hx-target` attrs).
//!
//! Skips gracefully when the WebDriver endpoint is unreachable —
//! see `tests/htmx_browser_e2e.rs` for the run instructions and
//! the planned removal post-migration.

use std::net::SocketAddr;
use std::time::Duration;

use fantoccini::ClientBuilder;
use fantoccini::Locator;
use ryokan::test_support::{build_test_app_state, e2e_browser_app, in_memory_pool, seed_series};
use sqlx::SqlitePool;

// ─── Harness boilerplate (mirrors the other browser-e2e files) ─────

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

async fn open_with_session(
    client: &fantoccini::Client,
    addr: SocketAddr,
    session_token: &str,
    path: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let base = format!("http://{addr}");
    client.goto(&format!("{base}/login")).await?;
    let raw = format!("session={session_token}; Path=/; SameSite=Lax");
    let cookie = fantoccini::cookies::Cookie::parse(raw)?;
    client.add_cookie(cookie).await?;
    client.goto(&format!("{base}{path}")).await?;
    Ok(())
}

/// Same as `htmx_browser_e2e_phase1::assert_htmx_handled_in_place` —
/// duplicated here because each integration test target is its own
/// binary crate. Distinguishes an htmx in-place swap from the
/// form-POST fallback's 303 redirect (which would silently pass
/// every row-delete test if htmx failed to load).
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
             instead of htmx swapping in place"
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

async fn seed_user_session(db: &SqlitePool) -> String {
    let user_id = ryokan::models::user::create_user(db, "grab-bag-user", "hunter2-test-password")
        .await
        .expect("create user");
    ryokan::models::session::create_session(db, user_id)
        .await
        .expect("create session")
}

// ─── Tests ─────────────────────────────────────────────────────────

#[tokio::test]
async fn blocklist_remove_swaps_row_out_in_browser() {
    // Seed two blocklist rows. The test deletes the first and asserts:
    //   (a) the deleted row is gone from the DOM
    //   (b) the surviving row is STILL in the DOM
    //   (c) the surviving DB row is untouched
    // Without (b), a stray `hx-target="closest div"` (which would swap
    // the entire containing div) passes silently — caught during a
    // mutation-testing pass while writing the e2e suite.
    let db = in_memory_pool().await;
    let series_id = seed_series(&db, 9001, "BrowserBlock Anime").await;
    sqlx::query(
        "INSERT INTO grabbed_torrents (series_id, hash, torrent_name, episode_numbers, state) \
         VALUES (?, ?, ?, '[1]', 'failed'), (?, ?, ?, '[2]', 'failed')",
    )
    .bind(series_id)
    .bind("blockhash-1")
    .bind("BrowserBlock.Doomed.Release")
    .bind(series_id)
    .bind("blockhash-2")
    .bind("BrowserBlock.Survivor.Release")
    .execute(&db)
    .await
    .expect("seed blocklist rows");

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
        open_with_session(&client, addr, &token, "/downloads?tab=blocklist").await?;

        // Click the row's delete button. The form sits inside the
        // row; we identify it by the unique torrent name in the
        // surrounding tr.
        client
            .execute(
                r#"
                const marker = 'BrowserBlock.Doomed.Release';
                const tr = Array.from(document.querySelectorAll('tr'))
                    .find(r => r.textContent.includes(marker));
                if (!tr) throw new Error('blocklist row not found');
                const btn = tr.querySelector('button[type="submit"]');
                if (!btn) throw new Error('row has no delete button');
                btn.click();
                "#,
                vec![],
            )
            .await?;

        // Wait for the confirm modal, then click Yes.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            let visible: bool = client
                .execute(
                    r#"
                    const m = document.getElementById('ryokan-confirm-modal');
                    return !!m && getComputedStyle(m).display !== 'none';
                    "#,
                    vec![],
                )
                .await?
                .as_bool()
                .unwrap_or(false);
            if visible {
                break;
            }
            if std::time::Instant::now() > deadline {
                return Err("confirm modal did not appear".into());
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        client
            .find(Locator::Id("ryokan-confirm-yes"))
            .await?
            .click()
            .await?;

        // Wait for the deleted row to vanish.
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            let still_present: bool = client
                .execute(
                    r#"
                    return Array.from(document.querySelectorAll('tr'))
                        .some(r => r.textContent.includes('BrowserBlock.Doomed.Release'));
                    "#,
                    vec![],
                )
                .await?
                .as_bool()
                .unwrap_or(false);
            if !still_present {
                break;
            }
            if std::time::Instant::now() > deadline {
                return Err("blocklist row was not removed within 3s".into());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // Mutation guard: the surviving row's <tr> must still be in
        // the DOM. Without this assertion, a hx-target="closest div"
        // typo swaps the entire blocklist tab away — the deleted
        // marker disappears but so does the survivor. Test should
        // catch the over-broad swap.
        let survivor_present: bool = client
            .execute(
                r#"
                return Array.from(document.querySelectorAll('tr'))
                    .some(r => r.textContent.includes('BrowserBlock.Survivor.Release'));
                "#,
                vec![],
            )
            .await?
            .as_bool()
            .unwrap_or(false);
        if !survivor_present {
            return Err(
                "surviving blocklist row was also removed — over-broad swap target?".into(),
            );
        }
        assert_htmx_handled_in_place(&client, &format!("http://{addr}/downloads")).await?;
        Ok::<(), Box<dyn std::error::Error>>(())
    }
    .await;

    let _ = client.close().await;
    result.expect("blocklist remove e2e");

    // DB-side sanity.
    let remaining =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM grabbed_torrents WHERE hash = ?")
            .bind("blockhash-1")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(remaining, 0, "row must actually be gone from the DB");
}

#[tokio::test]
async fn jellyfin_test_renders_failure_message_in_browser() {
    // Drives the connection-test fixture: bogus URL → handler returns
    // the failure partial → htmx swaps into #jellyfin-test-result.
    // Asserts the post-swap text contains either "Connection" or
    // a connect-refused-style message — we don't pin the exact
    // wording (reqwest errors vary across platforms).
    let db = in_memory_pool().await;
    let token = seed_user_session(&db).await;
    let state = build_test_app_state(db, None);
    let addr = spawn_app(state).await;

    let client = match try_connect_browser().await {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("[skip] {msg}");
            return;
        }
    };

    let result = async {
        open_with_session(&client, addr, &token, "/__test/connection-test-fixture").await?;
        client
            .find(Locator::Id("btn-jellyfin-test"))
            .await?
            .click()
            .await?;
        // Wait for the swap. Connection failure to 127.0.0.1:1 is
        // fast on Linux (RST inside ~10ms); poll up to 5s for slow
        // setups. Match on "127.0.0.1" specifically (the URL the
        // fixture form sends) — `wait_until_text_present` would
        // accept any non-empty string and miss a "rendered the wrong
        // template" regression.
        wait_until_substring(
            &client,
            "#jellyfin-test-result",
            "127.0.0.1",
            Duration::from_secs(5),
        )
        .await
    }
    .await;

    let _ = client.close().await;
    result.expect("jellyfin test e2e");
}

#[tokio::test]
async fn jellyfin_refresh_renders_not_configured_in_browser() {
    let db = in_memory_pool().await;
    let token = seed_user_session(&db).await;
    let state = build_test_app_state(db, None);
    let addr = spawn_app(state).await;

    let client = match try_connect_browser().await {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("[skip] {msg}");
            return;
        }
    };

    let result = async {
        open_with_session(&client, addr, &token, "/__test/connection-test-fixture").await?;
        client
            .find(Locator::Id("btn-jellyfin-refresh"))
            .await?
            .click()
            .await?;
        wait_until_substring(
            &client,
            "#jellyfin-test-result",
            "not configured",
            Duration::from_secs(3),
        )
        .await
    }
    .await;

    let _ = client.close().await;
    result.expect("jellyfin refresh e2e");
}

#[tokio::test]
async fn download_clients_test_renders_url_required_in_browser() {
    // The DC form fixture has an empty URL field; the handler should
    // surface "URL required" — easy assertion that doesn't depend on
    // a real qBit-style daemon.
    let db = in_memory_pool().await;
    let token = seed_user_session(&db).await;
    let state = build_test_app_state(db, None);
    let addr = spawn_app(state).await;

    let client = match try_connect_browser().await {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("[skip] {msg}");
            return;
        }
    };

    let result = async {
        open_with_session(&client, addr, &token, "/__test/connection-test-fixture").await?;
        client
            .find(Locator::Id("btn-dc-test"))
            .await?
            .click()
            .await?;
        wait_until_substring(
            &client,
            ".dc-test-result",
            "URL required",
            Duration::from_secs(3),
        )
        .await
    }
    .await;

    let _ = client.close().await;
    result.expect("dc test e2e");
}

// ─── Polling helpers ───────────────────────────────────────────────
// `wait_until_text_present` (any-text variant) was removed during the
// false-positive audit — the loose check would pass even if the
// handler returned a blank or unrelated string. Use `wait_until_substring`
// instead so the assertion pins to specific content.

async fn wait_until_substring(
    client: &fantoccini::Client,
    selector: &str,
    substring: &str,
    timeout: Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let present: bool = client
            .execute(
                r#"
                const sel = arguments[0];
                const sub = arguments[1];
                const el = document.querySelector(sel);
                if (!el) return false;
                return (el.textContent || '').includes(sub);
                "#,
                vec![serde_json::json!(selector), serde_json::json!(substring)],
            )
            .await?
            .as_bool()
            .unwrap_or(false);
        if present {
            return Ok(());
        }
        if std::time::Instant::now() > deadline {
            return Err(
                format!("`{selector}` did not contain `{substring}` within {timeout:?}").into(),
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
