//! `pause` / `resume` / `delete` against a mocked SAB. Verifies the
//! wire-format query params for each op and the queue-then-history
//! fallback that `delete` uses when the nzo_id isn't in the queue.

use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, ResponseTemplate};

use super::fixture::new_fixture;
use crate::services::download_client::DownloadClient;

#[tokio::test]
async fn pause_sends_queue_pause_with_nzo_id() {
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/sabnzbd/api"))
        .and(query_param("mode", "queue"))
        .and(query_param("name", "pause"))
        .and(query_param("value", "SABnzbd_nzo_xyz"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": true,
        })))
        .mount(&server)
        .await;

    client.pause("SABnzbd_nzo_xyz").await.expect("pause");
}

#[tokio::test]
async fn resume_sends_queue_resume_with_nzo_id() {
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/sabnzbd/api"))
        .and(query_param("mode", "queue"))
        .and(query_param("name", "resume"))
        .and(query_param("value", "SABnzbd_nzo_xyz"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": true,
        })))
        .mount(&server)
        .await;

    client.resume("SABnzbd_nzo_xyz").await.expect("resume");
}

#[tokio::test]
async fn delete_tries_queue_first_then_falls_back_to_history() {
    // Queue delete returns status:false (the nzo_id isn't in the
    // queue — already moved to history). The impl must fall through
    // to history delete; without the fallback, completed grabs that
    // a user blocklists from the Downloads page would never get
    // their unpacked output cleaned up.
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/sabnzbd/api"))
        .and(query_param("mode", "queue"))
        .and(query_param("name", "delete"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": false,
            "error": "nzo_id not in queue",
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/sabnzbd/api"))
        .and(query_param("mode", "history"))
        .and(query_param("name", "delete"))
        .and(query_param("del_files", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": true,
        })))
        .mount(&server)
        .await;

    client
        .delete("SABnzbd_nzo_done", true)
        .await
        .expect("delete must succeed via history fallback");
}

#[tokio::test]
async fn delete_surfaces_error_when_both_queue_and_history_fail() {
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/sabnzbd/api"))
        .and(query_param("mode", "queue"))
        .and(query_param("name", "delete"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": false,
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/sabnzbd/api"))
        .and(query_param("mode", "history"))
        .and(query_param("name", "delete"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": false,
            "error": "no such nzo_id",
        })))
        .mount(&server)
        .await;

    let result = client.delete("SABnzbd_nzo_missing", false).await;
    assert!(
        result.is_err(),
        "both-fail delete must surface as Err so callers don't think they succeeded"
    );
}
