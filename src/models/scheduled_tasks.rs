use sqlx::{Row, SqlitePool};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct ScheduledTaskStatus {
    pub task_key: String,
    pub display_name: String,
    pub schedule_label: String,
    pub enabled: bool,
    pub last_started_at: Option<String>,
    pub last_finished_at: Option<String>,
    pub last_status: String,
    pub last_detail: String,
}

pub async fn touch_definition(
    db: &SqlitePool,
    task_key: &str,
    display_name: &str,
    schedule_label: &str,
    enabled: bool,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO scheduled_task_runs (task_key, display_name, schedule_label, enabled)
        VALUES (?, ?, ?, ?)
        ON CONFLICT(task_key) DO UPDATE SET
            display_name = excluded.display_name,
            schedule_label = excluded.schedule_label,
            enabled = excluded.enabled,
            updated_at = CURRENT_TIMESTAMP
        "#,
    )
    .bind(task_key)
    .bind(display_name)
    .bind(schedule_label)
    .bind(if enabled {1_i64} else {0_i64})
    .execute(db)
    .await?;
    Ok(())
}

pub async fn mark_started(db: &SqlitePool, task_key: &str, detail: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"UPDATE scheduled_task_runs
           SET last_started_at = CURRENT_TIMESTAMP,
               last_status = 'running',
               last_detail = ?,
               updated_at = CURRENT_TIMESTAMP
           WHERE task_key = ?"#,
    )
    .bind(detail)
    .bind(task_key)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn mark_finished(db: &SqlitePool, task_key: &str, status: &str, detail: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"UPDATE scheduled_task_runs
           SET last_finished_at = CURRENT_TIMESTAMP,
               last_status = ?,
               last_detail = ?,
               updated_at = CURRENT_TIMESTAMP
           WHERE task_key = ?"#,
    )
    .bind(status)
    .bind(detail)
    .bind(task_key)
    .execute(db)
    .await?;
    Ok(())
}

/// Minutes elapsed since `task_key` last finished. Returns `None` if
/// the task has never recorded a `last_finished_at` (fresh install,
/// or task has never been run). The caller should treat `None` as
/// "run immediately" so first-time setup still fires on startup.
///
/// Uses SQLite's `strftime('%s', ...)` on both sides so the
/// computation lives entirely in the DB: we don't parse timestamps
/// into a DateTime type on the Rust side just to subtract them. The
/// column is stored in UTC (`CURRENT_TIMESTAMP`), and `'now'` is
/// also UTC, so no timezone math is needed.
pub async fn minutes_since_last_finished(
    db: &SqlitePool,
    task_key: &str,
) -> Option<i64> {
    let row = sqlx::query(
        r#"SELECT CAST((strftime('%s','now') - strftime('%s', last_finished_at)) / 60 AS INTEGER) AS minutes_ago
           FROM scheduled_task_runs
           WHERE task_key = ? AND last_finished_at IS NOT NULL"#,
    )
    .bind(task_key)
    .fetch_optional(db)
    .await
    .ok()
    .flatten()?;
    row.try_get::<i64, _>("minutes_ago").ok()
}

/// How long a background task should sleep at startup before its next
/// run, given a fixed `interval` between runs. Uses the persisted
/// `last_finished_at` to compute a remaining-window duration:
///
/// - Task never ran (fresh install, new task key): returns `ZERO` so
///   it runs immediately on startup.
/// - Task finished `elapsed >= interval` ago: returns `ZERO` — the
///   next run is overdue and should happen now.
/// - Task finished within the interval: returns `interval - elapsed`
///   so the restart effectively resumes the prior schedule rather
///   than re-firing the work a few minutes after the previous run.
///
/// This is the fix for the "every `cargo run` re-runs the 12h
/// metadata sweep" class of bug: without it, a bare `interval.tick()`
/// fires on the first call, and even patterns that skip-tick (like
/// `anibridge_refresh` and `library_classify`) waste a full interval
/// after each restart regardless of when the task actually last ran.
pub async fn duration_until_next_run(
    db: &SqlitePool,
    task_key: &str,
    interval: Duration,
) -> Duration {
    let Some(minutes_ago) = minutes_since_last_finished(db, task_key).await else {
        return Duration::ZERO;
    };
    if minutes_ago < 0 {
        return Duration::ZERO;
    }
    let elapsed = Duration::from_secs((minutes_ago as u64).saturating_mul(60));
    interval.saturating_sub(elapsed)
}

pub async fn list(db: &SqlitePool) -> Result<Vec<ScheduledTaskStatus>, sqlx::Error> {
    let rows = sqlx::query(
        r#"SELECT task_key, display_name, schedule_label, enabled, last_started_at, last_finished_at, last_status, last_detail
           FROM scheduled_task_runs
           ORDER BY display_name"#,
    )
    .fetch_all(db)
    .await?;

    Ok(rows.into_iter().map(|row| ScheduledTaskStatus {
        task_key: row.get("task_key"),
        display_name: row.get("display_name"),
        schedule_label: row.get("schedule_label"),
        enabled: row.get::<i64, _>("enabled") != 0,
        last_started_at: row.get::<Option<String>, _>("last_started_at"),
        last_finished_at: row.get::<Option<String>, _>("last_finished_at"),
        last_status: row.get("last_status"),
        last_detail: row.get("last_detail"),
    }).collect())
}
