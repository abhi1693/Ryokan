//! Discord wire-path payload assertions. Verifies that the JSON
//! body received by the receiver matches the documented Discord
//! embed schema. Payload-shape unit tests in `discord.rs::tests`
//! cover the pure-function `build_payload` shape; this layer
//! catches regressions in the actual POST round-trip — e.g., a
//! reqwest version that started re-serializing JSON in a way that
//! altered key order, or a future `WEBHOOK_HTTP_CLIENT` setting
//! that accidentally added a body-modifying interceptor.

use crate::services::notifications::NotificationProvider;
use crate::services::notifications::discord::DiscordProvider;
use serde_json::Value;
use sqlx::SqlitePool;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn make_provider(server: &MockServer) -> DiscordProvider {
    let webhook_url = format!("{}/api/webhooks/123/abc", server.uri());
    let pool = SqlitePool::connect_lazy("sqlite::memory:").expect("lazy pool");
    DiscordProvider::new(1, "test".into(), webhook_url, pool)
}

fn sample_event() -> crate::services::notifications::NotificationEvent {
    crate::services::notifications::NotificationEvent::Grabbed {
        series_id: 1,
        series_title: "Mushoku Tensei".into(),
        episode_number: 7,
        release_title: "[Group] Mushoku Tensei - 07 [1080p].mkv".into(),
        indexer: Some("Nyaa".into()),
        score: Some(125),
        client_kind: Some("qbittorrent".into()),
    }
}

async fn capture_body(server: &MockServer, provider: &DiscordProvider) -> Value {
    Mock::given(method("POST"))
        .and(path("/api/webhooks/123/abc"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(server)
        .await;
    provider.send(&sample_event()).await.expect("send ok");
    let received = server.received_requests().await.expect("recordings");
    assert_eq!(received.len(), 1);
    serde_json::from_slice(&received[0].body).expect("body is JSON")
}

#[tokio::test]
async fn payload_carries_username_and_embed_envelope() {
    let server = MockServer::start().await;
    let provider = make_provider(&server).await;
    let body = capture_body(&server, &provider).await;
    assert_eq!(body["username"], "Ryokan");
    let embeds = body["embeds"].as_array().expect("embeds is array");
    assert_eq!(embeds.len(), 1, "exactly one embed per webhook");
    let e = &embeds[0];
    assert_eq!(e["title"], "Grabbed: Mushoku Tensei E07");
    assert_eq!(e["color"], 5_763_719_u32);
    assert!(e["fields"].is_array());
    assert!(
        e["footer"]["text"]
            .as_str()
            .unwrap()
            .starts_with("Ryokan v")
    );
    assert!(
        e["timestamp"].as_str().unwrap().contains('T'),
        "timestamp must be RFC3339"
    );
}

#[tokio::test]
async fn allowed_mentions_parse_is_empty_array_on_the_wire() {
    // Belt-and-braces: the unit test pins this in build_payload,
    // and this wire-level test pins that the post round-trip
    // doesn't introduce a transformation that strips it.
    let server = MockServer::start().await;
    let provider = make_provider(&server).await;
    let body = capture_body(&server, &provider).await;
    let am = &body["allowed_mentions"];
    assert!(am.is_object(), "allowed_mentions must be present");
    assert_eq!(
        am["parse"].as_array().map(|a| a.len()),
        Some(0),
        "parse must be []"
    );
}

#[tokio::test]
async fn release_title_with_at_everyone_does_not_change_allowed_mentions_on_wire() {
    // Parametrized regression check: malicious release-title
    // content must not affect the wire-level allowed_mentions.
    let cases = [
        "@everyone get this",
        "@here grab this",
        "<@&123456789> ping",
        "<@123456789> ping",
    ];
    for malicious in cases {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/webhooks/123/abc"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        let provider = make_provider(&server).await;
        let event = crate::services::notifications::NotificationEvent::Grabbed {
            series_id: 1,
            series_title: "Test".into(),
            episode_number: 1,
            release_title: malicious.into(),
            indexer: None,
            score: None,
            client_kind: None,
        };
        provider.send(&event).await.expect("send ok");
        let received = server.received_requests().await.expect("recordings");
        let body: Value = serde_json::from_slice(&received[0].body).expect("body");
        assert_eq!(
            body["allowed_mentions"]["parse"]
                .as_array()
                .map(|a| a.len()),
            Some(0),
            "parse must remain [] for malicious title {malicious:?}"
        );
    }
}

#[tokio::test]
async fn release_field_is_backtick_wrapped_on_wire() {
    let server = MockServer::start().await;
    let provider = make_provider(&server).await;
    let body = capture_body(&server, &provider).await;
    let release_field = body["embeds"][0]["fields"][0]["value"]
        .as_str()
        .expect("release field is string");
    assert!(release_field.starts_with('`') && release_field.ends_with('`'));
}
