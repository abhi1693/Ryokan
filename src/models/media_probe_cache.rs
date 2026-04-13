//! Cache of ffprobe output keyed by `(path, mtime, size)`.
//!
//! Backs Layer 5 of the classification pipeline. ffprobe is a shell-out and
//! we don't want to re-run it on every library scan or re-classification —
//! the output is deterministic for a given file, so we cache the raw JSON
//! blob and invalidate by comparing the file's current mtime and size
//! against the cached values. That three-tuple is also how Sonarr and the
//! ffprobe-based tools in the broader *-arr ecosystem invalidate their
//! caches, so it's well-understood by users.
//!
//! Writes are best-effort: a failed cache write never breaks the
//! classification path. On a cache hit the probe JSON is returned verbatim
//! and re-parsed by the Layer 5 scanner, so the cache layer knows nothing
//! about stream codecs or PGS tracks.

use sqlx::{Row, SqlitePool};

pub async fn migrate(db: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS media_probe_cache (
            path        TEXT PRIMARY KEY,
            mtime       INTEGER NOT NULL,
            size        INTEGER NOT NULL,
            probe_json  TEXT NOT NULL,
            cached_at   DATETIME DEFAULT CURRENT_TIMESTAMP
        )
        "#,
    )
    .execute(db)
    .await?;
    Ok(())
}

/// Look up a cached probe JSON for `path`. Returns `None` when the row is
/// missing, when mtime/size no longer match (file was modified), or when
/// the DB lookup fails — Layer 5 falls back to a live shell-out in every
/// such case.
pub async fn get(db: &SqlitePool, path: &str, mtime: i64, size: i64) -> Option<String> {
    if path.is_empty() {
        return None;
    }
    let row = sqlx::query(
        "SELECT mtime, size, probe_json FROM media_probe_cache WHERE path = ?",
    )
    .bind(path)
    .fetch_optional(db)
    .await
    .ok()
    .flatten()?;

    let cached_mtime: i64 = row.get("mtime");
    let cached_size: i64 = row.get("size");
    if cached_mtime != mtime || cached_size != size {
        return None;
    }
    Some(row.get::<String, _>("probe_json"))
}

/// Insert or replace a probe JSON blob. Errors are swallowed so a cache
/// write failure never bubbles up into the classifier. The next lookup
/// will simply miss and Layer 5 will re-probe.
pub async fn upsert(db: &SqlitePool, path: &str, mtime: i64, size: i64, probe_json: &str) {
    if path.is_empty() {
        return;
    }
    let _ = sqlx::query(
        r#"INSERT INTO media_probe_cache (path, mtime, size, probe_json)
           VALUES (?, ?, ?, ?)
           ON CONFLICT(path) DO UPDATE SET
               mtime = excluded.mtime,
               size = excluded.size,
               probe_json = excluded.probe_json,
               cached_at = CURRENT_TIMESTAMP"#,
    )
    .bind(path)
    .bind(mtime)
    .bind(size)
    .bind(probe_json)
    .execute(db)
    .await;
}
