//! `pause` / `resume` / `delete`. Two load-bearing qBit quirks
//! tested here:
//!
//!   * The 5.x rename of `/torrents/pause` → `/torrents/stop` and
//!     `/torrents/resume` → `/torrents/start`. Ryokan's impl tries
//!     the new name first and falls back to the old one on
//!     non-success — the tests assert both paths behave correctly.
//!   * The `deleteFiles` flag passes through to the form body so
//!     the caller can decide whether to keep data on disk (seed-
//!     preserving "blocklist this torrent" flows need deleteFiles
//!     = false).

use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, ResponseTemplate};

use super::fixture::new_fixture;
use crate::services::download_client::DownloadClient;

// ─── pause: stop-first, pause-fallback ─────────────────────────────

#[tokio::test]
async fn pause_tries_new_name_first_and_succeeds_on_200() {
    let (server, client) = new_fixture().await;
    // `/torrents/stop` is the new name — 200 means "we're on 5.x,
    // the rename took effect, don't touch the legacy endpoint."
    Mock::given(method("POST"))
        .and(path("/api/v2/torrents/stop"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v2/torrents/pause"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    client.pause("abc").await.expect("pause");
}

#[tokio::test]
async fn pause_falls_back_to_legacy_name_when_new_returns_non_success() {
    // 4.x behavior: `/torrents/stop` doesn't exist; qBit returns a
    // non-success status (typically 404). Impl falls back to the
    // legacy `/torrents/pause`.
    let (server, client) = new_fixture().await;
    Mock::given(method("POST"))
        .and(path("/api/v2/torrents/stop"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v2/torrents/pause"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    client.pause("abc").await.expect("pause");
}

#[tokio::test]
async fn pause_sends_hashes_form_body() {
    let (server, client) = new_fixture().await;
    Mock::given(method("POST"))
        .and(path("/api/v2/torrents/stop"))
        .and(body_string_contains("hashes=abc"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    client.pause("abc").await.expect("pause");
}

// ─── resume: start-first, resume-fallback ──────────────────────────

#[tokio::test]
async fn resume_tries_new_name_first_and_succeeds_on_200() {
    let (server, client) = new_fixture().await;
    Mock::given(method("POST"))
        .and(path("/api/v2/torrents/start"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v2/torrents/resume"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    client.resume("abc").await.expect("resume");
}

#[tokio::test]
async fn resume_falls_back_to_legacy_name_when_new_returns_non_success() {
    let (server, client) = new_fixture().await;
    Mock::given(method("POST"))
        .and(path("/api/v2/torrents/start"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v2/torrents/resume"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    client.resume("abc").await.expect("resume");
}

// ─── delete: deleteFiles pass-through ─────────────────────────────

#[tokio::test]
async fn delete_with_delete_files_true_sends_true_in_form() {
    let (server, client) = new_fixture().await;
    Mock::given(method("POST"))
        .and(path("/api/v2/torrents/delete"))
        .and(body_string_contains("deleteFiles=true"))
        .and(body_string_contains("hashes=abc"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    client.delete("abc", true).await.expect("delete");
}

#[tokio::test]
async fn delete_with_delete_files_false_preserves_data_flag() {
    // Blocklist flow sets deleteFiles=false so the user's on-disk
    // files stick around. Pinning the form value ensures a refactor
    // that defaults to "true" for simplicity would break the
    // blocklist contract.
    let (server, client) = new_fixture().await;
    Mock::given(method("POST"))
        .and(path("/api/v2/torrents/delete"))
        .and(body_string_contains("deleteFiles=false"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    client.delete("abc", false).await.expect("delete");
}

#[tokio::test]
async fn delete_surfaces_non_2xx_as_error() {
    let (server, client) = new_fixture().await;
    Mock::given(method("POST"))
        .and(path("/api/v2/torrents/delete"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let err = client.delete("abc", true).await.unwrap_err();
    assert!(err.to_lowercase().contains("delete") || err.contains("Failed"));
}
