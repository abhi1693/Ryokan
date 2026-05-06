//! Payload + always-set-headers shape. Receivers depend on the
//! `Content-Type: application/json; charset=utf-8` and the
//! `X-Ryokan-{Delivery,Timestamp,Event}` envelope being present on
//! every send, signed or not.

use super::fixture::{make_provider, sample_event};
use crate::services::notifications::NotificationProvider;
use serde_json::Value;
use wiremock::matchers::{header, header_exists, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

#[tokio::test]
async fn happy_path_posts_canonical_event_json() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .and(header("Content-Type", "application/json; charset=utf-8"))
        .and(header("X-Ryokan-Event", "Grabbed"))
        .and(header_exists("X-Ryokan-Delivery"))
        .and(header_exists("X-Ryokan-Timestamp"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    let provider = make_provider(&server.uri());
    provider.send(&sample_event()).await.expect("send ok");
}

#[tokio::test]
async fn body_is_externally_tagged_event_json() {
    // Receivers (n8n, Apprise, ad-hoc shell scripts) parse against
    // the `{"kind": "...", "data": {...}}` shape from issue #118.
    // Pinned because a downstream serde feature flag flip
    // (internally tagged, untagged) would silently break every
    // configured receiver — pre-1.0 we eat the cost of a stable wire
    // contract test rather than a "why did all my Discord embeds
    // suddenly stop coloring correctly" bug report later.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let provider = make_provider(&server.uri());
    provider.send(&sample_event()).await.expect("send ok");

    let received = server.received_requests().await.expect("recordings");
    assert_eq!(received.len(), 1);
    let body: Value = serde_json::from_slice(&received[0].body).expect("body is JSON");
    assert_eq!(body["kind"], "Grabbed");
    assert_eq!(body["data"]["series_id"], 1);
    assert_eq!(body["data"]["episode_number"], 7);
    assert_eq!(body["data"]["indexer"], "nyaa");
    assert_eq!(body["data"]["score"], 150);
    assert_eq!(body["data"]["client_kind"], "qbittorrent");
}

#[tokio::test]
async fn no_signature_header_when_secret_is_unset() {
    // Pinned because a regression that always emitted an empty
    // signature header would let receivers think they were getting
    // signed deliveries (and skip verification on the unsigned
    // path). The header must be absent entirely when no secret is
    // configured.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let provider = make_provider(&server.uri());
    provider.send(&sample_event()).await.expect("send ok");
    let received = server.received_requests().await.expect("recordings");
    assert!(
        received[0]
            .headers
            .iter()
            .all(|(n, _)| !n.as_str().eq_ignore_ascii_case("x-ryokan-signature")),
        "X-Ryokan-Signature must be absent when secret is unset"
    );
}

#[tokio::test]
async fn delivery_id_is_unique_per_send() {
    // Receiver-side dedup relies on a fresh delivery id every send.
    // Pinned because the obvious "use creation_id as a salt" or
    // "memoize once per provider" foot-gun would defeat dedup on
    // every retry.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let provider = make_provider(&server.uri());
    provider.send(&sample_event()).await.expect("send 1");
    provider.send(&sample_event()).await.expect("send 2");
    let received = server.received_requests().await.expect("recordings");
    let id1 = header_value(&received[0], "X-Ryokan-Delivery");
    let id2 = header_value(&received[1], "X-Ryokan-Delivery");
    assert!(!id1.is_empty());
    assert_ne!(id1, id2, "delivery id must differ between sends");
}

fn header_value(req: &Request, name: &str) -> String {
    req.headers
        .iter()
        .find(|(n, _)| n.as_str().eq_ignore_ascii_case(name))
        .map(|(_, v)| v.to_str().unwrap_or("").to_string())
        .unwrap_or_default()
}
