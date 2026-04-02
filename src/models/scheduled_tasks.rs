use sqlx::{Row, SqlitePool};

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
