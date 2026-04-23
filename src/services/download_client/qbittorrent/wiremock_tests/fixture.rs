//! Shared wiremock fixture for QbitClient tests. Parallel to
//! Sonarr's `DownloadClientFixtureBase<TSubject>` (setup method on
//! a base class) but shaped to Rust's factory-function style —
//! build the server + subject in one call, return them paired, and
//! let the test close over the `MockServer` handle to register
//! per-test expectations.
//!
//! Wiremock's `MockServer` automatically tears down on drop, so
//! individual tests don't need explicit cleanup.

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::services::download_client::qbittorrent::QbitClient;

/// Canned response body qBittorrent returns for a successful login.
pub(super) const LOGIN_OK_BODY: &str = "Ok.";

/// Pre-seed `/api/v2/auth/login` so `ensure_login` succeeds. Every
/// trait method calls `ensure_login` via `do_get` / `do_post_form`;
/// the alternative is each test registering its own login mock,
/// which adds ~3 lines of boilerplate to every body.
pub(super) async fn install_login_ok(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/api/v2/auth/login"))
        .respond_with(ResponseTemplate::new(200).set_body_string(LOGIN_OK_BODY))
        .mount(server)
        .await;
}

/// Spin up a new mock server and build a `QbitClient` pointing at
/// it. The login path is pre-wired so callers can register just the
/// endpoint under test. Category defaults to `"ryokan-test"`; tests
/// that care about the category string can override it via
/// [`new_with_category`].
pub(super) async fn new_fixture() -> (MockServer, QbitClient) {
    new_with_category("ryokan-test").await
}

pub(super) async fn new_with_category(category: &str) -> (MockServer, QbitClient) {
    let server = MockServer::start().await;
    install_login_ok(&server).await;
    let client = QbitClient::new(&server.uri(), "admin", "hunter2", category);
    (server, client)
}
