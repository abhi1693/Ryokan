//! hx-boost rollout — Phase D browser-e2e coverage.
//!
//! Phase D scope per /home/john/Documents/ryokan-roadmap/hx_boost_rollout_plan.md:
//!   - `<body hx-boost="true">` covers every plain `<a>` and `<form>`
//!     site-wide
//!   - `<a href="/logout" hx-boost="false">` opt-out
//!   - `htmx.config.historyEnableCache = false` so back/forward refetch
//!     dynamic pages instead of restoring stale snapshots
//!
//! Tests pin the body-wide invariants Phase A's narrow opt-in didn't
//! exercise: pentagon nav, back/forward navigation, the logout opt-out,
//! and session-expiry middleware redirects (the `htmx_aware_redirect_from_req`
//! path in `require_auth`).
//!
//! Skips gracefully when geckodriver is unreachable.

use fantoccini::Locator;
use ryokan::test_support::{build_test_app_state, in_memory_pool};
use std::time::Duration;

#[path = "common/browser_e2e.rs"]
mod browser_e2e;
use browser_e2e::{
    assert_htmx_loaded, open_with_session, seed_user_session, spawn_app, try_connect_browser,
};

async fn set_desktop_viewport(client: &fantoccini::Client) -> Result<(), String> {
    // Desktop width so the top-nav `.nav-links` is visible. The
    // mobile-tabbar (display:none above 640px) doesn't matter for
    // pentagon-nav coverage; the desktop nav already links to all
    // five pages and is visible at 1280×900.
    client
        .set_window_rect(0, 0, 1280, 900)
        .await
        .map_err(|e| format!("set_window_rect: {e}"))
}

async fn wait_for_path(
    client: &fantoccini::Client,
    expected_path: &str,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let current = client
            .current_url()
            .await
            .map_err(|e| format!("current_url: {e}"))?;
        if current.path() == expected_path {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "timed out waiting for path={expected_path:?} (current path: {:?})",
                current.path()
            ));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn wait_for_js_truthy(
    client: &fantoccini::Client,
    expr: &str,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let result = client
            .execute(&format!("return !!({expr});"), vec![])
            .await
            .map_err(|e| format!("execute {expr:?}: {e}"))?;
        if result.as_bool().unwrap_or(false) {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "timed out waiting for JS expr to be truthy: {expr:?}"
            ));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Click a top-nav link and wait for the URL to update. Top nav uses
/// the desktop `.nav-links` slot, all five pages reachable.
async fn click_top_nav(
    client: &fantoccini::Client,
    href: &str,
    expected_path: &str,
) -> Result<(), String> {
    let sel = format!(".nav-links a[href=\"{href}\"]");
    let link = client
        .wait()
        .at_most(Duration::from_secs(5))
        .for_element(Locator::Css(&sel))
        .await
        .map_err(|e| format!("nav link {href}: {e}"))?;
    link.click()
        .await
        .map_err(|e| format!("click {href}: {e}"))?;
    wait_for_path(client, expected_path, Duration::from_secs(5)).await
}

/// **pentagon-nav** — full coverage that boost works between every
/// pair of top-level pages. With `<body hx-boost="true">`, navigating
/// from any of the five top-level pages to any other should swap
/// body + head and render the destination's CSS correctly.
///
/// We don't run all 5×4 = 20 transitions (slow); instead pick a
/// representative chain that hits each page at least once both as
/// origin and destination. Library → Settings → Search → System →
/// Downloads → Library is 5 boosted hops touching all 5 routes in
/// both directions.
#[tokio::test]
async fn boost_navigates_pentagon_via_body_level_opt_in() {
    let Ok(client) = try_connect_browser().await else {
        return;
    };

    let db = in_memory_pool().await;
    let state = build_test_app_state(db.clone(), None);
    let session = seed_user_session(&db).await;
    let addr = spawn_app(state).await;

    set_desktop_viewport(&client)
        .await
        .expect("desktop viewport");
    open_with_session(&client, addr, &session, "/")
        .await
        .expect("open library");
    let _ = assert_htmx_loaded(&client).await;

    // Plant a window marker — boosted nav preserves window scope, so
    // if we make 5 boosted hops and the marker survives, we know
    // every hop went through htmx (a real document load would have
    // wiped it).
    client
        .execute("window.__phaseDMarker = 'boosted';", vec![])
        .await
        .expect("plant marker");

    // Library → Settings
    click_top_nav(&client, "/settings", "/settings")
        .await
        .expect("nav to settings");
    // Settings → Search
    click_top_nav(&client, "/search", "/search")
        .await
        .expect("nav to search");
    // Search → System
    click_top_nav(&client, "/system", "/system")
        .await
        .expect("nav to system");
    // System → Downloads
    click_top_nav(&client, "/downloads", "/downloads")
        .await
        .expect("nav to downloads");
    // Downloads → Library
    click_top_nav(&client, "/", "/")
        .await
        .expect("nav back to library");

    // The window marker survives only across boosted swaps. A real
    // doc load would have wiped it.
    let marker = client
        .execute("return window.__phaseDMarker || null;", vec![])
        .await
        .expect("read marker");
    let marker_str = marker.as_str().unwrap_or("");
    assert_eq!(
        marker_str, "boosted",
        "window marker should survive 5 boosted hops; got {marker_str:?} — \
         one of the top-nav clicks did a real document load (boost off?)"
    );
}

/// **logout-opt-out** — `<a href="/logout" hx-boost="false">` must do
/// a real document navigation, not a boosted swap. The logout flow
/// hits a 303 to /login, the auth middleware redirects, the session
/// cookie clears via `Set-Cookie: Max-Age=0` — none of that is
/// boost-friendly. Verify by planting a window marker pre-click; a
/// real nav wipes it.
#[tokio::test]
async fn logout_link_opt_out_does_real_document_nav() {
    let Ok(client) = try_connect_browser().await else {
        return;
    };

    let db = in_memory_pool().await;
    let state = build_test_app_state(db.clone(), None);
    let session = seed_user_session(&db).await;
    let addr = spawn_app(state).await;

    set_desktop_viewport(&client)
        .await
        .expect("desktop viewport");
    open_with_session(&client, addr, &session, "/")
        .await
        .expect("open library");
    let _ = assert_htmx_loaded(&client).await;

    client
        .execute("window.__logoutMarker = 'present';", vec![])
        .await
        .expect("plant marker");

    let logout_link = client
        .wait()
        .at_most(Duration::from_secs(5))
        .for_element(Locator::Css("a[href=\"/logout\"]"))
        .await
        .expect("logout link");
    logout_link.click().await.expect("click logout");

    // Logout → 303 → /login. Wait for the URL to settle.
    wait_for_path(&client, "/login", Duration::from_secs(5))
        .await
        .expect("logout redirected to /login");

    // The marker should be GONE — `hx-boost="false"` made the click
    // a real document nav, which destroys the prior window scope.
    let marker = client
        .execute("return typeof window.__logoutMarker;", vec![])
        .await
        .expect("read marker");
    let typeof_str = marker.as_str().unwrap_or("");
    assert_eq!(
        typeof_str, "undefined",
        "after clicking logout, the prior window's marker should be gone \
         (real document nav). Got typeof={typeof_str:?} — boost may have \
         intercepted the click despite the hx-boost=\"false\" opt-out"
    );
}

/// **session-expiry** — the `require_auth` middleware redirect to
/// `/login` must work cleanly under boost. Phase C wired
/// `htmx_aware_redirect_from_req` into `require_auth`; this test
/// pins the end-to-end behavior.
///
/// Setup: legitimate session, then nuke it from the DB mid-session.
/// Click a boosted top-nav link. Expected: htmx receives a 200 +
/// `HX-Redirect: /login` from the middleware, triggers a real
/// `window.location` navigation, and the login form renders. Without
/// the Phase C migration, the middleware's bare 303 would get
/// fetched-and-swapped by boost and the login HTML would inline-swap
/// into the prior page's body (nesting).
#[tokio::test]
async fn boosted_nav_to_protected_page_with_invalidated_session_lands_on_login() {
    let Ok(client) = try_connect_browser().await else {
        return;
    };

    let db = in_memory_pool().await;
    let state = build_test_app_state(db.clone(), None);
    let session_token = seed_user_session(&db).await;
    let addr = spawn_app(state).await;

    set_desktop_viewport(&client)
        .await
        .expect("desktop viewport");
    open_with_session(&client, addr, &session_token, "/")
        .await
        .expect("open library");
    let _ = assert_htmx_loaded(&client).await;

    // Wipe the session from the DB so the middleware's
    // `validate_session` returns Ok(None) → redirect-to-login.
    sqlx::query("DELETE FROM sessions WHERE token = ?")
        .bind(&session_token)
        .execute(&db)
        .await
        .expect("delete session");

    // Boost-click any top-nav link. Middleware refuses, returns
    // 200 + HX-Redirect: /login. htmx triggers real nav.
    let settings_link = client
        .wait()
        .at_most(Duration::from_secs(5))
        .for_element(Locator::Css(".nav-links a[href=\"/settings\"]"))
        .await
        .expect("settings link");
    settings_link.click().await.expect("click");

    wait_for_path(&client, "/login", Duration::from_secs(5))
        .await
        .expect("invalidated-session click should land on /login");

    // Confirm the login form is rendered (not nested inside a stale
    // page body).
    client
        .wait()
        .at_most(Duration::from_secs(5))
        .for_element(Locator::Css("form[action=\"/login\"]"))
        .await
        .expect("login form visible at /login");
}

/// **history-cache-disabled** — `htmx.config.historyEnableCache`
/// should be `false` after the inline config script in base.html
/// runs. This pins the back/forward refresh-on-nav behavior:
/// without it, browser-back on a stale Downloads queue restores a
/// snapshot from the prior visit instead of refetching.
///
/// We can't easily drive a back/forward in fantoccini and prove the
/// fetch happened (would need network instrumentation that
/// geckodriver doesn't expose cleanly). Instead pin the config flag
/// — if a future template edit drops the `historyEnableCache=false`
/// line, this test catches it.
#[tokio::test]
async fn history_cache_is_disabled() {
    let Ok(client) = try_connect_browser().await else {
        return;
    };

    let db = in_memory_pool().await;
    let state = build_test_app_state(db.clone(), None);
    let session = seed_user_session(&db).await;
    let addr = spawn_app(state).await;

    set_desktop_viewport(&client)
        .await
        .expect("desktop viewport");
    open_with_session(&client, addr, &session, "/")
        .await
        .expect("open library");

    // Wait for htmx to load AND for the config flag to take effect.
    // The `htmx:load` once-listener in base.html sets the flag on
    // the first htmx event; poll for it.
    wait_for_js_truthy(
        &client,
        "window.htmx && window.htmx.config && window.htmx.config.historyEnableCache === false",
        Duration::from_secs(5),
    )
    .await
    .expect(
        "htmx.config.historyEnableCache must be false (set by the inline \
         <script> in base.html). Got: htmx loaded but the flag is still \
         the htmx default (true) — Phase D's config block didn't fire.",
    );
}
