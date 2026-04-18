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
    pub preferred_source: String,
    pub cutoff_source: String,
    pub cutoff_resolution: String,
    /// Legacy combined preferred-quality field. Kept one release for
    /// rollback; read paths should prefer `preferred_source` +
    /// `preferred_resolution`.
    pub quality_profile: String,
    /// Legacy combined cutoff field. See `quality_profile`.
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
    pub prefer_subs: bool,
    pub allow_non_english: bool,
    pub sonarr_enabled: bool,
    pub sonarr_api_key: String,
    pub radarr_enabled: bool,
    pub radarr_api_key: String,
    pub upgrade_search_enabled: bool,
    /// Floor applied to `total_cf_score` after Custom Formats evaluation.
    /// `i32::MIN` (the default) means no floor. Raised by the user via
    /// the Custom Formats settings page to reject candidates whose
    /// summed CF score falls below the threshold.
    pub custom_format_minimum_score: i32,
    /// Apply the hardcoded SeaDex "best release" score boost
    /// (`SEADEX_SCORE_BOOST = 10_000`) at scoring time. Off by default.
    /// Suppressed automatically when the user has any
    /// `Ryokan.SeaDexBestSpecification` Custom Format installed — that
    /// CF replaces the hardcoded boost with a user-controlled score.
    pub seadex_enabled: bool,
    /// #23 — Global default extra tokens appended to every Nyaa query
    /// (e.g. `bd 1080p`). Per-series override on `series` takes
    /// precedence when set. Empty means no tokens.
    pub default_custom_query_tokens: String,
    /// #23 — Global default Nyaa **uploader** restriction. When
    /// non-empty, Ryokan sets `?u=<name>` on every Nyaa search so only
    /// that account's uploads come back. Much tighter than a
    /// `[Group]` title-contains filter: no third-party re-uploads,
    /// no filename-token false matches. Trade-off is that the name
    /// must match an actual Nyaa account — groups without a dedicated
    /// account (HorribleSubs, etc.) will return zero results and the
    /// user has to clear the field. Per-series override takes
    /// precedence. Empty means no restriction.
    pub default_restrict_to_uploader: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            qbit_url: String::new(),
            qbit_user: String::new(),
            qbit_pass: String::new(),
            qbit_category: String::new(),
            qbit_download_path: String::new(),
            jellyfin_url: String::new(),
            jellyfin_api_key: String::new(),
            preferred_groups: String::new(),
            blocked_groups: String::new(),
            preferred_resolution: "1080".to_string(),
            preferred_source: "web".to_string(),
            cutoff_source: "bluray".to_string(),
            cutoff_resolution: "1080".to_string(),
            quality_profile: "web_1080".to_string(),
            quality_cutoff: "bd_1080".to_string(),
            finished_series_quality: "prefer_bd".to_string(),
            media_root: String::new(),
            title_language: "english".to_string(),
            force_mal_fallback: false,
            rss_enabled: false,
            rss_interval_minutes: 5,
            force_kitsu_fallback: false,
            post_processing_enabled: false,
            post_processing_mode: "hardlink".to_string(),
            auto_grab_on_add: true,
            prefer_subs: true,
            allow_non_english: false,
            sonarr_enabled: false,
            sonarr_api_key: String::new(),
            radarr_enabled: false,
            radarr_api_key: String::new(),
            upgrade_search_enabled: false,
            custom_format_minimum_score: i32::MIN,
            seadex_enabled: false,
            default_custom_query_tokens: String::new(),
            default_restrict_to_uploader: String::new(),
        }
    }
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
    preferred_source: String,
    cutoff_source: String,
    cutoff_resolution: String,
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
    prefer_subs: i64,
    allow_non_english: i64,
    sonarr_enabled: i64,
    sonarr_api_key: String,
    radarr_enabled: i64,
    radarr_api_key: String,
    upgrade_search_enabled: i64,
    custom_format_minimum_score: i64,
    seadex_enabled: i64,
    default_custom_query_tokens: String,
    default_restrict_to_uploader: String,
}

/// Get the singleton config row.
pub async fn get_config(db: &SqlitePool) -> Result<Option<Config>, sqlx::Error> {
    let row: Option<ConfigRow> = sqlx::query_as(
        "SELECT qbit_url, qbit_user, qbit_pass, qbit_category, qbit_download_path, jellyfin_url, jellyfin_api_key, preferred_groups, blocked_groups, preferred_resolution, preferred_source, cutoff_source, cutoff_resolution, quality_profile, quality_cutoff, finished_series_quality, media_root, title_language, force_mal_fallback, rss_enabled, rss_interval_minutes, force_kitsu_fallback, post_processing_enabled, post_processing_mode, auto_grab_on_add, prefer_subs, allow_non_english, sonarr_enabled, sonarr_api_key, radarr_enabled, radarr_api_key, upgrade_search_enabled, custom_format_minimum_score, seadex_enabled, default_custom_query_tokens, default_restrict_to_uploader FROM config WHERE id = 1",
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
        preferred_source: r.preferred_source,
        cutoff_source: r.cutoff_source,
        cutoff_resolution: r.cutoff_resolution,
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
        prefer_subs: r.prefer_subs != 0,
        allow_non_english: r.allow_non_english != 0,
        sonarr_enabled: r.sonarr_enabled != 0,
        sonarr_api_key: r.sonarr_api_key,
        radarr_enabled: r.radarr_enabled != 0,
        radarr_api_key: r.radarr_api_key,
        upgrade_search_enabled: r.upgrade_search_enabled != 0,
        custom_format_minimum_score: r.custom_format_minimum_score as i32,
        seadex_enabled: r.seadex_enabled != 0,
        default_custom_query_tokens: r.default_custom_query_tokens,
        default_restrict_to_uploader: r.default_restrict_to_uploader,
    }))
}

/// Upsert the config row.
pub async fn save_config(db: &SqlitePool, config: &Config) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO config (id, qbit_url, qbit_user, qbit_pass, qbit_category, qbit_download_path, jellyfin_url, jellyfin_api_key, preferred_groups, blocked_groups, preferred_resolution, preferred_source, cutoff_source, cutoff_resolution, quality_profile, quality_cutoff, finished_series_quality, media_root, title_language, force_mal_fallback, rss_enabled, rss_interval_minutes, force_kitsu_fallback, post_processing_enabled, post_processing_mode, auto_grab_on_add, prefer_subs, allow_non_english, sonarr_enabled, sonarr_api_key, radarr_enabled, radarr_api_key, upgrade_search_enabled, custom_format_minimum_score, seadex_enabled, default_custom_query_tokens, default_restrict_to_uploader)
        VALUES (1, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
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
            preferred_source = excluded.preferred_source,
            cutoff_source = excluded.cutoff_source,
            cutoff_resolution = excluded.cutoff_resolution,
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
            auto_grab_on_add = excluded.auto_grab_on_add,
            prefer_subs = excluded.prefer_subs,
            allow_non_english = excluded.allow_non_english,
            sonarr_enabled = excluded.sonarr_enabled,
            sonarr_api_key = excluded.sonarr_api_key,
            radarr_enabled = excluded.radarr_enabled,
            radarr_api_key = excluded.radarr_api_key,
            upgrade_search_enabled = excluded.upgrade_search_enabled,
            custom_format_minimum_score = excluded.custom_format_minimum_score,
            seadex_enabled = excluded.seadex_enabled,
            default_custom_query_tokens = excluded.default_custom_query_tokens,
            default_restrict_to_uploader = excluded.default_restrict_to_uploader
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
    .bind(&config.preferred_source)
    .bind(&config.cutoff_source)
    .bind(&config.cutoff_resolution)
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
    .bind(if config.prefer_subs { 1_i64 } else { 0_i64 })
    .bind(if config.allow_non_english { 1_i64 } else { 0_i64 })
    .bind(if config.sonarr_enabled { 1_i64 } else { 0_i64 })
    .bind(&config.sonarr_api_key)
    .bind(if config.radarr_enabled { 1_i64 } else { 0_i64 })
    .bind(&config.radarr_api_key)
    .bind(if config.upgrade_search_enabled { 1_i64 } else { 0_i64 })
    .bind(config.custom_format_minimum_score as i64)
    .bind(if config.seadex_enabled { 1_i64 } else { 0_i64 })
    .bind(&config.default_custom_query_tokens)
    .bind(&config.default_restrict_to_uploader)
    .execute(db)
    .await?;

    Ok(())
}
