use askama::Template;
use axum::{
    extract::{Query, State},
    response::{Html, IntoResponse, Redirect, Response},
    Form, Json,
};
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

/// View-model wrapper rendered when the Custom Formats tab is in edit
/// mode. Holds the full row plus any `trash_description` extracted
/// from the row's JSON body. Plan §5.7.6 wants descriptions to persist
/// through round-trips and surface in the edit drawer so the user
/// keeps the Trash Guides context that originally shipped the CF.
pub struct CustomFormatEditView {
    pub row: cf_model::CustomFormatRow,
    pub trash_description: Option<String>,
}

/// Parse a stored CF's JSON body and return the `trash_description`
/// string if it's present, non-empty, and a string. Silently returns
/// `None` on parse error — the row itself still renders via the raw
/// `edit.json` textarea, so the description is a nice-to-have, not a
/// blocker.
fn extract_trash_description(json: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let desc = value.get("trash_description")?.as_str()?.trim();
    if desc.is_empty() {
        None
    } else {
        Some(desc.to_string())
    }
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
    custom_format_edit: Option<CustomFormatEditView>,
    /// Pre-rendered string for the minimum-score input. Empty when the
    /// floor is the `i32::MIN` "no floor" sentinel. Computed here so the
    /// Askama template doesn't need to compare against an integer path.
    custom_format_min_score_display: String,
    /// Populated when the import flow hit a name collision. The CF tab
    /// renders a review block with per-collision radio buttons so the
    /// user can pick overwrite/rename/skip for each conflicting CF.
    /// See plan §6.2.
    custom_format_import_review: Option<ImportReviewView>,
    message: Option<String>,
    error: Option<String>,
}

/// Per-collision entry shown on the import review block. `index` is
/// the position of the CF inside the parsed payload so the resolve
/// handler can find the right entry after re-parsing the payload.
pub struct ImportCollision {
    pub index: usize,
    pub name: String,
}

/// View model for the import review block. Holds the original payload
/// (echoed back into a hidden field so the resolve handler can re-parse
/// it) plus the list of collisions the user needs to act on.
pub struct ImportReviewView {
    pub payload: String,
    pub collisions: Vec<ImportCollision>,
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

/// Build a fully-populated `SettingsTemplate` with the same loading
/// logic the main settings page uses. Extracted so the CF import
/// handler can re-render the settings page in place on a name
/// collision without duplicating every DB query the normal page
/// renderer runs. Callers override the `tab`, `edit_id`, `msg`, `err`,
/// and optional import-review fields to tailor the resulting page.
#[allow(clippy::too_many_arguments)]
async fn build_settings_template(
    state: &AppState,
    tab: Option<String>,
    edit_id: Option<i64>,
    msg: Option<String>,
    err: Option<String>,
    import_review: Option<ImportReviewView>,
) -> SettingsTemplate {
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
    let custom_format_edit = match edit_id {
        Some(id) => cf_model::get_by_id(&state.db, id)
            .await
            .ok()
            .flatten()
            .map(|row| {
                let trash_description = extract_trash_description(&row.json);
                CustomFormatEditView {
                    row,
                    trash_description,
                }
            }),
        None => None,
    };

    let custom_format_min_score_display = min_score_display(cfg.custom_format_minimum_score);
    SettingsTemplate {
        page: "settings".to_string(),
        tab: normalize_settings_tab(tab),
        config: cfg,
        groups,
        suggestions,
        custom_formats,
        custom_format_edit,
        custom_format_min_score_display,
        custom_format_import_review: import_review,
        message: msg,
        error: err,
    }
}

pub async fn settings_page(
    State(state): State<AppState>,
    Query(params): Query<SettingsQuery>,
) -> Html<String> {
    let template = build_settings_template(
        &state,
        params.tab,
        params.edit_id,
        params.msg,
        params.err,
        None,
    )
    .await;
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
            custom_format_import_review: None,
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
        custom_format_import_review: None,
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
        cf_model::insert(
            &state.db,
            name,
            trash_id,
            json_trimmed,
            form.score,
            cf_model::ORIGIN_MANUAL,
        )
        .await
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

/// Per-import decision on a name collision (plan §6.2). Derived from
/// the review form's radio-button values. `Skip` keeps the existing
/// row untouched; `Overwrite` replaces it in place; `Rename` writes a
/// new row under the user-supplied rename_to value.
#[derive(Clone, Debug)]
enum CollisionDecision {
    Skip,
    Overwrite,
    Rename(String),
}

/// Loop body shared between the no-collision fast path and the resolve
/// handler. Takes a set of (entry, decision) pairs plus the existing
/// name → id map, and performs the insert / update / skip for each.
/// Returns (imported, skipped_for_collision, failed, first_error).
async fn apply_import_entries(
    state: &AppState,
    entries: Vec<(serde_json::Value, CollisionDecision)>,
    existing_by_name: &std::collections::HashMap<String, i64>,
) -> (usize, usize, usize, Option<String>) {
    let mut imported = 0usize;
    let mut skipped = 0usize;
    let mut failed = 0usize;
    let mut first_error: Option<String> = None;

    for (mut entry, decision) in entries {
        // Pull out the Sonarr-shape fields we care about. `specifications`
        // lives inside the same blob we'll persist, so we re-serialize
        // the whole object verbatim to keep round-trip exports faithful.
        let original_name = entry
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
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

        if original_name.is_empty() {
            failed += 1;
            if first_error.is_none() {
                first_error = Some("one entry is missing a `name` field".to_string());
            }
            continue;
        }

        // Determine the effective name + the existing-row id (if any)
        // we should target based on the user's decision.
        let (effective_name, existing_id) = match decision {
            CollisionDecision::Skip => {
                // User chose to keep the existing row. Count it toward
                // the skipped tally and move on.
                skipped += 1;
                continue;
            }
            CollisionDecision::Overwrite => {
                let id = existing_by_name.get(&original_name).copied();
                (original_name.clone(), id)
            }
            CollisionDecision::Rename(new_name) => {
                let trimmed = new_name.trim();
                if trimmed.is_empty() {
                    failed += 1;
                    if first_error.is_none() {
                        first_error = Some(format!(
                            "'{original_name}': rename target is empty"
                        ));
                    }
                    continue;
                }
                if existing_by_name.contains_key(trimmed) {
                    failed += 1;
                    if first_error.is_none() {
                        first_error = Some(format!(
                            "'{original_name}': rename target '{trimmed}' also collides"
                        ));
                    }
                    continue;
                }
                // Mutate the in-memory JSON so the new name is persisted
                // into both the `name` column and the `json` column —
                // plan §6.2 requires that exports after a rename reflect
                // the new name, not the upstream one.
                if let serde_json::Value::Object(ref mut map) = entry {
                    map.insert(
                        "name".to_string(),
                        serde_json::Value::String(trimmed.to_string()),
                    );
                }
                (trimmed.to_string(), None)
            }
        };

        let raw_json = entry.to_string();
        if let Err(e) = cf_service::compile_from_json(&raw_json, score, 0) {
            failed += 1;
            if first_error.is_none() {
                first_error = Some(format!("'{effective_name}': {e}"));
            }
            continue;
        }

        let save_result = if let Some(id) = existing_id {
            cf_model::update(
                &state.db,
                id,
                &effective_name,
                trash_id.as_deref(),
                &raw_json,
                score,
            )
            .await
            .map(|_| id)
        } else {
            cf_model::insert(
                &state.db,
                &effective_name,
                trash_id.as_deref(),
                &raw_json,
                score,
                cf_model::ORIGIN_IMPORT,
            )
            .await
        };

        match save_result {
            Ok(_) => imported += 1,
            Err(e) => {
                failed += 1;
                if first_error.is_none() {
                    first_error = Some(format!("'{effective_name}': {e}"));
                }
            }
        }
    }

    (imported, skipped, failed, first_error)
}

/// Build a summary flash message from the four counters produced by
/// `apply_import_entries`.
fn import_summary(
    imported: usize,
    skipped: usize,
    failed: usize,
    first_error: Option<String>,
) -> String {
    // Arm order matters: the (0, 0, f) "all-rejected" arm must come
    // BEFORE the general (n, s, f) arm, and it must *not* swallow the
    // skipped count — an earlier version had a `(0, _, f)` arm that
    // silently dropped `skipped` when imported=0 and both skipped and
    // failed were non-zero. The arms below break that case out so
    // every counter combination shows every non-zero counter.
    match (imported, skipped, failed) {
        (0, 0, 0) => "Nothing to import.".to_string(),
        (n, 0, 0) => format!("Imported {n} Custom Format(s)."),
        (n, s, 0) => format!("Imported {n}, skipped {s} on collision."),
        (0, 0, f) => format!(
            "Import failed ({f} rejected). First error: {}",
            first_error.unwrap_or_default()
        ),
        (n, 0, f) => format!(
            "Imported {n}, failed {f}. First error: {}",
            first_error.unwrap_or_default()
        ),
        (n, s, f) => format!(
            "Imported {n}, skipped {s}, failed {f}. First error: {}",
            first_error.unwrap_or_default()
        ),
    }
}

/// Import a Sonarr v4 CF JSON export. Accepts a single object, a bare
/// array, or a `{custom_formats: [...]}` wrapper (plan §6.2). On a
/// name collision the handler re-renders the settings page with an
/// inline review block so the user can pick overwrite / rename / skip
/// per conflicting CF. With no collisions it commits the full batch
/// and redirects back with a flash summary.
#[utoipa::path(
    post,
    path = "/settings/custom-formats/import",
    tag = "Settings",
    summary = "Import Custom Formats from Sonarr v4 JSON",
    description = "Import one or more Custom Formats from a Sonarr v4 JSON export. Accepts a single object, an array, or a `{custom_formats:[…]}` wrapper. On a name collision the page re-renders with an inline review block; the user picks overwrite/rename/skip per conflict and submits to the resolve endpoint. Rebuilds the compiled-CF cache on success.",
    request_body(content = CustomFormatImportForm, content_type = "application/x-www-form-urlencoded"),
    responses(
        (status = 200, description = "Review page rendered (collisions exist)"),
        (status = 303, description = "Redirect back to the Custom Formats settings tab (no collisions)"),
    ),
)]
pub async fn settings_custom_formats_import(
    State(state): State<AppState>,
    Form(form): Form<CustomFormatImportForm>,
) -> Response {
    let payload = form.payload.trim();
    if payload.is_empty() {
        return Redirect::to(&cf_redirect(None, None, Some("Import payload is empty.")))
            .into_response();
    }

    let value: serde_json::Value = match serde_json::from_str(payload) {
        Ok(v) => v,
        Err(e) => {
            return Redirect::to(&cf_redirect(
                None,
                None,
                Some(&format!("Import failed: invalid JSON ({e})")),
            ))
            .into_response();
        }
    };

    let entries: Vec<serde_json::Value> = match normalize_cf_import_entries(value) {
        Ok(entries) => entries,
        Err(msg) => {
            return Redirect::to(&cf_redirect(None, None, Some(&msg))).into_response();
        }
    };

    // Build the name → id map once up-front so both the collision
    // scan and the apply loop can read from it without re-querying.
    let existing_rows = match cf_model::list_with_scores(&state.db).await {
        Ok(rows) => rows,
        Err(e) => {
            return Redirect::to(&cf_redirect(
                None,
                None,
                Some(&format!("Failed to read existing CFs: {e}")),
            ))
            .into_response();
        }
    };
    let existing_by_name: std::collections::HashMap<String, i64> = existing_rows
        .into_iter()
        .map(|r| (r.name, r.id))
        .collect();

    // Scan for collisions before touching the database. If any exist,
    // render the review page inline — the user picks a decision per
    // conflict and submits the resolve form.
    let mut collisions: Vec<ImportCollision> = Vec::new();
    for (idx, entry) in entries.iter().enumerate() {
        let name = entry
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if !name.is_empty() && existing_by_name.contains_key(&name) {
            collisions.push(ImportCollision { index: idx, name });
        }
    }

    if !collisions.is_empty() {
        // Re-render the settings page with the review block populated.
        // The original payload rides along in a hidden form field so the
        // resolve handler can re-parse it without server-side state.
        let review = ImportReviewView {
            payload: payload.to_string(),
            collisions,
        };
        let template = build_settings_template(
            &state,
            Some("custom_formats".to_string()),
            None,
            None,
            None,
            Some(review),
        )
        .await;
        return Html(template.render().unwrap_or_default()).into_response();
    }

    // No collisions — every entry defaults to Overwrite semantics,
    // but since no existing row shares the name, the overwrite branch
    // falls through to a plain insert.
    let decisions: Vec<(serde_json::Value, CollisionDecision)> = entries
        .into_iter()
        .map(|e| (e, CollisionDecision::Overwrite))
        .collect();
    let (imported, skipped, failed, first_error) =
        apply_import_entries(&state, decisions, &existing_by_name).await;

    if imported > 0 {
        cf_service::rebuild_cf_cache(&state.custom_formats, &state.db).await;
    }

    let summary = import_summary(imported, skipped, failed, first_error);
    if imported == 0 && failed > 0 {
        Redirect::to(&cf_redirect(None, None, Some(&summary))).into_response()
    } else {
        Redirect::to(&cf_redirect(None, Some(&summary), None)).into_response()
    }
}

/// Form for the import-resolve step. Carries the original payload
/// verbatim (echoed from a hidden field) plus two parallel lists of
/// actions and rename targets, each keyed by the collision's entry
/// index inside the parsed payload.
#[derive(Deserialize, utoipa::ToSchema)]
pub struct CustomFormatImportResolveForm {
    payload: String,
    /// One entry per collision: `"<index>:<action>"` where action is
    /// `skip`, `overwrite`, or `rename`. Serialized as a newline-
    /// delimited string to avoid the axum Form-extractor's complicated
    /// multi-value handling.
    decisions: String,
    /// One entry per rename collision: `"<index>:<new_name>"`. Only
    /// read when the corresponding `decisions` line has action `rename`.
    /// Also newline-delimited.
    renames: String,
}

/// Parse the newline-delimited decisions string into a HashMap keyed
/// by entry index. Unknown actions are mapped to `Skip` (the safest
/// default) and unknown indices are silently dropped.
fn parse_collision_decisions(
    decisions: &str,
    renames: &str,
) -> std::collections::HashMap<usize, CollisionDecision> {
    let mut rename_map: std::collections::HashMap<usize, String> =
        std::collections::HashMap::new();
    for line in renames.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some((idx_str, new_name)) = line.split_once(':') {
            if let Ok(idx) = idx_str.trim().parse::<usize>() {
                rename_map.insert(idx, new_name.trim().to_string());
            }
        }
    }

    let mut out: std::collections::HashMap<usize, CollisionDecision> =
        std::collections::HashMap::new();
    for line in decisions.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((idx_str, action)) = line.split_once(':') else {
            continue;
        };
        let Ok(idx) = idx_str.trim().parse::<usize>() else {
            continue;
        };
        let decision = match action.trim() {
            "overwrite" => CollisionDecision::Overwrite,
            "rename" => {
                let new_name = rename_map
                    .get(&idx)
                    .cloned()
                    .unwrap_or_default();
                CollisionDecision::Rename(new_name)
            }
            _ => CollisionDecision::Skip,
        };
        out.insert(idx, decision);
    }
    out
}

/// Resolve a staged CF import by applying the user's per-collision
/// decisions to the original payload. The handler re-parses the
/// payload from the hidden form field, looks up each collision's
/// decision by entry index, and runs the same `apply_import_entries`
/// loop as the fast path.
#[utoipa::path(
    post,
    path = "/settings/custom-formats/import-resolve",
    tag = "Settings",
    summary = "Resolve a staged CF import with per-collision decisions",
    description = "Second step of the CF import flow: re-parses the original payload (echoed from a hidden field) and applies the user's overwrite/rename/skip decision for each name collision. Entries with no collision default to plain insert. Rebuilds the compiled-CF cache and redirects back to the Custom Formats settings tab.",
    request_body(content = CustomFormatImportResolveForm, content_type = "application/x-www-form-urlencoded"),
    responses(
        (status = 303, description = "Redirect back to the Custom Formats settings tab"),
    ),
)]
pub async fn settings_custom_formats_import_resolve(
    State(state): State<AppState>,
    Form(form): Form<CustomFormatImportResolveForm>,
) -> Redirect {
    let payload = form.payload.trim();
    if payload.is_empty() {
        return Redirect::to(&cf_redirect(
            None,
            None,
            Some("Import resolve: payload is empty."),
        ));
    }

    let value: serde_json::Value = match serde_json::from_str(payload) {
        Ok(v) => v,
        Err(e) => {
            return Redirect::to(&cf_redirect(
                None,
                None,
                Some(&format!("Import resolve: invalid JSON ({e})")),
            ));
        }
    };

    let entries: Vec<serde_json::Value> = match normalize_cf_import_entries(value) {
        Ok(entries) => entries,
        Err(msg) => {
            return Redirect::to(&cf_redirect(None, None, Some(&msg)));
        }
    };

    let existing_rows = match cf_model::list_with_scores(&state.db).await {
        Ok(rows) => rows,
        Err(e) => {
            return Redirect::to(&cf_redirect(
                None,
                None,
                Some(&format!("Failed to read existing CFs: {e}")),
            ));
        }
    };
    let existing_by_name: std::collections::HashMap<String, i64> = existing_rows
        .into_iter()
        .map(|r| (r.name, r.id))
        .collect();

    let decisions_map = parse_collision_decisions(&form.decisions, &form.renames);

    // Attach a decision to every entry. Entries that aren't in the
    // decisions map weren't collisions in the first place — default
    // them to Overwrite, which falls through to a plain insert since
    // no existing row shares the name.
    let decisions: Vec<(serde_json::Value, CollisionDecision)> = entries
        .into_iter()
        .enumerate()
        .map(|(idx, entry)| {
            let decision = decisions_map
                .get(&idx)
                .cloned()
                .unwrap_or(CollisionDecision::Overwrite);
            (entry, decision)
        })
        .collect();

    let (imported, skipped, failed, first_error) =
        apply_import_entries(&state, decisions, &existing_by_name).await;

    if imported > 0 {
        cf_service::rebuild_cf_cache(&state.custom_formats, &state.db).await;
    }

    let summary = import_summary(imported, skipped, failed, first_error);
    if imported == 0 && failed > 0 {
        Redirect::to(&cf_redirect(None, None, Some(&summary)))
    } else {
        Redirect::to(&cf_redirect(None, Some(&summary), None))
    }
}

/// Counts returned by `install_default_cfs_core`. Shared between the
/// install-defaults and reset-defaults handlers so the summary-string
/// construction can live in one place.
struct InstallDefaultsReport {
    installed: usize,
    skipped: usize,
    failed: usize,
    first_error: Option<String>,
}

/// The raw bundled-defaults JSON baked into the binary at compile time
/// via `include_str!`. Parsed once per install/reset click rather than
/// at startup — the payload is a few KB, the user rarely clicks this,
/// and compile-time validation doesn't catch field-level typos anyway.
const DEFAULTS_JSON: &str = include_str!("../../static/default_custom_formats.json");

/// Parse the bundled `static/default_custom_formats.json` into a Vec of
/// CF entry values. Shared by install-defaults and reset-defaults so
/// they fail the same way on a malformed defaults file.
fn parse_default_cf_entries() -> Result<Vec<serde_json::Value>, String> {
    let value: serde_json::Value = serde_json::from_str(DEFAULTS_JSON)
        .map_err(|e| format!("Defaults file is malformed: {e}"))?;
    match value {
        serde_json::Value::Array(items) => Ok(items),
        _ => Err("Defaults file is not a JSON array.".to_string()),
    }
}

/// Loop over parsed defaults entries and insert each one with
/// `ORIGIN_DEFAULTS` within the caller's transaction. A compile error
/// on one entry bumps `report.failed` and continues — individual
/// entries are independent. A propagated `sqlx::Error` (connection
/// lost, constraint violation, etc.) short-circuits via `?` so the
/// caller can roll back by dropping the transaction without commit.
///
/// `existing_names` pre-seeds the collision-skip set. Reset Defaults
/// passes an empty set because it already dropped the defaults rows;
/// Install Defaults passes whatever `list_with_scores` returned before
/// the transaction started.
async fn install_defaults_entries_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    entries: Vec<serde_json::Value>,
    existing_names: &std::collections::HashSet<String>,
    report: &mut InstallDefaultsReport,
) -> Result<(), sqlx::Error> {
    for entry in entries {
        let name = entry
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if name.is_empty() {
            report.failed += 1;
            if report.first_error.is_none() {
                report.first_error = Some("defaults entry missing `name`".to_string());
            }
            continue;
        }
        if existing_names.contains(&name) {
            report.skipped += 1;
            continue;
        }
        let trash_id = entry
            .get("trash_id")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let score = entry
            .get("score")
            .and_then(|v| v.as_i64())
            .unwrap_or(0) as i32;

        let raw_json = entry.to_string();
        if let Err(e) = cf_service::compile_from_json(&raw_json, score, 0) {
            report.failed += 1;
            if report.first_error.is_none() {
                report.first_error = Some(format!("'{name}': {e}"));
            }
            continue;
        }

        cf_model::insert_with_tx(
            tx,
            &name,
            trash_id.as_deref(),
            &raw_json,
            score,
            cf_model::ORIGIN_DEFAULTS,
        )
        .await?;
        report.installed += 1;
    }
    Ok(())
}

/// Do the heavy lifting of loading the bundled defaults file, looping
/// over entries, and inserting each one with `ORIGIN_DEFAULTS`. Returns
/// either counts (even when `installed == 0` — e.g. the user clicked
/// install-defaults a second time) or a fatal error string that the
/// caller should surface as a flash message. The caller is responsible
/// for rebuilding the compiled-CF cache if `installed > 0`.
///
/// Wraps the whole insert loop in a single transaction so a propagated
/// sqlx error mid-loop rolls back whatever was inserted rather than
/// leaving a half-populated default set.
async fn install_default_cfs_core(state: &AppState) -> Result<InstallDefaultsReport, String> {
    let entries = parse_default_cf_entries()?;

    // Collect existing names so we can skip conflicts without a
    // per-entry SELECT round-trip. Read outside the transaction —
    // there's no UI for concurrent CF edits, and INSERT will fail
    // loudly on an unexpected collision anyway.
    let existing: std::collections::HashSet<String> = cf_model::list_with_scores(&state.db)
        .await
        .map_err(|e| format!("Failed to read existing CFs: {e}"))?
        .into_iter()
        .map(|r| r.name)
        .collect();

    let mut report = InstallDefaultsReport {
        installed: 0,
        skipped: 0,
        failed: 0,
        first_error: None,
    };

    let mut tx = state
        .db
        .begin()
        .await
        .map_err(|e| format!("Failed to open transaction: {e}"))?;
    install_defaults_entries_tx(&mut tx, entries, &existing, &mut report)
        .await
        .map_err(|e| format!("Install failed mid-loop: {e}"))?;
    tx.commit()
        .await
        .map_err(|e| format!("Failed to commit install transaction: {e}"))?;

    Ok(report)
}

/// Drop every `defaults`-origin row and reinstall the bundled set in
/// the SAME transaction, so a mid-loop sqlx error rolls the delete
/// back too — Reset leaves either the old defaults or the fresh
/// defaults, never nothing. Returns (deleted_count, report). Rebuild
/// of the compiled-CF cache is the caller's responsibility.
async fn reset_defaults_core(
    state: &AppState,
) -> Result<(u64, InstallDefaultsReport), String> {
    let entries = parse_default_cf_entries()?;

    let mut report = InstallDefaultsReport {
        installed: 0,
        skipped: 0,
        failed: 0,
        first_error: None,
    };

    let mut tx = state
        .db
        .begin()
        .await
        .map_err(|e| format!("Failed to open transaction: {e}"))?;

    let deleted = cf_model::delete_defaults_with_tx(&mut tx)
        .await
        .map_err(|e| format!("Reset failed (delete step): {e}"))?;

    // After the delete, every remaining CF is manual/import — those
    // names are still honored so the reinstall doesn't clobber a
    // user-authored CF that happens to share a name with a default.
    let existing: std::collections::HashSet<String> =
        sqlx::query_scalar::<_, String>("SELECT name FROM custom_formats")
            .fetch_all(&mut *tx)
            .await
            .map_err(|e| format!("Failed to read existing CFs: {e}"))?
            .into_iter()
            .collect();

    install_defaults_entries_tx(&mut tx, entries, &existing, &mut report)
        .await
        .map_err(|e| format!("Reset failed (install step): {e}"))?;

    tx.commit()
        .await
        .map_err(|e| format!("Failed to commit reset transaction: {e}"))?;

    Ok((deleted, report))
}

/// Install the bundled default Custom Format set (plan §7.2). The JSON
/// file lives at `static/default_custom_formats.json` and is embedded in
/// the binary via `include_str!` so the handler doesn't touch the
/// filesystem at runtime — the file is baked at compile time like every
/// other `static/` asset in Ryokan. Existing CFs with the same name are
/// skipped (not overwritten), so clicking the button twice is a no-op
/// on the second click. Installing is opt-in: the defaults ship dormant,
/// the user has to click through on the settings page to get them.
#[utoipa::path(
    post,
    path = "/settings/custom-formats/install-defaults",
    tag = "Settings",
    summary = "Install the bundled default Custom Format set",
    description = "One-click install for the bundled anime-tuned default CF set (see plan §7.2). Reads from the compile-time embedded `static/default_custom_formats.json`. CFs whose name already exists are skipped (not overwritten), so repeated clicks are idempotent. Rebuilds the compiled-CF cache on success. Redirects back to the Custom Formats settings tab with a flash message.",
    responses(
        (status = 303, description = "Redirect back to the Custom Formats settings tab"),
    ),
)]
pub async fn settings_custom_formats_install_defaults(
    State(state): State<AppState>,
) -> Redirect {
    let report = match install_default_cfs_core(&state).await {
        Ok(r) => r,
        Err(msg) => return Redirect::to(&cf_redirect(None, None, Some(&msg))),
    };

    if report.installed > 0 {
        cf_service::rebuild_cf_cache(&state.custom_formats, &state.db).await;
        logger::info(
            &state.db,
            LogCategory::System,
            &format!(
                "Installed {} default Custom Format(s)",
                report.installed
            ),
            &format!("skipped={}, failed={}", report.skipped, report.failed),
        )
        .await;
    }

    let summary = match (report.installed, report.skipped, report.failed) {
        (0, _, 0) => "All defaults already present — nothing to install.".to_string(),
        (n, 0, 0) => format!("Installed {n} default Custom Format(s)."),
        (n, s, 0) => format!("Installed {n}, skipped {s} already-present."),
        (0, _, f) => format!(
            "Install failed ({f} rejected). First error: {}",
            report.first_error.unwrap_or_default()
        ),
        (n, s, f) => format!(
            "Installed {n}, skipped {s}, failed {f}. First error: {}",
            report.first_error.unwrap_or_default()
        ),
    };

    if report.installed == 0 && report.failed > 0 {
        Redirect::to(&cf_redirect(None, None, Some(&summary)))
    } else {
        Redirect::to(&cf_redirect(None, Some(&summary), None))
    }
}

/// Reset the bundled default Custom Format set. Deletes every row whose
/// origin is `defaults` (leaving `manual` and `import` rows untouched),
/// then re-runs the install-defaults body so the bundled set lands on
/// disk in its current shape. This is the user's escape hatch if they
/// changed a default CF and want the original score/spec list back.
#[utoipa::path(
    post,
    path = "/settings/custom-formats/reset-defaults",
    tag = "Settings",
    summary = "Reset the bundled default Custom Format set",
    description = "Drops every CF row whose origin is `defaults` and reinstalls the bundled set from `static/default_custom_formats.json`. User-authored (`manual`) and imported (`import`) rows are left untouched. Rebuilds the compiled-CF cache on success. Redirects back to the Custom Formats settings tab with a flash message.",
    responses(
        (status = 303, description = "Redirect back to the Custom Formats settings tab"),
    ),
)]
pub async fn settings_custom_formats_reset_defaults(
    State(state): State<AppState>,
) -> Redirect {
    // Delete + reinstall run inside a single transaction so a mid-loop
    // failure rolls the whole thing back — the user is never left with
    // a partially-nuked default set.
    let (deleted, report) = match reset_defaults_core(&state).await {
        Ok(pair) => pair,
        Err(msg) => return Redirect::to(&cf_redirect(None, None, Some(&msg))),
    };

    // Always rebuild the cache here — either a row was dropped or a
    // fresh one was inserted, and in the edge case where both are
    // zero (a user with an empty database hit the button), rebuilding
    // a cache over zero rows is effectively free.
    cf_service::rebuild_cf_cache(&state.custom_formats, &state.db).await;
    logger::info(
        &state.db,
        LogCategory::System,
        &format!(
            "Reset defaults: dropped {} old, installed {} fresh",
            deleted, report.installed
        ),
        &format!(
            "skipped={}, failed={}",
            report.skipped, report.failed
        ),
    )
    .await;

    let summary = if report.failed > 0 {
        format!(
            "Reset: dropped {}, installed {}, failed {}. First error: {}",
            deleted,
            report.installed,
            report.failed,
            report.first_error.unwrap_or_default()
        )
    } else {
        format!(
            "Reset complete: dropped {} old default(s), installed {} fresh.",
            deleted, report.installed
        )
    };

    if report.failed > 0 && report.installed == 0 {
        Redirect::to(&cf_redirect(None, None, Some(&summary)))
    } else {
        Redirect::to(&cf_redirect(None, Some(&summary), None))
    }
}

/// Query parameters for the CF export endpoint. `mode` selects between
/// the default Ryokan-compatible export (keeps `Ryokan.`-namespaced
/// specs verbatim) and the Sonarr-safe variant (drops entire CFs that
/// contain any Ryokan-only spec so the file imports cleanly into a
/// vanilla Sonarr v4 instance). See plan §5.7.5.
#[derive(Deserialize)]
pub struct CfExportQuery {
    /// `"sonarr-safe"` triggers the Sonarr-safe branch; anything else
    /// (or absent) falls through to the default Ryokan-compatible mode.
    mode: Option<String>,
}

/// Normalize a parsed CF import payload into a flat list of per-CF
/// entries. Plan §6.2 requires that every shape Sonarr v4 might emit
/// imports cleanly:
///
/// - `[{…}, {…}]`         — bare array (what Sonarr's "Export" emits)
/// - `{…}`                 — bare single object
/// - `{"custom_formats": [{…}, {…}]}` — wrapped array (some third-party tools)
/// - `{"custom_formats": {…}}`        — wrapped single object (paranoid fallback)
///
/// Returns a human-readable error string that can be surfaced as a
/// flash message on the settings redirect.
fn normalize_cf_import_entries(
    value: serde_json::Value,
) -> Result<Vec<serde_json::Value>, String> {
    match value {
        serde_json::Value::Array(items) => Ok(items),
        serde_json::Value::Object(ref map) => {
            if let Some(inner) = map.get("custom_formats") {
                match inner {
                    serde_json::Value::Array(items) => Ok(items.clone()),
                    serde_json::Value::Object(_) => Ok(vec![inner.clone()]),
                    _ => Err(
                        "Import failed: `custom_formats` must be an object or array."
                            .to_string(),
                    ),
                }
            } else {
                // Bare single-CF object (legacy shape).
                Ok(vec![value])
            }
        }
        _ => Err("Import failed: top-level must be an object or array.".to_string()),
    }
}

/// Returns `true` if this CF's `specifications` array contains any spec
/// whose `implementation` begins with `"Ryokan."` — i.e. a Ryokan-only
/// kind that a vanilla Sonarr v4 install wouldn't recognize.
fn cf_has_ryokan_spec(cf: &serde_json::Value) -> bool {
    let Some(specs) = cf.get("specifications").and_then(|v| v.as_array()) else {
        return false;
    };
    specs.iter().any(|spec| {
        spec.get("implementation")
            .and_then(|v| v.as_str())
            .map(|s| s.starts_with("Ryokan."))
            .unwrap_or(false)
    })
}

/// Export every Custom Format as a JSON array download. Supports two
/// modes via the `mode` query parameter:
///
/// - **Ryokan-compatible** (default): keeps each row's raw `json`
///   column verbatim, so the file round-trips into another Ryokan
///   instance with `Ryokan.`-namespaced specs intact.
/// - **Sonarr-safe** (`?mode=sonarr-safe`): drops any CF containing a
///   `Ryokan.`-prefixed spec so the remainder imports cleanly into a
///   vanilla Sonarr v4 instance. Dropped CF names are written to the
///   System log category so the user can see what was stripped.
#[utoipa::path(
    get,
    path = "/settings/custom-formats/export",
    tag = "Settings",
    summary = "Export all Custom Formats as JSON",
    description = "Download every saved Custom Format as a JSON array. Default mode keeps Ryokan-namespaced specs verbatim; `?mode=sonarr-safe` drops entire CFs containing `Ryokan.`-only specs so the file imports into vanilla Sonarr v4. Each row's persisted V1-profile score is merged into the exported object.",
    params(
        ("mode" = Option<String>, Query, description = "`sonarr-safe` to drop Ryokan-only CFs"),
    ),
    responses(
        (status = 200, description = "JSON array of Custom Formats", body = serde_json::Value, content_type = "application/json"),
        (status = 500, description = "Database error"),
    ),
)]
pub async fn settings_custom_formats_export(
    State(state): State<AppState>,
    Query(query): Query<CfExportQuery>,
) -> Result<(axum::http::HeaderMap, String), (axum::http::StatusCode, String)> {
    let sonarr_safe = query
        .mode
        .as_deref()
        .map(|s| s.eq_ignore_ascii_case("sonarr-safe"))
        .unwrap_or(false);

    let rows = cf_model::list_with_scores(&state.db)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Parse each row's stored JSON back into a `Value` so the exported
    // array is real JSON (not a string-of-strings). Unparseable rows
    // are skipped — they wouldn't import cleanly into a target Sonarr
    // anyway, and logging the skip is enough of a breadcrumb.
    let mut out: Vec<serde_json::Value> = Vec::with_capacity(rows.len());
    let mut dropped_for_sonarr: Vec<String> = Vec::new();
    for row in rows {
        match serde_json::from_str::<serde_json::Value>(&row.json) {
            Ok(mut v) => {
                // In Sonarr-safe mode, drop the whole CF if any spec
                // uses a `Ryokan.`-prefixed implementation. This is the
                // conservative reading of plan §5.7.5 — partial strips
                // would change the CF's semantics silently, so whole-CF
                // drops are safer even if they produce a smaller file.
                if sonarr_safe && cf_has_ryokan_spec(&v) {
                    dropped_for_sonarr.push(row.name.clone());
                    continue;
                }

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

    // Surface dropped CFs to the Logs tab (System category) so the user
    // can see what was stripped. The export response itself is a file
    // download, so there's no flash-message slot to use here — the log
    // entry + the form-hint on the settings page is the whole UX path.
    if sonarr_safe && !dropped_for_sonarr.is_empty() {
        logger::info(
            &state.db,
            LogCategory::System,
            &format!(
                "Sonarr-safe CF export dropped {} CF(s) containing Ryokan-only specs",
                dropped_for_sonarr.len()
            ),
            &dropped_for_sonarr.join(", "),
        )
        .await;
    }

    let body = serde_json::to_string_pretty(&serde_json::Value::Array(out))
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );
    let disposition = if sonarr_safe {
        "attachment; filename=\"ryokan-custom-formats-sonarr-safe.json\""
    } else {
        "attachment; filename=\"ryokan-custom-formats.json\""
    };
    headers.insert(
        axum::http::header::CONTENT_DISPOSITION,
        axum::http::HeaderValue::from_static(disposition),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A CF whose specs are all vanilla Sonarr v4 implementations must
    /// be kept in Sonarr-safe mode (plan §5.7.5).
    #[test]
    fn cf_has_ryokan_spec_returns_false_for_pure_sonarr_cf() {
        let cf = serde_json::json!({
            "name": "Synthetic BluRay CF",
            "specifications": [
                {
                    "name": "Is BluRay",
                    "implementation": "SourceSpecification",
                    "negate": false,
                    "required": true,
                    "fields": [{"name": "value", "value": 6}]
                }
            ]
        });
        assert!(!cf_has_ryokan_spec(&cf));
    }

    /// Any `Ryokan.`-prefixed `implementation` must flip the flag so
    /// the whole CF gets dropped from a Sonarr-safe export.
    #[test]
    fn cf_has_ryokan_spec_returns_true_when_any_spec_is_ryokan_only() {
        let cf = serde_json::json!({
            "name": "SeaDex Best",
            "specifications": [
                {
                    "name": "Is BluRay",
                    "implementation": "SourceSpecification",
                    "negate": false,
                    "required": false,
                    "fields": [{"name": "value", "value": 6}]
                },
                {
                    "name": "SeaDex best",
                    "implementation": "Ryokan.SeaDexBestSpecification",
                    "negate": false,
                    "required": true,
                    "fields": []
                }
            ]
        });
        assert!(cf_has_ryokan_spec(&cf));
    }

    /// Guard against CFs with an empty or missing `specifications`
    /// array — the helper must default to `false` rather than panic.
    #[test]
    fn cf_has_ryokan_spec_handles_missing_or_empty_specs() {
        let empty = serde_json::json!({ "name": "Empty", "specifications": [] });
        assert!(!cf_has_ryokan_spec(&empty));

        let missing = serde_json::json!({ "name": "Missing" });
        assert!(!cf_has_ryokan_spec(&missing));
    }

    /// Case sensitivity: `Ryokan.` is the exact namespace prefix.
    /// A spec named `ryokan.…` (lowercase) shouldn't match and
    /// neither should a spec that merely contains `Ryokan` somewhere
    /// in the middle of the string.
    #[test]
    fn cf_has_ryokan_spec_requires_exact_prefix() {
        let cf = serde_json::json!({
            "name": "Edge",
            "specifications": [
                {
                    "implementation": "ryokan.SeaDexBestSpecification",
                    "fields": []
                },
                {
                    "implementation": "SomeRyokanThing",
                    "fields": []
                }
            ]
        });
        assert!(!cf_has_ryokan_spec(&cf));
    }

    /// A bare array of CF objects should pass through untouched — this
    /// is the exact shape Sonarr v4's "Export" button emits.
    #[test]
    fn normalize_cf_import_entries_accepts_bare_array() {
        let input = serde_json::json!([
            {"name": "First", "specifications": []},
            {"name": "Second", "specifications": []},
        ]);
        let entries = normalize_cf_import_entries(input).expect("array shape must be accepted");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["name"], "First");
        assert_eq!(entries[1]["name"], "Second");
    }

    /// A bare single object should be wrapped in a one-element vec so
    /// the caller loop handles it identically to the array shape.
    #[test]
    fn normalize_cf_import_entries_wraps_bare_single_object() {
        let input = serde_json::json!({"name": "Solo", "specifications": []});
        let entries = normalize_cf_import_entries(input).expect("object shape must be accepted");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["name"], "Solo");
    }

    /// The `{custom_formats: [...]}` wrapper is what some third-party
    /// export tools emit. It must be unwrapped transparently.
    #[test]
    fn normalize_cf_import_entries_unwraps_custom_formats_wrapper() {
        let input = serde_json::json!({
            "custom_formats": [
                {"name": "Wrapped One", "specifications": []},
                {"name": "Wrapped Two", "specifications": []},
            ]
        });
        let entries = normalize_cf_import_entries(input).expect("wrapper shape must be accepted");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["name"], "Wrapped One");
        assert_eq!(entries[1]["name"], "Wrapped Two");
    }

    /// The wrapper shape with a single-object inner value should also
    /// unwrap and wrap-as-vec in one step.
    #[test]
    fn normalize_cf_import_entries_unwraps_single_object_inside_wrapper() {
        let input = serde_json::json!({
            "custom_formats": {"name": "Wrapped Solo", "specifications": []}
        });
        let entries = normalize_cf_import_entries(input).expect("wrapped object must be accepted");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["name"], "Wrapped Solo");
    }

    /// Scalar top-level values (string, number, bool, null) must be
    /// rejected with a human-readable error string.
    #[test]
    fn normalize_cf_import_entries_rejects_scalar_top_level() {
        let err = normalize_cf_import_entries(serde_json::json!("not a cf"))
            .expect_err("scalar must be rejected");
        assert!(err.contains("top-level"), "got error: {err}");
    }

    /// If the wrapper's inner value is a scalar, that's a malformed
    /// payload — report it rather than silently treating it as empty.
    #[test]
    fn normalize_cf_import_entries_rejects_scalar_inner_wrapper() {
        let input = serde_json::json!({"custom_formats": 42});
        let err =
            normalize_cf_import_entries(input).expect_err("scalar inside wrapper must be rejected");
        assert!(err.contains("custom_formats"), "got error: {err}");
    }

    /// A Sonarr/Trash Guides CF that carries a `trash_description`
    /// should be surfaced verbatim so the edit drawer can render it.
    #[test]
    fn extract_trash_description_returns_string_when_present() {
        let json = serde_json::json!({
            "name": "Example",
            "trash_description": "This CF matches high-quality BluRay releases.",
            "specifications": []
        })
        .to_string();
        assert_eq!(
            extract_trash_description(&json),
            Some("This CF matches high-quality BluRay releases.".to_string())
        );
    }

    /// Absent, empty, whitespace-only, wrong-typed, or unparseable
    /// payloads should all return `None` so the template simply
    /// doesn't render the description block.
    #[test]
    fn extract_trash_description_returns_none_for_missing_or_invalid() {
        let no_field = serde_json::json!({"name": "X", "specifications": []}).to_string();
        assert_eq!(extract_trash_description(&no_field), None);

        let empty = serde_json::json!({"trash_description": ""}).to_string();
        assert_eq!(extract_trash_description(&empty), None);

        let whitespace = serde_json::json!({"trash_description": "   "}).to_string();
        assert_eq!(extract_trash_description(&whitespace), None);

        let wrong_type = serde_json::json!({"trash_description": 42}).to_string();
        assert_eq!(extract_trash_description(&wrong_type), None);

        assert_eq!(extract_trash_description("not json at all"), None);
    }

    /// A well-formed decisions string with all three action types
    /// should round-trip into the expected `CollisionDecision` enum
    /// values keyed by index.
    #[test]
    fn parse_collision_decisions_handles_all_three_actions() {
        let decisions = "0:skip\n1:overwrite\n2:rename";
        let renames = "2:My New Name";
        let out = parse_collision_decisions(decisions, renames);

        assert_eq!(out.len(), 3);
        assert!(matches!(out.get(&0), Some(CollisionDecision::Skip)));
        assert!(matches!(out.get(&1), Some(CollisionDecision::Overwrite)));
        match out.get(&2) {
            Some(CollisionDecision::Rename(name)) => assert_eq!(name, "My New Name"),
            other => panic!("expected Rename, got {other:?}"),
        }
    }

    /// Unknown action strings should fall back to `Skip` — the safest
    /// default, since `Skip` never touches the existing row.
    #[test]
    fn parse_collision_decisions_treats_unknown_actions_as_skip() {
        let out = parse_collision_decisions("7:nonsense", "");
        assert!(matches!(out.get(&7), Some(CollisionDecision::Skip)));
    }

    /// Malformed lines (missing colon, non-numeric index, blank) must
    /// be silently dropped so a malformed hidden field doesn't blow
    /// up the whole resolve handler.
    #[test]
    fn parse_collision_decisions_drops_malformed_lines() {
        let decisions = "\n  \nnot_a_number:skip\nmissing_colon\n3:overwrite";
        let out = parse_collision_decisions(decisions, "");
        assert_eq!(out.len(), 1);
        assert!(matches!(out.get(&3), Some(CollisionDecision::Overwrite)));
    }

    /// If the user picks `rename` but the renames block is missing
    /// the corresponding entry, the resolved `Rename` value should
    /// carry an empty string — the apply loop then records an
    /// actionable error and the user can fix it.
    #[test]
    fn parse_collision_decisions_rename_without_entry_has_empty_name() {
        let out = parse_collision_decisions("5:rename", "");
        match out.get(&5) {
            Some(CollisionDecision::Rename(name)) => assert!(name.is_empty()),
            other => panic!("expected Rename with empty name, got {other:?}"),
        }
    }

    /// Summary line shape for the common counter combinations. Keeping
    /// these in tests because the summary is user-visible flash text
    /// and regressions here show up as confusing UI.
    #[test]
    fn import_summary_shapes_by_counter_combinations() {
        assert_eq!(import_summary(0, 0, 0, None), "Nothing to import.");
        assert_eq!(import_summary(3, 0, 0, None), "Imported 3 Custom Format(s).");
        assert_eq!(
            import_summary(2, 1, 0, None),
            "Imported 2, skipped 1 on collision."
        );
        assert_eq!(
            import_summary(0, 0, 2, Some("oops".to_string())),
            "Import failed (2 rejected). First error: oops"
        );
        assert_eq!(
            import_summary(1, 1, 1, Some("bad".to_string())),
            "Imported 1, skipped 1, failed 1. First error: bad"
        );
    }

    // Regression for PR #9 review: an earlier version of `import_summary`
    // had a `(0, _, f)` catch-all arm that silently dropped `skipped`
    // when imported=0, skipped>0, and failed>0. All three counters must
    // appear in the flash message so the user knows what happened.
    #[test]
    fn import_summary_preserves_skipped_count_when_imported_is_zero() {
        let msg = import_summary(0, 1, 1, Some("regex".to_string()));
        assert!(msg.contains("skipped 1"), "missing skipped count: {msg}");
        assert!(msg.contains("failed 1"), "missing failed count: {msg}");
        assert!(msg.contains("regex"), "missing error context: {msg}");
    }

    #[test]
    fn import_summary_shapes_imports_with_failures_only() {
        // (n>0, s=0, f>0) — distinct from both (n, s, 0) and (n, s>0, f).
        assert_eq!(
            import_summary(2, 0, 1, Some("oops".to_string())),
            "Imported 2, failed 1. First error: oops"
        );
    }
}
