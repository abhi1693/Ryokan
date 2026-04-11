use askama::Template;
use axum::{extract::{Query, State}, response::Html, Form, Json};
use serde::Deserialize;

use crate::models::config;
use crate::models::log::LogCategory;
use crate::services::{jellyfin::JellyfinClient, logger, qbit::QbitClient};
use crate::AppState;

#[derive(Template)]
#[template(path = "settings.html")]
struct SettingsTemplate {
    page: String,
    tab: String,
    config: config::Config,
    message: Option<String>,
    error: Option<String>,
}

#[derive(Deserialize)]
pub struct SettingsQuery {
    tab: Option<String>,
}

#[derive(Deserialize)]
pub struct SettingsForm {
    tab: Option<String>,
    qbit_url: String,
    qbit_user: String,
    qbit_pass: String,
    qbit_category: String,
    qbit_download_path: String,
    jellyfin_url: String,
    jellyfin_api_key: String,
    preferred_groups: String,
    blocked_groups: String,
    preferred_resolution: String,
    quality_profile: String,
    quality_cutoff: String,
    finished_series_quality: String,
    media_root: String,
    title_language: String,
    rss_enabled: Option<String>,
    rss_interval_minutes: i32,
    post_processing_enabled: Option<String>,
    post_processing_mode: String,
}

#[derive(Deserialize)]
pub struct QbitTestForm {
    qbit_url: String,
    qbit_user: String,
    qbit_pass: String,
    qbit_category: Option<String>,
}

#[derive(Deserialize)]
pub struct JellyfinTestForm {
    jellyfin_url: String,
    jellyfin_api_key: String,
}

fn default_config() -> config::Config {
    config::Config {
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
        auto_grab_on_add: true,
    }
}

fn normalize_settings_tab(tab: Option<String>) -> String {
    match tab.as_deref() {
        Some("quality") => "quality".to_string(),
        Some("general") => "general".to_string(),
        _ => "integrations".to_string(),
    }
}

pub async fn settings_page(
    State(state): State<AppState>,
    Query(params): Query<SettingsQuery>,
) -> Html<String> {
    let cfg = config::get_config(&state.db)
        .await
        .ok()
        .flatten()
        .unwrap_or_else(default_config);

    let template = SettingsTemplate {
        page: "settings".to_string(),
        tab: normalize_settings_tab(params.tab),
        config: cfg,
        message: None,
        error: None,
    };
    Html(template.render().unwrap_or_default())
}

pub async fn settings_submit(
    State(state): State<AppState>,
    Form(form): Form<SettingsForm>,
) -> Html<String> {
    let current_force_mal_fallback = config::get_config(&state.db)
        .await
        .ok()
        .flatten()
        .map(|cfg| cfg.force_mal_fallback)
        .unwrap_or(false);

    let existing_cfg = config::get_config(&state.db)
        .await
        .ok()
        .flatten();

    let current_force_kitsu_fallback = existing_cfg.as_ref().map(|cfg| cfg.force_kitsu_fallback).unwrap_or(false);

    let cfg = config::Config {
        qbit_url: form.qbit_url.trim().to_string(),
        qbit_user: form.qbit_user.trim().to_string(),
        qbit_pass: form.qbit_pass,
        qbit_category: form.qbit_category.trim().to_string(),
        qbit_download_path: form.qbit_download_path.trim().trim_end_matches('/').to_string(),
        jellyfin_url: form.jellyfin_url.trim().trim_end_matches('/').to_string(),
        jellyfin_api_key: form.jellyfin_api_key.trim().to_string(),
        preferred_groups: form.preferred_groups.trim().to_string(),
        blocked_groups: form.blocked_groups.trim().to_string(),
        preferred_resolution: form.preferred_resolution.trim().to_string(),
        quality_profile: {
            let tier = crate::services::quality::QualityTier::from_str(&form.quality_profile);
            if tier != crate::services::quality::QualityTier::Unknown {
                form.quality_profile
            } else {
                "web_1080".to_string()
            }
        },
        quality_cutoff: {
            let tier = crate::services::quality::QualityTier::from_str(&form.quality_cutoff);
            if tier != crate::services::quality::QualityTier::Unknown {
                form.quality_cutoff
            } else {
                "bd_1080".to_string()
            }
        },
        finished_series_quality: match form.finished_series_quality.as_str() {
            "same" | "prefer_bd" | "bd_only" => form.finished_series_quality,
            _ => "prefer_bd".to_string(),
        },
        media_root: form.media_root.trim().trim_end_matches('/').to_string(),
        title_language: match form.title_language.as_str() {
            "romaji" | "english" | "native" => form.title_language,
            _ => "english".to_string(),
        },
        force_mal_fallback: current_force_mal_fallback,
        rss_enabled: form.rss_enabled.is_some(),
        rss_interval_minutes: form.rss_interval_minutes.clamp(1, 60),
        force_kitsu_fallback: current_force_kitsu_fallback,
        post_processing_enabled: form.post_processing_enabled.is_some(),
        post_processing_mode: match form.post_processing_mode.as_str() {
            "move" | "copy" | "hardlink" => form.post_processing_mode,
            _ => "hardlink".to_string(),
        },
        auto_grab_on_add: existing_cfg.as_ref().map(|c| c.auto_grab_on_add).unwrap_or(true),
    };

    let active_tab = normalize_settings_tab(form.tab.clone());

    if let Err(e) = config::save_config(&state.db, &cfg).await {
        logger::error(&state.db, LogCategory::System, "Failed to save settings", &e.to_string()).await;
        let template = SettingsTemplate {
            page: "settings".to_string(),
            tab: active_tab,
            config: cfg,
            message: None,
            error: Some(format!("Failed to save: {}", e)),
        };
        return Html(template.render().unwrap_or_default());
    }

    logger::info(&state.db, LogCategory::System, "Settings saved", "").await;
    let mut notices: Vec<String> = vec!["Settings saved.".to_string()];

    if active_tab == "integrations" {
        if !cfg.qbit_url.is_empty() {
            let client = QbitClient::new(&cfg.qbit_url, &cfg.qbit_user, &cfg.qbit_pass, &cfg.qbit_category);
            match client.test_connection().await {
                Ok(version) => {
                    logger::info(&state.db, LogCategory::QBit, &format!("Connected to qBittorrent {}", version), &cfg.qbit_url).await;
                    notices.push(format!("qBittorrent connected ({}).", version));
                    *state.qbit.write().await = Some(client);
                }
                Err(e) => {
                    logger::error(&state.db, LogCategory::QBit, "Connection failed", &e).await;
                    *state.qbit.write().await = None;
                    notices.push(format!("qBittorrent connection failed: {}.", e));
                }
            }
        } else {
            *state.qbit.write().await = None;
        }

        if !cfg.jellyfin_url.is_empty() && !cfg.jellyfin_api_key.is_empty() {
            let client = JellyfinClient::new(
                &cfg.jellyfin_url,
                &cfg.jellyfin_api_key,
            );
            match client.test_connection().await {
                Ok(info) => {
                    let label = if info.server_name.trim().is_empty() {
                        format!("Jellyfin ({})", info.version)
                    } else {
                        format!("Jellyfin {} ({}) connected.", info.server_name, info.version)
                    };
                    logger::info(&state.db, LogCategory::Jellyfin, &format!("{} connected", label), &cfg.jellyfin_url).await;
                    notices.push(label);
                    *state.jellyfin.write().await = Some(client);
                }
                Err(e) => {
                    logger::error(&state.db, LogCategory::Jellyfin, "Connection failed", &e).await;
                    *state.jellyfin.write().await = None;
                    notices.push(format!("Jellyfin connection failed: {}.", e));
                }
            }
        } else {
            *state.jellyfin.write().await = None;
        }

        if !cfg.media_root.is_empty() && !std::path::Path::new(&cfg.media_root).is_dir() {
            notices.push(format!("Warning: media root '{}' is not accessible.", cfg.media_root));
        }
    }

    let template = SettingsTemplate {
        page: "settings".to_string(),
        tab: active_tab,
        config: cfg,
        message: Some(notices.join("<br>")),
        error: None,
    };
    Html(template.render().unwrap_or_default())
}

pub async fn qbit_test(
    Json(form): Json<QbitTestForm>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let client = QbitClient::new(
        form.qbit_url.trim(),
        form.qbit_user.trim(),
        &form.qbit_pass,
        form.qbit_category.as_deref().unwrap_or(""),
    );

    match client.test_connection().await {
        Ok(version) => Ok(Json(serde_json::json!({"ok": true, "message": format!("Connected to qBittorrent {}", version)}))),
        Err(err) => Err((axum::http::StatusCode::BAD_GATEWAY, serde_json::json!({"ok": false, "message": err}).to_string())),
    }
}

pub async fn jellyfin_test(
    Json(form): Json<JellyfinTestForm>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let client = JellyfinClient::new(
        form.jellyfin_url.trim(),
        &form.jellyfin_api_key,
    );

    match client.test_connection().await {
        Ok(info) => Ok(Json(serde_json::json!({
            "ok": true,
            "message": if info.server_name.trim().is_empty() {
                format!("Connected to Jellyfin {}", info.version)
            } else {
                format!("Connected to Jellyfin {} ({})", info.server_name, info.version)
            }
        }))),
        Err(err) => Err((axum::http::StatusCode::BAD_GATEWAY, serde_json::json!({"ok": false, "message": err}).to_string())),
    }
}

pub async fn jellyfin_refresh(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let client = {
        let jellyfin = state.jellyfin.read().await;
        jellyfin
            .as_ref()
            .ok_or((axum::http::StatusCode::BAD_REQUEST, "Jellyfin not configured".to_string()))?
            .clone()
    };

    client
        .refresh_library()
        .await
        .map_err(|e| (axum::http::StatusCode::BAD_GATEWAY, e))?;

    Ok(Json(serde_json::json!({"ok": true, "message": "Jellyfin library refresh queued"})))
}
