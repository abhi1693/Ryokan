//! `SabClient::test()` — version-probe / connectivity. SAB has no
//! login handshake; auth is `?apikey=…` on every call. This file
//! covers the version-probe shape, error surfacing for HTTP failures,
//! and the SAB-specific "API key invalid" error envelope (returned
//! as 200 with a JSON error body, not 401, which is the most-likely
//! footgun for a fresh integration).

use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, ResponseTemplate};

use super::fixture::new_fixture;
use crate::services::download_client::DownloadClient;

#[tokio::test]
async fn test_returns_version_string_on_success() {
    // `test()` is now a three-step probe — `mode=version` (public)
    // for the version string, `mode=queue` (auth-required) so a
    // missing/invalid API key surfaces at config time instead of
    // silently passing the public-endpoint check and then failing
    // every grab with HTTP 403, and `mode=get_cats` for the category
    // auto-create check. The `get_cats` mock here returns the fixture
    // category as already-present so `ensure_category` is a no-op
    // (no `set_config` mock needed) and `test()` returns just the
    // version string.
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "version"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "version": "4.3.2",
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "queue"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"queue": {"slots": []}})),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "get_cats"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "categories": ["*", "ryokan-test"],
        })))
        .mount(&server)
        .await;

    let result = client.test().await.expect("test should succeed");
    assert_eq!(
        result, "4.3.2",
        "test() returns the bare version string when the configured category already exists in SAB; the kind prefix is the UI's job (status pill prepends \"SABnzbd \" itself, the toast says \"Connected: <version>\"). Doubling the prefix here was the bug fix that motivated this regression test."
    );
}

#[tokio::test]
async fn test_returns_http_error_when_server_500s() {
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "version"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let err = client.test().await.expect_err("test should fail on 500");
    assert!(
        err.contains("HTTP 500"),
        "error must surface the HTTP status; got: {err}"
    );
}

#[tokio::test]
async fn test_returns_parse_error_when_body_is_not_version_json() {
    // SAB's `?mode=version` expected to return `{"version": "X.Y.Z"}`.
    // Anything else is a misconfigured proxy / wrong-port footgun;
    // the parse error should surface with a clear "version parse
    // failed" prefix so the user can distinguish "I'm pointing at
    // the wrong service" from "credentials are wrong."
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "version"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<html>not sab</html>"))
        .mount(&server)
        .await;

    let err = client.test().await.expect_err("non-JSON must fail");
    assert!(
        err.contains("parse failed"),
        "parse failure must be tagged `parse failed`; got: {err}"
    );
}

#[tokio::test]
async fn test_sends_apikey_query_param() {
    // Auth-by-query param is the load-bearing SAB convention. If a
    // refactor accidentally drops the `apikey=` from the test() call
    // path, SAB's response shape varies by version (some return 200
    // with `{"error":"API Key Required"}`, others 403); either way
    // the connectivity probe would silently break. Pin the query
    // shape here so a regression triggers the wiremock no-match
    // panic on this test specifically.
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "version"))
        .and(query_param("apikey", super::fixture::TEST_API_KEY))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "version": "4.3.2",
        })))
        .mount(&server)
        .await;
    // Auth probe needs the same key — pin every call to verify the
    // apikey query param threads through every step of `test()`.
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "queue"))
        .and(query_param("apikey", super::fixture::TEST_API_KEY))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"queue": {"slots": []}})),
        )
        .mount(&server)
        .await;
    // Category auto-create probe: returns the configured category
    // as already-present so ensure_category short-circuits without
    // needing a set_config mock.
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "get_cats"))
        .and(query_param("apikey", super::fixture::TEST_API_KEY))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "categories": ["*", "ryokan-test"],
        })))
        .mount(&server)
        .await;

    client
        .test()
        .await
        .expect("apikey query param must be sent on every request");
}

/// New regression test: when SAB's queue probe returns 403, the
/// error must call out the API key specifically so the user knows
/// what to fix. The prior `test()` shape only probed `mode=version`
/// (public endpoint) and silently passed even with a missing/wrong
/// key — the resulting `Grab failed for episode N: SAB add returned
/// HTTP 403 Forbidden` toast on first grab was the actionable
/// symptom. This test pins the diagnostic message at the SAB
/// trait-impl boundary.
#[tokio::test]
async fn test_returns_api_key_error_when_queue_probe_returns_403() {
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "version"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "version": "4.3.2",
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "queue"))
        .respond_with(ResponseTemplate::new(403).set_body_string("API Key Incorrect"))
        .mount(&server)
        .await;

    let err = client
        .test()
        .await
        .expect_err("queue probe 403 must fail the test");
    assert!(
        err.contains("API key"),
        "diagnostic must mention API key explicitly so the user knows which field to fix; got: {err}"
    );
    assert!(
        err.contains("API Key Incorrect"),
        "SAB's body text must be surfaced so the user sees SAB's exact reason; got: {err}"
    );
}
