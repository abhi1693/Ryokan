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

use sqlx::SqlitePool;

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

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_grabbed_torrents_hash ON grabbed_torrents (hash) WHERE hash != ''")
        .execute(db)
        .await?;

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

    Ok(())
}


