//! `pause` / `resume` / `delete` against a mocked SAB. Verifies the
//! wire-format query params for each op and the queue-then-history
//! lookup order that `delete` uses.
//!
//! **Order is queue-first because SAB's history endpoint phantom-
//! succeeds.** Live-probed against SAB 5.0.1 source: queue's
//! `_api_queue_delete` returns `status: bool(removed)` (false on
//! unknown nzo_id), but history's `_api_history_delete` calls
//! `report()` regardless of whether the nzo_id was found in the
//! history DB — bogus nzo_id returns `status: true`. A history-first
//! impl thus phantom-succeeds on every in-flight cancel (queue items
//! aren't in history) and Ryokan marks the grab removed while SAB
//! keeps downloading. Queue-first uses queue's honest signal as the
//! primary path; history is the fallback for the post-import case.

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
async fn delete_tries_queue_first_for_in_flight_cancel() {
    // In-flight cancel: nzo_id is in queue. Queue's honest
    // `status:true` is the canonical signal; history must NOT be
    // touched, since history's phantom-success would have already
    // claimed the delete in the previous (history-first) impl while
    // the actual queue item kept downloading.
    //
    // The `.expect(0)` on history is the load-bearing assertion —
    // `delete` returning Ok is necessary but not sufficient (a
    // history-first impl would also return Ok off the phantom).
    // Pinning that history was never called is what catches a
    // regression.
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "queue"))
        .and(query_param("name", "delete"))
        .and(query_param("del_files", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": true,
        })))
        .mount(&server)
        .await;
    let history_mock = Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "history"))
        .and(query_param("name", "delete"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": true,
        })))
        .expect(0);
    server.register(history_mock).await;

    client
        .delete("SABnzbd_nzo_pending", true)
        .await
        .expect("delete must succeed via queue when the nzo_id is in flight");
    // Wiremock validates `.expect(0)` on drop.
    server.verify().await;
}

#[tokio::test]
async fn delete_falls_through_to_history_when_queue_does_not_have_the_nzo_id() {
    // Post-import cancel: nzo_id has aged out of queue and lives
    // only in history. Queue returns honest `status:false`, history
    // returns `status:true` (real or phantom — for our purposes both
    // mean "user's view is clean"). The `del_files=1` matcher pins
    // that the history call carries the cleanup flag so the
    // unpacked output dir is removed; without it a "delete and
    // remove files" click would leave artifacts on disk.
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
        .and(query_param("del_files", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": true,
        })))
        .mount(&server)
        .await;

    client
        .delete("SABnzbd_nzo_done", true)
        .await
        .expect("delete must succeed via history when queue reports the nzo_id absent");
}

#[tokio::test]
async fn delete_does_not_short_circuit_on_history_phantom_success() {
    // Pin the actual bug fix (2026-05-03): SAB's
    // `mode=history&name=delete` returns `status:true` regardless of
    // whether the nzo_id is in the history DB. The earlier
    // history-first impl saw that phantom success on every in-flight
    // cancel (the nzo_id was in queue, not history) and Ryokan
    // marked the grab removed while SAB kept downloading.
    //
    // This test mocks BOTH endpoints as `status:true` — the
    // ambiguous live SAB shape — and asserts the impl took the
    // queue path (using `.expect(0)` on the history mock). Without
    // the queue-first ordering, history's phantom-true would
    // satisfy the impl even when the queue item was real and
    // active.
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "queue"))
        .and(query_param("name", "delete"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": true,
        })))
        .mount(&server)
        .await;
    let history_mock = Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "history"))
        .and(query_param("name", "delete"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": true,
        })))
        .expect(0);
    server.register(history_mock).await;

    client
        .delete("SABnzbd_nzo_done", true)
        .await
        .expect("delete must succeed via queue without touching history's phantom-success");
    server.verify().await;
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
