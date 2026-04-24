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

/// Record a torrent grab for post-processing.
///
/// Three outcomes, all returning `Ok(Some(grab_id))` so callers can
/// attach `grabbed_torrent_series` route rows without re-querying:
///
///  1. **Fresh insert.** No prior active row for this hash — a new
///     grab row is inserted at state `pending`.
///  2. **Reactivation.** A prior row with the same non-empty hash
///     exists in state `pending` or `imported`. That row is flipped
///     back to `pending`, `imported_at` and `qbit_content_path` are
///     cleared, and `series_id` / `episode_numbers` / `torrent_name`
///     / `is_batch` are refreshed to the new request. Post-processing
///     will re-import the torrent as if it were fresh. This handles
///     the "I deleted the library file, re-grabbed the same release,
///     nothing happened" drift case — without it, the `INSERT OR
///     IGNORE` silently swallowed the second grab and the episode tag
///     would get stuck at `grabbed` forever.
///  3. **Empty-hash pass-through.** Hash is empty (legacy grab
///     paths). Partial index excludes empty-hash rows, so a fresh
///     insert always succeeds and no dedup/reactivation happens.
///
/// `Ok(None)` is no longer returned — any successful path now carries
/// the affected row's id, and a real DB error bubbles up as `Err`.
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

    // Step 1 — attempt a fresh insert. Partial UNIQUE index on (hash)
    // WHERE hash != '' AND state IN ('pending', 'imported') makes
    // INSERT OR IGNORE atomically dedup against an active row for the
    // same hash. Empty-hash rows bypass the index (see the outcome #3
    // comment on the fn) and always land.
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

    if let Some(id) = inserted_id {
        return Ok(Some(id));
    }

    // Empty-hash insert can't conflict (excluded by partial index), so
    // reaching here with an empty hash means something else went wrong
    // (e.g. a FK violation on series_id). Report None to surface the
    // anomaly instead of silently papering over it.
    if hash.is_empty() {
        return Ok(None);
    }

    // Step 2 — dedup hit. Reactivate the existing row ONLY when it's
    // already imported. `RETURNING id` gives us the existing grab's
    // primary key so callers get a consistent `Some(id)` on the drift
    // path.
    //
    // Why gate on `state='imported'` instead of `IN ('pending',
    // 'imported')`:
    //   A `pending` row means another concurrent flow — most likely
    //   post-processing mid-import — is actively working on the
    //   torrent. `stamp_qbit_content_path` runs BEFORE `import_torrent`,
    //   so at that moment the row is `pending` with a non-empty
    //   `qbit_content_path`. If we null-clobbered those columns here,
    //   the in-flight import would finish on a row that no longer
    //   knows where qBit left the file. Leaving pending rows alone
    //   (and returning Ok(None) when the insert is deduped against a
    //   pending row) matches the pre-reactivation "silent dedup"
    //   semantics for the narrow "already in progress" case and only
    //   diverges for the drift case (imported row the user wants to
    //   re-import).
    //
    // Refresh series_id / episode_numbers / torrent_name / is_batch
    // from the new request: a user who re-grabs typed a release
    // against a different episode set than the original (e.g. was a
    // batch, now a single episode) should see post-processing import
    // the new intent, not the stale one.
    let reactivated: Option<i64> = sqlx::query_scalar(
        "UPDATE grabbed_torrents
         SET state = 'pending',
             imported_at = NULL,
             client_content_path = '',
             grabbed_at = CURRENT_TIMESTAMP,
             series_id = ?,
             episode_numbers = ?,
             torrent_name = ?,
             is_batch = ?
         WHERE hash = ? AND state = 'imported'
         RETURNING id",
    )
    .bind(series_id)
    .bind(&eps_json)
    .bind(torrent_name)
    .bind(is_batch_i)
    .bind(hash)
    .fetch_optional(db)
    .await?;

    Ok(reactivated)
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
        let eps_json =
            serde_json::to_string(&route.episode_numbers).unwrap_or_else(|_| "[]".to_string());
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
            let file_idx: Vec<i64> = serde_json::from_str(&file_idx_json).unwrap_or_default();
            let eps_json: String = row.get("episode_numbers");
            let episode_numbers: Vec<i32> = serde_json::from_str(&eps_json).unwrap_or_default();
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
        let file_idx: Vec<i64> = serde_json::from_str(&file_idx_json).unwrap_or_default();
        let eps_json: String = row.get("episode_numbers");
        let episode_numbers: Vec<i32> = serde_json::from_str(&eps_json).unwrap_or_default();
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
            let episode_numbers: Vec<i32> = serde_json::from_str(&eps_json).unwrap_or_default();
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

/// Mark a grab as finalized without recording an actual import. Used by
/// `advance_state_without_import` when post-processing is disabled: the
/// torrent is complete on qBit's side and we want to stop polling it
/// (hence the `state = 'imported'` flip, which matches the unique-index
/// and pending-filter semantics elsewhere), but Ryokan never moved a
/// file, so `imported_at` stays NULL. Any future report or filter keyed
/// on `imported_at IS NOT NULL` will correctly see this grab as "not
/// imported by us."
pub async fn mark_completed_no_import(db: &SqlitePool, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE grabbed_torrents SET state = 'imported' WHERE id = ?")
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

/// Stamp the qBit-reported content_path (or save_path fallback) on the
/// grabbed_torrents row the first time we observe the torrent as
/// complete. Sonarr-parity dual-path tracking: the qBit-side path is
/// recorded here; the library-side path lives on
/// `episode_grab_history.file_name` after post-processing.
///
/// Idempotent: `WHERE COALESCE(client_content_path, '') = ''` so a
/// later completion tick on an already-stamped row is a no-op.
pub async fn stamp_client_content_path(
    db: &SqlitePool,
    id: i64,
    path: &str,
) -> Result<(), sqlx::Error> {
    if path.is_empty() {
        return Ok(());
    }
    sqlx::query(
        "UPDATE grabbed_torrents SET client_content_path = ?
         WHERE id = ? AND COALESCE(client_content_path, '') = ''",
    )
    .bind(path)
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

/// Mark every `pending` grab row as `failed`. Used by the #63 Phase 2
/// client-switch handler: when the user changes `active_client` in
/// Settings, any grab that was in-flight against the old client is
/// now orphaned (the new client has never seen that hash). Dropping
/// them from `pending` means they fall out of the partial UNIQUE
/// index on `(hash) WHERE state IN ('pending', 'imported')` and the
/// user can cleanly re-grab in the new client without a dedupe
/// collision. Returns the number of rows updated so the caller can
/// surface "N pending grabs cancelled" in the UI notice.
///
/// No reason string is stored — `grabbed_torrents` has no free-text
/// failure_reason column today. Callers log the reason separately
/// at `info` level if they want it on the trail.
pub async fn mark_all_pending_failed(db: &SqlitePool) -> Result<u64, sqlx::Error> {
    let result =
        sqlx::query("UPDATE grabbed_torrents SET state = 'failed' WHERE state = 'pending'")
            .execute(db)
            .await?;
    Ok(result.rows_affected())
}

pub async fn mark_removed(db: &SqlitePool, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE grabbed_torrents SET state = 'removed' WHERE id = ?")
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

/// Flip a previously-imported grab to the `replaced` state and stamp
/// `replaced_by_grab_id` with the id of the new grab that took its
/// place. Called by post-processing when a higher-scoring upgrade
/// lands on the same episode(s) as an existing import — distinct from
/// `mark_removed`, which is the user-cancel / cleanup path.
///
/// The history UI reads both columns: `replaced` rows show a "replaced
/// by <new release>" tooltip + link so users can see why an earlier
/// download disappeared, and the replacing grab's row surfaces a
/// "superseded N grabs" note derived from the reverse lookup.
pub async fn mark_replaced(
    db: &SqlitePool,
    id: i64,
    replaced_by_grab_id: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE grabbed_torrents SET state = 'replaced', replaced_by_grab_id = ? WHERE id = ?",
    )
    .bind(replaced_by_grab_id)
    .bind(id)
    .execute(db)
    .await?;
    Ok(())
}

/// Build the `series_title` SELECT expression honoring the user's
/// `title_language` preference. Mirrors the fallback order in
/// `services::nfo::title_for_preference` — `NULLIF(col, '')` is needed
/// because `series` columns are `NOT NULL DEFAULT ''` rather than
/// nullable, so a plain COALESCE would return the first empty string
/// instead of skipping to the next field.
///
/// Fallback order (must match nfo::title_for_preference):
/// - `romaji`  → english → native → title
/// - `native`  → english → romaji → title
/// - english / anything else → romaji → native → title
fn title_select_expr(preference: &str) -> &'static str {
    match preference {
        "romaji" => {
            "COALESCE(NULLIF(s.title_romaji, ''), NULLIF(s.title_english, ''), NULLIF(s.title_native, ''), s.title, '') AS series_title"
        }
        "native" => {
            "COALESCE(NULLIF(s.title_native, ''), NULLIF(s.title_english, ''), NULLIF(s.title_romaji, ''), s.title, '') AS series_title"
        }
        _ => {
            "COALESCE(NULLIF(s.title_english, ''), NULLIF(s.title_romaji, ''), NULLIF(s.title_native, ''), s.title, '') AS series_title"
        }
    }
}

/// Get all grabbed torrents with series title, ordered by most recent first.
pub async fn get_all_with_series(
    db: &SqlitePool,
    limit: i64,
    title_language: &str,
) -> Result<Vec<GrabbedTorrentWithSeries>, sqlx::Error> {
    let sql = format!(
        r#"SELECT g.id, g.hash, g.torrent_name, g.series_id, g.episode_numbers, g.state, g.grabbed_at, g.imported_at,
                  {title_expr},
                  COALESCE(s.anilist_id, 0) AS anilist_id,
                  g.replaced_by_grab_id,
                  COALESCE(rby.torrent_name, '') AS replaced_by_torrent_name,
                  (SELECT COUNT(*) FROM grabbed_torrents rp WHERE rp.replaced_by_grab_id = g.id) AS replaces_count
           FROM grabbed_torrents g
           LEFT JOIN series s ON s.id = g.series_id
           LEFT JOIN grabbed_torrents rby ON rby.id = g.replaced_by_grab_id
           ORDER BY g.grabbed_at DESC
           LIMIT ?"#,
        title_expr = title_select_expr(title_language),
    );
    let rows = sqlx::query(&sql).bind(limit).fetch_all(db).await?;

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
                replaced_by_grab_id: row.get("replaced_by_grab_id"),
                replaced_by_torrent_name: row.get("replaced_by_torrent_name"),
                replaces_count: row.get("replaces_count"),
            }
        })
        .collect())
}

/// Get all failed/blocked torrents.
pub async fn get_blocked(
    db: &SqlitePool,
    title_language: &str,
) -> Result<Vec<GrabbedTorrentWithSeries>, sqlx::Error> {
    let sql = format!(
        r#"SELECT g.id, g.hash, g.torrent_name, g.series_id, g.episode_numbers, g.state, g.grabbed_at, g.imported_at,
                  {title_expr},
                  COALESCE(s.anilist_id, 0) AS anilist_id,
                  g.replaced_by_grab_id,
                  '' AS replaced_by_torrent_name,
                  0 AS replaces_count
           FROM grabbed_torrents g
           LEFT JOIN series s ON s.id = g.series_id
           WHERE g.state = 'failed'
           ORDER BY g.grabbed_at DESC"#,
        title_expr = title_select_expr(title_language),
    );
    let rows = sqlx::query(&sql).fetch_all(db).await?;

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
                replaced_by_grab_id: row.get("replaced_by_grab_id"),
                replaced_by_torrent_name: row.get("replaced_by_torrent_name"),
                replaces_count: row.get("replaces_count"),
            }
        })
        .collect())
}

/// Is this infohash currently blocklisted? True when at least one
/// `grabbed_torrents` row exists for the hash with `state = 'failed'`.
/// Checked by the interactive file-picker preview endpoint so the
/// modal can render the inline-unblock warning (plan decision #12).
pub async fn is_blocklisted(db: &SqlitePool, hash: &str) -> Result<bool, sqlx::Error> {
    if hash.is_empty() {
        return Ok(false);
    }
    let existing: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM grabbed_torrents WHERE hash = ? AND state = 'failed' LIMIT 1",
    )
    .bind(hash)
    .fetch_optional(db)
    .await?;
    Ok(existing.is_some())
}

/// Flip every `state='failed'` row for this hash to `state='replaced'`
/// with a back-pointer to the new grab id. Called by the inline-unblock
/// path in `handlers::grab::grab_confirm` after `record_grab` writes
/// the fresh pending row.
///
/// Using `replaced` (rather than `removed`) preserves the hash→id
/// audit trail: the Downloads page's blocklist view filters on
/// `state='failed'`, and the new pending row's provenance is still
/// walkable through `replaced_by_grab_id`.
pub async fn unblock_by_hash(
    db: &SqlitePool,
    hash: &str,
    replaced_by: i64,
) -> Result<u64, sqlx::Error> {
    if hash.is_empty() {
        return Ok(0);
    }
    let result = sqlx::query(
        "UPDATE grabbed_torrents \
         SET state = 'replaced', replaced_by_grab_id = ? \
         WHERE hash = ? AND state = 'failed'",
    )
    .bind(replaced_by)
    .bind(hash)
    .execute(db)
    .await?;
    Ok(result.rows_affected())
}

/// Mark a grabbed torrent as failed (blocklisted) by matching torrent name and series.
pub async fn mark_failed_by_name(
    db: &SqlitePool,
    series_id: i64,
    torrent_name: &str,
) -> Result<u64, sqlx::Error> {
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

/// Same shape as [`find_imported_for_episode`] but for pending rows —
/// the torrent is in qBit but post-processing hasn't imported it yet.
/// Used by the cancel-pending handler to find what to pull out of qBit
/// before marking the row 'removed'.
///
/// `grabbed_torrents.state = 'pending'` is the on-the-wire label for
/// this stage (distinct from `episode_tags.state = 'grabbed'`, which
/// describes the episode's UI state and uses a different vocabulary
/// — yes, it's confusing). Returns both direct single-series grabs
/// and routed multi-series grabs (parent batch whose route targets
/// this series+episode), same UNION shape as the imported variant.
pub async fn find_pending_for_episode(
    db: &SqlitePool,
    series_id: i64,
    episode_number: i32,
) -> Result<Vec<GrabbedTorrent>, sqlx::Error> {
    let rows = sqlx::query(
        r#"SELECT id, hash, torrent_name, series_id, episode_numbers, grabbed_at, is_batch FROM (
             SELECT g.id AS id, g.hash AS hash, g.torrent_name AS torrent_name,
                    g.series_id AS series_id, g.episode_numbers AS episode_numbers,
                    g.grabbed_at AS grabbed_at,
                    COALESCE(g.is_batch, 0) AS is_batch
             FROM grabbed_torrents g, json_each(g.episode_numbers) AS je
             WHERE g.series_id = ? AND je.value = ? AND g.state = 'pending'
             UNION
             SELECT g.id AS id, g.hash AS hash, g.torrent_name AS torrent_name,
                    g.series_id AS series_id, g.episode_numbers AS episode_numbers,
                    g.grabbed_at AS grabbed_at,
                    COALESCE(g.is_batch, 0) AS is_batch
             FROM grabbed_torrents g
             JOIN grabbed_torrent_series r ON r.grab_id = g.id
             , json_each(r.episode_numbers) AS je
             WHERE r.series_id = ? AND je.value = ? AND g.state = 'pending'
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
                state: "pending".to_string(),
                grabbed_at: row.get("grabbed_at"),
                is_batch: is_batch_i != 0,
            }
        })
        .collect())
}

/// Bulk variant for post-processing's library scan: fetch every
/// imported grab covering this series in one round-trip, including
/// grabs that reach the series via the sibling-routes path. Returns
/// `(torrent_name, episode_numbers)` for each, sorted most-recent
/// first by `grabbed_at`.
///
/// scan_library_for_unclassified used to do *two* per-file queries
/// (`find_imported_for_episode` + a fallback `most_recent_…`) per
/// disk file inside a held POST_PROC_LOCK. For a 100-series, 24-ep
/// library that's ~4800 sequential round-trips per pass. With this
/// helper the caller pre-builds an in-memory map per series and the
/// per-file path is lock-free dictionary lookups.
///
/// `UNION ALL` (not `UNION`) because dedup falls naturally out of the
/// caller's `entry().or_insert_with()` first-write-wins semantics; we
/// don't pay for SQLite's UNION-side sort/hash.
pub async fn imported_grabs_for_series(
    db: &SqlitePool,
    series_id: i64,
) -> Result<Vec<(String, Vec<i32>)>, sqlx::Error> {
    let rows = sqlx::query(
        r#"SELECT torrent_name, episode_numbers, grabbed_at FROM (
             SELECT g.torrent_name AS torrent_name,
                    g.episode_numbers AS episode_numbers,
                    g.grabbed_at AS grabbed_at
             FROM grabbed_torrents g
             WHERE g.series_id = ? AND g.state = 'imported'
             UNION ALL
             SELECT g.torrent_name AS torrent_name,
                    r.episode_numbers AS episode_numbers,
                    g.grabbed_at AS grabbed_at
             FROM grabbed_torrents g
             JOIN grabbed_torrent_series r ON r.grab_id = g.id
             WHERE r.series_id = ? AND g.state = 'imported'
           )
           ORDER BY grabbed_at DESC"#,
    )
    .bind(series_id)
    .bind(series_id)
    .fetch_all(db)
    .await?;

    Ok(rows
        .iter()
        .map(|row| {
            let torrent_name: String = row.get("torrent_name");
            let eps_json: String = row.get("episode_numbers");
            let episode_numbers: Vec<i32> = serde_json::from_str(&eps_json).unwrap_or_default();
            (torrent_name, episode_numbers)
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
    /// When `state = 'replaced'`, the id of the grab that superseded
    /// this one (upgrade-driven replacement from post-processing).
    /// `None` for any other state and for replaced rows written before
    /// the column was introduced.
    pub replaced_by_grab_id: Option<i64>,
    /// Title of the grab referenced by `replaced_by_grab_id`, resolved
    /// via a LEFT JOIN at query time so the UI can render a "replaced
    /// by <release>" tooltip without a second round-trip. Empty when
    /// the pointer is NULL or dangles.
    pub replaced_by_torrent_name: String,
    /// Count of rows that carry `replaced_by_grab_id = this.id` — i.e.
    /// how many prior grabs this one superseded. Drives the
    /// "superseded N grabs" note on the replacing row. Zero for the
    /// common case.
    pub replaces_count: i64,
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

    /// record_grab's atomic dedup uses a partial UNIQUE index on
    /// `(hash) WHERE hash != '' AND state IN ('pending', 'imported')`.
    /// A dedup hit is no longer a silent no-op — the existing row is
    /// reactivated so post-processing picks it up again (see the
    /// drift-cause story on the `record_grab` fn). This test pins both
    /// the dedup-and-reactivate behavior and the empty-hash bypass.
    #[tokio::test]
    async fn record_grab_dedups_and_reactivates_same_hash() {
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

        // Second grab with same hash against a PENDING row dedups
        // silently — reactivation only runs on 'imported' rows to
        // avoid null-clobbering an in-flight import's
        // qbit_content_path / imported_at. Returns Ok(None) and the
        // existing row's fields are left alone.
        let id2 = record_grab(&db, "racehash", "release b", series_id, &[2], false)
            .await
            .expect("second record_grab");
        assert!(
            id2.is_none(),
            "pending-row dedup must not reactivate: {:?}",
            id2
        );

        // Confirm only one row exists and the pending fields are
        // intact (no silent rewrite).
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM grabbed_torrents WHERE hash = 'racehash'")
                .fetch_one(&db)
                .await
                .expect("count");
        assert_eq!(count, 1);

        let row: (String, String, String) = sqlx::query_as(
            "SELECT torrent_name, episode_numbers, state FROM grabbed_torrents WHERE id = ?",
        )
        .bind(id1)
        .fetch_one(&db)
        .await
        .expect("fetch row");
        assert_eq!(row.0, "release a", "pending row's fields must be untouched");
        assert_eq!(
            row.1, "[1]",
            "pending row's episode_numbers must be untouched"
        );
        assert_eq!(row.2, "pending", "pending row stays pending");

        // The original drift case: mark the row 'imported' (as
        // post-processing would have), then re-grab the same hash.
        // Reactivation must flip it back to 'pending' and null out
        // imported_at so the next post-processing tick picks it up.
        mark_imported(&db, id1).await.expect("mark imported");
        let imported_at_before: Option<String> =
            sqlx::query_scalar("SELECT imported_at FROM grabbed_torrents WHERE id = ?")
                .bind(id1)
                .fetch_one(&db)
                .await
                .expect("imported_at before");
        assert!(
            imported_at_before.is_some(),
            "mark_imported stamps imported_at"
        );

        let id3 = record_grab(&db, "racehash", "release c", series_id, &[1], false)
            .await
            .expect("third record_grab")
            .expect("re-grab of imported hash must yield an id");
        assert_eq!(id3, id1, "reactivation preserves the row id");

        let (state_after, imported_at_after): (String, Option<String>) =
            sqlx::query_as("SELECT state, imported_at FROM grabbed_torrents WHERE id = ?")
                .bind(id1)
                .fetch_one(&db)
                .await
                .expect("state after");
        assert_eq!(state_after, "pending", "imported→pending flip on re-grab");
        assert!(
            imported_at_after.is_none(),
            "imported_at must be cleared on reactivation"
        );

        // Failed grabs with the same hash are NOT covered by the
        // partial index, so a re-grab goes through the fresh-insert
        // path and writes a new row. This preserves blocklist
        // semantics (user marked the grab failed on purpose — the
        // re-grab is a genuinely new attempt, not a reactivation).
        mark_failed(&db, id1).await.expect("mark failed");
        let id4 = record_grab(&db, "racehash", "release d", series_id, &[1], false)
            .await
            .expect("fourth record_grab")
            .expect("re-record after failed must insert");
        assert_ne!(id4, id1, "post-failed re-grab inserts a new row");

        // Empty-hash rows aren't covered by the partial index and are
        // never deduped.
        let id5 = record_grab(&db, "", "no hash a", series_id, &[1], false)
            .await
            .expect("empty-hash a");
        let id6 = record_grab(&db, "", "no hash b", series_id, &[1], false)
            .await
            .expect("empty-hash b");
        assert!(id5.is_some() && id6.is_some(), "empty-hash never dedups");
        assert_ne!(id5, id6, "empty-hash inserts are distinct rows");
    }

    /// `find_pending_for_episode` backs the cancel-pending handler.
    /// It must only return 'pending' rows (not 'imported' / 'failed' /
    /// 'removed'), and must find both direct single-series grabs and
    /// grabs that reach the series via a route row.
    #[tokio::test]
    async fn find_pending_filters_by_state_and_series() {
        let db = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite");
        crate::models::migrate(&db).await.expect("migrate");

        let (series_id, _) = series::upsert(
            &db,
            series::SeriesCore {
                anilist_id: 99999,
                mal_id: None,
                title: "Show",
                title_romaji: "Show",
                title_english: "Show",
                title_native: "",
                cover_url: "",
                format: "TV",
                status: "FINISHED",
                episodes: Some(12),
                season_year: Some(2024),
                end_year: Some(2024),
            },
        )
        .await
        .expect("series upsert");

        // Pending grab for ep 5 — should be found.
        let pending_id = record_grab(
            &db,
            "pending0000000000000000000000000000000001",
            "[Group] Show - 05",
            series_id,
            &[5],
            false,
        )
        .await
        .expect("pending grab")
        .expect("id");

        // Imported grab for ep 6 — must NOT be returned for ep 6 pending
        // lookup (different state).
        let imported_id = record_grab(
            &db,
            "imported000000000000000000000000000000002",
            "[Group] Show - 06",
            series_id,
            &[6],
            false,
        )
        .await
        .expect("imported grab")
        .expect("id");
        mark_imported(&db, imported_id)
            .await
            .expect("mark imported");

        let hits_ep5 = find_pending_for_episode(&db, series_id, 5)
            .await
            .expect("query");
        assert_eq!(
            hits_ep5.len(),
            1,
            "should find the one pending grab for ep 5"
        );
        assert_eq!(hits_ep5[0].id, pending_id);
        assert_eq!(hits_ep5[0].state, "pending");

        let hits_ep6 = find_pending_for_episode(&db, series_id, 6)
            .await
            .expect("query");
        assert!(
            hits_ep6.is_empty(),
            "imported grabs must not leak into pending lookup"
        );

        // Cancel path: mark_removed flips the state; a second lookup
        // should no longer return the row.
        mark_removed(&db, pending_id).await.expect("mark removed");
        let hits_after_remove = find_pending_for_episode(&db, series_id, 5)
            .await
            .expect("query");
        assert!(
            hits_after_remove.is_empty(),
            "removed grabs must not reappear in pending lookup"
        );
    }

    #[tokio::test]
    async fn mark_replaced_flips_state_and_stamps_back_pointer() {
        use sqlx::Row;
        let db = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("memory db");
        crate::models::migrate(&db).await.expect("migrate");

        let (series_id, _) = series::upsert(
            &db,
            series::SeriesCore {
                anilist_id: 424242,
                mal_id: None,
                title: "Show",
                title_romaji: "Show",
                title_english: "Show",
                title_native: "",
                cover_url: "",
                format: "TV",
                status: "FINISHED",
                episodes: Some(12),
                season_year: Some(2024),
                end_year: Some(2024),
            },
        )
        .await
        .expect("series upsert");

        let old_id = record_grab(
            &db,
            "old00000000000000000000000000000000000001",
            "[OldGroup] Show - 01",
            series_id,
            &[1],
            false,
        )
        .await
        .expect("old")
        .expect("id");
        mark_imported(&db, old_id).await.expect("mark imported");

        let new_id = record_grab(
            &db,
            "new00000000000000000000000000000000000001",
            "[BetterGroup] Show - Batch [BD]",
            series_id,
            &[1, 2, 3],
            true,
        )
        .await
        .expect("new")
        .expect("id");

        mark_replaced(&db, old_id, new_id)
            .await
            .expect("mark replaced");

        let row =
            sqlx::query("SELECT state, replaced_by_grab_id FROM grabbed_torrents WHERE id = ?")
                .bind(old_id)
                .fetch_one(&db)
                .await
                .expect("lookup");
        let state: String = row.get("state");
        let replaced_by: Option<i64> = row.get("replaced_by_grab_id");
        assert_eq!(state, "replaced");
        assert_eq!(replaced_by, Some(new_id));

        // The replacing grab's row surfaces via replaces_count in the
        // with_series query — verify end-to-end.
        let history = get_all_with_series(&db, 10, "english")
            .await
            .expect("history");
        let new_row = history
            .iter()
            .find(|r| r.id == new_id)
            .expect("new grab present");
        assert_eq!(new_row.replaces_count, 1);
        let old_row = history
            .iter()
            .find(|r| r.id == old_id)
            .expect("old grab present");
        assert_eq!(old_row.state, "replaced");
        assert_eq!(old_row.replaced_by_grab_id, Some(new_id));
        assert_eq!(
            old_row.replaced_by_torrent_name,
            "[BetterGroup] Show - Batch [BD]"
        );
    }
}
