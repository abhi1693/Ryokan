//! Test-only scaffolding for integration-style tests.
//!
//! Shared between:
//!   * **Unit tests inside the library crate** (`#[cfg(test)] mod tests`
//!     in any `src/**/*.rs`) — picked up automatically via the
//!     `#[cfg(test)]` gate in `lib.rs`.
//!   * **Integration tests under `tests/`** — each file is its own
//!     binary crate that imports `ryokan` as a dep. They see this
//!     module through the `test-support` feature flag (also declared
//!     in `lib.rs`). Enable with `cargo test --features test-support`
//!     when running from the CLI; CI's `rust.yml` adds the flag on the
//!     test step.
//!
//! None of the helpers touch the network, spawn live daemons, or read
//! the real filesystem beyond what `tempfile` provides. Env-gated
//! `live_smoke` tests (see `services/download_client/*.rs`) are a
//! separate concern and don't go through this module.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use axum::Router;
use axum::routing::get;
use sqlx::SqlitePool;
use tokio::sync::RwLock;

use crate::AppState;
use crate::handlers;
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
pub async fn in_memory_pool() -> SqlitePool {
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
pub fn build_test_app_state(
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
        interactive_search_cache: crate::services::interactive_search_cache::new(),
    }
}

/// Seed one `series` row and return its auto-generated id. Fills in
/// the minimum required columns; callers pass `anilist_id` (the
/// unique external key) + `title`.
pub async fn seed_series(db: &SqlitePool, anilist_id: i64, title: &str) -> i64 {
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
pub async fn seed_grabbed_torrent(
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
pub async fn count_grabs_for_series(db: &SqlitePool, series_id: i64) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM grabbed_torrents WHERE series_id = ?")
        .bind(series_id)
        .fetch_one(db)
        .await
        .expect("count grabs")
}

/// Count rows in the `series` table. Helper for "series was deleted"
/// assertions.
pub async fn count_series(db: &SqlitePool) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM series")
        .fetch_one(db)
        .await
        .expect("count series")
}

// ─── PR 0 additions (test-coverage-expansion foundation) ──────────

/// Create a user + session and return the pair of (state, cookie
/// header value) so a test can make authenticated requests via
/// `axum::Router::oneshot`. The returned cookie is ready to drop
/// into a `Cookie: ` header — includes the `session=<hex>` form
/// the production `require_auth` middleware expects.
///
/// The session is written directly via `models::session::create_session`
/// rather than going through `/login` so the helper doesn't depend
/// on the login-throttle state or CSRF-origin middleware being
/// configured — authenticated-endpoint tests that care about those
/// layers should drive `/login` themselves.
pub async fn logged_in_session(db: &SqlitePool) -> (AppState, String) {
    let user_id = crate::models::user::create_user(db, "test-user", "hunter2-test-password")
        .await
        .expect("create test user");
    let token = crate::models::session::create_session(db, user_id)
        .await
        .expect("create session");
    let state = build_test_app_state(db.clone(), None);
    (state, format!("session={}", token))
}

/// Build a minimal axum router that mounts the handlers most PR 0
/// pilot + PR 1+ auth tests want to exercise. Deliberately narrower
/// than `main.rs`'s full `app` router so tests don't pay for every
/// middleware layer — adds routes it needs and leaves the rest for
/// later PRs to extend.
///
/// Currently includes:
/// * `GET /login` — public login page render
/// * `POST /login` — public login submit (wrapped in `csrf_public`)
/// * `GET /setup` — public setup page render
/// * `POST /setup` — public setup submit (wrapped in `csrf_public`)
/// * `GET /api/health` — protected health check (wrapped in `require_auth`)
///
/// Later test waves extend this helper — add new routes to the
/// matching route group (public vs protected) and re-merge.
pub fn handler_router(state: AppState) -> Router {
    use axum::middleware;

    let public_routes = Router::new()
        .route(
            "/login",
            get(handlers::auth::login_page).post(handlers::auth::login_submit),
        )
        .route(
            "/setup",
            get(handlers::auth::setup_page).post(handlers::auth::setup_submit),
        )
        .layer(middleware::from_fn(handlers::auth::csrf_public));

    let protected_routes = Router::new()
        .route("/api/health", get(handlers::settings::api_health))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            handlers::auth::require_auth,
        ));

    Router::new()
        .merge(public_routes)
        .merge(protected_routes)
        .with_state(state)
}

/// Persist a `Config` row with the Sonarr shim enabled and a known
/// API key, so arr-compat tests can exercise authenticated paths
/// without round-tripping through `handlers::settings::save_settings`.
/// Writes `sonarr_enabled = 1` and `sonarr_api_key = <provided>` on
/// top of an otherwise-default row so the shim's
/// `require_api_key` middleware passes through.
pub async fn seed_sonarr_enabled(db: &SqlitePool, api_key: &str) {
    let cfg = crate::models::config::Config {
        sonarr_enabled: true,
        sonarr_api_key: api_key.to_string(),
        ..crate::models::config::Config::default()
    };
    crate::models::config::save_config(db, &cfg)
        .await
        .expect("persist Sonarr-enabled config");
}

/// Parallel to [`seed_sonarr_enabled`] for the Radarr shim — writes
/// `radarr_enabled = 1` and `radarr_api_key = <provided>` on top of
/// an otherwise-default row.
pub async fn seed_radarr_enabled(db: &SqlitePool, api_key: &str) {
    let cfg = crate::models::config::Config {
        radarr_enabled: true,
        radarr_api_key: api_key.to_string(),
        ..crate::models::config::Config::default()
    };
    crate::models::config::save_config(db, &cfg)
        .await
        .expect("persist Radarr-enabled config");
}

/// Build a minimal Sonarr shim router: mounts the handful of system-
/// tier endpoints under `/api/v3/*` behind the real `require_api_key`
/// middleware. Deliberately narrower than `main.rs`'s full
/// `sonarr_routes` so tests that just want to exercise auth +
/// response shape don't have to carry the full series surface.
pub fn sonarr_router(state: AppState) -> Router {
    use axum::middleware;

    let routes = Router::new()
        .route(
            "/api/v3/system/status",
            get(handlers::sonarr_compat::system_status),
        )
        .route(
            "/api/v3/qualityprofile",
            get(handlers::sonarr_compat::quality_profiles),
        )
        .route(
            "/api/v3/qualityProfile",
            get(handlers::sonarr_compat::quality_profiles),
        )
        .route(
            "/api/v3/rootfolder",
            get(handlers::sonarr_compat::root_folders),
        )
        .route(
            "/api/v3/languageprofile",
            get(handlers::sonarr_compat::language_profiles),
        )
        .route("/api/v3/tag", get(handlers::sonarr_compat::list_tags))
        .route(
            "/api/v3/downloadclient",
            get(handlers::sonarr_compat::list_download_clients),
        )
        .layer(middleware::from_fn_with_state(
            state.clone(),
            handlers::sonarr_compat::require_api_key,
        ));

    Router::new().merge(routes).with_state(state)
}

/// Build a minimal Radarr shim router (parallel to [`sonarr_router`]).
/// Routes are mounted under `/radarr/api/v3/*` to match the
/// production prefix that lets both shims coexist behind Seerr's
/// two-slot-per-kind layout.
pub fn radarr_router(state: AppState) -> Router {
    use axum::middleware;

    let routes = Router::new()
        .route(
            "/radarr/api/v3/system/status",
            get(handlers::radarr_compat::system_status),
        )
        .route(
            "/radarr/api/v3/qualityprofile",
            get(handlers::radarr_compat::quality_profiles),
        )
        .route(
            "/radarr/api/v3/qualityProfile",
            get(handlers::radarr_compat::quality_profiles),
        )
        .route(
            "/radarr/api/v3/rootfolder",
            get(handlers::radarr_compat::root_folders),
        )
        .route(
            "/radarr/api/v3/tag",
            get(handlers::radarr_compat::list_tags),
        )
        .route(
            "/radarr/api/v3/downloadclient",
            get(handlers::radarr_compat::list_download_clients),
        )
        .layer(middleware::from_fn_with_state(
            state.clone(),
            handlers::radarr_compat::require_api_key,
        ));

    Router::new().merge(routes).with_state(state)
}
