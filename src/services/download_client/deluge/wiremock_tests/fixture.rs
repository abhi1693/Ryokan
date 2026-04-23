//! Shared Deluge wiremock fixture.
//!
//! Deluge dispatches by the JSON body's `method` field, not by
//! URL path. Every call goes to `/json` — the helper below
//! registers per-method responses behind a `body_partial_json`
//! matcher so test bodies stay readable.
//!
//! Pre-seeded handshake (auth.login → web.get_hosts →
//! web.get_plugins with Label already enabled → web.connect →
//! label.add) means each test only needs to wire the method it's
//! actually exercising.

use serde_json::{Value, json};
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::services::download_client::deluge::DelugeClient;

/// Register a mock RPC for `method_name` that returns `result`
/// inside the `{"result": ..., "error": null, "id": 1}` envelope.
pub(super) async fn install_rpc(server: &MockServer, method_name: &str, result: Value) {
    Mock::given(method("POST"))
        .and(path("/json"))
        .and(body_partial_json(json!({"method": method_name})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": result,
            "error": null,
            "id": 1,
        })))
        .mount(server)
        .await;
}

/// Register a mock RPC that returns an error envelope. Deluge
/// surfaces RPC errors via the `error.message` field on an
/// otherwise-HTTP-200 response.
pub(super) async fn install_rpc_error(server: &MockServer, method_name: &str, error_message: &str) {
    Mock::given(method("POST"))
        .and(path("/json"))
        .and(body_partial_json(json!({"method": method_name})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": null,
            "error": {
                "code": -32603,
                "message": error_message,
            },
            "id": 1,
        })))
        .mount(server)
        .await;
}

/// Install the full connect handshake (auth.login → web.get_hosts
/// → web.get_plugins with Label already enabled → web.connect →
/// label.add) plus `daemon.get_version` which `test()` calls after
/// connect. Tests that exercise a post-connect trait method rely on
/// this being in place. Deliberately does NOT mount a generic
/// `label.set_torrent` mock — wiremock-rs picks the first-matching
/// registered mock, and a permissive fixture mock would shadow
/// expect()-count-asserting mocks registered by individual tests.
/// The impl swallows a `label.set_torrent` failure via tracing::warn
/// (non-fatal), so tests that don't care about that call can
/// silently ignore the resulting 404.
pub(super) async fn install_handshake(server: &MockServer) {
    install_rpc(server, "auth.login", json!(true)).await;
    install_rpc(
        server,
        "web.get_hosts",
        // One host, shape `[host_id, host, port, user]`.
        json!([["host-abc", "127.0.0.1", 58846, "localclient"]]),
    )
    .await;
    install_rpc(
        server,
        "web.get_plugins",
        json!({
            "enabled_plugins": ["Label"],
            "available_plugins": ["Label"],
        }),
    )
    .await;
    install_rpc(server, "web.connect", json!(null)).await;
    install_rpc(server, "label.add", json!(null)).await;
    // `test()` calls daemon.get_version after connect to probe the
    // live daemon version. Mount a canned "2.2.0" response so tests
    // that drive `.test()` see a successful string, not a 404.
    install_rpc(server, "daemon.get_version", json!("2.2.0")).await;
}

/// Spin up a new mock server with the handshake pre-wired and
/// return a `DelugeClient` bound to it. Callers register per-test
/// response expectations on top.
pub(super) async fn new_fixture() -> (MockServer, DelugeClient) {
    let server = MockServer::start().await;
    install_handshake(&server).await;
    let client = DelugeClient::new(&server.uri(), "hunter2", "ryokan-test");
    (server, client)
}
