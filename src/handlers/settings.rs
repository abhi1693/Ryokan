use askama::Template;
use axum::{extract::{Query, State}, response::{Html, Redirect}, Form, Json};
use serde::Deserialize;

use crate::models::{config, group_source_map};
use crate::models::log::LogCategory;
use crate::services::{jellyfin::JellyfinClient, logger, qbit::QbitClient, source::Source};
use crate::AppState;

#[derive(Template)]
#[template(path = "settings.html")]
struct SettingsTemplate {
    page: String,
    tab: String,
    config: config::Config,
    groups: Vec<group_source_map::GroupSourceEntry>,
    suggestions: Vec<group_source_map::GroupSuggestion>,
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
    preferred_source: String,
    preferred_resolution: String,
    cutoff_source: String,
    cutoff_resolution: String,
    finished_series_quality: String,
    media_root: String,
    title_language: String,
    rss_enabled: Option<String>,
    rss_interval_minutes: i32,
    post_processing_enabled: Option<String>,
    post_processing_mode: String,
    prefer_subs: String,
    sonarr_enabled: Option<String>,
    sonarr_api_key: Option<String>,
    radarr_enabled: Option<String>,
    radarr_api_key: Option<String>,
    upgrade_search_enabled: Option<String>,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct QbitTestForm {
    qbit_url: String,
    qbit_user: String,
    qbit_pass: String,
    qbit_category: Option<String>,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct JellyfinTestForm {
    jellyfin_url: String,
    jellyfin_api_key: String,
}


fn normalize_settings_tab(tab: Option<String>) -> String {
    match tab.as_deref() {
        Some("quality") => "quality".to_string(),
        Some("groups") => "groups".to_string(),
        Some("general") => "general".to_string(),
        _ => "integrations".to_string(),
    }
}

async fn load_groups(
    db: &sqlx::SqlitePool,
) -> Vec<group_source_map::GroupSourceEntry> {
    group_source_map::list_all(db).await.unwrap_or_default()
}

/// Load group-map suggestions inferred from the user's manual overrides.
/// Threshold of 2 matches `compute_suggestions`' docstring rationale: a
/// single override is noise, two matching overrides is the smallest
/// pattern worth surfacing.
async fn load_suggestions(
    db: &sqlx::SqlitePool,
) -> Vec<group_source_map::GroupSuggestion> {
    group_source_map::compute_suggestions(db, 2).await.unwrap_or_default()
}

/// Validate a form-submitted source string by round-tripping through
/// `Source::from_str`. Returns the canonical lowercase form on success, or
/// the supplied default when the value is unrecognized.
fn validate_source(value: &str, default: &str) -> String {
    use crate::services::source::Source;
    let parsed = Source::from_str(value);
    if parsed == Source::Unknown {
        default.to_string()
    } else {
        // Store the canonical lowercase form (e.g. "bluray", "web") so reads
        // via Source::from_str always succeed.
        parsed.as_str().to_ascii_lowercase()
    }
}

/// Validate a form-submitted cutoff-source string. Like `validate_source`
/// but also passes through the BluRay sub-tier markers "bluray_remux" and
/// "bluray_bdmv" so settings can store BD Remux / BD RAW as distinct
/// cutoffs. Reads go through `source::parse_cutoff_source`.
fn validate_cutoff_source(value: &str, default: &str) -> String {
    if value == "bluray_remux" || value == "bluray_bdmv" {
        return value.to_string();
    }
    validate_source(value, default)
}

/// Validate a form-submitted resolution string by round-tripping through
/// `Resolution::from_str`. Returns the bare numeric form ("1080", "720", …)
/// on success, or the supplied default when unrecognized.
fn validate_resolution(value: &str, default: &str) -> String {
    use crate::services::source::Resolution;
    let parsed = Resolution::from_str(value);
    if parsed == Resolution::Unknown {
        default.to_string()
    } else {
        // Strip the trailing 'p' for DB consistency ("1080" not "1080p").
        parsed.as_str().trim_end_matches('p').to_string()
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
        .unwrap_or_default();

    let groups = load_groups(&state.db).await;
    let suggestions = load_suggestions(&state.db).await;

    let template = SettingsTemplate {
        page: "settings".to_string(),
        tab: normalize_settings_tab(params.tab),
        config: cfg,
        groups,
        suggestions,
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
        preferred_source: validate_source(&form.preferred_source, "web"),
        preferred_resolution: validate_resolution(&form.preferred_resolution, "1080"),
        cutoff_source: validate_cutoff_source(&form.cutoff_source, "bluray"),
        cutoff_resolution: validate_resolution(&form.cutoff_resolution, "1080"),
        // Legacy combined tier columns — kept one release for rollback.
        // No longer user-editable; carried forward from the existing row.
        quality_profile: existing_cfg
            .as_ref()
            .map(|c| c.quality_profile.clone())
            .unwrap_or_else(|| "web_1080".to_string()),
        quality_cutoff: existing_cfg
            .as_ref()
            .map(|c| c.quality_cutoff.clone())
            .unwrap_or_else(|| "bd_1080".to_string()),
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
        prefer_subs: form.prefer_subs == "1",
        allow_non_english: existing_cfg.as_ref().map(|c| c.allow_non_english).unwrap_or(false),
        sonarr_enabled: if form.tab.as_deref() == Some("integrations") || form.tab.is_none() {
            form.sonarr_enabled.is_some()
        } else {
            existing_cfg.as_ref().map(|c| c.sonarr_enabled).unwrap_or(false)
        },
        sonarr_api_key: if form.tab.as_deref() == Some("integrations") || form.tab.is_none() {
            form.sonarr_api_key.unwrap_or_default().trim().to_string()
        } else {
            existing_cfg.as_ref().map(|c| c.sonarr_api_key.clone()).unwrap_or_default()
        },
        radarr_enabled: if form.tab.as_deref() == Some("integrations") || form.tab.is_none() {
            form.radarr_enabled.is_some()
        } else {
            existing_cfg.as_ref().map(|c| c.radarr_enabled).unwrap_or(false)
        },
        radarr_api_key: if form.tab.as_deref() == Some("integrations") || form.tab.is_none() {
            form.radarr_api_key.unwrap_or_default().trim().to_string()
        } else {
            existing_cfg.as_ref().map(|c| c.radarr_api_key.clone()).unwrap_or_default()
        },
        upgrade_search_enabled: if form.tab.as_deref() == Some("quality") || form.tab.is_none() {
            form.upgrade_search_enabled.is_some()
        } else {
            existing_cfg.as_ref().map(|c| c.upgrade_search_enabled).unwrap_or(false)
        },
        // Carried forward from the existing row — edited through the
        // dedicated Custom Formats settings page (Phase 7), not this form.
        custom_format_minimum_score: existing_cfg
            .as_ref()
            .map(|c| c.custom_format_minimum_score)
            .unwrap_or(i32::MIN),
    };

    let active_tab = normalize_settings_tab(form.tab.clone());

    if let Err(e) = config::save_config(&state.db, &cfg).await {
        logger::error(&state.db, LogCategory::System, "Failed to save settings", &e.to_string()).await;
        let groups = load_groups(&state.db).await;
        let suggestions = load_suggestions(&state.db).await;
        let template = SettingsTemplate {
            page: "settings".to_string(),
            tab: active_tab,
            config: cfg,
            groups,
            suggestions,
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

    let groups = load_groups(&state.db).await;
    let suggestions = load_suggestions(&state.db).await;
    let template = SettingsTemplate {
        page: "settings".to_string(),
        tab: active_tab,
        config: cfg,
        groups,
        suggestions,
        message: Some(notices.join("<br>")),
        error: None,
    };
    Html(template.render().unwrap_or_default())
}

// ─────────────────────────────────────────────────────────────────────────
// Release group source map CRUD
// ─────────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct GroupUpsertForm {
    group_name: String,
    source: String,
    confidence: Option<f32>,
    notes: Option<String>,
}

#[derive(Deserialize)]
pub struct GroupDeleteForm {
    group_name: String,
}

/// Upsert a user-edited row in `group_source_map`. Silently no-ops on an
/// empty group name or unknown source. Redirects back to the groups tab
/// regardless so the user sees the updated list.
pub async fn settings_groups_upsert(
    State(state): State<AppState>,
    Form(form): Form<GroupUpsertForm>,
) -> Redirect {
    let name = form.group_name.trim();
    if name.is_empty() {
        return Redirect::to("/settings?tab=groups");
    }
    let source = Source::from_str(&form.source);
    if source == Source::Unknown {
        return Redirect::to("/settings?tab=groups");
    }
    let confidence = form.confidence.unwrap_or(0.95).clamp(0.0, 1.0);
    let notes = form.notes.unwrap_or_default();
    let notes = notes.trim();

    match group_source_map::upsert_user_edit(&state.db, name, source, confidence, notes).await {
        Ok(_) => {
            logger::info(
                &state.db,
                LogCategory::System,
                &format!("Group source updated: {}", name),
                source.as_str(),
            )
            .await;
        }
        Err(e) => {
            logger::error(
                &state.db,
                LogCategory::System,
                "Group source upsert failed",
                &e.to_string(),
            )
            .await;
        }
    }
    Redirect::to("/settings?tab=groups")
}

/// Delete a row from `group_source_map` by group name. Works on both seeded
/// and user-edited rows — seeded rows will be re-inserted on the next
/// startup via `seed_defaults`, so deletes of seeds are effectively a
/// one-session reset.
pub async fn settings_groups_delete(
    State(state): State<AppState>,
    Form(form): Form<GroupDeleteForm>,
) -> Redirect {
    let name = form.group_name.trim();
    if name.is_empty() {
        return Redirect::to("/settings?tab=groups");
    }
    match group_source_map::delete(&state.db, name).await {
        Ok(_) => {
            logger::info(
                &state.db,
                LogCategory::System,
                &format!("Group source deleted: {}", name),
                "",
            )
            .await;
        }
        Err(e) => {
            logger::error(
                &state.db,
                LogCategory::System,
                "Group source delete failed",
                &e.to_string(),
            )
            .await;
        }
    }
    Redirect::to("/settings?tab=groups")
}

#[utoipa::path(
    post,
    path = "/api/qbit/test",
    tag = "System",
    summary = "Test qBittorrent connection",
    description = "Test connectivity to a qBittorrent instance with the provided credentials.",
    request_body = QbitTestForm,
    responses(
        (status = 200, description = "Connection successful", body = serde_json::Value),
        (status = 502, description = "Connection failed"),
    ),
)]
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

#[utoipa::path(
    post,
    path = "/api/jellyfin/test",
    tag = "System",
    summary = "Test Jellyfin connection",
    description = "Test connectivity to a Jellyfin instance with the provided URL and API key.",
    request_body = JellyfinTestForm,
    responses(
        (status = 200, description = "Connection successful", body = serde_json::Value),
        (status = 502, description = "Connection failed"),
    ),
)]
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

#[utoipa::path(
    get,
    path = "/api/health",
    tag = "System",
    summary = "Health check",
    description = "Returns connection status of qBittorrent and Jellyfin integrations.",
    responses(
        (status = 200, description = "Health status", body = serde_json::Value),
    ),
)]
pub async fn api_health(
    State(state): State<AppState>,
) -> Json<serde_json::Value> {
    let qbit_status = {
        let client = state.qbit.read().await.clone();
        match client {
            Some(c) => match c.test_connection().await {
                Ok(version) => serde_json::json!({"ok": true, "message": format!("qBittorrent {}", version)}),
                Err(e) => serde_json::json!({"ok": false, "message": e}),
            },
            None => serde_json::json!({"ok": false, "message": "Not configured"}),
        }
    };

    let jellyfin_status = {
        let client = state.jellyfin.read().await.clone();
        match client {
            Some(c) => match c.test_connection().await {
                Ok(info) => {
                    let label = if info.server_name.trim().is_empty() {
                        format!("Jellyfin {}", info.version)
                    } else {
                        format!("{} ({})", info.server_name, info.version)
                    };
                    serde_json::json!({"ok": true, "message": label})
                }
                Err(e) => serde_json::json!({"ok": false, "message": e}),
            },
            None => serde_json::json!({"ok": false, "message": "Not configured"}),
        }
    };

    Json(serde_json::json!({
        "qbit": qbit_status,
        "jellyfin": jellyfin_status,
    }))
}

#[utoipa::path(
    post,
    path = "/api/jellyfin/refresh",
    tag = "System",
    summary = "Refresh Jellyfin library",
    description = "Trigger a library scan in Jellyfin to pick up newly added media.",
    responses(
        (status = 200, description = "Library refresh triggered", body = serde_json::Value),
        (status = 400, description = "Jellyfin not configured"),
        (status = 502, description = "Refresh failed"),
    ),
)]
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
