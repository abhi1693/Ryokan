//! Browser-driving e2e tests for the HTMX migration (issue #129).
//!
//! These tests spin up a real Ryokan-like axum server on a random port,
//! point a real browser at it via WebDriver, and assert the post-click
//! DOM state. Unlike the inline handler tests in `htmx_settings_delete.rs`
//! / `crud.rs::tests`, this proves the *full* loop end-to-end:
//!
//!   • the static htmx script loads from `/static/vendor/`
//!   • `hx-vals` form-encoding round-trips to the `Form` extractor
//!   • the handler's HTML response actually swaps into the DOM at
//!     `hx-target` with `hx-swap=outerHTML`
//!
//! Pure handler unit tests can verify each piece in isolation, but a
//! regression in any of htmx-script-loaded / form-encoding-correct /
//! response-shape-matches only surfaces when the three meet in a
//! browser. Hence this scaffold.
//!
//! ## Running locally
//!
//! 1. Install geckodriver + a Firefox-family browser (LibreWolf works).
//!    Arch: `sudo pacman -S geckodriver`. Confirm with `geckodriver --version`.
//! 2. Start geckodriver on a port the harness will dial:
//!    `geckodriver --port=4444 &`
//!    (Override with `RYOKAN_WEBDRIVER_URL=http://...` if you run it
//!    elsewhere or want chromedriver instead.)
//! 3. Run the suite:
//!    `cargo test --features browser-e2e --test htmx_browser_e2e`
//!
//! Tests are NOT enabled in CI: they require an out-of-band driver +
//! browser binary, and the goal is local-iteration speed during the
//! HTMX migration, not gating CI on browser availability. The whole
//! `browser-e2e` feature (Cargo.toml flag, fantoccini dev-dep, this
//! file, and the `e2e_browser_app` helper in `test_support.rs`) is
//! planned to be removed once issue #129 is closed — search for
//! `browser-e2e` to find every removal site.
//!
//! ## When the test gracefully skips
//!
//! Connecting to `RYOKAN_WEBDRIVER_URL` (default `http://localhost:4444`)
//! is treated as an environmental precondition. If the connect fails —
//! geckodriver isn't running, or the browser isn't installed — each
//! test prints a one-line note and returns OK rather than failing the
//! suite. That keeps `cargo test --features browser-e2e` runnable on a
//! laptop without geckodriver pre-started, which matches how CI-gated
//! `live_smoke` tests (`RYOKAN_QBIT_E2E` etc.) opt into a real daemon.

use std::net::SocketAddr;
use std::time::Duration;

use fantoccini::ClientBuilder;
use fantoccini::Locator;
use ryokan::test_support::{
    build_test_app_state, e2e_browser_app, in_memory_pool, logged_in_session_for,
};

/// Spawn the e2e browser app on a random local port; return the bound
/// address and a JoinHandle that owns the axum task. The handle is
/// dropped at end of test, which aborts the task — no graceful
/// shutdown plumbing because tests should not depend on it.
async fn spawn_app(state: ryokan::AppState) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind random port");
    let addr = listener.local_addr().expect("local_addr");
    let app = e2e_browser_app(state);
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    // Tiny settle so the listener is in `accept()` before the browser
    // dials it. axum::serve returns from bind synchronously, but a
    // first connect-immediately-after-bind on some kernels eats the SYN.
    tokio::time::sleep(Duration::from_millis(50)).await;
    addr
}

/// Connect to the WebDriver endpoint, returning Ok(client) on success
/// or Err(message) when the connect fails for any reason. Tests use
/// the Err path to print a graceful skip message and return.
///
/// Browser binary resolution: geckodriver defaults to looking for
/// `firefox` in PATH, which fails on a LibreWolf-only system (and on
/// CI-like images that have a non-default install path). The harness
/// honors `RYOKAN_BROWSER_BIN` as an explicit override; absent that,
/// it auto-detects `firefox` / `firefox-esr` / `librewolf` in PATH
/// and passes the first hit through `moz:firefoxOptions.binary`.
///
/// **LibreWolf shim**: geckodriver runs `<binary> --version` and only
/// accepts output that starts with `Mozilla Firefox`. LibreWolf prints
/// `Mozilla LibreWolf X.Y`, which trips a "binary is not a Firefox
/// executable" hard-fail before any test code runs. When the resolved
/// binary is LibreWolf, the harness writes a tiny wrapper script to a
/// tempfile that fakes the `--version` line and execs LibreWolf for
/// every other invocation, then hands geckodriver the wrapper path.
/// Survives until the next `tmp` cleanup; not worth a Drop guard.
async fn try_connect_browser() -> Result<fantoccini::Client, String> {
    let url = std::env::var("RYOKAN_WEBDRIVER_URL")
        .unwrap_or_else(|_| "http://localhost:4444".to_string());

    let mut caps = serde_json::Map::new();
    // Headless by default — devs running locally can flip with
    // RYOKAN_BROWSER_HEADLESS=0 if they want to watch the test drive
    // the browser visually for debugging. CI-shape (no driver running)
    // never reaches this code path.
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
        // Explicit override is taken at face value — caller knows what
        // they're doing. No version-shim wrapping.
        return Some(explicit);
    }
    // Real Firefox is the happy path; check it before LibreWolf so a
    // dual-install system doesn't pay the shim cost.
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

/// Write a wrapper script to a tempfile that intercepts `--version`
/// (returning a Firefox-shaped string geckodriver accepts) and execs
/// LibreWolf for every other call. Returns the wrapper path.
///
/// We can't just symlink — geckodriver follows the link and runs the
/// real binary's `--version`. The wrapper has to stand between
/// geckodriver and LibreWolf for that one call.
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
    // POSIX-safe single-quote wrap; embedded single-quotes are escaped
    // by the standard `'\''` close-escape-open dance. Good enough for
    // any path on a normal Linux filesystem.
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

#[tokio::test]
async fn episode_monitor_button_swaps_in_browser() {
    // Setup: a series + one monitored episode in the DB. The fixture
    // page renders the partial directly (no series-detail page render
    // needed) so the test isolates the htmx swap from page concerns.
    let db = in_memory_pool().await;
    let series_id = ryokan::test_support::seed_series(&db, 12345, "BrowserTest Anime").await;
    ryokan::models::monitoring::set_episode_monitored(&db, series_id, 1, true)
        .await
        .expect("seed monitored episode");
    let state = build_test_app_state(db.clone(), None);

    // Authenticated browsing session: write a row to `sessions` and
    // then preload the cookie via WebDriver before navigating, so the
    // first page load already passes `require_auth`.
    let (_state2, cookie_value) = logged_in_session_for(&db).await;
    // cookie_value is "session=<hex>"; split off the value for the
    // WebDriver Cookie payload (it expects name + value separately).
    let token = cookie_value
        .strip_prefix("session=")
        .expect("cookie helper returns session=<hex>")
        .to_string();

    let addr = spawn_app(state).await;

    let client = match try_connect_browser().await {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("[skip] {msg}");
            return;
        }
    };

    // Drive the test, but always close the browser at the end — even
    // on assertion panics — so a stuck session doesn't leave a hung
    // browser process around.
    let result = run_episode_monitor_test(&client, addr, series_id, &token).await;
    let _ = client.close().await;
    result.expect("episode monitor browser test");

    // Side-effect verification: the handler renders the partial from
    // the request's `monitored` value, so a no-op handler that skips
    // the DB write would still return a "monitored=false" button and
    // the browser test above would pass. Read the row back to
    // confirm the flip actually persisted.
    let states = ryokan::models::monitoring::get_series_states(&db, series_id)
        .await
        .expect("read episode states");
    let ep1 = states.iter().find(|r| r.episode_number == 1);
    assert_eq!(
        ep1.map(|r| r.monitored),
        Some(false),
        "DB row must reflect the post-click state; got {ep1:?}"
    );
}

async fn run_episode_monitor_test(
    client: &fantoccini::Client,
    addr: SocketAddr,
    series_id: i64,
    session_token: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    // WebDriver requires a page in the target domain to be loaded
    // before `add_cookie` will accept a cookie scoped to it. Land on
    // the /login page first (unauthenticated route, always serves)
    // and then drop the session cookie in.
    let base = format!("http://{addr}");
    client.goto(&format!("{base}/login")).await?;
    // Production sets SameSite=Lax. fantoccini's default cookie comes
    // with SameSite=None (no Secure), which modern browsers reject as
    // an unsafe combination. fantoccini re-exports `cookie::Cookie`
    // but NOT the `SameSite` enum, so the path of least resistance is
    // to parse a Set-Cookie-shaped string into the cookie — that route
    // doesn't need access to the enum at all.
    let raw = format!("session={session_token}; Path=/; SameSite=Lax");
    let cookie = fantoccini::cookies::Cookie::parse(raw).expect("parse cookie");
    client.add_cookie(cookie).await?;

    // Now load the test fixture page that renders the partial.
    client
        .goto(&format!(
            "{base}/__test/episode-monitor-fixture?series_id={series_id}&episode_number=1"
        ))
        .await?;

    // Pre-click assertion: button reads "Yes" because we seeded
    // `monitored = true`.
    let button = client.find(Locator::Css("button.ep-mon-btn")).await?;
    let pre_text = button.text().await?;
    assert_eq!(
        pre_text.trim(),
        "Yes",
        "fixture should render the seeded monitored=true state"
    );
    let pre_class = button.attr("class").await?.unwrap_or_default();
    assert!(
        pre_class.contains("ep-mon-yes"),
        "pre-click button should carry ep-mon-yes class; got `{pre_class}`"
    );

    // Click. htmx fires the POST, the handler returns a fresh button
    // partial with `monitored = false`, and `hx-swap=outerHTML`
    // replaces the element. WebDriver hands back stale-element after
    // a swap — re-query rather than reusing the old handle.
    button.click().await?;

    // Wait for the swap to land. htmx ajax + render is sub-100ms
    // locally; poll up to ~3s for CI/slow-laptop headroom.
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        let btn = client.find(Locator::Css("button.ep-mon-btn")).await?;
        let text = btn.text().await?.trim().to_string();
        let class = btn.attr("class").await?.unwrap_or_default();
        if text == "No" && class.contains("ep-mon-no") {
            break;
        }
        if std::time::Instant::now() > deadline {
            panic!(
                "button did not swap to monitored=false within 3s; last text={text:?}, class={class:?}"
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    Ok(())
}
