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
    let indexers: crate::IndexerCache = Arc::new(RwLock::new(Arc::new(Vec::new())));
    // Multi-client routing — pre-populate the pool with id=1 marked
    // as default if a client was supplied. Tests that exercise the
    // pin-resolution chain via specific ids should construct their
    // own pool; the simple "did the grab dispatch?" tests use this
    // shape and don't care about ids.
    let mut clients: std::collections::HashMap<i64, Arc<dyn DownloadClient>> =
        std::collections::HashMap::new();
    let mut default_torrent_id = None;
    if let Some(c) = download_client {
        clients.insert(1, c);
        // Test fixtures inject a torrent client by default — the
        // existing dyn DownloadClient mocks don't distinguish protocol
        // and every grab-flavored test case is torrent-shaped. A
        // future usenet-specific test fixture should pass its own
        // pre-built pool through `build_test_app_state_with_pool` (or
        // similar).
        default_torrent_id = Some(1);
    }
    let pool = crate::DownloadClientPool {
        clients,
        default_torrent_id,
        default_usenet_id: None,
    };
    let download_clients: crate::DownloadClientsCache = Arc::new(RwLock::new(Arc::new(pool)));
    AppState {
        db,
        download_clients,
        jellyfin: Arc::new(RwLock::new(None)),
        custom_formats: cf_cache,
        indexers,
        progress: ProgressRegistry::new(),
        users_exist: Arc::new(AtomicBool::new(true)),
        interactive_search_cache: crate::services::interactive_search_cache::new(),
        oauth_state: crate::services::oauth_state::new(),
        // A fixed start_time so snapshot tests of `system_status` get
        // a stable serialization. Real production paths use
        // `chrono::Utc::now()` at process boot.
        start_time: chrono::DateTime::<chrono::Utc>::from_timestamp(1_704_067_200, 0)
            .expect("epoch is valid"),
        tasks: crate::services::task_registry::TaskRegistry::new(),
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

// ─── Browser-e2e harness (issue #129 HTMX migration) ────────────────
//
// Gated on `cfg(feature = "browser-e2e")` so the inline test fixtures
// + ServeDir wiring don't pull `tower-http`'s `fs` feature into the
// graph for plain `cargo test --features test-support` runs. The
// `tower-http = ["fs"]` feature is already on for the production
// crate, so this gate is a coupling reminder: when the migration is
// complete and the e2e infra is removed, this whole module section
// goes too. See the `browser-e2e` feature comment in Cargo.toml.

#[cfg(feature = "browser-e2e")]
mod e2e {
    use super::*;
    use askama::Template;
    use axum::extract::Query;
    use axum::middleware;
    use axum::response::Html;
    use axum::routing::post;
    use serde::Deserialize;
    use tower_http::services::ServeDir;

    /// Fixture-page query: which series + episode to render the
    /// monitor button for. Both required so the test harness has to
    /// be explicit about what state it seeded.
    #[derive(Deserialize)]
    pub(crate) struct FixtureQuery {
        pub series_id: i64,
        pub episode_number: i32,
    }

    /// Fixture template — full HTML page with the htmx script tag
    /// and the monitor-button partial rendered for the requested
    /// series/episode. Reads the live `monitored` state from the DB
    /// so the test asserts on real state, not a hardcoded literal.
    #[derive(Template)]
    #[template(
        source = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>Browser-e2e fixture</title>
<script src="/static/vendor/htmx-2.0.9.min.js"></script>
</head>
<body>
<button class="ep-mon-btn {% if monitored %}ep-mon-yes{% else %}ep-mon-no{% endif %}"
    type="button"
    hx-post="/api/library/episode-monitoring"
    hx-vals='{"series_id": {{ series_id }}, "episode_number": {{ episode_number }}, "monitored": {% if monitored %}false{% else %}true{% endif %}}'
    hx-target="this"
    hx-swap="outerHTML">{% if monitored %}Yes{% else %}No{% endif %}</button>
</body>
</html>
"#,
        ext = "html"
    )]
    struct FixturePage {
        series_id: i64,
        episode_number: i32,
        monitored: bool,
    }

    /// Fixture page for Phase 1.5 grab-bag connection-test buttons
    /// (issue #129). Hard-codes one form per endpoint with a sibling
    /// result span — the test drives the click and asserts on the
    /// post-swap span text + color. Mirrors the production button
    /// shape from `templates/partials/settings/{integrations,download_clients}.html`
    /// closely enough that a divergence in `hx-include` semantics
    /// would be caught here.
    // Note: r##"..."## (not r#"..."#) — `hx-target="#foo"` contains
    // `"#` which would close a single-`#` raw string early.
    #[derive(Template)]
    #[template(
        source = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>Connection-test fixture</title>
<script src="/static/vendor/htmx-2.0.9.min.js"></script>
</head>
<body>
<form id="jellyfin-form">
    <input type="text" name="jellyfin_url" id="jellyfin_url" value="http://127.0.0.1:1">
    <input type="text" name="jellyfin_api_key" id="jellyfin_api_key" value="bogus">
    <button type="button" id="btn-jellyfin-test"
            hx-post="/api/jellyfin/test"
            hx-include="closest form"
            hx-target="#jellyfin-test-result"
            hx-swap="innerHTML"
            hx-disabled-elt="this">Test</button>
    <button type="button" id="btn-jellyfin-refresh"
            hx-post="/api/jellyfin/refresh"
            hx-target="#jellyfin-test-result"
            hx-swap="innerHTML"
            hx-disabled-elt="this">Refresh Library</button>
    <span id="jellyfin-test-result"></span>
</form>
<form id="dc-form">
    <input type="text" name="kind" value="qbittorrent">
    <input type="text" name="url" value="">
    <input type="text" name="username" value="">
    <input type="password" name="password" value="">
    <input type="text" name="label" value="">
    <button type="button" id="btn-dc-test"
            hx-post="/api/download-clients/test"
            hx-include="closest form"
            hx-target="next .dc-test-result"
            hx-swap="innerHTML"
            hx-disabled-elt="this">Test connection</button>
    <span class="dc-test-result"></span>
</form>
</body>
</html>
"##,
        ext = "html"
    )]
    struct ConnectionTestFixturePage;

    pub(crate) async fn connection_test_fixture()
    -> Result<Html<String>, (axum::http::StatusCode, String)> {
        let html = ConnectionTestFixturePage
            .render()
            .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        Ok(Html(html))
    }

    pub(crate) async fn fixture_page(
        axum::extract::State(state): axum::extract::State<AppState>,
        Query(q): Query<FixtureQuery>,
    ) -> Result<Html<String>, (axum::http::StatusCode, String)> {
        let monitored = crate::models::monitoring::get_series_states(&state.db, q.series_id)
            .await
            .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
            .into_iter()
            .find(|row| row.episode_number == q.episode_number)
            .map(|row| row.monitored)
            // Default to `true` so a missing row still renders a
            // pre-click "Yes" — the test seeds explicitly, so this
            // path is only hit on author error.
            .unwrap_or(true);
        let page = FixturePage {
            series_id: q.series_id,
            episode_number: q.episode_number,
            monitored,
        }
        .render()
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        Ok(Html(page))
    }

    /// Build the e2e router. Mounts only what the browser tests
    /// drive: `/static/*` for vendored htmx, `/login` for cookie
    /// preload, the fixture page, and the real episode-monitoring
    /// API behind `require_auth` so the cookie actually carries
    /// weight. New e2e fixtures get added here as the migration
    /// expands — keep this narrow, not "the whole app router."
    pub fn build(state: AppState) -> Router {
        // Static dir is repo-relative; tests can run from anywhere
        // (cargo flips CWD around) so resolve via CARGO_MANIFEST_DIR.
        let static_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("static");

        let public = Router::new().route(
            "/login",
            get(crate::handlers::auth::login_page).post(crate::handlers::auth::login_submit),
        );

        let protected = Router::new()
            .route("/__test/episode-monitor-fixture", get(fixture_page))
            .route(
                "/__test/connection-test-fixture",
                get(connection_test_fixture),
            )
            .route(
                "/api/library/episode-monitoring",
                post(crate::handlers::library::crud::set_episode_monitoring),
            )
            // ─── Phase 1.5 grab-bag (issue #129) ───────────────────
            .route(
                "/downloads",
                get(crate::handlers::downloads::downloads_page),
            )
            .route(
                "/api/downloads/blocklist/remove",
                post(crate::handlers::downloads::api_blocklist_remove),
            )
            .route(
                "/api/jellyfin/test",
                post(crate::handlers::settings::jellyfin_test),
            )
            .route(
                "/api/jellyfin/refresh",
                post(crate::handlers::settings::jellyfin_refresh),
            )
            .route(
                "/api/download-clients/test",
                post(crate::handlers::settings::download_clients::settings_download_clients_test),
            )
            // ─── Phase 1 settings-page surface ──────────────────────
            // Real production settings page + the four per-row delete
            // endpoints + the upsert/set-default routes the page form
            // posts to. Mounted here (rather than fixtures) so the
            // browser tests render the real templates and catch any
            // hx-attribute drift between the test scaffolding and
            // production.
            .route(
                "/settings",
                get(crate::handlers::settings::settings_page),
            )
            // Issue #129 Phase 1 completion — per-tab subform handlers
            // (`/settings/general`, `/settings/quality`,
            // `/settings/integrations`). Mounted here so the
            // browser-e2e tests at `htmx_browser_e2e_settings_subforms`
            // can POST through them; the production main.rs has the
            // same trio. Each handler reads `HxRequest` and returns
            // either the per-tab partial (HTMX) or the full
            // SettingsTemplate (no-JS).
            .route(
                "/settings/general",
                post(crate::handlers::settings::settings_general_submit),
            )
            .route(
                "/settings/quality",
                post(crate::handlers::settings::settings_quality_submit),
            )
            .route(
                "/settings/integrations",
                post(crate::handlers::settings::settings_integrations_submit),
            )
            .route(
                "/settings/indexers/delete",
                post(crate::handlers::settings::indexers::settings_indexers_delete),
            )
            .route(
                "/settings/indexers/upsert",
                post(crate::handlers::settings::indexers::settings_indexers_upsert),
            )
            .route(
                "/settings/indexers/section",
                axum::routing::get(crate::handlers::settings::indexers::settings_indexers_section),
            )
            .route(
                "/settings/indexers/add-form",
                axum::routing::get(crate::handlers::settings::indexers::settings_indexers_add_form),
            )
            .route(
                "/settings/indexers/{id}/edit-form",
                axum::routing::get(crate::handlers::settings::indexers::settings_indexers_edit_form),
            )
            .route(
                "/settings/download-clients/delete",
                post(crate::handlers::settings::download_clients::settings_download_clients_delete),
            )
            .route(
                "/settings/download-clients/set-default",
                post(crate::handlers::settings::download_clients::settings_download_clients_set_default),
            )
            .route(
                "/settings/custom-formats/delete",
                post(crate::handlers::settings::custom_formats::settings_custom_formats_delete),
            )
            .route(
                "/settings/groups/delete",
                post(crate::handlers::settings::settings_groups_delete),
            )
            .layer(middleware::from_fn_with_state(
                state.clone(),
                crate::handlers::auth::require_auth,
            ));

        Router::new()
            .merge(public)
            .merge(protected)
            .nest_service("/static", ServeDir::new(static_dir))
            .with_state(state)
    }
}

#[cfg(feature = "browser-e2e")]
pub use e2e::build as e2e_browser_app;

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

/// Issue #28 PR D — write `autobrr_api_key = <provided>` so the
/// webhook handler's auth check passes. Empty key means the
/// webhook is disabled (returns 503), so the test seed always
/// uses a non-empty value.
pub async fn seed_autobrr_enabled(db: &SqlitePool, api_key: &str) {
    let cfg = crate::models::config::Config {
        autobrr_api_key: api_key.to_string(),
        ..crate::models::config::Config::default()
    };
    crate::models::config::save_config(db, &cfg)
        .await
        .expect("persist autobrr-enabled config");
}

/// Issue #28 PR D — minimal router that mounts only the autobrr
/// webhook route, for tests that exercise the handler in
/// isolation without dragging in the rest of the protected
/// surface.
pub fn autobrr_webhook_router(state: AppState) -> Router {
    Router::new()
        .route(
            "/api/webhook/autobrr",
            axum::routing::post(crate::handlers::webhook::autobrr::webhook_autobrr),
        )
        .with_state(state)
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

/// Sonarr shim router that includes the resource-tier series + command
/// routes (`/series`, `/series/{id}`, `/series/lookup`, `/command`) on
/// top of [`sonarr_router`]'s system-tier surface. Kept as a separate
/// builder so tests that only exercise auth + system-tier shapes don't
/// have to load anibridge state, and so the series-tier tests
/// declare their dependency on those routes explicitly.
pub fn sonarr_router_with_series(state: AppState) -> Router {
    use axum::middleware;
    use axum::routing::{post, put};

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
        .route(
            "/api/v3/series",
            get(handlers::sonarr_compat::list_series).post(handlers::sonarr_compat::add_series),
        )
        .route(
            "/api/v3/series/lookup",
            get(handlers::sonarr_compat::series_lookup),
        )
        .route(
            "/api/v3/series/{id}",
            get(handlers::sonarr_compat::get_series),
        )
        .route(
            "/api/v3/series",
            put(handlers::sonarr_compat::update_series),
        )
        .route(
            "/api/v3/command",
            post(handlers::sonarr_compat::execute_command),
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

/// Radarr shim router that includes the resource-tier movie + command
/// routes (`/movie`, `/movie/{id}`, `/movie/lookup`, `/command`) on
/// top of [`radarr_router`]'s system-tier surface. Same separation
/// rationale as [`sonarr_router_with_series`].
pub fn radarr_router_with_movie(state: AppState) -> Router {
    use axum::middleware;
    use axum::routing::{post, put};

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
        .route(
            "/radarr/api/v3/movie",
            get(handlers::radarr_compat::list_movies).post(handlers::radarr_compat::add_movie),
        )
        .route(
            "/radarr/api/v3/movie/lookup",
            get(handlers::radarr_compat::movie_lookup),
        )
        .route(
            "/radarr/api/v3/movie/{id}",
            get(handlers::radarr_compat::get_movie),
        )
        .route(
            "/radarr/api/v3/movie",
            put(handlers::radarr_compat::update_movie),
        )
        .route(
            "/radarr/api/v3/command",
            post(handlers::radarr_compat::execute_command),
        )
        .layer(middleware::from_fn_with_state(
            state.clone(),
            handlers::radarr_compat::require_api_key,
        ));

    Router::new().merge(routes).with_state(state)
}
