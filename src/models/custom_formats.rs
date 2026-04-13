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

use sqlx::SqlitePool;

pub async fn migrate(db: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS custom_formats (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            name       TEXT NOT NULL UNIQUE,
            trash_id   TEXT,
            json       TEXT NOT NULL,
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
