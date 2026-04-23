//! Login-flow and re-auth coverage.

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::fixture::{install_login_ok, new_fixture};
use crate::services::download_client::DownloadClient;
use crate::services::download_client::qbittorrent::QbitClient;

#[tokio::test]
async fn test_returns_the_version_body() {
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/api/v2/app/version"))
        .respond_with(ResponseTemplate::new(200).set_body_string("v5.1.4"))
        .mount(&server)
        .await;
    let version = client.test().await.expect("test() should succeed");
    assert_eq!(version, "v5.1.4");
}

#[tokio::test]
async fn login_fails_body_surfaces_as_error() {
    // A fresh client without a prior successful login. Override the
    // default login mock with the Fails. body qBit returns on bad
    // credentials.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v2/auth/login"))
        .respond_with(ResponseTemplate::new(200).set_body_string("Fails."))
        .mount(&server)
        .await;
    // Register some endpoint the client can attempt to call —
    // `test()` is simplest.
    Mock::given(method("GET"))
        .and(path("/api/v2/app/version"))
        .respond_with(ResponseTemplate::new(200).set_body_string("v5.0.0"))
        .mount(&server)
        .await;
    let client = QbitClient::new(&server.uri(), "admin", "wrong-pass", "ryokan-test");
    let err = client.test().await.unwrap_err();
    assert!(
        err.contains("auth failed") || err.contains("invalid credentials"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn protected_endpoint_403_triggers_reauth_then_retries_request() {
    // First call to /torrents/info returns 403 — simulating an
    // expired session cookie. The do_get helper clears the
    // logged_in flag, calls login again, and re-issues the original
    // request. The second call should see 200.
    //
    // Wiremock doesn't support "answer differently on 1st vs 2nd
    // call" out of the box, so we use an up_to(1) expectation on a
    // 403 mock followed by a general 200 mock — the ordering of
    // mount order is priority so the specific expectation fires
    // first. Login is registered twice (the retry path calls it
    // again) so `expect(1)` would fail — let it be unbounded.
    let server = MockServer::start().await;
    install_login_ok(&server).await;
    Mock::given(method("GET"))
        .and(path("/api/v2/torrents/info"))
        .respond_with(ResponseTemplate::new(403))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v2/torrents/info"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&server)
        .await;
    let client = QbitClient::new(&server.uri(), "admin", "hunter2", "ryokan-test");
    // list_scoped goes through do_get → triggers the 403 → re-auth → retry path.
    let result = client.list_scoped().await;
    assert!(
        result.is_ok(),
        "list_scoped should recover from a single 403 via re-auth: {result:?}"
    );
}
