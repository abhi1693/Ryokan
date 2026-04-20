use sqlx::{Row, SqlitePool};

use crate::services::anilist::AnimeDetail;

pub const METADATA_REFRESH_INTERVAL_HOURS: i64 = 12;

#[derive(Debug, Clone)]
pub struct CachedSeriesMetadata {
    pub provider_id: i64,
    pub detail: AnimeDetail,
    pub cached_at: String,
    pub is_fresh: bool,
}

pub async fn get_by_series_id(
    db: &SqlitePool,
    series_id: i64,
) -> Result<Option<CachedSeriesMetadata>, sqlx::Error> {
    let row = sqlx::query(
        r#"
        SELECT provider_id, detail_json, cached_at,
               CASE
                   WHEN cached_at >= datetime('now', ?) THEN 1
                   ELSE 0
               END AS is_fresh
        FROM series_metadata_cache
        WHERE series_id = ?
        "#,
    )
    .bind(format!("-{} hours", METADATA_REFRESH_INTERVAL_HOURS))
    .bind(series_id)
    .fetch_optional(db)
    .await?;

    let Some(row) = row else {
        return Ok(None);
    };

    let detail_json: String = row.get("detail_json");
    let detail: AnimeDetail =
        serde_json::from_str(&detail_json).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;

    Ok(Some(CachedSeriesMetadata {
        provider_id: row.get("provider_id"),
        detail,
        cached_at: row.get("cached_at"),
        is_fresh: row.get::<i64, _>("is_fresh") != 0,
    }))
}

pub async fn upsert(
    db: &SqlitePool,
    series_id: i64,
    provider_id: i64,
    mal_id: Option<i64>,
    detail: &AnimeDetail,
) -> Result<(), sqlx::Error> {
    let detail_json =
        serde_json::to_string(detail).map_err(|e| sqlx::Error::Encode(Box::new(e)))?;

    sqlx::query(
        r#"
        INSERT INTO series_metadata_cache (series_id, provider_id, mal_id, detail_json, cached_at)
        VALUES (?, ?, ?, ?, CURRENT_TIMESTAMP)
        ON CONFLICT(series_id) DO UPDATE SET
            provider_id = excluded.provider_id,
            mal_id = excluded.mal_id,
            detail_json = excluded.detail_json,
            cached_at = CURRENT_TIMESTAMP
        "#,
    )
    .bind(series_id)
    .bind(provider_id)
    .bind(mal_id)
    .bind(detail_json)
    .execute(db)
    .await?;

    Ok(())
}

pub async fn get_by_provider_id(
    db: &SqlitePool,
    provider_id: i64,
) -> Result<Option<CachedSeriesMetadata>, sqlx::Error> {
    let row = sqlx::query(
        r#"
        SELECT provider_id, detail_json, cached_at,
               CASE
                   WHEN cached_at >= datetime('now', ?) THEN 1
                   ELSE 0
               END AS is_fresh
        FROM provider_metadata_cache
        WHERE provider_id = ?
        "#,
    )
    .bind(format!("-{} hours", METADATA_REFRESH_INTERVAL_HOURS))
    .bind(provider_id)
    .fetch_optional(db)
    .await?;

    let Some(row) = row else {
        return Ok(None);
    };

    let detail_json: String = row.get("detail_json");
    let detail: AnimeDetail =
        serde_json::from_str(&detail_json).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;

    Ok(Some(CachedSeriesMetadata {
        provider_id: row.get("provider_id"),
        detail,
        cached_at: row.get("cached_at"),
        is_fresh: row.get::<i64, _>("is_fresh") != 0,
    }))
}

pub async fn upsert_provider(
    db: &SqlitePool,
    provider_id: i64,
    mal_id: Option<i64>,
    detail: &AnimeDetail,
) -> Result<(), sqlx::Error> {
    let detail_json =
        serde_json::to_string(detail).map_err(|e| sqlx::Error::Encode(Box::new(e)))?;

    sqlx::query(
        r#"
        INSERT INTO provider_metadata_cache (provider_id, mal_id, detail_json, cached_at)
        VALUES (?, ?, ?, CURRENT_TIMESTAMP)
        ON CONFLICT(provider_id) DO UPDATE SET
            mal_id = excluded.mal_id,
            detail_json = excluded.detail_json,
            cached_at = CURRENT_TIMESTAMP
        "#,
    )
    .bind(provider_id)
    .bind(mal_id)
    .bind(detail_json)
    .execute(db)
    .await?;

    Ok(())
}
