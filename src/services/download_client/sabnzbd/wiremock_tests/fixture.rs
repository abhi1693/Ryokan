//! Shared wiremock fixture for `SabClient` tests. Spin up a fresh
//! `MockServer` per test, build a client pointing at it, and let the
//! test close over the server handle to register per-test
//! expectations. SAB has no login handshake (auth is API-key on
//! every request), so the fixture is simpler than the BT clients'
//! login-pre-seed shape.
//!
//! Tests SHOULD use `query_param` matchers on the wiremock builder
//! — every SAB call is `GET /sabnzbd/api?mode=…&apikey=…`, and
//! matching on `mode` lets the same path serve different mocked
//! responses per call type (queue vs history vs addurl etc).

use wiremock::MockServer;

use crate::services::download_client::sabnzbd::SabClient;

pub(super) const TEST_API_KEY: &str = "test-api-key";

/// Build a fixture with category `"ryokan-test"`. Tests that care
/// about category-filtering specifically can use [`new_with_category`].
pub(super) async fn new_fixture() -> (MockServer, SabClient) {
    new_with_category("ryokan-test").await
}

pub(super) async fn new_with_category(category: &str) -> (MockServer, SabClient) {
    let server = MockServer::start().await;
    // Pass server.uri() verbatim — `SabClient::endpoint()` appends
    // `/sabnzbd/api` since the URI doesn't already end in `/sabnzbd`.
    let client = SabClient::new(&server.uri(), "", TEST_API_KEY, category);
    (server, client)
}
