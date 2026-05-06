//! Shared fixture for webhook wiremock tests. Spins up a fresh
//! `MockServer` per test (so test parallelism doesn't share state)
//! and builds a `WebhookProvider` pointed at it.

use crate::services::notifications::NotificationEvent;
use crate::services::notifications::webhook::{WebhookConfig, WebhookProvider};

pub(super) fn make_provider(uri: &str) -> WebhookProvider {
    WebhookProvider::new(
        1,
        "test".into(),
        WebhookConfig {
            url: format!("{uri}/hook"),
            secret: None,
            headers: Vec::new(),
        },
    )
}

pub(super) fn make_provider_with_secret(uri: &str, secret: &str) -> WebhookProvider {
    WebhookProvider::new(
        1,
        "test".into(),
        WebhookConfig {
            url: format!("{uri}/hook"),
            secret: Some(secret.to_string()),
            headers: Vec::new(),
        },
    )
}

pub(super) fn make_provider_with_headers(
    uri: &str,
    headers: Vec<(String, String)>,
) -> WebhookProvider {
    WebhookProvider::new(
        1,
        "test".into(),
        WebhookConfig {
            url: format!("{uri}/hook"),
            secret: None,
            headers,
        },
    )
}

pub(super) fn sample_event() -> NotificationEvent {
    NotificationEvent::Grabbed {
        series_id: 1,
        series_title: "Test Show".into(),
        episode_number: 7,
        release_title: "[Group] Test Show - 07 (1080p) [WEB].mkv".into(),
        indexer: Some("nyaa".into()),
        score: Some(150),
        client_kind: Some("qbittorrent".into()),
    }
}
