//! Storage for user-defined Sonarr-v4-compatible Custom Formats.
//!
//! Two tables:
//!
//! - `custom_formats` — one row per CF, keyed by a stable auto-increment
//!   ID. Stores the full original Sonarr JSON (`json` column) so
//!   re-export round-trips byte-perfect into another Sonarr instance,
//!   plus the display name and the optional trash-guides ID for
//!   updates-from-upstream flows.
//!
//! - `custom_format_scores` — (custom_format_id, profile_id) → score.
//!   V1 hardcodes `profile_id = 1` everywhere; the column is only in
//!   place so V2 profile support is pure additive schema work (no data
//!   migration). `ON DELETE CASCADE` so a CF delete cleans up its own
//!   score row without a second query.
//!
//! The `custom_format_minimum_score` config row is added to the existing
//! typed `config` table in `models/mod.rs::migrate()` via a regular
//! `ALTER TABLE ... ADD COLUMN` — not here — because Ryokan's config is
//! a single typed row, not a key/value store.

use sqlx::{FromRow, Sqlite, SqlitePool, Transaction};

/// Single row in the `custom_formats` table joined with its V1-profile
/// score from `custom_format_scores`. A row with no score entry yet
/// returns `0` (via `COALESCE`) rather than `NULL`, so the settings UI
/// never has to handle a missing-score case.
#[derive(Debug, Clone, FromRow)]
pub struct CustomFormatRow {
    pub id: i64,
    pub name: String,
    pub trash_id: Option<String>,
    pub json: String,
    pub score: i64,
    /// Provenance of this row. One of `manual` (created via the upsert
    /// form), `import` (pasted into the Sonarr-export import box), or
    /// `defaults` (installed via the "Install Defaults" button). Used
    /// by the settings table to show a Source badge and by Reset to
    /// Defaults to target just the `defaults`-origin rows.
    pub origin: String,
    // Persisted timestamps. Not shown in the current UI but kept on the
    // struct so a future "sort by recently edited" view has the data
    // without a schema migration.
    #[allow(dead_code)]
    pub created_at: i64,
    #[allow(dead_code)]
    pub updated_at: i64,
}

/// Legal values for the `origin` column. Kept as bare &str rather
/// than an enum because the three values are only meaningful at the
/// handler boundary — the model layer just echoes the string into the
/// database.
pub const ORIGIN_MANUAL: &str = "manual";
pub const ORIGIN_IMPORT: &str = "import";
pub const ORIGIN_DEFAULTS: &str = "defaults";

pub async fn migrate(db: &SqlitePool) -> Result<(), sqlx::Error> {
    // Fresh installs land the full schema in one shot — including the
    // `origin` column that records provenance (`manual`/`import`/
    // `defaults`). `models/mod.rs::migrate` still runs a best-effort
    // `ALTER TABLE ADD COLUMN origin` after this for databases that
    // were created before the column shipped; that ALTER is idempotent
    // via `.ok()` and a no-op here.
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS custom_formats (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            name       TEXT NOT NULL UNIQUE,
            trash_id   TEXT,
            json       TEXT NOT NULL,
            origin     TEXT NOT NULL DEFAULT 'manual',
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL
        )
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS custom_format_scores (
            id               INTEGER PRIMARY KEY AUTOINCREMENT,
            custom_format_id INTEGER NOT NULL REFERENCES custom_formats(id) ON DELETE CASCADE,
            profile_id       INTEGER NOT NULL DEFAULT 1,
            score            INTEGER NOT NULL DEFAULT 0,
            UNIQUE(custom_format_id, profile_id)
        )
        "#,
    )
    .execute(db)
    .await?;

    Ok(())
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Return every CF row joined with its V1-profile score, ordered by
/// name for a stable settings-page layout.
pub async fn list_with_scores(db: &SqlitePool) -> Result<Vec<CustomFormatRow>, sqlx::Error> {
    sqlx::query_as::<_, CustomFormatRow>(
        r#"
        SELECT cf.id, cf.name, cf.trash_id, cf.json,
               COALESCE(cfs.score, 0) AS score,
               cf.origin,
               cf.created_at, cf.updated_at
        FROM custom_formats cf
        LEFT JOIN custom_format_scores cfs
               ON cfs.custom_format_id = cf.id
              AND cfs.profile_id = 1
        ORDER BY cf.name COLLATE NOCASE
        "#,
    )
    .fetch_all(db)
    .await
}

/// Fetch a single CF by id, joined with its V1-profile score. Used to
/// prefill the settings-page edit form when `?edit_id=N` is present.
pub async fn get_by_id(db: &SqlitePool, id: i64) -> Result<Option<CustomFormatRow>, sqlx::Error> {
    sqlx::query_as::<_, CustomFormatRow>(
        r#"
        SELECT cf.id, cf.name, cf.trash_id, cf.json,
               COALESCE(cfs.score, 0) AS score,
               cf.origin,
               cf.created_at, cf.updated_at
        FROM custom_formats cf
        LEFT JOIN custom_format_scores cfs
               ON cfs.custom_format_id = cf.id
              AND cfs.profile_id = 1
        WHERE cf.id = ?
        "#,
    )
    .bind(id)
    .fetch_optional(db)
    .await
}

/// Insert a new CF row and its V1-profile score in a single transaction.
/// Returns the newly-assigned row id so the caller can redirect straight
/// to the edit view. `origin` records provenance — callers should pass
/// one of the `ORIGIN_*` constants.
pub async fn insert(
    db: &SqlitePool,
    name: &str,
    trash_id: Option<&str>,
    json: &str,
    score: i32,
    origin: &str,
) -> Result<i64, sqlx::Error> {
    let mut tx = db.begin().await?;
    let id = insert_with_tx(&mut tx, name, trash_id, json, score, origin).await?;
    tx.commit().await?;
    Ok(id)
}

/// Transaction-scoped variant of [`insert`] for callers that need to
/// bundle multiple CF operations (e.g. the Reset Defaults handler's
/// `DELETE defaults` + re-`INSERT` loop) into a single atomic unit.
/// Does NOT commit — the caller owns transaction lifecycle.
pub async fn insert_with_tx(
    tx: &mut Transaction<'_, Sqlite>,
    name: &str,
    trash_id: Option<&str>,
    json: &str,
    score: i32,
    origin: &str,
) -> Result<i64, sqlx::Error> {
    let now = now_unix();
    let res = sqlx::query(
        r#"
        INSERT INTO custom_formats (name, trash_id, json, origin, created_at, updated_at)
        VALUES (?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind(name)
    .bind(trash_id)
    .bind(json)
    .bind(origin)
    .bind(now)
    .bind(now)
    .execute(&mut **tx)
    .await?;

    let id = res.last_insert_rowid();

    sqlx::query(
        r#"
        INSERT INTO custom_format_scores (custom_format_id, profile_id, score)
        VALUES (?, 1, ?)
        "#,
    )
    .bind(id)
    .bind(score as i64)
    .execute(&mut **tx)
    .await?;

    Ok(id)
}

/// Update an existing CF row and its V1-profile score. The score row
/// uses `INSERT ... ON CONFLICT` rather than a plain `UPDATE` so an old
/// row without a score entry still gets one on first edit.
pub async fn update(
    db: &SqlitePool,
    id: i64,
    name: &str,
    trash_id: Option<&str>,
    json: &str,
    score: i32,
) -> Result<(), sqlx::Error> {
    let now = now_unix();
    let mut tx = db.begin().await?;

    sqlx::query(
        r#"
        UPDATE custom_formats
        SET name = ?, trash_id = ?, json = ?, updated_at = ?
        WHERE id = ?
        "#,
    )
    .bind(name)
    .bind(trash_id)
    .bind(json)
    .bind(now)
    .bind(id)
    .execute(&mut *tx)
    .await?;

    sqlx::query(
        r#"
        INSERT INTO custom_format_scores (custom_format_id, profile_id, score)
        VALUES (?, 1, ?)
        ON CONFLICT(custom_format_id, profile_id) DO UPDATE SET score = excluded.score
        "#,
    )
    .bind(id)
    .bind(score as i64)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(())
}

/// Delete a CF row. The `ON DELETE CASCADE` on `custom_format_scores`
/// drops the score row automatically.
pub async fn delete(db: &SqlitePool, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM custom_formats WHERE id = ?")
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

/// Delete every CF row whose origin is `defaults`, within a
/// caller-supplied transaction. Used by the Reset to Defaults handler
/// to wipe just the bundled set before reinstalling from disk, inside
/// the same transaction as the reinstall so a mid-loop install failure
/// rolls the whole operation back. User-authored (`manual`) and
/// imported (`import`) rows are left untouched — that's the whole
/// point of the origin column. Does NOT commit — the caller owns
/// transaction lifecycle. Returns the number of rows that were dropped.
pub async fn delete_defaults_with_tx(tx: &mut Transaction<'_, Sqlite>) -> Result<u64, sqlx::Error> {
    let res = sqlx::query("DELETE FROM custom_formats WHERE origin = ?")
        .bind(ORIGIN_DEFAULTS)
        .execute(&mut **tx)
        .await?;
    Ok(res.rows_affected())
}
