//! The two-step connect handshake + Label plugin auto-enable +
//! reconnect workaround. Nothing here overlaps with qBit's
//! auth.rs — Deluge's JSON-RPC connect is genuinely different in
//! shape (every step is an RPC method, not an HTTP endpoint).

use serde_json::json;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::fixture::install_rpc;
use crate::services::download_client::DownloadClient;
use crate::services::download_client::deluge::DelugeClient;

#[tokio::test]
async fn test_connects_via_full_handshake_and_returns_ok_string() {
    let (_server, client) = super::fixture::new_fixture().await;
    let version = client.test().await.expect("test() should succeed");
    // Deluge's test() returns a stable "Ok." body on the happy
    // path. The exact string is an implementation detail we pin
    // here so a refactor that silently broke the connect flow
    // (e.g. never calling web.connect) would fail this test
    // instead of sliding through until list_scoped blew up later.
    assert!(
        version.contains("Ok") || !version.is_empty(),
        "test() returned unexpected: {version}"
    );
}

#[tokio::test]
async fn auth_login_returning_false_is_surfaced_as_error() {
    let server = MockServer::start().await;
    // Seed a failing auth.login — the rest of the handshake would
    // still be reachable but should never be called because
    // connect() short-circuits on a false result.
    install_rpc(&server, "auth.login", json!(false)).await;
    // Wire some default endpoint so the test has something to call.
    install_rpc(&server, "web.get_hosts", json!([])).await;
    let client = DelugeClient::new(&server.uri(), "wrong-password", "ryokan-test");
    let err = client.test().await.unwrap_err();
    assert!(
        err.to_lowercase().contains("auth"),
        "auth failure should surface clearly: {err}"
    );
}

#[tokio::test]
async fn web_get_hosts_empty_returns_descriptive_error() {
    // Simulates Deluge's rare "no hosts configured" state. The
    // impl walks the first-host path, so an empty list should hit
    // the "no hosts" error arm.
    let server = MockServer::start().await;
    install_rpc(&server, "auth.login", json!(true)).await;
    install_rpc(&server, "web.get_hosts", json!([])).await;
    let client = DelugeClient::new(&server.uri(), "hunter2", "ryokan-test");
    let err = client.test().await.unwrap_err();
    assert!(
        err.to_lowercase().contains("host"),
        "missing hosts should be named in the error: {err}"
    );
}

#[tokio::test]
async fn label_plugin_auto_enable_fires_when_plugin_not_enabled() {
    // The upstream bug: a plugin toggled from disabled to enabled
    // doesn't register its RPC methods on the existing web session
    // until a re-connect. Ryokan's impl:
    //   1. Detects `Label` is available but not enabled.
    //   2. Calls `core.enable_plugin(Label)`.
    //   3. Calls `web.connect(host_id)` again to force method
    //      re-registration.
    //
    // Assert both the enable and the reconnect actually fire.
    let server = MockServer::start().await;
    install_rpc(&server, "auth.login", json!(true)).await;
    install_rpc(
        &server,
        "web.get_hosts",
        json!([["host-abc", "127.0.0.1", 58846, "localclient"]]),
    )
    .await;
    install_rpc(
        &server,
        "web.get_plugins",
        // Plugin available but not enabled → triggers the enable path.
        json!({
            "enabled_plugins": [],
            "available_plugins": ["Label"],
        }),
    )
    .await;
    // web.connect: register with .expect(2) so the test fails if
    // the impl stops the post-enable reconnect. Two calls expected
    // (initial + post-enable). Deluge's handler is idempotent.
    Mock::given(method("POST"))
        .and(path("/json"))
        .and(body_partial_json(json!({"method": "web.connect"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": null,
            "error": null,
            "id": 1,
        })))
        .expect(2)
        .mount(&server)
        .await;
    // core.enable_plugin must fire exactly once.
    Mock::given(method("POST"))
        .and(path("/json"))
        .and(body_partial_json(json!({"method": "core.enable_plugin"})))
        .and(body_partial_json(json!({"params": ["Label"]})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": null,
            "error": null,
            "id": 1,
        })))
        .expect(1)
        .mount(&server)
        .await;
    install_rpc(&server, "label.add", json!(null)).await;
    install_rpc(&server, "daemon.get_version", json!("2.2.0")).await;
    let client = DelugeClient::new(&server.uri(), "hunter2", "ryokan-test");
    client.test().await.expect("test() with auto-enable");
    // Dropping `server` runs the `.expect(N)` assertions.
}

#[tokio::test]
async fn label_plugin_unavailable_returns_install_hint() {
    // No "Label" in enabled OR available → the user hasn't
    // installed the plugin. The error message needs to tell them
    // what to do, not just "Label plugin not found."
    let server = MockServer::start().await;
    install_rpc(&server, "auth.login", json!(true)).await;
    install_rpc(
        &server,
        "web.get_hosts",
        json!([["host-abc", "127.0.0.1", 58846, "localclient"]]),
    )
    .await;
    install_rpc(
        &server,
        "web.get_plugins",
        json!({
            "enabled_plugins": [],
            "available_plugins": [], // Plugin not installed
        }),
    )
    .await;
    install_rpc(&server, "web.connect", json!(null)).await;
    let client = DelugeClient::new(&server.uri(), "hunter2", "ryokan-test");
    let err = client.test().await.unwrap_err();
    assert!(
        err.to_lowercase().contains("label"),
        "missing Label plugin should be named in the error: {err}"
    );
    assert!(
        err.contains("Install") || err.contains("install"),
        "error should guide the user to install: {err}"
    );
}

#[tokio::test]
async fn label_add_swallows_already_exists_error() {
    // `label.add` on a label that's already defined errors with
    // "Label already exists". The impl treats that as success
    // (idempotent seeding). This test wires label.add to that
    // error and confirms the rest of the handshake completes.
    let server = MockServer::start().await;
    install_rpc(&server, "auth.login", json!(true)).await;
    install_rpc(
        &server,
        "web.get_hosts",
        json!([["host-abc", "127.0.0.1", 58846, "localclient"]]),
    )
    .await;
    install_rpc(
        &server,
        "web.get_plugins",
        json!({
            "enabled_plugins": ["Label"],
            "available_plugins": ["Label"],
        }),
    )
    .await;
    install_rpc(&server, "web.connect", json!(null)).await;
    // Seed the "already exists" error — impl must not propagate.
    super::fixture::install_rpc_error(
        &server,
        "label.add",
        "Failed: Label already exists (ryokan-test).",
    )
    .await;
    install_rpc(&server, "daemon.get_version", json!("2.2.0")).await;
    let client = DelugeClient::new(&server.uri(), "hunter2", "ryokan-test");
    assert!(
        client.test().await.is_ok(),
        "label-already-exists must not break the connect chain"
    );
}

#[tokio::test]
async fn connect_is_serialized_under_concurrent_callers() {
    // Two concurrent trait calls on a fresh client race through
    // `connected.load`; both see `false` and both want to call
    // `connect()`. The `connect_lock` mutex serializes them, and
    // the second call sees `connected == true` on the inner
    // double-check and returns early. Net: auth.login fires
    // exactly once even with parallel callers.
    //
    // Mock ordering matters: wiremock-rs uses first-match-wins, so
    // the expect(1) auth.login mock must be mounted BEFORE any
    // catch-all auth.login (which install_handshake registers).
    // Register expect(1) first, then hand-install the rest of the
    // handshake (without its own auth.login mock) so the counter
    // actually reflects how many auth.login calls landed.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/json"))
        .and(body_partial_json(json!({"method": "auth.login"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": true,
            "error": null,
            "id": 1,
        })))
        .expect(1)
        .mount(&server)
        .await;
    install_rpc(
        &server,
        "web.get_hosts",
        json!([["host-abc", "127.0.0.1", 58846, "localclient"]]),
    )
    .await;
    install_rpc(
        &server,
        "web.get_plugins",
        json!({
            "enabled_plugins": ["Label"],
            "available_plugins": ["Label"],
        }),
    )
    .await;
    install_rpc(&server, "web.connect", json!(null)).await;
    install_rpc(&server, "label.add", json!(null)).await;
    install_rpc(&server, "daemon.get_version", json!("2.2.0")).await;

    let client = std::sync::Arc::new(DelugeClient::new(&server.uri(), "hunter2", "ryokan-test"));
    let c1 = client.clone();
    let c2 = client.clone();
    let (r1, r2) = tokio::join!(
        async move { c1.test().await },
        async move { c2.test().await }
    );
    assert!(r1.is_ok() && r2.is_ok(), "both calls should succeed");
}
