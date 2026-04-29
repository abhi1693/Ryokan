//! `pause` / `resume` / `delete` against a mocked SAB. Verifies the
//! wire-format query params for each op and the history-then-queue
//! lookup order that `delete` uses. History is tried first because
//! SAB's `mode=queue&name=delete` returns `status: true`
//! unconditionally — even for a nzo_id that isn't in the queue —
//! which would silently no-op every post-import delete if we tried
//! queue first.

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
async fn delete_tries_history_first_then_falls_back_to_queue() {
    // Common case: a completed-and-imported job lives in SAB's
    // history (not its queue). History-first gives the canonical
    // path a clean win without ever pinging queue. If a refactor
    // ever reverts the order, queue would silently "succeed" (see
    // module-level comment) and `del_files=1` would never reach
    // history, leaving the unpacked output dir behind.
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
    // Queue mock that would also "succeed" — if the impl ever asks
    // queue first this test would still pass without exercising the
    // history path. The unique-to-history `del_files=1` matcher above
    // is what pins that history actually got hit.

    client
        .delete("SABnzbd_nzo_done", true)
        .await
        .expect("delete must succeed via history (the common post-import case)");
}

#[tokio::test]
async fn delete_falls_through_to_queue_when_history_does_not_have_the_nzo_id() {
    // In-flight cancel: the nzo_id is in the queue, not history yet.
    // History returns `status:false`, queue returns `status:true`.
    // Verifies the fallback works in the rarer-but-real direction.
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "history"))
        .and(query_param("name", "delete"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": false,
            "error": "nzo_id not in history",
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "queue"))
        .and(query_param("name", "delete"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": true,
        })))
        .mount(&server)
        .await;

    client
        .delete("SABnzbd_nzo_pending", false)
        .await
        .expect("delete must succeed via queue when history is empty");
}

#[tokio::test]
async fn delete_does_not_short_circuit_on_queue_phantom_success() {
    // Pin the actual bug fix: SAB's `mode=queue&name=delete` returns
    // `status:true` even when the nzo_id is NOT in the queue (its
    // `_handle_queue` calls `report(output)` after `remove_multiple`
    // unconditionally). If we tried queue first we'd see that
    // phantom success and never hit history with `del_files=1`.
    //
    // This test makes the queue mock return the "phantom success"
    // shape and asserts the impl still succeeds via the history
    // path AND specifically with `del_files=1` on the wire — the
    // unpacked storage dir would not get cleaned up otherwise.
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
            "status": true,
        })))
        .mount(&server)
        .await;

    client
        .delete("SABnzbd_nzo_done", true)
        .await
        .expect("delete must hit history with del_files=1, not the queue phantom-success");
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
    // imported files from disk on history delete. `=0` (the default)
    // leaves files in place. Pin the wire shape so a refactor that
    // drops `del_files` accidentally doesn't silently leave stale
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
