//! `pause` / `resume` / `delete` RPC shapes. Transmission uses
//! `torrent-stop` / `torrent-start` / `torrent-remove` as method
//! names (distinct from qBit's `/torrents/{pause,resume,delete}`
//! and Deluge's `core.{pause,resume,remove}_torrent`). The
//! `ids` arg takes a list even for a single hash, and
//! `torrent-remove` carries the `delete-local-data` flag.

use serde_json::json;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, ResponseTemplate};

use super::fixture::{TEST_SESSION_ID, new_fixture};
use crate::services::download_client::DownloadClient;

const HASH: &str = "aabbcc0011223344";

#[tokio::test]
async fn pause_sends_torrent_stop_with_ids_list() {
    let (server, client) = new_fixture().await;
    Mock::given(method("POST"))
        .and(path("/transmission/rpc"))
        .and(header("x-transmission-session-id", TEST_SESSION_ID))
        .and(body_partial_json(json!({
            "method": "torrent-stop",
            "arguments": {"ids": [HASH]}
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": "success",
            "arguments": {},
            "tag": 0,
        })))
        .expect(1)
        .mount(&server)
        .await;
    client.pause(HASH).await.expect("pause");
}

#[tokio::test]
async fn resume_sends_torrent_start_with_ids_list() {
    let (server, client) = new_fixture().await;
    Mock::given(method("POST"))
        .and(path("/transmission/rpc"))
        .and(header("x-transmission-session-id", TEST_SESSION_ID))
        .and(body_partial_json(json!({
            "method": "torrent-start",
            "arguments": {"ids": [HASH]}
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": "success",
            "arguments": {},
            "tag": 0,
        })))
        .expect(1)
        .mount(&server)
        .await;
    client.resume(HASH).await.expect("resume");
}

#[tokio::test]
async fn delete_sends_torrent_remove_with_delete_local_data_true() {
    let (server, client) = new_fixture().await;
    Mock::given(method("POST"))
        .and(path("/transmission/rpc"))
        .and(header("x-transmission-session-id", TEST_SESSION_ID))
        .and(body_partial_json(json!({
            "method": "torrent-remove",
            "arguments": {
                "ids": [HASH],
                "delete-local-data": true
            }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": "success",
            "arguments": {},
            "tag": 0,
        })))
        .expect(1)
        .mount(&server)
        .await;
    client.delete(HASH, true).await.expect("delete");
}

#[tokio::test]
async fn delete_with_delete_local_data_false_preserves_on_disk_data() {
    // Blocklist flow keeps files. The flag name (`delete-local-data`)
    // is kebab-case unlike the method names — pin it.
    let (server, client) = new_fixture().await;
    Mock::given(method("POST"))
        .and(path("/transmission/rpc"))
        .and(header("x-transmission-session-id", TEST_SESSION_ID))
        .and(body_partial_json(json!({
            "method": "torrent-remove",
            "arguments": {
                "ids": [HASH],
                "delete-local-data": false
            }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": "success",
            "arguments": {},
            "tag": 0,
        })))
        .expect(1)
        .mount(&server)
        .await;
    client.delete(HASH, false).await.expect("delete keep");
}

#[tokio::test]
async fn control_rpc_errors_surface_to_caller() {
    // Any non-"success" result string is propagated. Distinct from
    // qBit where delete-by-status-code is the error signal.
    let (server, client) = new_fixture().await;
    super::fixture::install_rpc_error(&server, "torrent-stop", "torrent not found").await;
    let err = client.pause(HASH).await.unwrap_err();
    assert!(
        err.contains("not found") || err.to_lowercase().contains("torrent"),
        "RPC error string should propagate: {err}"
    );
}

#[tokio::test]
async fn hash_is_lowercased_before_sending() {
    // Contract boundary: callers pass hash in any case; impl must
    // lowercase before sending to Transmission (case-sensitive
    // on the wire even though BT v1 hashes are case-insensitive
    // conceptually).
    let (server, client) = new_fixture().await;
    let upper_hash = "AABBCC0011223344";
    Mock::given(method("POST"))
        .and(path("/transmission/rpc"))
        .and(header("x-transmission-session-id", TEST_SESSION_ID))
        .and(body_partial_json(json!({
            "method": "torrent-stop",
            "arguments": {"ids": ["aabbcc0011223344"]}
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": "success",
            "arguments": {},
            "tag": 0,
        })))
        .expect(1)
        .mount(&server)
        .await;
    client
        .pause(upper_hash)
        .await
        .expect("pause with upper hash");
}
