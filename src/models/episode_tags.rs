use sqlx::{Row, SqlitePool};

use crate::services::source::ClassificationResult;

#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
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
    /// Structured source label ("BluRay", "Web", "DVD", "HDTV", "TV",
    /// "Unknown", or empty for rows grabbed before Phase 1b).
    pub source: String,
    /// Structured resolution label ("1080p", "720p", …, or empty).
    pub resolution: String,
    pub is_remux: bool,
    pub classification_confidence: f32,
    pub needs_review: bool,
}

/// Record a new grab for an episode — inserts into history and upserts the
/// current tag.
///
/// The legacy `quality_tag` column is populated from `classification.label()`
/// so existing read paths continue to work; the new structured columns
/// (source, resolution, is_remux, classification_confidence, needs_review)
/// are written alongside it on `episode_quality_tags`. `manual_override` is
/// intentionally preserved across re-grabs — a user-set override should
/// stick even when a newer grab comes in.
pub async fn record_grab(
    db: &SqlitePool,
    series_id: i64,
    episode_number: i32,
    classification: &ClassificationResult,
    release_title: &str,
    release_group: &str,
) -> Result<i64, sqlx::Error> {
    let quality_tag = classification.label();
    let source_str = classification.source.as_str();
    let resolution_str = classification.resolution.as_str();
    let is_remux = if classification.is_remux { 1_i64 } else { 0_i64 };
    let confidence = classification.confidence as f64;
    let needs_review = if classification.needs_review { 1_i64 } else { 0_i64 };

    let history_id: i64 = sqlx::query_scalar(
        "INSERT INTO episode_grab_history (series_id, episode_number, quality_tag, release_title, release_group, state)
         VALUES (?, ?, ?, ?, ?, 'grabbed')
         RETURNING id",
    )
    .bind(series_id)
    .bind(episode_number)
    .bind(&quality_tag)
    .bind(release_title)
    .bind(release_group)
    .fetch_one(db)
    .await?;

    sqlx::query(
        "INSERT INTO episode_quality_tags (
             series_id, episode_number, quality_tag, release_title, release_group, state,
             source, resolution, is_remux, classification_confidence, needs_review
         )
         VALUES (?, ?, ?, ?, ?, 'grabbed', ?, ?, ?, ?, ?)
         ON CONFLICT(series_id, episode_number) DO UPDATE SET
             quality_tag = excluded.quality_tag,
             release_title = excluded.release_title,
             release_group = excluded.release_group,
             state = 'grabbed',
             source = excluded.source,
             resolution = excluded.resolution,
             is_remux = excluded.is_remux,
             classification_confidence = excluded.classification_confidence,
             needs_review = excluded.needs_review,
             updated_at = CURRENT_TIMESTAMP",
    )
    .bind(series_id)
    .bind(episode_number)
    .bind(&quality_tag)
    .bind(release_title)
    .bind(release_group)
    .bind(source_str)
    .bind(resolution_str)
    .bind(is_remux)
    .bind(confidence)
    .bind(needs_review)
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
        "SELECT episode_number, quality_tag, release_title, release_group, state,
                source, resolution, is_remux, classification_confidence, needs_review
         FROM episode_quality_tags WHERE series_id = ?",
    )
    .bind(series_id)
    .fetch_all(db)
    .await?;

    let mut map = std::collections::HashMap::new();
    for row in rows {
        let ep_num: i32 = row.get("episode_number");
        let is_remux_i: i64 = row.get("is_remux");
        let needs_review_i: i64 = row.get("needs_review");
        let confidence: f64 = row.get("classification_confidence");
        map.insert(
            ep_num,
            EpisodeQualityTag {
                quality_tag: row.get("quality_tag"),
                release_title: row.get("release_title"),
                release_group: row.get("release_group"),
                state: row.get("state"),
                source: row.get("source"),
                resolution: row.get("resolution"),
                is_remux: is_remux_i != 0,
                classification_confidence: confidence as f32,
                needs_review: needs_review_i != 0,
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

/// Mark episode quality tags as "completed" for the given episodes of a series.
/// Called by post-processing after a torrent is successfully imported.
pub async fn mark_completed(
    db: &SqlitePool,
    series_id: i64,
    episode_numbers: &[i32],
) -> Result<(), sqlx::Error> {
    for &ep in episode_numbers {
        sqlx::query(
            "UPDATE episode_quality_tags SET state = 'completed', updated_at = CURRENT_TIMESTAMP
             WHERE series_id = ? AND episode_number = ? AND state = 'grabbed'",
        )
        .bind(series_id)
        .bind(ep)
        .execute(db)
        .await?;
    }
    Ok(())
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

/// Clear episode quality tags and mark grab history as "removed" for all episodes
/// associated with a grabbed torrent (identified by series_id + episode_numbers).
pub async fn clear_tags_for_removal(
    db: &SqlitePool,
    series_id: i64,
    episode_numbers: &[i32],
) -> Result<(), sqlx::Error> {
    for &ep in episode_numbers {
        // Delete the current quality tag so the episode no longer shows as grabbed.
        sqlx::query(
            "DELETE FROM episode_quality_tags WHERE series_id = ? AND episode_number = ?",
        )
        .bind(series_id)
        .bind(ep)
        .execute(db)
        .await?;

        // Mark any "grabbed" history entries for this episode as "removed".
        sqlx::query(
            "UPDATE episode_grab_history SET state = 'removed'
             WHERE series_id = ? AND episode_number = ? AND state = 'grabbed'",
        )
        .bind(series_id)
        .bind(ep)
        .execute(db)
        .await?;
    }
    Ok(())
}

/// Mark a grab history entry as failed, and update the current tag state if it matches.
pub async fn mark_grab_failed(
    db: &SqlitePool,
    history_id: i64,
) -> Result<(i64, i32, String), sqlx::Error> {
    // Fetch series_id, episode_number, release_title before marking failed.
    let row = sqlx::query(
        "SELECT series_id, episode_number, release_title FROM episode_grab_history WHERE id = ?",
    )
    .bind(history_id)
    .fetch_one(db)
    .await?;
    let series_id: i64 = row.get("series_id");
    let episode_number: i32 = row.get("episode_number");
    let release_title: String = row.get("release_title");

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

    Ok((series_id, episode_number, release_title))
}
