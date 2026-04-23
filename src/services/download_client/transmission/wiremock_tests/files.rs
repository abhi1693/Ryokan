//! `get_files` + `set_file_wanted`. Transmission's file-priority
//! model uses TWO separate keys on `torrent-set` — `files-wanted`
//! (array of indices to enable) and `files-unwanted` (array to
//! disable). Deliberately distinct from qBit's single integer
//! priority and Deluge's full priority-array patch.

use serde_json::json;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, ResponseTemplate};

use super::fixture::{TEST_SESSION_ID, install_rpc, new_fixture};
use crate::services::download_client::DownloadClient;

const HASH: &str = "aabbcc0011223344";

fn files_status(wanted: &[bool]) -> serde_json::Value {
    let files: Vec<_> = (0..wanted.len())
        .map(|i| {
            json!({
                "name": format!("ep_{i:02}.mkv"),
                "length": 1_000_000_000_i64,
                "bytesCompleted": 500_000_000_i64,
            })
        })
        .collect();
    let file_stats: Vec<_> = wanted
        .iter()
        .map(|w| {
            json!({
                "wanted": w,
                "priority": 0,
                "bytesCompleted": 500_000_000_i64,
            })
        })
        .collect();
    json!({
        "torrents": [
            {
                "id": 1,
                "hashString": HASH,
                "name": "Batch",
                "totalSize": 1_000_000_000_i64,
                "percentDone": 0.5,
                "rateDownload": 0,
                "status": 4,
                "eta": 0,
                "downloadDir": "/downloads",
                "labels": ["ryokan-test"],
                "isStalled": false,
                "errorString": "",
                "files": files,
                "fileStats": file_stats,
            }
        ]
    })
}

#[tokio::test]
async fn get_files_maps_wanted_bool_straight_through() {
    // Transmission's `fileStats[].wanted` is already a bool — the
    // simplest per-file flag of the three clients. Just round-trip.
    let (server, client) = new_fixture().await;
    install_rpc(&server, "torrent-get", files_status(&[true, false, true])).await;
    let files = client.get_files(HASH).await.expect("get_files");
    assert_eq!(files.len(), 3);
    assert!(files[0].wanted);
    assert!(!files[1].wanted);
    assert!(files[2].wanted);
}

#[tokio::test]
async fn set_file_wanted_true_sends_files_wanted_key() {
    let (server, client) = new_fixture().await;
    Mock::given(method("POST"))
        .and(path("/transmission/rpc"))
        .and(header("x-transmission-session-id", TEST_SESSION_ID))
        .and(body_partial_json(json!({
            "method": "torrent-set",
            "arguments": {
                "ids": [HASH],
                "files-wanted": [0, 2]
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
    client
        .set_file_wanted(HASH, &[0, 2], true)
        .await
        .expect("set_file_wanted true");
}

#[tokio::test]
async fn set_file_wanted_false_sends_files_unwanted_key() {
    // The KEY SWITCHES: `files-unwanted` instead of
    // `files-wanted`. A refactor that re-used `files-wanted` with
    // a negated sense would silently break every Transmission
    // user's file filtering.
    let (server, client) = new_fixture().await;
    Mock::given(method("POST"))
        .and(path("/transmission/rpc"))
        .and(header("x-transmission-session-id", TEST_SESSION_ID))
        .and(body_partial_json(json!({
            "method": "torrent-set",
            "arguments": {
                "ids": [HASH],
                "files-unwanted": [1, 3]
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
    client
        .set_file_wanted(HASH, &[1, 3], false)
        .await
        .expect("set_file_wanted false");
}

#[tokio::test]
async fn get_files_empty_array_returns_empty_vec() {
    let (server, client) = new_fixture().await;
    install_rpc(&server, "torrent-get", files_status(&[])).await;
    let files = client.get_files(HASH).await.expect("get_files");
    assert!(files.is_empty());
}

#[tokio::test]
async fn get_files_surfaces_torrent_not_found_when_empty_list() {
    // Transmission's `torrent-get` with a filter that matches no
    // torrents returns `{"torrents": []}`. The impl's get_torrent
    // helper surfaces this as "torrent not found" rather than
    // silently returning empty files.
    let (server, client) = new_fixture().await;
    install_rpc(&server, "torrent-get", json!({"torrents": []})).await;
    let err = client.get_files(HASH).await.unwrap_err();
    assert!(
        err.contains("not found"),
        "missing torrent should surface cleanly: {err}"
    );
}
