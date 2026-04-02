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

    // Add the column under its old name for DBs that predate the Kitsu migration,
    // then rename it to force_kitsu_fallback for DBs that still have the old name.
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

    Ok(())
}


