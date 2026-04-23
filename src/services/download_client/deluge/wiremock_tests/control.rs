//! `pause` / `resume` / `delete` RPC shapes.
//!
//! Two Deluge-specific quirks:
//!   * `core.pause_torrent` and `core.resume_torrent` take a
//!     **list** of hashes even for a single torrent. A copy-paste
//!     from qBit's single-hash flow would silently no-op here.
//!   * `core.remove_torrent` (singular) takes `(hash, delete_data)`
//!     positional args. The batch method is `core.remove_torrents`
//!     (plural) with a different signature — Ryokan uses the
//!     single-hash variant.

use serde_json::json;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, ResponseTemplate};

use super::fixture::new_fixture;
use crate::services::download_client::DownloadClient;

const HASH: &str = "aabbcc0011223344";

#[tokio::test]
async fn pause_sends_list_of_hashes_not_single_hash() {
    // The `[[HASH]]` (list wrapped in a list) shape is critical —
    // `core.pause_torrent` expects a list of hashes even when
    // pausing a single torrent. A single-hash `[HASH]` call
    // wouldn't error but would do nothing (wrong shape silently
    // no-ops on Deluge).
    let (server, client) = new_fixture().await;
    Mock::given(method("POST"))
        .and(path("/json"))
        .and(body_partial_json(json!({
            "method": "core.pause_torrent",
            "params": [[HASH]],
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": null,
            "error": null,
            "id": 1,
        })))
        .expect(1)
        .mount(&server)
        .await;
    client.pause(HASH).await.expect("pause");
}

#[tokio::test]
async fn resume_sends_list_of_hashes() {
    let (server, client) = new_fixture().await;
    Mock::given(method("POST"))
        .and(path("/json"))
        .and(body_partial_json(json!({
            "method": "core.resume_torrent",
            "params": [[HASH]],
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": null,
            "error": null,
            "id": 1,
        })))
        .expect(1)
        .mount(&server)
        .await;
    client.resume(HASH).await.expect("resume");
}

#[tokio::test]
async fn delete_sends_single_hash_and_delete_data_true() {
    // `core.remove_torrent` (singular) takes `(hash, delete_data)`
    // positional args. Pin both the method name (not `*_torrents`)
    // and the `delete_data = true` flag.
    let (server, client) = new_fixture().await;
    Mock::given(method("POST"))
        .and(path("/json"))
        .and(body_partial_json(json!({
            "method": "core.remove_torrent",
            "params": [HASH, true],
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": true,
            "error": null,
            "id": 1,
        })))
        .expect(1)
        .mount(&server)
        .await;
    // Belt check: the batch method must NOT fire.
    Mock::given(method("POST"))
        .and(path("/json"))
        .and(body_partial_json(json!({"method": "core.remove_torrents"})))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    client.delete(HASH, true).await.expect("delete");
}

#[tokio::test]
async fn delete_with_delete_data_false_preserves_data_flag() {
    // Blocklist flow sets delete_data=false — the on-disk files
    // should survive so the user can continue seeding under a
    // different client if they choose.
    let (server, client) = new_fixture().await;
    Mock::given(method("POST"))
        .and(path("/json"))
        .and(body_partial_json(json!({
            "method": "core.remove_torrent",
            "params": [HASH, false],
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": true,
            "error": null,
            "id": 1,
        })))
        .expect(1)
        .mount(&server)
        .await;
    client.delete(HASH, false).await.expect("delete keep");
}

#[tokio::test]
async fn pause_rpc_error_surfaces_to_caller() {
    let (server, client) = new_fixture().await;
    super::fixture::install_rpc_error(&server, "core.pause_torrent", "Torrent not found").await;
    let err = client.pause("unknown-hash").await.unwrap_err();
    assert!(
        err.to_lowercase().contains("not found") || err.contains("Torrent"),
        "RPC error should bubble: {err}"
    );
}
