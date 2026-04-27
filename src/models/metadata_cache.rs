use sqlx::{Row, SqlitePool};

use crate::services::anilist::AnimeDetail;

pub const METADATA_REFRESH_INTERVAL_HOURS: i64 = 12;

/// True when the SQLite-format timestamp string (`YYYY-MM-DD HH:MM:SS`,
/// UTC, as produced by `CURRENT_TIMESTAMP`) is older than the refresh
/// TTL. Used by the series-detail page to surface a "Metadata may be
/// out of date" banner when the background refresh hasn't landed within
/// the expected window — usually a sign the upstream provider chain
/// (AniList → Jikan → Kitsu) is unavailable. Issue #106.
///
/// Returns `false` for empty / unparseable timestamps so that a
/// transient row-not-found case doesn't false-positive into the warning.
pub fn is_timestamp_stale(sqlite_timestamp: &str) -> bool {
    let Ok(parsed) = chrono::NaiveDateTime::parse_from_str(sqlite_timestamp, "%Y-%m-%d %H:%M:%S")
    else {
        return false;
    };
    let now = chrono::Utc::now().naive_utc();
    (now - parsed) > chrono::Duration::hours(METADATA_REFRESH_INTERVAL_HOURS)
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_timestamp_stale_empty_string_returns_false() {
        // Empty cached_at means "no row exists" — don't false-positive
        // a missing-cache case into a stale-warning render.
        assert!(!is_timestamp_stale(""));
    }

    #[test]
    fn is_timestamp_stale_unparseable_returns_false() {
        // Defensive: a malformed timestamp shouldn't trip the warning.
        // The CURRENT_TIMESTAMP path always produces parseable values;
        // anything else is unexpected garbage and should be quiet.
        assert!(!is_timestamp_stale("not a date"));
        assert!(!is_timestamp_stale("2026-04-27"));
    }

    #[test]
    fn is_timestamp_stale_just_now_returns_false() {
        let now = chrono::Utc::now()
            .naive_utc()
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        assert!(!is_timestamp_stale(&now));
    }

    #[test]
    fn is_timestamp_stale_within_ttl_returns_false() {
        let recent = (chrono::Utc::now().naive_utc()
            - chrono::Duration::hours(METADATA_REFRESH_INTERVAL_HOURS - 1))
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();
        assert!(!is_timestamp_stale(&recent));
    }

    #[test]
    fn is_timestamp_stale_past_ttl_returns_true() {
        // One second past the TTL boundary — deliberately tight to lock
        // in the boundary semantics.
        let stale = (chrono::Utc::now().naive_utc()
            - chrono::Duration::hours(METADATA_REFRESH_INTERVAL_HOURS)
            - chrono::Duration::seconds(1))
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();
        assert!(is_timestamp_stale(&stale));
    }

    #[test]
    fn is_timestamp_stale_days_old_returns_true() {
        let very_stale = (chrono::Utc::now().naive_utc() - chrono::Duration::days(7))
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        assert!(is_timestamp_stale(&very_stale));
    }
}
