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
    custom_formats::CompiledCfCache, download_client::DownloadClient, indexers::Indexer,
    interactive_search_cache::InteractiveSearchCache, jellyfin::JellyfinClient,
    oauth_state::OAuthStateStore, progress::ProgressRegistry, task_registry::TaskRegistry,
};

/// PR #107 review fix #4: cached `Vec<Arc<dyn Indexer>>` swapped on
/// `Settings → Indexers` edits. Mirrors [`CompiledCfCache`] —
/// outer `RwLock` owns swap; inner `Arc<Vec<_>>` is cheap-cloned
/// out on the search hot path so the read lock releases before the
/// per-query fan-out begins. Avoids rebuilding reqwest::Client
/// instances on every per-target search.
pub type IndexerCache = Arc<RwLock<Arc<Vec<Arc<dyn Indexer>>>>>;

/// Multi-client routing — pair of (id-keyed map of live trait
/// impls, default client id). Both swap atomically when the
/// cache is rebuilt by [`services::download_client::rebuild_clients_cache`]
/// on Settings → Connections → Downloads edits. Lookup at grab
/// time is a `HashMap::get` against the inner `Arc` — read lock
/// releases before the dispatch.
///
/// `default_id` is the row id of the `is_default = 1` row at
/// build time. `None` when no client is configured (fresh
/// install) or when every row was disabled. The pin-resolution
/// helpers ([`AppState::client_for_indexer`] etc.) fall back to
/// `default_id` when no pin matches.
#[derive(Default)]
pub struct DownloadClientPool {
    pub clients: std::collections::HashMap<i64, Arc<dyn DownloadClient>>,
    pub default_id: Option<i64>,
}

/// Same swap-on-write shape as `IndexerCache` / `CompiledCfCache`
/// — outer `RwLock` owns the swap; inner `Arc<DownloadClientPool>`
/// is cheap-cloned out on the grab-dispatch path so the read
/// lock releases before any HTTP calls.
pub type DownloadClientsCache = Arc<RwLock<Arc<DownloadClientPool>>>;

/// Shared application state available to all handlers. Lives in the
/// library crate (rather than `main.rs`) so integration tests can
/// build instances of it without depending on the binary.
#[derive(Clone)]
pub struct AppState {
    pub db: SqlitePool,
    /// Multi-client routing pool. Replaced the single-slot
    /// `download_client` field — see [`DownloadClientPool`].
    /// Rebuilt on Settings → Downloads add/edit/delete via
    /// `services::download_client::rebuild_clients_cache`. Pin
    /// resolution at grab time goes through
    /// [`AppState::client_for_indexer`].
    pub download_clients: DownloadClientsCache,
    pub jellyfin: Arc<RwLock<Option<JellyfinClient>>>,
    /// Compiled Custom Formats, loaded once at startup and rebuilt on
    /// CF create/update/delete via `custom_formats::rebuild_cf_cache`.
    /// Outer `RwLock` owns swap; the inner `Arc<Vec<_>>` is cheap-cloned
    /// out on the scoring hot path so the read lock releases before the
    /// per-candidate evaluation loop begins.
    pub custom_formats: CompiledCfCache,
    /// Cached indexer clients (PR #107 review fix #4). Same swap-
    /// on-write pattern as `custom_formats` — rebuilt by the
    /// Settings → Indexers handlers on add/edit/delete, and read
    /// lock-free via `Arc::clone` on the search path. Avoids
    /// rebuilding reqwest clients per search.
    pub indexers: IndexerCache,
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
    /// In-memory store for pending OAuth attempts (issue #62 PR A).
    /// Holds the PKCE verifier between MAL's `/start` and `/submit`;
    /// 10-minute TTL sweeps forgotten flows. See
    /// [`services::oauth_state`] for scope + lifecycle.
    pub oauth_state: OAuthStateStore,
    /// Wall-clock timestamp captured at process boot. Used by the
    /// Sonarr/Radarr shims' `system_status` endpoint so Seerr's UI
    /// pill reports the actual time the connected app came online —
    /// the prior hardcoded "2024-01-01T00:00:00Z" effectively claimed
    /// the indexer had been up for over a year regardless of when
    /// Ryokan was last restarted, which made the pill useless as a
    /// liveness signal.
    pub start_time: chrono::DateTime<chrono::Utc>,
    /// Lifecycle metadata for every supervised background task.
    /// Each `supervise()` loop registers itself here and updates
    /// status atomically on every iteration; `/api/system/tasks`
    /// reads the snapshot for the System page. See
    /// [`services::task_registry`] for the registry's threading
    /// model (lock-free hot path, snapshot-on-read).
    pub tasks: TaskRegistry,
}

impl AppState {
    /// Resolve a download client for a grab attributable to
    /// `indexer_id`. Pin chain:
    ///
    /// 1. The indexer row's `download_client_id`, if set.
    /// 2. The pool's default client id.
    /// 3. None — caller surfaces "no download client configured."
    ///
    /// Reads the indexer's pin from the `IndexerCache` snapshot
    /// (no DB roundtrip on the grab path). Falls through to
    /// the default when the pinned client id no longer exists
    /// (e.g. user deleted the client without re-pinning the
    /// indexer somehow — shouldn't happen because `delete()`
    /// NULLs the pin, but the fall-through keeps grabs flowing
    /// rather than 500ing).
    pub async fn client_for_indexer(
        &self,
        indexer_id: Option<i64>,
    ) -> Option<Arc<dyn DownloadClient>> {
        self.client_for_indexer_with_id(indexer_id)
            .await
            .map(|(c, _)| c)
    }

    /// Same resolution as [`Self::client_for_indexer`] but also
    /// returns the resolved `download_clients.id` so callers can
    /// stamp it on `grabbed_torrents.download_client_id`.
    /// Post-processing routes per-grab through that id back to the
    /// owning client.
    pub async fn client_for_indexer_with_id(
        &self,
        indexer_id: Option<i64>,
    ) -> Option<(Arc<dyn DownloadClient>, i64)> {
        let pool = self.download_clients.read().await.clone();
        if let Some(id) = indexer_id {
            let indexers = self.indexers.read().await.clone();
            if let Some(idx) = indexers.iter().find(|i| i.id() == id)
                && let Some(pinned) = idx.download_client_id()
                && let Some(client) = pool.clients.get(&pinned)
            {
                return Some((client.clone(), pinned));
            }
        }
        let default_id = pool.default_id?;
        let client = pool.clients.get(&default_id)?.clone();
        Some((client, default_id))
    }

    /// Resolve a download client for the built-in Nyaa search
    /// (no `indexers` row). Reads
    /// `config.nyaa_download_client_id` and falls back to the
    /// default. Caller must pass the current config so the
    /// helper doesn't fire a DB query per grab.
    pub async fn client_for_nyaa(&self, nyaa_pin: Option<i64>) -> Option<Arc<dyn DownloadClient>> {
        self.client_for_nyaa_with_id(nyaa_pin).await.map(|(c, _)| c)
    }

    /// Same resolution as [`Self::client_for_nyaa`] but also returns
    /// the resolved `download_clients.id` for grab-row stamping.
    pub async fn client_for_nyaa_with_id(
        &self,
        nyaa_pin: Option<i64>,
    ) -> Option<(Arc<dyn DownloadClient>, i64)> {
        let pool = self.download_clients.read().await.clone();
        if let Some(pinned) = nyaa_pin
            && let Some(client) = pool.clients.get(&pinned)
        {
            return Some((client.clone(), pinned));
        }
        let default_id = pool.default_id?;
        let client = pool.clients.get(&default_id)?.clone();
        Some((client, default_id))
    }

    /// Default client — used by paths that don't have an
    /// indexer / Nyaa pin context (post-processing on a grab
    /// whose indexer was deleted, manual grabs, etc.). Same
    /// resolution as the helpers above with a None pin: just
    /// the default.
    pub async fn default_download_client(&self) -> Option<Arc<dyn DownloadClient>> {
        self.default_download_client_with_id().await.map(|(c, _)| c)
    }

    /// Same resolution as [`Self::default_download_client`] but also
    /// returns the resolved id for grab-row stamping. Mirror of the
    /// `_with_id` helpers above.
    pub async fn default_download_client_with_id(&self) -> Option<(Arc<dyn DownloadClient>, i64)> {
        let pool = self.download_clients.read().await.clone();
        let default_id = pool.default_id?;
        let client = pool.clients.get(&default_id)?.clone();
        Some((client, default_id))
    }

    /// Look up a specific client by `download_clients.id`. Used by
    /// post-processing's per-grab routing — `grabbed_torrents.download_client_id`
    /// stamps the row, and this helper resolves it back to the live
    /// client. Returns None when the row referenced was deleted from
    /// the pool (e.g. user removed the client mid-import); caller
    /// should fall back to default.
    pub async fn client_by_id(&self, id: i64) -> Option<Arc<dyn DownloadClient>> {
        let pool = self.download_clients.read().await.clone();
        pool.clients.get(&id).cloned()
    }
}

impl FromRef<AppState> for SqlitePool {
    fn from_ref(state: &AppState) -> SqlitePool {
        state.db.clone()
    }
}
