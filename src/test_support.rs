//! Test-only scaffolding for integration-style tests that need a live
//! `AppState` with an in-memory database, and optionally a real
//! `DownloadClient` connection so assertions can verify end-to-end
//! state across the handler-to-client boundary.
//!
//! The Wave 1 smoke tests in `services::download_client::*::tests`
//! verify the `DownloadClient` trait contract in isolation. Wave 2
//! (`handlers::library::*::tests` and friends) asserts the chain from
//! a handler call → DB mutation → downstream `DownloadClient::delete`
//! happens correctly. For that we need an AppState with both a real
//! `SqlitePool` and a real `DownloadClient` — hence this module.
//!
//! Only compiled under `#[cfg(test)]`; adds no weight to release
//! binaries. Gated further (at each call site) behind the per-client
//! `RYOKAN_*_E2E` env vars that Wave 1 uses — tests that don't have
//! a live daemon available early-return without touching the DB.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use sqlx::SqlitePool;
use tokio::sync::RwLock;

use crate::AppState;
use crate::services::custom_formats::CompiledCfCache;
use crate::services::download_client::DownloadClient;
use crate::services::progress::ProgressRegistry;

/// Create a fresh in-memory SQLite pool with the full schema applied.
/// Each call gets its own isolated `:memory:` database so tests run
/// concurrently without cross-test contamination.
///
/// Applies the same `migrate()` call the production bootstrap runs —
/// tests exercise the real schema including every `ALTER TABLE ...
/// ADD COLUMN` the idempotent migration pattern lays down at startup.
pub(crate) async fn in_memory_pool() -> SqlitePool {
    let pool = SqlitePool::connect(":memory:")
        .await
        .expect("open :memory: SQLite");
    crate::models::migrations::migrate(&pool)
        .await
        .expect("run migrations");
    pool
}

/// Construct an `AppState` suitable for handler-level integration
/// testing. Accepts a pre-built pool (from [`in_memory_pool`]) and an
/// optional download-client slot — Wave 2 tests that verify
/// delete-on-removal etc. plug a real `Arc<dyn DownloadClient>` in;
/// tests that don't care pass `None`.
///
/// Defaults for the non-DB fields:
/// * `jellyfin` — `None` (Wave 2 doesn't exercise Jellyfin integration).
/// * `custom_formats` — empty Vec inside the usual `Arc<RwLock<_>>`.
///   Scoring paths that look up CFs will see the zero-CF fallback,
///   which is fine since Wave 2 focuses on library/download state
///   rather than scoring.
/// * `progress` — fresh `ProgressRegistry`. No jobs registered.
/// * `users_exist` — `true`. Bypasses the `/setup`-redirect path so
///   Wave 2 handler calls don't need to invent an authenticated
///   session for every fixture.
pub(crate) fn build_test_app_state(
    db: SqlitePool,
    download_client: Option<Arc<dyn DownloadClient>>,
) -> AppState {
    let cf_cache: CompiledCfCache = Arc::new(RwLock::new(Arc::new(Vec::new())));
    AppState {
        db,
        download_client: Arc::new(RwLock::new(download_client)),
        jellyfin: Arc::new(RwLock::new(None)),
        custom_formats: cf_cache,
        progress: ProgressRegistry::new(),
        users_exist: Arc::new(AtomicBool::new(true)),
    }
}

/// Seed one `series` row and return its auto-generated id. Fills in
/// the minimum required columns; callers pass `anilist_id` (the
/// unique external key) + `title`.
pub(crate) async fn seed_series(db: &SqlitePool, anilist_id: i64, title: &str) -> i64 {
    sqlx::query(
        "INSERT INTO series (anilist_id, title, title_romaji, folder_name) \
         VALUES (?, ?, ?, ?)",
    )
    .bind(anilist_id)
    .bind(title)
    .bind(title)
    .bind(title)
    .execute(db)
    .await
    .expect("seed series");
    sqlx::query_scalar::<_, i64>("SELECT id FROM series WHERE anilist_id = ?")
        .bind(anilist_id)
        .fetch_one(db)
        .await
        .expect("fetch seeded series id")
}

/// Seed one `grabbed_torrents` row with the given episode-numbers
/// list. Passing an empty slice defaults to `[1]` so simple single-
/// episode tests can skip the argument. Writes `state = 'pending'`;
/// callers that need `imported` / `failed` / `replaced` should
/// `UPDATE` the row after seeding. Returns the auto-generated id.
pub(crate) async fn seed_grabbed_torrent(
    db: &SqlitePool,
    series_id: i64,
    hash: &str,
    torrent_name: &str,
    episode_numbers: &[i32],
) -> i64 {
    // `grabbed_torrents.episode_numbers` is a JSON-encoded array
    // (matches the production schema). Serialize with serde_json so
    // we don't hand-build the string and accidentally quote numbers.
    let eps_default: Vec<i32> = vec![1];
    let eps = if episode_numbers.is_empty() {
        &eps_default[..]
    } else {
        episode_numbers
    };
    let eps_json = serde_json::to_string(eps).expect("serialize episode_numbers");
    sqlx::query(
        "INSERT INTO grabbed_torrents (series_id, hash, torrent_name, episode_numbers, state) \
         VALUES (?, ?, ?, ?, 'pending')",
    )
    .bind(series_id)
    .bind(hash)
    .bind(torrent_name)
    .bind(eps_json)
    .execute(db)
    .await
    .expect("seed grabbed_torrent");
    sqlx::query_scalar::<_, i64>("SELECT last_insert_rowid()")
        .fetch_one(db)
        .await
        .expect("fetch grab id")
}

/// Count `grabbed_torrents` rows for a given series — quick helper
/// for post-op assertions like "D1: after series remove, zero grab
/// rows remain."
pub(crate) async fn count_grabs_for_series(db: &SqlitePool, series_id: i64) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM grabbed_torrents WHERE series_id = ?")
        .bind(series_id)
        .fetch_one(db)
        .await
        .expect("count grabs")
}

/// Count rows in the `series` table. Helper for "series was deleted"
/// assertions.
pub(crate) async fn count_series(db: &SqlitePool) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM series")
        .fetch_one(db)
        .await
        .expect("count series")
}
