//! HTMX foundation tests (issue #129 — HTMX migration, Phase 0).
//!
//! Smoke-tests the foundation:
//!
//!   1. Vendored htmx core + SSE extension exist on disk at the
//!      expected paths with the expected versions. If anyone renames
//!      / moves / accidentally deletes a vendored asset, this fails
//!      before a user hits a missing-script 404 in production.
//!   2. `templates/base.html` references both script tags so any
//!      page extending it picks up htmx automatically.
//!   3. `<body hx-boost="true">` is present so all same-origin nav
//!      links become fragment swaps without per-link wiring.
//!   4. The logout link explicitly opts out of boost via
//!      `hx-boost="false"` so a full reload clears state.
//!
//! Phase 1+ tests (per-handler `HxRequest` branching, fragment-vs-page
//! response shape) live alongside the handlers they cover; this file
//! only proves the foundation is wired correctly.
//!
//! Asserts against on-disk artifacts directly rather than going through
//! the test router (`handler_router` is a minimal test surface that
//! doesn't mount `/static` or page renderers, so a router-based test
//! would fail for unrelated reasons). The on-disk approach is also
//! strictly more robust — it catches a missing file regardless of
//! what the router happens to mount.

const HTMX_CORE_PATH: &str = "static/vendor/htmx-2.0.9.min.js";
const HTMX_SSE_PATH: &str = "static/vendor/htmx-ext-sse-2.2.4.min.js";

/// Vendored htmx core exists and looks like htmx (not, e.g., a 404
/// page captured from a bad fetch). `var htmx=` is htmx 2.x's minified
/// output prefix; if upstream changes the prefix in a future minor we
/// update the assertion here too.
#[test]
fn htmx_core_vendored_with_expected_shape() {
    let body = std::fs::read_to_string(HTMX_CORE_PATH)
        .unwrap_or_else(|e| panic!("vendored htmx core missing at {HTMX_CORE_PATH}: {e}"));
    assert!(
        body.starts_with("var htmx="),
        "asset at {HTMX_CORE_PATH} doesn't look like htmx (first 32 chars: {:?})",
        &body[..body.len().min(32)]
    );
    // Sanity check size is in the 40-80KB range we expect for 2.0.x
    // minified. Catches an accidental download of the unminified
    // (~170KB) variant or a partial-fetch truncation.
    let len = body.len();
    assert!(
        (40_000..=80_000).contains(&len),
        "vendored htmx core size {len} bytes is outside the expected 40-80KB minified range — \
         possibly the unminified (~170KB) variant or a truncated fetch"
    );
}

/// Vendored SSE extension exists. Smaller file — the size sanity
/// range reflects the minified `sse.min.js` size for htmx-ext-sse 2.x.
#[test]
fn htmx_sse_vendored_with_expected_shape() {
    let body = std::fs::read_to_string(HTMX_SSE_PATH)
        .unwrap_or_else(|e| panic!("vendored htmx SSE extension missing at {HTMX_SSE_PATH}: {e}"));
    // The SSE extension's minified output starts with the IIFE wrapper
    // calling htmx.defineExtension. Check for the substring rather than
    // a strict prefix to tolerate minifier variation.
    assert!(
        body.contains(r#"htmx.defineExtension("sse""#),
        "asset at {HTMX_SSE_PATH} doesn't register an 'sse' extension"
    );
    let len = body.len();
    assert!(
        (1_500..=10_000).contains(&len),
        "vendored htmx SSE extension size {len} bytes is outside the expected 1.5-10KB \
         minified range — possibly the unminified variant or a truncated fetch"
    );
}

/// `templates/base.html` declares the htmx + SSE script tags + the
/// `hx-boost="true"` body attribute + the logout opt-out. One test
/// asserts all four because they're load-bearing together: scripts
/// without boost don't enable nav swaps; boost without the logout
/// opt-out leaves logged-out state lingering across the boundary.
#[test]
fn base_template_wires_htmx_correctly() {
    let body =
        std::fs::read_to_string("templates/base.html").expect("templates/base.html must exist");

    // 1. htmx core script tag present (with version-suffixed filename
    //    so a major bump shows up as a touched line in the diff)
    assert!(
        body.contains(&format!("/{HTMX_CORE_PATH}")),
        "base.html must include the vendored htmx core script tag (/{HTMX_CORE_PATH})"
    );

    // 2. htmx SSE extension script tag present
    assert!(
        body.contains(&format!("/{HTMX_SSE_PATH}")),
        "base.html must include the vendored htmx SSE extension script tag (/{HTMX_SSE_PATH})"
    );

    // 3. body opens with hx-boost="true" — the cascade that makes
    //    same-origin nav links swap fragments instead of full-reloading
    assert!(
        body.contains(r#"<body hx-boost="true">"#),
        "base.html body must declare hx-boost=\"true\" for nav swap behavior"
    );

    // 4. Logout link explicitly opts out of boost so a full reload
    //    clears auth + session state cleanly. Without this opt-out,
    //    the boosted nav would AJAX through the logout endpoint and
    //    leave the prior page's JS state mounted on top of the
    //    swapped-in login content.
    assert!(
        body.contains(r#"hx-boost="false""#),
        "logout link must opt out of boost via hx-boost=\"false\""
    );

    // 5. Script tag order matters: htmx must load before base.js so
    //    custom JS can reference the `htmx.*` global. Verify by
    //    finding both lines and asserting the htmx one comes first.
    let htmx_pos = body
        .find("htmx-2.0.9.min.js")
        .expect("htmx core tag present");
    let base_js_pos = body
        .find("/static/js/base.js")
        .expect("base.js tag present");
    assert!(
        htmx_pos < base_js_pos,
        "htmx script tag must appear before base.js so the htmx global is available \
         to any custom JS"
    );
}
