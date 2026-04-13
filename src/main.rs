mod handlers;
mod models;
mod services;

use axum::{
    extract::FromRef,
    middleware,
    routing::{get, post},
    Router,
};
use sqlx::SqlitePool;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::RwLock;
use tower_http::services::ServeDir;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

use services::{jellyfin::JellyfinClient, qbit::QbitClient};

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Ryokan API",
        version = "0.1.0",
        description = "Self-hosted anime PVR — search, download, and manage your anime library.",
    ),
    paths(
        // Library
        handlers::library::anilist_search,
        handlers::library::api_series_detail,
        handlers::library::add_series,
        handlers::library::remove_series,
        handlers::library::reconcile_fallbacks,
        handlers::library::set_folder,
        handlers::library::set_monitoring,
        handlers::library::set_episode_monitoring,
        handlers::library::set_allow_upgrades,
        handlers::library::set_manual_override,
        handlers::library::list_folders,
        handlers::library::auto_search_series,
        handlers::library::auto_search_episode,
        handlers::library::search_batch_releases,
        handlers::library::interactive_search_episode,
        handlers::library::grab_interactive_result,
        handlers::library::delete_episode_file,
        handlers::library::get_episode_grab_history,
        handlers::library::mark_episode_failed,
        handlers::library::episode_download_progress,
        handlers::library::series_episodes_json,
        // Search
        handlers::search::search_page_api,
        handlers::search::grab_release,
        handlers::search::get_torrents,
        // Downloads
        handlers::downloads::api_pause_torrent,
        handlers::downloads::api_resume_torrent,
        handlers::downloads::api_delete_torrent,
        handlers::downloads::api_blocklist_remove,
        // System
        handlers::settings::api_health,
        handlers::settings::qbit_test,
        handlers::settings::jellyfin_test,
        handlers::settings::jellyfin_refresh,
        handlers::system::api_logs_poll,
        handlers::system::api_logs_clear,
        handlers::system::api_rss_sync,
        handlers::system::api_rss_clear_history,
        handlers::system::api_force_metadata_refresh,
        handlers::system::api_force_cleanup,
        handlers::system::api_force_post_processing,
        handlers::system::api_force_library_classify,
        handlers::system::api_force_upgrade_search,
        handlers::system::api_rebuild_cached_metadata,
        handlers::system::api_anibridge_reload,
    ),
    components(schemas(
        services::anilist::AnimeEntry,
        services::anilist::AnimeDetail,
        services::anilist::RelatedEntry,
        services::anilist::StreamingEpisode,
        services::nyaa::SearchResult,
        services::nyaa::SearchResponse,
        services::qbit::Torrent,
        services::auto_search::AutoSearchReport,
        services::auto_search::AutoSearchHit,
        models::log::LogEntry,
        models::episode_tags::GrabHistoryEntry,
        handlers::library::AddSeriesForm,
        handlers::library::RemoveSeriesForm,
        handlers::library::SetFolderForm,
        handlers::library::SetMonitoringForm,
        handlers::library::SetEpisodeMonitoringForm,
        handlers::library::SetAllowUpgradesForm,
        handlers::library::SetManualOverrideForm,
        handlers::library::MarkEpisodeFailedForm,
        handlers::library::EpisodeProgress,
        handlers::search::GrabForm,
        handlers::downloads::TorrentActionForm,
        handlers::downloads::TorrentDeleteForm,
        handlers::downloads::BlocklistRemoveForm,
        handlers::settings::QbitTestForm,
        handlers::settings::JellyfinTestForm,
    )),
    tags(
        (name = "Library", description = "Anime library management — add, remove, search, and monitor series"),
        (name = "Search", description = "Nyaa torrent search and grabbing"),
        (name = "Downloads", description = "qBittorrent download management"),
        (name = "System", description = "Health checks, logs, RSS sync, and background tasks"),
    ),
)]
struct ApiDoc;

/// Shared application state available to all handlers.
#[derive(Clone)]
pub struct AppState {
    pub db: SqlitePool,
    pub qbit: Arc<RwLock<Option<QbitClient>>>,
    pub jellyfin: Arc<RwLock<Option<JellyfinClient>>>,
}

// Allow handlers to extract SqlitePool directly from AppState.
impl FromRef<AppState> for SqlitePool {
    fn from_ref(state: &AppState) -> SqlitePool {
        state.db.clone()
    }
}

/// Run a supervising loop around a background tick future.
///
/// `make_fut` is called once per respawn. The returned future is run on
/// its own nested `tokio::spawn`, so if the inner task panics tokio
/// catches the unwind at the task boundary and surfaces it as a
/// `JoinError` — we log it, sleep briefly, and respawn. Without this
/// supervising layer a stray `.unwrap()` or overflow inside any one
/// background task would silently kill the task for the rest of the
/// process lifetime, leaving the operator with a "task X stopped firing
/// three days ago" mystery bug.
///
/// `name` is used purely in the log line so the operator can tell which
/// task misbehaved.
async fn supervise<F, Fut>(name: &'static str, mut make_fut: F) -> !
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    loop {
        let handle = tokio::spawn(make_fut());
        match handle.await {
            Err(e) if e.is_panic() => {
                tracing::error!(
                    "Background task '{}' panicked, restarting in 5s: {:?}",
                    name,
                    e
                );
            }
            Err(e) => {
                tracing::error!(
                    "Background task '{}' join error, restarting in 5s: {:?}",
                    name,
                    e
                );
            }
            Ok(()) => {
                tracing::warn!("Background task '{}' exited normally, restarting", name);
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
}

#[tokio::main]
async fn main() {
    // Initialize tracing.
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| "ryokan=debug,tower_http=debug".into()))
        .with(tracing_subscriber::fmt::layer())
        .init();

    // Database setup.
    // For local `cargo run`, default to a project-local ./data directory. Docker can
    // still override this with DATABASE_URL=sqlite:///data/ryokan.db?mode=rwc.
    let database_url = std::env::var("DATABASE_URL").unwrap_or_else(|_| {
        let _ = std::fs::create_dir_all("data");
        "sqlite://data/ryokan.db?mode=rwc".to_string()
    });

    let db = SqlitePool::connect(&database_url)
        .await
        .expect("Failed to connect to database");

    // Run migrations.
    models::migrate(&db).await.expect("Failed to run migrations");

    // Warm the bcrypt dummy-hash LazyLock so the first failed-username login
    // probe doesn't pay a cold-start ~50ms extra on top of the normal bcrypt
    // cost — that extra delay on the very first probe would itself be a
    // timing side channel distinguishing "cold process" from "warm process".
    // Run it in a blocking task so the ~50ms bcrypt::hash doesn't stall the
    // runtime worker during startup.
    let _ = tokio::task::spawn_blocking(models::user::warm_timing_equalizer).await;

    // Build shared state.
    let state = AppState {
        db: db.clone(),
        qbit: Arc::new(RwLock::new(None)),
        jellyfin: Arc::new(RwLock::new(None)),
    };

    // Initialize qBittorrent client from saved config if available.
    if let Ok(Some(config)) = models::config::get_config(&db).await {
        if !config.qbit_url.is_empty() {
            let client = QbitClient::new(
                &config.qbit_url,
                &config.qbit_user,
                &config.qbit_pass,
                &config.qbit_category,
            );
            *state.qbit.write().await = Some(client);
        }
        if !config.jellyfin_url.is_empty() && !config.jellyfin_api_key.is_empty() {
            let client = JellyfinClient::new(
                &config.jellyfin_url,
                &config.jellyfin_api_key,
            );
            *state.jellyfin.write().await = Some(client);
        }
    }

    // Routes that don't require auth. The CSRF layer applies to POSTs here
    // so a drive-by cross-origin /setup or /login submission is rejected
    // before touching the handler — the GET paths skip the check because
    // safe methods return Ok(()) from verify_same_origin.
    let public_routes = Router::new()
        .route("/login", get(handlers::auth::login_page).post(handlers::auth::login_submit))
        .route("/setup", get(handlers::auth::setup_page).post(handlers::auth::setup_submit))
        .layer(middleware::from_fn(handlers::auth::csrf_public))
        .merge(SwaggerUi::new("/api-docs").url("/api-docs/openapi.json", ApiDoc::openapi()));

    // Routes that require auth.
    let protected_routes = Router::new()
        .route("/", get(handlers::library::index))
        .route("/library/review", get(handlers::library::needs_review_page))
        .route("/series/{anilist_id}", get(handlers::library::series_detail))
        .route("/search", get(handlers::search::search_page).post(handlers::search::search_submit))
        .route("/api/anilist/search", get(handlers::library::anilist_search))
        .route("/api/library/add", post(handlers::library::add_series))
        .route("/api/library/remove", post(handlers::library::remove_series))
        .route("/api/library/reconcile-fallbacks", post(handlers::library::reconcile_fallbacks))
        .route("/api/series/{anilist_id}", get(handlers::library::api_series_detail))
        .route("/api/library/folder", post(handlers::library::set_folder))
        .route("/api/library/monitoring", post(handlers::library::set_monitoring))
        .route("/api/library/episode-monitoring", post(handlers::library::set_episode_monitoring))
        .route("/api/library/allow-upgrades", post(handlers::library::set_allow_upgrades))
        .route("/api/library/manual-override", post(handlers::library::set_manual_override))
        .route("/api/series/{anilist_id}/auto-search", post(handlers::library::auto_search_series))
        .route("/api/series/{anilist_id}/auto-search/{episode_number}", post(handlers::library::auto_search_episode))
        .route("/api/series/{anilist_id}/search-batch", post(handlers::library::search_batch_releases))
        .route("/api/series/{anilist_id}/interactive-search/{episode_number}", get(handlers::library::interactive_search_episode))
        .route("/api/series/{anilist_id}/grab/{episode_number}", post(handlers::library::grab_interactive_result))
        .route("/api/series/{anilist_id}/delete-file/{episode_number}", post(handlers::library::delete_episode_file))
        .route("/api/series/{anilist_id}/grab-history/{episode_number}", get(handlers::library::get_episode_grab_history))
        .route("/api/series/{anilist_id}/mark-failed/{episode_number}", post(handlers::library::mark_episode_failed))
        .route("/api/series/{anilist_id}/download-progress", get(handlers::library::episode_download_progress))
        .route("/api/series/{anilist_id}/episodes", get(handlers::library::series_episodes_json))
        .route("/api/library/folders", get(handlers::library::list_folders))
        .route("/api/grab", post(handlers::search::grab_release))
        .route("/api/search/page", get(handlers::search::search_page_api))
        .route("/api/torrents", get(handlers::search::get_torrents))
        .route("/downloads", get(handlers::downloads::downloads_page))
        .route("/api/downloads/pause", post(handlers::downloads::api_pause_torrent))
        .route("/api/downloads/resume", post(handlers::downloads::api_resume_torrent))
        .route("/api/downloads/delete", post(handlers::downloads::api_delete_torrent))
        .route("/api/downloads/blocklist/remove", post(handlers::downloads::api_blocklist_remove))
        .route("/settings", get(handlers::settings::settings_page).post(handlers::settings::settings_submit))
        .route("/settings/groups", post(handlers::settings::settings_groups_upsert))
        .route("/settings/groups/delete", post(handlers::settings::settings_groups_delete))
        .route("/api/qbit/test", post(handlers::settings::qbit_test))
        .route("/api/jellyfin/test", post(handlers::settings::jellyfin_test))
        .route("/api/health", get(handlers::settings::api_health))
        .route("/api/jellyfin/refresh", post(handlers::settings::jellyfin_refresh))
        .route("/system", get(handlers::system::system_page).post(handlers::system::debug_settings_submit))
        .route("/api/rss/sync", post(handlers::system::api_rss_sync))
        .route("/api/rss/clear-history", post(handlers::system::api_rss_clear_history))
        .route("/api/tasks/metadata-refresh", post(handlers::system::api_force_metadata_refresh))
        .route("/api/tasks/cleanup", post(handlers::system::api_force_cleanup))
        .route("/api/tasks/post-processing", post(handlers::system::api_force_post_processing))
        .route("/api/tasks/library-classify", post(handlers::system::api_force_library_classify))
        .route("/api/tasks/upgrade-search", post(handlers::system::api_force_upgrade_search))
        .route("/api/system/rebuild-anilist-cache", post(handlers::system::api_rebuild_cached_metadata))
        .route("/api/system/reload-anibridge", post(handlers::system::api_anibridge_reload))
        .route("/help", get(handlers::system::system_page))
        .route("/api/logs/poll", get(handlers::system::api_logs_poll))
        .route("/api/logs/clear", post(handlers::system::api_logs_clear))
        .route("/media/art/{cache_key}", get(handlers::media::artwork))
        .route("/logout", get(handlers::auth::logout))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            handlers::auth::require_auth,
        ));

    // Sonarr v3 API compatibility layer for Seerr integration.
    // Authenticated via ?apikey= query parameter, not cookies.
    let sonarr_routes = Router::new()
        .route("/api/v3/system/status", get(handlers::sonarr_compat::system_status))
        .route("/api/v3/qualityprofile", get(handlers::sonarr_compat::quality_profiles))
        .route("/api/v3/qualityProfile", get(handlers::sonarr_compat::quality_profiles))
        .route("/api/v3/rootfolder", get(handlers::sonarr_compat::root_folders))
        .route("/api/v3/rootFolder", get(handlers::sonarr_compat::root_folders))
        .route("/api/v3/languageprofile", get(handlers::sonarr_compat::language_profiles))
        .route("/api/v3/languageProfile", get(handlers::sonarr_compat::language_profiles))
        .route("/api/v3/tag", get(handlers::sonarr_compat::list_tags).post(handlers::sonarr_compat::create_tag))
        .route("/api/v3/series", get(handlers::sonarr_compat::list_series).post(handlers::sonarr_compat::add_series).put(handlers::sonarr_compat::update_series))
        .route("/api/v3/series/{id}", get(handlers::sonarr_compat::get_series))
        .route("/api/v3/series/lookup", get(handlers::sonarr_compat::series_lookup))
        .route("/api/v3/command", post(handlers::sonarr_compat::execute_command))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            handlers::sonarr_compat::require_api_key,
        ));

    // Radarr v3 API compatibility layer for Seerr integration (anime movies).
    // Mounted under /radarr/ prefix — Seerr uses URL Base "/radarr" to route here.
    let radarr_routes = Router::new()
        .route("/radarr/api/v3/system/status", get(handlers::radarr_compat::system_status))
        .route("/radarr/api/v3/qualityprofile", get(handlers::radarr_compat::quality_profiles))
        .route("/radarr/api/v3/qualityProfile", get(handlers::radarr_compat::quality_profiles))
        .route("/radarr/api/v3/rootfolder", get(handlers::radarr_compat::root_folders))
        .route("/radarr/api/v3/rootFolder", get(handlers::radarr_compat::root_folders))
        .route("/radarr/api/v3/tag", get(handlers::radarr_compat::list_tags).post(handlers::radarr_compat::create_tag))
        .route("/radarr/api/v3/movie", get(handlers::radarr_compat::list_movies).post(handlers::radarr_compat::add_movie).put(handlers::radarr_compat::update_movie))
        .route("/radarr/api/v3/movie/{id}", get(handlers::radarr_compat::get_movie))
        .route("/radarr/api/v3/movie/lookup", get(handlers::radarr_compat::movie_lookup))
        .route("/radarr/api/v3/command", post(handlers::radarr_compat::execute_command))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            handlers::radarr_compat::require_api_key,
        ));

    let app = Router::new()
        .merge(public_routes)
        .merge(protected_routes)
        .merge(sonarr_routes)
        .merge(radarr_routes)
        .nest_service("/static", ServeDir::new("static"))
        .with_state(state.clone());

    let addr = std::env::var("LISTEN_ADDR").unwrap_or_else(|_| "0.0.0.0:8978".to_string());
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .expect("Failed to bind");

    tracing::info!("Ryokan listening on {}", addr);

    // Register background task definitions for the System > Scheduled Tasks tab.
    let _ = models::scheduled_tasks::touch_definition(&db, "rss_sync", "RSS sync", "Every N minutes", false).await;
    let _ = models::scheduled_tasks::touch_definition(&db, "metadata_refresh", "Metadata refresh", "Every 12 hours", true).await;
    let _ = models::scheduled_tasks::touch_definition(&db, "cleanup", "Cleanup", "Every 1 hour", true).await;
    let _ = models::scheduled_tasks::touch_definition(&db, "post_processing", "Post-processing", "Every 1 minute (when enabled)", false).await;
    let upgrade_enabled = models::config::get_config(&db).await.ok().flatten().map(|c| c.upgrade_search_enabled).unwrap_or(false);
    let _ = models::scheduled_tasks::touch_definition(&db, "upgrade_search", "Quality upgrade search", "Every 24 hours (when enabled)", upgrade_enabled).await;
    let _ = models::scheduled_tasks::touch_definition(&db, "anibridge_refresh", "Anibridge mappings refresh", "Every 24 hours", true).await;
    let _ = models::scheduled_tasks::touch_definition(&db, "library_classify", "Library classify sweep", "Every 6 hours", true).await;

    // Pre-load anibridge mappings so the first Seerr request doesn't block on download.
    tokio::spawn(async { services::anibridge::ensure_loaded().await; });

    // Log startup to the database.
    services::logger::info(
        &db,
        models::log::LogCategory::System,
        "Ryokan started",
        &format!("Listening on {}", addr),
    )
    .await;

    // Background task: RSS auto-sync. Wrapped in `supervise` so a panic
    // inside sync_once is logged and the loop restarts rather than going
    // silent for the rest of the process lifetime.
    {
        let rss_state = state.clone();
        tokio::spawn(async move {
            supervise("rss_sync", move || {
                let inner_state = rss_state.clone();
                async move {
                    let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
                    let mut minutes_since_last: i64 = 10_000;
                    let mut consecutive_errors: i64 = 0;
                    loop {
                        interval.tick().await;
                        minutes_since_last += 1;

                        let cfg = match models::config::get_config(&inner_state.db).await {
                            Ok(Some(cfg)) => cfg,
                            _ => continue,
                        };

                        let _ = models::scheduled_tasks::touch_definition(&inner_state.db, "rss_sync", "RSS sync", &format!("Every {} minutes", cfg.rss_interval_minutes.clamp(1, 60)), cfg.rss_enabled).await;
                        if !cfg.rss_enabled {
                            continue;
                        }

                        let every = (cfg.rss_interval_minutes as i64).clamp(1, 60);
                        // Exponential backoff on consecutive errors: skip 2^errors extra intervals (capped at 32)
                        let backoff = if consecutive_errors > 0 {
                            every * (1i64 << consecutive_errors.min(5))
                        } else {
                            every
                        };
                        if minutes_since_last < backoff {
                            continue;
                        }

                        minutes_since_last = 0;
                        let _ = models::scheduled_tasks::mark_started(&inner_state.db, "rss_sync", "Automatic RSS sync started").await;
                        match services::rss::sync_once(&inner_state, "auto").await {
                            Ok(summary) => {
                                consecutive_errors = 0;
                                let _ = models::scheduled_tasks::mark_finished(&inner_state.db, "rss_sync", "ok", &summary.detail).await;
                            }
                            Err(err) => {
                                consecutive_errors += 1;
                                let _ = models::scheduled_tasks::mark_finished(&inner_state.db, "rss_sync", "error", &err).await;
                                services::logger::error(
                                    &inner_state.db,
                                    models::log::LogCategory::System,
                                    "Auto RSS sync failed",
                                    &format!("{} (backoff: {} consecutive errors)", err, consecutive_errors),
                                ).await;
                            }
                        }
                    }
                }
            }).await;
        });
    }


    // Background task: refresh cached series metadata every 12 hours.
    {
        let metadata_db = db.clone();
        tokio::spawn(async move {
            supervise("metadata_refresh", move || {
                let db = metadata_db.clone();
                async move {
                    let mut interval = tokio::time::interval(std::time::Duration::from_secs(12 * 60 * 60));
                    loop {
                        interval.tick().await;
                        let _ = models::scheduled_tasks::mark_started(&db, "metadata_refresh", "Refreshing tracked series metadata").await;
                        let (refreshed, failed) = services::metadata_sync::refresh_all_series_metadata(&db).await;
                        let status = if failed > 0 { "warn" } else { "ok" };
                        let detail = format!("refreshed={}, failed={}", refreshed, failed);
                        let _ = models::scheduled_tasks::mark_finished(&db, "metadata_refresh", status, &detail).await;
                    }
                }
            }).await;
        });
    }

    // Background task: clean up logs and old RSS decisions older than 30 days every hour.
    {
        let cleanup_db = db.clone();
        tokio::spawn(async move {
            supervise("cleanup", move || {
                let cleanup_db = cleanup_db.clone();
                async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(3600));
            loop {
                interval.tick().await;
                let _ = models::scheduled_tasks::mark_started(&cleanup_db, "cleanup", "Pruning logs and RSS decision history").await;
                let mut cleanup_errors = Vec::new();
                match models::log::cleanup(&cleanup_db, 30).await {
                    Ok(deleted) if deleted > 0 => {
                        tracing::debug!("Cleaned up {} old log entries", deleted);
                    }
                    Err(e) => {
                        cleanup_errors.push(format!("logs: {}", e));
                        tracing::error!("Log cleanup failed: {}", e);
                    }
                    _ => {}
                }
                // Prune old RSS decisions (keep grabbed forever, prune skipped/rejected after 30 days).
                match models::rss::cleanup_old_decisions(&cleanup_db, 30).await {
                    Ok(deleted) if deleted > 0 => {
                        tracing::debug!("Cleaned up {} old RSS decisions", deleted);
                    }
                    Err(e) => {
                        cleanup_errors.push(format!("rss: {}", e));
                        tracing::error!("RSS decision cleanup failed: {}", e);
                    }
                    _ => {}
                }
                // Prune cold Nyaa description cache rows. `cached_at` is only
                // refreshed on cache miss (live fetch), not on cache hits, so
                // this evicts rows that haven't triggered a network fetch in
                // 90 days. Consequence is a forced re-fetch the next time the
                // row is needed, not lost data.
                match models::nyaa_description_cache::cleanup(&cleanup_db, 90).await {
                    Ok(deleted) if deleted > 0 => {
                        tracing::debug!("Cleaned up {} old nyaa description cache rows", deleted);
                    }
                    Err(e) => {
                        cleanup_errors.push(format!("nyaa_description_cache: {}", e));
                        tracing::error!("Nyaa description cache cleanup failed: {}", e);
                    }
                    _ => {}
                }
                // Prune stale media probe cache rows. These are keyed by
                // filesystem path, so deleted / renamed files leave rows
                // that nothing will ever re-touch — the hourly sweep is the
                // only eviction path. Consequence for still-live files is a
                // single re-probe after the TTL expires.
                match models::media_probe_cache::cleanup(&cleanup_db, 90).await {
                    Ok(deleted) if deleted > 0 => {
                        tracing::debug!("Cleaned up {} old media probe cache rows", deleted);
                    }
                    Err(e) => {
                        cleanup_errors.push(format!("media_probe_cache: {}", e));
                        tracing::error!("Media probe cache cleanup failed: {}", e);
                    }
                    _ => {}
                }
                // Prune expired session rows. `validate_session` already
                // rejects rows older than 7 days, but without this sweep
                // the sessions table grows unbounded — every login leaves
                // a permanent row. 7 days matches the cookie Max-Age.
                match models::session::cleanup(&cleanup_db, 7).await {
                    Ok(deleted) if deleted > 0 => {
                        tracing::debug!("Cleaned up {} expired session rows", deleted);
                    }
                    Err(e) => {
                        cleanup_errors.push(format!("sessions: {}", e));
                        tracing::error!("Session cleanup failed: {}", e);
                    }
                    _ => {}
                }
                // Prune idle LOGIN_FAILURES entries. The per-request sweep
                // in `login_check` only touches keys actively being hit,
                // so IPs / usernames that failed once and then went quiet
                // would linger until the process restarts. Hourly global
                // sweep keeps the map bounded.
                handlers::auth::sweep_login_failures();
                let status = if cleanup_errors.is_empty() { "ok" } else { "warn" };
                let detail = if cleanup_errors.is_empty() { "Cleanup completed".to_string() } else { cleanup_errors.join("; ") };
                let _ = models::scheduled_tasks::mark_finished(&cleanup_db, "cleanup", status, &detail).await;
            }
                }
            }).await;
        });
    }

    // Background task: post-processing — move/rename completed downloads every minute.
    {
        let pp_state = state.clone();
        tokio::spawn(async move {
            supervise("post_processing", move || {
                let pp_state = pp_state.clone();
                async move {
                    let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
                    loop {
                        interval.tick().await;
                        let enabled = models::config::get_config(&pp_state.db)
                            .await
                            .ok()
                            .flatten()
                            .map(|c| c.post_processing_enabled)
                            .unwrap_or(false);
                        let _ = models::scheduled_tasks::touch_definition(
                            &pp_state.db,
                            "post_processing",
                            "Post-processing",
                            "Every 1 minute (when enabled)",
                            enabled,
                        ).await;
                        if !enabled {
                            continue;
                        }
                        let _ = models::scheduled_tasks::mark_started(&pp_state.db, "post_processing", "Checking for completed downloads").await;
                        services::post_processing::run_once(&pp_state).await;
                        let _ = models::scheduled_tasks::mark_finished(&pp_state.db, "post_processing", "ok", "").await;
                    }
                }
            }).await;
        });
    }

    // Background task: library classify sweep every 6 hours. Re-runs the
    // classifier against any on-disk files that are still tagged empty or
    // "unknown" so the library self-heals when earlier low-confidence
    // filename-only results can now be resolved with ffprobe. The 6-hour
    // cadence is slow enough that ffprobe cost stays trivial and fast
    // enough that a new unknown row upgrades the same day.
    {
        let classify_state = state.clone();
        tokio::spawn(async move {
            supervise("library_classify", move || {
                let classify_state = classify_state.clone();
                async move {
                    let mut interval = tokio::time::interval(std::time::Duration::from_secs(6 * 60 * 60));
                    // Skip the immediate tick — we don't want a big ffprobe
                    // sweep racing the rest of startup.
                    interval.tick().await;
                    loop {
                        interval.tick().await;
                        let _ = models::scheduled_tasks::touch_definition(
                            &classify_state.db,
                            "library_classify",
                            "Library classify sweep",
                            "Every 6 hours",
                            true,
                        ).await;
                        let _ = models::scheduled_tasks::mark_started(
                            &classify_state.db,
                            "library_classify",
                            "Re-classifying unknown / unclassified files",
                        ).await;
                        let report = services::post_processing::scan_library_for_unclassified(&classify_state).await;
                        let detail = format!(
                            "series={}, files_scanned={}, classified={}, needs_review={}",
                            report.series_scanned,
                            report.files_scanned,
                            report.files_classified,
                            report.files_needing_review,
                        );
                        let _ = models::scheduled_tasks::mark_finished(
                            &classify_state.db,
                            "library_classify",
                            "ok",
                            &detail,
                        ).await;
                    }
                }
            }).await;
        });
    }

    // Background task: quality upgrade search every 24 hours (when enabled).
    {
        let upgrade_state = state.clone();
        tokio::spawn(async move {
            supervise("upgrade_search", move || {
                let upgrade_state = upgrade_state.clone();
                async move {
                    let mut interval = tokio::time::interval(std::time::Duration::from_secs(24 * 60 * 60));
                    loop {
                        interval.tick().await;
                        let enabled = models::config::get_config(&upgrade_state.db)
                            .await
                            .ok()
                            .flatten()
                            .map(|c| c.upgrade_search_enabled)
                            .unwrap_or(false);
                        let _ = models::scheduled_tasks::touch_definition(
                            &upgrade_state.db,
                            "upgrade_search",
                            "Quality upgrade search",
                            "Every 24 hours (when enabled)",
                            enabled,
                        ).await;
                        if !enabled {
                            continue;
                        }
                        let _ = models::scheduled_tasks::mark_started(&upgrade_state.db, "upgrade_search", "Searching for quality upgrades").await;
                        match tokio::time::timeout(
                            std::time::Duration::from_secs(30 * 60),
                            services::upgrade::run_once(&upgrade_state),
                        ).await {
                            Ok(Ok(summary)) => {
                                let _ = models::scheduled_tasks::mark_finished(&upgrade_state.db, "upgrade_search", "ok", &summary.detail).await;
                            }
                            Ok(Err(err)) => {
                                let _ = models::scheduled_tasks::mark_finished(&upgrade_state.db, "upgrade_search", "error", &err).await;
                                services::logger::error(
                                    &upgrade_state.db,
                                    models::log::LogCategory::System,
                                    "Upgrade search failed",
                                    &err,
                                ).await;
                            }
                            Err(_) => {
                                let _ = models::scheduled_tasks::mark_finished(&upgrade_state.db, "upgrade_search", "error", "Timed out after 30 minutes").await;
                                services::logger::error(
                                    &upgrade_state.db,
                                    models::log::LogCategory::System,
                                    "Upgrade search timed out",
                                    "Exceeded 30-minute limit",
                                ).await;
                            }
                        }
                    }
                }
            }).await;
        });
    }

    // Background task: Anibridge mappings refresh (every 24 hours).
    {
        let anibridge_db = db.clone();
        tokio::spawn(async move {
            supervise("anibridge_refresh", move || {
                let anibridge_db = anibridge_db.clone();
                async move {
                    let mut interval = tokio::time::interval(std::time::Duration::from_secs(24 * 60 * 60));
                    interval.tick().await; // skip immediate tick — initial load happens on first use
                    loop {
                        interval.tick().await;
                        let _ = models::scheduled_tasks::mark_started(&anibridge_db, "anibridge_refresh", "Refreshing anibridge mappings").await;
                        if services::anibridge::reload().await {
                            let _ = models::scheduled_tasks::mark_finished(&anibridge_db, "anibridge_refresh", "ok", "Mappings refreshed").await;
                        } else {
                            let _ = models::scheduled_tasks::mark_finished(&anibridge_db, "anibridge_refresh", "error", "Failed to download mappings").await;
                        }
                    }
                }
            }).await;
        });
    }

    // Use `into_make_service_with_connect_info::<SocketAddr>()` so the auth
    // handler can pull the true client socket address via
    // `ConnectInfo<SocketAddr>`. This is the ground-truth IP the rate limiter
    // uses whenever RYOKAN_TRUSTED_PROXY is unset — without it, the only
    // source of an IP is the (spoofable) X-Forwarded-For / X-Real-IP headers.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .expect("Server error");
}
