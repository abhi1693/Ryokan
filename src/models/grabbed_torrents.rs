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
    pub grabbed_at: String,
    /// Whether the original Nyaa listing was marked as a batch/season
    /// pack. Persisted at grab time so the post-download classifier can
    /// re-run Layer 4 (temporal inference) with the same value the
    /// pre-download pass used — otherwise the "finished 1+ year ago +
    /// batch → BluRay" rule never fires on library-sweep reclassifies.
    pub is_batch: bool,
}

/// Record a torrent grab for post-processing. Skips silently if we
/// already have a pending or imported record with the same (non-empty)
/// hash, in which case `Ok(None)` is returned. On a fresh insert,
/// returns `Ok(Some(id))` so the Phase 2 multi-series routing path can
/// attach `grabbed_torrent_series` rows via
/// [`record_grab_series_routes`] without re-querying.
///
/// `is_batch` is the caller's view (from the Nyaa listing or search
/// hit) of whether the release is a batch/season pack. Persisted so
/// the post-download classifier can feed the same flag back into
/// Layer 4.
pub async fn record_grab(
    db: &SqlitePool,
    hash: &str,
    torrent_name: &str,
    series_id: i64,
    episode_numbers: &[i32],
    is_batch: bool,
) -> Result<Option<i64>, sqlx::Error> {
    let eps_json = serde_json::to_string(episode_numbers).unwrap_or_else(|_| "[]".to_string());

    if !hash.is_empty() {
        let existing: Option<i64> = sqlx::query_scalar(
            "SELECT id FROM grabbed_torrents WHERE hash = ? AND state IN ('pending', 'imported') LIMIT 1",
        )
        .bind(hash)
        .fetch_optional(db)
        .await?;
        if existing.is_some() {
            return Ok(None);
        }
    }

    let is_batch_i = if is_batch { 1_i64 } else { 0_i64 };
    let result = sqlx::query(
        "INSERT INTO grabbed_torrents (hash, torrent_name, series_id, episode_numbers, state, is_batch) VALUES (?, ?, ?, ?, 'pending', ?)",
    )
    .bind(hash)
    .bind(torrent_name)
    .bind(series_id)
    .bind(&eps_json)
    .bind(is_batch_i)
    .execute(db)
    .await?;

    Ok(Some(result.last_insert_rowid()))
}

/// Per-file routing row for a grabbed torrent. Used to drive
/// post-processing for multi-series batch releases: when a Phase 2
/// grab detects sibling series in a megapack, one of these gets
/// written per sibling (plus one for the parent, covering unclaimed
/// files). Post-processing iterates the torrent's video files and
/// consults the routes to decide which series' media folder each file
/// belongs to.
///
/// `file_indices` are zero-based indices into the torrent's canonical
/// file list as returned by qBit's `torrents/files` endpoint — the
/// same ordering the detection function saw at grab time.
///
/// `episode_numbers` is pre-parsed at grab time so post-processing
/// doesn't have to re-derive episode numbers from filenames (and so
/// we can record them on the parent `grabbed_torrents` row for the
/// existing `find_imported_for_episode` lookup to keep working).
#[derive(Debug, Clone)]
pub struct GrabSeriesRoute {
    pub grab_id: i64,
    pub series_id: i64,
    pub file_indices: Vec<usize>,
    pub episode_numbers: Vec<i32>,
    pub matched_subtitle: String,
}

/// Write one or more `grabbed_torrent_series` rows for a freshly
/// recorded grab. Used by the Phase 2 auto-expand path in
/// `handlers::library` to persist per-sibling file routing. Single-
/// series grabs don't need to call this — post-processing falls
/// through to `grab.series_id` when no route rows exist.
pub async fn record_grab_series_routes(
    db: &SqlitePool,
    routes: &[GrabSeriesRoute],
) -> Result<(), sqlx::Error> {
    for route in routes {
        let file_idx_i64: Vec<i64> = route.file_indices.iter().map(|i| *i as i64).collect();
        let file_indices_json =
            serde_json::to_string(&file_idx_i64).unwrap_or_else(|_| "[]".to_string());
        let eps_json = serde_json::to_string(&route.episode_numbers)
            .unwrap_or_else(|_| "[]".to_string());
        sqlx::query(
            r#"INSERT OR REPLACE INTO grabbed_torrent_series
               (grab_id, series_id, file_indices, episode_numbers, matched_subtitle)
               VALUES (?, ?, ?, ?, ?)"#,
        )
        .bind(route.grab_id)
        .bind(route.series_id)
        .bind(&file_indices_json)
        .bind(&eps_json)
        .bind(&route.matched_subtitle)
        .execute(db)
        .await?;
    }
    Ok(())
}

/// Fetch every route row for a grab. Returns an empty vec for legacy
/// single-series grabs that predate Phase 2 — post-processing treats
/// an empty result as "route all files to grab.series_id" in that
/// case.
pub async fn get_series_routes(
    db: &SqlitePool,
    grab_id: i64,
) -> Result<Vec<GrabSeriesRoute>, sqlx::Error> {
    let rows = sqlx::query(
        r#"SELECT grab_id, series_id, file_indices, episode_numbers, matched_subtitle
           FROM grabbed_torrent_series
           WHERE grab_id = ?"#,
    )
    .bind(grab_id)
    .fetch_all(db)
    .await?;

    Ok(rows
        .iter()
        .map(|row| {
            let file_idx_json: String = row.get("file_indices");
            let file_idx: Vec<i64> =
                serde_json::from_str(&file_idx_json).unwrap_or_default();
            let eps_json: String = row.get("episode_numbers");
            let episode_numbers: Vec<i32> =
                serde_json::from_str(&eps_json).unwrap_or_default();
            GrabSeriesRoute {
                grab_id: row.get("grab_id"),
                series_id: row.get("series_id"),
                file_indices: file_idx.into_iter().map(|i| i as usize).collect(),
                episode_numbers,
                matched_subtitle: row.get("matched_subtitle"),
            }
        })
        .collect())
}

/// Look up the stored `is_batch` flag for a grab by its torrent name.
/// Returns `None` when the row doesn't exist — that's the case for
/// externally-imported library files that Ryokan never grabbed, which
/// have no batch signal one way or the other.
///
/// Used by post-download reclassification: the classifier has the
/// torrent name (via `grab.torrent_name`) but needs to know if the
/// original grab was a batch to feed Layer 4 correctly.
pub async fn get_is_batch_by_name(
    db: &SqlitePool,
    series_id: i64,
    torrent_name: &str,
) -> Option<bool> {
    sqlx::query_scalar::<_, i64>(
        "SELECT is_batch FROM grabbed_torrents WHERE series_id = ? AND torrent_name = ? ORDER BY grabbed_at DESC LIMIT 1",
    )
    .bind(series_id)
    .bind(torrent_name)
    .fetch_optional(db)
    .await
    .ok()
    .flatten()
    .map(|v| v != 0)
}

/// Get all grabs that have not yet been processed.
pub async fn get_all_pending(db: &SqlitePool) -> Result<Vec<GrabbedTorrent>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id, hash, torrent_name, series_id, episode_numbers, grabbed_at, COALESCE(is_batch, 0) AS is_batch FROM grabbed_torrents WHERE state = 'pending' ORDER BY grabbed_at ASC",
    )
    .fetch_all(db)
    .await?;

    Ok(rows
        .iter()
        .map(|row| {
            let eps_json: String = row.get("episode_numbers");
            let episode_numbers: Vec<i32> =
                serde_json::from_str(&eps_json).unwrap_or_default();
            let is_batch_i: i64 = row.get("is_batch");
            GrabbedTorrent {
                id: row.get("id"),
                hash: row.get("hash"),
                torrent_name: row.get("torrent_name"),
                series_id: row.get("series_id"),
                episode_numbers,
                state: "pending".to_string(),
                grabbed_at: row.get("grabbed_at"),
                is_batch: is_batch_i != 0,
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

pub async fn mark_removed(db: &SqlitePool, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE grabbed_torrents SET state = 'removed' WHERE id = ?")
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

/// Mark a grabbed torrent as failed (blocklisted) by matching torrent name and series.
pub async fn mark_failed_by_name(db: &SqlitePool, series_id: i64, torrent_name: &str) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE grabbed_torrents SET state = 'failed' WHERE series_id = ? AND torrent_name = ? AND state IN ('pending', 'imported')",
    )
    .bind(series_id)
    .bind(torrent_name)
    .execute(db)
    .await?;
    Ok(result.rows_affected())
}

/// Find previously imported grabs for a series that cover a given episode.
/// Used by post-processing to identify old torrents to clean up during upgrades.
///
/// Unions two paths so Phase 2 sibling-routed imports get found
/// correctly: (1) legacy path — grab rows where `series_id` is the
/// primary and the episode appears in `grabbed_torrents.episode_numbers`;
/// (2) routes path — grab rows where the series appears in
/// `grabbed_torrent_series` (as a sibling of a batch torrent) and the
/// episode appears in the route row's `episode_numbers`. Without the
/// second path, upgrades for a sibling series would never find the
/// batch import to clean up.
pub async fn find_imported_for_episode(
    db: &SqlitePool,
    series_id: i64,
    episode_number: i32,
) -> Result<Vec<GrabbedTorrent>, sqlx::Error> {
    // episode_numbers is stored as a JSON array, so we search with
    // json_each on both the legacy column and the routes column.
    // UNION dedups grabs where the same series matches through both
    // paths (parent of a single-series grab).
    let rows = sqlx::query(
        r#"SELECT id, hash, torrent_name, series_id, episode_numbers, grabbed_at FROM (
             SELECT g.id AS id, g.hash AS hash, g.torrent_name AS torrent_name,
                    g.series_id AS series_id, g.episode_numbers AS episode_numbers,
                    g.grabbed_at AS grabbed_at
             FROM grabbed_torrents g, json_each(g.episode_numbers) AS je
             WHERE g.series_id = ? AND je.value = ? AND g.state = 'imported'
             UNION
             SELECT g.id AS id, g.hash AS hash, g.torrent_name AS torrent_name,
                    g.series_id AS series_id, g.episode_numbers AS episode_numbers,
                    g.grabbed_at AS grabbed_at
             FROM grabbed_torrents g
             JOIN grabbed_torrent_series r ON r.grab_id = g.id
             , json_each(r.episode_numbers) AS je
             WHERE r.series_id = ? AND je.value = ? AND g.state = 'imported'
           )
           ORDER BY grabbed_at DESC"#,
    )
    .bind(series_id)
    .bind(episode_number)
    .bind(series_id)
    .bind(episode_number)
    .fetch_all(db)
    .await?;

    Ok(rows
        .iter()
        .map(|row| {
            let eps_json: String = row.get("episode_numbers");
            let episode_numbers: Vec<i32> = serde_json::from_str(&eps_json).unwrap_or_default();
            GrabbedTorrent {
                id: row.get("id"),
                hash: row.get("hash"),
                torrent_name: row.get("torrent_name"),
                series_id: row.get("series_id"),
                episode_numbers,
                state: "imported".to_string(),
                grabbed_at: row.get("grabbed_at"),
                is_batch: false,
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

/// Return every (id, hash) pair currently associated with `series_id`,
/// regardless of state. Used by the "remove series" handler so we can
/// stop seeding and tell qBittorrent to drop the data when the user
/// removes a series from the library — without this, qBit keeps holding
/// torrent state for a series Ryokan has already forgotten about.
pub async fn get_all_for_series(
    db: &SqlitePool,
    series_id: i64,
) -> Result<Vec<(i64, String)>, sqlx::Error> {
    let rows = sqlx::query("SELECT id, hash FROM grabbed_torrents WHERE series_id = ?")
        .bind(series_id)
        .fetch_all(db)
        .await?;

    Ok(rows
        .iter()
        .map(|row| (row.get::<i64, _>("id"), row.get::<String, _>("hash")))
        .collect())
}

/// Delete every grabbed_torrents row for a series in one query. Called
/// after the per-torrent qBit delete pass during series removal so the
/// table doesn't accumulate stale rows pointing at hashes qBit no longer
/// knows about.
pub async fn delete_all_for_series(db: &SqlitePool, series_id: i64) -> Result<u64, sqlx::Error> {
    let result = sqlx::query("DELETE FROM grabbed_torrents WHERE series_id = ?")
        .bind(series_id)
        .execute(db)
        .await?;
    Ok(result.rows_affected())
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
