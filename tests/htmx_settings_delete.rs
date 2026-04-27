//! HTMX migration Phase 1 tests (issue #129) — settings delete
//! handlers must support BOTH the HTMX swap path (empty 200 lets the
//! row form's `hx-target="closest tr" hx-swap="outerHTML"` remove
//! the row inline) AND the legacy form-POST path (redirect with flash
//! query params; preserves no-JS progressive enhancement).
//!
//! Each handler gets two test pairs: HxRequest(true) returns 200 with
//! an empty body; HxRequest(false) returns a 303 redirect to the
//! settings tab. Same delete operation under the hood; only the
//! response shape differs.
//!
//! Calls handlers directly (not via router) — same pattern as the
//! `protocol_guard` block in `src/handlers/settings/indexers.rs`.

use axum::extract::{Form, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum_htmx::HxRequest;
use sqlx::SqlitePool;

use ryokan::handlers::settings::custom_formats::{
    CustomFormatDeleteForm, settings_custom_formats_delete,
};
use ryokan::handlers::settings::download_clients::{
    DownloadClientIdForm, settings_download_clients_delete,
};
use ryokan::handlers::settings::indexers::{IndexerDeleteForm, settings_indexers_delete};
use ryokan::handlers::settings::{GroupDeleteForm, settings_groups_delete};
use ryokan::models::custom_formats as cf_model;
use ryokan::models::download_clients::{DownloadClientForm, insert as insert_dc};
use ryokan::models::group_source_map;
use ryokan::models::indexers::{IndexerForm, insert as insert_indexer};
use ryokan::services::source::Source;
use ryokan::test_support::{build_test_app_state, in_memory_pool};

fn extract_location(resp: &axum::response::Response) -> Option<String> {
    resp.headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

async fn seed_indexer(db: &SqlitePool) -> i64 {
    insert_indexer(
        db,
        IndexerForm {
            name: "TestIndex",
            kind: "torznab",
            url: "https://example.com/torznab",
            api_key: "abc",
            priority: 25,
            enabled: true,
            is_private_tracker: false,
            seed_ratio: None,
            seed_time_minutes: None,
            min_seeders: 1,
            request_timeout_secs: None,
            download_client_id: None,
            rss_enabled: false,
        },
    )
    .await
    .expect("seed indexer")
}

async fn seed_download_client(db: &SqlitePool) -> i64 {
    insert_dc(
        db,
        DownloadClientForm {
            name: "TestClient",
            kind: "qbittorrent",
            url: "http://qbit.local",
            username: "",
            password: "",
            label: "",
            download_path: "",
            enabled: true,
            is_default: false,
        },
    )
    .await
    .expect("seed download client")
}

async fn seed_custom_format(db: &SqlitePool) -> i64 {
    cf_model::insert(db, "TestCF", None, "{}", 0, "manual")
        .await
        .expect("seed custom format")
}

async fn seed_group(db: &SqlitePool) -> &'static str {
    let name = "TestGroup";
    group_source_map::upsert_user_edit(db, name, Source::BluRay, 1.0, "test seed")
        .await
        .expect("seed group");
    name
}

// ─── Indexers ──────────────────────────────────────────────────────────

#[tokio::test]
async fn indexers_delete_returns_empty_200_for_htmx_request() {
    let db = in_memory_pool().await;
    let id = seed_indexer(&db).await;
    let state = build_test_app_state(db.clone(), None);

    let resp = settings_indexers_delete(
        State(state.clone()),
        HxRequest(true),
        Form(IndexerDeleteForm { id }),
    )
    .await
    .into_response();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("read body");
    assert!(
        body.is_empty(),
        "HTMX delete must return empty body so hx-swap=outerHTML removes the row"
    );

    // Row actually deleted (sanity check the handler did the work).
    let remaining = ryokan::models::indexers::list_all(&state.db).await.unwrap();
    assert!(
        remaining.is_empty(),
        "indexer row must be gone after delete"
    );
}

#[tokio::test]
async fn indexers_delete_returns_redirect_for_non_htmx_request() {
    let db = in_memory_pool().await;
    let id = seed_indexer(&db).await;
    let state = build_test_app_state(db, None);

    let resp = settings_indexers_delete(
        State(state),
        HxRequest(false),
        Form(IndexerDeleteForm { id }),
    )
    .await
    .into_response();

    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let location = extract_location(&resp).unwrap_or_default();
    assert!(
        location.starts_with("/settings?tab=indexers"),
        "non-HTMX delete must redirect back to indexers tab; got: {location}"
    );
    assert!(
        location.contains("msg="),
        "non-HTMX redirect must include success flash; got: {location}"
    );
}

// ─── Download clients ──────────────────────────────────────────────────

#[tokio::test]
async fn download_clients_delete_returns_empty_200_for_htmx_request() {
    let db = in_memory_pool().await;
    let id = seed_download_client(&db).await;
    let state = build_test_app_state(db.clone(), None);

    let resp = settings_download_clients_delete(
        State(state.clone()),
        HxRequest(true),
        Form(DownloadClientIdForm { id }),
    )
    .await
    .into_response();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("read body");
    assert!(body.is_empty());
}

#[tokio::test]
async fn download_clients_delete_returns_redirect_for_non_htmx_request() {
    let db = in_memory_pool().await;
    let id = seed_download_client(&db).await;
    let state = build_test_app_state(db, None);

    let resp = settings_download_clients_delete(
        State(state),
        HxRequest(false),
        Form(DownloadClientIdForm { id }),
    )
    .await
    .into_response();

    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let location = extract_location(&resp).unwrap_or_default();
    assert!(
        location.starts_with("/settings?tab=integrations"),
        "non-HTMX delete must redirect back to integrations tab; got: {location}"
    );
}

// ─── Custom formats ────────────────────────────────────────────────────

#[tokio::test]
async fn custom_formats_delete_returns_empty_200_for_htmx_request() {
    let db = in_memory_pool().await;
    let id = seed_custom_format(&db).await;
    let state = build_test_app_state(db.clone(), None);

    let resp = settings_custom_formats_delete(
        State(state.clone()),
        HxRequest(true),
        Form(CustomFormatDeleteForm { id }),
    )
    .await
    .into_response();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("read body");
    assert!(body.is_empty());
}

#[tokio::test]
async fn custom_formats_delete_returns_redirect_for_non_htmx_request() {
    let db = in_memory_pool().await;
    let id = seed_custom_format(&db).await;
    let state = build_test_app_state(db, None);

    let resp = settings_custom_formats_delete(
        State(state),
        HxRequest(false),
        Form(CustomFormatDeleteForm { id }),
    )
    .await
    .into_response();

    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let location = extract_location(&resp).unwrap_or_default();
    assert!(
        location.contains("tab=custom_formats"),
        "non-HTMX delete must redirect back to custom_formats tab; got: {location}"
    );
}

// ─── Groups ────────────────────────────────────────────────────────────

#[tokio::test]
async fn groups_delete_returns_empty_200_for_htmx_request() {
    let db = in_memory_pool().await;
    let name = seed_group(&db).await;
    let state = build_test_app_state(db.clone(), None);

    let resp = settings_groups_delete(
        State(state.clone()),
        HxRequest(true),
        Form(GroupDeleteForm {
            group_name: name.to_string(),
        }),
    )
    .await
    .into_response();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("read body");
    assert!(body.is_empty());
}

#[tokio::test]
async fn groups_delete_returns_redirect_for_non_htmx_request() {
    let db = in_memory_pool().await;
    let name = seed_group(&db).await;
    let state = build_test_app_state(db, None);

    let resp = settings_groups_delete(
        State(state),
        HxRequest(false),
        Form(GroupDeleteForm {
            group_name: name.to_string(),
        }),
    )
    .await
    .into_response();

    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let location = extract_location(&resp).unwrap_or_default();
    assert!(
        location.starts_with("/settings?tab=groups"),
        "non-HTMX delete must redirect back to groups tab; got: {location}"
    );
}

#[tokio::test]
async fn groups_delete_with_empty_name_returns_400_for_htmx_request() {
    // Empty name is a "shouldn't happen" case (the row form has a
    // hidden input). For HTMX, surface as 400 so devtools shows the
    // bug; for non-HTMX, redirect-no-op preserves the legacy shape.
    let db = in_memory_pool().await;
    let state = build_test_app_state(db, None);

    let resp = settings_groups_delete(
        State(state),
        HxRequest(true),
        Form(GroupDeleteForm {
            group_name: "   ".to_string(),
        }),
    )
    .await
    .into_response();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}
