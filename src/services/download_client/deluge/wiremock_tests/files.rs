//! `get_files` + `set_file_wanted` — Deluge's priority scale is
//! `0=skip / 1=low / 4=normal / 7=high`, explicitly **different from
//! qBit's 0/1**. Writing 1 where Ryokan meant "wanted" would
//! bandwidth-de-prioritize every file relative to peers; this test
//! file pins the 0/4 contract so a copy-paste from the qBit impl
//! can't silently regress behavior.

use serde_json::json;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, ResponseTemplate};

use super::fixture::{install_rpc, new_fixture};
use crate::services::download_client::DownloadClient;

const HASH: &str = "aabbcc0011223344";

fn status_with_priorities(prios: &[i32]) -> serde_json::Value {
    let files: Vec<_> = (0..prios.len())
        .map(|i| {
            json!({
                "path": format!("ep_{i:02}.mkv"),
                "size": 1_000_000_000_i64,
                "index": i,
            })
        })
        .collect();
    json!({
        "file_priorities": prios,
        "files": files,
        "file_progress": vec![1.0_f64; prios.len()],
        "hash": HASH,
        "name": "Test Batch",
        "total_size": 1_000_000_000_i64,
        "progress": 100.0,
        "download_payload_rate": 0,
        "state": "Seeding",
        "eta": 0,
        "save_path": "/downloads",
        "is_finished": true,
        "label": "ryokan-test"
    })
}

#[tokio::test]
async fn get_files_maps_priority_0_to_wanted_false() {
    let (server, client) = new_fixture().await;
    install_rpc(
        &server,
        "core.get_torrent_status",
        status_with_priorities(&[0]),
    )
    .await;
    let files = client.get_files(HASH).await.expect("get_files");
    assert_eq!(files.len(), 1);
    assert!(
        !files[0].wanted,
        "priority 0 (skip) should map to wanted=false"
    );
}

#[tokio::test]
async fn get_files_maps_priority_4_to_wanted_true() {
    // Normal priority — the qBit-side would be `1`, but Deluge
    // uses 4 here. Test pins the distinction.
    let (server, client) = new_fixture().await;
    install_rpc(
        &server,
        "core.get_torrent_status",
        status_with_priorities(&[4]),
    )
    .await;
    let files = client.get_files(HASH).await.expect("get_files");
    assert!(files[0].wanted, "priority 4 (normal) should be wanted=true");
}

#[tokio::test]
async fn get_files_maps_priority_7_to_wanted_true() {
    // High priority — still wanted=true. Ryokan doesn't write 7
    // itself, but a user-adjusted priority on a re-narrow must
    // round-trip as wanted.
    let (server, client) = new_fixture().await;
    install_rpc(
        &server,
        "core.get_torrent_status",
        status_with_priorities(&[7]),
    )
    .await;
    let files = client.get_files(HASH).await.expect("get_files");
    assert!(files[0].wanted, "priority 7 (high) should be wanted=true");
}

#[tokio::test]
async fn set_file_wanted_false_writes_full_priority_array_with_zeros_at_requested_indices() {
    // Deluge only accepts a full-length priority array — no partial
    // updates. The impl reads current priorities, patches the
    // requested indices, and writes back. Start with all-normal (4)
    // and flip indices 1 and 3 to skip (0).
    let (server, client) = new_fixture().await;
    install_rpc(
        &server,
        "core.get_torrent_status",
        status_with_priorities(&[4, 4, 4, 4, 4]),
    )
    .await;
    // The set_torrent_options params must carry the exact
    // patched array: `[4, 0, 4, 0, 4]`.
    Mock::given(method("POST"))
        .and(path("/json"))
        .and(body_partial_json(json!({
            "method": "core.set_torrent_options",
            "params": [[HASH], {"file_priorities": [4, 0, 4, 0, 4]}],
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": null,
            "error": null,
            "id": 1,
        })))
        .expect(1)
        .mount(&server)
        .await;
    client
        .set_file_wanted(HASH, &[1, 3], false)
        .await
        .expect("set_file_wanted skip");
}

#[tokio::test]
async fn set_file_wanted_true_writes_priority_4_not_1() {
    // Critical: wanted=true maps to Deluge priority 4, NOT 1.
    // Writing 1 instead of 4 would bandwidth-de-prioritize every
    // re-enabled file below peer average.
    let (server, client) = new_fixture().await;
    install_rpc(
        &server,
        "core.get_torrent_status",
        status_with_priorities(&[0, 0, 0]),
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/json"))
        .and(body_partial_json(json!({
            "method": "core.set_torrent_options",
            "params": [[HASH], {"file_priorities": [4, 4, 0]}],
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": null,
            "error": null,
            "id": 1,
        })))
        .expect(1)
        .mount(&server)
        .await;
    client
        .set_file_wanted(HASH, &[0, 1], true)
        .await
        .expect("set_file_wanted wanted");
}

#[tokio::test]
async fn set_file_wanted_out_of_range_index_is_silently_dropped() {
    // The impl uses `prio.get_mut(i)` → `None` for out-of-range
    // indices. A stale modal sending an index past the current
    // file count should no-op gracefully, not panic or return an
    // error. Still writes the unchanged array back.
    let (server, client) = new_fixture().await;
    install_rpc(
        &server,
        "core.get_torrent_status",
        status_with_priorities(&[4, 4]),
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/json"))
        .and(body_partial_json(json!({
            "method": "core.set_torrent_options",
            "params": [[HASH], {"file_priorities": [4, 4]}],
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": null,
            "error": null,
            "id": 1,
        })))
        .expect(1)
        .mount(&server)
        .await;
    client
        .set_file_wanted(HASH, &[99], false)
        .await
        .expect("set_file_wanted stale index");
}
