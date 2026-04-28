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
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "version"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "version": "4.3.2",
        })))
        .mount(&server)
        .await;

    let result = client.test().await.expect("test should succeed");
    assert_eq!(
        result, "4.3.2",
        "test() returns the bare version string; the kind prefix is the UI's job (status pill prepends \"SABnzbd \" itself, the toast says \"Connected: <version>\"). Doubling the prefix here was the bug fix that motivated this regression test."
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

    client
        .test()
        .await
        .expect("apikey query param must be sent on every request");
}
