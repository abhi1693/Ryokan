//! `pause` / `resume` / `delete`. The delete path is the most
//! interesting: `d.erase` does NOT touch disk per rtorrent's docs
//! ("the data stored for the item is not touched in any way"), so
//! the impl reads `d.base_path`/`d.directory`/`d.name` first, runs
//! `d.erase`, then removes the filesystem path separately in a
//! `spawn_blocking`. The safe-delete guard (`base_path != directory`)
//! is already covered by unit tests in the parent module; these
//! tests pin the wire shape.

use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, ResponseTemplate};

use super::fixture::{install_xmlrpc, int_response, new_fixture};
use crate::services::download_client::DownloadClient;

const HASH_LC: &str = "aabbccddeeff00112233445566778899aabbccdd";
const HASH_UC: &str = "AABBCCDDEEFF00112233445566778899AABBCCDD";

#[tokio::test]
async fn pause_calls_d_pause_with_uppercase_hash() {
    let (server, client) = new_fixture().await;
    Mock::given(method("POST"))
        .and(path("/RPC2"))
        .and(body_string_contains("<methodName>d.pause</methodName>"))
        .and(body_string_contains(HASH_UC))
        .respond_with(ResponseTemplate::new(200).set_body_string(int_response(0)))
        .expect(1)
        .mount(&server)
        .await;
    client.pause(HASH_LC).await.expect("pause");
}

#[tokio::test]
async fn resume_calls_d_resume_with_uppercase_hash() {
    let (server, client) = new_fixture().await;
    Mock::given(method("POST"))
        .and(path("/RPC2"))
        .and(body_string_contains("<methodName>d.resume</methodName>"))
        .and(body_string_contains(HASH_UC))
        .respond_with(ResponseTemplate::new(200).set_body_string(int_response(0)))
        .expect(1)
        .mount(&server)
        .await;
    client.resume(HASH_LC).await.expect("resume");
}

#[tokio::test]
async fn delete_with_files_false_skips_filesystem_read() {
    // delete_files=false means keep files on disk. The impl
    // short-circuits — no d.multicall2 to read paths, just
    // d.erase and return. Enforce that ordering with expect(0)
    // on the multicall.
    let (server, client) = new_fixture().await;
    Mock::given(method("POST"))
        .and(path("/RPC2"))
        .and(body_string_contains(
            "<methodName>d.multicall2</methodName>",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_string(int_response(0)))
        .expect(0)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/RPC2"))
        .and(body_string_contains("<methodName>d.erase</methodName>"))
        .and(body_string_contains(HASH_UC))
        .respond_with(ResponseTemplate::new(200).set_body_string(int_response(0)))
        .expect(1)
        .mount(&server)
        .await;
    client.delete(HASH_LC, false).await.expect("delete keep");
}

#[tokio::test]
async fn delete_with_files_true_reads_paths_via_multicall_then_erases() {
    // delete_files=true: the impl reads base_path/directory/name
    // via d.multicall2, then calls d.erase. The actual FS removal
    // runs in spawn_blocking on a .meta sentinel (so this test
    // can't observe filesystem effects, but both RPC calls should
    // fire).
    let (server, client) = new_fixture().await;
    // Return a pre-metadata .meta base_path so the safe-delete
    // guard in the impl skips actual FS removal but both RPCs
    // still fire.
    let row = format!(
        "<array><data>\
            <value><string>{HASH_UC}</string></value>\
            <value><string>/downloads/Pending.meta</string></value>\
            <value><string>/downloads</string></value>\
            <value><string>Pending.meta</string></value>\
        </data></array>"
    );
    install_xmlrpc(
        &server,
        "d.multicall2",
        super::fixture::array_response(&[row]),
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/RPC2"))
        .and(body_string_contains("<methodName>d.erase</methodName>"))
        .respond_with(ResponseTemplate::new(200).set_body_string(int_response(0)))
        .expect(1)
        .mount(&server)
        .await;
    client.delete(HASH_LC, true).await.expect("delete+fs");
}

#[tokio::test]
async fn fault_response_propagates_error_to_caller() {
    // XML-RPC fault envelope carries an error code + string. The
    // impl's decoder already tests fault parsing at the codec
    // level; this one pins that a fault on a trait method
    // surfaces as a Rust error (not silently succeeds).
    let (server, client) = new_fixture().await;
    let fault_body = "<?xml version=\"1.0\"?>\
        <methodResponse><fault><value><struct>\
        <member><name>faultCode</name><value><i4>-501</i4></value></member>\
        <member><name>faultString</name><value><string>Torrent not found</string></value></member>\
        </struct></value></fault></methodResponse>";
    Mock::given(method("POST"))
        .and(path("/RPC2"))
        .and(body_string_contains("<methodName>d.pause</methodName>"))
        .respond_with(ResponseTemplate::new(200).set_body_string(fault_body))
        .mount(&server)
        .await;
    let err = client.pause(HASH_LC).await.unwrap_err();
    assert!(
        err.to_lowercase().contains("not found") || err.contains("-501") || err.contains("fault"),
        "fault should propagate to caller: {err}"
    );
}
