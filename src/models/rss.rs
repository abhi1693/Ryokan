use serde::Serialize;
use sqlx::SqlitePool;

// sqlx row-tuple shapes for the `rss_runs` and `rss_seen` SELECTs.
// Aliased so the call-site annotations don't blow past Clippy's
// `type_complexity` lint and — more importantly — so a schema change
// has exactly one place to edit instead of two parallel tuple
// declarations that have to stay in lock-step.
type RssRunRow = (
    i64,
    String,
    Option<String>,
    String,
    String,
    i64,
    i64,
    i64,
    i64,
    String,
);

type RssDecisionRow = (
    i64,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    i64,
);

#[derive(Debug, Clone, Serialize)]
pub struct RssRun {
    pub id: i64,
    pub started_at: String,
    pub finished_at: String,
    pub trigger_source: String,
    pub status: String,
    pub items_seen: i32,
    pub matched: i32,
    pub grabbed: i32,
    pub skipped: i32,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RssDecision {
    pub id: i64,
    pub created_at: String,
    pub title: String,
    pub series_title: String,
    pub group_name: String,
    pub decision: String,
    pub reason: String,
    pub source: String,
    pub is_batch: bool,
}

pub async fn start_run(db: &SqlitePool, trigger_source: &str) -> Result<i64, sqlx::Error> {
    let result = sqlx::query(
        "INSERT INTO rss_runs (trigger_source, status) VALUES (?, 'running')",
    )
    .bind(trigger_source)
    .execute(db)
    .await?;
    Ok(result.last_insert_rowid())
}

/// Per-run counters that `finish_run` writes back to `rss_runs`. Named
/// fields instead of four positional `i32`s so callers can't swap
/// `matched` and `grabbed` (or similar) by mistake at the callsite —
/// the compiler won't catch it when every slot is the same type.
pub struct RunSummary<'a> {
    pub status: &'a str,
    pub items_seen: i32,
    pub matched: i32,
    pub grabbed: i32,
    pub skipped: i32,
    pub detail: &'a str,
}

pub async fn finish_run(
    db: &SqlitePool,
    id: i64,
    summary: RunSummary<'_>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"UPDATE rss_runs
           SET finished_at = CURRENT_TIMESTAMP,
               status = ?,
               items_seen = ?,
               matched = ?,
               grabbed = ?,
               skipped = ?,
               detail = ?
         WHERE id = ?"#,
    )
    .bind(summary.status)
    .bind(summary.items_seen)
    .bind(summary.matched)
    .bind(summary.grabbed)
    .bind(summary.skipped)
    .bind(summary.detail)
    .bind(id)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn item_was_grabbed(db: &SqlitePool, item_key: &str) -> Result<bool, sqlx::Error> {
    let row: Option<(i64,)> = sqlx::query_as("SELECT id FROM rss_seen WHERE item_key = ? AND decision = 'grabbed'")
        .bind(item_key)
        .fetch_optional(db)
        .await?;
    Ok(row.is_some())
}

/// Payload for `record_decision`. Named fields instead of six `&str`s
/// in a row — swapping `title`/`link` or `decision`/`reason` at a
/// callsite was a silent-corruption risk the compiler couldn't catch,
/// and Clippy was already complaining about the arg count.
pub struct DecisionRecord<'a> {
    pub item_key: &'a str,
    pub title: &'a str,
    pub link: &'a str,
    pub series_id: Option<i64>,
    pub series_title: &'a str,
    pub group_name: &'a str,
    pub is_batch: bool,
    pub decision: &'a str,
    pub reason: &'a str,
    pub source: &'a str,
}

pub async fn record_decision(
    db: &SqlitePool,
    record: DecisionRecord<'_>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"INSERT INTO rss_seen
           (item_key, title, link, series_id, series_title, group_name, is_batch, decision, reason, source, created_at)
           VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, CURRENT_TIMESTAMP)
           ON CONFLICT(item_key) DO UPDATE SET
               title = excluded.title,
               link = excluded.link,
               series_id = excluded.series_id,
               series_title = excluded.series_title,
               group_name = excluded.group_name,
               is_batch = excluded.is_batch,
               decision = excluded.decision,
               reason = excluded.reason,
               source = excluded.source,
               created_at = CURRENT_TIMESTAMP"#,
    )
    .bind(record.item_key)
    .bind(record.title)
    .bind(record.link)
    .bind(record.series_id)
    .bind(record.series_title)
    .bind(record.group_name)
    .bind(i64::from(record.is_batch))
    .bind(record.decision)
    .bind(record.reason)
    .bind(record.source)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn latest_run(db: &SqlitePool) -> Result<Option<RssRun>, sqlx::Error> {
    let row: Option<RssRunRow> = sqlx::query_as(
        r#"SELECT id, started_at, finished_at, trigger_source, status, items_seen, matched, grabbed, skipped, detail
           FROM rss_runs ORDER BY id DESC LIMIT 1"#,
    )
    .fetch_optional(db)
    .await?;

    Ok(row.map(|(id, started_at, finished_at, trigger_source, status, items_seen, matched, grabbed, skipped, detail)| RssRun {
        id,
        started_at,
        finished_at: finished_at.unwrap_or_default(),
        trigger_source,
        status,
        items_seen: items_seen as i32,
        matched: matched as i32,
        grabbed: grabbed as i32,
        skipped: skipped as i32,
        detail,
    }))
}

pub async fn recent_decisions(db: &SqlitePool, limit: i64) -> Result<Vec<RssDecision>, sqlx::Error> {
    let rows: Vec<RssDecisionRow> = sqlx::query_as(
        r#"SELECT id, created_at, title, series_title, group_name, decision, reason, source, is_batch
           FROM rss_seen ORDER BY id DESC LIMIT ?"#,
    )
    .bind(limit)
    .fetch_all(db)
    .await?;

    Ok(rows.into_iter().map(|(id, created_at, title, series_title, group_name, decision, reason, source, is_batch)| RssDecision {
        id,
        created_at,
        title,
        series_title,
        group_name,
        decision,
        reason,
        source,
        is_batch: is_batch != 0,
    }).collect())
}


pub async fn grabbed_titles(db: &SqlitePool, limit: i64) -> Result<Vec<String>, sqlx::Error> {
    let rows: Vec<(String,)> = sqlx::query_as(
        r#"SELECT title FROM rss_seen WHERE decision = 'grabbed' ORDER BY id DESC LIMIT ?"#,
    )
    .bind(limit)
    .fetch_all(db)
    .await?;

    Ok(rows.into_iter().map(|(title,)| title).collect())
}

/// Delete all RSS decisions older than `days` days (grabbed included).
/// Nyaa's RSS feed only holds items for ~2 days, so 30-day retention is
/// more than enough to prevent re-grabs while still allowing re-downloads
/// after a file is deleted from disk.
pub async fn cleanup_old_decisions(db: &SqlitePool, days: i64) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        r#"DELETE FROM rss_seen
           WHERE created_at < datetime('now', '-' || ? || ' days')"#,
    )
    .bind(days)
    .execute(db)
    .await?;
    Ok(result.rows_affected())
}

/// Clear all grab history so previously-grabbed items can be re-evaluated.
pub async fn clear_grab_history(db: &SqlitePool) -> Result<u64, sqlx::Error> {
    let result = sqlx::query("DELETE FROM rss_seen WHERE decision = 'grabbed'")
        .execute(db)
        .await?;
    Ok(result.rows_affected())
}

/// Clear all RSS decision history.
#[allow(dead_code)]
pub async fn clear_all_history(db: &SqlitePool) -> Result<u64, sqlx::Error> {
    let result = sqlx::query("DELETE FROM rss_seen")
        .execute(db)
        .await?;
    Ok(result.rows_affected())
}
