use sqlx::{Row, SqlitePool};

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

pub async fn has_blob(db: &SqlitePool, blob_hash: &str) -> Result<bool, sqlx::Error> {
    let row = sqlx::query(r#"SELECT 1 FROM image_blobs WHERE blob_hash = ?"#)
        .bind(blob_hash)
        .fetch_optional(db)
        .await?;

    Ok(row.is_some())
}

pub async fn upsert_ref(
    db: &SqlitePool,
    cache_key: &str,
    parent_kind: &str,
    parent_id: Option<i64>,
    image_kind: &str,
    source_url: &str,
    blob_hash: &str,
    last_write: i64,
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
    .bind(cache_key)
    .bind(parent_kind)
    .bind(parent_id)
    .bind(image_kind)
    .bind(source_url)
    .bind(blob_hash)
    .bind(last_write)
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
