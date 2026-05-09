//! Calendar reader (issues #115 / #116).
//!
//! Single source of truth for "what episodes are airing soon"
//! shared by the iCal feed (`/api/calendar.ics`) and the in-app
//! calendar page (`/calendar`). Both consumers call
//! [`fetch_upcoming`] with their preferred window.
//!
//! ## Architecture (post-refactor)
//!
//! Reads stamped airings from `episode_airings` joined against
//! `series` — no AL round-trip on the request path. The
//! `services::airing_refresh` 12h supervised task is what stamps
//! the table, and the library add path stamps a single series
//! inline so a freshly-added series shows up without waiting for
//! the next tick.
//!
//! Mirrors Sonarr's `Episode.AirDateUtc` shape: stamp once, serve
//! from the DB forever (until the next refresh tick). Saves the
//! per-request AL cost — load-bearing because AL's degraded
//! airingSchedules budget is 30/min.
//!
//! No more in-process cache: the SQL query is a single indexed
//! range scan against `idx_episode_airings_at`, cheap enough that
//! the previous 15-min memoization was strictly redundant.
//!
//! ## Negative-AL-id blind spot
//!
//! Series added via the Jikan/MAL fallback path with no AL mapping
//! get `series.anilist_id = -mal_id`. The refresh task's
//! `anilist_id > 0` filter means these series never get stamped
//! airings, so they're invisible to the calendar. This matches
//! every other AL-keyed surface (SeaDex, refresh, etc.) and is
//! documented in the Settings → Calendar panel.
//!
//! ## Auth
//!
//! This module is auth-agnostic — it just returns
//! `Vec<UpcomingEpisode>`. The iCal handler gates on the
//! `calendar` scoped key (`require_calendar_scope`); the in-app
//! page gates on the cookie `require_auth` middleware. Both call
//! this same function.

use sqlx::Row;
use sqlx::SqlitePool;

use crate::models::config::Config;

/// Per-episode shape served to both consumers. Title is already
/// resolved via `Config.title_language` so neither the iCal text
/// emitter nor the page renderer needs to re-pick from
/// romaji/english/native.
#[derive(Debug, Clone)]
pub struct UpcomingEpisode {
    /// Ryokan-internal `series.id`. Used to back-link to
    /// `/series/{id}` in iCal events + the calendar page cards.
    pub series_id: i64,
    /// AL media id. Always positive — only positive-AL-id series
    /// get stamped airings.
    pub anilist_id: i32,
    /// Already resolved per `Config.title_language` with the usual
    /// romaji → english → native fallback so callers don't have to
    /// pick.
    pub series_title: String,
    pub episode: i32,
    /// Unix epoch seconds (UTC). Both consumers render this in the
    /// user's local timezone client-side.
    pub airing_at: i64,
    /// Per-episode runtime in minutes. Defaults to 24 — the value
    /// most TV anime series use.
    pub duration_minutes: i32,
    /// True when this series is monitored (`monitor_mode != 'none'`).
    /// The `?monitored=true` filter on the iCal endpoint uses this;
    /// the calendar page renders it as a per-row badge.
    pub monitored: bool,
    /// `series.cover_url` (AL CDN poster). Used by the in-app
    /// `/calendar` page to render a thumbnail next to each episode
    /// card; the iCal feed ignores it.
    pub cover_url: String,
    /// Lowercased concatenation of every title variant (romaji +
    /// english + native + db-stored). Used by the calendar page's
    /// client-side series-name filter so a user typing "attack on
    /// titan" can match the series even when their `title_language`
    /// is set to romaji or native and the visible `series_title`
    /// is the Japanese form.
    pub search_haystack: String,
}

/// Default forward window for the iCal feed. Until the
/// configurable `?days=N` query param wires through, callers pass
/// `DEFAULT_FORWARD_DAYS * 86400` directly.
pub const DEFAULT_FORWARD_DAYS: i64 = 30;

/// Fetch upcoming episodes airing in `from..to` for every monitored
/// series in the library (or every series, depending on
/// `monitored_only`). One indexed SQL query against
/// `episode_airings ⨝ series`; never hits AL on the request path.
///
/// Returns `Ok(Vec)` even when no airings match. Errors only
/// propagate from the SQL layer — wrapped in the `String` shape
/// the rest of Ryokan uses end-to-end.
pub async fn fetch_upcoming(
    db: &SqlitePool,
    config: &Config,
    from: i64,
    to: i64,
    monitored_only: bool,
) -> Result<Vec<UpcomingEpisode>, String> {
    let sql = if monitored_only {
        BASE_QUERY_MONITORED
    } else {
        BASE_QUERY_ALL
    };
    let rows = sqlx::query(sql)
        .bind(from)
        .bind(to)
        .fetch_all(db)
        .await
        .map_err(|e| format!("calendar query: {e}"))?;

    let mut out: Vec<UpcomingEpisode> = Vec::with_capacity(rows.len());
    for r in rows {
        let series_id: i64 = r.try_get("series_id").unwrap_or(0);
        let anilist_id: i64 = r.try_get("anilist_id").unwrap_or(0);
        if series_id <= 0 || anilist_id <= 0 {
            continue;
        }
        let title_db: String = r.try_get("title").unwrap_or_default();
        let title_romaji: String = r.try_get("title_romaji").unwrap_or_default();
        let title_english: String = r.try_get("title_english").unwrap_or_default();
        let title_native: String = r.try_get("title_native").unwrap_or_default();
        let cover_url: String = r.try_get("cover_url").unwrap_or_default();
        let monitor_mode: String = r.try_get("monitor_mode").unwrap_or_default();
        let episode: i32 = r.try_get::<i64, _>("episode").unwrap_or(0) as i32;
        let airing_at: i64 = r.try_get("airing_at").unwrap_or(0);
        let duration_minutes: i32 = r.try_get::<i64, _>("duration_minutes").unwrap_or(24) as i32;

        let series_title = pick_title(
            &config.title_language,
            &title_romaji,
            &title_english,
            &title_native,
            &title_db,
        );
        let search_haystack = [
            title_romaji.as_str(),
            title_english.as_str(),
            title_native.as_str(),
            title_db.as_str(),
        ]
        .iter()
        .filter(|t| !t.is_empty())
        .map(|t| t.to_lowercase())
        .collect::<Vec<_>>()
        .join(" ");

        out.push(UpcomingEpisode {
            series_id,
            anilist_id: anilist_id as i32,
            series_title,
            episode,
            airing_at,
            duration_minutes,
            monitored: monitor_mode != "none",
            cover_url,
            search_haystack,
        });
    }
    // SQL `ORDER BY airing_at ASC` already sorts; redundant Rust
    // sort kept off the hot path.
    Ok(out)
}

const BASE_QUERY_ALL: &str = "\
SELECT s.id AS series_id, s.anilist_id, s.title, s.title_romaji, s.title_english, \
       s.title_native, s.cover_url, s.monitor_mode, \
       ea.episode, ea.airing_at, ea.duration_minutes \
FROM episode_airings ea \
JOIN series s ON s.id = ea.series_id \
WHERE s.anilist_id > 0 \
  AND ea.airing_at >= ? AND ea.airing_at < ? \
ORDER BY ea.airing_at ASC";

const BASE_QUERY_MONITORED: &str = "\
SELECT s.id AS series_id, s.anilist_id, s.title, s.title_romaji, s.title_english, \
       s.title_native, s.cover_url, s.monitor_mode, \
       ea.episode, ea.airing_at, ea.duration_minutes \
FROM episode_airings ea \
JOIN series s ON s.id = ea.series_id \
WHERE s.anilist_id > 0 AND s.monitor_mode != 'none' \
  AND ea.airing_at >= ? AND ea.airing_at < ? \
ORDER BY ea.airing_at ASC";

/// Pick a title respecting `Config.title_language`, falling back
/// through the other language fields and finally the
/// database-stored title. Mirrors the per-series-page title
/// resolution shape so what the user sees in their library matches
/// what they see in their calendar.
fn pick_title(lang: &str, romaji: &str, english: &str, native: &str, db_title: &str) -> String {
    let preferred = match lang {
        "english" => english,
        "native" => native,
        _ => romaji,
    };
    if !preferred.is_empty() {
        return preferred.to_string();
    }
    for candidate in [romaji, english, native, db_title] {
        if !candidate.is_empty() {
            return candidate.to_string();
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::episode_airings::{self, EpisodeAiring};

    async fn pool() -> SqlitePool {
        crate::test_support::in_memory_pool().await
    }

    async fn add_series(
        pool: &SqlitePool,
        anilist_id: i64,
        title: &str,
        monitor_mode: &str,
    ) -> i64 {
        sqlx::query_scalar::<_, i64>(
            "INSERT INTO series (anilist_id, title, title_romaji, title_english, title_native, cover_url, monitor_mode) \
             VALUES (?, ?, ?, ?, ?, '', ?) RETURNING id",
        )
        .bind(anilist_id)
        .bind(title)
        .bind(title) // romaji
        .bind("")
        .bind("")
        .bind(monitor_mode)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[test]
    fn pick_title_prefers_user_language() {
        assert_eq!(
            pick_title("english", "Romaji", "English", "Native", "DB"),
            "English"
        );
        assert_eq!(
            pick_title("native", "Romaji", "English", "Native", "DB"),
            "Native"
        );
        assert_eq!(
            pick_title("romaji", "Romaji", "English", "Native", "DB"),
            "Romaji"
        );
    }

    #[test]
    fn pick_title_falls_back_through_languages() {
        assert_eq!(
            pick_title("english", "Romaji", "", "Native", "DB"),
            "Romaji"
        );
        assert_eq!(pick_title("english", "", "", "Native", "DB"), "Native");
        assert_eq!(pick_title("english", "", "", "", "DB"), "DB");
        assert_eq!(pick_title("english", "", "", "", ""), "");
    }

    #[tokio::test]
    async fn empty_db_returns_empty_without_error() {
        let pool = pool().await;
        let cfg = Config::default();
        let out = fetch_upcoming(&pool, &cfg, 0, 1_000_000, true)
            .await
            .unwrap();
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn read_returns_only_in_window_airings_sorted() {
        let pool = pool().await;
        let series_id = add_series(&pool, 1, "Series A", "all").await;
        episode_airings::upsert_for_series(
            &pool,
            series_id,
            &[
                EpisodeAiring {
                    series_id,
                    episode: 5,
                    airing_at: 2_000,
                    duration_minutes: 24,
                },
                EpisodeAiring {
                    series_id,
                    episode: 3,
                    airing_at: 1_000,
                    duration_minutes: 24,
                },
                // Out of window — should be excluded.
                EpisodeAiring {
                    series_id,
                    episode: 1,
                    airing_at: 500,
                    duration_minutes: 24,
                },
            ],
        )
        .await
        .unwrap();

        let cfg = Config::default();
        let out = fetch_upcoming(&pool, &cfg, 1_000, 3_000, false)
            .await
            .unwrap();
        assert_eq!(out.len(), 2);
        // Sorted ascending by airing_at.
        assert_eq!(out[0].episode, 3);
        assert_eq!(out[1].episode, 5);
    }

    #[tokio::test]
    async fn monitored_only_excludes_none_mode_series() {
        let pool = pool().await;
        let unmonitored = add_series(&pool, 1, "Off", "none").await;
        let monitored = add_series(&pool, 2, "On", "all").await;
        for sid in [unmonitored, monitored] {
            episode_airings::upsert_for_series(
                &pool,
                sid,
                &[EpisodeAiring {
                    series_id: sid,
                    episode: 1,
                    airing_at: 1_500,
                    duration_minutes: 24,
                }],
            )
            .await
            .unwrap();
        }

        let cfg = Config::default();
        let with_filter = fetch_upcoming(&pool, &cfg, 0, 5_000, true).await.unwrap();
        assert_eq!(with_filter.len(), 1);
        assert_eq!(with_filter[0].series_id, monitored);

        let without_filter = fetch_upcoming(&pool, &cfg, 0, 5_000, false).await.unwrap();
        assert_eq!(without_filter.len(), 2);
    }

    #[tokio::test]
    async fn negative_anilist_id_series_are_invisible() {
        let pool = pool().await;
        // Insert a Jikan-fallback series with a negative AL id and
        // smuggle a stamped airing in (the refresh task wouldn't
        // normally do this, but the read-side filter should still
        // reject the row).
        sqlx::query("INSERT INTO series (anilist_id, title, monitor_mode) VALUES (?, ?, 'all')")
            .bind(-12345_i64)
            .bind("MAL-only")
            .execute(&pool)
            .await
            .unwrap();
        let id: i64 = sqlx::query_scalar("SELECT id FROM series WHERE anilist_id = -12345")
            .fetch_one(&pool)
            .await
            .unwrap();
        episode_airings::upsert_for_series(
            &pool,
            id,
            &[EpisodeAiring {
                series_id: id,
                episode: 1,
                airing_at: 1_500,
                duration_minutes: 24,
            }],
        )
        .await
        .unwrap();

        let cfg = Config::default();
        let out = fetch_upcoming(&pool, &cfg, 0, 5_000, false).await.unwrap();
        assert!(
            out.is_empty(),
            "negative-AL-id series must not appear in the calendar"
        );
    }
}
