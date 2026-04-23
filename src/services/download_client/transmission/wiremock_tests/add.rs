//! `torrent-add` — including Transmission's `torrent-duplicate`
//! envelope disambiguation (on a `result: "success"` response,
//! not an error).

use serde_json::json;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, ResponseTemplate};

use super::fixture::{TEST_SESSION_ID, install_rpc, new_fixture};
use crate::services::download_client::AddOutcome;
use crate::services::download_client::DownloadClient;

const MAGNET: &str = "magnet:?xt=urn:btih:aabbccddeeff00112233445566778899aabbccdd";
const HASH: &str = "aabbccddeeff00112233445566778899aabbccdd";

#[tokio::test]
async fn happy_path_returns_added_on_torrent_added_envelope() {
    let (server, client) = new_fixture().await;
    install_rpc(
        &server,
        "torrent-add",
        json!({
            "torrent-added": {
                "id": 42,
                "name": "Test Release",
                "hashString": HASH,
            }
        }),
    )
    .await;
    let outcome = client.add_torrent(MAGNET, HASH).await.expect("add");
    assert_eq!(outcome, AddOutcome::Added);
}

#[tokio::test]
async fn torrent_duplicate_envelope_returns_already_present() {
    // The key Transmission quirk: duplicate adds arrive as
    // `result: "success"` with a `torrent-duplicate` key instead
    // of `torrent-added`. Error-path disambiguation (like Deluge
    // uses) would miss this because the top-level result is still
    // "success."
    let (server, client) = new_fixture().await;
    install_rpc(
        &server,
        "torrent-add",
        json!({
            "torrent-duplicate": {
                "id": 42,
                "name": "Test Release",
                "hashString": HASH,
            }
        }),
    )
    .await;
    let outcome = client.add_torrent(MAGNET, HASH).await.expect("add");
    assert_eq!(outcome, AddOutcome::AlreadyPresent);
}

#[tokio::test]
async fn add_torrent_sends_filename_labels_and_paused_flag() {
    // Pin the full arguments shape. `filename` is the URL/magnet
    // input key; `labels` is the Transmission-native tagging; and
    // `paused: false` on the non-paused entry point.
    let (server, client) = new_fixture().await;
    Mock::given(method("POST"))
        .and(path("/transmission/rpc"))
        .and(header("x-transmission-session-id", TEST_SESSION_ID))
        .and(body_partial_json(json!({
            "method": "torrent-add",
            "arguments": {
                "filename": MAGNET,
                "labels": ["ryokan-test"],
                "paused": false,
            }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": "success",
            "arguments": {"torrent-added": {"id": 1, "name": "t", "hashString": HASH}},
            "tag": 0,
        })))
        .expect(1)
        .mount(&server)
        .await;
    client.add_torrent(MAGNET, HASH).await.expect("add");
}

#[tokio::test]
async fn add_torrent_paused_flips_paused_flag_to_true() {
    let (server, client) = new_fixture().await;
    Mock::given(method("POST"))
        .and(path("/transmission/rpc"))
        .and(header("x-transmission-session-id", TEST_SESSION_ID))
        .and(body_partial_json(json!({
            "method": "torrent-add",
            "arguments": {"paused": true}
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": "success",
            "arguments": {"torrent-added": {"id": 1, "name": "t", "hashString": HASH}},
            "tag": 0,
        })))
        .expect(1)
        .mount(&server)
        .await;
    client
        .add_torrent_paused(MAGNET, HASH)
        .await
        .expect("add paused");
}

#[tokio::test]
async fn add_error_result_string_propagates_to_caller() {
    // Any non-"success" result string is surfaced verbatim by the
    // impl so the operator sees the real daemon-side reason.
    let (server, client) = new_fixture().await;
    super::fixture::install_rpc_error(&server, "torrent-add", "invalid or corrupt torrent file")
        .await;
    let err = client.add_torrent(MAGNET, HASH).await.unwrap_err();
    assert!(
        err.contains("invalid") || err.contains("corrupt"),
        "daemon error message should propagate: {err}"
    );
}

#[tokio::test]
async fn duplicate_add_also_sends_torrent_set_labels_to_adopt_existing_torrent() {
    // Duplicate-add doesn't re-apply labels — the impl explicitly
    // calls `torrent-set` with our scoping label so a user-added-
    // then-re-grabbed torrent becomes visible to `list_scoped`.
    let (server, client) = new_fixture().await;
    install_rpc(
        &server,
        "torrent-add",
        json!({
            "torrent-duplicate": {
                "id": 42,
                "name": "Test Release",
                "hashString": HASH,
            }
        }),
    )
    .await;
    // torrent-set with labels: must fire exactly once after the
    // duplicate-add resolves.
    Mock::given(method("POST"))
        .and(path("/transmission/rpc"))
        .and(header("x-transmission-session-id", TEST_SESSION_ID))
        .and(body_partial_json(json!({
            "method": "torrent-set",
            "arguments": {"ids": [HASH], "labels": ["ryokan-test"]}
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": "success",
            "arguments": {},
            "tag": 0,
        })))
        .expect(1)
        .mount(&server)
        .await;
    client.add_torrent(MAGNET, HASH).await.expect("add");
}
