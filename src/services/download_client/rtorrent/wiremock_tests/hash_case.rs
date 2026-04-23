//! Trait-boundary hash-case contract. rtorrent addresses torrents
//! by UPPERCASE hex hash on the wire, but the `DownloadClient`
//! trait contract says callers always pass lowercase. This file
//! pins the case-munging contract from every angle we can reach.

use super::fixture::{array_response, install_xmlrpc, int_response, new_fixture};
use crate::services::download_client::DownloadClient;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, ResponseTemplate};

const HASH_LC: &str = "aabbccddeeff00112233445566778899aabbccdd";
const HASH_UC: &str = "AABBCCDDEEFF00112233445566778899AABBCCDD";

#[tokio::test]
async fn pause_accepts_lowercase_hash_and_sends_uppercase_on_wire() {
    let (server, client) = new_fixture().await;
    Mock::given(method("POST"))
        .and(path("/RPC2"))
        .and(body_string_contains("<methodName>d.pause</methodName>"))
        .and(body_string_contains(HASH_UC))
        .respond_with(ResponseTemplate::new(200).set_body_string(int_response(0)))
        .expect(1)
        .mount(&server)
        .await;
    // Below mock asserts LOWERCASE hash is NOT on the wire —
    // if the impl forgets to case-munge, expect(0) trips.
    Mock::given(method("POST"))
        .and(path("/RPC2"))
        .and(body_string_contains("<methodName>d.pause</methodName>"))
        .and(body_string_contains(HASH_LC))
        .respond_with(ResponseTemplate::new(200).set_body_string(int_response(0)))
        .expect(0)
        .mount(&server)
        .await;
    client.pause(HASH_LC).await.expect("pause");
}

#[tokio::test]
async fn hash_exists_compares_case_insensitively() {
    // The d.multicall2 row returns an UPPERCASE hash. The impl's
    // hash_exists helper compares `eq_ignore_ascii_case`, so a
    // lowercase input matches an uppercase row. A future refactor
    // that forgot the case-insensitive compare would report
    // "hash absent" for every add and keep re-adding duplicates.
    let (server, client) = new_fixture().await;
    let row = format!("<array><data><value><string>{HASH_UC}</string></value></data></array>");
    install_xmlrpc(&server, "d.multicall2", array_response(&[row])).await;
    // Label reapplication on already-present — harmless if it fires.
    install_xmlrpc(&server, "d.custom1.set", int_response(0)).await;
    let outcome = client
        .add_torrent("magnet:?xt=urn:btih:aabb", HASH_LC)
        .await
        .expect("add");
    assert_eq!(
        outcome,
        crate::services::download_client::AddOutcome::AlreadyPresent,
        "lowercase input must match uppercase-on-wire hash"
    );
}
