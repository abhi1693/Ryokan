//! CSRF session-id handshake. Transmission returns 409 +
//! `X-Transmission-Session-Id` on every first-contact request; the
//! client captures the header and retries. Mid-stream rotation
//! (daemon restart → fresh session id) goes through the same path.

use serde_json::json;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::fixture::{TEST_SESSION_ID, new_fixture};
use crate::services::download_client::DownloadClient;
use crate::services::download_client::transmission::TransmissionClient;

#[tokio::test]
async fn first_request_receives_409_and_retries_with_captured_header() {
    // `new_fixture()` installs the 409+header handshake with
    // up_to_n_times(1). Install a matching session-get mock that
    // requires the header — if the retry doesn't include it, this
    // mock doesn't match and the client surfaces an error.
    let (server, client) = new_fixture().await;
    Mock::given(method("POST"))
        .and(path("/transmission/rpc"))
        .and(header("x-transmission-session-id", TEST_SESSION_ID))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": "success",
            "arguments": {"version": "4.0.5"},
            "tag": 0,
        })))
        .mount(&server)
        .await;
    let version = client.test().await.expect("test() should succeed");
    assert_eq!(version, "4.0.5");
}

#[tokio::test]
async fn missing_session_id_header_on_409_surfaces_as_error() {
    // Malformed daemon: 409 without the expected header. Nothing
    // to retry with — the impl surfaces a distinctive error so
    // the operator sees the actual cause instead of a generic
    // "auth failed."
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/transmission/rpc"))
        .respond_with(ResponseTemplate::new(409))
        .mount(&server)
        .await;
    let client = TransmissionClient::new(&server.uri(), "", "", "ryokan-test");
    let err = client.test().await.unwrap_err();
    assert!(
        err.contains("X-Transmission-Session-Id") || err.to_lowercase().contains("session"),
        "missing session-id header should be named in the error: {err}"
    );
}

#[tokio::test]
async fn repeated_409_after_retry_surfaces_as_error() {
    // If the daemon rotates the session-id between retry
    // attempts (rare but possible), the impl caps retries at 1
    // to avoid an infinite loop. Two consecutive 409s → error.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/transmission/rpc"))
        .respond_with(
            ResponseTemplate::new(409).insert_header("X-Transmission-Session-Id", TEST_SESSION_ID),
        )
        .mount(&server)
        .await;
    let client = TransmissionClient::new(&server.uri(), "", "", "ryokan-test");
    let err = client.test().await.unwrap_err();
    assert!(
        err.to_lowercase().contains("409") || err.to_lowercase().contains("session"),
        "repeated 409 should surface: {err}"
    );
}

#[tokio::test]
async fn http_401_surfaces_as_auth_failure_not_session_rotation() {
    // Transmission returns 401 for bad Basic-Auth credentials, NOT
    // a 409. The impl distinguishes so a misconfigured password
    // doesn't masquerade as "session rotation."
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/transmission/rpc"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    let client = TransmissionClient::new(&server.uri(), "user", "wrong-pass", "ryokan-test");
    let err = client.test().await.unwrap_err();
    assert!(
        err.to_lowercase().contains("auth"),
        "401 should surface as auth failure: {err}"
    );
}
