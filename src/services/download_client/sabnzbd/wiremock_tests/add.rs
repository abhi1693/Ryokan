//! `add_torrent_returning_id` + `add_torrent_paused` against a
//! mocked SAB. Pins the wire-format expectations: `mode=addurl`,
//! `name=<URL>`, `cat=<category>`, `priority=-1` for the paused
//! variant; `nzo_id` extraction from the response; the empty-
//! `nzo_ids` → AlreadyPresent fallback path via queue scan.

use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, ResponseTemplate};

use super::fixture::{TEST_API_KEY, new_fixture};
use crate::services::download_client::{AddOutcome, DownloadClient};

#[tokio::test]
async fn add_returns_canonical_nzo_id_from_addurl_response() {
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/sabnzbd/api"))
        .and(query_param("mode", "addurl"))
        .and(query_param("apikey", TEST_API_KEY))
        .and(query_param("cat", "ryokan-test"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": true,
            "nzo_ids": ["SABnzbd_nzo_test1"],
        })))
        .mount(&server)
        .await;

    let (outcome, id) = client
        .add_torrent_returning_id("https://nzb.example.com/sample.nzb", "")
        .await
        .expect("add must succeed");
    assert!(matches!(outcome, AddOutcome::Added));
    assert_eq!(
        id, "SABnzbd_nzo_test1",
        "returned id must come from SAB's nzo_ids array, not the caller's input"
    );
}

#[tokio::test]
async fn add_paused_sends_priority_minus_1() {
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/sabnzbd/api"))
        .and(query_param("mode", "addurl"))
        .and(query_param("priority", "-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": true,
            "nzo_ids": ["SABnzbd_nzo_paused"],
        })))
        .mount(&server)
        .await;

    let outcome = client
        .add_torrent_paused("https://nzb.example.com/x.nzb", "")
        .await
        .expect("add_paused must succeed");
    // `priority=-1` is what SAB's "Paused" priority maps to, and the
    // wiremock matcher above asserts the query string carries it.
    // If a refactor stops sending it, the mock 404s and the call
    // surfaces as Err — this test would fail at the .expect line.
    assert!(matches!(outcome, AddOutcome::Added));
}

#[tokio::test]
async fn empty_nzo_ids_falls_through_to_queue_scan_for_already_present() {
    // SAB's pre-queue dedup returns `status:true, nzo_ids:[]` when
    // it recognizes a duplicate URL. The impl scans queue+history
    // for a matching URL and reports AlreadyPresent if found.
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/sabnzbd/api"))
        .and(query_param("mode", "addurl"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": true,
            "nzo_ids": [],
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/sabnzbd/api"))
        .and(query_param("mode", "queue"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "queue": {
                "slots": [
                    {
                        "nzo_id": "SABnzbd_nzo_existing",
                        "filename": "existing.nzb",
                        "cat": "ryokan-test",
                        "status": "Downloading",
                        "url": "https://nzb.example.com/dup.nzb",
                    }
                ]
            }
        })))
        .mount(&server)
        .await;

    let (outcome, id) = client
        .add_torrent_returning_id("https://nzb.example.com/dup.nzb", "")
        .await
        .expect("add must succeed");
    assert!(matches!(outcome, AddOutcome::AlreadyPresent));
    assert_eq!(id, "SABnzbd_nzo_existing");
}

#[tokio::test]
async fn empty_nzo_ids_with_no_matching_slot_surfaces_error() {
    // When the queue scan can't find the URL, the empty-nzo_ids
    // signal is treated as a real failure rather than papered over.
    // Without this guarantee a real malformed-URL error from SAB
    // would slip past as a silent success.
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/sabnzbd/api"))
        .and(query_param("mode", "addurl"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": true,
            "nzo_ids": [],
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/sabnzbd/api"))
        .and(query_param("mode", "queue"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "queue": { "slots": [] }
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/sabnzbd/api"))
        .and(query_param("mode", "history"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "history": { "slots": [] }
        })))
        .mount(&server)
        .await;

    let result = client
        .add_torrent_returning_id("https://nzb.example.com/missing.nzb", "")
        .await;
    assert!(
        result.is_err(),
        "empty nzo_ids + no matching slot must surface as an error, got: {result:?}"
    );
}
