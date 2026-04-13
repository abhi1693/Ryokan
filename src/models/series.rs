use serde::Serialize;
use sqlx::{Row, SqlitePool};

use crate::models::monitoring::MonitorMode;

#[derive(Debug, Clone, Serialize)]
pub struct Series {
    pub id: i64,
    pub anilist_id: i64,
    pub mal_id: Option<i64>,
    pub title: String,
    pub title_romaji: String,
    pub title_english: String,
    pub title_native: String,
    pub cover_url: String,
    pub format: String,
    pub status: String,
    pub episodes: Option<i32>,
    pub season_year: Option<i32>,
    /// Year the series *finished* airing, when AniList has an explicit
    /// end date. Used by Layer 4 (temporal inference) to distinguish
    /// "finished recently" from "started years ago and is still going."
    /// `None` for currently-airing shows and for metadata providers
    /// that don't supply an end date.
    pub end_year: Option<i32>,
    pub folder_name: String,
    pub monitor_mode: String,
    /// Phase 4 per-series upgrade toggle. When false the upgrade scanner
    /// skips this series entirely, even if a higher-quality release is
    /// available. Defaults to true to preserve historical behavior.
    pub allow_upgrades: bool,
}

impl Series {
    pub fn monitor_mode_enum(&self) -> MonitorMode {
        MonitorMode::from_str(&self.monitor_mode)
    }
}

fn map_series_row(row: sqlx::sqlite::SqliteRow) -> Series {
    Series {
        id: row.get("id"),
        anilist_id: row.get("anilist_id"),
        mal_id: row.try_get("mal_id").ok(),
        title: row.get("title"),
        title_romaji: row.get("title_romaji"),
        title_english: row.get("title_english"),
        title_native: row.get("title_native"),
        cover_url: row.get("cover_url"),
        format: row.get("format"),
        status: row.get("status"),
        episodes: row.get("episodes"),
        season_year: row.try_get("season_year").ok().flatten(),
        end_year: row.try_get("end_year").ok().flatten(),
        folder_name: row.get("folder_name"),
        monitor_mode: row.try_get("monitor_mode").unwrap_or_else(|_| "future".to_string()),
        // Default to true so series from before the column existed (migration
        // backfills via ADD COLUMN DEFAULT 1) opt *in* to upgrades.
        allow_upgrades: row.try_get::<i64, _>("allow_upgrades").map(|v| v != 0).unwrap_or(true),
    }
}

/// Get all tracked series, ordered by most recently added.
pub async fn get_all(db: &SqlitePool) -> Result<Vec<Series>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id, anilist_id, mal_id, title, title_romaji, title_english, title_native, cover_url, format, status, episodes, season_year, end_year, folder_name, monitor_mode, allow_upgrades FROM series ORDER BY added_at DESC",
    )
    .fetch_all(db)
    .await?;

    Ok(rows.into_iter().map(map_series_row).collect())
}

pub async fn get_by_id(db: &SqlitePool, id: i64) -> Result<Option<Series>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT id, anilist_id, mal_id, title, title_romaji, title_english, title_native, cover_url, format, status, episodes, season_year, end_year, folder_name, monitor_mode, allow_upgrades FROM series WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(db)
    .await?;

    Ok(row.map(map_series_row))
}

pub async fn get_by_anilist_id(db: &SqlitePool, anilist_id: i64) -> Result<Option<Series>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT id, anilist_id, mal_id, title, title_romaji, title_english, title_native, cover_url, format, status, episodes, season_year, end_year, folder_name, monitor_mode, allow_upgrades FROM series WHERE anilist_id = ?",
    )
    .bind(anilist_id)
    .fetch_optional(db)
    .await?;

    Ok(row.map(map_series_row))
}

pub async fn get_by_mal_id(db: &SqlitePool, mal_id: i64) -> Result<Option<Series>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT id, anilist_id, mal_id, title, title_romaji, title_english, title_native, cover_url, format, status, episodes, season_year, end_year, folder_name, monitor_mode, allow_upgrades FROM series WHERE mal_id = ?",
    )
    .bind(mal_id)
    .fetch_optional(db)
    .await?;

    Ok(row.map(map_series_row))
}

/// Core metadata bundle shared by `upsert` and `refresh_core_metadata`.
///
/// Collapsing the 11 scalar args into a named struct closes a real
/// correctness hole: four of the fields (`title`, `title_romaji`,
/// `title_english`, `title_native`) are all `&str` and sit next to
/// each other in the call order. Positional callers could swap any
/// two of them and neither the compiler nor the SQL would object —
/// the wrong string would just silently end up in the wrong column.
/// Named fields force callsites to be explicit.
pub struct SeriesCore<'a> {
    pub anilist_id: i64,
    pub mal_id: Option<i64>,
    pub title: &'a str,
    pub title_romaji: &'a str,
    pub title_english: &'a str,
    pub title_native: &'a str,
    pub cover_url: &'a str,
    pub format: &'a str,
    pub status: &'a str,
    pub episodes: Option<i32>,
    pub season_year: Option<i32>,
    /// Year the series finished airing, or `None` for currently-airing or
    /// unknown. Populated from AniList's `endDate.year` on the metadata
    /// fetch path; callers that build SeriesCore from providers without
    /// an end-date concept pass `None`. `upsert`/`refresh_core_metadata`
    /// use `COALESCE(?, end_year)` so a later fetch that *does* carry
    /// the year can fill in the gap without clobbering a previously-set
    /// value.
    pub end_year: Option<i32>,
}

/// Insert or update a series based on AniList/MAL provider identity.
pub async fn upsert(
    db: &SqlitePool,
    core: SeriesCore<'_>,
) -> Result<(i64, bool), sqlx::Error> {
    if let Some(mid) = core.mal_id {
        if let Some(existing) = get_by_mal_id(db, mid).await? {
            sqlx::query(
                r#"
                UPDATE series
                SET anilist_id = ?,
                    mal_id = ?,
                    title = ?,
                    title_romaji = ?,
                    title_english = ?,
                    title_native = ?,
                    cover_url = ?,
                    format = ?,
                    status = ?,
                    episodes = ?,
                    season_year = COALESCE(?, season_year),
                    end_year = COALESCE(?, end_year),
                    monitor_mode = COALESCE(NULLIF(monitor_mode, ''), ?)
                WHERE id = ?
                "#,
            )
            .bind(core.anilist_id)
            .bind(mid)
            .bind(core.title)
            .bind(core.title_romaji)
            .bind(core.title_english)
            .bind(core.title_native)
            .bind(core.cover_url)
            .bind(core.format)
            .bind(core.status)
            .bind(core.episodes)
            .bind(core.season_year)
            .bind(core.end_year)
            .bind(default_monitor_mode(core.status).as_str())
            .bind(existing.id)
            .execute(db)
            .await?;
            return Ok((existing.id, false));
        }
    }

    if let Some(existing) = get_by_anilist_id(db, core.anilist_id).await? {
        sqlx::query(
            r#"
            UPDATE series
            SET mal_id = COALESCE(?, mal_id),
                title = ?,
                title_romaji = ?,
                title_english = ?,
                title_native = ?,
                cover_url = ?,
                format = ?,
                status = ?,
                episodes = ?,
                season_year = COALESCE(?, season_year),
                end_year = COALESCE(?, end_year),
                monitor_mode = COALESCE(NULLIF(monitor_mode, ''), ?)
            WHERE id = ?
            "#,
        )
        .bind(core.mal_id)
        .bind(core.title)
        .bind(core.title_romaji)
        .bind(core.title_english)
        .bind(core.title_native)
        .bind(core.cover_url)
        .bind(core.format)
        .bind(core.status)
        .bind(core.episodes)
        .bind(core.season_year)
        .bind(core.end_year)
        .bind(default_monitor_mode(core.status).as_str())
        .bind(existing.id)
        .execute(db)
        .await?;
        return Ok((existing.id, false));
    }

    // Auto-generate a folder name from the best available title.
    let folder = {
        let raw = if !core.title_english.is_empty() {
            core.title_english
        } else if !core.title_romaji.is_empty() {
            core.title_romaji
        } else {
            core.title
        };
        crate::services::media::sanitize_folder_name(raw)
    };

    let result = sqlx::query(
        r#"
        INSERT INTO series (anilist_id, mal_id, title, title_romaji, title_english, title_native, cover_url, format, status, episodes, season_year, end_year, folder_name, monitor_mode)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind(core.anilist_id)
    .bind(core.mal_id)
    .bind(core.title)
    .bind(core.title_romaji)
    .bind(core.title_english)
    .bind(core.title_native)
    .bind(core.cover_url)
    .bind(core.format)
    .bind(core.status)
    .bind(core.episodes)
    .bind(core.season_year)
    .bind(core.end_year)
    .bind(&folder)
    .bind(default_monitor_mode(core.status).as_str())
    .execute(db)
    .await?;

    Ok((result.last_insert_rowid(), true))
}

/// Remove a series by its database ID.
pub async fn remove(db: &SqlitePool, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM series_metadata_cache WHERE series_id = ?")
        .bind(id)
        .execute(db)
        .await
        .ok();

    sqlx::query("DELETE FROM series_relations_cache WHERE series_id = ?").bind(id).execute(db).await.ok();
    sqlx::query("DELETE FROM series_episode_metadata WHERE series_id = ?").bind(id).execute(db).await.ok();

    sqlx::query("DELETE FROM series WHERE id = ?")
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

/// Update the folder name mapping for a series.
pub async fn update_folder(db: &SqlitePool, id: i64, folder_name: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE series SET folder_name = ? WHERE id = ?")
        .bind(folder_name)
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}



pub async fn refresh_core_metadata(
    db: &SqlitePool,
    id: i64,
    core: SeriesCore<'_>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        UPDATE series
        SET anilist_id = ?,
            mal_id = COALESCE(?, mal_id),
            title = ?,
            title_romaji = ?,
            title_english = ?,
            title_native = ?,
            cover_url = ?,
            format = ?,
            status = ?,
            episodes = ?,
            season_year = COALESCE(?, season_year),
            end_year = COALESCE(?, end_year)
        WHERE id = ?
        "#,
    )
    .bind(core.anilist_id)
    .bind(core.mal_id)
    .bind(core.title)
    .bind(core.title_romaji)
    .bind(core.title_english)
    .bind(core.title_native)
    .bind(core.cover_url)
    .bind(core.format)
    .bind(core.status)
    .bind(core.episodes)
    .bind(core.season_year)
    .bind(core.end_year)
    .bind(id)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn get_unreconciled_fallbacks(db: &SqlitePool) -> Result<Vec<Series>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id, anilist_id, mal_id, title, title_romaji, title_english, title_native, cover_url, format, status, episodes, season_year, end_year, folder_name, monitor_mode, allow_upgrades FROM series WHERE mal_id IS NOT NULL AND anilist_id < 0 ORDER BY added_at DESC",
    )
    .fetch_all(db)
    .await?;

    Ok(rows.into_iter().map(map_series_row).collect())
}

pub async fn update_monitor_mode(db: &SqlitePool, id: i64, monitor_mode: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE series SET monitor_mode = ? WHERE id = ?")
        .bind(monitor_mode)
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

/// Toggle the per-series upgrade opt-in. When false the upgrade scanner
/// in `services::upgrade` skips this series entirely.
pub async fn update_allow_upgrades(db: &SqlitePool, id: i64, allow: bool) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE series SET allow_upgrades = ? WHERE id = ?")
        .bind(if allow { 1_i64 } else { 0_i64 })
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

pub fn default_monitor_mode(status: &str) -> MonitorMode {
    let upper = status.trim().to_ascii_uppercase();
    match upper.as_str() {
        "FINISHED" | "FINISHED_AIRING" | "CANCELLED" => MonitorMode::Missing,
        _ => MonitorMode::Future,
    }
}
