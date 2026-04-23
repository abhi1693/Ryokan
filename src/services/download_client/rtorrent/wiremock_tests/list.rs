//! `list_scoped` — `d.multicall2` with an empty-target (rtorrent
//! convention for "all hashes all views") and the custom1-as-label
//! filter wrapping it.

use super::fixture::{array_response, install_xmlrpc, new_fixture};
use crate::services::download_client::DownloadClient;

#[tokio::test]
async fn list_scoped_parses_multicall2_array_of_rows() {
    // `d.multicall2` returns an array where each row is an array
    // of values — one value per column in the caller-supplied
    // schema. The impl fetches a fixed column set (hash, name,
    // size, progress, state, etc.) and maps each row to a
    // DownloadItem.
    let (server, client) = new_fixture().await;
    // Construct a row matching list_scoped's fetched 13-column
    // schema, in exact order (impl-side enforced via cols[N] index):
    //   0: d.hash=        <string>
    //   1: d.name=        <string>
    //   2: d.size_bytes=  <i8>
    //   3: d.bytes_done=  <i8>
    //   4: d.down.rate=   <i8>
    //   5: d.custom1=     <string>
    //   6: d.complete=    <i4>
    //   7: d.is_active=   <i4>
    //   8: d.hashing=     <i4>
    //   9: d.is_open=     <i4>
    //  10: d.message=     <string>
    //  11: d.base_path=   <string>
    //  12: d.directory=   <string>
    let row = "<array><data>\
            <value><string>AABBCCDDEEFF00112233445566778899AABBCCDD</string></value>\
            <value><string>Test.Release</string></value>\
            <value><i8>1000000000</i8></value>\
            <value><i8>1000000000</i8></value>\
            <value><i8>0</i8></value>\
            <value><string>ryokan-test</string></value>\
            <value><i4>1</i4></value>\
            <value><i4>0</i4></value>\
            <value><i4>0</i4></value>\
            <value><i4>1</i4></value>\
            <value><string></string></value>\
            <value><string>/downloads/Test.Release</string></value>\
            <value><string>/downloads</string></value>\
        </data></array>"
        .to_string();
    install_xmlrpc(&server, "d.multicall2", array_response(&[row])).await;
    let items = client.list_scoped().await.expect("list_scoped");
    assert_eq!(items.len(), 1);
    // The hash is uppercase on the wire; the trait-level hash
    // should be lowercased (per the DownloadClient contract).
    assert_eq!(items[0].hash, "aabbccddeeff00112233445566778899aabbccdd");
    assert_eq!(items[0].name, "Test.Release");
}

#[tokio::test]
async fn list_scoped_filters_on_custom1_equal_to_label() {
    // The d.multicall2 body includes a `d.custom1=` read so the
    // impl can filter rows where the returned custom1 equals
    // "ryokan-test". Rows with a different custom1 are excluded.
    let (server, client) = new_fixture().await;
    let ours_row = "<array><data>\
            <value><string>AABBCCDDEEFF00112233445566778899AABBCCDD</string></value>\
            <value><string>Ours</string></value>\
            <value><i8>1</i8></value>\
            <value><i8>1</i8></value>\
            <value><i8>0</i8></value>\
            <value><string>ryokan-test</string></value>\
            <value><i4>1</i4></value>\
            <value><i4>0</i4></value>\
            <value><i4>0</i4></value>\
            <value><i4>1</i4></value>\
            <value><string></string></value>\
            <value><string>/downloads/Ours</string></value>\
            <value><string>/downloads</string></value>\
        </data></array>"
        .to_string();
    let theirs_row = "<array><data>\
            <value><string>BBCCDDEEFF00112233445566778899AABBCCDDEE</string></value>\
            <value><string>Theirs</string></value>\
            <value><i8>1</i8></value>\
            <value><i8>1</i8></value>\
            <value><i8>0</i8></value>\
            <value><string>some-other-label</string></value>\
            <value><i4>1</i4></value>\
            <value><i4>0</i4></value>\
            <value><i4>0</i4></value>\
            <value><i4>1</i4></value>\
            <value><string></string></value>\
            <value><string>/downloads/Theirs</string></value>\
            <value><string>/downloads</string></value>\
        </data></array>"
        .to_string();
    install_xmlrpc(
        &server,
        "d.multicall2",
        array_response(&[ours_row, theirs_row]),
    )
    .await;
    let items = client.list_scoped().await.expect("list_scoped");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].name, "Ours");
}

#[tokio::test]
async fn list_scoped_empty_multicall2_returns_empty_vec() {
    let (server, client) = new_fixture().await;
    install_xmlrpc(&server, "d.multicall2", array_response(&[])).await;
    let items = client.list_scoped().await.expect("list_scoped");
    assert!(items.is_empty());
}
