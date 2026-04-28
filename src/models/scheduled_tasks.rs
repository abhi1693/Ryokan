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
    .bind(if enabled { 1_i64 } else { 0_i64 })
    .execute(db)
    .await?;
    Ok(())
}

pub async fn mark_started(
    db: &SqlitePool,
    task_key: &str,
    detail: &str,
) -> Result<(), sqlx::Error> {
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

pub async fn mark_finished(
    db: &SqlitePool,
    task_key: &str,
    status: &str,
    detail: &str,
) -> Result<(), sqlx::Error> {
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

/// Boot-time recovery: any row left at `last_status = 'running'` when
/// the process starts is by definition stuck. The handler / supervised
/// loop that wrote it can't still be running — they live in this
/// process, and this process just started. The `running` row was
/// stranded by a prior crash, kill -9, OOM, container restart, or (the
/// 2026-04-28 incident) a request future being dropped on tab-out
/// before the spawn-detach helper landed. Without this pass, a stuck
/// `running` row sits forever in System → Scheduled Tasks misleadingly
/// suggesting work is in flight.
///
/// Returns the number of rows recovered so the caller can log it. The
/// detail message is deliberately user-facing — it shows up in the
/// row's last_detail column on the UI.
pub async fn recover_stuck_running(db: &SqlitePool) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        r#"UPDATE scheduled_task_runs
           SET last_status = 'error',
               last_finished_at = CURRENT_TIMESTAMP,
               last_detail = 'Task interrupted by process restart. Click Run now to retry.',
               updated_at = CURRENT_TIMESTAMP
           WHERE last_status = 'running'"#,
    )
    .execute(db)
    .await?;
    Ok(result.rows_affected())
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
pub async fn minutes_since_last_finished(db: &SqlitePool, task_key: &str) -> Option<i64> {
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

    Ok(rows
        .into_iter()
        .map(|row| ScheduledTaskStatus {
            task_key: row.get("task_key"),
            display_name: row.get("display_name"),
            schedule_label: row.get("schedule_label"),
            enabled: row.get::<i64, _>("enabled") != 0,
            last_started_at: row.get::<Option<String>, _>("last_started_at"),
            last_finished_at: row.get::<Option<String>, _>("last_finished_at"),
            last_status: row.get("last_status"),
            last_detail: row.get("last_detail"),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::in_memory_pool;

    #[tokio::test]
    async fn touch_definition_inserts_then_upserts() {
        let db = in_memory_pool().await;
        touch_definition(&db, "rss_sync", "RSS Sync", "every 60s", true)
            .await
            .unwrap();

        let rows = list(&db).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].task_key, "rss_sync");
        assert_eq!(rows[0].display_name, "RSS Sync");
        assert_eq!(rows[0].schedule_label, "every 60s");
        assert!(rows[0].enabled);

        // Re-touch updates display name + label without inserting a row.
        touch_definition(&db, "rss_sync", "RSS Sync (renamed)", "every 30s", false)
            .await
            .unwrap();
        let rows = list(&db).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].display_name, "RSS Sync (renamed)");
        assert_eq!(rows[0].schedule_label, "every 30s");
        assert!(!rows[0].enabled);
    }

    #[tokio::test]
    async fn mark_started_then_finished_writes_status() {
        let db = in_memory_pool().await;
        touch_definition(&db, "post_proc", "Post-Processing", "60s", true)
            .await
            .unwrap();

        mark_started(&db, "post_proc", "starting").await.unwrap();
        let rows = list(&db).await.unwrap();
        assert_eq!(rows[0].last_status, "running");
        assert_eq!(rows[0].last_detail, "starting");
        assert!(rows[0].last_started_at.is_some());
        assert!(rows[0].last_finished_at.is_none());

        mark_finished(&db, "post_proc", "ok", "imported 3 files")
            .await
            .unwrap();
        let rows = list(&db).await.unwrap();
        assert_eq!(rows[0].last_status, "ok");
        assert_eq!(rows[0].last_detail, "imported 3 files");
        assert!(rows[0].last_finished_at.is_some());
    }

    #[tokio::test]
    async fn recover_stuck_running_flips_running_rows_to_error() {
        let db = in_memory_pool().await;
        touch_definition(&db, "metadata_refresh", "Metadata Refresh", "12h", true)
            .await
            .unwrap();
        touch_definition(&db, "rss_sync", "RSS Sync", "60s", true)
            .await
            .unwrap();
        touch_definition(&db, "cleanup", "Cleanup", "1h", true)
            .await
            .unwrap();
        // Two stuck-running rows + one healthy ok row.
        mark_started(&db, "metadata_refresh", "Manual refresh started")
            .await
            .unwrap();
        mark_started(&db, "rss_sync", "Auto sync started")
            .await
            .unwrap();
        mark_started(&db, "cleanup", "Auto cleanup").await.unwrap();
        mark_finished(&db, "cleanup", "ok", "Cleanup completed")
            .await
            .unwrap();

        let rows = list(&db).await.unwrap();
        let running: Vec<_> = rows.iter().filter(|r| r.last_status == "running").collect();
        assert_eq!(running.len(), 2, "expected 2 running rows pre-recovery");

        let recovered = recover_stuck_running(&db).await.unwrap();
        assert_eq!(recovered, 2);

        let rows = list(&db).await.unwrap();
        let by_key: std::collections::HashMap<_, _> =
            rows.iter().map(|r| (r.task_key.as_str(), r)).collect();

        let metadata = by_key["metadata_refresh"];
        assert_eq!(metadata.last_status, "error");
        assert!(
            metadata
                .last_detail
                .contains("interrupted by process restart"),
            "user-facing message must explain the cause: {}",
            metadata.last_detail
        );
        assert!(
            metadata.last_finished_at.is_some(),
            "recovered row must record a finished_at so the next run can compute elapsed time"
        );

        // Healthy row is untouched.
        let cleanup = by_key["cleanup"];
        assert_eq!(cleanup.last_status, "ok");
        assert_eq!(cleanup.last_detail, "Cleanup completed");
    }

    #[tokio::test]
    async fn recover_stuck_running_is_idempotent_with_no_running_rows() {
        let db = in_memory_pool().await;
        touch_definition(&db, "rss_sync", "RSS Sync", "60s", true)
            .await
            .unwrap();
        let recovered = recover_stuck_running(&db).await.unwrap();
        assert_eq!(recovered, 0, "no running rows means nothing to recover");
    }

    #[tokio::test]
    async fn mark_calls_on_unknown_key_are_noops() {
        // The UPDATE has a `WHERE task_key = ?` clause, so calling
        // mark_started/mark_finished without a prior touch_definition
        // is safe — it just affects zero rows.
        let db = in_memory_pool().await;
        mark_started(&db, "ghost", "x").await.unwrap();
        mark_finished(&db, "ghost", "ok", "y").await.unwrap();
        assert!(list(&db).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn list_orders_by_display_name() {
        let db = in_memory_pool().await;
        touch_definition(&db, "z_key", "Zebra", "1h", true)
            .await
            .unwrap();
        touch_definition(&db, "a_key", "Apple", "1h", true)
            .await
            .unwrap();
        touch_definition(&db, "m_key", "Mango", "1h", true)
            .await
            .unwrap();

        let rows = list(&db).await.unwrap();
        let names: Vec<_> = rows.iter().map(|r| r.display_name.as_str()).collect();
        assert_eq!(names, vec!["Apple", "Mango", "Zebra"]);
    }

    #[tokio::test]
    async fn minutes_since_last_finished_returns_none_for_never_run() {
        let db = in_memory_pool().await;
        // Never-touched task.
        assert!(minutes_since_last_finished(&db, "absent").await.is_none());
        // Touched but never finished.
        touch_definition(&db, "fresh", "Fresh", "1h", true)
            .await
            .unwrap();
        assert!(minutes_since_last_finished(&db, "fresh").await.is_none());
    }

    #[tokio::test]
    async fn minutes_since_last_finished_returns_zero_for_just_finished() {
        let db = in_memory_pool().await;
        touch_definition(&db, "k", "K", "1h", true).await.unwrap();
        mark_finished(&db, "k", "ok", "").await.unwrap();
        let m = minutes_since_last_finished(&db, "k").await.unwrap();
        // The minutes since "now" → 0 most of the time, but allow 1 if
        // we cross a minute boundary mid-test. It must never be > 1.
        assert!(m == 0 || m == 1, "unexpected minutes_ago: {m}");
    }

    #[tokio::test]
    async fn duration_until_next_run_zero_when_never_ran() {
        let db = in_memory_pool().await;
        let d = duration_until_next_run(&db, "never", Duration::from_secs(3600)).await;
        assert_eq!(d, Duration::ZERO);
    }

    #[tokio::test]
    async fn duration_until_next_run_zero_when_just_finished_with_short_interval() {
        // Finish the task, then ask for "next run" with a 1-second
        // interval — saturating_sub clamps to ZERO once elapsed >=
        // interval. This pins the "overdue → run now" branch.
        let db = in_memory_pool().await;
        touch_definition(&db, "k", "K", "1s", true).await.unwrap();
        mark_finished(&db, "k", "ok", "").await.unwrap();
        // Nudge the stored timestamp 5 minutes into the past so
        // elapsed >> interval regardless of test scheduling jitter.
        sqlx::query(
            "UPDATE scheduled_task_runs
             SET last_finished_at = datetime('now', '-5 minutes')
             WHERE task_key = ?",
        )
        .bind("k")
        .execute(&db)
        .await
        .unwrap();

        let d = duration_until_next_run(&db, "k", Duration::from_secs(60)).await;
        assert_eq!(d, Duration::ZERO);
    }

    #[tokio::test]
    async fn duration_until_next_run_returns_remainder_when_within_interval() {
        // Finished 5 minutes ago with a 1-hour interval → remainder
        // should be ~55 minutes. Allow a small tolerance for the
        // minute-truncation in `minutes_since_last_finished`.
        let db = in_memory_pool().await;
        touch_definition(&db, "k", "K", "1h", true).await.unwrap();
        mark_finished(&db, "k", "ok", "").await.unwrap();
        sqlx::query(
            "UPDATE scheduled_task_runs
             SET last_finished_at = datetime('now', '-5 minutes')
             WHERE task_key = ?",
        )
        .bind("k")
        .execute(&db)
        .await
        .unwrap();

        let d = duration_until_next_run(&db, "k", Duration::from_secs(3600)).await;
        // Expect ~55 minutes remaining (3300s); accept 54..=56 to
        // tolerate the SQL-side minute-bucket rounding.
        let secs = d.as_secs();
        assert!(
            (3240..=3360).contains(&secs),
            "unexpected remainder: {secs}s"
        );
    }
}
