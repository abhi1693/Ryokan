use sqlx::{Row, SqlitePool};

#[derive(Debug, Clone, serde::Serialize)]
pub struct GrabHistoryEntry {
    pub id: i64,
    pub quality_tag: String,
    pub release_title: String,
    pub release_group: String,
    pub grabbed_at: String,
    pub state: String,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct EpisodeQualityTag {
    pub quality_tag: String,
    pub release_title: String,
    pub release_group: String,
    pub state: String,
}

/// Record a new grab for an episode — inserts into history and upserts the current tag.
pub async fn record_grab(
    db: &SqlitePool,
    series_id: i64,
    episode_number: i32,
    quality_tag: &str,
    release_title: &str,
    release_group: &str,
) -> Result<i64, sqlx::Error> {
    let history_id: i64 = sqlx::query_scalar(
        "INSERT INTO episode_grab_history (series_id, episode_number, quality_tag, release_title, release_group, state)
         VALUES (?, ?, ?, ?, ?, 'grabbed')
         RETURNING id",
    )
    .bind(series_id)
    .bind(episode_number)
    .bind(quality_tag)
    .bind(release_title)
    .bind(release_group)
    .fetch_one(db)
    .await?;

    sqlx::query(
        "INSERT INTO episode_quality_tags (series_id, episode_number, quality_tag, release_title, release_group, state)
         VALUES (?, ?, ?, ?, ?, 'grabbed')
         ON CONFLICT(series_id, episode_number) DO UPDATE SET
             quality_tag = excluded.quality_tag,
             release_title = excluded.release_title,
             release_group = excluded.release_group,
             state = 'grabbed',
             updated_at = CURRENT_TIMESTAMP",
    )
    .bind(series_id)
    .bind(episode_number)
    .bind(quality_tag)
    .bind(release_title)
    .bind(release_group)
    .execute(db)
    .await?;

    Ok(history_id)
}

/// Get the current quality tag map for a series.
pub async fn get_for_series(
    db: &SqlitePool,
    series_id: i64,
) -> Result<std::collections::HashMap<i32, EpisodeQualityTag>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT episode_number, quality_tag, release_title, release_group, state
         FROM episode_quality_tags WHERE series_id = ?",
    )
    .bind(series_id)
    .fetch_all(db)
    .await?;

    let mut map = std::collections::HashMap::new();
    for row in rows {
        let ep_num: i32 = row.get("episode_number");
        map.insert(
            ep_num,
            EpisodeQualityTag {
                quality_tag: row.get("quality_tag"),
                release_title: row.get("release_title"),
                release_group: row.get("release_group"),
                state: row.get("state"),
            },
        );
    }
    Ok(map)
}

/// Get grab history for a specific episode (newest first, up to 10 entries).
pub async fn get_grab_history(
    db: &SqlitePool,
    series_id: i64,
    episode_number: i32,
) -> Result<Vec<GrabHistoryEntry>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id, quality_tag, release_title, release_group, grabbed_at, state
         FROM episode_grab_history
         WHERE series_id = ? AND episode_number = ?
         ORDER BY grabbed_at DESC
         LIMIT 10",
    )
    .bind(series_id)
    .bind(episode_number)
    .fetch_all(db)
    .await?;

    Ok(rows
        .iter()
        .map(|row| GrabHistoryEntry {
            id: row.get("id"),
            quality_tag: row.get("quality_tag"),
            release_title: row.get("release_title"),
            release_group: row.get("release_group"),
            grabbed_at: row.get("grabbed_at"),
            state: row.get("state"),
        })
        .collect())
}

/// Clear the current quality tag for an episode (e.g. after file deletion).
pub async fn clear_episode_tag(
    db: &SqlitePool,
    series_id: i64,
    episode_number: i32,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "DELETE FROM episode_quality_tags WHERE series_id = ? AND episode_number = ?",
    )
    .bind(series_id)
    .bind(episode_number)
    .execute(db)
    .await?;
    Ok(())
}

/// Mark a grab history entry as failed, and update the current tag state if it matches.
pub async fn mark_grab_failed(
    db: &SqlitePool,
    history_id: i64,
) -> Result<(i64, i32), sqlx::Error> {
    // Fetch series_id + episode_number before marking failed so we can return them.
    let row = sqlx::query(
        "SELECT series_id, episode_number FROM episode_grab_history WHERE id = ?",
    )
    .bind(history_id)
    .fetch_one(db)
    .await?;
    let series_id: i64 = row.get("series_id");
    let episode_number: i32 = row.get("episode_number");

    sqlx::query("UPDATE episode_grab_history SET state = 'failed' WHERE id = ?")
        .bind(history_id)
        .execute(db)
        .await?;

    // Update the current tag to 'failed' so the UI reflects the state.
    sqlx::query(
        "UPDATE episode_quality_tags SET state = 'failed', updated_at = CURRENT_TIMESTAMP
         WHERE series_id = ? AND episode_number = ?",
    )
    .bind(series_id)
    .bind(episode_number)
    .execute(db)
    .await?;

    Ok((series_id, episode_number))
}
