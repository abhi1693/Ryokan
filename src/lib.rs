//! Ryokan — library-crate root.
//!
//! This crate is consumed in two places:
//!
//!   1. **`src/main.rs`** — the binary entry point. Imports [`AppState`],
//!      the module tree, and boots the axum listener. Keeping `main.rs`
//!      thin lets the router and every handler live in the library so
//!      integration tests (under `tests/`) can exercise them without
//!      spawning a subprocess.
//!   2. **`tests/`** integration tests. Each file is its own crate that
//!      depends on `ryokan` as a library — they call [`handlers`],
//!      [`models`], etc. directly. Test-only helpers live in
//!      [`test_support`], gated behind the `test-support` Cargo feature
//!      so they don't leak into release binaries.
//!
//! The module tree is all `pub mod` so the binary and integration
//! tests see the same surface. Individual items inside each module
//! stay `pub(crate)` unless a caller outside the lib (i.e. a test)
//! needs them — keeping internal helpers private prevents accidental
//! API-stability obligations.

// Several enums (`Source`, `Resolution`, `WebKind`, `LogLevel`, etc.)
// expose `fn from_str(&str) -> Self` as an infallible coercion —
// unknown inputs fall back to a default variant rather than erroring.
// That shape matches Ryokan's `Result<_, String>` error convention
// (the few callers that care about the error already have the raw
// string) and predates the lib/bin split. The standard `FromStr`
// trait requires `Result<Self, Err>`, so implementing it would force
// every call site to handle a `Result` that by design can't fail.
// Silenced crate-wide rather than rewriting seven call sites; if a
// future variant of this method can actually fail, it should be
// named something else (e.g. `parse`) at that site.
#![allow(clippy::should_implement_trait)]

pub mod handlers;
pub mod models;
pub mod services;

/// Test scaffolding — in-memory pool builder, `AppState` assembler,
/// series/grab seeders. Always compiled during the library's own
/// `cargo test` via `cfg(test)`; externally visible only when the
/// `test-support` feature is enabled (integration tests in `tests/`
/// opt in through `[features] test-support = []` in Cargo.toml).
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

use std::sync::Arc;

use axum::extract::FromRef;
use sqlx::SqlitePool;
use tokio::sync::RwLock;

use services::{
    custom_formats::CompiledCfCache, download_client::DownloadClient,
    interactive_search_cache::InteractiveSearchCache, jellyfin::JellyfinClient,
    progress::ProgressRegistry,
};

/// Shared application state available to all handlers. Lives in the
/// library crate (rather than `main.rs`) so integration tests can
/// build instances of it without depending on the binary.
#[derive(Clone)]
pub struct AppState {
    pub db: SqlitePool,
    pub download_client: Arc<RwLock<Option<Arc<dyn DownloadClient>>>>,
    pub jellyfin: Arc<RwLock<Option<JellyfinClient>>>,
    /// Compiled Custom Formats, loaded once at startup and rebuilt on
    /// CF create/update/delete via `custom_formats::rebuild_cf_cache`.
    /// Outer `RwLock` owns swap; the inner `Arc<Vec<_>>` is cheap-cloned
    /// out on the scoring hot path so the read lock releases before the
    /// per-candidate evaluation loop begins.
    pub custom_formats: CompiledCfCache,
    /// In-memory progress registry for long-running user-triggered jobs
    /// (currently the manual auto-search). The frontend mints an opaque
    /// `progress_id`, the trigger handler binds it via
    /// `register(...).await`, and the polling endpoint at
    /// `/api/progress/{id}` drains buffered events. See
    /// `services::progress` for the full lifecycle.
    pub progress: ProgressRegistry,
    /// Flip-to-true-once cache of `user::has_users`. The auth middleware
    /// runs on every protected request and was firing a `SELECT COUNT(*)
    /// FROM users` query for each one just to decide whether to redirect
    /// to `/setup`. Because Ryokan never deletes the admin account, once
    /// this flag is true it stays true for the life of the process, and
    /// the check becomes a lock-free atomic load. While false, the
    /// middleware still hits the DB on the setup-pending path so a fresh
    /// `/setup` submission is picked up on the very next request.
    pub users_exist: Arc<std::sync::atomic::AtomicBool>,
    /// 5-minute TTL cache for interactive-search results so rapid
    /// reloads of the modal during UI iteration reuse the previous
    /// Nyaa hit. Scoped to interactive-search only; auto-search,
    /// RSS, and manual grabs continue to hit Nyaa directly. See
    /// [`services::interactive_search_cache`] for key shape + TTL.
    pub interactive_search_cache: InteractiveSearchCache,
}

impl FromRef<AppState> for SqlitePool {
    fn from_ref(state: &AppState) -> SqlitePool {
        state.db.clone()
    }
}
