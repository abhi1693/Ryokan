//! HTMX foundation tests.
//!
//! Pins the load-bearing scaffolding the rest of the HTMX surface
//! depends on:
//!
//!   1. Vendored htmx core + extensions exist on disk at the expected
//!      paths with the expected versions. If anyone renames / moves /
//!      accidentally deletes a vendored asset, this fails before a
//!      user hits a missing-script 404 in production.
//!   2. `templates/base.html` references all three script tags in the
//!      correct order — htmx core must load before `base.js` so any
//!      custom JS that references the `htmx.*` global sees it on first
//!      paint.
//!   3. Body-wide `hx-boost="true"` + `hx-ext="head-support"` are set
//!      on `<body>`. Both are load-bearing per `templates/CLAUDE.md`:
//!      boost opts every plain `<a>` and `<form>` into htmx swap-based
//!      nav; head-support is the prerequisite that diff-merges `<head>`
//!      so per-page `{% block page_css %}` swaps cleanly between pages.
//!      Without head-support, boost would swap body content and leave
//!      the head's per-page CSS stale (pages render unstyled across nav).
//!   4. The `htmx-config` meta tag pins `historyEnableCache:false` so
//!      browser back/forward refetches dynamic pages instead of
//!      restoring stale snapshots (Downloads queue, System logs).
//!
//! Per-handler `HxRequest` branching, fragment-vs-page response shape,
//! and DOM-state assertions live alongside their handlers
//! (`tests/htmx_browser_e2e*.rs` for the browser layer).
//!
//! Asserts against on-disk artifacts directly rather than going through
//! the test router (`handler_router` is a minimal test surface that
//! doesn't mount `/static` or page renderers, so a router-based test
//! would fail for unrelated reasons). The on-disk approach is also
//! strictly more robust — it catches a missing file regardless of
//! what the router happens to mount.

const HTMX_CORE_PATH: &str = "static/vendor/htmx-2.0.9.min.js";
const HTMX_SSE_PATH: &str = "static/vendor/htmx-ext-sse-2.2.4.min.js";
const HTMX_HEAD_SUPPORT_PATH: &str = "static/vendor/htmx-ext-head-support-2.0.5.min.js";

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

/// Vendored head-support extension exists. The head-support extension
/// is the prerequisite that lets `hx-boost` swap pages with different
/// `<head>` blocks cleanly — without it, boost leaves per-page CSS stale.
#[test]
fn htmx_head_support_vendored_with_expected_shape() {
    let body = std::fs::read_to_string(HTMX_HEAD_SUPPORT_PATH).unwrap_or_else(|e| {
        panic!("vendored htmx head-support missing at {HTMX_HEAD_SUPPORT_PATH}: {e}")
    });
    assert!(
        body.contains(r#"htmx.defineExtension("head-support""#),
        "asset at {HTMX_HEAD_SUPPORT_PATH} doesn't register a 'head-support' extension"
    );
    let len = body.len();
    assert!(
        (1_000..=8_000).contains(&len),
        "vendored htmx head-support extension size {len} bytes is outside the expected 1-8KB \
         minified range — possibly the unminified variant or a truncated fetch"
    );
}

/// `templates/base.html` declares all three vendored script tags in
/// the correct order (htmx before `base.js` so the global is available
/// to any custom JS that runs on DOMContentLoaded).
#[test]
fn base_template_wires_htmx_correctly() {
    let body =
        std::fs::read_to_string("templates/base.html").expect("templates/base.html must exist");

    for path in [HTMX_CORE_PATH, HTMX_SSE_PATH, HTMX_HEAD_SUPPORT_PATH] {
        assert!(
            body.contains(&format!("/{path}")),
            "base.html must include the vendored script tag for /{path}"
        );
    }

    // Script tag order matters: htmx must load before base.js so
    // custom JS can reference the `htmx.*` global. Verify by finding
    // both positions and asserting the htmx one comes first.
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

/// Body declares `hx-boost="true"` + `hx-ext="head-support"`. Both are
/// load-bearing per `templates/CLAUDE.md`. Pinned so a stray edit (e.g.
/// removing one to "simplify" the body tag) fails the suite before
/// hitting a user — boost without head-support leaves per-page CSS
/// stale across nav; head-support without boost is just dead code.
#[test]
fn base_body_declares_boost_and_head_support() {
    let body =
        std::fs::read_to_string("templates/base.html").expect("templates/base.html must exist");

    // Find the `<body` open tag and check the attributes are inside it.
    let open = body.find("<body").expect("base.html has a <body> tag");
    let close = body[open..]
        .find('>')
        .map(|i| open + i)
        .expect("<body> open tag is closed");
    let body_tag = &body[open..=close];

    assert!(
        body_tag.contains(r#"hx-boost="true""#),
        "<body> must declare hx-boost=\"true\" (got: {body_tag:?})"
    );
    assert!(
        body_tag.contains(r#"hx-ext="head-support""#),
        "<body> must declare hx-ext=\"head-support\" — required for hx-boost to swap pages \
         with different <head> blocks (got: {body_tag:?})"
    );
}

/// `htmx-config` meta tag pins `historyEnableCache:false` so browser
/// back/forward refetches dynamic pages (Downloads queue, System logs)
/// instead of restoring stale snapshots. htmx 2.x reads this meta tag
/// during init.
#[test]
fn base_pins_history_cache_off_via_meta() {
    let body =
        std::fs::read_to_string("templates/base.html").expect("templates/base.html must exist");

    // Look for the meta tag with the htmx-config name and the
    // historyEnableCache:false setting. JSON formatting tolerates both
    // single and double quotes around the content attribute, so
    // substring-match the inner JSON.
    assert!(
        body.contains(r#"name="htmx-config""#),
        "base.html must declare a <meta name=\"htmx-config\"> tag for htmx init-time config"
    );
    assert!(
        body.contains(r#""historyEnableCache":false"#),
        "htmx-config meta must set historyEnableCache:false so back/forward refetches \
         dynamic pages instead of restoring stale snapshots"
    );
}
