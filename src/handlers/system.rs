use askama::Template;
use axum::{
    extract::{Query, State},
    response::{Html, Json},
    Form,
};
use serde::Deserialize;

use crate::models::{config, log::{self, LogCategory}, rss, scheduled_tasks};
use crate::services::{logger, metadata_sync, rss as rss_service};
use crate::AppState;

#[derive(Template)]
#[template(path = "system.html")]
struct SystemTemplate {
    page: String,
    tab: String,
    force_mal_fallback: bool,
    force_kitsu_fallback: bool,
    debug_message: Option<String>,
    debug_error: Option<String>,
    logs: Vec<log::LogEntry>,
    log_count: i64,
    filter_level: String,
    filter_category: String,
    filter_search: String,
    categories: Vec<(&'static str, &'static str)>,
    rss_enabled: bool,
    rss_interval_minutes: i32,
    rss_last_run: Option<rss::RssRun>,
    rss_recent: Vec<rss::RssDecision>,
    scheduled_tasks: Vec<scheduled_tasks::ScheduledTaskStatus>,
}

#[derive(Deserialize)]
pub struct SystemQuery {
    tab: Option<String>,
    level: Option<String>,
    category: Option<String>,
    search: Option<String>,
    message: Option<String>,
    error: Option<String>,
}

#[derive(Deserialize)]
pub struct DebugSettingsForm {
    force_mal_fallback: Option<String>,
    force_kitsu_fallback: Option<String>,
}

fn normalize_system_tab(tab: Option<String>) -> String {
    match tab.as_deref() {
        Some("help") => "help".to_string(),
        Some("debug") => "debug".to_string(),
        Some("rss") => "rss".to_string(),
        Some("tasks") => "tasks".to_string(),
        _ => "logs".to_string(),
    }
}

pub async fn system_page(
    State(state): State<AppState>,
    Query(params): Query<SystemQuery>,
) -> Html<String> {
    let tab = normalize_system_tab(params.tab.clone());

    let filter_level = params.level.unwrap_or_else(|| "info".to_string());
    let filter_category = params.category.unwrap_or_default();
    let filter_search = params.search.unwrap_or_default();

    let logs = if tab == "logs" {
        log::query(
            &state.db,
            &log::LogQuery {
                level: Some(filter_level.clone()),
                category: if filter_category.is_empty() {
                    None
                } else {
                    Some(filter_category.clone())
                },
                search: if filter_search.is_empty() {
                    None
                } else {
                    Some(filter_search.clone())
                },
                limit: 200,
                before_id: None,
            },
        )
        .await
        .unwrap_or_default()
    } else {
        Vec::new()
    };

    let cfg = config::get_config(&state.db)
        .await
        .ok()
        .flatten();

    let force_mal_fallback = cfg.as_ref().map(|cfg| cfg.force_mal_fallback).unwrap_or(false);
    let force_kitsu_fallback = cfg.as_ref().map(|cfg| cfg.force_kitsu_fallback).unwrap_or(false);
    let rss_enabled = cfg.as_ref().map(|cfg| cfg.rss_enabled).unwrap_or(false);
    let rss_interval_minutes = cfg.as_ref().map(|cfg| cfg.rss_interval_minutes).unwrap_or(5);
    let rss_last_run = rss::latest_run(&state.db).await.unwrap_or(None);
    let rss_recent = if tab == "rss" {
rss::recent_decisions(&state.db, 500).await.unwrap_or_default()
    } else {
        Vec::new()
    };

    let scheduled_tasks = if tab == "tasks" { scheduled_tasks::list(&state.db).await.unwrap_or_default() } else { Vec::new() };

    let log_count = log::count(&state.db).await.unwrap_or(0);

    let categories = vec![
        ("search", LogCategory::Search.label()),
        ("grab", LogCategory::Grab.label()),
        ("auto_search", LogCategory::AutoSearch.label()),
        ("nyaa", LogCategory::Nyaa.label()),
        ("anilist", LogCategory::AniList.label()),
        ("jikan", LogCategory::Jikan.label()),
        ("qbit", LogCategory::QBit.label()),
        ("jellyfin", LogCategory::Jellyfin.label()),
        ("media", LogCategory::Media.label()),
        ("library", LogCategory::Library.label()),
        ("auth", LogCategory::Auth.label()),
        ("system", LogCategory::System.label()),
    ];

    let template = SystemTemplate {
        page: "system".to_string(),
        tab,
        force_mal_fallback,
        force_kitsu_fallback,
        debug_message: params.message,
        debug_error: params.error,
        logs,
        log_count,
        filter_level,
        filter_category,
        filter_search,
        categories,
        rss_enabled,
        rss_interval_minutes,
        rss_last_run,
        rss_recent,
        scheduled_tasks,
    };
    Html(template.render().unwrap_or_default())
}

pub async fn debug_settings_submit(
    State(state): State<AppState>,
    Form(form): Form<DebugSettingsForm>,
) -> Html<String> {
    let mut cfg = config::get_config(&state.db)
        .await
        .ok()
        .flatten()
        .unwrap_or(config::Config {
            qbit_url: String::new(),
            qbit_user: String::new(),
            qbit_pass: String::new(),
            qbit_category: String::new(),
            qbit_download_path: String::new(),
            jellyfin_url: String::new(),
            jellyfin_api_key: String::new(),
            preferred_groups: String::new(),
            blocked_groups: String::new(),
            preferred_resolution: "1080".to_string(),
            quality_profile: "web_1080".to_string(),
            quality_cutoff: "bd_1080".to_string(),
            finished_series_quality: "prefer_bd".to_string(),
            media_root: String::new(),
            title_language: "english".to_string(),
            force_mal_fallback: false,
            rss_enabled: false,
            rss_interval_minutes: 5,
            force_kitsu_fallback: false,
            post_processing_enabled: false,
            post_processing_mode: "hardlink".to_string(),
        });

    cfg.force_mal_fallback = form.force_mal_fallback.is_some();
    cfg.force_kitsu_fallback = form.force_kitsu_fallback.is_some();

    let result = config::save_config(&state.db, &cfg).await;
    let (message, error) = match result {
        Ok(_) => {
            logger::info(
                &state.db,
                LogCategory::System,
                "Updated fallback debug settings",
                &format!("mal_jikan={}, kitsu={}", if cfg.force_mal_fallback { "enabled" } else { "disabled" }, if cfg.force_kitsu_fallback { "enabled" } else { "disabled" }),
            ).await;
            (Some(format!("Fallback debug settings saved. MAL/Jikan: {}. Kitsu: {}.", if cfg.force_mal_fallback { "enabled" } else { "disabled" }, if cfg.force_kitsu_fallback { "enabled" } else { "disabled" })), None)
        }
        Err(e) => {
            logger::error(&state.db, LogCategory::System, "Failed to update fallback debug settings", &e.to_string()).await;
            (None, Some(format!("Failed to save debug settings: {}", e)))
        }
    };

    let template = SystemTemplate {
        page: "system".to_string(),
        tab: "debug".to_string(),
        force_mal_fallback: cfg.force_mal_fallback,
        force_kitsu_fallback: cfg.force_kitsu_fallback,
        debug_message: message,
        debug_error: error,
        logs: Vec::new(),
        log_count: log::count(&state.db).await.unwrap_or(0),
        filter_level: "info".to_string(),
        filter_category: String::new(),
        filter_search: String::new(),
        categories: vec![
            ("search", LogCategory::Search.label()),
            ("grab", LogCategory::Grab.label()),
            ("auto_search", LogCategory::AutoSearch.label()),
            ("nyaa", LogCategory::Nyaa.label()),
            ("anilist", LogCategory::AniList.label()),
            ("jikan", LogCategory::Jikan.label()),
            ("qbit", LogCategory::QBit.label()),
            ("jellyfin", LogCategory::Jellyfin.label()),
            ("media", LogCategory::Media.label()),
            ("library", LogCategory::Library.label()),
            ("auth", LogCategory::Auth.label()),
            ("system", LogCategory::System.label()),
        ],
        rss_enabled: cfg.rss_enabled,
        rss_interval_minutes: cfg.rss_interval_minutes,
        rss_last_run: rss::latest_run(&state.db).await.unwrap_or(None),
        rss_recent: Vec::new(),
        scheduled_tasks: scheduled_tasks::list(&state.db).await.unwrap_or_default(),
    };
    Html(template.render().unwrap_or_default())
}

#[derive(Deserialize)]
pub struct LogPollQuery {
    after: Option<i64>,
    level: Option<String>,
    category: Option<String>,
}

pub async fn api_logs_poll(
    State(state): State<AppState>,
    Query(params): Query<LogPollQuery>,
) -> Json<Vec<log::LogEntry>> {
    let after_id = params.after.unwrap_or(0);
    let mut entries = log::entries_after(&state.db, after_id, 100)
        .await
        .unwrap_or_default();

    if let Some(ref level) = params.level {
        let min_level = level_rank(level);
        entries.retain(|e| level_rank(&e.level) >= min_level);
    }
    if let Some(ref cat) = params.category {
        if !cat.is_empty() {
            entries.retain(|e| e.category == *cat);
        }
    }

    Json(entries)
}



pub async fn api_rebuild_cached_metadata(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let (rebuilt, skipped, failed) = metadata_sync::rebuild_cached_metadata_for_all(&state.db).await;
    let message = format!(
        "Metadata cache rebuild complete. Rebuilt: {}. Skipped: {}. Failed: {}.",
        rebuilt, skipped, failed
    );

    Ok(Json(serde_json::json!({
        "ok": failed == 0,
        "rebuilt": rebuilt,
        "skipped": skipped,
        "failed": failed,
        "message": message,
    })))
}

pub async fn api_logs_clear(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    logger::info(&state.db, LogCategory::System, "Logs cleared by user", "").await;
    sqlx::query("DELETE FROM logs")
        .execute(&state.db)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::json!({"ok": true})))
}


pub async fn api_rss_sync(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let _ = scheduled_tasks::mark_started(&state.db, "rss_sync", "Manual RSS sync started").await;
    match rss_service::sync_once(&state, "manual").await {
        Ok(summary) => {
            let _ = scheduled_tasks::mark_finished(&state.db, "rss_sync", "ok", &summary.detail).await;
            Ok(Json(serde_json::json!({
                "ok": true,
                "message": summary.detail,
                "summary": summary,
            })))
        }
        Err(err) => {
            let _ = scheduled_tasks::mark_finished(&state.db, "rss_sync", "error", &err).await;
            Err((axum::http::StatusCode::BAD_GATEWAY, serde_json::json!({
                "ok": false,
                "message": err,
            }).to_string()))
        }
    }
}

pub async fn api_rss_clear_history(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let deleted = rss::clear_grab_history(&state.db)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    logger::info(
        &state.db,
        LogCategory::System,
        "RSS grab history cleared",
        &format!("Removed {} grabbed entries", deleted),
    ).await;
    Ok(Json(serde_json::json!({
        "ok": true,
        "message": format!("Cleared {} grab history entries. Previously grabbed episodes will be re-evaluated on next sync.", deleted),
    })))
}

fn level_rank(level: &str) -> u8 {
    match level.to_lowercase().as_str() {
        "trace" => 0,
        "debug" => 1,
        "info" => 2,
        "warn" => 3,
        "error" => 4,
        _ => 2,
    }
}
