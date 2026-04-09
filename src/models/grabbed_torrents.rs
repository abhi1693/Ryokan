use sqlx::{Row, SqlitePool};

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct GrabbedTorrent {
    pub id: i64,
    pub hash: String,
    pub torrent_name: String,
    pub series_id: i64,
    pub episode_numbers: Vec<i32>,
    pub state: String,
}

/// Record a torrent grab for post-processing. Skips silently if we already
/// have a pending or imported record with the same (non-empty) hash.
pub async fn record_grab(
    db: &SqlitePool,
    hash: &str,
    torrent_name: &str,
    series_id: i64,
    episode_numbers: &[i32],
) -> Result<(), sqlx::Error> {
    let eps_json = serde_json::to_string(episode_numbers).unwrap_or_else(|_| "[]".to_string());

    if !hash.is_empty() {
        let existing: Option<i64> = sqlx::query_scalar(
            "SELECT id FROM grabbed_torrents WHERE hash = ? AND state IN ('pending', 'imported') LIMIT 1",
        )
        .bind(hash)
        .fetch_optional(db)
        .await?;
        if existing.is_some() {
            return Ok(());
        }
    }

    sqlx::query(
        "INSERT INTO grabbed_torrents (hash, torrent_name, series_id, episode_numbers, state) VALUES (?, ?, ?, ?, 'pending')",
    )
    .bind(hash)
    .bind(torrent_name)
    .bind(series_id)
    .bind(&eps_json)
    .execute(db)
    .await?;

    Ok(())
}

/// Get all grabs that have not yet been processed.
pub async fn get_all_pending(db: &SqlitePool) -> Result<Vec<GrabbedTorrent>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id, hash, torrent_name, series_id, episode_numbers FROM grabbed_torrents WHERE state = 'pending' ORDER BY grabbed_at ASC",
    )
    .fetch_all(db)
    .await?;

    Ok(rows
        .iter()
        .map(|row| {
            let eps_json: String = row.get("episode_numbers");
            let episode_numbers: Vec<i32> =
                serde_json::from_str(&eps_json).unwrap_or_default();
            GrabbedTorrent {
                id: row.get("id"),
                hash: row.get("hash"),
                torrent_name: row.get("torrent_name"),
                series_id: row.get("series_id"),
                episode_numbers,
                state: "pending".to_string(),
            }
        })
        .collect())
}

pub async fn mark_imported(db: &SqlitePool, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE grabbed_torrents SET state = 'imported', imported_at = CURRENT_TIMESTAMP WHERE id = ?",
    )
    .bind(id)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn mark_failed(db: &SqlitePool, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE grabbed_torrents SET state = 'failed' WHERE id = ?")
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}
