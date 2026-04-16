use sqlx::{Row, SqlitePool};
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct ArtworkEntry {
    pub cache_key: String,
    pub local_path: String,
    pub content_type: String,
    pub last_write: i64,
}

pub async fn upsert_blob(
    db: &SqlitePool,
    blob_hash: &str,
    local_path: &str,
    content_type: &str,
    byte_size: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO image_blobs (blob_hash, local_path, content_type, byte_size, created_at)
        VALUES (?, ?, ?, ?, CURRENT_TIMESTAMP)
        ON CONFLICT(blob_hash) DO UPDATE SET
            local_path = excluded.local_path,
            content_type = excluded.content_type,
            byte_size = excluded.byte_size
        "#,
    )
    .bind(blob_hash)
    .bind(local_path)
    .bind(content_type)
    .bind(byte_size)
    .execute(db)
    .await?;
    Ok(())
}

/// Returns the `local_path` stored for a given blob hash, or `None` if no
/// such blob row exists. Callers use this both as an existence check and
/// to verify the on-disk file is still where the DB says it is — older
/// builds wrote relative paths to this column, which break when the
/// runtime CWD changes. See `services::artwork::cache_image` for the
/// self-heal path.
pub async fn get_blob_path(db: &SqlitePool, blob_hash: &str) -> Result<Option<String>, sqlx::Error> {
    let row = sqlx::query(r#"SELECT local_path FROM image_blobs WHERE blob_hash = ?"#)
        .bind(blob_hash)
        .fetch_optional(db)
        .await?;

    Ok(row.map(|r| r.get::<String, _>("local_path")))
}

/// Payload for `upsert_ref`. Named fields so callers can't swap the
/// four `&str` arguments (cache_key / parent_kind / image_kind /
/// source_url) without the compiler noticing — swapping two of those
/// silently corrupts the cache index, and the call arrives in a
/// background task where the error wouldn't surface until someone
/// notices wrong artwork.
pub struct RefUpsert<'a> {
    pub cache_key: &'a str,
    pub parent_kind: &'a str,
    pub parent_id: Option<i64>,
    pub image_kind: &'a str,
    pub source_url: &'a str,
    pub blob_hash: &'a str,
    pub last_write: i64,
}

pub async fn upsert_ref(
    db: &SqlitePool,
    r: RefUpsert<'_>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO image_refs (
            cache_key, parent_kind, parent_id, image_kind,
            source_url, blob_hash, last_write, cached_at
        )
        VALUES (?, ?, ?, ?, ?, ?, ?, CURRENT_TIMESTAMP)
        ON CONFLICT(cache_key) DO UPDATE SET
            parent_kind = excluded.parent_kind,
            parent_id = excluded.parent_id,
            image_kind = excluded.image_kind,
            source_url = excluded.source_url,
            blob_hash = excluded.blob_hash,
            last_write = excluded.last_write,
            cached_at = CURRENT_TIMESTAMP
        "#,
    )
    .bind(r.cache_key)
    .bind(r.parent_kind)
    .bind(r.parent_id)
    .bind(r.image_kind)
    .bind(r.source_url)
    .bind(r.blob_hash)
    .bind(r.last_write)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn get(db: &SqlitePool, cache_key: &str) -> Result<Option<ArtworkEntry>, sqlx::Error> {
    let row = sqlx::query(
        r#"
        SELECT ir.cache_key, ib.local_path, ib.content_type, ir.last_write
        FROM image_refs ir
        JOIN image_blobs ib ON ib.blob_hash = ir.blob_hash
        WHERE ir.cache_key = ?
        "#,
    )
    .bind(cache_key)
    .fetch_optional(db)
    .await?;

    Ok(row.map(|row| ArtworkEntry {
        cache_key: row.get("cache_key"),
        local_path: row.get("local_path"),
        content_type: row.get("content_type"),
        last_write: row.get("last_write"),
    }))
}

pub async fn get_local_url(db: &SqlitePool, cache_key: &str) -> Result<Option<String>, sqlx::Error> {
    Ok(get(db, cache_key)
        .await?
        .map(|e| format!("/media/art/{}?v={}", e.cache_key, e.last_write)))
}

/// Batch variant of `get_local_url`: fetches `/media/art/...` URLs for a
/// set of cache keys in a single SQL round trip. Used by list pages
/// (library index, needs-review, etc.) that previously fired one serial
/// query per row — for a 200-series library that was 200 sequential DB
/// hits just to render the covers.
///
/// Keys with no cached row are omitted from the returned map; callers
/// should fall back to the source URL in that case.
pub async fn get_local_urls_batch(
    db: &SqlitePool,
    cache_keys: &[String],
) -> Result<HashMap<String, String>, sqlx::Error> {
    if cache_keys.is_empty() {
        return Ok(HashMap::new());
    }

    // Build the `IN (?, ?, ...)` placeholder list at runtime. sqlx doesn't
    // expand slice bindings on SQLite, so we splice the placeholders into
    // the SQL string and bind each key individually. The key set comes
    // from trusted server-side format strings (`series-<id>-cover`), not
    // user input, so there's no injection surface here.
    let placeholders = std::iter::repeat_n("?", cache_keys.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        r#"
        SELECT ir.cache_key, ir.last_write
        FROM image_refs ir
        JOIN image_blobs ib ON ib.blob_hash = ir.blob_hash
        WHERE ir.cache_key IN ({})
        "#,
        placeholders
    );
    let mut query = sqlx::query(&sql);
    for key in cache_keys {
        query = query.bind(key);
    }

    let rows = query.fetch_all(db).await?;
    let mut out = HashMap::with_capacity(rows.len());
    for row in rows {
        let key: String = row.get("cache_key");
        let last_write: i64 = row.get("last_write");
        out.insert(key.clone(), format!("/media/art/{}?v={}", key, last_write));
    }
    Ok(out)
}

/// Two-step prune of the artwork cache, called from the hourly cleanup
/// task. Returns `(refs_deleted, blobs_deleted)`. Without this the
/// `image_refs` and `image_blobs` tables — and the on-disk blob files
/// they point at — only ever grow: removing a series leaves orphan
/// rows, renaming a cover URL leaves the old blob behind, etc.
///
/// Step 1 — drop refs whose parent series no longer exists.
/// Step 2 — drop blobs (and the backing files) that no ref references
/// AND that haven't been written in `min_age_days` (the age gate
/// covers the small race in cache_image where upsert_blob runs before
/// upsert_ref — we don't want a concurrent prune to delete a blob
/// that's about to get its ref written).
pub async fn cleanup_orphans(
    db: &SqlitePool,
    min_age_days: i64,
) -> Result<(u64, u64), sqlx::Error> {
    // Step 1: orphan refs. Only series-keyed refs are pruned here —
    // any other parent_kind is left alone since we don't own its
    // identity table.
    let refs_deleted = sqlx::query(
        "DELETE FROM image_refs
         WHERE parent_kind IN ('series', 'series_relation')
           AND parent_id IS NOT NULL
           AND parent_id NOT IN (SELECT id FROM series)",
    )
    .execute(db)
    .await?
    .rows_affected();

    // Step 2: orphan blobs. Read paths first so we can delete the
    // on-disk files, then delete the rows. The age gate uses
    // `created_at < datetime('now', '-Nd days')`.
    let age_filter = format!("-{} days", min_age_days);
    let orphan_rows = sqlx::query(
        r#"SELECT blob_hash, local_path FROM image_blobs b
           WHERE NOT EXISTS (SELECT 1 FROM image_refs r WHERE r.blob_hash = b.blob_hash)
             AND b.created_at < datetime('now', ?)"#,
    )
    .bind(&age_filter)
    .fetch_all(db)
    .await?;

    let blobs_deleted = orphan_rows.len() as u64;
    for row in &orphan_rows {
        let local_path: String = row.get("local_path");
        if !local_path.is_empty() {
            let _ = std::fs::remove_file(&local_path);
        }
    }
    if blobs_deleted > 0 {
        sqlx::query(
            r#"DELETE FROM image_blobs
               WHERE NOT EXISTS (SELECT 1 FROM image_refs r WHERE r.blob_hash = image_blobs.blob_hash)
                 AND created_at < datetime('now', ?)"#,
        )
        .bind(&age_filter)
        .execute(db)
        .await?;
    }

    Ok((refs_deleted, blobs_deleted))
}
