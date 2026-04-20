//! Cache of torrent description bodies scraped from Nyaa `/view/{id}` pages.
//!
//! Backs Layer 2 of the classification pipeline. When Layer 1 (filename) and
//! Layer 3 (release group) fail to produce a confident source decision, the
//! classifier falls through to fetching the full Nyaa listing and scanning
//! the description for structured source metadata. Those fetches are
//! rate-limited to one per second, so caching the extracted description body
//! keyed by `info_hash` keeps repeated classifications of the same torrent
//! (RSS polling, re-scoring, upgrade detection) off the network entirely.
//!
//! Rows are keyed on `info_hash` (content-addressed) so they remain valid
//! as long as the torrent exists. The cache has no functional TTL, but the
//! hourly cleanup task calls [`cleanup`] to prune rows whose `cached_at` is
//! older than 90 days. `cached_at` is only refreshed on a live fetch (cache
//! miss path); cache hits leave it alone, so the sweep evicts any row that
//! hasn't triggered a fresh network fetch in 90 days. The consequence is a
//! single forced re-fetch on the next access, never data loss — the worst
//! case is that one Nyaa request gets re-paid for a torrent that's still
//! being actively re-classified from the cache.

use sqlx::{Row, SqlitePool};

pub async fn migrate(db: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS nyaa_description_cache (
            info_hash   TEXT PRIMARY KEY COLLATE NOCASE,
            description TEXT NOT NULL DEFAULT '',
            cached_at   DATETIME DEFAULT CURRENT_TIMESTAMP
        )
        "#,
    )
    .execute(db)
    .await?;
    Ok(())
}

/// Look up a cached description body by torrent info_hash. Returns `None`
/// when there's no row or the DB lookup fails — Layer 2 degrades to a live
/// fetch in either case.
pub async fn get(db: &SqlitePool, info_hash: &str) -> Option<String> {
    let trimmed = info_hash.trim();
    if trimmed.is_empty() {
        return None;
    }
    let row = sqlx::query("SELECT description FROM nyaa_description_cache WHERE info_hash = ?")
        .bind(trimmed)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()?;
    Some(row.get::<String, _>("description"))
}

/// Insert or replace a cached description body. Errors are ignored — cache
/// writes should never break the classification path, and the next lookup
/// will simply miss and trigger another fetch.
pub async fn upsert(db: &SqlitePool, info_hash: &str, description: &str) {
    let trimmed = info_hash.trim();
    if trimmed.is_empty() {
        return;
    }
    let _ = sqlx::query(
        r#"INSERT INTO nyaa_description_cache (info_hash, description)
           VALUES (?, ?)
           ON CONFLICT(info_hash) DO UPDATE SET
               description = excluded.description,
               cached_at = CURRENT_TIMESTAMP"#,
    )
    .bind(trimmed)
    .bind(description)
    .execute(db)
    .await;
}

/// Prune cache rows whose `cached_at` is older than `max_age_days`. Called
/// from the hourly cleanup task so long-running instances don't accumulate
/// description bodies for torrents that haven't been touched in months.
/// Active torrents (RSS sync re-hits them) get their `cached_at` refreshed
/// by `upsert`, so this only evicts rows that truly went cold.
pub async fn cleanup(db: &SqlitePool, max_age_days: i32) -> Result<u64, sqlx::Error> {
    let cutoff = format!("-{} days", max_age_days);
    let res =
        sqlx::query("DELETE FROM nyaa_description_cache WHERE cached_at < datetime('now', ?)")
            .bind(cutoff)
            .execute(db)
            .await?;
    Ok(res.rows_affected())
}
