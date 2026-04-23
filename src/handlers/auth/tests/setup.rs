//! First-run gate coverage — the `/setup` page and `login_page`'s
//! redirect-to-setup behavior before any user exists. Runs against
//! the actual handlers through `handler_router` so the middleware
//! ordering (setup-vs-login redirect chain) is exercised end-to-end.
//!
//! Design invariant: Ryokan never deletes the admin account. Once
//! `users_exist` flips true the login page renders the form; until
//! then every authed path redirects to `/setup` and `/login` itself
//! forwards to `/setup` so a clean-install browser lands on the
//! account-creation form instead of a login form it can't submit.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use tower::ServiceExt;

use crate::test_support::{
    build_test_app_state, handler_router, in_memory_pool, logged_in_session,
};

/// Override the `users_exist` cache to false so the test reflects a
/// fresh-install state. The production cache flips true on the first
/// successful `has_users()` read; tests need to force it back to
/// false after calling `build_test_app_state` (which defaults it to
/// true so most tests don't have to invent an authenticated session).
fn fresh_install_state(db: sqlx::SqlitePool) -> crate::AppState {
    let mut state = build_test_app_state(db, None);
    state.users_exist = Arc::new(AtomicBool::new(false));
    state
}

// ─── /setup GET ────────────────────────────────────────────────────

#[tokio::test]
async fn setup_page_renders_before_any_user_exists() {
    let db = in_memory_pool().await;
    let state = fresh_install_state(db);
    let app = handler_router(state);
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/setup")
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
        "setup page should render HTML"
    );
}

#[tokio::test]
async fn setup_page_redirects_to_login_when_users_exist() {
    let db = in_memory_pool().await;
    // Seed a user so `has_users` returns true.
    let (state, _cookie) = logged_in_session(&db).await;
    let app = handler_router(state);
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/setup")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(matches!(
        response.status(),
        StatusCode::SEE_OTHER | StatusCode::FOUND
    ));
    let location = response
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert_eq!(location, "/login");
}

// ─── /login GET's pre-user redirect ───────────────────────────────

#[tokio::test]
async fn login_page_redirects_to_setup_before_any_user_exists() {
    let db = in_memory_pool().await;
    let state = fresh_install_state(db);
    let app = handler_router(state);
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/login")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(matches!(
        response.status(),
        StatusCode::SEE_OTHER | StatusCode::FOUND
    ));
    let location = response
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert_eq!(location, "/setup");
}

#[tokio::test]
async fn login_page_renders_when_users_exist() {
    let db = in_memory_pool().await;
    let (state, _cookie) = logged_in_session(&db).await;
    let app = handler_router(state);
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/login")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

// ─── users_exist atomic cache ─────────────────────────────────────

#[tokio::test]
async fn users_exist_atomic_promotes_on_first_protected_request() {
    // Fresh-install (users_exist=false) state, but seed a user
    // behind the cache so has_users() returns true when the
    // middleware consults the DB. The middleware should promote the
    // cache to true so subsequent calls skip the SELECT.
    let db = in_memory_pool().await;
    let state = fresh_install_state(db.clone());
    // Pre-create a user (bypassing `logged_in_session` because we
    // want the cache in its stale=false state and no cookie).
    crate::models::user::create_user(&db, "cache-test", "hunter2-cache-test")
        .await
        .unwrap();
    assert!(
        !state.users_exist.load(Ordering::Relaxed),
        "cache should start false"
    );

    // Hit a protected endpoint. The middleware sees users_exist=false,
    // falls through to the DB query which reports true, promotes the
    // cache, then falls through to the session check which redirects
    // the unauthenticated caller to /login (not /setup).
    let app = handler_router(state.clone());
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let location = response
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert_eq!(location, "/login", "should redirect to /login, not /setup");
    assert!(
        state.users_exist.load(Ordering::Relaxed),
        "cache should be promoted to true after successful has_users read"
    );
}
