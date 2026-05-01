// ── Endpoint coverage ──────────────────────────────────────────────
//
// Direct-call coverage for the smaller `/api/*` system endpoints —
// no router needed because each handler takes `State<AppState>` and
// returns its own response. Targets the mostly-DB-only paths
// (logs, cleanup, library-classify, upgrade-search dispatch) and
// the rate-limited client-log ingest.
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
    let resp = api_force_library_classify(axum::extract::State(state))
        .await
        .expect("library classify spawn should succeed");
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

#[tokio::test]
async fn api_force_external_sync_no_account_returns_friendly_message() {
    // The Run-now button on the Scheduled Tasks tab hits this
    // endpoint. With no AL/MAL account linked, the underlying
    // `tick_once_or_busy` returns Ok(empty summary) — confusing
    // for the user since the toast would say "Sync complete." Pin
    // the explicit 400 + "no account linked" message so future
    // refactors can't silently regress to the no-op-success shape.
    // Status (vs body) is what `system.js` keys "Task failed" toast
    // off of (`r.ok`), so a green-toast regression would surface
    // here as the wrong status.
    let db = in_memory_pool().await;
    let state = build_test_app_state(db, None);
    let resp = api_force_external_sync(axum::extract::State(state)).await;
    let (status, body) = resp.expect_err("no-account branch must surface as Err");
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
    assert!(
        body.contains("No external account is linked"),
        "no-account message must surface the precise remediation; got: {body}"
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
