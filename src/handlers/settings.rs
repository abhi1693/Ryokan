use askama::Template;
use axum::{extract::{Query, State}, response::{Html, Redirect}, Form, Json};
use serde::Deserialize;

use crate::models::{config, custom_formats as cf_model, group_source_map};
use crate::models::log::LogCategory;
use crate::services::{
    custom_formats as cf_service, jellyfin::JellyfinClient, logger, qbit::QbitClient,
    source::Source,
};
use crate::AppState;

/// View-model wrapper rendered on the Custom Formats tab. Pairs each
/// stored CF row with its parsed spec count (or parse error), so the
/// table can surface "3 specs" next to well-formed rows and a red
/// "parse error: ..." marker next to ones the user needs to fix.
pub struct CustomFormatView {
    pub row: cf_model::CustomFormatRow,
    pub specs_count: usize,
    pub parse_error: Option<String>,
}

#[derive(Template)]
#[template(path = "settings.html")]
struct SettingsTemplate {
    page: String,
    tab: String,
    config: config::Config,
    groups: Vec<group_source_map::GroupSourceEntry>,
    suggestions: Vec<group_source_map::GroupSuggestion>,
    custom_formats: Vec<CustomFormatView>,
    custom_format_edit: Option<cf_model::CustomFormatRow>,
    /// Pre-rendered string for the minimum-score input. Empty when the
    /// floor is the `i32::MIN` "no floor" sentinel. Computed here so the
    /// Askama template doesn't need to compare against an integer path.
    custom_format_min_score_display: String,
    message: Option<String>,
    error: Option<String>,
}

fn min_score_display(score: i32) -> String {
    if score == i32::MIN {
        String::new()
    } else {
        score.to_string()
    }
}

#[derive(Deserialize)]
pub struct SettingsQuery {
    tab: Option<String>,
    /// When the Custom Formats tab is active and `edit_id` is set, the
    /// upsert form prefills from the existing row so the user can fix
    /// the JSON in place rather than deleting and re-pasting.
    edit_id: Option<i64>,
    /// Optional flash message / error surfaced after a POST-redirect.
    /// Kept minimal — detailed validation errors skip the redirect path
    /// and re-render inline so the form state is preserved.
    msg: Option<String>,
    err: Option<String>,
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
    seadex_enabled: Option<String>,
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
        Some("custom_formats") => "custom_formats".to_string(),
        Some("groups") => "groups".to_string(),
        Some("general") => "general".to_string(),
        _ => "integrations".to_string(),
    }
}

/// Load every CF row and annotate each one with its parsed spec count
/// (or the parse error string, if compilation fails). Used by the
/// Custom Formats tab to surface broken rows in the list view so the
/// user can find and fix them without trawling logs.
async fn load_custom_formats_view(db: &sqlx::SqlitePool) -> Vec<CustomFormatView> {
    let rows = cf_model::list_with_scores(db).await.unwrap_or_default();
    rows.into_iter()
        .map(|row| {
            match cf_service::compile_from_json(&row.json, row.score as i32, row.id) {
                Ok(cf) => CustomFormatView {
                    specs_count: cf.specs.len(),
                    parse_error: None,
                    row,
                },
                Err(e) => CustomFormatView {
                    specs_count: 0,
                    parse_error: Some(e),
                    row,
                },
            }
        })
        .collect()
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
    let custom_formats = load_custom_formats_view(&state.db).await;

    // Prefill the CF edit form only when the query param points at a row
    // that actually exists — stale edit links just fall through to the
    // "Add new" form, which is the safer default.
    let custom_format_edit = match params.edit_id {
        Some(id) => cf_model::get_by_id(&state.db, id).await.ok().flatten(),
        None => None,
    };

    let custom_format_min_score_display = min_score_display(cfg.custom_format_minimum_score);
    let template = SettingsTemplate {
        page: "settings".to_string(),
        tab: normalize_settings_tab(params.tab),
        config: cfg,
        groups,
        suggestions,
        custom_formats,
        custom_format_edit,
        custom_format_min_score_display,
        message: params.msg,
        error: params.err,
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
        // Carried forward from the existing row — edited via the
        // dedicated Custom Formats tab's minimum-score form, not here.
        custom_format_minimum_score: existing_cfg
            .as_ref()
            .map(|c| c.custom_format_minimum_score)
            .unwrap_or(i32::MIN),
        seadex_enabled: if form.tab.as_deref() == Some("quality") || form.tab.is_none() {
            form.seadex_enabled.is_some()
        } else {
            existing_cfg.as_ref().map(|c| c.seadex_enabled).unwrap_or(false)
        },
    };

    let active_tab = normalize_settings_tab(form.tab.clone());

    if let Err(e) = config::save_config(&state.db, &cfg).await {
        logger::error(&state.db, LogCategory::System, "Failed to save settings", &e.to_string()).await;
        let groups = load_groups(&state.db).await;
        let suggestions = load_suggestions(&state.db).await;
        let custom_formats = load_custom_formats_view(&state.db).await;
        let custom_format_min_score_display = min_score_display(cfg.custom_format_minimum_score);
        let template = SettingsTemplate {
            page: "settings".to_string(),
            tab: active_tab,
            config: cfg,
            groups,
            suggestions,
            custom_formats,
            custom_format_edit: None,
            custom_format_min_score_display,
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
    let custom_formats = load_custom_formats_view(&state.db).await;
    let custom_format_min_score_display = min_score_display(cfg.custom_format_minimum_score);
    let template = SettingsTemplate {
        page: "settings".to_string(),
        tab: active_tab,
        config: cfg,
        groups,
        suggestions,
        custom_formats,
        custom_format_edit: None,
        custom_format_min_score_display,
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

// ─────────────────────────────────────────────────────────────────────────
// Custom Formats CRUD
// ─────────────────────────────────────────────────────────────────────────

#[derive(Deserialize, utoipa::ToSchema)]
pub struct CustomFormatUpsertForm {
    /// `None` = create a new row; `Some(n)` = update existing row `n`.
    /// Hidden input on the edit form prefill.
    id: Option<i64>,
    name: String,
    score: i32,
    trash_id: Option<String>,
    json: String,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct CustomFormatDeleteForm {
    id: i64,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct CustomFormatMinScoreForm {
    /// Blank string = clear the floor (`i32::MIN`). Numeric strings are
    /// parsed; anything else falls back to the current value so a fat-
    /// finger save can't silently wipe the user's threshold.
    minimum_score: String,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct CustomFormatImportForm {
    /// Pasted Sonarr v4 CF JSON export — either a single CF object or
    /// an array of them. Each entry compiles through the same
    /// `compile_from_json` path as the create form; failures are
    /// counted and reported but don't abort the whole import.
    payload: String,
}

/// Create or update a Custom Format row. Validates the supplied JSON
/// via `compile_from_json` before touching the database — if the parse
/// fails, the user is bounced back to the CF tab with the error and
/// the edit form re-prefilled from the attempted id so their work
/// isn't lost. On success, rebuilds the compiled-CF cache so the next
/// scoring pass sees the change.
#[utoipa::path(
    post,
    path = "/settings/custom-formats/upsert",
    tag = "Settings",
    summary = "Create or update a Custom Format",
    description = "Upsert a Sonarr v4-compatible Custom Format and its V1-profile score. Validates the JSON via the CF compiler before writing to the database. On success, rebuilds the compiled-CF cache so the next scoring pass sees the change. Redirects back to the Custom Formats settings tab with a flash message.",
    request_body(content = CustomFormatUpsertForm, content_type = "application/x-www-form-urlencoded"),
    responses(
        (status = 303, description = "Redirect back to the Custom Formats settings tab"),
    ),
)]
pub async fn settings_custom_formats_upsert(
    State(state): State<AppState>,
    Form(form): Form<CustomFormatUpsertForm>,
) -> Redirect {
    let name = form.name.trim();
    if name.is_empty() {
        return Redirect::to(&cf_redirect(
            form.id,
            None,
            Some("Custom Format name cannot be blank."),
        ));
    }
    let trash_id = form.trash_id.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty());
    let json_trimmed = form.json.trim();

    // Validate against the real compiler so a CF that would fail to
    // parse on startup also fails to save through the UI.
    if let Err(e) = cf_service::compile_from_json(json_trimmed, form.score, form.id.unwrap_or(0)) {
        return Redirect::to(&cf_redirect(form.id, None, Some(&format!("Parse error: {e}"))));
    }

    let save_result = if let Some(id) = form.id {
        cf_model::update(&state.db, id, name, trash_id, json_trimmed, form.score)
            .await
            .map(|_| id)
    } else {
        cf_model::insert(&state.db, name, trash_id, json_trimmed, form.score).await
    };

    match save_result {
        Ok(id) => {
            cf_service::rebuild_cf_cache(&state.custom_formats, &state.db).await;
            logger::info(
                &state.db,
                LogCategory::System,
                &format!("Custom Format saved: {name} (id={id})"),
                "",
            )
            .await;
            Redirect::to(&cf_redirect(None, Some(&format!("Saved '{name}'.")), None))
        }
        Err(e) => {
            logger::error(
                &state.db,
                LogCategory::System,
                "Custom Format save failed",
                &e.to_string(),
            )
            .await;
            Redirect::to(&cf_redirect(
                form.id,
                None,
                Some(&format!("Database error: {e}")),
            ))
        }
    }
}

/// Delete a Custom Format row by id. Score row is dropped automatically
/// via the `ON DELETE CASCADE` on `custom_format_scores`.
#[utoipa::path(
    post,
    path = "/settings/custom-formats/delete",
    tag = "Settings",
    summary = "Delete a Custom Format",
    description = "Delete a Custom Format row by id. The associated score row is dropped automatically via ON DELETE CASCADE. Rebuilds the compiled-CF cache on success. Redirects back to the Custom Formats settings tab.",
    request_body(content = CustomFormatDeleteForm, content_type = "application/x-www-form-urlencoded"),
    responses(
        (status = 303, description = "Redirect back to the Custom Formats settings tab"),
    ),
)]
pub async fn settings_custom_formats_delete(
    State(state): State<AppState>,
    Form(form): Form<CustomFormatDeleteForm>,
) -> Redirect {
    match cf_model::delete(&state.db, form.id).await {
        Ok(_) => {
            cf_service::rebuild_cf_cache(&state.custom_formats, &state.db).await;
            logger::info(
                &state.db,
                LogCategory::System,
                &format!("Custom Format deleted: id={}", form.id),
                "",
            )
            .await;
            Redirect::to(&cf_redirect(None, Some("Custom Format deleted."), None))
        }
        Err(e) => {
            logger::error(
                &state.db,
                LogCategory::System,
                "Custom Format delete failed",
                &e.to_string(),
            )
            .await;
            Redirect::to(&cf_redirect(
                None,
                None,
                Some(&format!("Delete failed: {e}")),
            ))
        }
    }
}

/// Update the global `custom_format_minimum_score` floor. Blank input
/// clears the floor (sets it back to `i32::MIN`, the "no floor"
/// sentinel). Non-numeric input falls through to the existing value so
/// a typo can't silently wipe the threshold.
#[utoipa::path(
    post,
    path = "/settings/custom-formats/minimum-score",
    tag = "Settings",
    summary = "Set the Custom Format minimum-score floor",
    description = "Update the global minimum-score threshold. Auto-search drops releases whose summed CF score falls below this value; interactive search still shows everything. Blank clears the floor. Redirects back to the Custom Formats settings tab.",
    request_body(content = CustomFormatMinScoreForm, content_type = "application/x-www-form-urlencoded"),
    responses(
        (status = 303, description = "Redirect back to the Custom Formats settings tab"),
    ),
)]
pub async fn settings_custom_formats_minimum_score(
    State(state): State<AppState>,
    Form(form): Form<CustomFormatMinScoreForm>,
) -> Redirect {
    let existing = config::get_config(&state.db).await.ok().flatten();
    let Some(mut cfg) = existing else {
        return Redirect::to(&cf_redirect(None, None, Some("Config not initialized.")));
    };

    let trimmed = form.minimum_score.trim();
    let new_floor = if trimmed.is_empty() {
        i32::MIN
    } else {
        match trimmed.parse::<i32>() {
            Ok(n) => n,
            Err(_) => {
                return Redirect::to(&cf_redirect(
                    None,
                    None,
                    Some("Minimum score must be an integer (leave blank for 'no floor')."),
                ));
            }
        }
    };

    cfg.custom_format_minimum_score = new_floor;
    match config::save_config(&state.db, &cfg).await {
        Ok(_) => {
            let msg = if new_floor == i32::MIN {
                "Minimum score cleared (no floor).".to_string()
            } else {
                format!("Minimum score set to {new_floor}.")
            };
            Redirect::to(&cf_redirect(None, Some(&msg), None))
        }
        Err(e) => Redirect::to(&cf_redirect(None, None, Some(&format!("Save failed: {e}")))),
    }
}

/// Import a Sonarr v4 CF JSON export. Accepts either a single object
/// or an array of them; per-entry parse / insert failures are counted
/// and reported but don't abort the whole import. After a successful
/// run the compiled cache is rebuilt so the imported CFs are live on
/// the next scoring pass.
#[utoipa::path(
    post,
    path = "/settings/custom-formats/import",
    tag = "Settings",
    summary = "Import Custom Formats from Sonarr v4 JSON",
    description = "Import one or more Custom Formats from a Sonarr v4 JSON export. Accepts either a single object or an array. Each entry is compiled via the standard CF parser; per-entry failures are counted and the first error is surfaced in the flash message, but valid entries still import. Rebuilds the compiled-CF cache on success. Redirects back to the Custom Formats settings tab.",
    request_body(content = CustomFormatImportForm, content_type = "application/x-www-form-urlencoded"),
    responses(
        (status = 303, description = "Redirect back to the Custom Formats settings tab"),
    ),
)]
pub async fn settings_custom_formats_import(
    State(state): State<AppState>,
    Form(form): Form<CustomFormatImportForm>,
) -> Redirect {
    let payload = form.payload.trim();
    if payload.is_empty() {
        return Redirect::to(&cf_redirect(None, None, Some("Import payload is empty.")));
    }

    let value: serde_json::Value = match serde_json::from_str(payload) {
        Ok(v) => v,
        Err(e) => {
            return Redirect::to(&cf_redirect(
                None,
                None,
                Some(&format!("Import failed: invalid JSON ({e})")),
            ));
        }
    };

    // Normalize single-object imports into a one-element array so the
    // loop below handles both shapes identically.
    let entries: Vec<serde_json::Value> = match value {
        serde_json::Value::Array(items) => items,
        serde_json::Value::Object(_) => vec![value],
        _ => {
            return Redirect::to(&cf_redirect(
                None,
                None,
                Some("Import failed: top-level must be an object or array."),
            ));
        }
    };

    let mut imported = 0usize;
    let mut failed = 0usize;
    let mut first_error: Option<String> = None;

    for entry in entries {
        // Pull out the Sonarr-shape fields we care about. `specifications`
        // lives inside the same blob we'll persist, so we re-serialize
        // the whole object verbatim to keep round-trip exports faithful.
        let name = entry.get("name").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
        let trash_id = entry
            .get("trash_id")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        // Sonarr exports keep the score inside a sibling `score` or
        // `trash_score` field; both are optional. Default to 0 when
        // neither is present so the user can set the score later.
        let score = entry
            .get("score")
            .and_then(|v| v.as_i64())
            .or_else(|| entry.get("trash_score").and_then(|v| v.as_i64()))
            .unwrap_or(0) as i32;

        if name.is_empty() {
            failed += 1;
            if first_error.is_none() {
                first_error = Some("one entry is missing a `name` field".to_string());
            }
            continue;
        }

        let raw_json = entry.to_string();
        if let Err(e) = cf_service::compile_from_json(&raw_json, score, 0) {
            failed += 1;
            if first_error.is_none() {
                first_error = Some(format!("'{name}': {e}"));
            }
            continue;
        }

        match cf_model::insert(&state.db, &name, trash_id.as_deref(), &raw_json, score).await {
            Ok(_) => imported += 1,
            Err(e) => {
                failed += 1;
                if first_error.is_none() {
                    first_error = Some(format!("'{name}': {e}"));
                }
            }
        }
    }

    if imported > 0 {
        cf_service::rebuild_cf_cache(&state.custom_formats, &state.db).await;
    }

    let summary = match (imported, failed) {
        (0, 0) => "Nothing to import.".to_string(),
        (n, 0) => format!("Imported {n} Custom Format(s)."),
        (0, m) => format!(
            "Import failed ({m} rejected). First error: {}",
            first_error.unwrap_or_default()
        ),
        (n, m) => format!(
            "Imported {n}, skipped {m}. First error: {}",
            first_error.unwrap_or_default()
        ),
    };

    if imported == 0 && failed > 0 {
        Redirect::to(&cf_redirect(None, None, Some(&summary)))
    } else {
        Redirect::to(&cf_redirect(None, Some(&summary), None))
    }
}

/// Export every Custom Format as a JSON array download. The payload
/// keeps each row's raw `json` column verbatim so re-importing the
/// file into Sonarr (or another Ryokan instance) round-trips cleanly.
#[utoipa::path(
    get,
    path = "/settings/custom-formats/export",
    tag = "Settings",
    summary = "Export all Custom Formats as JSON",
    description = "Download every saved Custom Format as a JSON array. Each row's raw Sonarr v4 object is merged with the persisted V1-profile score, so the export round-trips cleanly into Sonarr or another Ryokan instance. Served with `Content-Disposition: attachment; filename=\"ryokan-custom-formats.json\"`.",
    responses(
        (status = 200, description = "JSON array of Custom Formats", body = serde_json::Value, content_type = "application/json"),
        (status = 500, description = "Database error"),
    ),
)]
pub async fn settings_custom_formats_export(
    State(state): State<AppState>,
) -> Result<(axum::http::HeaderMap, String), (axum::http::StatusCode, String)> {
    let rows = cf_model::list_with_scores(&state.db)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Parse each row's stored JSON back into a `Value` so the exported
    // array is real JSON (not a string-of-strings). Unparseable rows
    // are skipped — they wouldn't import cleanly into a target Sonarr
    // anyway, and logging the skip is enough of a breadcrumb.
    let mut out: Vec<serde_json::Value> = Vec::with_capacity(rows.len());
    for row in rows {
        match serde_json::from_str::<serde_json::Value>(&row.json) {
            Ok(mut v) => {
                // Merge the persisted score into the exported object so
                // the importer doesn't have to re-key scores by name.
                // Sonarr ignores unknown fields on import, so this is
                // safe to ship even when re-importing into Sonarr.
                if let serde_json::Value::Object(ref mut map) = v {
                    map.insert(
                        "score".to_string(),
                        serde_json::Value::Number(row.score.into()),
                    );
                }
                out.push(v);
            }
            Err(e) => {
                tracing::warn!(
                    "custom_formats export: skipping id={} name={} — parse error: {}",
                    row.id,
                    row.name,
                    e
                );
            }
        }
    }

    let body = serde_json::to_string_pretty(&serde_json::Value::Array(out))
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );
    headers.insert(
        axum::http::header::CONTENT_DISPOSITION,
        axum::http::HeaderValue::from_static(
            "attachment; filename=\"ryokan-custom-formats.json\"",
        ),
    );
    Ok((headers, body))
}

/// Build a redirect target for the Custom Formats tab. Optionally
/// carries an `edit_id` (to re-open the failed row's form), a success
/// `msg`, or an `err`. Query values are URL-encoded so arbitrary error
/// strings survive the round-trip safely.
fn cf_redirect(edit_id: Option<i64>, msg: Option<&str>, err: Option<&str>) -> String {
    let mut url = String::from("/settings?tab=custom_formats");
    if let Some(id) = edit_id {
        url.push_str(&format!("&edit_id={id}"));
    }
    if let Some(m) = msg {
        url.push_str(&format!("&msg={}", urlencoding::encode(m)));
    }
    if let Some(e) = err {
        url.push_str(&format!("&err={}", urlencoding::encode(e)));
    }
    url
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
