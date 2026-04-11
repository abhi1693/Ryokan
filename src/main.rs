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
use std::sync::Arc;
use tokio::sync::RwLock;
use tower_http::services::ServeDir;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use services::{jellyfin::JellyfinClient, qbit::QbitClient};

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

    // Routes that don't require auth.
    let public_routes = Router::new()
        .route("/login", get(handlers::auth::login_page).post(handlers::auth::login_submit))
        .route("/setup", get(handlers::auth::setup_page).post(handlers::auth::setup_submit));

    // Routes that require auth.
    let protected_routes = Router::new()
        .route("/", get(handlers::library::index))
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
        .route("/api/series/{anilist_id}/auto-search", post(handlers::library::auto_search_series))
        .route("/api/series/{anilist_id}/auto-search/{episode_number}", post(handlers::library::auto_search_episode))
        .route("/api/series/{anilist_id}/search-batch", post(handlers::library::search_batch_releases))
        .route("/api/series/{anilist_id}/interactive-search/{episode_number}", get(handlers::library::interactive_search_episode))
        .route("/api/series/{anilist_id}/grab/{episode_number}", post(handlers::library::grab_interactive_result))
        .route("/api/series/{anilist_id}/delete-file/{episode_number}", post(handlers::library::delete_episode_file))
        .route("/api/series/{anilist_id}/grab-history/{episode_number}", get(handlers::library::get_episode_grab_history))
        .route("/api/series/{anilist_id}/mark-failed/{episode_number}", post(handlers::library::mark_episode_failed))
        .route("/api/series/{anilist_id}/download-progress", get(handlers::library::episode_download_progress))
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
        .route("/api/qbit/test", post(handlers::settings::qbit_test))
        .route("/api/jellyfin/test", post(handlers::settings::jellyfin_test))
        .route("/api/jellyfin/refresh", post(handlers::settings::jellyfin_refresh))
        .route("/system", get(handlers::system::system_page).post(handlers::system::debug_settings_submit))
        .route("/api/rss/sync", post(handlers::system::api_rss_sync))
                .route("/api/rss/clear-history", post(handlers::system::api_rss_clear_history))
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

    let app = Router::new()
        .merge(public_routes)
        .merge(protected_routes)
        .merge(sonarr_routes)
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

    // Log startup to the database.
    services::logger::info(
        &db,
        models::log::LogCategory::System,
        "Ryokan started",
        &format!("Listening on {}", addr),
    )
    .await;

    // Background task: RSS auto-sync.
    {
        let rss_state = state.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
            let mut minutes_since_last: i64 = 10_000;
            let mut consecutive_errors: i64 = 0;
            loop {
                interval.tick().await;
                minutes_since_last += 1;

                let cfg = match models::config::get_config(&rss_state.db).await {
                    Ok(Some(cfg)) => cfg,
                    _ => continue,
                };

                let _ = models::scheduled_tasks::touch_definition(&rss_state.db, "rss_sync", "RSS sync", &format!("Every {} minutes", cfg.rss_interval_minutes.clamp(1, 60)), cfg.rss_enabled).await;
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
                let _ = models::scheduled_tasks::mark_started(&rss_state.db, "rss_sync", "Automatic RSS sync started").await;
                match services::rss::sync_once(&rss_state, "auto").await {
                    Ok(summary) => {
                        consecutive_errors = 0;
                        let _ = models::scheduled_tasks::mark_finished(&rss_state.db, "rss_sync", "ok", &summary.detail).await;
                    }
                    Err(err) => {
                        consecutive_errors += 1;
                        let _ = models::scheduled_tasks::mark_finished(&rss_state.db, "rss_sync", "error", &err).await;
                        services::logger::error(
                            &rss_state.db,
                            models::log::LogCategory::System,
                            "Auto RSS sync failed",
                            &format!("{} (backoff: {} consecutive errors)", err, consecutive_errors),
                        ).await;
                    }
                }
            }
        });
    }


    // Background task: refresh cached series metadata every 12 hours.
    {
        let metadata_db = db.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(12 * 60 * 60));
            loop {
                interval.tick().await;
                let _ = models::scheduled_tasks::mark_started(&metadata_db, "metadata_refresh", "Refreshing tracked series metadata").await;
                let (refreshed, failed) = services::metadata_sync::refresh_all_series_metadata(&metadata_db).await;
                let status = if failed > 0 { "warn" } else { "ok" };
                let detail = format!("refreshed={}, failed={}", refreshed, failed);
                let _ = models::scheduled_tasks::mark_finished(&metadata_db, "metadata_refresh", status, &detail).await;
            }
        });
    }

    // Background task: clean up logs and old RSS decisions older than 30 days every hour.
    {
        let cleanup_db = db.clone();
        tokio::spawn(async move {
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
                let status = if cleanup_errors.is_empty() { "ok" } else { "warn" };
                let detail = if cleanup_errors.is_empty() { "Cleanup completed".to_string() } else { cleanup_errors.join("; ") };
                let _ = models::scheduled_tasks::mark_finished(&cleanup_db, "cleanup", status, &detail).await;
            }
        });
    }

    // Background task: post-processing — move/rename completed downloads every minute.
    {
        let pp_state = state.clone();
        tokio::spawn(async move {
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
        });
    }

    axum::serve(listener, app).await.expect("Server error");
}
