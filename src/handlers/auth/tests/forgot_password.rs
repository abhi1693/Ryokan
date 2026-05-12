//! `/forgot-password` page coverage. The page is deliberately on
//! the unauthenticated route group (a locked-out user can't pass
//! `require_auth`, and that's the one page they need), and renders
//! a standalone template with no nav — confirmed by its content.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::routing::get;
use tower::ServiceExt;

use crate::handlers;
use crate::test_support::{build_test_app_state, in_memory_pool};

/// The shared `handler_router` helper only mounts `/login` and
/// `/setup` in the public group. This lightweight variant adds
/// `/forgot-password` so the recovery page can be exercised without
/// bloating the shared helper. A future change can promote this
/// route back into the shared `handler_router` if
/// more tests need it.
fn router_with_forgot_password(state: crate::AppState) -> axum::Router {
    use axum::Router;
    use axum::middleware;
    let public = Router::new()
        .route(
            "/forgot-password",
            get(handlers::auth::forgot_password_page),
        )
        .layer(middleware::from_fn(handlers::auth::csrf_public));
    Router::new().merge(public).with_state(state)
}

#[tokio::test]
async fn forgot_password_page_renders_without_auth() {
    let db = in_memory_pool().await;
    let state = build_test_app_state(db, None);
    let app = router_with_forgot_password(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/forgot-password")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        content_type.starts_with("text/html"),
        "forgot-password page should render HTML, got: {content_type}"
    );
}

#[tokio::test]
async fn forgot_password_page_mentions_recovery_recipe_markers() {
    let db = in_memory_pool().await;
    let state = build_test_app_state(db, None);
    let app = router_with_forgot_password(state);
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/forgot-password")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body = String::from_utf8_lossy(&body_bytes);
    // Two low-ceremony content assertions: the page must talk about
    // the reset sentinel filename (how the user triggers recovery)
    // and the `RYOKAN_RESET_AUTH` env var (the other trigger). If
    // future copy edits rename these, update both here and the
    // template together — these tests fence the template's most
    // load-bearing strings.
    assert!(
        body.contains(".reset-auth") || body.contains("reset-auth"),
        "forgot-password page should mention the reset-auth sentinel"
    );
    assert!(
        body.contains("RYOKAN_RESET_AUTH"),
        "forgot-password page should mention the RYOKAN_RESET_AUTH env var"
    );
}
