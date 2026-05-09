//! iCal calendar feed (issue #115).
//!
//! `GET /api/calendar.ics` — returns an iCalendar 2.0 document
//! covering the next N days of upcoming episodes (default 30) for
//! the user's library. Subscribed by Google Calendar / Apple
//! Calendar / Thunderbird via the per-key URL surfaced in the
//! Settings → Calendar panel.
//!
//! Auth: `calendar`-scoped API key (`require_calendar_scope`
//! middleware in `handlers::scoped_auth`). Calendar subscribers
//! can't carry cookies, which is why the scoped-key system in
//! #114 had to land first.
//!
//! ## Output shape
//!
//! Hand-rolled iCalendar 2.0 text (no `ics`-crate dependency —
//! the format is small and the round-trip we care about is just
//! "RFC-5545 compatible enough for Google + Apple + Thunderbird").
//!
//! Per VEVENT:
//! - `SUMMARY`: `<series_title> S01E<episode>` (anime convention =
//!   always Season 1; matches `services/post_processing/mod.rs`'s
//!   on-disk naming shape).
//! - `DTSTART` / `DTEND`: from `airing_at` + `duration_minutes`.
//! - `UID`: `ryokan-<series_id>-<episode>@ryokan.local` — stable
//!   across feed fetches so calendar clients dedupe.
//! - `DESCRIPTION`: monitoring state + grabbed status.
//! - `URL`: deep link back to `/series/{id}` on the request's host
//!   (best-effort; falls back to a relative URL if the host can't
//!   be resolved).
//! - `STATUS`: `TENTATIVE` for episodes >7 days out (AL airing
//!   schedules can shift), `CONFIRMED` for the next 7 days.
//!
//! ## Caching
//!
//! Server-side: 15-min cache in `services::calendar`.
//! HTTP-side: `Cache-Control: public, max-age=600` + `ETag` derived
//! from `max(airing_at)` so calendar clients honor conditional GETs
//! for free.

use askama::Template;
use axum::{
    extract::{Query, State},
    http::{HeaderMap, HeaderName, StatusCode, header},
    response::{Html, IntoResponse, Response},
};
use axum_htmx::HxRequest;
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::models::config;
use crate::services::calendar::{self, DEFAULT_FORWARD_DAYS, UpcomingEpisode};

const NOW_PLUS_7_DAYS_THRESHOLD: i64 = 7 * 86400;

/// "Next week" needs an offset start (skip the first 7 days). All
/// other ranges start from now. Returns `(from_offset_days,
/// length_days)`.
fn range_to_window(range: &str) -> (i64, i64) {
    match range {
        "next_week" | "next-week" => (7, 7),
        "month" => (0, 30),
        _ => (0, 7),
    }
}

#[derive(Debug, Deserialize)]
pub struct CalendarPageQuery {
    /// `this_week` (default), `next_week`, or `month`.
    #[serde(default)]
    pub range: Option<String>,
    /// `?monitored=true` filters to only monitored series. Default
    /// off — surfaces every airing series.
    #[serde(default)]
    pub monitored: Option<bool>,
}

#[derive(Template)]
#[template(path = "calendar.html")]
struct CalendarPageTemplate {
    page: String,
    title_language: String,
    /// Active range token — drives the toggle's selected state.
    range: String,
    monitored_only: bool,
    /// Pre-grouped day buckets — the template iterates these
    /// directly. HTMX swaps render the same `partials/calendar/
    /// list.html` partial against this same shape, so the
    /// initial paint and the swap paint produce identical
    /// markup.
    day_buckets: Vec<DayBucket>,
    /// Calendar-scoped API keys for the Subscribe section. Filtered
    /// to enabled keys with the `calendar` or `admin` scope so
    /// users only see ones that'd actually authorize the feed.
    calendar_keys: Vec<CalendarKeyOption>,
    /// True when the user has at least one positive-AL-id series in
    /// their library; drives the empty-state copy ("add a series"
    /// vs. "no episodes airing in this range").
    library_is_empty: bool,
}

/// Per-episode wire shape for both the server template and the
/// JSON endpoint. Pre-formats per-day-grouping fields so the
/// client doesn't have to re-derive them.
#[derive(Debug, Clone, Serialize)]
pub struct EpisodeView {
    pub series_id: i64,
    pub series_title: String,
    pub cover_url: String,
    pub episode: i32,
    /// Unix epoch seconds (UTC). The client renders this in the
    /// user's local timezone via `new Date(unixTs * 1000)`.
    pub airing_at: i64,
    pub monitored: bool,
    /// Lowercase concatenation of every title variant (romaji +
    /// english + native + db-stored) so the page's series-name
    /// search input matches against any of them, not just the
    /// resolved `title_language` form. Server-precomputed once.
    pub search_haystack: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct DayBucket {
    /// Render-ready day label, e.g. `"Monday, May 12"`. Server-
    /// formatted in UTC for the initial render; the client may
    /// re-group by browser-local date if it cares (most won't —
    /// the day boundary at UTC matches the airingAt value the
    /// browser would compute back).
    pub label: String,
    /// UTC midnight Unix timestamp for this bucket's day. Used
    /// client-side to highlight the today-section and let the
    /// initial-load auto-scroll find it.
    pub day_key: i64,
    pub episodes: Vec<EpisodeView>,
}

#[derive(Debug, Clone, Serialize)]
struct CalendarKeyOption {
    id: i64,
    name: String,
}

/// `GET /calendar` — the in-app calendar page. Cookie-auth gated
/// (sits inside the `protected_routes` group).
///
/// Branches on `HxRequest`:
/// - `HX-Request: true` → renders just the `partials/calendar/list.html`
///   partial. Used by the range-tab swap path so changing the range
///   only replaces `#calendar-list` instead of the whole body.
/// - Plain GET → renders the full page. Used on direct URL hits,
///   browser back/forward to the page, and the no-JS fallback for
///   the range tabs.
pub async fn page(
    State(state): State<AppState>,
    HxRequest(is_htmx): HxRequest,
    Query(params): Query<CalendarPageQuery>,
) -> Html<String> {
    let cfg = config::get_config(&state.db)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    let range = params.range.unwrap_or_else(|| "this_week".to_string());
    let monitored_only = params.monitored.unwrap_or(false);

    let (offset_days, length_days) = range_to_window(&range);
    let now = chrono::Utc::now().timestamp();
    let from = now + offset_days * 86400;
    let to = from + length_days * 86400;

    let episodes = calendar::fetch_upcoming(&state.db, &cfg, from, to, monitored_only)
        .await
        .unwrap_or_default();

    let library_is_empty = library_has_no_positive_al_series(&state.db).await;

    let episode_views: Vec<EpisodeView> = episodes
        .into_iter()
        .map(|e| EpisodeView {
            series_id: e.series_id,
            series_title: e.series_title,
            cover_url: e.cover_url,
            episode: e.episode,
            airing_at: e.airing_at,
            monitored: e.monitored,
            search_haystack: e.search_haystack,
        })
        .collect();

    let day_buckets = group_by_day(&episode_views);

    // HTMX request → just the list partial (the swappable
    // region inside #calendar-list). Skips the calendar_keys
    // load + the page chrome since neither belongs in the
    // partial.
    if is_htmx {
        let partial = CalendarListPartial {
            day_buckets,
            library_is_empty,
        };
        return Html(partial.render().unwrap_or_default());
    }

    // Full page render — includes chrome (range tabs, filters,
    // iCal modal) plus the partial body.
    let calendar_keys = collect_calendar_keys(&state.db).await;
    let tmpl = CalendarPageTemplate {
        page: "calendar".to_string(),
        title_language: cfg.title_language.clone(),
        range,
        monitored_only,
        day_buckets,
        calendar_keys,
        library_is_empty,
    };
    Html(tmpl.render().unwrap_or_default())
}

#[derive(Template)]
#[template(path = "partials/calendar/list.html")]
struct CalendarListPartial {
    day_buckets: Vec<DayBucket>,
    library_is_empty: bool,
}

async fn library_has_no_positive_al_series(db: &sqlx::SqlitePool) -> bool {
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM series WHERE anilist_id > 0")
        .fetch_one(db)
        .await
        .unwrap_or(0);
    count == 0
}

async fn collect_calendar_keys(db: &sqlx::SqlitePool) -> Vec<CalendarKeyOption> {
    let keys = crate::models::api_key::list(db).await.unwrap_or_default();
    keys.into_iter()
        .filter(|k| k.enabled && k.scopes.iter().any(|s| s == "calendar" || s == "admin"))
        .map(|k| CalendarKeyOption {
            id: k.id,
            name: k.name,
        })
        .collect()
}

/// Group a flat episode list into day buckets keyed by the date
/// portion of `airing_at` (UTC). Server-side grouping so the
/// initial render is one pass; the JS-driven re-render uses the
/// same logic against the JSON wire shape.
fn group_by_day(episodes: &[EpisodeView]) -> Vec<DayBucket> {
    use std::collections::BTreeMap;
    let mut by_date: BTreeMap<i64, Vec<EpisodeView>> = BTreeMap::new();
    for ep in episodes {
        // Collapse to UTC midnight so two episodes on the same UTC
        // date sort into the same bucket.
        let day_key = ep.airing_at - (ep.airing_at.rem_euclid(86400));
        by_date.entry(day_key).or_default().push(ep.clone());
    }
    by_date
        .into_iter()
        .map(|(day_key, eps)| DayBucket {
            label: chrono::DateTime::<chrono::Utc>::from_timestamp(day_key, 0)
                .map(|dt| dt.format("%A, %b %-d").to_string())
                .unwrap_or_default(),
            day_key,
            episodes: eps,
        })
        .collect()
}

/// Query string for the iCal endpoint. Both fields opt-in; the
/// default behavior is "next 30 days, every airing series."
#[derive(Debug, Deserialize)]
pub struct IcalQuery {
    /// Forward window in days. Capped at 90 server-side so a
    /// `?days=10000` request can't blow up the AL fetch budget.
    #[serde(default)]
    pub days: Option<i64>,
    /// `?monitored=true` filters to only monitored series. Default
    /// off — the unconditional default surfaces every airing
    /// series so users can browse "what's coming up" beyond their
    /// own list.
    #[serde(default)]
    pub monitored: Option<bool>,
}

const MAX_DAYS: i64 = 90;

/// `GET /api/calendar.ics`. Wired in `main.rs` behind the
/// `require_calendar_scope` middleware so only `calendar`-scoped
/// API keys reach it.
pub async fn ical_feed(
    State(state): State<AppState>,
    Query(params): Query<IcalQuery>,
    headers: HeaderMap,
) -> Response {
    let cfg = match config::get_config(&state.db).await {
        Ok(Some(c)) => c,
        Ok(None) | Err(_) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                [(header::RETRY_AFTER, "5")],
                "Ryokan config not yet available",
            )
                .into_response();
        }
    };

    let days = params
        .days
        .unwrap_or(DEFAULT_FORWARD_DAYS)
        .clamp(1, MAX_DAYS);
    let monitored_only = params.monitored.unwrap_or(false);
    let now = chrono::Utc::now().timestamp();
    let from = now;
    let to = now + days * 86400;

    let episodes = match calendar::fetch_upcoming(&state.db, &cfg, from, to, monitored_only).await {
        Ok(v) => v,
        Err(e) => {
            // Surface the AL failure-prefix taxonomy as the right
            // HTTP shape: 503 + Retry-After for transient issues so
            // calendar clients back off (they rage-poll on 401 less
            // than on 5xx). Pure 4xx for misconfig isn't reachable
            // here — the auth middleware already gated; any error
            // bubbling out is upstream.
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                [(header::RETRY_AFTER, "60")],
                format!("Calendar fetch failed: {e}"),
            )
                .into_response();
        }
    };

    let host = extract_host(&headers);
    let body = render_ical(&episodes, &cfg.title_language, host.as_deref(), now);
    let etag = etag_for(&episodes);

    // Conditional GET — if the client sent the same etag they're
    // already showing, return 304 with empty body.
    if let Some(if_none_match) = headers.get(header::IF_NONE_MATCH)
        && let Ok(s) = if_none_match.to_str()
        && s == etag.as_str()
    {
        return (StatusCode::NOT_MODIFIED, [(header::ETAG, etag.as_str())]).into_response();
    }

    let mut response_headers = vec![
        (
            header::CONTENT_TYPE,
            "text/calendar; charset=utf-8".to_string(),
        ),
        (header::CACHE_CONTROL, "public, max-age=600".to_string()),
        (header::ETAG, etag),
    ];
    // `Content-Disposition` so a direct browser hit downloads as
    // ryokan.ics rather than rendering inline as text/plain. Some
    // calendar apps (Apple Calendar, Outlook) want the file path
    // to end in `.ics` for their auto-import handlers.
    response_headers.push((
        HeaderName::from_static("content-disposition"),
        "inline; filename=\"ryokan.ics\"".to_string(),
    ));

    let header_pairs: Vec<(HeaderName, String)> = response_headers;
    let mut builder = Response::builder().status(StatusCode::OK);
    for (name, value) in header_pairs {
        builder = builder.header(name, value);
    }
    builder.body(body.into()).unwrap_or_else(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to build response",
        )
            .into_response()
    })
}

/// Best-effort extraction of the request's external host so the
/// iCal `URL` field can deep-link back to the series page.
/// Prefers `X-Forwarded-Host` then `Host`; returns None when
/// neither header is parseable, in which case the URL field is
/// emitted as a relative path.
fn extract_host(headers: &HeaderMap) -> Option<String> {
    if let Some(h) = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get(header::HOST))
        && let Ok(s) = h.to_str()
    {
        return Some(s.to_string());
    }
    None
}

/// Hand-rolled iCalendar 2.0 text. RFC 5545 compatible enough for
/// Google Calendar / Apple Calendar / Thunderbird auto-subscribe;
/// not a complete implementation (no recurring events, no VTIMEZONE,
/// no per-event TIMEZONE — every DTSTART is plain UTC `Z`-suffixed).
fn render_ical(
    episodes: &[UpcomingEpisode],
    _title_language: &str,
    host: Option<&str>,
    now_unix: i64,
) -> String {
    // CRLF line endings per RFC 5545 §3.1. Some clients are lax
    // about LF, but Apple Calendar specifically rejects mixed.
    let mut out = String::with_capacity(256 + episodes.len() * 256);
    out.push_str("BEGIN:VCALENDAR\r\n");
    out.push_str("VERSION:2.0\r\n");
    out.push_str("PRODID:-//Ryokan//Calendar 1.0//EN\r\n");
    out.push_str("CALSCALE:GREGORIAN\r\n");
    out.push_str("METHOD:PUBLISH\r\n");
    out.push_str("X-WR-CALNAME:Ryokan\r\n");

    for ep in episodes {
        let start_utc = format_ical_utc(ep.airing_at);
        let duration_secs = (ep.duration_minutes.max(1) as i64) * 60;
        let end_utc = format_ical_utc(ep.airing_at + duration_secs);
        let summary = format!("{} S01E{:02}", ep.series_title, ep.episode);
        let uid = format!("ryokan-{}-{}@ryokan.local", ep.series_id, ep.episode);
        let status = if ep.airing_at - now_unix > NOW_PLUS_7_DAYS_THRESHOLD {
            "TENTATIVE"
        } else {
            "CONFIRMED"
        };
        let mon_label = if ep.monitored {
            "Monitored"
        } else {
            "Not monitored"
        };
        let description = format!("AniList ID: {}\\n{}", ep.anilist_id, mon_label);
        let url = match host {
            Some(h) => format!("http://{}/series/{}", h, ep.series_id),
            None => format!("/series/{}", ep.series_id),
        };

        out.push_str("BEGIN:VEVENT\r\n");
        // DTSTAMP is required per RFC 5545; use now as the
        // server-side stamp. Some validators reject events
        // without it.
        out.push_str(&format!("DTSTAMP:{}\r\n", format_ical_utc(now_unix)));
        out.push_str(&format!("UID:{}\r\n", escape_ical_text(&uid)));
        out.push_str(&format!("DTSTART:{}\r\n", start_utc));
        out.push_str(&format!("DTEND:{}\r\n", end_utc));
        out.push_str(&format!("SUMMARY:{}\r\n", escape_ical_text(&summary)));
        out.push_str(&format!(
            "DESCRIPTION:{}\r\n",
            escape_ical_text(&description)
        ));
        out.push_str(&format!("URL:{}\r\n", escape_ical_text(&url)));
        out.push_str(&format!("STATUS:{}\r\n", status));
        out.push_str("END:VEVENT\r\n");
    }

    out.push_str("END:VCALENDAR\r\n");
    out
}

/// Format a Unix epoch seconds value as an RFC 5545 UTC timestamp:
/// `YYYYMMDDTHHMMSSZ`. The `Z` suffix marks UTC; without it the
/// time gets interpreted as floating local time and shows up
/// shifted in subscribers' calendars.
fn format_ical_utc(unix_secs: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp(unix_secs, 0)
        .map(|dt| dt.format("%Y%m%dT%H%M%SZ").to_string())
        .unwrap_or_else(|| "19700101T000000Z".to_string())
}

/// Escape per RFC 5545 §3.3.11. Backslash, comma, semicolon get
/// escaped; literal newlines become `\n`. Carriage returns are
/// dropped (they'd be re-introduced by the line wrapper).
fn escape_ical_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            ',' => out.push_str("\\,"),
            ';' => out.push_str("\\;"),
            '\n' => out.push_str("\\n"),
            '\r' => {}
            _ => out.push(ch),
        }
    }
    out
}

/// Build an ETag from `max(airing_at)` across the included events.
/// Conditional-GET-friendly because the same set of events
/// produces the same etag, and adding/removing events shifts the
/// max (or the count, accounted for via the prefix).
fn etag_for(episodes: &[UpcomingEpisode]) -> String {
    let max_airing = episodes.iter().map(|e| e.airing_at).max().unwrap_or(0);
    format!("\"{}-{}\"", episodes.len(), max_airing)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ep(
        series_id: i64,
        anilist_id: i32,
        episode: i32,
        airing_at: i64,
        monitored: bool,
    ) -> UpcomingEpisode {
        UpcomingEpisode {
            series_id,
            anilist_id,
            series_title: "Test Series".to_string(),
            episode,
            airing_at,
            duration_minutes: 24,
            monitored,
            cover_url: String::new(),
            search_haystack: "test series".to_string(),
        }
    }

    #[test]
    fn empty_calendar_renders_valid_skeleton() {
        let body = render_ical(&[], "romaji", None, 0);
        assert!(body.starts_with("BEGIN:VCALENDAR\r\n"));
        assert!(body.contains("VERSION:2.0\r\n"));
        assert!(body.contains("PRODID:-//Ryokan//Calendar 1.0//EN\r\n"));
        assert!(body.ends_with("END:VCALENDAR\r\n"));
        assert!(!body.contains("BEGIN:VEVENT"));
    }

    #[test]
    fn vevent_carries_uid_dtstart_dtend_status() {
        let now = 1_700_000_000_i64; // somewhere in 2023
        let body = render_ical(
            &[ep(42, 100, 7, now + 3 * 86400, true)],
            "romaji",
            Some("ryokan.example:8978"),
            now,
        );
        assert!(body.contains("UID:ryokan-42-7@ryokan.local\r\n"));
        // 3 days out → CONFIRMED, not TENTATIVE.
        assert!(body.contains("STATUS:CONFIRMED\r\n"));
        // DTSTART is the UTC airing time.
        assert!(body.contains("DTSTART:"));
        assert!(body.contains("DTEND:"));
        // SUMMARY is `<title> S01E<NN>` zero-padded.
        assert!(body.contains("SUMMARY:Test Series S01E07\r\n"));
        // URL uses the host header.
        assert!(body.contains("URL:http://ryokan.example:8978/series/42\r\n"));
    }

    #[test]
    fn far_out_episodes_get_tentative_status() {
        let now = 1_700_000_000_i64;
        // 14 days out → past the 7-day threshold → TENTATIVE.
        let body = render_ical(&[ep(1, 1, 1, now + 14 * 86400, true)], "romaji", None, now);
        assert!(body.contains("STATUS:TENTATIVE\r\n"));
    }

    #[test]
    fn etag_is_stable_for_same_events() {
        let now = 1_700_000_000_i64;
        let evs = vec![
            ep(1, 1, 1, now + 86400, true),
            ep(2, 2, 1, now + 2 * 86400, false),
        ];
        let a = etag_for(&evs);
        let b = etag_for(&evs);
        assert_eq!(a, b);
    }

    #[test]
    fn etag_changes_when_max_airing_changes() {
        let now = 1_700_000_000_i64;
        let a = etag_for(&[ep(1, 1, 1, now + 86400, true)]);
        let b = etag_for(&[ep(1, 1, 1, now + 2 * 86400, true)]);
        assert_ne!(a, b);
    }

    #[test]
    fn description_text_escape_handles_special_chars() {
        let escaped = escape_ical_text("a, b; c\\d\nE");
        assert_eq!(escaped, "a\\, b\\; c\\\\d\\nE");
    }

    #[test]
    fn url_falls_back_to_relative_when_no_host() {
        let now = 1_700_000_000_i64;
        let body = render_ical(&[ep(99, 1, 1, now + 3600, true)], "romaji", None, now);
        assert!(body.contains("URL:/series/99\r\n"));
    }
}
