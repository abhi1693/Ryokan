//! `list_scoped` against a mocked SAB. Verifies queue + history
//! merge, category filtering, and SAB → normalized state mapping.

use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, ResponseTemplate};

use super::fixture::new_fixture;
use crate::services::download_client::{DownloadClient, DownloadItemState};

#[tokio::test]
async fn list_scoped_merges_queue_and_history_filtered_by_category() {
    let (server, client) = new_fixture().await;
    // Queue carries a mix: one in-category, one foreign-category.
    // Only the in-category slot should land in the result.
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "queue"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "queue": {
                "slots": [
                    {
                        "nzo_id": "SABnzbd_nzo_q1",
                        "filename": "show-01.nzb",
                        "cat": "ryokan-test",
                        "status": "Downloading",
                        "percentage": "47",
                        "kbpersec": "2048",
                        "timeleft": "0:01:30",
                        "url": "https://x/q1.nzb",
                    },
                    {
                        "nzo_id": "SABnzbd_nzo_other",
                        "filename": "other-01.nzb",
                        "cat": "different",
                        "status": "Downloading",
                        "percentage": "12",
                        "kbpersec": "0",
                        "timeleft": "0:00:00",
                        "url": "https://x/other.nzb",
                    }
                ]
            }
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "history"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "history": {
                "slots": [
                    {
                        "nzo_id": "SABnzbd_nzo_h1",
                        "name": "show-02",
                        "category": "ryokan-test",
                        "status": "Completed",
                        "storage": "/mnt/sab/complete/show-02",
                        "bytes": 524288,
                        "url": "https://x/h1.nzb",
                    }
                ]
            }
        })))
        .mount(&server)
        .await;

    let items = client.list_scoped().await.expect("list_scoped");
    assert_eq!(
        items.len(),
        2,
        "expect one queue slot + one history slot in-category"
    );
    let q = items.iter().find(|i| i.hash == "SABnzbd_nzo_q1").unwrap();
    assert_eq!(q.name, "show-01.nzb");
    assert_eq!(q.state_kind, DownloadItemState::Downloading);
    assert!(
        (q.progress - 0.47).abs() < 0.001,
        "queue progress maps `percentage`/100, got {}",
        q.progress
    );
    assert_eq!(
        q.dlspeed,
        2048 * 1024,
        "kbpersec → bytes/sec conversion: 2048 * 1024 = {}",
        2048 * 1024
    );
    assert_eq!(q.eta, 90, "timeleft 0:01:30 = 90s");
    assert_eq!(
        q.content_path, "",
        "queue slots have no content_path until SAB unpacks (history)"
    );

    let h = items.iter().find(|i| i.hash == "SABnzbd_nzo_h1").unwrap();
    assert_eq!(h.state_kind, DownloadItemState::PausedComplete);
    assert!(
        h.state_kind.is_complete(),
        "Completed must satisfy is_complete() so post-processing imports"
    );
    assert_eq!(h.size, 524288);
    assert_eq!(
        h.content_path, "/mnt/sab/complete/show-02",
        "history `storage` populates content_path"
    );

    assert!(
        items.iter().all(|i| i.hash != "SABnzbd_nzo_other"),
        "foreign-category slot must be filtered out by list_scoped"
    );
}

#[tokio::test]
async fn list_scoped_returns_empty_when_no_matching_category() {
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "queue"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "queue": { "slots": [] }
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "history"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "history": { "slots": [] }
        })))
        .mount(&server)
        .await;

    let items = client.list_scoped().await.expect("list_scoped");
    assert!(items.is_empty());
}

#[tokio::test]
async fn list_scoped_unknown_history_status_surfaces_as_errored() {
    // History rows in unknown post-proc states ("Repair Failed",
    // "Move Failed") would import broken data if treated as
    // complete. Errored makes post-processing skip them.
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "queue"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "queue": { "slots": [] }
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "history"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "history": {
                "slots": [
                    {
                        "nzo_id": "SABnzbd_nzo_broken",
                        "name": "broken-show",
                        "category": "ryokan-test",
                        "status": "Repair Failed",
                        "storage": "",
                        "bytes": 0,
                        "url": "",
                    }
                ]
            }
        })))
        .mount(&server)
        .await;

    let items = client.list_scoped().await.expect("list_scoped");
    let broken = items
        .iter()
        .find(|i| i.hash == "SABnzbd_nzo_broken")
        .unwrap();
    assert_eq!(broken.state_kind, DownloadItemState::Errored);
}
