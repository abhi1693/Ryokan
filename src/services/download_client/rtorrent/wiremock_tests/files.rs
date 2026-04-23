//! `set_file_wanted` — the critical quirk is the MANDATORY
//! `d.update_priorities` call after `f.priority.set`. Without
//! the flush, rtorrent keeps the per-file priority in a
//! staged-but-not-applied state; bandwidth still goes to the
//! skipped files. A naive impl that forgets this call silently
//! broadcasts user picks that never take effect — one of the
//! most common "my script sets priorities and nothing happens"
//! bugs in rtorrent automation.

use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, ResponseTemplate};

use super::fixture::{install_xmlrpc, int_response, new_fixture};
use crate::services::download_client::DownloadClient;

const HASH_LC: &str = "aabbccddeeff00112233445566778899aabbccdd";
const HASH_UC: &str = "AABBCCDDEEFF00112233445566778899AABBCCDD";

#[tokio::test]
async fn set_file_wanted_true_sends_priority_1_per_file() {
    // f.priority.set takes a "target:fN" string addressing file
    // index N on hash target. Value 1 = normal priority = wanted.
    let (server, client) = new_fixture().await;
    Mock::given(method("POST"))
        .and(path("/RPC2"))
        .and(body_string_contains(
            "<methodName>f.priority.set</methodName>",
        ))
        .and(body_string_contains(format!("{HASH_UC}:f0")))
        .and(body_string_contains("<i8>1</i8>"))
        .respond_with(ResponseTemplate::new(200).set_body_string(int_response(0)))
        .expect(1)
        .mount(&server)
        .await;
    // d.update_priorities flush — MUST fire.
    Mock::given(method("POST"))
        .and(path("/RPC2"))
        .and(body_string_contains(
            "<methodName>d.update_priorities</methodName>",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_string(int_response(0)))
        .expect(1)
        .mount(&server)
        .await;
    client
        .set_file_wanted(HASH_LC, &[0], true)
        .await
        .expect("set_file_wanted true");
}

#[tokio::test]
async fn set_file_wanted_false_sends_priority_0_per_file() {
    let (server, client) = new_fixture().await;
    Mock::given(method("POST"))
        .and(path("/RPC2"))
        .and(body_string_contains(
            "<methodName>f.priority.set</methodName>",
        ))
        .and(body_string_contains("<i8>0</i8>"))
        .respond_with(ResponseTemplate::new(200).set_body_string(int_response(0)))
        .expect(1)
        .mount(&server)
        .await;
    install_xmlrpc(&server, "d.update_priorities", int_response(0)).await;
    client
        .set_file_wanted(HASH_LC, &[0], false)
        .await
        .expect("set_file_wanted false");
}

#[tokio::test]
async fn set_file_wanted_fires_priority_set_once_per_index() {
    // Three indices → three priority.set calls + one flush.
    let (server, client) = new_fixture().await;
    Mock::given(method("POST"))
        .and(path("/RPC2"))
        .and(body_string_contains(
            "<methodName>f.priority.set</methodName>",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_string(int_response(0)))
        .expect(3)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/RPC2"))
        .and(body_string_contains(
            "<methodName>d.update_priorities</methodName>",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_string(int_response(0)))
        .expect(1)
        .mount(&server)
        .await;
    client
        .set_file_wanted(HASH_LC, &[0, 1, 2], true)
        .await
        .expect("set_file_wanted batch");
}

#[tokio::test]
async fn set_file_wanted_flush_fires_even_with_empty_indices() {
    // Empty indices → zero priority.set calls, but the flush
    // STILL fires per the current impl loop structure. This pins
    // that behavior — if a future refactor decides to skip the
    // flush when nothing changed (a reasonable optimization), the
    // test prompts a paired change rather than a silent behavior
    // flip.
    let (server, client) = new_fixture().await;
    Mock::given(method("POST"))
        .and(path("/RPC2"))
        .and(body_string_contains(
            "<methodName>f.priority.set</methodName>",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_string(int_response(0)))
        .expect(0)
        .mount(&server)
        .await;
    install_xmlrpc(&server, "d.update_priorities", int_response(0)).await;
    client
        .set_file_wanted(HASH_LC, &[], true)
        .await
        .expect("set_file_wanted empty");
}

#[tokio::test]
async fn set_file_wanted_uses_uppercase_hash_on_wire() {
    // Trait contract says callers pass lowercase hash; impl
    // case-munges internally. The f.priority.set target string
    // must be uppercase or rtorrent silently no-ops (case-
    // sensitive hash lookup).
    let (server, client) = new_fixture().await;
    Mock::given(method("POST"))
        .and(path("/RPC2"))
        .and(body_string_contains(
            "<methodName>f.priority.set</methodName>",
        ))
        .and(body_string_contains(HASH_UC))
        .respond_with(ResponseTemplate::new(200).set_body_string(int_response(0)))
        .expect(1)
        .mount(&server)
        .await;
    install_xmlrpc(&server, "d.update_priorities", int_response(0)).await;
    client
        .set_file_wanted(HASH_LC, &[0], true)
        .await
        .expect("set_file_wanted case check");
}
