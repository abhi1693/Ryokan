//! `SabClient::test()` auto-creates the configured category in SAB
//! when it's missing. Without this, a user whose SAB doesn't have
//! the category Ryokan was configured with would see grabs land
//! cleanly in SAB but vanish from Ryokan's view (the `list_scoped`
//! filter drops them), with no actionable surface beyond a
//! debug-level "dropped every slot" log.
//!
//! These tests pin the auto-create flow at the wire boundary:
//!   * happy path — category missing → set_config → success message
//!   * already-present → no set_config call, plain version string
//!   * permission error on set_config → warning surfaced, but test() succeeds
//!   * empty configured category → no get_cats / set_config, plain version
//!   * get_cats parse failure → warning, test() still succeeds (the
//!     connection itself works; categories are a secondary concern).

use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, ResponseTemplate};

use super::fixture::{TEST_API_KEY, new_fixture, new_with_category};
use crate::services::download_client::DownloadClient;

/// Mount the standard version + queue auth probes used by every
/// `test()` call. Returns nothing because the test owns the server
/// handle for further per-test mounts (e.g. `get_cats`).
async fn mount_version_and_auth(server: &wiremock::MockServer) {
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "version"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "version": "4.5.5",
        })))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "queue"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"queue": {"slots": []}})),
        )
        .mount(server)
        .await;
}

#[tokio::test]
async fn test_creates_category_when_missing() {
    let (server, client) = new_with_category("anime").await;
    mount_version_and_auth(&server).await;

    // SAB doesn't know about "anime" — only the catch-all and a
    // pre-existing "tv" category.
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "get_cats"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "categories": ["*", "tv"],
        })))
        .mount(&server)
        .await;
    // ensure_category posts mode=set_config&keyword=anime&dir=anime;
    // pin all three so a refactor that drops one of them surfaces
    // here rather than at the user's "where did it go" report.
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "set_config"))
        .and(query_param("section", "categories"))
        .and(query_param("keyword", "anime"))
        .and(query_param("dir", "anime"))
        .and(query_param("apikey", TEST_API_KEY))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": true,
        })))
        .mount(&server)
        .await;

    let result = client
        .test()
        .await
        .expect("test() must succeed when ensure_category creates the missing cat");
    assert!(
        result.contains("4.5.5"),
        "version string must be present; got: {result}"
    );
    assert!(
        result.contains("created category 'anime'"),
        "the toast must surface the create action so the user knows SAB was mutated; got: {result}"
    );
}

#[tokio::test]
async fn test_skips_set_config_when_category_already_exists() {
    let (server, client) = new_with_category("anime").await;
    mount_version_and_auth(&server).await;

    // Category already there — case-insensitive match counts.
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "get_cats"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "categories": ["*", "Anime", "tv"],
        })))
        .mount(&server)
        .await;
    // No set_config mock — if ensure_category fires set_config when
    // the category already exists, the request hits wiremock's
    // no-match panic.

    let result = client
        .test()
        .await
        .expect("test() must succeed when category already exists");
    assert_eq!(
        result, "4.5.5",
        "no '(created category ...)' suffix when category was already present; got: {result}"
    );
}

#[tokio::test]
async fn test_no_op_when_configured_category_is_empty() {
    let (server, client) = new_with_category("").await;
    mount_version_and_auth(&server).await;

    // No get_cats mock — empty configured category short-circuits
    // ensure_category before any HTTP call, so a no-match panic on
    // get_cats would only fire if the short-circuit broke.

    let result = client
        .test()
        .await
        .expect("test() must succeed when no category is configured");
    assert_eq!(
        result, "4.5.5",
        "no category suffix when row has no category configured; got: {result}"
    );
}

#[tokio::test]
async fn test_surfaces_set_config_403_as_warning_not_fatal() {
    // The full SAB API key has set_config permission; the read-only
    // `nzb_api_key` does not, and SAB returns 401/403 when the
    // wrong key is used. The connection itself still works — queue
    // and addurl take the read-only key — so test() should not fail.
    // Surface the failure as a parenthesized warning so the user
    // sees actionable text on the toast.
    let (server, client) = new_with_category("anime").await;
    mount_version_and_auth(&server).await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "get_cats"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "categories": ["*"],
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "set_config"))
        .respond_with(ResponseTemplate::new(403).set_body_string("API Key Incorrect"))
        .mount(&server)
        .await;

    let result = client
        .test()
        .await
        .expect("test() must succeed even when set_config 403s — connection is fine");
    assert!(
        result.contains("4.5.5"),
        "version must be present; got: {result}"
    );
    assert!(
        result.contains("warning") && result.contains("nzb_key"),
        "warning must mention the nzb_key footgun so the user knows what to fix; got: {result}"
    );
}

#[tokio::test]
async fn test_surfaces_get_cats_failure_as_warning() {
    // get_cats failing is non-fatal for the same reason set_config
    // failure is — connection works, categories are secondary.
    let (server, client) = new_with_category("anime").await;
    mount_version_and_auth(&server).await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "get_cats"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let result = client
        .test()
        .await
        .expect("test() must succeed despite get_cats 500");
    assert!(result.contains("4.5.5"));
    assert!(
        result.contains("warning") && result.contains("get_cats"),
        "warning must identify which probe failed so the user knows where to look; got: {result}"
    );
}

#[tokio::test]
async fn test_treats_empty_categories_array_as_get_cats_failure() {
    // SAB always returns at least the catch-all `"*"` in the list,
    // so an empty array is malformed (most often a misbehaving proxy
    // that strips the apikey response body). Surface as a warning
    // rather than silently treating as "no categories defined."
    let (server, client) = new_with_category("anime").await;
    mount_version_and_auth(&server).await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "get_cats"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "categories": [],
        })))
        .mount(&server)
        .await;

    let result = client
        .test()
        .await
        .expect("test() must succeed even when get_cats returns malformed body");
    assert!(
        result.contains("warning"),
        "warning must surface; got: {result}"
    );
}

#[tokio::test]
async fn test_uses_configured_category_when_creating() {
    // The fixture's category is "ryokan-test"; ensure_category must
    // pass that exact value as both `keyword` and `dir`. Pinned
    // here separately from the `anime` test so a future rename of
    // the fixture's default doesn't silently break the contract.
    let (server, client) = new_fixture().await;
    mount_version_and_auth(&server).await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "get_cats"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "categories": ["*"],
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "set_config"))
        .and(query_param("section", "categories"))
        .and(query_param("keyword", "ryokan-test"))
        .and(query_param("dir", "ryokan-test"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": true,
        })))
        .mount(&server)
        .await;

    let result = client.test().await.expect("test() must succeed");
    assert!(result.contains("created category 'ryokan-test'"));
}

#[tokio::test]
async fn set_config_passes_both_keyword_and_name_for_cross_version_safety() {
    // SAB 5.x's API doc (live-probed against 5.0.1, 2026-05-02)
    // documents `name=…` as the category identifier; older 4.x
    // versions used `keyword=…`. The impl passes both to stay
    // compatible across versions; pinning both here so a refactor
    // that drops one silently breaks compatibility on the version
    // it doesn't support — surfaces here instead of in production.
    let (server, client) = new_with_category("anime").await;
    mount_version_and_auth(&server).await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "get_cats"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "categories": ["*"],
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "set_config"))
        .and(query_param("section", "categories"))
        // BOTH must be present — ordering inside `make_query` is
        // implementation-defined but each must individually match.
        .and(query_param("keyword", "anime"))
        .and(query_param("name", "anime"))
        .and(query_param("dir", "anime"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": true,
        })))
        .mount(&server)
        .await;

    let result = client.test().await.expect("test() must succeed");
    assert!(
        result.contains("created category 'anime'"),
        "create succeeded means SAB accepted the request with the keyword+name+dir trio; got: {result}"
    );
}

// ─── Add-path auto-create + defensive change_cat ──────────────────────

/// Helper for the add-path tests: mount the standard addurl mock
/// returning a fixed nzo_id. Tests register their own get_cats /
/// set_config / change_cat mocks on top.
async fn mount_addurl(server: &wiremock::MockServer, nzo_id: &str) {
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "addurl"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": true,
            "nzo_ids": [nzo_id],
        })))
        .mount(server)
        .await;
}

#[tokio::test]
async fn add_path_creates_category_when_missing_then_change_cats_the_job() {
    // The motivating scenario: user saves SAB row in Settings
    // without clicking Test, then fires their first grab. The
    // add path must (1) create the missing category, (2) issue
    // addurl, (3) defensively re-tag the just-added job — because
    // SAB's set_config-to-addurl race could leave the slot in the
    // default bucket if config reload didn't propagate before
    // the addurl call hit. Without (3), the very symptom we set
    // out to fix would still bite the first grab on a fresh setup.
    let (server, client) = new_with_category("anime").await;
    mount_addurl(&server, "SABnzbd_nzo_xyz").await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "get_cats"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "categories": ["*"],
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "set_config"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": true,
        })))
        .mount(&server)
        .await;
    // The defensive change_cat. If this mock doesn't fire, wiremock's
    // verify-on-drop catches it — pinning that the add path actually
    // issues change_cat after creating the category.
    let change_cat_mock = Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "change_cat"))
        .and(query_param("value", "SABnzbd_nzo_xyz"))
        .and(query_param("value2", "anime"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": true,
        })))
        .expect(1);
    server.register(change_cat_mock).await;

    use crate::services::download_client::AddOutcome;
    let (outcome, id) = client
        .add_torrent_returning_id("https://nzb.example/abc.nzb", "")
        .await
        .expect("add must succeed");
    assert_eq!(outcome, AddOutcome::Added);
    assert_eq!(id, "SABnzbd_nzo_xyz");
    // Wiremock validates `.expect(1)` on drop — explicit verify here
    // surfaces failure with a clearer test name.
    server.verify().await;
}

#[tokio::test]
async fn add_path_skips_change_cat_when_category_already_exists() {
    // The common case once the user is past their first grab:
    // category already in SAB → ensure_category returns Ok(false)
    // → add path proceeds with addurl, no change_cat needed.
    // Pinned so a refactor that always fires change_cat (which
    // would silently double-write the category SAB already
    // accepted from cat= on the addurl) gets caught.
    let (server, client) = new_with_category("anime").await;
    mount_addurl(&server, "SABnzbd_nzo_xyz").await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "get_cats"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "categories": ["*", "anime"],
        })))
        .mount(&server)
        .await;
    // change_cat must NOT fire — pinning that with `.expect(0)`.
    let change_cat_mock = Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "change_cat"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0);
    server.register(change_cat_mock).await;

    let _ = client
        .add_torrent_returning_id("https://nzb.example/abc.nzb", "")
        .await
        .expect("add must succeed");
    server.verify().await;
}

#[tokio::test]
async fn add_path_caches_ensure_so_second_add_skips_get_cats() {
    // First add probes get_cats (and creates). Second add must
    // skip the probe entirely — the per-client AtomicBool flag
    // means subsequent grabs don't pay the get_cats round-trip
    // cost. Pinned with `.expect(1)` on the get_cats mock and
    // two addurl calls.
    let (server, client) = new_with_category("anime").await;
    mount_addurl(&server, "SABnzbd_nzo_xyz").await;
    let get_cats_mock = Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "get_cats"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "categories": ["*", "anime"],
        })))
        .expect(1);
    server.register(get_cats_mock).await;

    let _ = client
        .add_torrent_returning_id("https://nzb.example/first.nzb", "")
        .await
        .expect("first add must succeed");
    let _ = client
        .add_torrent_returning_id("https://nzb.example/second.nzb", "")
        .await
        .expect("second add must succeed");
    server.verify().await;
}

#[tokio::test]
async fn add_path_swallows_change_cat_failure_so_grab_still_succeeds() {
    // change_cat HTTP failure must not propagate as an add error —
    // the add itself succeeded; a worst-case "this one grab is in
    // the default bucket" symptom is preferable to a hard add
    // failure that prevents the grab entirely. Verified by setting
    // the mock to 500 and asserting the add still returns Added.
    let (server, client) = new_with_category("anime").await;
    mount_addurl(&server, "SABnzbd_nzo_xyz").await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "get_cats"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "categories": ["*"],
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "set_config"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": true,
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "change_cat"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    use crate::services::download_client::AddOutcome;
    let (outcome, _id) = client
        .add_torrent_returning_id("https://nzb.example/abc.nzb", "")
        .await
        .expect("add must succeed even when change_cat 500s");
    assert_eq!(outcome, AddOutcome::Added);
}

#[tokio::test]
async fn add_path_swallows_get_cats_failure_so_grab_still_succeeds() {
    // get_cats HTTP failure on the add path must also not propagate
    // — same rationale. The add proceeds with the bare cat= on
    // addurl; if SAB happens to know the category, things work; if
    // not, the user sees the (now well-instrumented) "dropped every
    // slot" diagnostic and can fix it manually or via Test.
    let (server, client) = new_with_category("anime").await;
    mount_addurl(&server, "SABnzbd_nzo_xyz").await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "get_cats"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    // No change_cat mock — `.expect(0)` would also work, but
    // un-mounted mocks just return 404 which the add path also
    // swallows; the assertion is "add returns Added," not
    // "change_cat fired."

    use crate::services::download_client::AddOutcome;
    let (outcome, _id) = client
        .add_torrent_returning_id("https://nzb.example/abc.nzb", "")
        .await
        .expect("add must succeed even when get_cats 500s");
    assert_eq!(outcome, AddOutcome::Added);
}
