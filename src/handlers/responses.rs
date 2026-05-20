//! Shared response helpers — Phase C of the hx-boost rollout plan.
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
        match url.parse() {
            Ok(value) => {
                let mut resp = Response::new(axum::body::Body::empty());
                *resp.status_mut() = StatusCode::OK;
                resp.headers_mut().insert("HX-Redirect", value);
                resp
            }
            Err(e) => {
                // `HeaderValue::parse` rejected the URL — almost
                // always an ASCII control character (newline, CR,
                // null) the caller forgot to escape (non-ASCII /
                // 0x80–0xFF bytes do parse fine, those are fine
                // for opaque Latin-1 header values).
                //
                // Falling through to `Redirect::to(url)` is NOT
                // safe — axum's `Redirect::to` does
                // `Uri::try_from(url).unwrap()` internally and
                // panics on the same malformed input. We surface a
                // 500 instead so the user notices something went
                // wrong (a silent header-less 200 would leave the
                // boosted click stuck on the current page with no
                // feedback) and the URL gets logged so the caller
                // bug is greppable.
                tracing::warn!(
                    url = %url,
                    error = %e,
                    "htmx_aware_redirect: HX-Redirect HeaderValue parse failed. \
                     Caller built a malformed URL — likely an unescaped control \
                     character in a flash-message value. Returning 500 so the \
                     bug surfaces to the user instead of silently no-op-ing."
                );
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            }
        }
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

    /// Edge case: a URL containing an ASCII control character
    /// (newline, CR, null) — `HeaderValue::parse` rejects those.
    /// Non-ASCII bytes (kanji, etc.) actually parse fine since
    /// `HeaderValue` allows the 0x80-0xFF range as opaque Latin-1.
    /// The case that DOES trip the parser is a CRLF injection or
    /// stray newline — should never happen via legitimate callers
    /// since `urlencoding::encode` escapes them, but we don't want
    /// the boosted click to silently no-op if one slips through.
    /// We can't fall through to `Redirect::to(url)` because axum's
    /// `Redirect::to` ALSO panics on the same malformed input
    /// (uses `Uri::try_from(url).unwrap()` internally), so the
    /// helper returns 500 to surface the caller bug visibly.
    #[test]
    fn htmx_aware_redirect_returns_500_when_url_rejected_by_header_value() {
        // ASCII LF (0x0A) is rejected by HeaderValue — it's the
        // CRLF-injection guard in hyper/http. Real-world trigger is
        // a caller that built a redirect URL by string-concatenating
        // an unescaped log message.
        let url = "/settings?tab=indexers&err=line1\nline2";
        let resp = htmx_aware_redirect(true, url);
        assert_eq!(
            resp.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "rejected URL must surface as 500 (visible to user, logged on server) \
             — both `HX-Redirect` AND `Redirect::to` reject the same input, so a \
             header-less 200 silent no-op was the only quieter alternative"
        );
        assert!(
            resp.headers().get("HX-Redirect").is_none(),
            "fallback path can't set HX-Redirect (the parse just failed)"
        );
    }

    /// Sanity check: a URL with non-ASCII bytes (legitimately raw
    /// UTF-8 from e.g. a release title) does NOT trip the fallback.
    /// `HeaderValue::parse` accepts the 0x80-0xFF range; the boost
    /// path keeps emitting `HX-Redirect` cleanly. The test is here
    /// to document the surface — a future "fail closed on
    /// non-ASCII" tightening would need a paired test edit.
    #[test]
    fn htmx_aware_redirect_handles_non_ascii_url_via_hx_redirect_path() {
        let url = "/settings?tab=indexers&err=漢字";
        let resp = htmx_aware_redirect(true, url);
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(resp.headers().get("HX-Redirect").is_some());
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
