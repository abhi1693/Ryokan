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

/// Get all grabbed torrents with series title, ordered by most recent first.
pub async fn get_all_with_series(db: &SqlitePool, limit: i64) -> Result<Vec<GrabbedTorrentWithSeries>, sqlx::Error> {
    let rows = sqlx::query(
        r#"SELECT g.id, g.hash, g.torrent_name, g.series_id, g.episode_numbers, g.state, g.grabbed_at, g.imported_at,
                  COALESCE(s.title_english, s.title_romaji, s.title, '') AS series_title,
                  COALESCE(s.anilist_id, 0) AS anilist_id
           FROM grabbed_torrents g
           LEFT JOIN series s ON s.id = g.series_id
           ORDER BY g.grabbed_at DESC
           LIMIT ?"#,
    )
    .bind(limit)
    .fetch_all(db)
    .await?;

    Ok(rows
        .iter()
        .map(|row| {
            let eps_json: String = row.get("episode_numbers");
            let episode_numbers: Vec<i32> = serde_json::from_str(&eps_json).unwrap_or_default();
            GrabbedTorrentWithSeries {
                id: row.get("id"),
                hash: row.get("hash"),
                torrent_name: row.get("torrent_name"),
                series_id: row.get("series_id"),
                episode_numbers,
                state: row.get("state"),
                grabbed_at: row.get("grabbed_at"),
                imported_at: row.get("imported_at"),
                series_title: row.get("series_title"),
                anilist_id: row.get("anilist_id"),
            }
        })
        .collect())
}

/// Get all failed/blocked torrents.
pub async fn get_blocked(db: &SqlitePool) -> Result<Vec<GrabbedTorrentWithSeries>, sqlx::Error> {
    let rows = sqlx::query(
        r#"SELECT g.id, g.hash, g.torrent_name, g.series_id, g.episode_numbers, g.state, g.grabbed_at, g.imported_at,
                  COALESCE(s.title_english, s.title_romaji, s.title, '') AS series_title,
                  COALESCE(s.anilist_id, 0) AS anilist_id
           FROM grabbed_torrents g
           LEFT JOIN series s ON s.id = g.series_id
           WHERE g.state = 'failed'
           ORDER BY g.grabbed_at DESC"#,
    )
    .fetch_all(db)
    .await?;

    Ok(rows
        .iter()
        .map(|row| {
            let eps_json: String = row.get("episode_numbers");
            let episode_numbers: Vec<i32> = serde_json::from_str(&eps_json).unwrap_or_default();
            GrabbedTorrentWithSeries {
                id: row.get("id"),
                hash: row.get("hash"),
                torrent_name: row.get("torrent_name"),
                series_id: row.get("series_id"),
                episode_numbers,
                state: row.get("state"),
                grabbed_at: row.get("grabbed_at"),
                imported_at: row.get("imported_at"),
                series_title: row.get("series_title"),
                anilist_id: row.get("anilist_id"),
            }
        })
        .collect())
}

/// Remove a grabbed torrent record entirely.
pub async fn remove(db: &SqlitePool, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM grabbed_torrents WHERE id = ?")
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct GrabbedTorrentWithSeries {
    pub id: i64,
    pub hash: String,
    pub torrent_name: String,
    pub series_id: i64,
    pub episode_numbers: Vec<i32>,
    pub state: String,
    pub grabbed_at: String,
    pub imported_at: Option<String>,
    pub series_title: String,
    pub anilist_id: i64,
}
