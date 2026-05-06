//! Discord wire-path outcomes — happy 204, 429 with Retry-After,
//! 5xx with truncated body. Payload-shape coverage lives in the
//! parent module's `discord.rs::tests` block (pure-function tests
//! over `build_payload` don't need a mock server).

use crate::services::notifications::NotificationProvider;
use crate::services::notifications::discord::DiscordProvider;
use sqlx::SqlitePool;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn make_provider(server: &MockServer) -> DiscordProvider {
    // Discord requires `discord.com` host on a real save, but the
    // test harness needs to point at the wiremock URI. Construct
    // via `new` (which skips validate_url) so we can target the
    // mock server path. Save-time validation is covered by the
    // `validate_url_*` tests in `discord.rs`.
    let webhook_url = format!("{}/api/webhooks/123/abc", server.uri());
    let pool = SqlitePool::connect_lazy("sqlite::memory:").expect("lazy pool");
    DiscordProvider::new(1, "test".into(), webhook_url, pool)
}

fn sample_event() -> crate::services::notifications::NotificationEvent {
    crate::services::notifications::NotificationEvent::Grabbed {
        series_id: 1,
        series_title: "Test".into(),
        episode_number: 7,
        release_title: "Test - 07 [WEB]".into(),
        indexer: Some("nyaa".into()),
        score: Some(125),
        client_kind: Some("qbittorrent".into()),
    }
}

#[tokio::test]
async fn discord_happy_path_returns_ok_on_204() {
    // Discord webhooks return 204 No Content on success.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/webhooks/123/abc"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    let provider = make_provider(&server).await;
    provider.send(&sample_event()).await.expect("send ok");
}

#[tokio::test]
async fn discord_429_includes_retry_after_in_error_message() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/webhooks/123/abc"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("Retry-After", "2")
                .set_body_string(r#"{"message":"You are being rate limited.","retry_after":2}"#),
        )
        .expect(1)
        .mount(&server)
        .await;
    let provider = make_provider(&server).await;
    let err = provider
        .send(&sample_event())
        .await
        .expect_err("429 must surface as Err");
    assert!(err.contains("429"), "got: {err}");
    assert!(
        err.contains("retry-after=2"),
        "Retry-After must be parsed and surfaced; got: {err}"
    );
}

#[tokio::test]
async fn discord_429_without_retry_after_still_returns_err() {
    // Discord may omit the header on the global rate limit; the
    // provider must still return Err with a sane retry-after fallback.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/webhooks/123/abc"))
        .respond_with(ResponseTemplate::new(429).set_body_string("rate limited"))
        .mount(&server)
        .await;
    let provider = make_provider(&server).await;
    let err = provider
        .send(&sample_event())
        .await
        .expect_err("429 must surface as Err");
    assert!(err.contains("429"), "got: {err}");
}

#[tokio::test]
async fn discord_5xx_returns_err_with_status_in_message() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/webhooks/123/abc"))
        .respond_with(ResponseTemplate::new(503).set_body_string("upstream down"))
        .mount(&server)
        .await;
    let provider = make_provider(&server).await;
    let err = provider
        .send(&sample_event())
        .await
        .expect_err("5xx must surface as Err");
    assert!(err.contains("503"), "got: {err}");
    assert!(err.contains("upstream down"), "got: {err}");
}

#[tokio::test]
async fn discord_4xx_with_huge_body_truncates_in_error_message() {
    // Discord typically returns small JSON error envelopes, but a
    // misconfigured proxy in front of the webhook could return tens
    // of KB of HTML. Pinned to defend against log-table blowup
    // through the `Notifications` log category.
    let server = MockServer::start().await;
    let huge = "X".repeat(50_000);
    Mock::given(method("POST"))
        .and(path("/api/webhooks/123/abc"))
        .respond_with(ResponseTemplate::new(400).set_body_string(huge))
        .mount(&server)
        .await;
    let provider = make_provider(&server).await;
    let err = provider
        .send(&sample_event())
        .await
        .expect_err("4xx must surface as Err");
    assert!(
        err.len() < 1_000,
        "error must be truncated; got {} chars",
        err.len()
    );
    assert!(err.contains('…'), "truncation marker must be present");
}
