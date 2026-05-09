//! Calendar data layer (issue #115).
//!
//! Single source of truth for "what episodes are airing soon"
//! shared by the iCal feed (`/api/calendar.ics`) and the in-app
//! calendar page (`/calendar`, issue #116). Both consumers call
//! [`fetch_upcoming`] with their preferred window; the cache + AL
//! fetch + DB join happen here exactly once per (id-set, window)
//! tuple regardless of how many subscribers are polling.
//!
//! ## Architecture
//!
//! Read-through to AniList with a 15-minute server-side cache. No
//! new persistent state — the AL `Page.airingSchedules` query is
//! the right shape for this, and persisting per-series next-airing
//! state would couple iCal freshness to the 12h metadata-refresh
//! cadence (and miss future weeks).
//!
//! Cache is keyed by `(sorted_anilist_ids, from, to)`. When the
//! user adds or removes a series, the id set changes and the next
//! request misses the cache; a 15-min stale window for "I just
//! added a series, when will it appear in my calendar?" is
//! acceptable.
//!
//! ## Negative-AL-id blind spot
//!
//! Series added via the Jikan/MAL fallback path with no AL mapping
//! get `series.anilist_id = -mal_id`. We filter `anilist_id > 0`
//! before the AL query (matching every other AL call site) so
//! synthetic ids don't leak into AL requests. Documented in the
//! Settings → Calendar panel so users with MAL-exclusive series
//! aren't surprised by their absence from the feed.
//!
//! ## Auth
//!
//! This module is auth-agnostic — it just returns Vec<UpcomingEpisode>.
//! The iCal handler gates on the `calendar` scoped key
//! (`require_calendar_scope`); the in-app page gates on the cookie
//! `require_auth` middleware. Both call this same function.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex as StdMutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use sqlx::Row;
use sqlx::SqlitePool;

use crate::models::config::Config;
use crate::services::anilist::airing_schedules;

/// Per-episode shape served to both consumers. Title is already
/// resolved via `Config.title_language` so neither the iCal text
/// emitter nor the JSON page renderer needs to re-pick from
/// romaji/english/native.
#[derive(Debug, Clone, Serialize)]
pub struct UpcomingEpisode {
    /// Ryokan-internal `series.id`. Used to back-link to
    /// `/series/{id}` in iCal events + the calendar page cards.
    pub series_id: i64,
    /// AL media id. Always positive — negative-id sentinel rows are
    /// filtered out before the AL query.
    pub anilist_id: i32,
    /// Already resolved per `Config.title_language` with the usual
    /// romaji → english → native fallback so callers don't have to
    /// pick.
    pub series_title: String,
    pub episode: i32,
    /// Unix epoch seconds (UTC). Both consumers render this in the
    /// user's local timezone client-side.
    pub airing_at: i64,
    /// AL's `Media.duration` (per-episode runtime in minutes).
    /// Defaults to 24 — the value most TV anime series use.
    pub duration_minutes: i32,
    /// True when this series is monitored (`monitor_mode != 'none'`).
    /// The `?monitored=true` filter on the iCal endpoint uses this;
    /// the calendar page renders it as a per-row badge.
    pub monitored: bool,
    /// `series.cover_url` (AL CDN poster). Used by the in-app
    /// `/calendar` page to render a thumbnail next to each episode
    /// card; the iCal feed ignores it (calendar clients don't
    /// render embedded artwork).
    pub cover_url: String,
    /// Lowercased concatenation of every title variant (romaji +
    /// english + native + db-stored). Used by the calendar page's
    /// client-side series-name filter so a user typing "attack on
    /// titan" can match the series even when their `title_language`
    /// is set to romaji or native and the visible `series_title`
    /// is the Japanese form. Computed server-side once instead of
    /// having the JS layer normalize per-keystroke.
    pub search_haystack: String,
}

/// Default forward window for the iCal feed. The plan doc reserves
/// a configurable `?days=N` query param; until that wires through,
/// callers pass `DEFAULT_FORWARD_DAYS * 86400` directly.
pub const DEFAULT_FORWARD_DAYS: i64 = 30;

/// Cache TTL. 15 minutes balances "iCal subscribers polling every
/// 15-60 min get fast responses" against "I just added a series —
/// when will it show up?" The 30-day forward window plus AL's
/// upstream cadence makes a tighter TTL pointless.
const CACHE_TTL: Duration = Duration::from_secs(15 * 60);

/// Width of the cache-key window-bucket. Callers pass `from = now`
/// (which changes every second), so without bucketing the cache
/// key would change on every request and the cache would never
/// hit — even within the TTL window. Snapping `from` and `to` to
/// the nearest 15-minute boundary makes the key stable for the
/// whole cache lifetime; users get an at-most-15-min-snapped
/// window which is negligible at the 7-30 day windows the page
/// actually serves.
const CACHE_BUCKET_SECS: i64 = 15 * 60;

fn bucket_floor(ts: i64) -> i64 {
    ts - ts.rem_euclid(CACHE_BUCKET_SECS)
}

#[derive(Hash, Eq, PartialEq, Clone)]
struct CacheKey {
    /// Sorted so two callers with the same set in different order
    /// hit the same cache entry.
    sorted_ids: Vec<i32>,
    from: i64,
    to: i64,
}

#[derive(Clone)]
struct CacheEntry {
    fetched_at: Instant,
    data: Vec<UpcomingEpisode>,
}

/// Process-wide cache. `StdMutex` (not async) is fine — the lock
/// is held for HashMap get/insert only, never across an await
/// point. Same shape SeaDex's lookup cache uses.
static CACHE: LazyLock<StdMutex<HashMap<CacheKey, CacheEntry>>> =
    LazyLock::new(|| StdMutex::new(HashMap::new()));

/// Fetch upcoming episodes airing in `from..to` for every monitored
/// series in the library (or every series, depending on
/// `monitored_only`). Cache hit returns immediately; cache miss
/// pulls the AL ids from `series`, calls
/// [`airing_schedules::fetch_airing_schedules`], joins the result
/// against `series` to resolve internal ids + monitor state, and
/// writes the cache.
///
/// Errors propagate from the AL layer with the "AniList
/// rate-limited" / "AniList unavailable" prefix taxonomy so the
/// iCal handler can decide whether to serve a stale cache or 503.
pub async fn fetch_upcoming(
    db: &SqlitePool,
    config: &Config,
    from: i64,
    to: i64,
    monitored_only: bool,
) -> Result<Vec<UpcomingEpisode>, String> {
    // Snap window endpoints to a 15-minute bucket so the cache key
    // stabilizes across requests within the same TTL window. Without
    // this, callers passing `from = chrono::Utc::now().timestamp()`
    // (which changes every second) would generate a unique key per
    // request and the cache would effectively never hit. See
    // `CACHE_BUCKET_SECS` above.
    let from = bucket_floor(from);
    let to = bucket_floor(to);

    // Pull the per-row mapping from anilist_id → (series_id, monitor
    // state, locale-resolved title). One query for both the AL id
    // list and the post-fetch join.
    let rows = load_series_index(db, config, monitored_only).await?;
    if rows.is_empty() {
        // No monitored (or no positive-AL-id) series — no AL request,
        // empty result. Don't cache the empty result; a freshly-added
        // series should appear without waiting for cache expiry.
        return Ok(Vec::new());
    }

    let mut sorted_ids: Vec<i32> = rows.iter().map(|r| r.anilist_id).collect();
    sorted_ids.sort_unstable();
    let key = CacheKey {
        sorted_ids: sorted_ids.clone(),
        from,
        to,
    };

    if let Some(cached) = cache_get(&key) {
        return Ok(cached);
    }

    // Cache miss — fetch from AL.
    let schedules = airing_schedules::fetch_airing_schedules(&sorted_ids, from, to).await?;
    let by_anilist_id: HashMap<i32, &SeriesIndexRow> =
        rows.iter().map(|r| (r.anilist_id, r)).collect();

    let mut out: Vec<UpcomingEpisode> = Vec::with_capacity(schedules.len());
    for s in schedules {
        let Some(row) = by_anilist_id.get(&s.media_id) else {
            // AL returned a schedule for a media id we didn't ask
            // about (shouldn't happen; mediaId_in is ID-list-
            // restricted). Drop silently rather than emit an
            // unidentifiable VEVENT.
            continue;
        };
        // Pick a per-call title: prefer the user's preferred
        // language, fall back through romaji → english → native →
        // database-stored title to ensure SUMMARY is never empty.
        let title = pick_title(
            &config.title_language,
            &s.title_romaji,
            &s.title_english,
            &s.title_native,
            &row.title,
        );
        // Every title variant lowercased + space-separated so a
        // single substring search hits any of them. `db_title` is
        // included as a fallback for series the user manually
        // renamed (rare; the AL-fetched titles are usually the
        // ones a user would type to search).
        let haystack = [
            s.title_romaji.as_str(),
            s.title_english.as_str(),
            s.title_native.as_str(),
            row.title.as_str(),
        ]
        .iter()
        .filter(|t| !t.is_empty())
        .map(|t| t.to_lowercase())
        .collect::<Vec<_>>()
        .join(" ");
        out.push(UpcomingEpisode {
            series_id: row.series_id,
            anilist_id: s.media_id,
            series_title: title,
            episode: s.episode,
            airing_at: s.airing_at,
            duration_minutes: s.duration_minutes.unwrap_or(24),
            monitored: row.monitored,
            cover_url: row.cover_url.clone(),
            search_haystack: haystack,
        });
    }
    out.sort_by_key(|e| e.airing_at);

    cache_put(key, out.clone());
    Ok(out)
}

struct SeriesIndexRow {
    series_id: i64,
    anilist_id: i32,
    /// Database-stored title. Used as the final fallback if the AL
    /// row's titles are all empty (shouldn't happen but the
    /// SUMMARY-must-not-be-empty contract is load-bearing for iCal
    /// validators).
    title: String,
    monitored: bool,
    cover_url: String,
}

async fn load_series_index(
    db: &SqlitePool,
    _config: &Config,
    monitored_only: bool,
) -> Result<Vec<SeriesIndexRow>, String> {
    let sql = if monitored_only {
        "SELECT id, anilist_id, title, monitor_mode, cover_url \
         FROM series \
         WHERE anilist_id > 0 AND monitor_mode != 'none'"
    } else {
        "SELECT id, anilist_id, title, monitor_mode, cover_url \
         FROM series \
         WHERE anilist_id > 0"
    };
    let rows = sqlx::query(sql)
        .fetch_all(db)
        .await
        .map_err(|e| format!("series index query: {e}"))?;
    let mut out: Vec<SeriesIndexRow> = Vec::with_capacity(rows.len());
    for row in rows {
        let series_id: i64 = row.try_get("id").unwrap_or(0);
        let anilist_id: i64 = row.try_get("anilist_id").unwrap_or(0);
        if anilist_id <= 0 || series_id <= 0 {
            continue;
        }
        let title: String = row.try_get("title").unwrap_or_default();
        let monitor_mode: String = row.try_get("monitor_mode").unwrap_or_default();
        let cover_url: String = row.try_get("cover_url").unwrap_or_default();
        out.push(SeriesIndexRow {
            series_id,
            anilist_id: anilist_id as i32,
            title,
            monitored: monitor_mode != "none",
            cover_url,
        });
    }
    Ok(out)
}

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

fn cache_get(key: &CacheKey) -> Option<Vec<UpcomingEpisode>> {
    let mut cache = CACHE.lock().ok()?;
    let entry = cache.get(key)?;
    if entry.fetched_at.elapsed() > CACHE_TTL {
        cache.remove(key);
        return None;
    }
    Some(entry.data.clone())
}

fn cache_put(key: CacheKey, data: Vec<UpcomingEpisode>) {
    let Ok(mut cache) = CACHE.lock() else {
        return;
    };
    cache.insert(
        key,
        CacheEntry {
            fetched_at: Instant::now(),
            data,
        },
    );
    // Lazy cleanup: drop expired entries when we cross a soft
    // ceiling. Avoids unbounded growth from users polling many
    // distinct ?days=N windows.
    if cache.len() > 64 {
        let now = Instant::now();
        cache.retain(|_, e| now.duration_since(e.fetched_at) <= CACHE_TTL);
    }
}

/// Drop every cache entry. Test-only — used to keep cache state
/// isolated between tests sharing the process-wide CACHE.
#[cfg(test)]
pub(crate) fn reset_cache() {
    if let Ok(mut cache) = CACHE.lock() {
        cache.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_floor_snaps_to_15_minute_boundaries() {
        // Pin the cache-key bucketing. A request at 14:07:33 and a
        // request at 14:08:01 should produce the same bucket (14:00)
        // so they hit the same cache entry. Regression test for the
        // bug where every per-second `now()` produced a unique key.
        let secs_in_15min = 15 * 60;
        // Choose a timestamp partway through a 15-min window: floor
        // should round down to the window start.
        let mid_window = 1_700_000_000_i64 + 7 * 60 + 33; // +07:33 into a window
        let bucketed = bucket_floor(mid_window);
        let next = bucket_floor(mid_window + 60); // +1 min later
        assert_eq!(
            bucketed, next,
            "two timestamps in the same 15-min window must bucket to the same value"
        );
        // Crossing a 15-min boundary changes the bucket.
        let crossed = bucket_floor(mid_window + secs_in_15min);
        assert_ne!(bucketed, crossed);
        // Bucket value is always a multiple of 15 minutes.
        assert_eq!(bucketed % secs_in_15min, 0);
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
        // Preferred is empty; should walk romaji → english → native.
        assert_eq!(
            pick_title("english", "Romaji", "", "Native", "DB"),
            "Romaji"
        );
        assert_eq!(pick_title("english", "", "", "Native", "DB"), "Native");
        // All language fields empty — final fallback to db title.
        assert_eq!(pick_title("english", "", "", "", "DB"), "DB");
        assert_eq!(pick_title("english", "", "", "", ""), "");
    }

    #[tokio::test]
    async fn empty_series_index_returns_empty_without_al_call() {
        // Empty DB → no AL request fires (would 401 in test env
        // anyway since there's no wiremock fixture set up here),
        // function returns an empty Vec immediately.
        reset_cache();
        let pool = crate::test_support::in_memory_pool().await;
        let cfg = Config::default();
        let result = fetch_upcoming(&pool, &cfg, 0, 1_000_000, true).await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_empty());
    }

    #[tokio::test]
    async fn negative_anilist_ids_are_filtered_from_index() {
        reset_cache();
        let pool = crate::test_support::in_memory_pool().await;
        sqlx::query("INSERT INTO series (anilist_id, title, monitor_mode) VALUES (?, ?, 'all')")
            .bind(-12345_i64) // Jikan-fallback negative-id sentinel
            .bind("MAL-only series")
            .execute(&pool)
            .await
            .unwrap();
        let rows = load_series_index(&pool, &Config::default(), true)
            .await
            .unwrap();
        assert!(
            rows.is_empty(),
            "negative-id sentinel rows should be filtered out"
        );
    }

    #[tokio::test]
    async fn monitored_only_filters_none_rows() {
        reset_cache();
        let pool = crate::test_support::in_memory_pool().await;
        sqlx::query("INSERT INTO series (anilist_id, title, monitor_mode) VALUES (?, ?, 'all')")
            .bind(1_i64)
            .bind("Watched")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO series (anilist_id, title, monitor_mode) VALUES (?, ?, 'none')")
            .bind(2_i64)
            .bind("Ignored")
            .execute(&pool)
            .await
            .unwrap();

        let monitored = load_series_index(&pool, &Config::default(), true)
            .await
            .unwrap();
        assert_eq!(monitored.len(), 1);
        assert_eq!(monitored[0].anilist_id, 1);

        let all = load_series_index(&pool, &Config::default(), false)
            .await
            .unwrap();
        assert_eq!(all.len(), 2);
    }
}
