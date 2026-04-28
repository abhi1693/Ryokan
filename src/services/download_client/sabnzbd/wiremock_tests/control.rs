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
        .and(path("/api"))
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
        .and(path("/api"))
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
        .and(path("/api"))
        .and(query_param("mode", "queue"))
        .and(query_param("name", "delete"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": false,
            "error": "nzo_id not in queue",
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api"))
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
        .and(path("/api"))
        .and(query_param("mode", "queue"))
        .and(query_param("name", "delete"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": false,
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api"))
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

#[tokio::test]
async fn delete_with_delete_files_true_passes_del_files_one_to_sab() {
    // SAB's `del_files=1` query param tells the daemon to remove the
    // imported files from disk (history-tab fallback path). `=0` (the
    // default) leaves files in place. Pin the wire shape so a refactor
    // that drops `del_files` accidentally doesn't silently leave stale
    // import artifacts on disk after the user explicitly said "delete
    // and remove."
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "history"))
        .and(query_param("name", "delete"))
        .and(query_param("del_files", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": true,
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "queue"))
        .and(query_param("name", "delete"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": false,
        })))
        .mount(&server)
        .await;

    client
        .delete("SABnzbd_nzo_done", true)
        .await
        .expect("history-fallback delete with del_files=1 must succeed");
}

#[tokio::test]
async fn pause_surfaces_error_when_sab_returns_status_false() {
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "queue"))
        .and(query_param("name", "pause"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": false,
            "error": "nzo_id not in queue",
        })))
        .mount(&server)
        .await;

    let result = client.pause("SABnzbd_nzo_phantom").await;
    assert!(
        result.is_err(),
        "status:false on pause must surface as Err so the caller knows the op didn't take"
    );
}

#[tokio::test]
async fn resume_surfaces_error_when_sab_returns_status_false() {
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "queue"))
        .and(query_param("name", "resume"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": false,
        })))
        .mount(&server)
        .await;

    let result = client.resume("SABnzbd_nzo_phantom").await;
    assert!(result.is_err());
}

#[tokio::test]
async fn pause_propagates_http_error_when_server_500s() {
    // Distinct from "status:false in 200 body" — the daemon could
    // be unreachable. Bubble the HTTP error rather than swallowing
    // into a phantom-success.
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "queue"))
        .and(query_param("name", "pause"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let result = client.pause("SABnzbd_nzo_anything").await;
    assert!(result.is_err());
}
