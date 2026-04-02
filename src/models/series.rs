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
    pub folder_name: String,
    pub monitor_mode: String,
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
        folder_name: row.get("folder_name"),
        monitor_mode: row.try_get("monitor_mode").unwrap_or_else(|_| "future".to_string()),
    }
}

/// Get all tracked series, ordered by most recently added.
pub async fn get_all(db: &SqlitePool) -> Result<Vec<Series>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id, anilist_id, mal_id, title, title_romaji, title_english, title_native, cover_url, format, status, episodes, folder_name, monitor_mode FROM series ORDER BY added_at DESC",
    )
    .fetch_all(db)
    .await?;

    Ok(rows.into_iter().map(map_series_row).collect())
}

pub async fn get_by_id(db: &SqlitePool, id: i64) -> Result<Option<Series>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT id, anilist_id, mal_id, title, title_romaji, title_english, title_native, cover_url, format, status, episodes, folder_name, monitor_mode FROM series WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(db)
    .await?;

    Ok(row.map(map_series_row))
}

pub async fn get_by_anilist_id(db: &SqlitePool, anilist_id: i64) -> Result<Option<Series>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT id, anilist_id, mal_id, title, title_romaji, title_english, title_native, cover_url, format, status, episodes, folder_name, monitor_mode FROM series WHERE anilist_id = ?",
    )
    .bind(anilist_id)
    .fetch_optional(db)
    .await?;

    Ok(row.map(map_series_row))
}

pub async fn get_by_mal_id(db: &SqlitePool, mal_id: i64) -> Result<Option<Series>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT id, anilist_id, mal_id, title, title_romaji, title_english, title_native, cover_url, format, status, episodes, folder_name, monitor_mode FROM series WHERE mal_id = ?",
    )
    .bind(mal_id)
    .fetch_optional(db)
    .await?;

    Ok(row.map(map_series_row))
}

/// Insert or update a series based on AniList/MAL provider identity.
pub async fn upsert(
    db: &SqlitePool,
    anilist_id: i64,
    mal_id: Option<i64>,
    title: &str,
    title_romaji: &str,
    title_english: &str,
    title_native: &str,
    cover_url: &str,
    format: &str,
    status: &str,
    episodes: Option<i32>,
) -> Result<(i64, bool), sqlx::Error> {
    if let Some(mid) = mal_id {
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
                    monitor_mode = COALESCE(NULLIF(monitor_mode, ''), ?)
                WHERE id = ?
                "#,
            )
            .bind(anilist_id)
            .bind(mid)
            .bind(title)
            .bind(title_romaji)
            .bind(title_english)
            .bind(title_native)
            .bind(cover_url)
            .bind(format)
            .bind(status)
            .bind(episodes)
            .bind(default_monitor_mode(status).as_str())
            .bind(existing.id)
            .execute(db)
            .await?;
            return Ok((existing.id, false));
        }
    }

    if let Some(existing) = get_by_anilist_id(db, anilist_id).await? {
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
                monitor_mode = COALESCE(NULLIF(monitor_mode, ''), ?)
            WHERE id = ?
            "#,
        )
        .bind(mal_id)
        .bind(title)
        .bind(title_romaji)
        .bind(title_english)
        .bind(title_native)
        .bind(cover_url)
        .bind(format)
        .bind(status)
        .bind(episodes)
        .bind(default_monitor_mode(status).as_str())
        .bind(existing.id)
        .execute(db)
        .await?;
        return Ok((existing.id, false));
    }

    let result = sqlx::query(
        r#"
        INSERT INTO series (anilist_id, mal_id, title, title_romaji, title_english, title_native, cover_url, format, status, episodes, folder_name, monitor_mode)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, '', ?)
        "#,
    )
    .bind(anilist_id)
    .bind(mal_id)
    .bind(title)
    .bind(title_romaji)
    .bind(title_english)
    .bind(title_native)
    .bind(cover_url)
    .bind(format)
    .bind(status)
    .bind(episodes)
    .bind(default_monitor_mode(status).as_str())
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
    anilist_id: i64,
    mal_id: Option<i64>,
    title: &str,
    title_romaji: &str,
    title_english: &str,
    title_native: &str,
    cover_url: &str,
    format: &str,
    status: &str,
    episodes: Option<i32>,
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
            episodes = ?
        WHERE id = ?
        "#,
    )
    .bind(anilist_id)
    .bind(mal_id)
    .bind(title)
    .bind(title_romaji)
    .bind(title_english)
    .bind(title_native)
    .bind(cover_url)
    .bind(format)
    .bind(status)
    .bind(episodes)
    .bind(id)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn get_unreconciled_fallbacks(db: &SqlitePool) -> Result<Vec<Series>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id, anilist_id, mal_id, title, title_romaji, title_english, title_native, cover_url, format, status, episodes, folder_name, monitor_mode FROM series WHERE mal_id IS NOT NULL AND anilist_id < 0 ORDER BY added_at DESC",
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

pub fn default_monitor_mode(status: &str) -> MonitorMode {
    let upper = status.trim().to_ascii_uppercase();
    match upper.as_str() {
        "FINISHED" | "FINISHED_AIRING" | "CANCELLED" => MonitorMode::Missing,
        _ => MonitorMode::Future,
    }
}
