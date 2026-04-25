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
    let file_name: String =
        sqlx::query_scalar("SELECT file_name FROM episode_grab_history WHERE id = 1")
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

    reconcile_column_rename(
        &db,
        "config",
        "default_restrict_to_group",
        "default_restrict_to_uploader",
    )
    .await;

    let uploader: String =
        sqlx::query_scalar("SELECT default_restrict_to_uploader FROM config WHERE id = 1")
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
    sqlx::query("INSERT INTO config (id, default_restrict_to_group) VALUES (1, 'SubsPlease')")
        .execute(&db)
        .await
        .expect("seed legacy row");

    reconcile_column_rename(
        &db,
        "config",
        "default_restrict_to_group",
        "default_restrict_to_uploader",
    )
    .await;

    let uploader: String =
        sqlx::query_scalar("SELECT default_restrict_to_uploader FROM config WHERE id = 1")
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

    reconcile_column_rename(
        &db,
        "config",
        "default_restrict_to_group",
        "default_restrict_to_uploader",
    )
    .await;

    let uploader: String =
        sqlx::query_scalar("SELECT default_restrict_to_uploader FROM config WHERE id = 1")
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

    reconcile_column_rename(
        &db,
        "config",
        "default_restrict_to_group",
        "default_restrict_to_uploader",
    )
    .await;

    assert!(column_exists(&db, "config", "default_restrict_to_uploader").await);
    let uploader: String =
        sqlx::query_scalar("SELECT default_restrict_to_uploader FROM config WHERE id = 1")
            .fetch_one(&db)
            .await
            .expect("fetch uploader");
    assert_eq!(uploader, "", "fresh install starts with the default empty");
}

// ─── Idempotency + schema shape (PR 6) ──────────────────────────

async fn fresh_migrated_pool() -> SqlitePool {
    let pool = SqlitePool::connect("sqlite::memory:")
        .await
        .expect("in-memory sqlite");
    migrate(&pool).await.expect("migrate must succeed");
    pool
}

async fn table_exists(db: &SqlitePool, table: &str) -> bool {
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?",
    )
    .bind(table)
    .fetch_one(db)
    .await
    .unwrap_or(0)
        > 0
}

#[tokio::test]
async fn migrate_on_empty_db_succeeds() {
    let db = SqlitePool::connect("sqlite::memory:")
        .await
        .expect("in-memory sqlite");
    migrate(&db).await.expect("first migrate should succeed");
}

#[tokio::test]
async fn migrate_is_idempotent_on_second_invocation() {
    // The CREATE TABLE IF NOT EXISTS + ALTER TABLE … ADD COLUMN
    // with .ok() pattern is the whole point of in-code migrations
    // — running migrate() twice on the same pool must not error.
    // A refactor that swaps in a stricter IF NOT EXISTS variant
    // (or forgets .ok() on a new ALTER) would trip this test.
    let db = SqlitePool::connect("sqlite::memory:")
        .await
        .expect("in-memory sqlite");
    migrate(&db).await.expect("first migrate");
    migrate(&db)
        .await
        .expect("second migrate must also succeed");
}

#[tokio::test]
async fn migrate_creates_core_tables() {
    // Spot-check the load-bearing tables — adding a new one is
    // fine, but silently dropping one of these is the kind of
    // regression that lives undetected until a user reports data
    // loss. Limit the list to a handful of foundational ones
    // rather than every single table to avoid churn noise when
    // schema evolves.
    let db = fresh_migrated_pool().await;
    for table in [
        "users",
        "sessions",
        "config",
        "series",
        "grabbed_torrents",
        "grabbed_torrent_series",
        "episode_quality_tags",
        "episode_grab_history",
        "rss_seen",
        "logs",
    ] {
        assert!(
            table_exists(&db, table).await,
            "core table `{table}` missing after migrate"
        );
    }
}

#[tokio::test]
async fn foreign_keys_pragma_is_enabled_after_migrate() {
    // sqlx enables `PRAGMA foreign_keys = ON` by default, but
    // that default is a design dependency several migrations and
    // models rely on (rss_seen NO ACTION handling, series
    // CASCADE, etc.). Pinning here so a future sqlx upgrade that
    // changed the default would fail this test loudly rather
    // than silently corrupting child-table state.
    let db = fresh_migrated_pool().await;
    let pragma: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
        .fetch_one(&db)
        .await
        .expect("PRAGMA foreign_keys should read");
    assert_eq!(pragma, 1, "foreign_keys pragma must be ON");
}

#[tokio::test]
async fn deleting_a_series_cascades_to_grabbed_torrents() {
    // Per the schema, grabbed_torrents.series_id has ON DELETE
    // CASCADE. Removing a series must take its grabs with it or
    // the DB ends up with orphaned grab rows that lookup paths
    // fail on.
    let db = fresh_migrated_pool().await;
    sqlx::query(
        "INSERT INTO series (anilist_id, title, title_romaji, folder_name) \
         VALUES (1, 'Show', 'Show', 'show')",
    )
    .execute(&db)
    .await
    .unwrap();
    let series_id: i64 = sqlx::query_scalar("SELECT id FROM series WHERE anilist_id = 1")
        .fetch_one(&db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO grabbed_torrents (series_id, hash, torrent_name, episode_numbers, state) \
         VALUES (?, 'h1', 'name', '[1]', 'pending')",
    )
    .bind(series_id)
    .execute(&db)
    .await
    .unwrap();

    sqlx::query("DELETE FROM series WHERE id = ?")
        .bind(series_id)
        .execute(&db)
        .await
        .expect("series delete should succeed");
    let remaining: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM grabbed_torrents WHERE series_id = ?")
            .bind(series_id)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(
        remaining, 0,
        "grabbed_torrents row must CASCADE with series"
    );
}

#[tokio::test]
async fn deleting_a_series_cascades_to_episode_quality_tags() {
    let db = fresh_migrated_pool().await;
    sqlx::query(
        "INSERT INTO series (anilist_id, title, title_romaji, folder_name) \
         VALUES (2, 'Show', 'Show', 'show')",
    )
    .execute(&db)
    .await
    .unwrap();
    let series_id: i64 = sqlx::query_scalar("SELECT id FROM series WHERE anilist_id = 2")
        .fetch_one(&db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO episode_quality_tags (series_id, episode_number, quality_tag) \
         VALUES (?, 1, 'WEBDL-1080p')",
    )
    .bind(series_id)
    .execute(&db)
    .await
    .unwrap();

    sqlx::query("DELETE FROM series WHERE id = ?")
        .bind(series_id)
        .execute(&db)
        .await
        .unwrap();
    let remaining: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM episode_quality_tags WHERE series_id = ?")
            .bind(series_id)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(remaining, 0);
}

#[tokio::test]
async fn rss_seen_is_no_action_not_cascade_on_series() {
    // The FK policy for `rss_seen.series_id` is deliberately
    // `NO ACTION`, not CASCADE — the audit trail survives a
    // series deletion. series::remove is responsible for
    // NULL-ing out rss_seen.series_id BEFORE the series row
    // delete to satisfy the FK constraint. This test exercises
    // the "survive the delete" half of that contract — setting
    // series_id = NULL first, then deleting the series row,
    // then confirming rss_seen still has its bookkeeping row.
    let db = fresh_migrated_pool().await;
    sqlx::query(
        "INSERT INTO series (anilist_id, title, title_romaji, folder_name) \
         VALUES (3, 'Show', 'Show', 'show')",
    )
    .execute(&db)
    .await
    .unwrap();
    let series_id: i64 = sqlx::query_scalar("SELECT id FROM series WHERE anilist_id = 3")
        .fetch_one(&db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO rss_seen (item_key, series_id, series_title) VALUES (?, ?, ?)")
        .bind("guid-keep")
        .bind(series_id)
        .bind("Show")
        .execute(&db)
        .await
        .unwrap();

    // Per series::remove: NULL out the FK first, THEN delete the
    // series row. Without this two-step, the DELETE errors on FK
    // constraint failure.
    sqlx::query("UPDATE rss_seen SET series_id = NULL WHERE series_id = ?")
        .bind(series_id)
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("DELETE FROM series WHERE id = ?")
        .bind(series_id)
        .execute(&db)
        .await
        .expect("delete after NULL-out should succeed");

    // The audit row survives — same guid, series_id now NULL,
    // series_title kept for reference.
    let remaining: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM rss_seen WHERE item_key = 'guid-keep'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(
        remaining, 1,
        "rss_seen audit row must survive series delete"
    );
}

#[tokio::test]
async fn direct_series_delete_without_null_out_fails_fk_constraint() {
    // The counter-test: attempting to delete a series without
    // first NULL-ing out `rss_seen.series_id` must fail with a
    // FK constraint error. Pins the invariant that series::remove
    // relies on — if a future refactor drops the NO ACTION policy
    // on rss_seen, this test catches it.
    let db = fresh_migrated_pool().await;
    sqlx::query(
        "INSERT INTO series (anilist_id, title, title_romaji, folder_name) \
         VALUES (4, 'Show', 'Show', 'show')",
    )
    .execute(&db)
    .await
    .unwrap();
    let series_id: i64 = sqlx::query_scalar("SELECT id FROM series WHERE anilist_id = 4")
        .fetch_one(&db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO rss_seen (item_key, series_id, series_title) VALUES (?, ?, ?)")
        .bind("guid-fail")
        .bind(series_id)
        .bind("Show")
        .execute(&db)
        .await
        .unwrap();

    let result = sqlx::query("DELETE FROM series WHERE id = ?")
        .bind(series_id)
        .execute(&db)
        .await;
    assert!(
        result.is_err(),
        "delete without NULL-out must fail FK constraint (got {result:?})"
    );
}

#[tokio::test]
async fn migrate_creates_schema_migrations_table() {
    let db = fresh_migrated_pool().await;
    // Not populated by `migrate()` directly — created on first
    // use by `ensure_schema_migrations_table` in group_source_map.
    // Run that seed path to ensure the table exists + is
    // writable.
    group_source_map::seed_defaults(&db)
        .await
        .expect("seed_defaults should succeed");
    assert!(
        table_exists(&db, "schema_migrations").await,
        "schema_migrations table should exist after seed pass"
    );
}

/// Stronger companion to `migrate_is_idempotent_on_second_invocation`
/// — that test only proves the second call doesn't *error*. This one
/// proves it doesn't silently *mutate* user-set values that the
/// initial migration backfilled.
///
/// The Jellyfin URL backfill is the load-bearing case: it derives
/// `jellyfin_url` from the legacy `jellyfin_host`/`jellyfin_port`/
/// `jellyfin_use_ssl` columns when `jellyfin_url` is empty. Without
/// the `WHERE jellyfin_url = ''` gate, a second migration call after
/// the user customized the derived URL would clobber it back to the
/// host-derived form. The gate is the actual idempotency guarantee
/// for every other UPDATE-style backfill in `migrate()`; this test
/// pins it for the most user-impactful one.
#[tokio::test]
async fn migrate_does_not_overwrite_user_values_on_second_invocation() {
    let db = SqlitePool::connect("sqlite::memory:")
        .await
        .expect("in-memory sqlite");
    migrate(&db).await.expect("first migrate");

    // migrate() doesn't seed the config row (`save_config` does that
    // on first settings-page write). Seed a minimal row directly so
    // the test exercises the user-customized-URL → second-migrate
    // path. Only the four jellyfin_* columns the backfill cares
    // about need real values.
    sqlx::query(
        "INSERT INTO config (id, jellyfin_url, jellyfin_host, jellyfin_port, jellyfin_use_ssl) \
         VALUES (1, 'https://my.real.jellyfin.example/jf', 'derived.example', '8096', 0)",
    )
    .execute(&db)
    .await
    .expect("seed config row with user-customized jellyfin_url");

    migrate(&db).await.expect("second migrate must succeed");

    let url: String = sqlx::query_scalar("SELECT jellyfin_url FROM config WHERE id = 1")
        .fetch_one(&db)
        .await
        .expect("read jellyfin_url back");
    assert_eq!(
        url, "https://my.real.jellyfin.example/jf",
        "second migrate must not overwrite the user's custom jellyfin_url"
    );
}

/// Pin behavior of the typed-rename helper for the
/// `force_tmdb_fallback` → `force_kitsu_fallback` recovery path.
/// PR #37's regression shape (ADD-then-RENAME with `.ok()`) used to
/// leave a stray INTEGER column alongside the new one on a post-
/// migrated install, with the user's enable/disable bit stranded in
/// either column depending on which migrate() build ran first.
/// `reconcile_column_rename_typed` collapses every starting state to
/// "new column exists, value preserved, legacy column dropped".
#[tokio::test]
async fn reconcile_typed_rename_recovers_half_migrated_integer_column() {
    let db = SqlitePool::connect("sqlite::memory:")
        .await
        .expect("in-memory sqlite");

    // Simulate the half-migrated state: BOTH columns present, user's
    // bit in the legacy one, new one still at the default 0.
    sqlx::query(
        r#"CREATE TABLE config (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            force_tmdb_fallback   INTEGER NOT NULL DEFAULT 0,
            force_kitsu_fallback  INTEGER NOT NULL DEFAULT 0
        )"#,
    )
    .execute(&db)
    .await
    .expect("create legacy config");
    sqlx::query(
        "INSERT INTO config (id, force_tmdb_fallback, force_kitsu_fallback) VALUES (1, 1, 0)",
    )
    .execute(&db)
    .await
    .expect("seed legacy bit");

    reconcile_column_rename_typed(
        &db,
        "config",
        "force_tmdb_fallback",
        "force_kitsu_fallback",
        "INTEGER NOT NULL DEFAULT 0",
        "= 0",
    )
    .await;

    let kitsu: i64 = sqlx::query_scalar("SELECT force_kitsu_fallback FROM config WHERE id = 1")
        .fetch_one(&db)
        .await
        .expect("read new column");
    assert_eq!(
        kitsu, 1,
        "user's enable bit must move from legacy → new column"
    );
    assert!(
        !column_exists(&db, "config", "force_tmdb_fallback").await,
        "legacy column must be dropped, not duplicated"
    );
}

/// The fresh-install path: neither column exists yet, the typed
/// helper must add the new one with the caller-supplied INTEGER
/// declaration (not the TEXT default the string-flavored helper
/// uses).
#[tokio::test]
async fn reconcile_typed_rename_adds_integer_column_on_fresh_install() {
    let db = SqlitePool::connect("sqlite::memory:")
        .await
        .expect("in-memory sqlite");
    sqlx::query("CREATE TABLE config (id INTEGER PRIMARY KEY CHECK (id = 1))")
        .execute(&db)
        .await
        .expect("create empty config");
    sqlx::query("INSERT INTO config (id) VALUES (1)")
        .execute(&db)
        .await
        .expect("seed config row");

    reconcile_column_rename_typed(
        &db,
        "config",
        "force_tmdb_fallback",
        "force_kitsu_fallback",
        "INTEGER NOT NULL DEFAULT 0",
        "= 0",
    )
    .await;

    assert!(column_exists(&db, "config", "force_kitsu_fallback").await);
    let kitsu: i64 = sqlx::query_scalar("SELECT force_kitsu_fallback FROM config WHERE id = 1")
        .fetch_one(&db)
        .await
        .expect("read new column");
    assert_eq!(kitsu, 0, "fresh-install default must be the integer 0");
    assert!(
        !column_exists(&db, "config", "force_tmdb_fallback").await,
        "fresh-install path must not create the legacy column",
    );
}
