use askama::Template;
use axum::{
    Form,
    extract::{Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Json, Response},
};
use serde::Deserialize;
use std::collections::VecDeque;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::AppState;
use crate::handlers::library::reconcile::populate_series_cover_urls;
use crate::models::{
    config, episode_tags,
    log::{self, LogCategory, LogLevel},
    rss, scheduled_tasks,
};
use crate::services::{logger, metadata_sync, post_processing, rss as rss_service, upgrade};

#[derive(Template)]
#[template(path = "system.html")]
struct SystemTemplate {
    page: String,
    tab: String,
    force_mal_fallback: bool,
    force_kitsu_fallback: bool,
    auto_grab_on_add: bool,
    allow_non_english: bool,
    debug_message: Option<String>,
    debug_error: Option<String>,
    logs: Vec<log::LogEntry>,
    log_count: i64,
    filter_level: String,
    filter_category: String,
    filter_search: String,
    /// Current page's cursor (the `before_id` query param value, or
    /// `None` for the first/newest page). Used in the template to
    /// render the "Newest" reset link conditionally.
    log_before_id: Option<i64>,
    /// Cursor for the "Older →" link, set to the `id` of the oldest
    /// entry on the current page. `None` when the page is the last
    /// (or when there are no entries at all).
    log_older_id: Option<i64>,
    /// Mirrors `log_before_id` for the RSS tab — the active cursor
    /// the user navigated to (drives the "← Newest" link).
    rss_before_id: Option<i64>,
    /// Mirrors `log_older_id` for the RSS tab — the next-page cursor
    /// when there's more history beyond the current page.
    rss_older_id: Option<i64>,
    categories: Vec<(&'static str, &'static str)>,
    rss_enabled: bool,
    rss_interval_minutes: i32,
    rss_last_run: Option<rss::RssRun>,
    rss_recent: Vec<rss::RssDecision>,
    scheduled_tasks: Vec<scheduled_tasks::ScheduledTaskStatus>,
    /// Cross-library episodes currently flagged `needs_review`. Only
    /// populated when `tab == "review"`; empty on every other tab so
    /// the serial fan-out stays cheap.
    review_entries: Vec<episode_tags::NeedsReviewEntry>,
    title_language: String,
}

#[derive(Deserialize)]
pub struct SystemQuery {
    tab: Option<String>,
    level: Option<String>,
    category: Option<String>,
    search: Option<String>,
    message: Option<String>,
    error: Option<String>,
    /// Cursor for "Older →" pagination on the logs tab. When set,
    /// the query fetches entries with `id < before_id`. Omitted on
    /// the first page so the user always lands on the newest
    /// entries.
    before_id: Option<i64>,
}

#[derive(Deserialize)]
pub struct DebugSettingsForm {
    force_mal_fallback: Option<String>,
    force_kitsu_fallback: Option<String>,
    auto_grab_on_add: Option<String>,
    allow_non_english: Option<String>,
}

fn normalize_system_tab(tab: Option<String>) -> String {
    match tab.as_deref() {
        Some("scoring") => "scoring".to_string(),
        Some("help") => "scoring".to_string(), // legacy alias
        Some("debug") => "debug".to_string(),
        Some("rss") => "rss".to_string(),
        Some("tasks") => "tasks".to_string(),
        Some("review") => "review".to_string(),
        Some("credits") => "credits".to_string(),
        _ => "logs".to_string(),
    }
}

#[cfg(test)]
pub(crate) fn normalize_system_tab_for_test(tab: Option<String>) -> String {
    normalize_system_tab(tab)
}

/// Apply the `+1-fetch` cursor pagination contract: the model fetched
/// `page_size + 1` rows, this helper truncates the extra one and
/// returns its now-last entry's id as the cursor for the next page.
/// `None` means "no older page" — either the dataset was smaller
/// than `page_size + 1`, or the model returned exactly `page_size`
/// (no extra row, no next page).
///
/// The strict `> page_size` (not `>=`) is the load-bearing
/// invariant: a `>=` here would return a non-empty cursor on the
/// last page, the user would click "Older" and see an empty page.
/// Pinned by `truncate_to_page_returns_none_at_exact_page_size`
/// and `truncate_to_page_returns_some_when_extra_row_present`.
fn truncate_to_page<T, F: Fn(&T) -> i64>(
    mut entries: Vec<T>,
    page_size: usize,
    id_of: F,
) -> (Vec<T>, Option<i64>) {
    let older_id = if entries.len() > page_size {
        entries.truncate(page_size);
        entries.last().map(&id_of)
    } else {
        None
    };
    (entries, older_id)
}

pub async fn system_page(
    State(state): State<AppState>,
    Query(params): Query<SystemQuery>,
) -> Html<String> {
    let tab = normalize_system_tab(params.tab.clone());

    let filter_level = params.level.unwrap_or_else(|| "info".to_string());
    let filter_category = params.category.unwrap_or_default();
    let filter_search = params.search.unwrap_or_default();

    // Fan out every independent lookup in parallel. The previous code ran
    // these six queries sequentially — the wall time was the sum of all
    // RTTs. With `tokio::join!` each future races on its own pool
    // connection and the handler waits on the slowest one only.
    let logs_before_id = params.before_id;
    let logs_fut = async {
        if tab == "logs" {
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
                    // Fetch one extra row so the template can tell
                    // whether there's an "Older" page to link to
                    // (without a separate COUNT query). Drop the
                    // extra below before passing to the template.
                    limit: 201,
                    before_id: logs_before_id,
                },
            )
            .await
            .unwrap_or_default()
        } else {
            Vec::new()
        }
    };
    let rss_before_id = params.before_id;
    let rss_recent_fut = async {
        if tab == "rss" {
            // Same +1 trick the logs query uses: fetch one extra row
            // so we can tell whether "Older →" should render without
            // a separate COUNT query. Truncated below.
            rss::recent_decisions_paginated(&state.db, 201, rss_before_id)
                .await
                .unwrap_or_default()
        } else {
            Vec::new()
        }
    };
    let scheduled_tasks_fut = async {
        if tab == "tasks" {
            scheduled_tasks::list(&state.db).await.unwrap_or_default()
        } else {
            Vec::new()
        }
    };
    let review_entries_fut = async {
        if tab == "review" {
            let mut entries = episode_tags::get_needs_review(&state.db)
                .await
                .unwrap_or_default();
            populate_series_cover_urls(
                &state.db,
                &mut entries,
                |e| e.series_id,
                |entry, url| entry.cover_url = url,
            )
            .await;
            entries
        } else {
            Vec::new()
        }
    };

    let (
        logs,
        cfg_res,
        rss_last_run_res,
        rss_recent,
        scheduled_tasks,
        log_count_res,
        review_entries,
    ) = tokio::join!(
        logs_fut,
        config::get_config(&state.db),
        rss::latest_run(&state.db),
        rss_recent_fut,
        scheduled_tasks_fut,
        log::count(&state.db),
        review_entries_fut,
    );
    let cfg = cfg_res.ok().flatten();
    let rss_last_run = rss_last_run_res.unwrap_or(None);
    let log_count = log_count_res.unwrap_or(0);

    let force_mal_fallback = cfg
        .as_ref()
        .map(|cfg| cfg.force_mal_fallback)
        .unwrap_or(false);
    let force_kitsu_fallback = cfg
        .as_ref()
        .map(|cfg| cfg.force_kitsu_fallback)
        .unwrap_or(false);
    let auto_grab_on_add = cfg.as_ref().map(|cfg| cfg.auto_grab_on_add).unwrap_or(true);
    let allow_non_english = cfg
        .as_ref()
        .map(|cfg| cfg.allow_non_english)
        .unwrap_or(false);
    let rss_enabled = cfg.as_ref().map(|cfg| cfg.rss_enabled).unwrap_or(false);
    let rss_interval_minutes = cfg
        .as_ref()
        .map(|cfg| cfg.rss_interval_minutes)
        .unwrap_or(5);

    let categories = vec![
        ("search", LogCategory::Search.label()),
        ("grab", LogCategory::Grab.label()),
        ("auto_search", LogCategory::AutoSearch.label()),
        ("nyaa", LogCategory::Nyaa.label()),
        ("rss", LogCategory::Rss.label()),
        ("anilist", LogCategory::AniList.label()),
        ("jikan", LogCategory::Jikan.label()),
        ("qbit", LogCategory::QBit.label()),
        ("jellyfin", LogCategory::Jellyfin.label()),
        ("media", LogCategory::Media.label()),
        ("library", LogCategory::Library.label()),
        ("auth", LogCategory::Auth.label()),
        ("system", LogCategory::System.label()),
        ("post_process", LogCategory::PostProcess.label()),
        ("scoring", LogCategory::Scoring.label()),
    ];

    let title_language = cfg
        .as_ref()
        .map(|c| c.title_language.clone())
        .unwrap_or_else(|| "english".to_string());
    // Pagination cursor handling: the query asked for `limit + 1` so
    // we can detect whether an "Older" page exists without a separate
    // COUNT. If we got the extra row, drop it and stash the oldest
    // visible row's id as the `before_id` for the next page; if we
    // got fewer than the limit, this is the last page.
    let (logs, log_older_id) = truncate_to_page(logs, 200, |e| e.id);
    let (rss_recent, rss_older_id) = truncate_to_page(rss_recent, 200, |e| e.id);
    let template = SystemTemplate {
        page: "system".to_string(),
        tab,
        force_mal_fallback,
        force_kitsu_fallback,
        auto_grab_on_add,
        allow_non_english,
        debug_message: params.message,
        debug_error: params.error,
        logs,
        log_count,
        filter_level,
        filter_category,
        filter_search,
        log_before_id: logs_before_id,
        log_older_id,
        rss_before_id,
        rss_older_id,
        categories,
        rss_enabled,
        rss_interval_minutes,
        rss_last_run,
        rss_recent,
        scheduled_tasks,
        review_entries,
        title_language,
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
        .unwrap_or_default();

    cfg.force_mal_fallback = form.force_mal_fallback.is_some();
    cfg.force_kitsu_fallback = form.force_kitsu_fallback.is_some();
    cfg.allow_non_english = form.allow_non_english.is_some();
    cfg.auto_grab_on_add = form.auto_grab_on_add.is_some();

    let result = config::save_config(&state.db, &cfg).await;
    let (message, error) = match result {
        Ok(_) => {
            logger::info(
                &state.db,
                LogCategory::System,
                "Updated fallback debug settings",
                &format!(
                    "mal_jikan={}, kitsu={}",
                    if cfg.force_mal_fallback {
                        "enabled"
                    } else {
                        "disabled"
                    },
                    if cfg.force_kitsu_fallback {
                        "enabled"
                    } else {
                        "disabled"
                    }
                ),
            )
            .await;
            (
                Some(format!(
                    "Fallback debug settings saved. MAL/Jikan: {}. Kitsu: {}.",
                    if cfg.force_mal_fallback {
                        "enabled"
                    } else {
                        "disabled"
                    },
                    if cfg.force_kitsu_fallback {
                        "enabled"
                    } else {
                        "disabled"
                    }
                )),
                None,
            )
        }
        Err(e) => {
            logger::error(
                &state.db,
                LogCategory::System,
                "Failed to update fallback debug settings",
                &e.to_string(),
            )
            .await;
            (None, Some(format!("Failed to save debug settings: {}", e)))
        }
    };

    let template = SystemTemplate {
        page: "system".to_string(),
        tab: "debug".to_string(),
        force_mal_fallback: cfg.force_mal_fallback,
        force_kitsu_fallback: cfg.force_kitsu_fallback,
        auto_grab_on_add: cfg.auto_grab_on_add,
        allow_non_english: cfg.allow_non_english,
        debug_message: message,
        debug_error: error,
        logs: Vec::new(),
        log_count: log::count(&state.db).await.unwrap_or(0),
        filter_level: "info".to_string(),
        filter_category: String::new(),
        filter_search: String::new(),
        log_before_id: None,
        log_older_id: None,
        rss_before_id: None,
        rss_older_id: None,
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
            ("post_process", LogCategory::PostProcess.label()),
            ("scoring", LogCategory::Scoring.label()),
        ],
        rss_enabled: cfg.rss_enabled,
        rss_interval_minutes: cfg.rss_interval_minutes,
        rss_last_run: rss::latest_run(&state.db).await.unwrap_or(None),
        rss_recent: Vec::new(),
        scheduled_tasks: scheduled_tasks::list(&state.db).await.unwrap_or_default(),
        review_entries: Vec::new(),
        title_language: cfg.title_language.clone(),
    };
    Html(template.render().unwrap_or_default())
}

#[derive(Deserialize, utoipa::IntoParams)]
pub struct LogPollQuery {
    after: Option<i64>,
    level: Option<String>,
    category: Option<String>,
}

#[utoipa::path(
    get,
    path = "/api/logs/poll",
    tag = "System",
    summary = "Poll log entries",
    description = "Retrieve recent log entries, optionally filtered by level and category. Supports long-polling via the `after` parameter.",
    params(LogPollQuery),
    responses(
        (status = 200, description = "Log entries", body = Vec<log::LogEntry>),
    ),
)]
pub async fn api_logs_poll(
    State(state): State<AppState>,
    Query(params): Query<LogPollQuery>,
) -> Json<Vec<log::LogEntry>> {
    let after_id = params.after.unwrap_or(0);
    // Level + category are pushed into SQL via entries_after so the
    // 3s poll only materializes matching rows. The old path pulled
    // 100 rows per tick and filtered in memory — fine functionally
    // but wasteful when a narrow filter (e.g. level=error) matched
    // nothing in a quiet window.
    let entries = log::entries_after(
        &state.db,
        after_id,
        100,
        params.level.as_deref(),
        params.category.as_deref(),
    )
    .await
    .unwrap_or_default();

    Json(entries)
}

#[utoipa::path(
    post,
    path = "/api/system/rebuild-anilist-cache",
    tag = "System",
    summary = "Rebuild metadata cache",
    description = "Re-fetch and rebuild the cached AniList/MAL metadata for all tracked series.",
    responses(
        (status = 200, description = "Rebuild report", body = serde_json::Value),
    ),
)]
pub async fn api_rebuild_cached_metadata(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    // Detach the sweep from the request handler's lifetime. Previously
    // this was a direct `.await` on the sweep future — so when the
    // client navigated away mid-rebuild, Axum dropped the handler
    // future and cancellation propagated into the loop, stopping the
    // rebuild partway through with no trace other than a browser-side
    // `NetworkError when attempting to fetch resource.` bubbling back
    // into the logs via the client's error toast.
    //
    // Three layers of `tokio::spawn`:
    //   - **Outer** task owns `mark_finished` + translates the middle
    //     task's outcome into the HTTP response. Its body is
    //     maximally simple — one spawn, one await returning Result,
    //     one match, one DB update — so the `scheduled_task_runs`
    //     row is guaranteed to exit `last_status = 'running'` even
    //     if the middle layer panics in a code path we didn't
    //     anticipate (bad future arm in a match, a panic inside
    //     `mark_started`, etc.).
    //   - **Middle** task owns `mark_started` and the inner rebuild
    //     orchestration. Any panic here surfaces as `Err(JoinError)`
    //     on the outer's `.await` and gets translated to a terminal
    //     `"error"` status by the outer.
    //   - **Inner** task runs the actual sweep; its JoinError (on
    //     panic) is caught by the middle task and folded into its
    //     own result so the outer sees a single combined outcome.
    //
    // Distinct task key (`metadata_rebuild`, not the shared
    // `metadata_refresh`) so the manual full-rebuild doesn't overwrite
    // the scheduled 12h `refresh_all_series_metadata` status row
    // when the two overlap — they're semantically different
    // operations and the audit trail for each should stand alone.
    let db = state.db.clone();
    let outer = tokio::spawn(async move {
        let middle_db = db.clone();
        let middle = tokio::spawn(async move {
            let _ = scheduled_tasks::mark_started(
                &middle_db,
                "metadata_rebuild",
                "Manual metadata cache rebuild started",
            )
            .await;

            let rebuild_db = middle_db.clone();
            let inner = tokio::spawn(async move {
                metadata_sync::rebuild_cached_metadata_for_all(&rebuild_db).await
            });
            inner.await // Result<(usize, usize, usize), JoinError>
        });

        let (status, detail, payload): (&str, String, Option<(usize, usize, usize)>) =
            match middle.await {
                Ok(Ok((rebuilt, skipped, failed))) => {
                    let st = if failed > 0 { "warn" } else { "ok" };
                    (
                        st,
                        format!("rebuilt={rebuilt}, skipped={skipped}, failed={failed}"),
                        Some((rebuilt, skipped, failed)),
                    )
                }
                Ok(Err(join_err)) => {
                    // Inner panicked. The middle task caught it and
                    // bubbled it up cleanly.
                    let kind = if join_err.is_panic() {
                        "panicked"
                    } else {
                        "join error"
                    };
                    ("error", format!("rebuild sweep {kind}: {join_err}"), None)
                }
                Err(join_err) => {
                    // Middle itself panicked — e.g. `mark_started`
                    // internals, or something between the nested
                    // spawns. Still mark the run finished so the
                    // status row exits `running`.
                    let kind = if join_err.is_panic() {
                        "panicked"
                    } else {
                        "join error"
                    };
                    (
                        "error",
                        format!("rebuild orchestration task {kind}: {join_err}"),
                        None,
                    )
                }
            };
        let _ = scheduled_tasks::mark_finished(&db, "metadata_rebuild", status, &detail).await;
        payload
    });

    let payload = outer.await.map_err(|e| {
        (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("rebuild orchestration task failed to join: {}", e),
        )
    })?;

    let Some((rebuilt, skipped, failed)) = payload else {
        // Inner panicked — we already wrote an "error" row into
        // scheduled_task_runs so operators can see what happened.
        // Surface a 500 to the client (on the happy path where they
        // stayed on the page) so they don't think it silently
        // succeeded.
        return Err((
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "Rebuild task panicked; see scheduled tasks for details.".to_string(),
        ));
    };

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

#[utoipa::path(
    post,
    path = "/api/system/reload-anibridge",
    tag = "System",
    summary = "Reload Anibridge mappings",
    description = "Re-download the AniList-to-MAL ID mapping table from Anibridge.",
    responses(
        (status = 200, description = "Mappings reloaded", body = serde_json::Value),
        (status = 502, description = "Reload failed"),
    ),
)]
pub async fn api_anibridge_reload(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    logger::info(
        &state.db,
        LogCategory::System,
        "Anibridge mappings reload requested",
        "",
    )
    .await;
    let _ = scheduled_tasks::mark_started(
        &state.db,
        "anibridge_refresh",
        "Manual anibridge mappings refresh",
    )
    .await;

    if crate::services::anibridge::reload().await {
        let _ = scheduled_tasks::mark_finished(
            &state.db,
            "anibridge_refresh",
            "ok",
            "Mappings refreshed",
        )
        .await;
        Ok(Json(serde_json::json!({
            "ok": true,
            "message": "Anibridge mappings reloaded successfully",
        })))
    } else {
        let _ = scheduled_tasks::mark_finished(
            &state.db,
            "anibridge_refresh",
            "error",
            "Failed to download mappings",
        )
        .await;
        Err((
            axum::http::StatusCode::BAD_GATEWAY,
            "Failed to reload anibridge mappings".to_string(),
        ))
    }
}

#[utoipa::path(
    post,
    path = "/api/logs/clear",
    tag = "System",
    summary = "Clear all logs",
    description = "Delete all log entries from the database.",
    responses(
        (status = 200, description = "Logs cleared", body = serde_json::Value),
        (status = 500, description = "Database error"),
    ),
)]
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

/// Query for the log-export endpoint. `range` selects a quick preset
/// or `all` for everything. Date-range support could land later via
/// explicit `since` / `until` ISO timestamps; the preset form covers
/// the common cases (recent debugging, weekly snapshot for support).
#[derive(Deserialize, utoipa::ToSchema)]
pub struct LogExportQuery {
    /// `today` / `7d` / `30d` / `all`. Anything else coerces to `all`
    /// so a typo can't return an empty file.
    #[serde(default)]
    pub range: String,
}

#[utoipa::path(
    get,
    path = "/api/logs/export",
    tag = "System",
    summary = "Download logs as a tab-separated text file",
    description = "Returns the full log table (or a date-bounded subset) as a downloadable plain-text file. \
                   `range` selects a quick preset: `today` (since midnight UTC), `7d` (last 7 days), `30d` \
                   (last 30 days), or `all` (no date filter). Format: tab-separated columns \
                   `timestamp\\tlevel\\tcategory\\tmessage\\tdetail` with a header row, one entry per line. \
                   Suitable for grep / awk / pasting into a bug report.",
    params(
        ("range" = Option<String>, Query, description = "today / 7d / 30d / all"),
    ),
    responses(
        (status = 200, description = "Plain-text log dump (Content-Disposition: attachment)"),
        (status = 500, description = "Database error"),
    ),
)]
pub async fn api_logs_export(
    State(state): State<AppState>,
    Query(q): Query<LogExportQuery>,
) -> Result<axum::response::Response, (axum::http::StatusCode, String)> {
    use axum::http::{HeaderMap, HeaderValue, header};
    use axum::response::IntoResponse;

    // Map the range preset to a SQL date filter. SQLite's
    // `datetime('now', '-N days')` keeps the cutoff comparison cheap
    // and lets the index on `timestamp` do its job.
    let (since_clause, since_label): (&str, &str) = match q.range.as_str() {
        "today" => ("timestamp >= datetime('now', 'start of day')", "today"),
        "7d" => ("timestamp >= datetime('now', '-7 days')", "7d"),
        "30d" => ("timestamp >= datetime('now', '-30 days')", "30d"),
        // Anything else (including empty / malformed) falls through
        // to the unbounded "all" — better to return more data than
        // none on a typo.
        _ => ("1=1", "all"),
    };

    let sql = format!(
        "SELECT timestamp, level, category, message, detail \
         FROM logs WHERE {since_clause} ORDER BY id ASC"
    );
    let rows: Vec<(String, String, String, String, String)> = sqlx::query_as(&sql)
        .fetch_all(&state.db)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Tab-separated with a header row. Embedded tabs / newlines / CRs
    // in the message or detail body are escaped to spaces so each entry
    // stays on a single line — matters for grep / awk consumption. CR
    // is in the escape set defensively: tracing output realistically
    // never contains one, but a panic message or external-service
    // error round-tripped through `logger::*` could, and a bare \r
    // makes some line-oriented tools treat it as a record separator.
    let mut body = String::with_capacity(rows.len() * 128);
    body.push_str("timestamp\tlevel\tcategory\tmessage\tdetail\n");
    for (ts, level, category, message, detail) in &rows {
        let m = message.replace(['\t', '\n', '\r'], " ");
        let d = detail.replace(['\t', '\n', '\r'], " ");
        body.push_str(&format!("{ts}\t{level}\t{category}\t{m}\t{d}\n"));
    }

    // Dated filename so a user downloading multiple snapshots doesn't
    // overwrite earlier ones. Use the chrono UTC date — local-time
    // formatting would be misleading since the timestamps inside the
    // file are SQLite-default UTC.
    let date = chrono::Utc::now().format("%Y-%m-%d");
    let filename = format!("ryokan-logs-{date}-{since_label}.tsv");

    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/tab-separated-values; charset=utf-8"),
    );
    let disposition = format!("attachment; filename=\"{filename}\"");
    if let Ok(val) = HeaderValue::from_str(&disposition) {
        headers.insert(header::CONTENT_DISPOSITION, val);
    }
    Ok((headers, body).into_response())
}

/// Payload for the client-side log ingestion endpoint. Every in-app toast
/// (fired via `window.ryokanToast` in `base.html`) hits this endpoint so
/// the notification persists in the Logs tab after the transient toast
/// fades. Toasts are user-facing so mapping is straightforward:
///   kind `info`/`success` → LogLevel::Info
///   kind `warn`           → LogLevel::Warn
///   kind `error`          → LogLevel::Error
/// The `category` string is looked up against `LogCategory::from_str`
/// and falls back to `System` when the caller doesn't specify or passes
/// a value outside the known set.
#[derive(Deserialize, utoipa::ToSchema)]
pub struct ClientLogForm {
    pub kind: String,
    pub category: Option<String>,
    pub title: String,
    pub body: Option<String>,
}

// Field-length and rate-limit caps for `/api/logs/client`. The endpoint
// is behind cookie auth + same-origin CSRF, so the threat model is a
// buggy/runaway client (or a curious user with devtools open) flooding
// the logs table — not a malicious unauthenticated attacker. The single
// global window is sufficient for a self-hosted single-user PVR; it
// would need to be per-session for a multi-tenant deployment.
const CLIENT_LOG_TITLE_MAX: usize = 512;
const CLIENT_LOG_BODY_MAX: usize = 4096;
const CLIENT_LOG_RATE_WINDOW: Duration = Duration::from_secs(60);
const CLIENT_LOG_RATE_MAX: usize = 30;

static CLIENT_LOG_HITS: LazyLock<Mutex<VecDeque<Instant>>> =
    LazyLock::new(|| Mutex::new(VecDeque::with_capacity(CLIENT_LOG_RATE_MAX)));

fn check_client_log_rate() -> bool {
    let mut hits = CLIENT_LOG_HITS.lock().unwrap();
    admit_log_event(
        &mut hits,
        Instant::now(),
        CLIENT_LOG_RATE_WINDOW,
        CLIENT_LOG_RATE_MAX,
    )
}

/// Pure sliding-window rate-limit check, factored out of
/// `check_client_log_rate` so the policy is testable without poking
/// the process-wide `CLIENT_LOG_HITS` static. Drops timestamps older
/// than `window` from the front of `hits`, then admits the event if
/// the remaining count is under `max`. On admission, records `now`.
fn admit_log_event(
    hits: &mut VecDeque<Instant>,
    now: Instant,
    window: Duration,
    max: usize,
) -> bool {
    while let Some(front) = hits.front() {
        if now.duration_since(*front) > window {
            hits.pop_front();
        } else {
            break;
        }
    }
    if hits.len() >= max {
        return false;
    }
    hits.push_back(now);
    true
}

#[utoipa::path(
    post,
    path = "/api/logs/client",
    tag = "System",
    summary = "Log a client-side toast notification",
    description = "Persists a transient in-app toast to the logs table so users can see recent notifications in the System → Logs tab after the toast has faded. Fired automatically by window.ryokanToast.",
    request_body = ClientLogForm,
    responses(
        (status = 200, description = "Toast logged", body = serde_json::Value),
        (status = 400, description = "Title or body exceeds size cap"),
        (status = 429, description = "Rate limit exceeded"),
    ),
)]
pub async fn api_logs_client(
    State(state): State<AppState>,
    Json(form): Json<ClientLogForm>,
) -> Response {
    if form.title.len() > CLIENT_LOG_TITLE_MAX
        || form.body.as_deref().map(str::len).unwrap_or(0) > CLIENT_LOG_BODY_MAX
    {
        return (StatusCode::BAD_REQUEST, "title or body too large").into_response();
    }
    if !check_client_log_rate() {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            "client log rate limit exceeded",
        )
            .into_response();
    }
    let level = match form.kind.as_str() {
        "warn" => LogLevel::Warn,
        "error" => LogLevel::Error,
        _ => LogLevel::Info,
    };
    let category = form
        .category
        .as_deref()
        .and_then(LogCategory::from_str)
        .unwrap_or(LogCategory::System);
    let detail = form.body.as_deref().unwrap_or("");
    logger::log(&state.db, level, category, &form.title, detail).await;
    Json(serde_json::json!({"ok": true})).into_response()
}

#[utoipa::path(
    post,
    path = "/api/rss/sync",
    tag = "System",
    summary = "Trigger RSS sync",
    description = "Manually trigger an RSS feed sync to check for new episodes.",
    responses(
        (status = 200, description = "Sync completed", body = serde_json::Value),
        (status = 500, description = "Sync failed"),
    ),
)]
pub async fn api_rss_sync(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let _ = scheduled_tasks::mark_started(&state.db, "rss_sync", "Manual RSS sync started").await;
    match rss_service::sync_once(&state, "manual").await {
        Ok(summary) => {
            let _ =
                scheduled_tasks::mark_finished(&state.db, "rss_sync", "ok", &summary.detail).await;
            Ok(Json(serde_json::json!({
                "ok": true,
                "message": summary.detail,
                "summary": summary,
            })))
        }
        Err(err) => {
            let _ = scheduled_tasks::mark_finished(&state.db, "rss_sync", "error", &err).await;
            Err((
                axum::http::StatusCode::BAD_GATEWAY,
                serde_json::json!({
                    "ok": false,
                    "message": err,
                })
                .to_string(),
            ))
        }
    }
}

#[utoipa::path(
    post,
    path = "/api/rss/clear-history",
    tag = "System",
    summary = "Clear RSS grab history",
    description = "Clear the RSS grab history so previously grabbed episodes are re-evaluated on the next sync.",
    responses(
        (status = 200, description = "History cleared", body = serde_json::Value),
        (status = 500, description = "Database error"),
    ),
)]
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
    )
    .await;
    Ok(Json(serde_json::json!({
        "ok": true,
        "message": format!("Cleared {} grab history entries. Previously grabbed episodes will be re-evaluated on next sync.", deleted),
    })))
}

#[utoipa::path(
    post,
    path = "/api/tasks/metadata-refresh",
    tag = "System",
    summary = "Trigger metadata refresh",
    description = "Manually trigger a metadata refresh for all tracked series.",
    responses(
        (status = 200, description = "Refresh report", body = serde_json::Value),
    ),
)]
pub async fn api_force_metadata_refresh(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let _ = scheduled_tasks::mark_started(
        &state.db,
        "metadata_refresh",
        "Manual metadata refresh started",
    )
    .await;
    let (refreshed, failed) = metadata_sync::refresh_all_series_metadata(&state.db).await;
    let status = if failed > 0 { "warn" } else { "ok" };
    let detail = format!("refreshed={}, failed={}", refreshed, failed);
    let _ = scheduled_tasks::mark_finished(&state.db, "metadata_refresh", status, &detail).await;
    Ok(Json(serde_json::json!({
        "ok": failed == 0,
        "message": format!("Metadata refresh complete. Refreshed: {}. Failed: {}.", refreshed, failed),
    })))
}

#[utoipa::path(
    post,
    path = "/api/tasks/cleanup",
    tag = "System",
    summary = "Trigger cleanup",
    description = "Manually trigger cleanup of old log entries and RSS decisions (older than 30 days).",
    responses(
        (status = 200, description = "Cleanup report", body = serde_json::Value),
    ),
)]
pub async fn api_force_cleanup(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let _ = scheduled_tasks::mark_started(&state.db, "cleanup", "Manual cleanup started").await;
    let mut errors = Vec::new();
    if let Err(e) = crate::models::log::cleanup(&state.db, 30).await {
        errors.push(format!("logs: {}", e));
    }
    if let Err(e) = rss::cleanup_old_decisions(&state.db, 30).await {
        errors.push(format!("rss: {}", e));
    }
    let status = if errors.is_empty() { "ok" } else { "warn" };
    let detail = if errors.is_empty() {
        "Cleanup completed".to_string()
    } else {
        errors.join("; ")
    };
    let _ = scheduled_tasks::mark_finished(&state.db, "cleanup", status, &detail).await;
    Ok(Json(serde_json::json!({
        "ok": errors.is_empty(),
        "message": detail,
    })))
}

#[utoipa::path(
    post,
    path = "/api/tasks/post-processing",
    tag = "System",
    summary = "Trigger post-processing",
    description = "Manually trigger post-processing to move/rename completed downloads into the media library.",
    responses(
        (status = 200, description = "Post-processing completed", body = serde_json::Value),
    ),
)]
pub async fn api_force_post_processing(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let _ =
        scheduled_tasks::mark_started(&state.db, "post_processing", "Manual post-processing run")
            .await;
    post_processing::run_once(&state).await;
    let _ =
        scheduled_tasks::mark_finished(&state.db, "post_processing", "ok", "Manual run completed")
            .await;
    Ok(Json(serde_json::json!({
        "ok": true,
        "message": "Post-processing run completed",
    })))
}

#[utoipa::path(
    post,
    path = "/api/tasks/library-classify",
    tag = "System",
    summary = "Classify externally-imported files",
    description = "Walk every tracked series' media folder and run the source/resolution classifier on files that don't yet have a structured classification row. Useful after importing pre-existing media from another PVR or a manual drop.",
    responses(
        (status = 200, description = "Library classify report", body = serde_json::Value),
    ),
)]
pub async fn api_force_library_classify(State(state): State<AppState>) -> Json<serde_json::Value> {
    let report = post_processing::scan_library_for_unclassified(&state).await;
    let message = format!(
        "Library classify scan complete. Series scanned: {}. Files scanned: {}. Classified: {}. Needs review: {}.",
        report.series_scanned,
        report.files_scanned,
        report.files_classified,
        report.files_needing_review,
    );
    Json(serde_json::json!({
        "ok": true,
        "message": message,
        "series_scanned": report.series_scanned,
        "files_scanned": report.files_scanned,
        "files_classified": report.files_classified,
        "files_needing_review": report.files_needing_review,
    }))
}

#[utoipa::path(
    post,
    path = "/api/tasks/upgrade-search",
    tag = "System",
    summary = "Trigger quality upgrade search",
    description = "Manually trigger a search for quality upgrades across all monitored episodes.",
    responses(
        (status = 200, description = "Upgrade search report", body = serde_json::Value),
        (status = 500, description = "Search failed"),
    ),
)]
pub async fn api_force_upgrade_search(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let _ =
        scheduled_tasks::mark_started(&state.db, "upgrade_search", "Manual upgrade search started")
            .await;
    match upgrade::run_once(&state).await {
        Ok(summary) => {
            let _ =
                scheduled_tasks::mark_finished(&state.db, "upgrade_search", "ok", &summary.detail)
                    .await;
            Ok(Json(serde_json::json!({
                "ok": true,
                "message": summary.detail,
                "series_checked": summary.series_checked,
                "episodes_checked": summary.episodes_checked,
                "upgrades_grabbed": summary.upgrades_grabbed,
            })))
        }
        Err(err) => {
            let _ =
                scheduled_tasks::mark_finished(&state.db, "upgrade_search", "error", &err).await;
            Err((axum::http::StatusCode::BAD_GATEWAY, err))
        }
    }
}

/// Wrapper for the `/api/system/tasks` response so OpenAPI / Swagger
/// can describe the actual `{ "tasks": [...] }` shape rather than an
/// opaque `serde_json::Value`. Pre-this-shape the path's `body =`
/// declaration was `serde_json::Value`, which Swagger UI rendered as
/// "any JSON" — clients reading the spec couldn't see the entry
/// fields. Reviewer caught this; mirror Sonarr's habit of typed
/// response wrappers.
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct SystemTasksResponse {
    pub tasks: Vec<crate::services::task_registry::TaskSnapshot>,
}

#[utoipa::path(
    get,
    path = "/api/system/tasks",
    tag = "System",
    summary = "Snapshot every supervised background task's lifecycle state",
    description = "Returns one entry per task registered with the supervisor — name, current status (running / backoff), unix-seconds start of the current iteration, last exit (timestamp + cause: panic / join_error / normal), iteration exit count, and the configured backoff duration in milliseconds. Read-only snapshot; no side effects. The System page polls this for the task-status table; ops can also curl it for a quick health check (`curl /api/system/tasks | jq '.tasks[] | select(.status == \"backoff\")'` surfaces every task that's currently in a crash-loop respawn delay).",
    responses(
        (status = 200, description = "Snapshot of every registered task", body = SystemTasksResponse),
    ),
)]
pub async fn api_system_tasks(State(state): State<AppState>) -> Json<SystemTasksResponse> {
    let tasks = state.tasks.snapshot().await;
    Json(SystemTasksResponse { tasks })
}

#[cfg(test)]
mod tasks_endpoint_tests {
    use crate::services::task_registry::ExitKind;
    use crate::test_support::{build_test_app_state, in_memory_pool};

    #[tokio::test]
    async fn endpoint_returns_registered_tasks_with_snapshot_state() {
        let db = in_memory_pool().await;
        let state = build_test_app_state(db, None);

        // Register two tasks and mark them through different lifecycle
        // states so the snapshot has something distinguishable to assert on.
        let rss_state = state.tasks.register("rss_sync").await;
        rss_state.mark_started(1_000);
        let cleanup_state = state.tasks.register("cleanup").await;
        cleanup_state.mark_started(500);
        cleanup_state.mark_exited(600, ExitKind::Panic);
        cleanup_state.mark_backoff(10_000);

        let resp = super::api_system_tasks(axum::extract::State(state)).await;
        let tasks = resp.0.tasks;
        assert_eq!(tasks.len(), 2);

        let cleanup = tasks.iter().find(|t| t.name == "cleanup").unwrap();
        assert_eq!(cleanup.status, "backoff");
        assert_eq!(cleanup.last_exit_kind, "panic");
        assert_eq!(cleanup.exit_count, 1);
        assert_eq!(cleanup.current_backoff_ms, 10_000);

        let rss = tasks.iter().find(|t| t.name == "rss_sync").unwrap();
        assert_eq!(rss.status, "running");
        assert_eq!(rss.last_exit_kind, "none");
        assert_eq!(rss.exit_count, 0);
        assert_eq!(rss.current_backoff_ms, 0);
    }

    #[tokio::test]
    async fn endpoint_returns_empty_array_when_no_tasks_registered() {
        // Fresh AppState shouldn't have any tasks yet — the
        // `services::task_registry::TaskRegistry::new()` shipped on
        // `AppState` is lazy; supervise() registers on first call.
        // Tests + cargo run between starts both go through this state,
        // so the empty-snapshot shape needs to be valid JSON.
        let db = in_memory_pool().await;
        let state = build_test_app_state(db, None);
        let resp = super::api_system_tasks(axum::extract::State(state)).await;
        assert!(resp.0.tasks.is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── admit_log_event ──────────────────────────────────────────────
    //
    // The pure helper behind `check_client_log_rate`. Tested with an
    // explicit clock + state so we can drive the sliding window
    // without poking the process-wide `CLIENT_LOG_HITS` static.

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn admit_log_event_admits_under_cap() {
        let mut hits = VecDeque::new();
        let now = t0();
        let window = Duration::from_secs(60);
        let max = 3;
        assert!(admit_log_event(&mut hits, now, window, max));
        assert!(admit_log_event(&mut hits, now, window, max));
        assert!(admit_log_event(&mut hits, now, window, max));
        // Three admitted, queue is full.
        assert_eq!(hits.len(), 3);
    }

    #[test]
    fn admit_log_event_rejects_at_cap() {
        let mut hits = VecDeque::new();
        let now = t0();
        let window = Duration::from_secs(60);
        let max = 2;
        assert!(admit_log_event(&mut hits, now, window, max));
        assert!(admit_log_event(&mut hits, now, window, max));
        // Cap reached — third call must reject.
        assert!(!admit_log_event(&mut hits, now, window, max));
        // Queue stays at the cap; the rejected event is NOT recorded
        // (otherwise a sustained burst would push the window forward
        // forever and never let traffic in again).
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn admit_log_event_drops_expired_timestamps_before_check() {
        let mut hits = VecDeque::new();
        let window = Duration::from_secs(60);
        let max = 2;

        let earlier = t0();
        // Seed two old hits manually.
        hits.push_back(earlier);
        hits.push_back(earlier);

        // Advance past the window — both should age out and the next
        // event admits cleanly.
        let now = earlier + Duration::from_secs(61);
        assert!(admit_log_event(&mut hits, now, window, max));
        // Only the just-admitted event remains.
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn admit_log_event_keeps_in_window_timestamps() {
        let mut hits = VecDeque::new();
        let window = Duration::from_secs(60);
        let max = 2;
        let earlier = t0();
        hits.push_back(earlier);

        // Half a window later — the seeded hit is still in window so
        // the second event tips us up to cap; the third must reject.
        let now = earlier + Duration::from_secs(30);
        assert!(admit_log_event(&mut hits, now, window, max));
        assert!(!admit_log_event(&mut hits, now, window, max));
    }

    #[test]
    fn admit_log_event_zero_max_rejects_everything() {
        // Defensive: a misconfigured cap of 0 must not admit any
        // events (rather than treating "0" as "no limit"). Pin the
        // ordering so a future "shortcut" optimization can't flip
        // the policy.
        let mut hits = VecDeque::new();
        assert!(!admit_log_event(
            &mut hits,
            t0(),
            Duration::from_secs(60),
            0
        ));
        assert!(hits.is_empty());
    }

    // ── normalize_system_tab ─────────────────────────────────────────
    //
    // The /system page lives behind a `?tab=` query param. The
    // normalizer pins which strings are recognized; everything else
    // collapses to "logs" so a stale bookmark doesn't render an empty
    // page. Pinning every accepted value guards against a future
    // refactor that drops a tab silently.

    #[test]
    fn normalize_system_tab_recognized_values_pass_through() {
        for tab in ["scoring", "debug", "rss", "tasks", "review", "credits"] {
            assert_eq!(normalize_system_tab(Some(tab.to_string())), tab);
        }
    }

    #[test]
    fn normalize_system_tab_legacy_help_alias_maps_to_scoring() {
        // The "scoring" tab used to be called "help" — the alias
        // covers stale bookmarks from before the rename.
        assert_eq!(normalize_system_tab(Some("help".to_string())), "scoring");
    }

    #[test]
    fn normalize_system_tab_unknown_or_missing_falls_back_to_logs() {
        assert_eq!(normalize_system_tab(None), "logs");
        assert_eq!(normalize_system_tab(Some("".to_string())), "logs");
        assert_eq!(normalize_system_tab(Some("garbage".to_string())), "logs");
    }
}

// ── Endpoint coverage ──────────────────────────────────────────────
//
// Direct-call coverage for the smaller `/api/*` system endpoints —
// no router needed because each handler takes `State<AppState>` and
// returns its own response. Targets the mostly-DB-only paths
// (logs, cleanup, library-classify, upgrade-search dispatch) and
// the rate-limited client-log ingest.
#[cfg(test)]
mod endpoint_tests {
    use super::*;
    use crate::test_support::{build_test_app_state, in_memory_pool};

    // ── api_logs_poll ────────────────────────────────────────────────

    #[tokio::test]
    async fn api_logs_poll_returns_only_entries_strictly_after_cursor() {
        let db = in_memory_pool().await;
        log::insert(
            &db,
            log::LogLevel::Info,
            log::LogCategory::System,
            "first",
            "",
        )
        .await
        .unwrap();
        let cursor = log::latest_id(&db).await.unwrap();
        log::insert(
            &db,
            log::LogLevel::Info,
            log::LogCategory::System,
            "second",
            "",
        )
        .await
        .unwrap();

        let state = build_test_app_state(db, None);
        let resp = api_logs_poll(
            axum::extract::State(state),
            axum::extract::Query(LogPollQuery {
                after: Some(cursor),
                level: None,
                category: None,
            }),
        )
        .await;
        let entries = resp.0;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].message, "second");
    }

    #[tokio::test]
    async fn api_logs_poll_filters_by_level_and_category_at_sql_layer() {
        let db = in_memory_pool().await;
        log::insert(
            &db,
            log::LogLevel::Info,
            log::LogCategory::Search,
            "info-search",
            "",
        )
        .await
        .unwrap();
        log::insert(
            &db,
            log::LogLevel::Warn,
            log::LogCategory::Grab,
            "warn-grab",
            "",
        )
        .await
        .unwrap();
        log::insert(
            &db,
            log::LogLevel::Error,
            log::LogCategory::Grab,
            "error-grab",
            "",
        )
        .await
        .unwrap();

        let state = build_test_app_state(db, None);
        let resp = api_logs_poll(
            axum::extract::State(state),
            axum::extract::Query(LogPollQuery {
                after: Some(0),
                level: Some("warn".to_string()),
                category: Some("grab".to_string()),
            }),
        )
        .await;
        // level=warn → warn+error pass; category=grab keeps both.
        let entries = resp.0;
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().all(|e| e.category == "grab"));
        assert!(
            entries
                .iter()
                .all(|e| matches!(e.level.as_str(), "warn" | "error"))
        );
    }

    // ── api_logs_clear ───────────────────────────────────────────────

    #[tokio::test]
    async fn api_logs_clear_drops_all_rows_and_logs_a_replacement_marker() {
        let db = in_memory_pool().await;
        log::insert(
            &db,
            log::LogLevel::Info,
            log::LogCategory::System,
            "before",
            "",
        )
        .await
        .unwrap();
        let state = build_test_app_state(db.clone(), None);
        let resp = api_logs_clear(axum::extract::State(state))
            .await
            .expect("ok");
        assert_eq!(resp.0["ok"], true);

        // The handler logs "Logs cleared by user" *before* the
        // DELETE, but that row is part of what gets cleared (the
        // DELETE has no WHERE clause). Net effect: the table is
        // empty after a successful clear.
        let count = log::count(&db).await.unwrap();
        assert_eq!(count, 0);
    }

    // ── api_logs_client ──────────────────────────────────────────────

    #[tokio::test]
    async fn api_logs_client_persists_toast_with_mapped_level_and_category() {
        let db = in_memory_pool().await;
        let state = build_test_app_state(db.clone(), None);

        let resp = api_logs_client(
            axum::extract::State(state),
            axum::extract::Json(ClientLogForm {
                kind: "warn".to_string(),
                category: Some("grab".to_string()),
                title: "Toast title".to_string(),
                body: Some("body text".to_string()),
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);

        let rows = log::query(&db, &log::LogQuery::default()).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].level, "warn");
        assert_eq!(rows[0].category, "grab");
        assert_eq!(rows[0].message, "Toast title");
        assert_eq!(rows[0].detail, "body text");
    }

    #[tokio::test]
    async fn api_logs_client_unknown_category_falls_back_to_system() {
        let db = in_memory_pool().await;
        let state = build_test_app_state(db.clone(), None);
        let _ = api_logs_client(
            axum::extract::State(state),
            axum::extract::Json(ClientLogForm {
                kind: "info".to_string(),
                category: Some("not-a-category".to_string()),
                title: "x".to_string(),
                body: None,
            }),
        )
        .await;
        let rows = log::query(&db, &log::LogQuery::default()).await.unwrap();
        assert_eq!(rows[0].category, "system");
    }

    #[tokio::test]
    async fn api_logs_client_rejects_oversized_title_and_body() {
        let db = in_memory_pool().await;
        let state = build_test_app_state(db.clone(), None);
        let big_title = "x".repeat(CLIENT_LOG_TITLE_MAX + 1);
        let resp = api_logs_client(
            axum::extract::State(state.clone()),
            axum::extract::Json(ClientLogForm {
                kind: "info".to_string(),
                category: None,
                title: big_title,
                body: None,
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        let big_body = "y".repeat(CLIENT_LOG_BODY_MAX + 1);
        let resp = api_logs_client(
            axum::extract::State(state),
            axum::extract::Json(ClientLogForm {
                kind: "info".to_string(),
                category: None,
                title: "ok".to_string(),
                body: Some(big_body),
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        // Nothing should have been persisted on either oversize.
        assert_eq!(log::count(&db).await.unwrap(), 0);
    }

    // ── api_force_cleanup ────────────────────────────────────────────

    #[tokio::test]
    async fn api_force_cleanup_deletes_aged_logs_and_rss_decisions() {
        let db = in_memory_pool().await;
        // Seed a 60-day-old log + a 60-day-old RSS decision; both
        // should be gone after cleanup.
        log::insert(
            &db,
            log::LogLevel::Info,
            log::LogCategory::System,
            "old",
            "",
        )
        .await
        .unwrap();
        sqlx::query("UPDATE logs SET timestamp = datetime('now', '-60 days')")
            .execute(&db)
            .await
            .unwrap();

        rss::record_decision(
            &db,
            rss::DecisionRecord {
                item_key: "k:60d",
                title: "old item",
                link: "",
                series_id: None,
                series_title: "",
                group_name: "",
                is_batch: false,
                decision: "skipped",
                reason: "",
                source: "",
                source_id: None,
            },
        )
        .await
        .unwrap();
        sqlx::query("UPDATE rss_seen SET created_at = datetime('now', '-60 days')")
            .execute(&db)
            .await
            .unwrap();

        let state = build_test_app_state(db.clone(), None);
        let resp = api_force_cleanup(axum::extract::State(state))
            .await
            .expect("ok");
        assert_eq!(resp.0["ok"], true);

        assert_eq!(log::count(&db).await.unwrap(), 0);
        assert!(rss::recent_decisions(&db, 10).await.unwrap().is_empty());
    }

    // ── api_force_library_classify ──────────────────────────────────

    #[tokio::test]
    async fn api_force_library_classify_returns_zero_report_for_empty_library() {
        // Empty config (no media_root) → scan_library_for_unclassified
        // early-returns and the report is all zeros. Verifies the
        // handler wraps the report into the expected JSON shape.
        let db = in_memory_pool().await;
        let state = build_test_app_state(db, None);
        let resp = api_force_library_classify(axum::extract::State(state)).await;
        assert_eq!(resp.0["ok"], true);
        assert_eq!(resp.0["series_scanned"], 0);
        assert_eq!(resp.0["files_scanned"], 0);
        assert_eq!(resp.0["files_classified"], 0);
        assert_eq!(resp.0["files_needing_review"], 0);
    }

    // ── api_force_upgrade_search ────────────────────────────────────

    #[tokio::test]
    async fn api_force_upgrade_search_translates_summary_into_response() {
        // Default config = no quality cutoff configured (after
        // Config::default() resets to "" / ""), so upgrade::run_once
        // returns the "No quality cutoff" branch as Ok summary. The
        // handler wraps the summary fields into the JSON envelope.
        let db = in_memory_pool().await;
        sqlx::query(
            "INSERT INTO config (id, media_root, post_processing_mode, cutoff_source, cutoff_resolution) \
             VALUES (1, '/tmp', 'hardlink', '', '')",
        )
        .execute(&db)
        .await
        .unwrap();
        let state = build_test_app_state(db, None);
        let resp = api_force_upgrade_search(axum::extract::State(state))
            .await
            .expect("ok summary");
        assert_eq!(resp.0["ok"], true);
        assert_eq!(resp.0["series_checked"], 0);
        assert_eq!(resp.0["upgrades_grabbed"], 0);
        assert!(
            resp.0["message"]
                .as_str()
                .unwrap()
                .contains("No quality cutoff")
        );
    }

    // ── api_logs_export ──────────────────────────────────────────────

    async fn read_response_body(resp: axum::response::Response) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("read body");
        String::from_utf8(bytes.to_vec()).expect("body is utf-8")
    }

    #[tokio::test]
    async fn api_logs_export_returns_tab_separated_with_header_row() {
        let db = in_memory_pool().await;
        log::insert(
            &db,
            log::LogLevel::Warn,
            log::LogCategory::System,
            "test event",
            "with detail",
        )
        .await
        .unwrap();
        let state = build_test_app_state(db, None);

        let resp = api_logs_export(
            axum::extract::State(state),
            axum::extract::Query(LogExportQuery {
                range: "all".to_string(),
            }),
        )
        .await
        .expect("export ok")
        .into_response();

        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let ct = resp
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            ct.contains("tab-separated-values"),
            "Content-Type must be tab-separated-values; got {ct}"
        );
        let cd = resp
            .headers()
            .get(axum::http::header::CONTENT_DISPOSITION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        assert!(
            cd.starts_with("attachment; filename=\"ryokan-logs-"),
            "Content-Disposition must trigger a download with a dated filename; got {cd}"
        );
        let body = read_response_body(resp).await;
        assert!(
            body.starts_with("timestamp\tlevel\tcategory\tmessage\tdetail\n"),
            "first line must be the TSV header row; got: {body}"
        );
        // Newline-terminated header + at least one data row.
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2, "expected header + 1 entry; got {lines:?}");
        assert!(
            lines[1].contains("\twarn\tsystem\ttest event\twith detail"),
            "data row should round-trip the inserted entry; got: {}",
            lines[1]
        );
    }

    #[tokio::test]
    async fn api_logs_export_unknown_range_falls_back_to_all() {
        // A typo in the `range` query param shouldn't return an empty
        // file — that would silently lose data. Fall through to the
        // unbounded "all" branch.
        let db = in_memory_pool().await;
        log::insert(
            &db,
            log::LogLevel::Info,
            log::LogCategory::System,
            "should appear",
            "",
        )
        .await
        .unwrap();
        let state = build_test_app_state(db, None);

        let resp = api_logs_export(
            axum::extract::State(state),
            axum::extract::Query(LogExportQuery {
                range: "yesterdayyy".to_string(),
            }),
        )
        .await
        .expect("export ok")
        .into_response();
        let body = read_response_body(resp).await;
        assert!(
            body.contains("should appear"),
            "unknown range must fall through to `all` rather than returning an empty body; got: {body}"
        );
    }

    #[tokio::test]
    async fn api_logs_export_today_range_excludes_old_entries() {
        // Insert a row with an explicit timestamp from 8 days ago,
        // verify the `today` range filter excludes it.
        let db = in_memory_pool().await;
        sqlx::query(
            "INSERT INTO logs (timestamp, level, category, message, detail) \
             VALUES (datetime('now', '-8 days'), 'info', 'system', 'old entry', '')",
        )
        .execute(&db)
        .await
        .unwrap();
        log::insert(
            &db,
            log::LogLevel::Info,
            log::LogCategory::System,
            "fresh entry",
            "",
        )
        .await
        .unwrap();
        let state = build_test_app_state(db, None);

        let resp = api_logs_export(
            axum::extract::State(state),
            axum::extract::Query(LogExportQuery {
                range: "today".to_string(),
            }),
        )
        .await
        .expect("export ok")
        .into_response();
        let body = read_response_body(resp).await;
        assert!(body.contains("fresh entry"));
        assert!(
            !body.contains("old entry"),
            "today range must exclude entries older than midnight UTC; got: {body}"
        );
    }

    // ── truncate_to_page (PR 132 review follow-up) ──────────────────
    //
    // The handler's `+1`-fetch cursor logic is shared between the
    // logs and RSS tabs. The model-level tests pin the `id < cursor`
    // contract (`recent_decisions_paginated_skips_entries_at_or_above_cursor`)
    // but the handler-side "is there an older page?" decision lives
    // here. A regression where the comparison flips to `>=` would
    // emit a non-empty cursor on the last page → user clicks Older
    // → empty page → confused. These three tests pin the boundaries.

    #[derive(Clone)]
    struct Row {
        id: i64,
    }

    #[test]
    fn truncate_to_page_returns_none_at_exact_page_size() {
        // Model returned exactly `page_size` rows — no extra row,
        // therefore no older page. Cursor must be None so the
        // template skips the "Older →" link.
        let entries: Vec<Row> = (1..=200).map(|id| Row { id }).collect();
        let (kept, older) = truncate_to_page(entries, 200, |r| r.id);
        assert_eq!(kept.len(), 200);
        assert_eq!(
            older, None,
            "200 rows on a page-size of 200 → no older page (no `+1` extra fetched)"
        );
    }

    #[test]
    fn truncate_to_page_returns_some_when_extra_row_present() {
        // Model returned page_size + 1 (the canonical "more pages"
        // signal). Truncate to page_size, return the now-last row's
        // id as the next cursor.
        let entries: Vec<Row> = (1..=201).map(|id| Row { id }).collect();
        let (kept, older) = truncate_to_page(entries, 200, |r| r.id);
        assert_eq!(kept.len(), 200);
        assert_eq!(
            older,
            Some(200),
            "201 rows on a page-size of 200 → cursor is the 200th row's id (the new boundary for `id < cursor`)"
        );
    }

    #[test]
    fn truncate_to_page_returns_none_for_short_page() {
        // Empty / partial page → no older page either.
        let (kept, older) = truncate_to_page::<Row, _>(vec![], 200, |r| r.id);
        assert_eq!(kept.len(), 0);
        assert_eq!(older, None);

        let entries: Vec<Row> = (1..=42).map(|id| Row { id }).collect();
        let (kept, older) = truncate_to_page(entries, 200, |r| r.id);
        assert_eq!(kept.len(), 42);
        assert_eq!(older, None);
    }
}
