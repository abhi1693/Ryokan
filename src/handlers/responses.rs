//! Shared response helpers — Phase C of the hx-boost rollout
//! (per /home/john/Documents/ryokan-roadmap/hx_boost_rollout_plan.md).
//!
//! ## Why this module exists
//!
//! Once `hx-boost="true"` lands on `<body>` (Phase D), every plain
//! `<a>` click and every plain `<form>` submission goes through htmx.
//! htmx 2.x's default behavior on a 3xx response is to follow it via
//! `fetch`, then swap the destination's HTML into the form's
//! `hx-target` (or the boost-default body target). That produces a
//! nested-page render: the destination renders correctly but ends up
//! inside the form-target's parent, creating duplicate `<h2>` /
//! `<nav>` markers and a broken layout.
//!
//! The fix is to detect HTMX requests via the `HxRequest` extractor
//! and return `200 OK` with an `HX-Redirect: <url>` header. htmx
//! intercepts that header and does a real client-side `window.location`
//! navigation, which boost then handles correctly. Non-HTMX requests
//! get the standard 303 so the no-JS progressive-enhancement path
//! still works.
//!
//! Single canonical implementation here keeps the per-handler
//! boilerplate consistent. Existing per-tab helpers like
//! `handlers::settings::indexers::error_redirect` were the prototype;
//! this module collapses them into one shape.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};

/// HTMX-aware redirect for middleware that holds a [`Request`]
/// rather than the [`HxRequest`] extractor. Reads the `HX-Request`
/// header directly. Used by the `require_auth` middleware so a
/// session-expired boosted nav lands on `/login` cleanly instead
/// of nesting the login page inside the prior page's layout.
///
/// htmx sets `HX-Request: true` on every hx-* / hx-boost request;
/// any other value (or absent) falls through to the plain 303.
pub fn htmx_aware_redirect_from_req(
    req: &axum::http::Request<axum::body::Body>,
    url: &str,
) -> Response {
    let is_htmx = req
        .headers()
        .get("HX-Request")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    htmx_aware_redirect(is_htmx, url)
}

/// HTMX-aware redirect. Returns:
///   - HTMX request → `200 OK` with `HX-Redirect: <url>` header,
///     empty body. htmx triggers a real `window.location` navigation
///     so boosted callers don't try to inline-swap the destination
///     page's HTML into the requesting form's target.
///   - Plain request → `303 See Other` with `Location: <url>`. The
///     no-JS form-POST path still gets a normal redirect.
///
/// Use this in every form-POST handler that returns a redirect on
/// either success or error. Pass `is_htmx` from the `HxRequest`
/// extractor; pass the absolute path (e.g. `/settings?tab=indexers&err=...`)
/// as `url`. The url should already be percent-encoded if it contains
/// query-string values that came from user input.
pub fn htmx_aware_redirect(is_htmx: bool, url: &str) -> Response {
    if is_htmx {
        let mut resp = Response::new(axum::body::Body::empty());
        *resp.status_mut() = StatusCode::OK;
        if let Ok(value) = url.parse() {
            resp.headers_mut().insert("HX-Redirect", value);
        }
        resp
    } else {
        Redirect::to(url).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// HTMX path: empty body, 200 OK, `HX-Redirect` header carries
    /// the destination URL. The header value is what htmx reads to
    /// trigger the client-side navigation.
    #[test]
    fn htmx_aware_redirect_emits_hx_redirect_header_on_htmx_request() {
        let url = "/settings?tab=indexers&err=Save+failed";
        let resp = htmx_aware_redirect(true, url);
        assert_eq!(resp.status(), StatusCode::OK);
        let header = resp
            .headers()
            .get("HX-Redirect")
            .expect("HX-Redirect header present")
            .to_str()
            .expect("header is ASCII");
        assert_eq!(header, url);
        // Should NOT carry a Location header; htmx ignores 3xx
        // responses for hx-* requests.
        assert!(
            resp.headers().get("Location").is_none(),
            "HX-Redirect path must not also set Location — that would \
             prompt some htmx versions to follow the 3xx fetch-and-swap"
        );
    }

    /// Non-HTMX path: standard 303 redirect, `Location` header set.
    /// Keeps the form-POST progressive-enhancement path working.
    #[test]
    fn htmx_aware_redirect_emits_303_on_plain_request() {
        let url = "/settings?tab=indexers&msg=Saved";
        let resp = htmx_aware_redirect(false, url);
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        let header = resp
            .headers()
            .get("Location")
            .expect("Location header present")
            .to_str()
            .expect("ascii");
        assert_eq!(header, url);
        assert!(
            resp.headers().get("HX-Redirect").is_none(),
            "303 path must not set HX-Redirect — that's only for the \
             HXRequest=true branch"
        );
    }

    /// Edge case: a URL containing a non-ASCII byte (shouldn't
    /// happen — callers percent-encode upstream — but we don't
    /// want to panic if one slips through). The `parse()` call
    /// returns Err for non-ASCII; we fall back to a header-less
    /// 200 rather than panicking. htmx without an HX-Redirect
    /// stays on the current page; degraded but safe.
    #[test]
    fn htmx_aware_redirect_does_not_panic_on_non_ascii_url() {
        let url = "/settings?tab=indexers&err=漢字";
        let resp = htmx_aware_redirect(true, url);
        // Either 200 with no header, or 200 with the header set
        // (depending on what HeaderValue::parse does with this).
        // The test pins "doesn't panic" — both are acceptable.
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// `htmx_aware_redirect_from_req` reads the `HX-Request` header.
    /// `true` (case-insensitive) → HX-Redirect path. Anything else
    /// → 303. Pin both branches.
    #[test]
    fn htmx_aware_redirect_from_req_reads_hx_request_header() {
        let req = axum::http::Request::builder()
            .header("HX-Request", "true")
            .body(axum::body::Body::empty())
            .expect("build request");
        let resp = htmx_aware_redirect_from_req(&req, "/login");
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(resp.headers().get("HX-Redirect").is_some());
    }

    #[test]
    fn htmx_aware_redirect_from_req_falls_through_when_header_absent() {
        let req = axum::http::Request::builder()
            .body(axum::body::Body::empty())
            .expect("build request");
        let resp = htmx_aware_redirect_from_req(&req, "/login");
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        assert!(resp.headers().get("HX-Redirect").is_none());
    }

    #[test]
    fn htmx_aware_redirect_from_req_handles_case_insensitive_true() {
        let req = axum::http::Request::builder()
            .header("HX-Request", "TRUE")
            .body(axum::body::Body::empty())
            .expect("build request");
        let resp = htmx_aware_redirect_from_req(&req, "/login");
        assert_eq!(resp.status(), StatusCode::OK);
    }
}
