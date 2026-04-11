use serde::{Deserialize, Serialize};
use sqlx::{FromRow, SqlitePool};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub qbit_url: String,
    pub qbit_user: String,
    pub qbit_pass: String,
    pub qbit_category: String,
    pub qbit_download_path: String,
    pub jellyfin_url: String,
    pub jellyfin_api_key: String,
    pub preferred_groups: String,
    pub blocked_groups: String,
    pub preferred_resolution: String,
    pub quality_profile: String,
    pub quality_cutoff: String,
    pub finished_series_quality: String,
    pub media_root: String,
    pub title_language: String,
    pub force_mal_fallback: bool,
    pub rss_enabled: bool,
    pub rss_interval_minutes: i32,
    pub force_kitsu_fallback: bool,
    pub post_processing_enabled: bool,
    pub post_processing_mode: String,
    pub auto_grab_on_add: bool,
}

#[derive(Debug, FromRow)]
struct ConfigRow {
    qbit_url: String,
    qbit_user: String,
    qbit_pass: String,
    qbit_category: String,
    qbit_download_path: String,
    jellyfin_url: String,
    jellyfin_api_key: String,
    preferred_groups: String,
    blocked_groups: String,
    preferred_resolution: String,
    quality_profile: String,
    quality_cutoff: String,
    finished_series_quality: String,
    media_root: String,
    title_language: String,
    force_mal_fallback: i64,
    rss_enabled: i64,
    rss_interval_minutes: i64,
    force_kitsu_fallback: i64,
    post_processing_enabled: i64,
    post_processing_mode: String,
    auto_grab_on_add: i64,
}

/// Get the singleton config row.
pub async fn get_config(db: &SqlitePool) -> Result<Option<Config>, sqlx::Error> {
    let row: Option<ConfigRow> = sqlx::query_as(
        "SELECT qbit_url, qbit_user, qbit_pass, qbit_category, qbit_download_path, jellyfin_url, jellyfin_api_key, preferred_groups, blocked_groups, preferred_resolution, quality_profile, quality_cutoff, finished_series_quality, media_root, title_language, force_mal_fallback, rss_enabled, rss_interval_minutes, force_kitsu_fallback, post_processing_enabled, post_processing_mode, auto_grab_on_add FROM config WHERE id = 1",
    )
    .fetch_optional(db)
    .await?;

    Ok(row.map(|r| Config {
        qbit_url: r.qbit_url,
        qbit_user: r.qbit_user,
        qbit_pass: r.qbit_pass,
        qbit_category: r.qbit_category,
        qbit_download_path: r.qbit_download_path,
        jellyfin_url: r.jellyfin_url,
        jellyfin_api_key: r.jellyfin_api_key,
        preferred_groups: r.preferred_groups,
        blocked_groups: r.blocked_groups,
        preferred_resolution: r.preferred_resolution,
        quality_profile: r.quality_profile,
        quality_cutoff: r.quality_cutoff,
        finished_series_quality: r.finished_series_quality,
        media_root: r.media_root,
        title_language: r.title_language,
        force_mal_fallback: r.force_mal_fallback != 0,
        rss_enabled: r.rss_enabled != 0,
        rss_interval_minutes: r.rss_interval_minutes as i32,
        force_kitsu_fallback: r.force_kitsu_fallback != 0,
        post_processing_enabled: r.post_processing_enabled != 0,
        post_processing_mode: r.post_processing_mode,
        auto_grab_on_add: r.auto_grab_on_add != 0,
    }))
}

/// Upsert the config row.
pub async fn save_config(db: &SqlitePool, config: &Config) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO config (id, qbit_url, qbit_user, qbit_pass, qbit_category, qbit_download_path, jellyfin_url, jellyfin_api_key, preferred_groups, blocked_groups, preferred_resolution, quality_profile, quality_cutoff, finished_series_quality, media_root, title_language, force_mal_fallback, rss_enabled, rss_interval_minutes, force_kitsu_fallback, post_processing_enabled, post_processing_mode, auto_grab_on_add)
        VALUES (1, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(id) DO UPDATE SET
            qbit_url = excluded.qbit_url,
            qbit_user = excluded.qbit_user,
            qbit_pass = excluded.qbit_pass,
            qbit_category = excluded.qbit_category,
            qbit_download_path = excluded.qbit_download_path,
            jellyfin_url = excluded.jellyfin_url,
            jellyfin_api_key = excluded.jellyfin_api_key,
            preferred_groups = excluded.preferred_groups,
            blocked_groups = excluded.blocked_groups,
            preferred_resolution = excluded.preferred_resolution,
            quality_profile = excluded.quality_profile,
            quality_cutoff = excluded.quality_cutoff,
            finished_series_quality = excluded.finished_series_quality,
            media_root = excluded.media_root,
            title_language = excluded.title_language,
            force_mal_fallback = excluded.force_mal_fallback,
            rss_enabled = excluded.rss_enabled,
            rss_interval_minutes = excluded.rss_interval_minutes,
            force_kitsu_fallback = excluded.force_kitsu_fallback,
            post_processing_enabled = excluded.post_processing_enabled,
            post_processing_mode = excluded.post_processing_mode,
            auto_grab_on_add = excluded.auto_grab_on_add
        "#,
    )
    .bind(&config.qbit_url)
    .bind(&config.qbit_user)
    .bind(&config.qbit_pass)
    .bind(&config.qbit_category)
    .bind(&config.qbit_download_path)
    .bind(&config.jellyfin_url)
    .bind(&config.jellyfin_api_key)
    .bind(&config.preferred_groups)
    .bind(&config.blocked_groups)
    .bind(&config.preferred_resolution)
    .bind(&config.quality_profile)
    .bind(&config.quality_cutoff)
    .bind(&config.finished_series_quality)
    .bind(&config.media_root)
    .bind(&config.title_language)
    .bind(if config.force_mal_fallback { 1_i64 } else { 0_i64 })
    .bind(if config.rss_enabled { 1_i64 } else { 0_i64 })
    .bind(config.rss_interval_minutes as i64)
    .bind(if config.force_kitsu_fallback { 1_i64 } else { 0_i64 })
    .bind(if config.post_processing_enabled { 1_i64 } else { 0_i64 })
    .bind(&config.post_processing_mode)
    .bind(if config.auto_grab_on_add { 1_i64 } else { 0_i64 })
    .execute(db)
    .await?;

    Ok(())
}
