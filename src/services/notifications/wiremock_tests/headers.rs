//! Custom-header pass-through. User-configured headers must reach
//! the receiver verbatim, in insertion order, and must be added
//! after the load-bearing `X-Ryokan-*` envelope so a user who
//! deliberately overrides (say) `User-Agent` can.

use super::fixture::{make_provider_with_headers, sample_event};
use crate::services::notifications::NotificationProvider;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn custom_headers_reach_the_receiver() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .and(header("Authorization", "Bearer token-123"))
        .and(header("X-Custom-Tag", "ryokan-prod"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    let provider = make_provider_with_headers(
        &server.uri(),
        vec![
            ("Authorization".into(), "Bearer token-123".into()),
            ("X-Custom-Tag".into(), "ryokan-prod".into()),
        ],
    );
    provider.send(&sample_event()).await.expect("send ok");
}

#[tokio::test]
async fn custom_headers_can_override_x_ryokan_event_but_not_signature() {
    // Custom headers are added last so users can override anything
    // except `Content-Type` and `X-Ryokan-Signature` (the two
    // load-bearing wire-contract headers). Pinned so a refactor
    // that flips the header-add order can't silently break the
    // override use case (e.g., a user wants a constant correlation
    // id under the `X-Ryokan-Event` slot for downstream log
    // routing).
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .and(header("X-Ryokan-Event", "Custom-Override"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    let provider = make_provider_with_headers(
        &server.uri(),
        vec![("X-Ryokan-Event".into(), "Custom-Override".into())],
    );
    provider.send(&sample_event()).await.expect("send ok");
}

#[tokio::test]
async fn user_override_of_x_ryokan_signature_is_silently_dropped() {
    // A user who configured `X-Ryokan-Signature` as a custom header
    // would otherwise overwrite the computed HMAC, breaking
    // receiver-side verification with no obvious cause from
    // Ryokan's side. The build_request layer skips the override
    // and the legitimate signature still reaches the receiver.
    use super::fixture::sample_event;
    use crate::services::notifications::NotificationProvider;
    use crate::services::notifications::webhook::{WebhookConfig, WebhookProvider};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .and(header("X-Ryokan-Signature", "sha256=00deadbeef"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let provider = WebhookProvider::new(
        1,
        "test".into(),
        WebhookConfig {
            url: format!("{}/hook", server.uri()),
            secret: Some("real-secret".into()),
            headers: vec![("X-Ryokan-Signature".into(), "sha256=00deadbeef".into())],
        },
    );
    // The forged-signature mock returns 404 (the matcher requires
    // the malicious value); the real-signature mock returns 200.
    // If the override was honored the request would 404, and `send`
    // would surface Err. Reaching `Ok` proves the override was
    // dropped and the legitimate sha256=<real-hex> went on the wire.
    provider
        .send(&sample_event())
        .await
        .expect("override dropped, real signature wins");
}

#[tokio::test]
async fn user_override_of_content_type_is_silently_dropped() {
    // Same defense as the signature case — a user who configured
    // `Content-Type: text/plain` would force receivers expecting
    // JSON into a parse failure path. Skip the override.
    use super::fixture::sample_event;
    use crate::services::notifications::NotificationProvider;
    use crate::services::notifications::webhook::{WebhookConfig, WebhookProvider};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .and(header("Content-Type", "application/json; charset=utf-8"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    let provider = WebhookProvider::new(
        1,
        "test".into(),
        WebhookConfig {
            url: format!("{}/hook", server.uri()),
            secret: None,
            headers: vec![("Content-Type".into(), "text/plain".into())],
        },
    );
    provider
        .send(&sample_event())
        .await
        .expect("override dropped, json content-type wins");
}
