//! 429 + Retry-After handling. Prowlarr enforces per-indexer
//! rate limits and surfaces them as 429 with a Retry-After
//! header. Ryokan's client must capture both so the caller can
//! apply a cooldown matching the AniList rate-limit pattern.

use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

use super::fixture::new_fixture;
use crate::services::indexers::{Indexer, SearchQuery};

#[tokio::test]
async fn http_429_with_retry_after_surfaces_seconds_in_error() {
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "120")
                .set_body_string("Too Many Requests"),
        )
        .mount(&server)
        .await;

    let result = client.search(&SearchQuery::default()).await;
    let err = result.expect_err("must be Err");
    // Surface "rate-limited" + retry_after for log + cooldown.
    assert!(
        err.contains("rate-limited"),
        "must mention rate-limit: {err}"
    );
    assert!(
        err.contains("120"),
        "must surface retry_after seconds: {err}"
    );
}

#[tokio::test]
async fn http_429_without_retry_after_surfaces_unknown_retry() {
    // Some indexers return 429 without Retry-After. The client
    // must still surface it as a rate-limit error rather than a
    // generic HTTP failure, so the caller's cooldown logic can
    // pick a default.
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .respond_with(ResponseTemplate::new(429).set_body_string("Slow down"))
        .mount(&server)
        .await;

    let result = client.search(&SearchQuery::default()).await;
    let err = result.expect_err("must be Err");
    assert!(err.contains("rate-limited"), "must surface 429: {err}");
    // retry_after surfaces as `None` in the error message.
    assert!(err.contains("None"), "no retry-after → None: {err}");
}
