//! MyAnimeList watch-list fetch + access-token refresh (issue #62
//! PR B).
//!
//! The OAuth handlers in `handlers::oauth` already speak MAL for the
//! token-exchange + `@me` calls during the link flow. This module
//! adds the two operations the sync task needs:
//!
//!   1. `fetch_animelist(token)` — paginated GET against
//!      `/v2/users/@me/animelist`, follows `paging.next` URLs to
//!      drain every entry in one logical call, with a per-request
//!      pause that keeps the sync polite even when MAL says nothing.
//!      Returns `Vec<MalAnimeListEntry>` projected to the fields
//!      the sync engine merges (media id, status, progress, score,
//!      updated_at).
//!
//!   2. `refresh_access_token(refresh_token)` — POST to
//!      `/v1/oauth2/token` with `grant_type=refresh_token`. MAL
//!      access tokens expire every 30 days; the sync task calls
//!      this on a 401 from `fetch_animelist` and persists the new
//!      tokens via `models::external_accounts::update_tokens`
//!      before retrying the fetch once.
//!
//! Both functions reuse a shared `LazyLock<reqwest::Client>` so
//! repeated sync ticks don't pay the DNS + TLS handshake cost on
//! every call. Same convention as `services::rss::feed::RSS_HTTP_CLIENT`.

use std::sync::LazyLock;
use std::time::Duration;

use serde::Deserialize;

/// MAL public client ID (App Type `other`). Same value used in
/// `handlers::oauth`'s OAuth flow; duplicated here so the sync
/// path doesn't have to depend on a handler module. Keep in sync
/// if AL/MAL forces a re-registration.
const MAL_CLIENT_ID: &str = "5205ccde38839a4afc6b03bbecfaa9c7";

/// Per-request pause between paginated MAL fetches. The plan's
/// "MAL uses a per-request delay" note (decision #4) lands here.
/// 1 second is well clear of MAL's documented 5 req/s cap and
/// matches the ratelimit-sensitivity wedge used elsewhere in the
/// project for politeness vs throughput.
const MAL_REQUEST_DELAY: Duration = Duration::from_secs(1);

/// Page size on `/v2/users/@me/animelist`. Spec says 1000 max but
/// MAL has been observed to silently cap responses around 100-500
/// in the wild; let MAL truncate and follow `paging.next` rather
/// than guessing the live cap.
const MAL_PAGE_SIZE: u32 = 1000;

static HTTP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("MAL reqwest client build")
});

/// Token-endpoint response shape. Same shape as the link-flow's
/// `MalTokenResponse` in `handlers::oauth` — duplicated as `pub`
/// here because the sync task on token refresh persists the same
/// fields back via `external_accounts::update_tokens`.
#[derive(Debug, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub refresh_token: String,
    /// Seconds-until-expiry. MAL issues 30-day access tokens
    /// (`expires_in == 2592000`) on both initial exchange and
    /// refresh.
    pub expires_in: i64,
}

/// One entry from a MAL watch list, projected to the fields the
/// sync engine merges. Mirrors `AniListMediaListEntry`'s shape so
/// the sync engine can dispatch on provider and treat both
/// uniformly. Token-refresh state lives at the connection-handle
/// layer (`fetch_animelist` returns `MalFetchError::Unauthorized`
/// distinctly so the caller knows when to refresh + retry).
#[derive(Debug, Clone)]
pub struct MalAnimeListEntry {
    /// MAL media id. The sync engine maps this to AniList via the
    /// existing `anibridge` resolver before merging into `series`;
    /// entries that don't resolve fall into the negated-id sentinel
    /// path (per the project's `jikan_fallback_seadex_blindspot`
    /// memory).
    pub media_id: i64,
    /// MAL status string: `watching`, `completed`, `on_hold`,
    /// `dropped`, `plan_to_watch`. Lowercase + snake_case unlike
    /// AL's SHOUTING. The sync engine maps to its abstract list
    /// taxonomy.
    pub status: String,
    /// `num_episodes_watched`.
    pub progress: i64,
    /// Integer 0-10 on MAL (no half-step format). Stored as f64
    /// for parity with the AL entry.
    pub score: f64,
    /// `updated_at` parsed from MAL's RFC 3339 / ISO 8601 string
    /// into Unix epoch seconds. Used for delta filtering against
    /// `external_accounts.list_last_synced_at`.
    pub updated_at: i64,
}

/// Distinct error case so the sync task can refresh + retry on
/// 401 without parsing the error string.
#[derive(Debug)]
pub enum MalFetchError {
    /// The access token is dead. Caller should run
    /// `refresh_access_token`, persist the new tokens, and retry
    /// once before surfacing failure.
    Unauthorized,
    /// Anything else: rate-limited, malformed response, transport
    /// error. The sync task surfaces the message and waits for
    /// the next tick.
    Other(String),
}

impl std::fmt::Display for MalFetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unauthorized => write!(f, "MAL rejected the access token (401)"),
            Self::Other(msg) => write!(f, "MAL fetch failed: {msg}"),
        }
    }
}

/// Drain `/v2/users/@me/animelist`, following `paging.next` URLs
/// until exhausted. One HTTP request per page, 1s pause between
/// pages.
///
/// Token-refresh policy: a 401 on ANY page returns
/// `MalFetchError::Unauthorized` immediately without burning more
/// pagination requests on a dead token. The caller refreshes,
/// updates `external_accounts.access_token_encrypted`, and calls
/// this function again with the new token.
pub async fn fetch_animelist(token: &str) -> Result<Vec<MalAnimeListEntry>, MalFetchError> {
    let mut next_url: Option<String> = Some(format!(
        "https://api.myanimelist.net/v2/users/@me/animelist?fields=list_status&limit={MAL_PAGE_SIZE}&nsfw=true"
    ));
    let mut out: Vec<MalAnimeListEntry> = Vec::new();
    let mut first_page = true;

    while let Some(url) = next_url.take() {
        if !first_page {
            // Politeness pause between pages. First page fires
            // immediately; subsequent ones wait.
            tokio::time::sleep(MAL_REQUEST_DELAY).await;
        }
        first_page = false;

        let resp = HTTP_CLIENT
            .get(&url)
            .header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"))
            .header(reqwest::header::USER_AGENT, "Ryokan/0.1")
            .send()
            .await
            .map_err(|e| MalFetchError::Other(format!("HTTP error: {e}")))?;

        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(MalFetchError::Unauthorized);
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(MalFetchError::Other(format!(
                "status {status}: {}",
                excerpt(&body)
            )));
        }

        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| MalFetchError::Other(format!("response parse: {e}")))?;

        let data = body
            .get("data")
            .and_then(|v| v.as_array())
            .ok_or_else(|| MalFetchError::Other("response missing `data` array".into()))?;

        for entry in data {
            let Some(node) = entry.get("node") else {
                continue;
            };
            let Some(media_id) = node.get("id").and_then(|v| v.as_i64()) else {
                continue;
            };
            let list_status = entry.get("list_status");
            let status = list_status
                .and_then(|s| s.get("status"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let progress = list_status
                .and_then(|s| s.get("num_episodes_watched"))
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let score = list_status
                .and_then(|s| s.get("score"))
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            let updated_at = list_status
                .and_then(|s| s.get("updated_at"))
                .and_then(|v| v.as_str())
                .and_then(parse_rfc3339_to_unix)
                .unwrap_or(0);

            out.push(MalAnimeListEntry {
                media_id,
                status,
                progress,
                score,
                updated_at,
            });
        }

        next_url = body
            .pointer("/paging/next")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
    }

    Ok(out)
}

/// POST to `/v1/oauth2/token` with `grant_type=refresh_token`.
/// MAL returns a fresh access_token + refresh_token + expires_in
/// (the refresh_token DOES rotate per call, despite the OAuth 2.0
/// spec leaving it optional — verified 2026-04-22 against MAL's
/// production endpoint). Caller persists both new values via
/// `external_accounts::update_tokens` so the next sync tick uses
/// the rotated refresh_token.
pub async fn refresh_access_token(refresh_token: &str) -> Result<TokenResponse, String> {
    let resp = HTTP_CLIENT
        .post("https://myanimelist.net/v1/oauth2/token")
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .header(reqwest::header::USER_AGENT, "Ryokan/0.1")
        .form(&[
            ("client_id", MAL_CLIENT_ID),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
        ])
        .send()
        .await
        .map_err(|e| format!("MAL refresh HTTP error: {e}"))?;

    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| format!("MAL refresh response read failed: {e}"))?;

    if !status.is_success() {
        // 400 with `invalid_grant` means the refresh token itself is
        // dead (revoked, or 1-year expiry hit). Caller should treat
        // as "user must re-link" rather than retrying — surface the
        // message verbatim so the eventual UI banner can read it.
        return Err(format!("MAL refresh returned {status}: {}", excerpt(&body)));
    }

    serde_json::from_str::<TokenResponse>(&body).map_err(|e| {
        format!(
            "MAL refresh response parse failed: {e} (body: {})",
            excerpt(&body)
        )
    })
}

/// Truncate a body excerpt for log messages. MAL's error responses
/// can carry HTML in some failure modes; capping at 240 chars
/// keeps the `logs` table from accumulating multi-KB blobs.
fn excerpt(s: &str) -> String {
    if s.len() <= 240 {
        s.to_string()
    } else {
        format!("{}…", &s[..240])
    }
}

/// Parse an RFC 3339 / ISO 8601 timestamp string into Unix epoch
/// seconds. MAL returns timestamps like `"2026-04-25T18:32:11+00:00"`
/// on `list_status.updated_at`. Returns `None` on parse failure;
/// caller treats that as "no updated_at known" and skips delta
/// filtering for that entry.
fn parse_rfc3339_to_unix(s: &str) -> Option<i64> {
    // Avoid pulling in a date-parsing dep just for this. The shape
    // MAL emits is `YYYY-MM-DDTHH:MM:SS±HH:MM` (or `Z` for UTC) —
    // we only need the date components and the timezone offset, all
    // ASCII-numeric. A hand-roll keeps the parser narrow and fast.
    let bytes = s.as_bytes();
    if bytes.len() < 19 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return None;
    }
    let parse = |range: std::ops::Range<usize>| -> Option<i64> {
        std::str::from_utf8(&bytes[range])
            .ok()
            .and_then(|s| s.parse().ok())
    };
    let year: i64 = parse(0..4)?;
    let month: i64 = parse(5..7)?;
    let day: i64 = parse(8..10)?;
    let hour: i64 = parse(11..13)?;
    let minute: i64 = parse(14..16)?;
    let second: i64 = parse(17..19)?;

    // Timezone suffix: 'Z' means UTC, otherwise `+HH:MM` or `-HH:MM`.
    let mut offset_secs: i64 = 0;
    if bytes.len() > 19 {
        let tz = &s[19..];
        if tz != "Z" && tz != "+00:00" && !tz.is_empty() {
            let sign: i64 = if tz.starts_with('-') { -1 } else { 1 };
            let tz = tz.trim_start_matches(['+', '-']);
            let parts: Vec<&str> = tz.split(':').collect();
            if parts.len() == 2 {
                let h: i64 = parts[0].parse().ok()?;
                let m: i64 = parts[1].parse().ok()?;
                offset_secs = sign * (h * 3600 + m * 60);
            }
        }
    }

    // Days since 1970-01-01 via the standard civil-from-days
    // formula (Howard Hinnant's "date" algorithm). Avoids pulling
    // in a date crate; the formula handles every Gregorian leap
    // year correctly through the year 9999.
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * m + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days_since_epoch = era * 146097 + doe - 719468;

    let utc_seconds = days_since_epoch * 86400 + hour * 3600 + minute * 60 + second;
    Some(utc_seconds - offset_secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rfc3339_utc_z() {
        // MAL has been observed emitting both `Z` and `+00:00` for
        // UTC; both should produce the same epoch.
        let z = parse_rfc3339_to_unix("2026-04-25T18:32:11Z").unwrap();
        let plus = parse_rfc3339_to_unix("2026-04-25T18:32:11+00:00").unwrap();
        assert_eq!(z, plus);
        // Spot-check via a known reference value: 2026-04-25
        // 18:32:11 UTC = 1777141931 (computed: 2024-01-01 epoch
        // 1704067200, +366 days for 2024, +365 days for 2025, +90
        // days for Q1 2026, +24 days for Apr 1..25, + 18h32m11s).
        assert_eq!(z, 1777141931);
    }

    #[test]
    fn parse_rfc3339_with_positive_offset() {
        // 2026-04-25 18:32:11+09:00 == 2026-04-25 09:32:11 UTC,
        // which is 9 hours earlier.
        let plus_9 = parse_rfc3339_to_unix("2026-04-25T18:32:11+09:00").unwrap();
        let utc = parse_rfc3339_to_unix("2026-04-25T09:32:11Z").unwrap();
        assert_eq!(plus_9, utc);
    }

    #[test]
    fn parse_rfc3339_with_negative_offset() {
        // 2026-04-25 18:32:11-05:00 == 2026-04-25 23:32:11 UTC.
        let minus_5 = parse_rfc3339_to_unix("2026-04-25T18:32:11-05:00").unwrap();
        let utc = parse_rfc3339_to_unix("2026-04-25T23:32:11Z").unwrap();
        assert_eq!(minus_5, utc);
    }

    #[test]
    fn parse_rfc3339_returns_none_on_garbage() {
        assert!(parse_rfc3339_to_unix("").is_none());
        assert!(parse_rfc3339_to_unix("not a date").is_none());
        // Too short to even contain the date component.
        assert!(parse_rfc3339_to_unix("2026-04").is_none());
        // Wrong separators.
        assert!(parse_rfc3339_to_unix("2026/04/25T18:32:11Z").is_none());
    }

    #[test]
    fn parse_rfc3339_handles_leap_day() {
        // 2024-02-29 12:00:00Z is a real instant; the civil-from-days
        // formula needs to handle leap years correctly. Reference
        // value computed offline: 1709208000.
        assert_eq!(
            parse_rfc3339_to_unix("2024-02-29T12:00:00Z").unwrap(),
            1709208000
        );
    }

    #[test]
    fn excerpt_caps_long_strings() {
        let big = "x".repeat(500);
        let short = excerpt(&big);
        // 240 ASCII bytes + the 3-byte ellipsis (U+2026 in UTF-8) =
        // 243 bytes total. Cap is `len()` (byte length); the assert
        // pins the upper bound exactly so a regression that switches
        // to a longer suffix character or a larger cap fails loudly.
        assert_eq!(short.len(), 243);
        assert!(short.ends_with('…'));
    }

    #[test]
    fn excerpt_passes_short_strings_through() {
        assert_eq!(excerpt("ok"), "ok");
        assert_eq!(excerpt(""), "");
    }
}
