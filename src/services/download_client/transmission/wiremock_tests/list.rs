//! `list_scoped` — Transmission's `torrent-get` has no server-side
//! label filter, so the impl filters client-side by the `labels`
//! array. This file pins that filter behavior and the response
//! parsing.

use serde_json::json;
use wiremock::MockServer;

use super::fixture::{install_rpc, install_session_handshake, new_fixture};
use crate::services::download_client::transmission::TransmissionClient;
use crate::services::download_client::{DownloadClient, DownloadItemState};

fn torrent(
    hash: &str,
    name: &str,
    labels: &[&str],
    status: i32,
    percent_done: f64,
) -> serde_json::Value {
    json!({
        "id": 1,
        "hashString": hash,
        "name": name,
        "totalSize": 1_000_000_000_i64,
        "percentDone": percent_done,
        "rateDownload": 0,
        "status": status,
        "eta": 0,
        "downloadDir": "/downloads",
        "labels": labels,
        "isStalled": false,
        "errorString": "",
        "files": [],
        "fileStats": [],
    })
}

#[tokio::test]
async fn list_scoped_filters_by_label_client_side() {
    // Two torrents: one carrying "ryokan-test", one without.
    // Only the labeled one should appear in `list_scoped`.
    let (server, client) = new_fixture().await;
    install_rpc(
        &server,
        "torrent-get",
        json!({
            "torrents": [
                torrent("ryokan-hash", "Ours", &["ryokan-test"], 6, 1.0),
                torrent("other-hash", "Theirs", &["something-else"], 4, 0.5),
            ]
        }),
    )
    .await;
    let items = client.list_scoped().await.expect("list_scoped");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].hash, "ryokan-hash");
    assert_eq!(items[0].name, "Ours");
}

#[tokio::test]
async fn list_scoped_includes_torrents_with_our_label_among_multiple_labels() {
    // Transmission allows multiple labels on a torrent. Our label
    // being ANY of the values should include the torrent.
    let (server, client) = new_fixture().await;
    install_rpc(
        &server,
        "torrent-get",
        json!({
            "torrents": [
                torrent("multi-hash", "Multi", &["user-tag", "ryokan-test", "anime"], 6, 1.0),
            ]
        }),
    )
    .await;
    let items = client.list_scoped().await.expect("list_scoped");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].hash, "multi-hash");
}

#[tokio::test]
async fn list_scoped_excludes_torrents_without_any_labels() {
    // Unlabeled torrents (added manually outside Ryokan) must not
    // appear — otherwise post-processing would try to import them.
    let (server, client) = new_fixture().await;
    install_rpc(
        &server,
        "torrent-get",
        json!({
            "torrents": [
                torrent("unlabeled", "Orphan", &[], 6, 1.0),
            ]
        }),
    )
    .await;
    let items = client.list_scoped().await.expect("list_scoped");
    assert!(
        items.is_empty(),
        "unlabeled torrents must be filtered out, got {} items",
        items.len()
    );
}

#[tokio::test]
async fn list_scoped_maps_transmission_status_codes_to_normalized_enum() {
    // Transmission status codes: 0=stopped, 1=check-wait,
    // 2=checking, 3=download-wait, 4=downloading, 5=seed-wait,
    // 6=seeding. Pin the mapping via a representative sample.
    let (server, client) = new_fixture().await;
    install_rpc(
        &server,
        "torrent-get",
        json!({
            "torrents": [
                torrent("h4", "downloading", &["ryokan-test"], 4, 0.5),
                torrent("h6", "seeding", &["ryokan-test"], 6, 1.0),
                torrent("h2", "checking", &["ryokan-test"], 2, 0.0),
            ]
        }),
    )
    .await;
    let items = client.list_scoped().await.expect("list_scoped");
    let by_hash: std::collections::HashMap<String, DownloadItemState> = items
        .iter()
        .map(|i| (i.hash.clone(), i.state_kind))
        .collect();
    // Status 4 = downloading → not complete.
    assert!(!by_hash[&"h4".to_string()].is_complete());
    // Status 6 = seeding → complete.
    assert!(by_hash[&"h6".to_string()].is_complete());
    // Status 2 = checking → checking-family, not complete.
    assert!(!by_hash[&"h2".to_string()].is_complete());
}

#[tokio::test]
async fn list_scoped_empty_torrents_array_returns_empty_vec() {
    let (server, client) = new_fixture().await;
    install_rpc(&server, "torrent-get", json!({"torrents": []})).await;
    let items = client.list_scoped().await.expect("list_scoped");
    assert!(items.is_empty());
}

#[tokio::test]
async fn list_scoped_missing_torrents_key_returns_empty_vec() {
    // Transmission is loose about the `arguments` shape — if the
    // key is absent (edge case) the impl should default to empty
    // rather than error.
    let server = MockServer::start().await;
    install_session_handshake(&server).await;
    install_rpc(&server, "torrent-get", json!({})).await;
    let client = TransmissionClient::new(&server.uri(), "", "", "ryokan-test");
    let items = client.list_scoped().await.expect("list_scoped");
    assert!(items.is_empty());
}
