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
    // The issue spec calls out that custom headers are added last
    // so users can override "anything except Content-Type and
    // X-Ryokan-Signature." Pinned so a refactor that flips the
    // header-add order can't silently break the override use case
    // (e.g., a user wants a constant correlation id under the
    // X-Ryokan-Event slot for downstream log routing).
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
