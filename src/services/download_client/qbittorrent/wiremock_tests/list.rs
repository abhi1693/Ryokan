//! `list_scoped` — JSON array decode, category query param,
//! state-string mapping at the trait boundary.

use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, ResponseTemplate};

use super::fixture::{new_fixture, new_with_category};
use crate::services::download_client::{DownloadClient, DownloadItemState};

fn seeded_torrents() -> serde_json::Value {
    serde_json::json!([
        {
            "hash": "a1",
            "name": "Show S01E01",
            "size": 2_000_000_000_i64,
            "progress": 1.0,
            "dlspeed": 0,
            "state": "stalledUP",
            "category": "ryokan-test",
            "eta": 8_640_000,
            "save_path": "/downloads",
            "content_path": "/downloads/Show S01E01"
        },
        {
            "hash": "b2",
            "name": "Show S01E02",
            "size": 1_500_000_000_i64,
            "progress": 0.42,
            "dlspeed": 1_500_000,
            "state": "downloading",
            "category": "ryokan-test",
            "eta": 42,
            "save_path": "/downloads",
            "content_path": "/downloads/Show S01E02"
        }
    ])
}

#[tokio::test]
async fn sends_category_as_query_param() {
    // The `category` filter is how `list_scoped` limits the response
    // to Ryokan-owned torrents — a missing or mistyped value would
    // return every torrent in the client, which post-processing
    // would then treat as lost and try to import.
    let (server, client) = new_with_category("my-custom-cat").await;
    Mock::given(method("GET"))
        .and(path("/api/v2/torrents/info"))
        .and(query_param("category", "my-custom-cat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(seeded_torrents()))
        .expect(1)
        .mount(&server)
        .await;
    let items = client.list_scoped().await.expect("list_scoped");
    assert_eq!(items.len(), 2);
}

#[tokio::test]
async fn parses_two_torrents_with_correct_fields() {
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/api/v2/torrents/info"))
        .respond_with(ResponseTemplate::new(200).set_body_json(seeded_torrents()))
        .mount(&server)
        .await;
    let items = client.list_scoped().await.expect("list_scoped");
    assert_eq!(items[0].hash, "a1");
    assert_eq!(items[0].name, "Show S01E01");
    assert_eq!(items[0].progress, 1.0);
    assert_eq!(items[1].hash, "b2");
    assert_eq!(items[1].progress, 0.42);
    assert_eq!(items[1].dlspeed, 1_500_000);
}

#[tokio::test]
async fn maps_state_strings_via_to_download_item() {
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/api/v2/torrents/info"))
        .respond_with(ResponseTemplate::new(200).set_body_json(seeded_torrents()))
        .mount(&server)
        .await;
    let items = client.list_scoped().await.expect("list_scoped");
    // stalledUP → normalized Seeding-family (complete). downloading
    // → Downloading (not complete).
    assert!(items[0].state_kind.is_complete());
    assert_eq!(items[1].state_kind, DownloadItemState::Downloading);
}

#[tokio::test]
async fn empty_response_returns_empty_vec() {
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/api/v2/torrents/info"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&server)
        .await;
    let items = client.list_scoped().await.expect("list_scoped");
    assert!(items.is_empty());
}

#[tokio::test]
async fn malformed_json_surfaces_parse_error() {
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/api/v2/torrents/info"))
        .respond_with(ResponseTemplate::new(200).set_body_string("not json at all"))
        .mount(&server)
        .await;
    let err = client.list_scoped().await.unwrap_err();
    assert!(
        err.to_lowercase().contains("parse") || err.to_lowercase().contains("torrents"),
        "unexpected error: {err}"
    );
}
