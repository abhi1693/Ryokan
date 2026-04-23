//! `add_torrent` wire-level coverage. Two load-bearing qBit quirks
//! land here: the v5.x `200 "Fails."` duplicate disambiguation via
//! `/torrents/info` (silent on 4.x; 5.x returns `"Fails."` whether
//! the magnet is malformed OR already in the session), and the
//! form-body shape (`urls` + `category` keys).

use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, ResponseTemplate};

use super::fixture::new_fixture;
use crate::services::download_client::DownloadClient;
use crate::services::download_client::{AddOutcome, qbittorrent::QbitClient};

const MAGNET: &str = "magnet:?xt=urn:btih:aabbccddeeff00112233445566778899aabbccdd";
const HASH: &str = "aabbccddeeff00112233445566778899aabbccdd";

#[tokio::test]
async fn happy_path_returns_added_on_ok_body() {
    let (server, client) = new_fixture().await;
    Mock::given(method("POST"))
        .and(path("/api/v2/torrents/add"))
        .respond_with(ResponseTemplate::new(200).set_body_string("Ok."))
        .mount(&server)
        .await;
    let outcome = client.add_torrent(MAGNET, HASH).await.expect("add");
    assert_eq!(outcome, AddOutcome::Added);
}

#[tokio::test]
async fn fails_body_with_hash_present_in_info_returns_already_present() {
    // qBit 5.x's silent-duplicate path: `200 "Fails."` on a hash the
    // session already holds. The impl disambiguates by looking up
    // `/torrents/info?hashes=<hash>` — present → AlreadyPresent.
    let (server, client) = new_fixture().await;
    Mock::given(method("POST"))
        .and(path("/api/v2/torrents/add"))
        .respond_with(ResponseTemplate::new(200).set_body_string("Fails."))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v2/torrents/info"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            {
                "hash": HASH,
                "name": "Existing release",
                "size": 1000,
                "progress": 0.5,
                "dlspeed": 0,
                "state": "downloading",
                "category": "ryokan-test",
                "eta": 0,
                "save_path": "/downloads",
                "content_path": "/downloads/Existing"
            }
        ])))
        .mount(&server)
        .await;
    let outcome = client.add_torrent(MAGNET, HASH).await.expect("add");
    assert_eq!(
        outcome,
        AddOutcome::AlreadyPresent,
        "Fails. body + hash in info should report AlreadyPresent (5.x silent-duplicate)"
    );
}

#[tokio::test]
async fn fails_body_with_hash_absent_from_info_surfaces_as_error() {
    // Same `Fails.` body but the hash is NOT in the session — this
    // is a genuinely-malformed magnet or tracker-rejected URL.
    // Surfacing as error lets auto-search back off instead of
    // assuming the torrent is already being downloaded.
    let (server, client) = new_fixture().await;
    Mock::given(method("POST"))
        .and(path("/api/v2/torrents/add"))
        .respond_with(ResponseTemplate::new(200).set_body_string("Fails."))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v2/torrents/info"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&server)
        .await;
    let err = client.add_torrent(MAGNET, HASH).await.unwrap_err();
    assert!(
        err.contains("Fails.") || err.contains("rejected"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn non_2xx_response_returns_error() {
    let (server, client) = new_fixture().await;
    Mock::given(method("POST"))
        .and(path("/api/v2/torrents/add"))
        .respond_with(ResponseTemplate::new(500).set_body_string("server unhappy"))
        .mount(&server)
        .await;
    let err = client.add_torrent(MAGNET, HASH).await.unwrap_err();
    assert!(
        err.contains("500") || err.contains("add failed"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn form_body_contains_urls_and_category_keys() {
    // Pin the form shape — if a refactor stopped sending `category`
    // (load-bearing for `list_scoped`'s filter), every grab would
    // land outside Ryokan's filter and appear lost.
    let (server, _client) = new_fixture().await;
    Mock::given(method("POST"))
        .and(path("/api/v2/torrents/add"))
        .and(body_string_contains("urls="))
        .and(body_string_contains("category=ryokan-test"))
        .respond_with(ResponseTemplate::new(200).set_body_string("Ok."))
        .expect(1)
        .mount(&server)
        .await;
    // Rebuild a client bound to this specific server instance (the
    // fixture's default category matches the match-expression above).
    let client = QbitClient::new(&server.uri(), "admin", "hunter2", "ryokan-test");
    client.add_torrent(MAGNET, HASH).await.expect("add");
    // Dropping `server` here runs Mock's `expect(1)` check — panics
    // if the matcher wasn't hit exactly once.
}
