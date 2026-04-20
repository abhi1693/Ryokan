pub mod config;
pub mod log;
pub mod series;
pub mod rss;
pub mod monitoring;
pub mod session;
pub mod user;
pub mod metadata_cache;
pub mod local_metadata;
pub mod scheduled_tasks;
pub mod artwork_cache;
pub mod grabbed_torrents;
pub mod episode_tags;
pub mod group_source_map;
pub mod nyaa_description_cache;
pub mod media_probe_cache;
pub mod custom_formats;

use sqlx::{Row, SqlitePool};

/// Check whether `column` exists on `table`. Used inside [`migrate`] to
/// gate idempotent ALTER chains whose "ADD COLUMN then RENAME" shape
/// would otherwise leave vestigial columns on fresh installs (the ADD
/// succeeds unconditionally because `.ok()` swallows the
/// already-migrated case, then the RENAME silently no-ops when the
/// target already exists). By asking SQLite directly, we can skip the
/// ADD step entirely on installs where the current column name already
/// exists.
async fn column_exists(db: &SqlitePool, table: &str, column: &str) -> bool {
    // PRAGMA doesn't accept bound parameters, but `table` is a hardcoded
    // string literal from our own migration code — no user input — so
    // inline interpolation is safe.
    let sql = format!("PRAGMA table_info({})", table);
    let Ok(rows) = sqlx::query(&sql).fetch_all(db).await else {
        return false;
    };
    rows.iter()
        .any(|r| r.try_get::<String, _>("name").ok().as_deref() == Some(column))
}

/// Recover any of the four possible states a column-rename migration
/// can leave a user's DB in when the first attempt is broken.
///
/// State matrix:
///
/// | legacy | new | action                                             |
/// |--------|-----|----------------------------------------------------|
/// |   ✓    | ✓   | copy legacy→new (only when new is empty), drop legacy |
/// |   ✓    | ✗   | rename legacy→new                                  |
/// |   ✗    | ✓   | no-op                                              |
/// |   ✗    | ✗   | add new (empty default)                            |
///
/// The "both columns exist" row is the one PR #37's first migration
/// attempt produced: it ran ADD-then-RENAME, so ADD succeeded, RENAME
/// hit "duplicate column" → `.ok()` → data stranded in the legacy
/// column alongside an empty new column.
///
/// `legacy` / `new` are hardcoded column-name string literals from
/// the callers in `migrate()`, so inline interpolation into the SQL
/// is safe (no user input reaches PRAGMA or ALTER TABLE here).
async fn reconcile_restrict_to_group_rename(
    db: &SqlitePool,
    table: &str,
    legacy: &str,
    new: &str,
) {
    let legacy_exists = column_exists(db, table, legacy).await;
    let new_exists = column_exists(db, table, new).await;

    match (legacy_exists, new_exists) {
        (true, true) => {
            // Recovery path for the PR #37 half-migrated state.
            // Copy legacy→new where new is still the default
            // (empty string). Guard with `new = ''` so a later pass
            // that legitimately set new via UPDATE isn't overwritten
            // from the stale legacy value.
            let copy = format!(
                "UPDATE {table} SET {new} = {legacy} WHERE {new} = '' AND {legacy} IS NOT NULL"
            );
            let _ = sqlx::query(&copy).execute(db).await;

            // SQLite ≥ 3.35 supports DROP COLUMN. Silently absorb
            // if it fails — in that case the legacy column stays,
            // duplicating data, but the new column has the live
            // value and that's the one the app reads.
            let drop = format!("ALTER TABLE {table} DROP COLUMN {legacy}");
            let _ = sqlx::query(&drop).execute(db).await;
        }
        (true, false) => {
            // Clean pre-PR-#37 DB with only the legacy name.
            let rename = format!("ALTER TABLE {table} RENAME COLUMN {legacy} TO {new}");
            let _ = sqlx::query(&rename).execute(db).await;
        }
        (false, true) => {
            // Already migrated, nothing to do.
        }
        (false, false) => {
            // Fresh install — ADD with empty default.
            let add = format!("ALTER TABLE {table} ADD COLUMN {new} TEXT NOT NULL DEFAULT ''");
            let _ = sqlx::query(&add).execute(db).await;
        }
    }
}

/// Run all database migrations.
pub async fn migrate(db: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS users (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            username TEXT NOT NULL UNIQUE,
            password_hash TEXT NOT NULL,
            created_at DATETIME DEFAULT CURRENT_TIMESTAMP
        )
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS sessions (
            token TEXT PRIMARY KEY,
            user_id INTEGER NOT NULL,
            created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
            FOREIGN KEY (user_id) REFERENCES users(id)
        )
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS config (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            qbit_url TEXT NOT NULL DEFAULT '',
            qbit_user TEXT NOT NULL DEFAULT '',
            qbit_pass TEXT NOT NULL DEFAULT '',
            qbit_category TEXT NOT NULL DEFAULT '',
            jellyfin_host TEXT NOT NULL DEFAULT '',
            jellyfin_port TEXT NOT NULL DEFAULT '',
            jellyfin_api_key TEXT NOT NULL DEFAULT '',
            jellyfin_use_ssl INTEGER NOT NULL DEFAULT 0,
            jellyfin_url TEXT NOT NULL DEFAULT '',
            preferred_groups TEXT NOT NULL DEFAULT '',
            blocked_groups TEXT NOT NULL DEFAULT '',
            preferred_resolution TEXT NOT NULL DEFAULT '1080',
            quality_profile TEXT NOT NULL DEFAULT 'web_1080',
            quality_cutoff TEXT NOT NULL DEFAULT 'bd_1080',
            finished_series_quality TEXT NOT NULL DEFAULT 'prefer_bd',
            media_root TEXT NOT NULL DEFAULT '',
            title_language TEXT NOT NULL DEFAULT 'english',
            force_mal_fallback INTEGER NOT NULL DEFAULT 0,
            rss_enabled INTEGER NOT NULL DEFAULT 0,
            rss_interval_minutes INTEGER NOT NULL DEFAULT 5,
            force_kitsu_fallback INTEGER NOT NULL DEFAULT 0,
            created_at DATETIME DEFAULT CURRENT_TIMESTAMP
        )
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query("ALTER TABLE config ADD COLUMN title_language TEXT NOT NULL DEFAULT 'english'")
        .execute(db)
        .await
        .ok();

    sqlx::query("ALTER TABLE config ADD COLUMN jellyfin_use_ssl INTEGER NOT NULL DEFAULT 0")
        .execute(db)
        .await
        .ok();

    sqlx::query("ALTER TABLE config ADD COLUMN jellyfin_api_key TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();

    sqlx::query("ALTER TABLE config ADD COLUMN jellyfin_port TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();

    sqlx::query("ALTER TABLE config ADD COLUMN jellyfin_host TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();

    sqlx::query("ALTER TABLE config ADD COLUMN quality_profile TEXT NOT NULL DEFAULT 'web_1080'")
        .execute(db)
        .await
        .ok();

    sqlx::query("ALTER TABLE config ADD COLUMN quality_cutoff TEXT NOT NULL DEFAULT 'bd_1080'")
        .execute(db)
        .await
        .ok();

    sqlx::query("ALTER TABLE config ADD COLUMN finished_series_quality TEXT NOT NULL DEFAULT 'prefer_bd'")
        .execute(db)
        .await
        .ok();

    sqlx::query("ALTER TABLE config ADD COLUMN force_mal_fallback INTEGER NOT NULL DEFAULT 0")
        .execute(db)
        .await
        .ok();


    sqlx::query("ALTER TABLE config ADD COLUMN blocked_groups TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();

    sqlx::query("ALTER TABLE config ADD COLUMN rss_enabled INTEGER NOT NULL DEFAULT 0")
        .execute(db)
        .await
        .ok();

    sqlx::query("ALTER TABLE config ADD COLUMN rss_interval_minutes INTEGER NOT NULL DEFAULT 5")
        .execute(db)
        .await
        .ok();

    // Migrate old jellyfin_host/port/ssl into jellyfin_url for existing DBs.
    sqlx::query("ALTER TABLE config ADD COLUMN jellyfin_url TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();

    // If jellyfin_url is empty but jellyfin_host is set, build URL from legacy columns.
    sqlx::query(
        r#"
        UPDATE config SET jellyfin_url =
            CASE
                WHEN jellyfin_url != '' THEN jellyfin_url
                WHEN jellyfin_host = '' THEN ''
                WHEN jellyfin_use_ssl = 1 AND jellyfin_port != '' THEN 'https://' || jellyfin_host || ':' || jellyfin_port
                WHEN jellyfin_use_ssl = 1 THEN 'https://' || jellyfin_host
                WHEN jellyfin_port != '' THEN 'http://' || jellyfin_host || ':' || jellyfin_port
                ELSE 'http://' || jellyfin_host
            END
        WHERE id = 1 AND jellyfin_url = '' AND jellyfin_host != ''
        "#,
    )
    .execute(db)
    .await
    .ok();

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS series (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            anilist_id INTEGER NOT NULL UNIQUE,
            mal_id INTEGER UNIQUE,
            title TEXT NOT NULL,
            title_romaji TEXT NOT NULL DEFAULT '',
            title_english TEXT NOT NULL DEFAULT '',
            title_native TEXT NOT NULL DEFAULT '',
            cover_url TEXT NOT NULL DEFAULT '',
            format TEXT NOT NULL DEFAULT '',
            status TEXT NOT NULL DEFAULT '',
            episodes INTEGER,
            folder_name TEXT NOT NULL DEFAULT '',
            added_at DATETIME DEFAULT CURRENT_TIMESTAMP
        )
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query("ALTER TABLE series ADD COLUMN title_native TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();

    sqlx::query("ALTER TABLE series ADD COLUMN mal_id INTEGER")
        .execute(db)
        .await
        .ok();

    sqlx::query("ALTER TABLE series ADD COLUMN title_romaji TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();

    sqlx::query("ALTER TABLE series ADD COLUMN title_english TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();

    sqlx::query("ALTER TABLE series ADD COLUMN cover_url TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();

    sqlx::query("ALTER TABLE series ADD COLUMN format TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();

    sqlx::query("ALTER TABLE series ADD COLUMN status TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();

    sqlx::query("ALTER TABLE series ADD COLUMN episodes INTEGER")
        .execute(db)
        .await
        .ok();

    sqlx::query("ALTER TABLE series ADD COLUMN folder_name TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();


    sqlx::query("ALTER TABLE series ADD COLUMN monitor_mode TEXT NOT NULL DEFAULT 'future'")
        .execute(db)
        .await
        .ok();

    sqlx::query("CREATE UNIQUE INDEX IF NOT EXISTS idx_series_mal_id ON series (mal_id) WHERE mal_id IS NOT NULL")
        .execute(db)
        .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS episode_cache (
            mal_id INTEGER NOT NULL,
            episode_number INTEGER NOT NULL,
            title TEXT NOT NULL DEFAULT '',
            aired TEXT NOT NULL DEFAULT '',
            cached_at DATETIME DEFAULT CURRENT_TIMESTAMP,
            PRIMARY KEY (mal_id, episode_number)
        )
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS kitsu_episode_cache (
            kitsu_id INTEGER NOT NULL,
            episode_number INTEGER NOT NULL,
            title TEXT NOT NULL DEFAULT '',
            aired TEXT NOT NULL DEFAULT '',
            cached_at DATETIME DEFAULT CURRENT_TIMESTAMP,
            PRIMARY KEY (kitsu_id, episode_number)
        )
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS episode_monitor_state (
            series_id INTEGER NOT NULL,
            episode_number INTEGER NOT NULL,
            monitored INTEGER NOT NULL DEFAULT 0,
            updated_at DATETIME DEFAULT CURRENT_TIMESTAMP,
            PRIMARY KEY (series_id, episode_number),
            FOREIGN KEY (series_id) REFERENCES series(id) ON DELETE CASCADE
        )
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS rss_runs (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            started_at DATETIME DEFAULT CURRENT_TIMESTAMP,
            finished_at DATETIME,
            trigger_source TEXT NOT NULL DEFAULT 'manual',
            status TEXT NOT NULL DEFAULT 'running',
            items_seen INTEGER NOT NULL DEFAULT 0,
            matched INTEGER NOT NULL DEFAULT 0,
            grabbed INTEGER NOT NULL DEFAULT 0,
            skipped INTEGER NOT NULL DEFAULT 0,
            detail TEXT NOT NULL DEFAULT ''
        )
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS rss_seen (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            item_key TEXT NOT NULL UNIQUE,
            title TEXT NOT NULL DEFAULT '',
            link TEXT NOT NULL DEFAULT '',
            series_id INTEGER,
            series_title TEXT NOT NULL DEFAULT '',
            group_name TEXT NOT NULL DEFAULT '',
            is_batch INTEGER NOT NULL DEFAULT 0,
            decision TEXT NOT NULL DEFAULT 'skipped',
            reason TEXT NOT NULL DEFAULT '',
            source TEXT NOT NULL DEFAULT 'rss',
            created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
            FOREIGN KEY (series_id) REFERENCES series(id)
        )
        "#,
    )
    .execute(db)
    .await?;


    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS series_relations_cache (
            series_id INTEGER NOT NULL,
            related_provider_id INTEGER NOT NULL,
            related_mal_id INTEGER,
            title_romaji TEXT NOT NULL DEFAULT '',
            title_english TEXT NOT NULL DEFAULT '',
            title_native TEXT NOT NULL DEFAULT '',
            cover_url TEXT NOT NULL DEFAULT '',
            format TEXT NOT NULL DEFAULT '',
            status TEXT NOT NULL DEFAULT '',
            episodes INTEGER,
            relation_type TEXT NOT NULL DEFAULT '',
            season_year INTEGER,
            media_type TEXT NOT NULL DEFAULT '',
            cached_at DATETIME DEFAULT CURRENT_TIMESTAMP,
            PRIMARY KEY (series_id, related_provider_id, relation_type),
            FOREIGN KEY (series_id) REFERENCES series(id) ON DELETE CASCADE
        )
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS series_episode_metadata (
            series_id INTEGER NOT NULL,
            episode_number INTEGER NOT NULL,
            title TEXT NOT NULL DEFAULT '',
            title_romaji TEXT NOT NULL DEFAULT '',
            title_english TEXT NOT NULL DEFAULT '',
            title_native TEXT NOT NULL DEFAULT '',
            aired TEXT NOT NULL DEFAULT '',
            source TEXT NOT NULL DEFAULT '',
            cached_at DATETIME DEFAULT CURRENT_TIMESTAMP,
            PRIMARY KEY (series_id, episode_number),
            FOREIGN KEY (series_id) REFERENCES series(id) ON DELETE CASCADE
        )
        "#,
    )
    .execute(db)
    .await?;


    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS provider_metadata_cache (
            provider_id INTEGER PRIMARY KEY,
            mal_id INTEGER,
            detail_json TEXT NOT NULL,
            cached_at DATETIME DEFAULT CURRENT_TIMESTAMP
        )
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS provider_relations_cache (
            provider_id INTEGER NOT NULL,
            related_provider_id INTEGER NOT NULL,
            related_mal_id INTEGER,
            title_romaji TEXT NOT NULL DEFAULT '',
            title_english TEXT NOT NULL DEFAULT '',
            title_native TEXT NOT NULL DEFAULT '',
            cover_url TEXT NOT NULL DEFAULT '',
            format TEXT NOT NULL DEFAULT '',
            status TEXT NOT NULL DEFAULT '',
            episodes INTEGER,
            relation_type TEXT NOT NULL DEFAULT '',
            season_year INTEGER,
            media_type TEXT NOT NULL DEFAULT '',
            cached_at DATETIME DEFAULT CURRENT_TIMESTAMP,
            PRIMARY KEY (provider_id, related_provider_id, relation_type)
        )
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS provider_episode_metadata (
            provider_id INTEGER NOT NULL,
            episode_number INTEGER NOT NULL,
            title TEXT NOT NULL DEFAULT '',
            title_romaji TEXT NOT NULL DEFAULT '',
            title_english TEXT NOT NULL DEFAULT '',
            title_native TEXT NOT NULL DEFAULT '',
            aired TEXT NOT NULL DEFAULT '',
            source TEXT NOT NULL DEFAULT '',
            cached_at DATETIME DEFAULT CURRENT_TIMESTAMP,
            PRIMARY KEY (provider_id, episode_number)
        )
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS logs (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp DATETIME DEFAULT CURRENT_TIMESTAMP,
            level TEXT NOT NULL DEFAULT 'info',
            category TEXT NOT NULL DEFAULT 'system',
            message TEXT NOT NULL DEFAULT '',
            detail TEXT NOT NULL DEFAULT ''
        )
        "#,
    )
    .execute(db)
    .await?;

    // Index for efficient log queries.
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_logs_timestamp ON logs (timestamp DESC)"
    )
    .execute(db)
    .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_logs_level_cat ON logs (level, category)"
    )
    .execute(db)
    .await?;

    // ── Legacy migrations (kept for existing DB compat) ────────────────
    // tmdb_api_key and plex_mappings_* are no longer used but columns
    // remain in existing databases.
    sqlx::query("ALTER TABLE config ADD COLUMN tmdb_api_key TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();

    // Three-DB-states rename for `force_tmdb_fallback` → `force_kitsu_fallback`:
    //
    //   1. Fresh install — neither column exists. The ADD succeeds and creates
    //      `force_tmdb_fallback`; the subsequent RENAME moves it to the new name.
    //      End state: `force_kitsu_fallback` exists.
    //   2. Legacy install — only `force_tmdb_fallback` exists. The ADD is a no-op
    //      (`.ok()` swallows "duplicate column name"); the RENAME moves it to the
    //      new name. End state: `force_kitsu_fallback` exists.
    //   3. Post-migration install — only `force_kitsu_fallback` exists. The ADD
    //      *creates* `force_tmdb_fallback` as a vestigial column because SQLite has
    //      no IF NOT EXISTS check on column name alone; the RENAME then fails
    //      because the target name is already taken (swallowed by `.ok()`). End
    //      state: `force_kitsu_fallback` still exists, but so does a stray
    //      `force_tmdb_fallback` column. This is harmless — nothing reads it — but
    //      it's a cosmetic wart. If you're cleaning up, an `IF NOT EXISTS` guarded
    //      by a `PRAGMA table_info` check would fix it.
    sqlx::query("ALTER TABLE config ADD COLUMN force_tmdb_fallback INTEGER NOT NULL DEFAULT 0")
        .execute(db)
        .await
        .ok();

    sqlx::query("ALTER TABLE config RENAME COLUMN force_tmdb_fallback TO force_kitsu_fallback")
        .execute(db)
        .await
        .ok();

    sqlx::query("ALTER TABLE config ADD COLUMN plex_mappings_enabled INTEGER NOT NULL DEFAULT 0")
        .execute(db)
        .await
        .ok();

    sqlx::query("ALTER TABLE config ADD COLUMN plex_mappings_auto_refresh INTEGER NOT NULL DEFAULT 0")
        .execute(db)
        .await
        .ok();

    sqlx::query("ALTER TABLE config ADD COLUMN post_processing_enabled INTEGER NOT NULL DEFAULT 0")
        .execute(db)
        .await
        .ok();

    sqlx::query("ALTER TABLE config ADD COLUMN post_processing_mode TEXT NOT NULL DEFAULT 'hardlink'")
        .execute(db)
        .await
        .ok();

    // The path where qBittorrent downloads live, as seen by Ryokan.
    // When qBit runs in Docker its internal save_path (e.g. /downloads/) differs
    // from where the host (or Ryokan) can actually read the files.
    sqlx::query("ALTER TABLE config ADD COLUMN qbit_download_path TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS grabbed_torrents (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            hash TEXT NOT NULL DEFAULT '',
            torrent_name TEXT NOT NULL DEFAULT '',
            series_id INTEGER NOT NULL,
            episode_numbers TEXT NOT NULL DEFAULT '[]',
            state TEXT NOT NULL DEFAULT 'pending',
            grabbed_at DATETIME DEFAULT CURRENT_TIMESTAMP,
            imported_at DATETIME,
            FOREIGN KEY (series_id) REFERENCES series(id) ON DELETE CASCADE
        )
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_grabbed_torrents_state ON grabbed_torrents (state)")
        .execute(db)
        .await?;

    // Many hot-path queries filter on series_id (find_imported_for_episode,
    // get_all_for_series, mark_failed_by_name, etc.) and the prior schema
    // had no index covering it — every lookup did a full table scan. Sort
    // key lets get_all_for_series / get_blocked / get_all_with_series read
    // in chronological order without a separate sort.
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_grabbed_torrents_series ON grabbed_torrents (series_id, grabbed_at DESC)",
    )
    .execute(db)
    .await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_grabbed_torrents_hash ON grabbed_torrents (hash) WHERE hash != ''")
        .execute(db)
        .await?;

    // One-time backfill: deduplicate active grabs sharing a hash before
    // creating the unique index below. Pre-fix race in record_grab could
    // produce duplicate pending/imported rows for the same hash; the
    // unique index would otherwise refuse to create. Keeps the oldest
    // row per hash (lowest id), drops the rest. Idempotent — a second
    // boot finds no duplicates and the DELETE no-ops.
    sqlx::query(
        r#"DELETE FROM grabbed_torrents
           WHERE hash != ''
             AND state IN ('pending', 'imported')
             AND id NOT IN (
                 SELECT MIN(id) FROM grabbed_torrents
                 WHERE hash != '' AND state IN ('pending', 'imported')
                 GROUP BY hash
             )"#,
    )
    .execute(db)
    .await?;

    // Partial UNIQUE index that backs the atomic dedup in record_grab's
    // INSERT OR IGNORE. Restricted to active states so a hash that's
    // been blocklisted ('failed') or removed can still be re-recorded
    // — preserving the prior SELECT's `state IN ('pending', 'imported')`
    // filter as the dedup window.
    sqlx::query(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_grabbed_torrents_hash_active
         ON grabbed_torrents (hash)
         WHERE hash != '' AND state IN ('pending', 'imported')",
    )
    .execute(db)
    .await?;

    // Per-file series routing for multi-series batch releases. A
    // megapack that covers e.g. JoJo S1-S5 gets one row per sibling
    // in this table, each carrying the file indices (into the
    // torrent's canonical file list) that belong to that sibling and
    // the episode numbers those files represent. The parent series
    // (the one the user actually searched for) also gets a row here
    // covering unclaimed files. Legacy single-series grabs that
    // predate Phase 2 have no row here and are handled by a
    // fall-through path in post_processing that treats the
    // grabbed_torrents.series_id as the sole route.
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS grabbed_torrent_series (
            grab_id INTEGER NOT NULL,
            series_id INTEGER NOT NULL,
            file_indices TEXT NOT NULL DEFAULT '[]',
            episode_numbers TEXT NOT NULL DEFAULT '[]',
            matched_subtitle TEXT NOT NULL DEFAULT '',
            PRIMARY KEY (grab_id, series_id),
            FOREIGN KEY (grab_id) REFERENCES grabbed_torrents(id) ON DELETE CASCADE,
            FOREIGN KEY (series_id) REFERENCES series(id) ON DELETE CASCADE
        )
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_grabbed_torrent_series_series ON grabbed_torrent_series (series_id)")
        .execute(db)
        .await?;

    // Per-route episode offset for the Phase 2 auto-expand path.
    // Applied by post_processing at rename time to convert a file's
    // absolute episode number into the sibling's arc-local episode
    // number (e.g. smol Monogatari batch: E14 → E01 of Owari S2 with
    // offset 13, NoobSubs JoJo: E25 → E01 of Egypt-hen with offset 24).
    // Non-offset siblings (filenames numbered arc-local from 1) get
    // offset 0, matching the legacy default for rows written before
    // this column existed.
    sqlx::query("ALTER TABLE grabbed_torrent_series ADD COLUMN episode_offset INTEGER NOT NULL DEFAULT 0")
        .execute(db)
        .await
        .ok();

    // tmdb_id on series is a leftover from before the Kitsu migration;
    // the column is harmless to keep for existing databases.
    sqlx::query("ALTER TABLE series ADD COLUMN tmdb_id INTEGER")
        .execute(db)
        .await
        .ok();

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS series_metadata_cache (
            series_id INTEGER PRIMARY KEY,
            provider_id INTEGER NOT NULL,
            mal_id INTEGER,
            detail_json TEXT NOT NULL,
            cached_at DATETIME DEFAULT CURRENT_TIMESTAMP,
            FOREIGN KEY (series_id) REFERENCES series(id) ON DELETE CASCADE
        )
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_series_metadata_cache_cached_at ON series_metadata_cache (cached_at DESC)")
        .execute(db)
        .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS artwork_cache (
            cache_key TEXT PRIMARY KEY,
            parent_kind TEXT NOT NULL DEFAULT '',
            parent_id INTEGER,
            image_kind TEXT NOT NULL DEFAULT '',
            source_url TEXT NOT NULL DEFAULT '',
            local_path TEXT NOT NULL DEFAULT '',
            content_type TEXT NOT NULL DEFAULT '',
            last_write INTEGER NOT NULL DEFAULT 0,
            cached_at DATETIME DEFAULT CURRENT_TIMESTAMP,
            FOREIGN KEY (parent_id) REFERENCES series(id) ON DELETE CASCADE
        )
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_artwork_cache_parent ON artwork_cache (parent_kind, parent_id, image_kind)")
        .execute(db)
        .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS image_blobs (
            blob_hash TEXT PRIMARY KEY,
            local_path TEXT NOT NULL DEFAULT '',
            content_type TEXT NOT NULL DEFAULT '',
            byte_size INTEGER NOT NULL DEFAULT 0,
            created_at DATETIME DEFAULT CURRENT_TIMESTAMP
        )
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS image_refs (
            cache_key TEXT PRIMARY KEY,
            parent_kind TEXT NOT NULL DEFAULT '',
            parent_id INTEGER,
            image_kind TEXT NOT NULL DEFAULT '',
            source_url TEXT NOT NULL DEFAULT '',
            blob_hash TEXT NOT NULL,
            last_write INTEGER NOT NULL DEFAULT 0,
            cached_at DATETIME DEFAULT CURRENT_TIMESTAMP,
            FOREIGN KEY (blob_hash) REFERENCES image_blobs(blob_hash) ON DELETE CASCADE,
            FOREIGN KEY (parent_id) REFERENCES series(id) ON DELETE CASCADE
        )
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_image_refs_parent ON image_refs (parent_kind, parent_id, image_kind)")
        .execute(db)
        .await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_image_refs_blob_hash ON image_refs (blob_hash)")
        .execute(db)
        .await?;

    // Episode quality tags: store the latest grabbed release for each episode.
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS episode_quality_tags (
            series_id INTEGER NOT NULL,
            episode_number INTEGER NOT NULL,
            quality_tag TEXT NOT NULL DEFAULT '',
            release_title TEXT NOT NULL DEFAULT '',
            release_group TEXT NOT NULL DEFAULT '',
            state TEXT NOT NULL DEFAULT 'grabbed',
            updated_at DATETIME DEFAULT CURRENT_TIMESTAMP,
            PRIMARY KEY (series_id, episode_number),
            FOREIGN KEY (series_id) REFERENCES series(id) ON DELETE CASCADE
        )
        "#,
    )
    .execute(db)
    .await?;

    // Full grab history per episode (all grabs, with state tracking for failed marks).
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS episode_grab_history (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            series_id INTEGER NOT NULL,
            episode_number INTEGER NOT NULL,
            quality_tag TEXT NOT NULL DEFAULT '',
            release_title TEXT NOT NULL DEFAULT '',
            release_group TEXT NOT NULL DEFAULT '',
            file_name TEXT NOT NULL DEFAULT '',
            size_bytes INTEGER NOT NULL DEFAULT 0,
            is_batch INTEGER NOT NULL DEFAULT 0,
            state TEXT NOT NULL DEFAULT 'grabbed',
            grabbed_at DATETIME DEFAULT CURRENT_TIMESTAMP,
            FOREIGN KEY (series_id) REFERENCES series(id) ON DELETE CASCADE
        )
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_episode_grab_history_series ON episode_grab_history (series_id, episode_number, grabbed_at DESC)")
        .execute(db)
        .await?;

    // On-disk *post-processed* file name for this episode. Seeded from the
    // Nyaa release title at grab time, then overwritten at post-process
    // time with the final Sonarr-style filename Ryokan renamed the imported
    // file to (e.g. `Jujutsu Kaisen - S01E06 - Hidden Inventory.mkv`). The
    // episode detail modal reads this column so each grab-history row
    // shows the per-episode file name — distinct from the batch torrent's
    // release title, which is already in `release_title`. Historically
    // this column was called `torrent_name`.
    //
    // Upgrade path: check for `file_name` first — a fresh install gets
    // it from CREATE TABLE above, and an already-migrated install has
    // it from a prior rename. If it's missing we're on a legacy
    // `torrent_name` install and need to rename. The defensive ADD
    // covers the corner case where neither column is present (which
    // shouldn't happen, but keeps downstream writes safe).
    if !column_exists(db, "episode_grab_history", "file_name").await {
        // Two paths for the legacy schema:
        //
        //  - `torrent_name` exists  → RENAME it to `file_name`. Propagate
        //    any failure from the RENAME instead of swallowing with .ok():
        //    the previous code paired the RENAME with an unconditional ADD
        //    so a transient RENAME failure (DB lock, FK quirk, I/O hiccup)
        //    would leave an empty `file_name` column on top of intact
        //    `torrent_name` data and the next boot would think the
        //    migration was already done. Refusing to start with a real
        //    error is preferable to silent data loss.
        //
        //  - `torrent_name` is also missing → defensive ADD for the
        //    corrupted-schema corner case. Without `torrent_name` to
        //    rename from there is no data to lose, so the ADD is safe.
        if column_exists(db, "episode_grab_history", "torrent_name").await {
            sqlx::query("ALTER TABLE episode_grab_history RENAME COLUMN torrent_name TO file_name")
                .execute(db)
                .await?;
        } else {
            sqlx::query("ALTER TABLE episode_grab_history ADD COLUMN file_name TEXT NOT NULL DEFAULT ''")
                .execute(db)
                .await?;
        }
    }

    // Episode-file size. For non-batch grabs this gets refined to the
    // imported file's size at post-process time. For batch grabs it
    // stays as the whole torrent's total reported at grab time — the
    // episode detail modal surfaces that as "this episode came from an
    // X GiB batch". The CASE guard in `mark_grab_history_completed`
    // enforces this asymmetry.
    sqlx::query("ALTER TABLE episode_grab_history ADD COLUMN size_bytes INTEGER NOT NULL DEFAULT 0")
        .execute(db)
        .await
        .ok();

    // is_batch marker — needed at read time so the UI can decide whether
    // to surface `size_bytes` as "whole batch" or "single file". It's
    // also what `mark_grab_history_completed` uses to decide whether to
    // refine `size_bytes` on import (non-batch only).
    sqlx::query("ALTER TABLE episode_grab_history ADD COLUMN is_batch INTEGER NOT NULL DEFAULT 0")
        .execute(db)
        .await
        .ok();

    // auto_grab_on_add: whether to automatically search for monitored episodes after adding a series.
    sqlx::query("ALTER TABLE config ADD COLUMN auto_grab_on_add INTEGER NOT NULL DEFAULT 1")
        .execute(db)
        .await
        .ok();

    // prefer_subs: when true (default), penalize dual audio / dub releases in scoring.
    sqlx::query("ALTER TABLE config ADD COLUMN prefer_subs INTEGER NOT NULL DEFAULT 1")
        .execute(db)
        .await
        .ok();

    // allow_non_english: when false (default), auto-search/RSS uses English-translated Nyaa category.
    sqlx::query("ALTER TABLE config ADD COLUMN allow_non_english INTEGER NOT NULL DEFAULT 0")
        .execute(db)
        .await
        .ok();

    // Sonarr API compatibility layer for Seerr integration.
    sqlx::query("ALTER TABLE config ADD COLUMN sonarr_enabled INTEGER NOT NULL DEFAULT 0")
        .execute(db)
        .await
        .ok();
    sqlx::query("ALTER TABLE config ADD COLUMN sonarr_api_key TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();

    // season_year on series for Sonarr/Radarr compat year field.
    sqlx::query("ALTER TABLE series ADD COLUMN season_year INTEGER")
        .execute(db)
        .await
        .ok();

    // end_year lets Layer 4 (temporal inference) reason about how long a
    // *finished* series has been off-air, rather than treating season_year
    // (the start year) as the finish proxy. Long-running shows can finish
    // years after their season_year; without end_year the "finished 1+
    // year ago" rule would fire immediately for every ep of a decade-long
    // run. Populated from AniList's endDate.year where available.
    sqlx::query("ALTER TABLE series ADD COLUMN end_year INTEGER")
        .execute(db)
        .await
        .ok();

    // Phase 4: per-series upgrade toggle. When 0, the upgrade scanner
    // skips this series entirely — user opts out of re-grabs even if a
    // higher-quality release appears. Default 1 preserves prior behavior.
    sqlx::query("ALTER TABLE series ADD COLUMN allow_upgrades INTEGER NOT NULL DEFAULT 1")
        .execute(db)
        .await
        .ok();

    // Radarr API compatibility layer for Seerr integration (anime movies).
    sqlx::query("ALTER TABLE config ADD COLUMN radarr_enabled INTEGER NOT NULL DEFAULT 0")
        .execute(db)
        .await
        .ok();
    sqlx::query("ALTER TABLE config ADD COLUMN radarr_api_key TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();
    sqlx::query("ALTER TABLE config ADD COLUMN upgrade_search_enabled INTEGER NOT NULL DEFAULT 0")
        .execute(db)
        .await
        .ok();

    // Classification pipeline: release group → source map (Phase 1a).
    // Creates the table and seeds the built-in defaults. Idempotent.
    group_source_map::migrate(db).await?;

    // Layer 2 description cache — scraped Nyaa description bodies keyed by
    // torrent info_hash, populated on the low-confidence classifier path.
    nyaa_description_cache::migrate(db).await?;

    // Layer 5 ffprobe cache — cached ffprobe JSON keyed by (path, mtime, size).
    // Populated after imports land so re-classifications (library scans,
    // upgrade checks) don't re-shell out to ffprobe for the same file.
    media_probe_cache::migrate(db).await?;

    // Sonarr-v4-compatible Custom Formats. Two tables: one for CF
    // definitions (raw JSON preserved for byte-perfect re-export) and
    // one for (custom_format_id, profile_id) → score. V1 hardcodes
    // profile_id = 1 everywhere.
    custom_formats::migrate(db).await?;

    // Upgrade path for databases that were created before the `origin`
    // column shipped. Fresh installs already got this column from the
    // CREATE TABLE in `custom_formats::migrate`; the ALTER here is a
    // no-op on those and adds the column on legacy databases. Legal
    // values: `manual`, `import`, `defaults`. Pre-existing rows default
    // to `manual` — anyone who already installed defaults before this
    // column shipped can use the Reset button to relabel them.
    sqlx::query("ALTER TABLE custom_formats ADD COLUMN origin TEXT NOT NULL DEFAULT 'manual'")
        .execute(db)
        .await
        .ok();

    // ── Phase 1b: classification columns on episode_quality_tags ─────────
    // These record the ClassificationResult at grab time so later scoring,
    // upgrade detection, and UI review workflows can read structured source
    // / resolution / remux data instead of parsing the legacy quality_tag
    // string. Defaults are empty/zero for rows that predate Phase 1b; the
    // legacy quality_tag column remains populated for backwards compat.
    sqlx::query("ALTER TABLE episode_quality_tags ADD COLUMN source TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();
    sqlx::query("ALTER TABLE episode_quality_tags ADD COLUMN resolution TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();
    sqlx::query("ALTER TABLE episode_quality_tags ADD COLUMN is_remux INTEGER NOT NULL DEFAULT 0")
        .execute(db)
        .await
        .ok();
    sqlx::query("ALTER TABLE episode_quality_tags ADD COLUMN classification_confidence REAL NOT NULL DEFAULT 0")
        .execute(db)
        .await
        .ok();
    sqlx::query("ALTER TABLE episode_quality_tags ADD COLUMN needs_review INTEGER NOT NULL DEFAULT 0")
        .execute(db)
        .await
        .ok();
    sqlx::query("ALTER TABLE episode_quality_tags ADD COLUMN manual_override INTEGER NOT NULL DEFAULT 0")
        .execute(db)
        .await
        .ok();

    // Sonarr-parity sub-classification columns:
    //  - is_bdmv: distinguishes BD-RAW / BDMV (full disc structure) from
    //    a plain BluRay encode or a Remux. Mutually exclusive with
    //    is_remux at the label level.
    //  - web_kind: distinguishes WEB-DL from WEBRip when the filename was
    //    specific enough to tell. Stored as the canonical string ("WEB-DL",
    //    "WEBRip", or "" for legacy bare-WEB rows).
    sqlx::query("ALTER TABLE episode_quality_tags ADD COLUMN is_bdmv INTEGER NOT NULL DEFAULT 0")
        .execute(db)
        .await
        .ok();
    // is_batch on grabbed_torrents lets the post-download classifier
    // re-run Layer 4 (temporal inference) with the original batch flag
    // that the pre-download call used, rather than hardcoding `false`
    // and losing the "finished 1+ year ago + batch → BluRay" signal
    // after import.
    sqlx::query("ALTER TABLE grabbed_torrents ADD COLUMN is_batch INTEGER NOT NULL DEFAULT 0")
        .execute(db)
        .await
        .ok();

    // Sonarr-parity dual-path tracking (#14 follow-up). Stamped from
    // qBit's `content_path` (API ≥ 2.6.1) or `save_path` fallback the
    // moment qBit reports the torrent complete, independent of whether
    // post-processing has moved the file into the library. The episode
    // detail modal renders this alongside the post-processed library
    // path so the operator can see both locations when a torrent
    // finishes (matters with post-proc off, or when hardlinking keeps
    // both paths valid simultaneously).
    sqlx::query("ALTER TABLE grabbed_torrents ADD COLUMN qbit_content_path TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();

    sqlx::query("ALTER TABLE episode_quality_tags ADD COLUMN web_kind TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();
    // Persist the full evidence trail as JSON so the Needs-Review UI can
    // audit *why* a row was flagged without re-classifying. Stores a
    // serialized `Vec<SourceEvidence>` — empty string for legacy rows and
    // for rehydrated/synthetic classifications that don't carry a live
    // trail.
    sqlx::query(
        "ALTER TABLE episode_quality_tags ADD COLUMN classification_evidence TEXT NOT NULL DEFAULT ''",
    )
    .execute(db)
    .await
    .ok();

    // ── Phase 1b: split quality_profile/quality_cutoff into explicit source
    //             and resolution fields ──────────────────────────────────
    // preferred_resolution already exists and stores a bare resolution
    // string ("1080", "720", …) — it's migrated in place and is now the
    // authoritative preferred-resolution field. The three new columns cover
    // the bits that didn't exist before. Legacy quality_profile and
    // quality_cutoff are kept for one release as a rollback hatch.
    // Floor for total_cf_score after the CF pipeline sums a candidate's
    // matching formats. Default `-2147483648` (= i32::MIN) means "no
    // floor" — the user opts in by raising it via the Custom Formats
    // settings page. Read paths fall back to this sentinel when the
    // column is present but the row predates it.
    sqlx::query("ALTER TABLE config ADD COLUMN custom_format_minimum_score INTEGER NOT NULL DEFAULT -2147483648")
        .execute(db)
        .await
        .ok();

    // SeaDex "best release" boost toggle. Default OFF so upgrades don't
    // kick in silently for existing installs on first run after the
    // feature ships. Suppressed at scoring time when the user already
    // has a SeaDexBestSpecification CF (avoids double-counting).
    sqlx::query("ALTER TABLE config ADD COLUMN seadex_enabled INTEGER NOT NULL DEFAULT 0")
        .execute(db)
        .await
        .ok();

    sqlx::query("ALTER TABLE config ADD COLUMN preferred_source TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();
    sqlx::query("ALTER TABLE config ADD COLUMN cutoff_source TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();
    sqlx::query("ALTER TABLE config ADD COLUMN cutoff_resolution TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();

    // Backfill the new columns from the legacy combined fields. Only runs
    // for existing rows where the new columns are still empty, so a fresh
    // install (which uses Default::default values) is left alone.
    sqlx::query(
        r#"
        UPDATE config SET preferred_source = CASE
            WHEN quality_profile LIKE 'web_%' THEN 'web'
            WHEN quality_profile LIKE 'bd_%' THEN 'bluray'
            WHEN quality_profile LIKE 'remux_%' THEN 'bluray'
            WHEN quality_profile = 'dvd' THEN 'dvd'
            ELSE 'web'
        END
        WHERE id = 1 AND preferred_source = ''
        "#,
    )
    .execute(db)
    .await
    .ok();

    sqlx::query(
        r#"
        UPDATE config SET cutoff_source = CASE
            WHEN quality_cutoff LIKE 'web_%' THEN 'web'
            WHEN quality_cutoff LIKE 'bd_%' THEN 'bluray'
            WHEN quality_cutoff LIKE 'remux_%' THEN 'bluray'
            WHEN quality_cutoff = 'dvd' THEN 'dvd'
            ELSE 'bluray'
        END
        WHERE id = 1 AND cutoff_source = ''
        "#,
    )
    .execute(db)
    .await
    .ok();

    sqlx::query(
        r#"
        UPDATE config SET cutoff_resolution = CASE
            WHEN quality_cutoff LIKE '%_480' OR quality_cutoff = 'dvd' THEN '480'
            WHEN quality_cutoff LIKE '%_720' THEN '720'
            WHEN quality_cutoff LIKE '%_1080' THEN '1080'
            WHEN quality_cutoff LIKE '%_2160' THEN '2160'
            ELSE '1080'
        END
        WHERE id = 1 AND cutoff_resolution = ''
        "#,
    )
    .execute(db)
    .await
    .ok();

    // Rewrite denormalized `quality_tag` strings on pre-existing
    // `episode_quality_tags` / `episode_grab_history` rows to match the
    // Sonarr-parity label format the classifier now emits:
    // `BD-1080p`, `BD-1080p Remux`, `BD-1080p RAW`, `WEB-1080p`,
    // `WEBRip-1080p`, `HDTV-1080p`, `DVD-480p`, etc. This migration
    // bridges three prior schemas: (1) very old space-joined rows
    // like "BluRay 1080p" / "WEB-DL 1080p", (2) the intermediate
    // dash-joined rename (`BD-Remux-1080p`, `BD-RAW-1080p`,
    // `WEBRIP-1080p`) that shipped briefly before the Sonarr-parity
    // reorder landed, and (3) the post-#48 `WEBDL-1080p` intermediate
    // that was subsequently unified into bare `WEB-1080p`.
    //
    // `episode_quality_tags` has the structured source/resolution/
    // web_kind/is_remux/is_bdmv columns, so we regenerate `quality_tag`
    // directly from ground truth — always correct regardless of which
    // label format happened to be in the column. `episode_grab_history`
    // doesn't carry the structured columns (it's a grab-time audit
    // trail, not a classification store), so we fall back to ordered
    // REPLACE statements on known legacy patterns. Fully idempotent:
    // the regen overwrites with the same value on re-runs, and the
    // REPLACE chain no-ops once its source patterns are gone.
    // The CASE is duplicated in SET and WHERE on purpose: gating the
    // UPDATE means SQLite only writes rows whose quality_tag would
    // actually change, so a boot on an already-migrated database does
    // zero WAL writes here instead of dirtying every row in the table.
    // Without the gate, every boot churned the WAL and held the write
    // lock long enough to delay the very first incoming request after
    // startup.
    //
    // MAINTENANCE: any edit to the SET CASE must be mirrored in the
    // WHERE CASE below (and vice versa). Diverging the two is a
    // correctness bug — the WHERE's job is to match the SET's output
    // exactly, so the gate only skips rows that truly don't need the
    // rewrite.
    sqlx::query(
        r#"
        UPDATE episode_quality_tags SET quality_tag = CASE
            WHEN TRIM(COALESCE(source, '')) = ''
              OR LOWER(source) = 'unknown' THEN
                CASE WHEN COALESCE(resolution, '') IN ('', 'Unknown')
                     THEN 'Unknown' ELSE resolution END
            ELSE
                (CASE
                    WHEN LOWER(source) IN ('bluray', 'blu-ray', 'bd') THEN 'BD'
                    WHEN LOWER(source) = 'web' THEN
                        CASE
                            WHEN LOWER(COALESCE(web_kind, '')) IN ('webrip', 'web-rip', 'web.rip') THEN 'WEBRip'
                            ELSE 'WEB'
                        END
                    ELSE UPPER(source)
                END)
                || CASE WHEN COALESCE(resolution, '') IN ('', 'Unknown')
                        THEN '' ELSE '-' || resolution END
                || CASE
                    WHEN LOWER(source) IN ('bluray', 'blu-ray', 'bd')
                         AND COALESCE(is_bdmv, 0) = 1 THEN ' RAW'
                    WHEN LOWER(source) IN ('bluray', 'blu-ray', 'bd')
                         AND COALESCE(is_remux, 0) = 1 THEN ' Remux'
                    ELSE ''
                END
        END
        WHERE COALESCE(quality_tag, '') <> CASE
            WHEN TRIM(COALESCE(source, '')) = ''
              OR LOWER(source) = 'unknown' THEN
                CASE WHEN COALESCE(resolution, '') IN ('', 'Unknown')
                     THEN 'Unknown' ELSE resolution END
            ELSE
                (CASE
                    WHEN LOWER(source) IN ('bluray', 'blu-ray', 'bd') THEN 'BD'
                    WHEN LOWER(source) = 'web' THEN
                        CASE
                            WHEN LOWER(COALESCE(web_kind, '')) IN ('webrip', 'web-rip', 'web.rip') THEN 'WEBRip'
                            ELSE 'WEB'
                        END
                    ELSE UPPER(source)
                END)
                || CASE WHEN COALESCE(resolution, '') IN ('', 'Unknown')
                        THEN '' ELSE '-' || resolution END
                || CASE
                    WHEN LOWER(source) IN ('bluray', 'blu-ray', 'bd')
                         AND COALESCE(is_bdmv, 0) = 1 THEN ' RAW'
                    WHEN LOWER(source) IN ('bluray', 'blu-ray', 'bd')
                         AND COALESCE(is_remux, 0) = 1 THEN ' Remux'
                    ELSE ''
                END
        END
        "#,
    )
    .execute(db)
    .await
    .ok();

    // `episode_grab_history` replacements. Two-pass approach, since
    // SQLite REPLACE is a dumb substring swap and can't reorder tokens
    // around a variable-width resolution in one shot:
    //
    //   Pass A — normalize legacy space-joined tokens to the
    //            intermediate dash-joined form ("BluRay BDMV 1080p" →
    //            "BD-RAW-1080p", "WEB-DL 1080p" → "WEB-1080p", etc.).
    //            Only the BluRay BDMV/Remux/plain and WEB variants need
    //            ordering care: the qualified BluRay patterns must fire
    //            before the generic "BluRay " prefix is stripped.
    //
    //   Pass B — reorder `BD-{RAW|Remux}-{res}` into the final
    //            Sonarr-parity `BD-{res} {RAW|Remux}` form. REPLACE
    //            needs one entry per supported resolution because it
    //            can't swap tokens generically.
    //
    // Straggler entries at the end:
    //  - "WEBRIP-" → "WEBRip-" fixes the all-caps form from the pre-
    //    Sonarr-parity rename pass.
    //  - `WEBDL-<res>` → `WEB-<res>` (one per `Resolution::as_str()`
    //    output: 480p/576p/720p/1080p/2160p) catches DBs that booted
    //    the intermediate `WEBDL-` build between issue #48's
    //    unification and this migration. Add a new resolution here
    //    if the `Resolution` enum ever gains one.
    for (old, new) in [
        // ── Pass A: legacy space-joined → intermediate dash form ──
        ("BluRay BDMV ", "BD-RAW-"),
        ("BluRay Remux ", "BD-Remux-"),
        ("BluRay ", "BD-"),
        // WebDl collapses to the bare "WEB" label (issue #48), so
        // legacy "WEB-DL 1080p" strings rewrite straight to the new
        // unified form, skipping the old "WEBDL-" intermediate.
        ("WEB-DL ", "WEB-"),
        ("WEBRip ", "WEBRip-"),
        ("Web ", "WEB-"),
        ("HDTV ", "HDTV-"),
        ("DVD ", "DVD-"),
        ("TV ", "TV-"),
        // ── Pass B: intermediate dash form → Sonarr-parity reorder ──
        ("BD-RAW-480p", "BD-480p RAW"),
        ("BD-RAW-576p", "BD-576p RAW"),
        ("BD-RAW-720p", "BD-720p RAW"),
        ("BD-RAW-1080p", "BD-1080p RAW"),
        ("BD-RAW-2160p", "BD-2160p RAW"),
        ("BD-Remux-480p", "BD-480p Remux"),
        ("BD-Remux-576p", "BD-576p Remux"),
        ("BD-Remux-720p", "BD-720p Remux"),
        ("BD-Remux-1080p", "BD-1080p Remux"),
        ("BD-Remux-2160p", "BD-2160p Remux"),
        // Case-fix stragglers from the intermediate all-caps form.
        ("WEBRIP-", "WEBRip-"),
        // Issue #48: collapse any stored `WEBDL-<res>` strings
        // (written by prior builds) to `WEB-<res>`. Needs one entry
        // per resolution because REPLACE is a dumb substring swap.
        ("WEBDL-480p", "WEB-480p"),
        ("WEBDL-576p", "WEB-576p"),
        ("WEBDL-720p", "WEB-720p"),
        ("WEBDL-1080p", "WEB-1080p"),
        ("WEBDL-2160p", "WEB-2160p"),
    ] {
        let like_pat = format!("%{}%", old);
        let _ = sqlx::query(
            "UPDATE episode_grab_history
             SET quality_tag = REPLACE(quality_tag, ?, ?)
             WHERE quality_tag LIKE ?",
        )
        .bind(old)
        .bind(new)
        .bind(like_pat)
        .execute(db)
        .await;
    }

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS scheduled_task_runs (
            task_key TEXT PRIMARY KEY,
            display_name TEXT NOT NULL DEFAULT '',
            schedule_label TEXT NOT NULL DEFAULT '',
            enabled INTEGER NOT NULL DEFAULT 1,
            last_started_at DATETIME,
            last_finished_at DATETIME,
            last_status TEXT NOT NULL DEFAULT 'idle',
            last_detail TEXT NOT NULL DEFAULT '',
            updated_at DATETIME DEFAULT CURRENT_TIMESTAMP
        )
        "#,
    )
    .execute(db)
    .await?;

    // Backfill folder_name for any existing series that have an empty value.
    // Uses English title → Romaji → title, with filesystem-unsafe chars sanitized.
    let empty_folder_rows: Vec<(i64, String, String, String)> = sqlx::query_as(
        "SELECT id, title, title_romaji, title_english FROM series WHERE folder_name = '' OR folder_name IS NULL",
    )
    .fetch_all(db)
    .await
    .unwrap_or_default();

    for (id, title, title_romaji, title_english) in &empty_folder_rows {
        let best = if !title_english.is_empty() {
            title_english.as_str()
        } else if !title_romaji.is_empty() {
            title_romaji.as_str()
        } else {
            title.as_str()
        };
        let folder = crate::services::media::sanitize_folder_name(best);
        if !folder.is_empty() {
            let _ = sqlx::query("UPDATE series SET folder_name = ? WHERE id = ?")
                .bind(&folder)
                .bind(id)
                .execute(db)
                .await;
        }
    }

    // #23 — Custom search tokens + release-group restriction.
    // Global defaults live on `config`; per-series overrides live on
    // `series`. Both are plain text for flexibility — the user pastes
    // whatever Nyaa query syntax they want (`bd`, `1080p`, `h.264`)
    // and the nyaa query builder appends it verbatim after the title.
    // Empty string means "no override / no tokens", which is the
    // existing behavior.
    sqlx::query("ALTER TABLE config ADD COLUMN default_custom_query_tokens TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();
    sqlx::query("ALTER TABLE series ADD COLUMN custom_query_tokens TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();

    // Rename `*_to_group` → `*_to_uploader` with full recovery for the
    // DBs that landed in a half-migrated state from PR #37's first-
    // pass migration (which added the new column before renaming, so
    // `ADD` succeeded and the subsequent `RENAME` failed as "duplicate
    // column" — leaving the user's uploader value stranded in an
    // orphan legacy column alongside an empty new one).
    //
    // `reconcile_restrict_to_group_rename` handles the four possible
    // states — legacy-only, new-only, both, neither — in the order
    // that makes each a one-shot forward move without data loss.
    reconcile_restrict_to_group_rename(db, "config", "default_restrict_to_group", "default_restrict_to_uploader").await;
    reconcile_restrict_to_group_rename(db, "series", "restrict_to_group", "restrict_to_uploader").await;

    // #30 — Cumulative episode count of the shortest TV-format PREQUEL
    // chain. Used at search time to accept absolute-numbered Nyaa
    // releases against a relative-numbered AL cour target (e.g. target
    // JJK S3 E9 matches "[SubsPlease] Jujutsu Kaisen - 56" because
    // S1(24) + S2(23) = 47 and 47 + 9 = 56). Populated by
    // `metadata_sync::refresh_series_metadata` after the relation graph
    // has been cached, and again at library-add time so first-searches
    // don't wait for the next refresh sweep.
    sqlx::query("ALTER TABLE series ADD COLUMN cumulative_prior_episodes INTEGER NOT NULL DEFAULT 0")
        .execute(db)
        .await
        .ok();

    // SeaDex lookup cache, persisted across restarts. The in-memory cache
    // in `services::auto_search` already de-duplicates within a process,
    // but cold-boot RSS sweeps were re-fetching every series's SeaDex
    // entry on the first 24h cycle after every restart. Persisting the
    // 24h window to SQLite means a restart picks up where the cache left
    // off. Error-marked entries (5-min TTL) are deliberately NOT persisted
    // — they reflect upstream health, which restart should re-probe.
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS seadex_lookup_cache (
            anilist_id INTEGER PRIMARY KEY,
            payload_json TEXT NOT NULL,
            cached_at INTEGER NOT NULL
        )
        "#,
    )
    .execute(db)
    .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: a legacy install has `episode_grab_history.torrent_name`
    /// populated with the release title for every historical grab. The
    /// column-rename path previously ran
    ///   RENAME torrent_name → file_name (.ok())
    ///   ADD COLUMN file_name TEXT (.ok())
    /// back-to-back. If the RENAME failed for any reason (DB lock, FK
    /// quirk, I/O hiccup) the subsequent ADD silently created an empty
    /// `file_name` column on top of intact `torrent_name` data and every
    /// prior row's release title was effectively lost — `.ok()` on both
    /// statements meant no log line, no error, nothing to alert the
    /// operator.
    ///
    /// This test exercises the happy path: pre-create the table with the
    /// legacy schema, stuff a row into it, run migrate, confirm the row's
    /// file_name now carries what torrent_name held.
    #[tokio::test]
    async fn migrate_renames_legacy_torrent_name_to_file_name_preserving_data() {
        let db = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite");

        // Pre-create episode_grab_history with the legacy schema (column
        // is `torrent_name`, no `file_name`). CREATE TABLE IF NOT EXISTS
        // inside migrate() will then skip this table and migrate() will
        // reach the rename branch under test.
        sqlx::query(
            r#"
            CREATE TABLE episode_grab_history (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                series_id INTEGER NOT NULL,
                episode_number INTEGER NOT NULL,
                quality_tag TEXT NOT NULL DEFAULT '',
                release_title TEXT NOT NULL DEFAULT '',
                release_group TEXT NOT NULL DEFAULT '',
                torrent_name TEXT NOT NULL DEFAULT '',
                state TEXT NOT NULL DEFAULT 'grabbed',
                grabbed_at DATETIME DEFAULT CURRENT_TIMESTAMP
            )
            "#,
        )
        .execute(&db)
        .await
        .expect("pre-create legacy table");

        sqlx::query(
            "INSERT INTO episode_grab_history
                 (series_id, episode_number, quality_tag, release_title, release_group, torrent_name)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(1_i64)
        .bind(1_i32)
        .bind("WEBDL-1080p")
        .bind("[Group] Show - 01 [WEB-DL 1080p].mkv")
        .bind("Group")
        .bind("[Group] Show - 01 [WEB-DL 1080p].mkv")
        .execute(&db)
        .await
        .expect("insert legacy row");

        migrate(&db).await.expect("migrate must succeed");

        // After migrate, the data that lived in `torrent_name` must now be
        // in `file_name`. If the rename failed and the defensive ADD
        // branch ran instead, this value would be empty (the default).
        let file_name: String = sqlx::query_scalar(
            "SELECT file_name FROM episode_grab_history WHERE id = 1",
        )
        .fetch_one(&db)
        .await
        .expect("row 1 must still exist");
        assert_eq!(file_name, "[Group] Show - 01 [WEB-DL 1080p].mkv");

        // And the old column should no longer be there (RENAME moved it,
        // didn't duplicate it).
        assert!(!column_exists(&db, "episode_grab_history", "torrent_name").await);
    }

    /// PR #37's first migration attempt ran ADD-then-RENAME for the
    /// `restrict_to_group` → `restrict_to_uploader` rename, so any DB
    /// that booted that build ended up with both columns: the legacy
    /// one populated with the user's uploader value, the new one
    /// empty. The fix for that ships the recovery pass tested here
    /// — on a DB with both columns present, the user's value must
    /// land in the new column and the legacy column must drop.
    #[tokio::test]
    async fn reconcile_rename_recovers_half_migrated_restrict_to_group() {
        let db = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite");

        // Simulate the PR #37 v1 broken state: pre-create `config`
        // with BOTH columns, legacy populated, new empty.
        sqlx::query(
            r#"CREATE TABLE config (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                default_restrict_to_group     TEXT NOT NULL DEFAULT '',
                default_restrict_to_uploader  TEXT NOT NULL DEFAULT ''
            )"#,
        )
        .execute(&db)
        .await
        .expect("create legacy config");
        sqlx::query(
            "INSERT INTO config (id, default_restrict_to_group, default_restrict_to_uploader)
             VALUES (1, 'SubsPlease', '')",
        )
        .execute(&db)
        .await
        .expect("seed legacy row");

        reconcile_restrict_to_group_rename(
            &db,
            "config",
            "default_restrict_to_group",
            "default_restrict_to_uploader",
        )
        .await;

        let uploader: String = sqlx::query_scalar(
            "SELECT default_restrict_to_uploader FROM config WHERE id = 1",
        )
        .fetch_one(&db)
        .await
        .expect("fetch uploader");
        assert_eq!(
            uploader, "SubsPlease",
            "user's uploader value must be copied forward into the new column"
        );

        // Legacy column should be gone after the reconcile.
        assert!(
            !column_exists(&db, "config", "default_restrict_to_group").await,
            "orphan legacy column must be dropped once data has been copied"
        );
    }

    /// Legacy-only state (DB migrated from a build predating PR #37):
    /// rename the column in place, keep the data.
    #[tokio::test]
    async fn reconcile_rename_brings_legacy_column_forward() {
        let db = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite");

        sqlx::query(
            r#"CREATE TABLE config (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                default_restrict_to_group TEXT NOT NULL DEFAULT ''
            )"#,
        )
        .execute(&db)
        .await
        .expect("create legacy config");
        sqlx::query(
            "INSERT INTO config (id, default_restrict_to_group) VALUES (1, 'SubsPlease')",
        )
        .execute(&db)
        .await
        .expect("seed legacy row");

        reconcile_restrict_to_group_rename(
            &db,
            "config",
            "default_restrict_to_group",
            "default_restrict_to_uploader",
        )
        .await;

        let uploader: String = sqlx::query_scalar(
            "SELECT default_restrict_to_uploader FROM config WHERE id = 1",
        )
        .fetch_one(&db)
        .await
        .expect("fetch uploader");
        assert_eq!(uploader, "SubsPlease");
        assert!(!column_exists(&db, "config", "default_restrict_to_group").await);
    }

    /// Both columns, new column already populated — user's live value
    /// must win over the stale legacy value. Edge case: the old
    /// rename attempt was half-successful somehow (or a user
    /// manually edited the new column).
    #[tokio::test]
    async fn reconcile_rename_does_not_overwrite_populated_new_column() {
        let db = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite");

        sqlx::query(
            r#"CREATE TABLE config (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                default_restrict_to_group     TEXT NOT NULL DEFAULT '',
                default_restrict_to_uploader  TEXT NOT NULL DEFAULT ''
            )"#,
        )
        .execute(&db)
        .await
        .expect("create legacy config");
        sqlx::query(
            "INSERT INTO config (id, default_restrict_to_group, default_restrict_to_uploader)
             VALUES (1, 'StaleLegacy', 'LiveNew')",
        )
        .execute(&db)
        .await
        .expect("seed row");

        reconcile_restrict_to_group_rename(
            &db,
            "config",
            "default_restrict_to_group",
            "default_restrict_to_uploader",
        )
        .await;

        let uploader: String = sqlx::query_scalar(
            "SELECT default_restrict_to_uploader FROM config WHERE id = 1",
        )
        .fetch_one(&db)
        .await
        .expect("fetch uploader");
        assert_eq!(
            uploader, "LiveNew",
            "non-empty new column must not be overwritten by stale legacy"
        );
    }

    /// Fresh install — neither column exists yet. Reconcile must
    /// ADD the new column with the empty default.
    #[tokio::test]
    async fn reconcile_rename_adds_new_column_on_fresh_install() {
        let db = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite");

        sqlx::query(
            r#"CREATE TABLE config (
                id INTEGER PRIMARY KEY CHECK (id = 1)
            )"#,
        )
        .execute(&db)
        .await
        .expect("create bare config");
        sqlx::query("INSERT INTO config (id) VALUES (1)")
            .execute(&db)
            .await
            .expect("seed empty row");

        reconcile_restrict_to_group_rename(
            &db,
            "config",
            "default_restrict_to_group",
            "default_restrict_to_uploader",
        )
        .await;

        assert!(column_exists(&db, "config", "default_restrict_to_uploader").await);
        let uploader: String = sqlx::query_scalar(
            "SELECT default_restrict_to_uploader FROM config WHERE id = 1",
        )
        .fetch_one(&db)
        .await
        .expect("fetch uploader");
        assert_eq!(uploader, "", "fresh install starts with the default empty");
    }
}
