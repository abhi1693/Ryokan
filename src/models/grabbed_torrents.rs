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
    let is_batch_i = if is_batch { 1_i64 } else { 0_i64 };

    // Atomic dedup via partial UNIQUE index on (hash) WHERE hash != ''
    // AND state IN ('pending', 'imported') (created in models::migrate).
    // INSERT OR IGNORE swallows the conflict and RETURNING yields no
    // row, so fetch_optional resolves to None on the dedup path. The
    // previous SELECT-then-INSERT pattern had a race window where two
    // concurrent record_grab calls (RSS auto-sync racing a manual grab
    // on the same release) both observed "no existing row" and both
    // inserted, producing duplicate pending rows that triggered
    // double-import attempts in post-processing.
    //
    // Empty-hash rows aren't covered by the partial index (the WHERE
    // clause filters them out), so they always insert — preserving the
    // pre-fix behavior where empty-hash grabs from legacy paths never
    // deduped against each other.
    let inserted_id: Option<i64> = sqlx::query_scalar(
        "INSERT OR IGNORE INTO grabbed_torrents
             (hash, torrent_name, series_id, episode_numbers, state, is_batch)
         VALUES (?, ?, ?, ?, 'pending', ?)
         RETURNING id",
    )
    .bind(hash)
    .bind(torrent_name)
    .bind(series_id)
    .bind(&eps_json)
    .bind(is_batch_i)
    .fetch_optional(db)
    .await?;
    Ok(inserted_id)
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
    /// Amount to subtract from each file's parsed episode number at
    /// rename/tag time. Non-zero for siblings whose files use
    /// numbering that's continuous across the parent (e.g. an E14
    /// file in a 20-ep Owarimonogatari batch is actually Owari S2's
    /// E01 and needs `episode_offset = 13`). Zero for siblings with
    /// arc-local numbering.
    pub episode_offset: i32,
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
               (grab_id, series_id, file_indices, episode_numbers, matched_subtitle, episode_offset)
               VALUES (?, ?, ?, ?, ?, ?)"#,
        )
        .bind(route.grab_id)
        .bind(route.series_id)
        .bind(&file_indices_json)
        .bind(&eps_json)
        .bind(&route.matched_subtitle)
        .bind(route.episode_offset)
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
    // COALESCE on episode_offset keeps legacy rows (written before
    // the ALTER TABLE migration) readable as offset=0.
    let rows = sqlx::query(
        r#"SELECT grab_id, series_id, file_indices, episode_numbers, matched_subtitle,
                  COALESCE(episode_offset, 0) AS episode_offset
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
                episode_offset: row.get("episode_offset"),
            }
        })
        .collect())
}

/// Bulk variant of [`get_series_routes`] — fetches routes for many
/// grabs in one round-trip and groups by `grab_id`. The download-
/// progress poller calls this once per poll instead of fanning out
/// N queries for N pending grabs; the poller runs every few seconds
/// on every open series page, so the difference matters.
///
/// Grabs with no routes are simply absent from the result map; callers
/// should treat a missing entry as an empty route list.
pub async fn get_series_routes_for_grabs(
    db: &SqlitePool,
    grab_ids: &[i64],
) -> Result<std::collections::HashMap<i64, Vec<GrabSeriesRoute>>, sqlx::Error> {
    if grab_ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }

    // sqlx doesn't bind `IN (?)` against a slice directly, so build
    // the placeholder list at runtime. `grab_ids` comes from a
    // `SELECT id FROM grabbed_torrents` loop so every value is a
    // trusted i64 — no injection surface.
    let placeholders = vec!["?"; grab_ids.len()].join(", ");
    let sql = format!(
        r#"SELECT grab_id, series_id, file_indices, episode_numbers, matched_subtitle,
                  COALESCE(episode_offset, 0) AS episode_offset
           FROM grabbed_torrent_series
           WHERE grab_id IN ({})"#,
        placeholders
    );
    let mut q = sqlx::query(&sql);
    for id in grab_ids {
        q = q.bind(*id);
    }
    let rows = q.fetch_all(db).await?;

    let mut grouped: std::collections::HashMap<i64, Vec<GrabSeriesRoute>> =
        std::collections::HashMap::new();
    for row in rows {
        let file_idx_json: String = row.get("file_indices");
        let file_idx: Vec<i64> =
            serde_json::from_str(&file_idx_json).unwrap_or_default();
        let eps_json: String = row.get("episode_numbers");
        let episode_numbers: Vec<i32> =
            serde_json::from_str(&eps_json).unwrap_or_default();
        let route = GrabSeriesRoute {
            grab_id: row.get("grab_id"),
            series_id: row.get("series_id"),
            file_indices: file_idx.into_iter().map(|i| i as usize).collect(),
            episode_numbers,
            matched_subtitle: row.get("matched_subtitle"),
            episode_offset: row.get("episode_offset"),
        };
        grouped.entry(route.grab_id).or_default().push(route);
    }
    Ok(grouped)
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
        r#"SELECT id, hash, torrent_name, series_id, episode_numbers, grabbed_at, is_batch FROM (
             SELECT g.id AS id, g.hash AS hash, g.torrent_name AS torrent_name,
                    g.series_id AS series_id, g.episode_numbers AS episode_numbers,
                    g.grabbed_at AS grabbed_at,
                    COALESCE(g.is_batch, 0) AS is_batch
             FROM grabbed_torrents g, json_each(g.episode_numbers) AS je
             WHERE g.series_id = ? AND je.value = ? AND g.state = 'imported'
             UNION
             SELECT g.id AS id, g.hash AS hash, g.torrent_name AS torrent_name,
                    g.series_id AS series_id, g.episode_numbers AS episode_numbers,
                    g.grabbed_at AS grabbed_at,
                    COALESCE(g.is_batch, 0) AS is_batch
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
            let is_batch_i: i64 = row.get("is_batch");
            GrabbedTorrent {
                id: row.get("id"),
                hash: row.get("hash"),
                torrent_name: row.get("torrent_name"),
                series_id: row.get("series_id"),
                episode_numbers,
                state: "imported".to_string(),
                grabbed_at: row.get("grabbed_at"),
                is_batch: is_batch_i != 0,
            }
        })
        .collect())
}

/// Return the torrent name of the most recent imported grab for this
/// series, regardless of which episodes the grab's `episode_numbers`
/// column claims it covers. Used by
/// `scan_library_for_unclassified` as a fallback to
/// [`find_imported_for_episode`] when the per-episode lookup misses:
/// pre-fix batch grabs were recorded with `episode_numbers = []`, so
/// `json_each` yields nothing and the precise lookup returns empty
/// even though there's a perfectly good batch grab with a real
/// release name sitting in the table. For those stale rows we want
/// to classify against that release name instead of the sanitized
/// on-disk filename.
///
/// Returns `None` when the series has never had an imported grab,
/// in which case the scanner falls back to the sanitized filename
/// (correct behavior for externally-imported files Ryokan never
/// grabbed).
pub async fn most_recent_imported_torrent_name_for_series(
    db: &SqlitePool,
    series_id: i64,
) -> Option<String> {
    sqlx::query_scalar::<_, String>(
        "SELECT torrent_name FROM grabbed_torrents
         WHERE series_id = ? AND state = 'imported'
         ORDER BY grabbed_at DESC LIMIT 1",
    )
    .bind(series_id)
    .fetch_optional(db)
    .await
    .ok()
    .flatten()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::series;

    /// Round-trip a GrabSeriesRoute through record_grab_series_routes
    /// + get_series_routes to verify the new episode_offset column is
    /// written and read correctly. Covers Commit 3's schema plumbing
    /// (ALTER TABLE ADD COLUMN + INSERT bind + SELECT with COALESCE).
    #[tokio::test]
    async fn grab_series_route_round_trip_preserves_episode_offset() {
        let db = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite");
        crate::models::migrate(&db).await.expect("migrate");

        let (parent_id, _) = series::upsert(
            &db,
            series::SeriesCore {
                anilist_id: 21320,
                mal_id: None,
                title: "Owarimonogatari",
                title_romaji: "Owarimonogatari",
                title_english: "Owarimonogatari",
                title_native: "",
                cover_url: "",
                format: "TV",
                status: "FINISHED",
                episodes: Some(13),
                season_year: Some(2015),
                end_year: Some(2015),
            },
        )
        .await
        .expect("parent upsert");

        let (sibling_id, _) = series::upsert(
            &db,
            series::SeriesCore {
                anilist_id: 21860,
                mal_id: None,
                title: "Owarimonogatari Second Season",
                title_romaji: "Owarimonogatari Second Season",
                title_english: "Owarimonogatari Second Season",
                title_native: "",
                cover_url: "",
                format: "TV",
                status: "FINISHED",
                episodes: Some(7),
                season_year: Some(2017),
                end_year: Some(2017),
            },
        )
        .await
        .expect("sibling upsert");

        let grab_id = record_grab(
            &db,
            "roundtriphash0000000000000000000000000000",
            "[smol] Monogatari S07 (Owarimonogatari) [BD 1080p]",
            parent_id,
            &[],
            true,
        )
        .await
        .expect("record_grab")
        .expect("grab inserted");

        let routes = vec![
            // Parent route: no offset.
            GrabSeriesRoute {
                grab_id,
                series_id: parent_id,
                file_indices: vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12],
                episode_numbers: (1..=13).collect(),
                matched_subtitle: String::new(),
                episode_offset: 0,
            },
            // Sibling route: absolute-numbered, offset = parent cap.
            GrabSeriesRoute {
                grab_id,
                series_id: sibling_id,
                file_indices: vec![13, 14, 15, 16, 17, 18, 19],
                episode_numbers: (14..=20).collect(),
                matched_subtitle: "episode-range fallback (14..=20)".to_string(),
                episode_offset: 13,
            },
        ];

        record_grab_series_routes(&db, &routes)
            .await
            .expect("record routes");

        let read_back = get_series_routes(&db, grab_id)
            .await
            .expect("get_series_routes");
        assert_eq!(read_back.len(), 2);

        let parent_route = read_back
            .iter()
            .find(|r| r.series_id == parent_id)
            .expect("parent route");
        assert_eq!(parent_route.episode_offset, 0);

        let sibling_route = read_back
            .iter()
            .find(|r| r.series_id == sibling_id)
            .expect("sibling route");
        assert_eq!(sibling_route.episode_offset, 13);
        assert_eq!(
            sibling_route.matched_subtitle,
            "episode-range fallback (14..=20)"
        );
        assert_eq!(sibling_route.file_indices.len(), 7);
    }

    /// Regression: find_imported_for_episode previously hard-coded
    /// `is_batch: false`, so callers (handlers/library.rs and
    /// post_processing) treated batch torrents as single-episode grabs and
    /// `delete_torrent(..., delete_files=true)` would wipe the entire pack
    /// off disk during an upgrade-replace.
    #[tokio::test]
    async fn find_imported_for_episode_preserves_is_batch() {
        let db = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite");
        crate::models::migrate(&db).await.expect("migrate");

        let (series_id, _) = series::upsert(
            &db,
            series::SeriesCore {
                anilist_id: 21202,
                mal_id: None,
                title: "Show",
                title_romaji: "Show",
                title_english: "Show",
                title_native: "",
                cover_url: "",
                format: "TV",
                status: "FINISHED",
                episodes: Some(24),
                season_year: Some(2015),
                end_year: Some(2015),
            },
        )
        .await
        .expect("series upsert");

        let batch_eps: Vec<i32> = (1..=24).collect();
        let batch_grab_id = record_grab(
            &db,
            "batchhash00000000000000000000000000000000",
            "[Group] Show 01-24 [BD 1080p]",
            series_id,
            &batch_eps,
            true,
        )
        .await
        .expect("record batch grab")
        .expect("batch grab inserted");
        mark_imported(&db, batch_grab_id)
            .await
            .expect("mark batch imported");

        let single_grab_id = record_grab(
            &db,
            "singlehash0000000000000000000000000000000",
            "[Group] Show - 07 [WEB-DL 1080p]",
            series_id,
            &[7],
            false,
        )
        .await
        .expect("record single grab")
        .expect("single grab inserted");
        mark_imported(&db, single_grab_id)
            .await
            .expect("mark single imported");

        // Episode 5 is only covered by the batch grab — its is_batch must
        // round-trip as true.
        let ep5 = find_imported_for_episode(&db, series_id, 5)
            .await
            .expect("find ep5");
        assert_eq!(ep5.len(), 1, "expected one grab covering episode 5");
        assert!(
            ep5[0].is_batch,
            "batch grab for episode 5 must report is_batch=true"
        );

        // Episode 7 is covered by both grabs. The single-episode grab was
        // recorded second so it sorts first (ORDER BY grabbed_at DESC), but
        // both rows must report their true is_batch value.
        let ep7 = find_imported_for_episode(&db, series_id, 7)
            .await
            .expect("find ep7");
        assert_eq!(ep7.len(), 2, "expected both grabs covering episode 7");
        let single = ep7
            .iter()
            .find(|g| g.id == single_grab_id)
            .expect("single grab present");
        assert!(!single.is_batch, "single-episode grab is_batch=false");
        let batch = ep7
            .iter()
            .find(|g| g.id == batch_grab_id)
            .expect("batch grab present");
        assert!(batch.is_batch, "batch grab is_batch=true");
    }

    /// Regression: record_grab previously did SELECT-then-INSERT with no
    /// transaction, so two concurrent calls (RSS auto-sync racing a
    /// manual grab on the same hash) both observed "no existing row" and
    /// both inserted. Post-processing then attempted to import the same
    /// hash twice. The fix is a partial UNIQUE index + INSERT OR IGNORE,
    /// which collapses the SELECT and INSERT into one atomic statement.
    ///
    /// This test covers the three cases the dedup window has to honour:
    ///  - same hash, both active → second insert is rejected
    ///  - failed grab with same hash → re-record is allowed
    ///  - empty hash → never deduped, always inserts
    #[tokio::test]
    async fn record_grab_dedups_same_hash_atomically() {
        let db = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite");
        crate::models::migrate(&db).await.expect("migrate");

        let (series_id, _) = series::upsert(
            &db,
            series::SeriesCore {
                anilist_id: 12345,
                mal_id: None,
                title: "Show",
                title_romaji: "Show",
                title_english: "Show",
                title_native: "",
                cover_url: "",
                format: "TV",
                status: "FINISHED",
                episodes: Some(12),
                season_year: Some(2020),
                end_year: Some(2020),
            },
        )
        .await
        .expect("series upsert");

        // First active grab inserts.
        let id1 = record_grab(&db, "racehash", "release a", series_id, &[1], false)
            .await
            .expect("first record_grab")
            .expect("first must insert");

        // Second active grab with same hash is dedup'd.
        let id2 = record_grab(&db, "racehash", "release b", series_id, &[1], false)
            .await
            .expect("second record_grab");
        assert!(id2.is_none(), "duplicate active hash must dedup");

        // Confirm only one row exists for that hash.
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM grabbed_torrents WHERE hash = 'racehash'")
                .fetch_one(&db)
                .await
                .expect("count");
        assert_eq!(count, 1);

        // Mark the first one as failed; now the same hash can be
        // re-recorded — the partial index excludes failed/removed rows.
        mark_failed(&db, id1).await.expect("mark failed");
        let id3 = record_grab(&db, "racehash", "release c", series_id, &[1], false)
            .await
            .expect("third record_grab");
        assert!(
            id3.is_some(),
            "after blocklist, same hash must be re-recordable"
        );

        // Empty-hash rows aren't covered by the partial index.
        let id4 = record_grab(&db, "", "no hash a", series_id, &[1], false)
            .await
            .expect("empty-hash a");
        let id5 = record_grab(&db, "", "no hash b", series_id, &[1], false)
            .await
            .expect("empty-hash b");
        assert!(id4.is_some() && id5.is_some(), "empty-hash never dedups");
    }
}
