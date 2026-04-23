//! Shared Transmission wiremock fixture.
//!
//! Transmission's RPC lives at `/transmission/rpc`. Every call is
//! JSON-RPC-ish:
//!   * Request body: `{"method": "...", "arguments": {...}}`
//!   * Response body: `{"result": "success"|"error-string",
//!     "arguments": {...}}`
//!
//! Method dispatch is by the `method` field in the body. The
//! fixture pre-seeds the session-id handshake (first request
//! without `X-Transmission-Session-Id` returns 409 + header; the
//! impl retries with the header and subsequent calls land at the
//! real mock).

use serde_json::{Value, json};
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::services::download_client::transmission::TransmissionClient;

pub(super) const TEST_SESSION_ID: &str = "test-session-id-aabbcc";

/// Install the CSRF handshake: the first request on this server
/// responds with 409 + `X-Transmission-Session-Id`. The impl
/// captures the header and retries; subsequent requests carry the
/// header and are matched by `install_rpc`'s header-requiring
/// matcher instead of this one.
///
/// `up_to_n_times(1)` exhausts the 409 mock after a single hit so
/// later requests (which would otherwise still match the "method +
/// path" matcher here — we can't easily match on "header absent"
/// in wiremock) fall through to the per-method mocks that require
/// the session-id header.
pub(super) async fn install_session_handshake(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/transmission/rpc"))
        .respond_with(
            ResponseTemplate::new(409).insert_header("X-Transmission-Session-Id", TEST_SESSION_ID),
        )
        .up_to_n_times(1)
        .mount(server)
        .await;
}

/// Register a mock for `method_name` that returns `result` inside
/// the `{"result": "success", "arguments": ...}` envelope. The
/// `body_partial_json` matcher pins the Transmission request body's
/// `method` field; the `header_exists` matcher ensures the
/// session-id handshake has already completed.
pub(super) async fn install_rpc(server: &MockServer, method_name: &str, arguments: Value) {
    Mock::given(method("POST"))
        .and(path("/transmission/rpc"))
        .and(header("x-transmission-session-id", TEST_SESSION_ID))
        .and(body_partial_json(json!({"method": method_name})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": "success",
            "arguments": arguments,
            "tag": 0,
        })))
        .mount(server)
        .await;
}

/// Register a mock that returns a non-success result — Transmission
/// signals errors via the top-level `result` field on an HTTP-200
/// response. The `arguments` object is empty.
pub(super) async fn install_rpc_error(server: &MockServer, method_name: &str, result: &str) {
    Mock::given(method("POST"))
        .and(path("/transmission/rpc"))
        .and(header("x-transmission-session-id", TEST_SESSION_ID))
        .and(body_partial_json(json!({"method": method_name})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": result,
            "arguments": {},
            "tag": 0,
        })))
        .mount(server)
        .await;
}

/// Spin up a mock server, pre-install the session handshake, and
/// return a `TransmissionClient` wired to it. Callers add the
/// per-test method mocks on top.
pub(super) async fn new_fixture() -> (MockServer, TransmissionClient) {
    let server = MockServer::start().await;
    install_session_handshake(&server).await;
    let client = TransmissionClient::new(&server.uri(), "", "", "ryokan-test");
    (server, client)
}
