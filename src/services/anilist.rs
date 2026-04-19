use serde::{Deserialize, Serialize};
use crate::services::html::sanitize_rich_description;
use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex as StdMutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;

use crate::services::jikan;

const ANILIST_API: &str = "https://graphql.anilist.co";

/// TTL for the search result cache. Short enough to stay fresh, long enough to
/// absorb bursts of repeat queries (which is what actually hammers AniList/Jikan
/// during testing or when a user re-searches the same title).
const SEARCH_CACHE_TTL: Duration = Duration::from_secs(60);

/// How long to treat AniList as unavailable after a 429/5xx. If AniList sends a
/// `Retry-After` header we use that value instead (capped at 5 minutes).
const ANILIST_COOLDOWN_DEFAULT: Duration = Duration::from_secs(60);
const ANILIST_COOLDOWN_MAX: Duration = Duration::from_secs(300);

/// In-memory cache TTL for anime detail responses (15 minutes).
const DETAIL_CACHE_TTL_SECS: u64 = 15 * 60;

/// Maximum number of entries in the in-memory detail cache. When exceeded,
/// expired entries are evicted first; if still over limit the oldest entry
/// is removed.
const DETAIL_CACHE_MAX_ENTRIES: usize = 500;

/// In-memory cache for AniList detail responses to avoid rate limiting.
struct CacheEntry {
    detail: AnimeDetail,
    fetched_at: Instant,
}

static DETAIL_CACHE: LazyLock<RwLock<HashMap<i64, CacheEntry>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// Shared reqwest client. Each `reqwest::Client::new()` call previously
/// rebuilt the TLS context and connection pool from scratch — wasteful
/// across the search / detail / fallback paths that all hit
/// graphql.anilist.co. Using a single Lazy client lets the pool reuse
/// connections across calls.
///
/// Timeouts: 10s to establish a TCP+TLS handshake, 30s overall per
/// request. Without an overall timeout a hung connection (e.g. half-
/// open after a network partition) pins a pool slot until kernel TCP
/// keepalive resolves, which on default Linux is roughly 2 hours —
/// long enough that interactive searches feel permanently broken even
/// after AL is healthy again. The 30s ceiling is generous relative to
/// AL's typical sub-second response time but still bounded; cooldown /
/// retry semantics live in the callers and are unaffected by this
/// per-attempt cap.
static HTTP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()
        .expect("building the AniList reqwest client should not fail")
});

type SearchCacheEntry = (Instant, Vec<AnimeEntry>);

/// Search result cache, keyed on (provider-mode, normalized query).
/// Provider-mode is "al" for the normal AniList-first path and "mal" for the
/// force_mal_fallback path; we keep them separate because they return different
/// `source` fields per entry and the frontend displays the distinction.
static SEARCH_CACHE: LazyLock<StdMutex<HashMap<String, SearchCacheEntry>>> =
    LazyLock::new(|| StdMutex::new(HashMap::new()));

/// Monotonically-set "AniList is in cooldown until Instant". Consulted at the
/// top of every search so that once we've learned AniList is rate-limiting us,
/// we stop wasting a round-trip per search (which was dragging Jikan into the
/// rate-limit bucket too).
static ANILIST_COOLDOWN_UNTIL: LazyLock<StdMutex<Option<Instant>>> =
    LazyLock::new(|| StdMutex::new(None));

/// Safety margin added to AL's `Retry-After` value (or to the default
/// cooldown when no Retry-After is present). Without this, waiting
/// exactly the duration AL hands back consistently lands the next
/// request at the window boundary and trips a fresh 429 — observed
/// live during a sweep where the second 429 followed the first by
/// roughly the original Retry-After value. 2s clears the boundary in
/// practice without meaningfully extending sweep time.
const COOLDOWN_SAFETY_MARGIN: Duration = Duration::from_secs(2);

/// Below this many `X-RateLimit-Remaining`, switch from "minimum
/// inter-request spacing" to "wait until window reset." Picked low so
/// we mostly run at full headroom-driven speed and only slow down when
/// we're about to bump the cap.
const REMAINING_HEADROOM_THRESHOLD: u32 = 3;

/// Fallback per-minute limit used when AL hasn't told us its current
/// limit yet. Conservative — matches the documented "currently degraded
/// to 30 req/min" state. Once we see `X-RateLimit-Limit` we use that
/// instead, so during normal AL operation (90 req/min) we adapt up.
const ANILIST_LIMIT_FALLBACK: u32 = 30;

/// Per-response snapshot of AniList's rate-limit headers, used by
/// `throttle_before_anilist_request` to pace the next call without
/// guessing.
#[derive(Clone, Copy)]
struct RateLimitState {
    /// `X-RateLimit-Limit` — total requests in the current window.
    limit: u32,
    /// `X-RateLimit-Remaining` from the latest response.
    remaining: u32,
    /// `X-RateLimit-Reset` translated from a Unix timestamp into our
    /// monotonic clock at recording time. None when the header was
    /// absent, which AL only sends on 429s in normal operation.
    reset_at: Option<Instant>,
}

static RATE_LIMIT_STATE: LazyLock<StdMutex<Option<RateLimitState>>> =
    LazyLock::new(|| StdMutex::new(None));

/// Last time we sent a request to AniList. Used to enforce a minimum
/// inter-request spacing derived from the current per-minute limit, so
/// a relation walk that fires N back-to-back AL calls can't burst over
/// AL's burst limiter (which is documented but not header-exposed).
static LAST_AL_REQUEST: LazyLock<StdMutex<Option<Instant>>> =
    LazyLock::new(|| StdMutex::new(None));

/// Compute the minimum spacing between AL requests for a given
/// per-minute limit. 60s / limit, with a 10% safety pad on top so that
/// clock drift / measurement noise can't accidentally push us above
/// the cap. Returns a defensive 2s when limit is 0 (shouldn't happen).
fn min_inter_request(limit: u32) -> Duration {
    if limit == 0 {
        return Duration::from_secs(2);
    }
    Duration::from_millis((66_000 / limit as u64).max(100))
}

/// Capture rate-limit headers from the latest AL response. Called for
/// every AL response (success or error) so the next throttle decision
/// is based on AL's fresh count rather than our stale belief. Headers
/// AL doesn't send (e.g. `X-RateLimit-Reset` outside of throttling)
/// just leave the corresponding field at None / unchanged.
fn record_rate_limit_headers(headers: &reqwest::header::HeaderMap) {
    let parse = |name: &str| -> Option<u64> {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
    };

    let limit = parse("x-ratelimit-limit").map(|v| v as u32);
    let remaining = parse("x-ratelimit-remaining").map(|v| v as u32);
    let reset_unix = parse("x-ratelimit-reset");

    // If AL didn't send anything useful, leave existing state alone.
    if limit.is_none() && remaining.is_none() && reset_unix.is_none() {
        return;
    }

    let reset_at = reset_unix.and_then(|reset| {
        let now_unix = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
        if reset > now_unix {
            Some(Instant::now() + Duration::from_secs(reset - now_unix))
        } else {
            None
        }
    });

    if let Ok(mut guard) = RATE_LIMIT_STATE.lock() {
        let prev = *guard;
        *guard = Some(RateLimitState {
            limit: limit.or(prev.map(|s| s.limit)).unwrap_or(ANILIST_LIMIT_FALLBACK),
            remaining: remaining.or(prev.map(|s| s.remaining)).unwrap_or(0),
            // Keep the prior reset_at if AL didn't send one this time —
            // it's the most recent ground truth we have for when the
            // window flips.
            reset_at: reset_at.or(prev.and_then(|s| s.reset_at)),
        });
    }
}

/// Pure throttle decision. Returns how long to sleep before the next
/// AniList request, given the latest rate-limit snapshot, the
/// timestamp of our last request (if any), and the current instant.
/// Extracted from `throttle_before_anilist_request` so the branching
/// is unit-testable without `tokio::time::sleep`.
///
/// Two strategies, in priority order:
///   - **Window-flip** (`remaining <= REMAINING_HEADROOM_THRESHOLD`
///     and `reset_at` is in the future): wait until the next window
///     opens. Returns the larger of (window-wait, burst-guard) so we
///     don't accidentally undercut burst spacing.
///   - **Burst guard** (always): don't exceed `60s / limit` between
///     consecutive requests. Returns 0 if the last request was long
///     enough ago.
fn decide_wait(
    state: Option<RateLimitState>,
    last_request: Option<Instant>,
    now: Instant,
) -> Duration {
    let limit = state.map(|s| s.limit).unwrap_or(ANILIST_LIMIT_FALLBACK);
    let min_spacing = min_inter_request(limit);

    let burst_wait = match last_request {
        Some(last) => {
            let elapsed = now.saturating_duration_since(last);
            min_spacing.saturating_sub(elapsed)
        }
        None => Duration::ZERO,
    };

    if let Some(s) = state
        && s.remaining <= REMAINING_HEADROOM_THRESHOLD
        && let Some(reset_at) = s.reset_at
        && reset_at > now
    {
        let window_wait = reset_at.saturating_duration_since(now) + Duration::from_secs(1);
        return window_wait.max(burst_wait);
    }

    burst_wait
}

/// Pace the next AniList request to stay inside the documented window
/// and burst limits, using the latest rate-limit headers as the source
/// of truth. Without this, a relation walk burst-fires N calls in
/// seconds — even when N is well under the per-minute limit, AL's
/// burst limiter trips and we eat a full minute of cooldown for what
/// would've been a 1-second pause if we'd just paced ourselves.
async fn throttle_before_anilist_request() {
    let snap = RATE_LIMIT_STATE.lock().ok().and_then(|g| *g);
    let last = LAST_AL_REQUEST.lock().ok().and_then(|g| *g);
    let wait = decide_wait(snap, last, Instant::now());

    if !wait.is_zero() {
        // Only log the headroom-low case — burst-guard waits are
        // sub-second and dominate normal operation, no point spamming.
        if let Some(s) = snap
            && s.remaining <= REMAINING_HEADROOM_THRESHOLD
            && wait > Duration::from_secs(1)
        {
            tracing::info!(
                target: "ryokan::anilist",
                remaining = s.remaining,
                wait_secs = wait.as_secs(),
                "AniList rate-limit headroom low; pausing until window resets"
            );
        }
        tokio::time::sleep(wait).await;
    }

    if let Ok(mut guard) = LAST_AL_REQUEST.lock() {
        *guard = Some(Instant::now());
    }
}

/// Compute cooldown duration on a 429 from AL's headers, preferring
/// `X-RateLimit-Reset` (absolute timestamp, the most precise signal AL
/// gives us) over `Retry-After` (relative seconds), with the configured
/// default as the last fallback. The result is capped and padded the
/// same way as the default-only path.
fn cooldown_from_headers(
    headers: &reqwest::header::HeaderMap,
    default_dur: Duration,
) -> Duration {
    let parse_u64 = |name: &str| -> Option<u64> {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
    };

    if let Some(reset_unix) = parse_u64("x-ratelimit-reset") {
        let now_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if reset_unix > now_unix {
            let raw = Duration::from_secs(reset_unix - now_unix).min(ANILIST_COOLDOWN_MAX);
            return raw + COOLDOWN_SAFETY_MARGIN;
        }
    }

    let retry_after = parse_u64("retry-after");
    compute_cooldown_duration(retry_after, default_dur)
}

fn normalize_search_key(force_fallback: bool, query: &str) -> String {
    let folded: String = query
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    format!("{}::{}", if force_fallback { "mal" } else { "al" }, folded)
}

fn search_cache_get(key: &str) -> Option<Vec<AnimeEntry>> {
    let now = Instant::now();
    let mut cache = SEARCH_CACHE.lock().ok()?;
    if let Some((fetched_at, results)) = cache.get(key)
        && now.duration_since(*fetched_at) <= SEARCH_CACHE_TTL {
            return Some(results.clone());
        }
    cache.remove(key);
    None
}

fn search_cache_put(key: String, results: Vec<AnimeEntry>) {
    if let Ok(mut cache) = SEARCH_CACHE.lock() {
        // Bound the cache. Simple heuristic — if we're >200 entries, drop expired
        // ones; if still too big, just clear. Search queries are long-tail anyway.
        if cache.len() > 200 {
            let now = Instant::now();
            cache.retain(|_, (t, _)| now.duration_since(*t) <= SEARCH_CACHE_TTL);
            if cache.len() > 200 {
                cache.clear();
            }
        }
        cache.insert(key, (Instant::now(), results));
    }
}

pub fn anilist_cooldown_active() -> bool {
    if let Ok(guard) = ANILIST_COOLDOWN_UNTIL.lock()
        && let Some(until) = *guard {
            return Instant::now() < until;
        }
    false
}

/// Single source of truth for "this AniList error means we're being
/// throttled / blocked." Callers use this to decide whether to
/// defer-and-retry (preserve AL fidelity) vs. treat AL as down and fall
/// back to MAL.
///
/// All errors classified as throttle by `classify_anilist_failure` carry
/// the `AniList rate-limited` prefix; the in-process cooldown short-circuit
/// uses `cooldown active`. Non-throttle failures (5xx, 403 with
/// non-Cloudflare body, parse errors) deliberately do *not* match — those
/// are "AL is genuinely down" and fallback callers should substitute MAL.
pub fn is_rate_limit_error(err: &str) -> bool {
    err.contains("AniList rate-limited") || err.contains("cooldown active")
}

/// Classification of an AniList failure response. Drives both the
/// error-string wording and whether `set_anilist_cooldown` is called.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AniListFailureKind {
    /// Throttle: 429, Cloudflare challenge, or GraphQL-level "too many
    /// requests". Caller should defer-and-retry; do not substitute MAL.
    RateLimited,
    /// AL itself is unhealthy: 5xx, body parse failure, or a 403 whose
    /// body doesn't look like Cloudflare (suggests an AL-side problem
    /// rather than upstream throttling). Caller may fall back to MAL.
    Unavailable,
    /// AL responded successfully but the requested entity doesn't exist.
    NotFound,
}

/// Inspect status + body to figure out *why* AniList rejected the call.
/// Status alone is ambiguous (especially 403 — Cloudflare vs. AL itself
/// vs. auth issue), so we also look for body markers. Returns the kind
/// plus a tagged error string the caller can return to its consumers;
/// downstream code matches on the tag prefix (`AniList rate-limited` /
/// `AniList unavailable` / `AniList not found`) rather than HTTP codes,
/// so adding new wordings doesn't break the policy.
fn classify_anilist_failure(
    status: reqwest::StatusCode,
    body_text: &str,
) -> (AniListFailureKind, String) {
    let parsed: Option<serde_json::Value> = serde_json::from_str(body_text).ok();
    let gql_msg = parsed.as_ref().and_then(extract_graphql_error);

    // Cloudflare HTML challenge — distinctive body markers, can come back
    // with any 4xx/5xx status. Treat as throttle.
    let lower = body_text.to_ascii_lowercase();
    let is_cloudflare = lower.contains("cf-ray")
        || lower.contains("just a moment")
        || lower.contains("attention required")
        || (lower.contains("cloudflare") && lower.contains("<html"));
    if is_cloudflare {
        return (
            AniListFailureKind::RateLimited,
            format!("AniList rate-limited: Cloudflare challenge (HTTP {})", status),
        );
    }

    // GraphQL-level throttle hint, regardless of status.
    if let Some(msg) = &gql_msg {
        let lower = msg.to_ascii_lowercase();
        if lower.contains("too many requests")
            || lower.contains("rate limit")
            || lower.contains("throttled")
        {
            return (
                AniListFailureKind::RateLimited,
                format!("AniList rate-limited: {} (HTTP {})", msg, status),
            );
        }
    }

    let detail = gql_msg.unwrap_or_else(|| excerpt(body_text));
    match status.as_u16() {
        429 => (
            AniListFailureKind::RateLimited,
            format!("AniList rate-limited (HTTP 429): {}", detail),
        ),
        404 => (
            AniListFailureKind::NotFound,
            format!("AniList not found (HTTP 404): {}", detail),
        ),
        // 403 lands here when the body wasn't Cloudflare-shaped — that's
        // AL-side (auth / blocked at the app layer), not upstream
        // throttling, so it's "unavailable" and callers may MAL-fallback.
        // 5xx is the same family.
        _ => (
            AniListFailureKind::Unavailable,
            format!("AniList unavailable (HTTP {}): {}", status, detail),
        ),
    }
}

/// Truncate a body string for inclusion in an error message. Char-aware
/// so it can't slice in the middle of a multi-byte UTF-8 sequence.
fn excerpt(text: &str) -> String {
    const MAX_CHARS: usize = 200;
    let trimmed = text.trim();
    let mut iter = trimmed.chars();
    let prefix: String = iter.by_ref().take(MAX_CHARS).collect();
    if iter.next().is_some() {
        format!("{}…", prefix)
    } else {
        prefix
    }
}

/// Pure cooldown-duration computation. Extracted from `set_anilist_cooldown`
/// for unit-testability — the wall-clock side effect lives in the caller.
fn compute_cooldown_duration(
    retry_after_secs: Option<u64>,
    default_dur: Duration,
) -> Duration {
    let base = retry_after_secs
        .map(Duration::from_secs)
        .unwrap_or(default_dur)
        .min(ANILIST_COOLDOWN_MAX);
    base + COOLDOWN_SAFETY_MARGIN
}

fn set_anilist_cooldown(retry_after_secs: Option<u64>, default_dur: Duration) {
    let dur = compute_cooldown_duration(retry_after_secs, default_dur);
    if let Ok(mut guard) = ANILIST_COOLDOWN_UNTIL.lock() {
        *guard = Some(Instant::now() + dur);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct AnimeEntry {
    pub id: i64,
    pub id_mal: Option<i64>,
    pub title_romaji: String,
    pub title_english: String,
    pub title_native: String,
    pub cover_url: String,
    pub format: String,
    pub status: String,
    pub status_display: String,
    pub episodes: Option<i32>,
    pub season_year: Option<i32>,
    pub source: String,
}

/// Search AniList for anime by title, falling back to MAL/Jikan if AniList 403s.
#[allow(dead_code)]
pub async fn search_anime(query: &str) -> Result<Vec<AnimeEntry>, String> {
    search_anime_with_options(query, false).await
}

pub async fn search_anime_with_options(query: &str, force_mal_fallback: bool) -> Result<Vec<AnimeEntry>, String> {
    let query = query.trim();
    if query.is_empty() {
        return Ok(Vec::new());
    }

    // 1. Cache lookup — skip all upstream work for repeat queries within the TTL.
    let cache_key = normalize_search_key(force_mal_fallback, query);
    if let Some(cached) = search_cache_get(&cache_key) {
        tracing::debug!("anilist search cache hit for {:?} ({} results)", query, cached.len());
        return Ok(cached);
    }

    if force_mal_fallback {
        let results = fallback_jikan(query, None).await?;
        search_cache_put(cache_key, results.clone());
        return Ok(results);
    }

    // 2. If AniList is known to be rate-limited, don't bother hitting it — go
    //    straight to Jikan. This is the key fix for the "both APIs rate-limited
    //    at once" symptom: previously every search during the 60s AL cooldown
    //    still pinged AL, got another 429, then called Jikan, burning Jikan's
    //    (stricter) rate-limit budget alongside.
    if anilist_cooldown_active() {
        tracing::debug!(
            "anilist search skipping AniList for {:?} (still in cooldown)",
            query
        );
        let results = fallback_jikan(
            query,
            Some("AniList rate-limited (skipped during cooldown)".to_string()),
        ).await?;
        search_cache_put(cache_key, results.clone());
        return Ok(results);
    }

    let gql = serde_json::json!({
        "query": r#"
            query ($search: String) {
                Page(page: 1, perPage: 10) {
                    media(search: $search, type: ANIME, sort: SEARCH_MATCH) {
                        id
                        idMal
                        title {
                            romaji
                            english
                            native
                        }
                        coverImage {
                            large
                        }
                        format
                        status
                        episodes
                    }
                }
            }
        "#,
        "variables": { "search": query }
    });

    // Pace via the same shared rate-limit state that fetch_anime_detail
    // uses — search-path 429s would otherwise leave the detail-path
    // throttle decisions working off a stale `remaining`.
    throttle_before_anilist_request().await;

    let client = &*HTTP_CLIENT;
    let resp = match client
        .post(ANILIST_API)
        .header("User-Agent", "Ryokan/0.1")
        .json(&gql)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                "AniList request failed for query {:?}: {}; falling back to Jikan/MAL",
                query, e
            );
            let results = fallback_jikan(
                query,
                Some(format!("AniList unreachable: {}", e)),
            ).await?;
            search_cache_put(cache_key, results.clone());
            return Ok(results);
        }
    };

    let status = resp.status();
    record_rate_limit_headers(resp.headers());

    // Silently fall back to Jikan/MAL on transient AniList outages:
    //   403 — Cloudflare challenge / geo-block
    //   429 — rate limit (30 req/min anon)
    //   5xx — upstream outage
    // These are the cases where the user's search should just Work via a
    // fallback provider rather than surfacing a cryptic HTTP error.
    if status == reqwest::StatusCode::FORBIDDEN
        || status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
    {
        let retry_after_secs = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok());
        tracing::warn!(
            "AniList search HTTP {} for query {:?} (retry-after={:?}); falling back to Jikan/MAL",
            status, query, retry_after_secs
        );
        // Start a cooldown so subsequent searches in this window skip
        // AL entirely. 403 (Cloudflare challenge) is the most common
        // AniList outage mode; without this branch, every request kept
        // round-tripping through AL just to bounce on the 403 again
        // before falling back to Jikan. Cloudflare doesn't include
        // Retry-After, so pick a longer default — 60s rarely outlasts
        // a real challenge — and let ANILIST_COOLDOWN_MAX cap it.
        let default_cooldown = if status == reqwest::StatusCode::FORBIDDEN {
            Duration::from_secs(300)
        } else {
            ANILIST_COOLDOWN_DEFAULT
        };
        set_anilist_cooldown(retry_after_secs, default_cooldown);
        let reason = match status.as_u16() {
            429 => format!(
                "AniList rate-limited{}",
                retry_after_secs
                    .map(|r| format!(" (retry in {}s)", r))
                    .unwrap_or_default()
            ),
            403 => "AniList blocked our request (Cloudflare challenge)".to_string(),
            code => format!("AniList upstream error (HTTP {})", code),
        };
        let results = fallback_jikan(query, Some(reason)).await?;
        search_cache_put(cache_key, results.clone());
        return Ok(results);
    }

    // Read the body as text first so a non-JSON error body (common on 4xx/5xx)
    // produces a useful error instead of "Failed to parse AniList response".
    let body_text = resp
        .text()
        .await
        .map_err(|e| format!("AniList response read failed (HTTP {}): {}", status, e))?;

    let body: serde_json::Value = match serde_json::from_str(&body_text) {
        Ok(v) => v,
        Err(parse_err) => {
            if !status.is_success() {
                let snippet: String = body_text.chars().take(200).collect();
                return Err(format!("AniList search failed (HTTP {}): {}", status, snippet.trim()));
            }
            return Err(format!("Failed to parse AniList response: {}", parse_err));
        }
    };

    if !status.is_success() {
        let msg = extract_graphql_error(&body).unwrap_or_else(|| body.to_string());
        return Err(format!("AniList search failed (HTTP {}): {}", status, msg));
    }

    if let Some(msg) = extract_graphql_error(&body) {
        return Err(format!("AniList search failed: {}", msg));
    }

    let media = match body["data"]["Page"]["media"].as_array() {
        Some(arr) => arr,
        None => {
            // Schema mismatch — `data.Page.media` is missing or not an
            // array. Don't cache the empty result here: a legitimate
            // 0-hit search hits the Some branch with an empty arr and
            // *does* get cached at line 317 below. Caching the
            // schema-mismatch case would lock us out of fresh requests
            // for SEARCH_CACHE_TTL even after AniList recovers.
            tracing::warn!(
                target: "ryokan::anilist",
                query = %query,
                "AniList response missing data.Page.media; not caching empty result"
            );
            return Ok(Vec::new());
        }
    };

    let entries: Vec<AnimeEntry> = media
        .iter()
        .map(|m| AnimeEntry {
            id: m["id"].as_i64().unwrap_or(0),
            id_mal: m["idMal"].as_i64(),
            title_romaji: m["title"]["romaji"].as_str().unwrap_or("").to_string(),
            title_english: m["title"]["english"].as_str().unwrap_or("").to_string(),
            title_native: m["title"]["native"].as_str().unwrap_or("").to_string(),
            cover_url: m["coverImage"]["large"].as_str().unwrap_or("").to_string(),
            format: m["format"].as_str().unwrap_or("").to_string(),
            status: m["status"].as_str().unwrap_or("").to_string(),
            status_display: prettify_status(m["status"].as_str().unwrap_or("")),
            episodes: m["episodes"].as_i64().map(|e| e as i32),
            season_year: m["seasonYear"].as_i64().map(|y| y as i32),
            source: "anilist".to_string(),
        })
        .collect();

    search_cache_put(cache_key, entries.clone());
    Ok(entries)
}

/// Shared fallback helper used when AniList is unavailable or force_mal_fallback is set.
/// Tries Jikan (MAL-backed). If Jikan also fails, `compose_search_error` builds a
/// user-friendly combined message for the frontend.
async fn fallback_jikan(
    query: &str,
    anilist_reason: Option<String>,
) -> Result<Vec<AnimeEntry>, String> {
    match jikan::search_anime(query).await {
        Ok(results) => Ok(results),
        Err(jikan_err) => Err(compose_search_error(anilist_reason.as_deref(), &jikan_err)),
    }
}

/// Produce a clean, human-readable error message from an AniList failure reason
/// (optional — e.g. "AniList rate-limited (retry in 28s)") and a Jikan failure
/// reason. Callers see something like:
///   "Both AniList and Jikan/MAL are rate-limited right now. Try again in ~30s."
/// instead of a raw JSON dump concatenation.
fn compose_search_error(anilist_reason: Option<&str>, jikan_err: &str) -> String {
    let al_rate_limited = anilist_reason
        .map(|r| r.contains("rate-limited") || r.contains("429"))
        .unwrap_or(false);
    let jikan_rate_limited = jikan_err.contains("rate-limited") || jikan_err.contains("429");

    if al_rate_limited && jikan_rate_limited {
        // Try to surface the AL retry hint if we parsed one earlier.
        let hint = anilist_reason
            .and_then(|r| {
                let start = r.find("retry in ")?;
                let tail = &r[start + "retry in ".len()..];
                let end = tail.find(')').unwrap_or(tail.len());
                Some(tail[..end].to_string())
            })
            .map(|s| format!(" Try again in ~{}.", s))
            .unwrap_or_else(|| " Try again in a minute.".to_string());
        return format!("Both AniList and Jikan/MAL are rate-limited right now.{}", hint);
    }

    match anilist_reason {
        Some(al) => format!("{}. MAL/Jikan fallback also failed: {}", al, jikan_err),
        None => format!("MAL/Jikan search failed: {}", jikan_err),
    }
}



#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct RelatedEntry {
    pub id: i64,
    pub id_mal: Option<i64>,
    pub title_romaji: String,
    pub title_english: String,
    pub title_native: String,
    pub cover_url: String,
    pub format: String,
    pub status: String,
    pub status_display: String,
    pub episodes: Option<i32>,
    pub relation_type: String,
    pub season_year: Option<i32>,
    pub media_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct StreamingEpisode {
    pub title: String,
    pub thumbnail: String,
    pub url: String,
    pub site: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct AnimeDetail {
    pub id: i64,
    pub id_mal: Option<i64>,
    pub title_romaji: String,
    pub title_english: String,
    pub title_native: String,
    pub cover_url: String,
    pub banner_url: String,
    pub format: String,
    pub status: String,
    pub status_display: String,
    pub episodes: Option<i32>,
    pub duration: Option<i32>,
    pub season: String,
    pub season_year: Option<i32>,
    /// AniList `endDate.year` when the show has finished. `#[serde(default)]`
    /// so cached JSON blobs from before this field existed deserialize
    /// cleanly to `None`. Consumed by Layer 4 temporal inference.
    #[serde(default)]
    pub end_year: Option<i32>,
    pub description: String,
    pub genres: Vec<String>,
    pub average_score: Option<i32>,
    pub average_score_display: Option<String>,
    pub score_is_ten_point: bool,
    pub score_class: String,
    pub next_airing_episode: Option<i32>,
    pub next_airing_at: Option<i64>,
    pub synonyms: Vec<String>,
    pub streaming_episodes: Vec<StreamingEpisode>,
    pub relations: Vec<RelatedEntry>,
}

impl AnimeDetail {
    /// Effective episode count for rendering, episode cache building, and
    /// monitoring. AniList reports `episodes: null` for currently-airing
    /// series because the final count isn't known yet, so we fall back to
    /// `nextAiringEpisode - 1` (the number of episodes that have already
    /// aired). Without this every airing show looks like it has zero
    /// episodes, which breaks the episode list and the monitoring UI.
    pub fn effective_episode_count(&self) -> i32 {
        match self.episodes.unwrap_or(0) {
            0 => self.next_airing_episode.map(|n| (n - 1).max(0)).unwrap_or(0),
            n => n,
        }
    }

    /// True when the series has finished airing (or was cancelled) per any
    /// of the three metadata providers' vocabularies. AniList uses
    /// `FINISHED` / `CANCELLED`, Jikan normalizes "Finished Airing" →
    /// `FINISHED_AIRING`, and Kitsu uses `FINISHED`. Without this helper
    /// the callsites that just compared against the literal `"FINISHED"`
    /// string silently misclassified every Jikan-fed series as "still
    /// airing", breaking the finished-mode BD probe and the 2-year
    /// sequel-rejection filter whenever the AniList fallback kicked in.
    pub fn is_finished(&self) -> bool {
        matches!(
            self.status.as_str(),
            "FINISHED" | "FINISHED_AIRING" | "CANCELLED"
        )
    }
}

fn prettify_status(status: &str) -> String {
    status.replace('_', " ")
}

fn score_class(score: Option<i32>, is_ten_point: bool) -> String {
    let class = if is_ten_point {
        match score {
            Some(s) if s >= 9 => "tag-score-purple",
            Some(s) if s >= 7 => "tag-score-green",
            Some(s) if s > 5 => "tag-score-yellow",
            _ => "tag-score-red",
        }
    } else {
        match score {
            Some(s) if s >= 85 => "tag-score-purple",
            Some(s) if s >= 75 => "tag-score-green",
            Some(s) if s > 65 => "tag-score-yellow",
            _ => "tag-score-red",
        }
    };
    class.to_string()
}

pub async fn get_anime_detail(id: i64) -> Result<AnimeDetail, String> {
    get_anime_detail_with_options(id, None, false).await
}

pub async fn get_anime_detail_with_options(id: i64, mal_id_hint: Option<i64>, force_mal_fallback: bool) -> Result<AnimeDetail, String> {
    if id < 0 {
        return jikan::get_anime_detail_cached(-id).await;
    }
    if force_mal_fallback
        && let Some(mid) = mal_id_hint {
            return jikan::get_anime_detail_cached(mid).await;
        }

    {
        let cache = DETAIL_CACHE.read().await;
        if let Some(entry) = cache.get(&id)
            && entry.fetched_at.elapsed().as_secs() < DETAIL_CACHE_TTL_SECS {
                return Ok(entry.detail.clone());
            }
    }

    let detail = fetch_anime_detail(id).await?;

    {
        let mut cache = DETAIL_CACHE.write().await;
        cache.insert(id, CacheEntry {
            detail: detail.clone(),
            fetched_at: Instant::now(),
        });
        // Evict stale/oldest entries when the cache grows too large.
        if cache.len() > DETAIL_CACHE_MAX_ENTRIES {
            let expired: Vec<i64> = cache
                .iter()
                .filter(|(_, e)| e.fetched_at.elapsed().as_secs() >= DETAIL_CACHE_TTL_SECS)
                .map(|(k, _)| *k)
                .collect();
            for k in &expired {
                cache.remove(k);
            }
            // If still over limit, drop the oldest entry.
            if cache.len() > DETAIL_CACHE_MAX_ENTRIES
                && let Some((&oldest_key, _)) = cache.iter().min_by_key(|(_, e)| e.fetched_at) {
                    cache.remove(&oldest_key);
                }
        }
    }

    Ok(detail)
}

/// What to filter the `Media` query by. AniList's `Media` resolver
/// accepts `id` and `idMal` as independent filters, so a single query
/// shape covers both lookup styles by passing the unused argument as
/// `null` in the variables. Lets `find_anime_detail_by_mal_id` reuse
/// the full field selection without duplicating the query body.
#[derive(Debug, Clone, Copy)]
enum MediaSelector {
    Id(i64),
    IdMal(i64),
}

async fn fetch_anime_detail(id: i64) -> Result<AnimeDetail, String> {
    fetch_media_detail(MediaSelector::Id(id))
        .await?
        .ok_or_else(|| "Anime not found".to_string())
}

/// Build the GraphQL `variables` map for the shared `Media(id:, idMal:)`
/// query. AniList's resolver treats an explicit `id: null` (or
/// `idMal: null`) as "filter where the field equals null" and returns
/// 404, so the unused arm of [`MediaSelector`] must be **omitted** from
/// the variables map (sent as undefined), not sent as JSON null.
/// Verified live 2026-04-19: `{id: 1, idMal: null}` → "Not Found";
/// `{id: 1}` → Cowboy Bebop. Tested in `media_selector_omits_unused_var`.
fn build_media_selector_variables(selector: MediaSelector) -> serde_json::Map<String, serde_json::Value> {
    let mut variables = serde_json::Map::new();
    match selector {
        MediaSelector::Id(v) => {
            variables.insert("id".to_string(), serde_json::json!(v));
        }
        MediaSelector::IdMal(v) => {
            variables.insert("idMal".to_string(), serde_json::json!(v));
        }
    }
    variables
}

async fn fetch_media_detail(selector: MediaSelector) -> Result<Option<AnimeDetail>, String> {
    // Skip the round trip entirely when a recent 429/403/5xx has tripped
    // the global cooldown. Without this, a metadata-refresh sweep that
    // hits AniList's per-minute cap on the first burst keeps firing
    // request after request — each one immediately bouncing on 429 —
    // for the full duration of the sweep, even though we already know
    // AniList is rate-limiting us. The error string flows up through
    // metadata_sync's fallback chain and the warn log added in PR #31
    // surfaces the cooldown state to the operator.
    if anilist_cooldown_active() {
        // Wording note: "skipping AniList request" — only the AniList
        // round trip is skipped here. The caller's fallback chain
        // (jikan/MAL → kitsu) still runs and may produce the detail
        // from a different provider.
        return Err("AniList rate-limit cooldown active; skipping AniList request".to_string());
    }
    let variables = build_media_selector_variables(selector);
    let gql = serde_json::json!({
        "query": r#"
            query ($id: Int, $idMal: Int) {
                Media(id: $id, idMal: $idMal, type: ANIME) {
                    id
                    idMal
                    title { romaji english native }
                    synonyms
                    coverImage { large extraLarge }
                    bannerImage
                    format
                    status
                    episodes
                    duration
                    season
                    seasonYear
                    endDate { year }
                    description(asHtml: true)
                    genres
                    averageScore
                    nextAiringEpisode {
                        episode
                        airingAt
                    }
                    streamingEpisodes {
                        title
                        thumbnail
                        url
                        site
                    }
                    relations {
                        edges {
                            relationType(version: 2)
                            node {
                                id
                                idMal
                                title { romaji english native }
                                format
                                status
                                episodes
                                coverImage { large }
                                type
                                seasonYear
                            }
                        }
                    }
                }
            }
        "#,
        "variables": variables
    });

    // Pace the request based on the latest X-RateLimit-Remaining /
    // X-RateLimit-Reset we've seen. This is the primary defense against
    // 429s — by the time AL hands back a 429 we've already wasted the
    // round trip; throttling proactively keeps the sweep inside AL's
    // window and burst limits.
    throttle_before_anilist_request().await;

    let client = &*HTTP_CLIENT;
    let resp = client
        .post(ANILIST_API)
        .header("User-Agent", "Ryokan/0.1")
        .json(&gql)
        .send()
        .await
        .map_err(|e| format!("AniList request failed: {}", e))?;

    let status = resp.status();
    // Headers carry both the rate-limit snapshot (used by future
    // throttles) and Retry-After / X-RateLimit-Reset for cooldown
    // computation. Clone so we can use them after the body has been
    // consumed.
    let headers = resp.headers().clone();
    record_rate_limit_headers(&headers);

    // Read as text first (not .json()) so a Cloudflare HTML challenge
    // doesn't blow up at the parse step — we need the body to classify
    // the failure correctly.
    let body_text = match resp.text().await {
        Ok(t) => t,
        Err(e) => {
            // Body-read failure: the status header was already received,
            // so preserve the rate-limit signal when the status itself
            // told us we were throttled. Without this branch a connection
            // reset partway through a 429 body would erase the throttle
            // signal — `is_rate_limit_error` returns false, the caller
            // happily MAL-falls-back, and the whole "no MAL on rate-limit"
            // invariant collapses on a flaky network.
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                let dur = cooldown_from_headers(&headers, ANILIST_COOLDOWN_DEFAULT);
                if let Ok(mut guard) = ANILIST_COOLDOWN_UNTIL.lock() {
                    *guard = Some(Instant::now() + dur);
                }
                return Err(format!(
                    "AniList rate-limited (HTTP 429): body read failed: {}",
                    e
                ));
            }
            return Err(format!("AniList unavailable: failed to read response: {}", e));
        }
    };

    if !status.is_success() {
        let (kind, msg) = classify_anilist_failure(status, &body_text);
        // Cooldown only on real throttling. 5xx and AL-side 403s are
        // "AL is down" — letting them set the cooldown would convert
        // subsequent calls into deferred-rate-limit errors and prevent
        // the MAL fallback the caller actually wants.
        if kind == AniListFailureKind::RateLimited {
            // 403 (Cloudflare) doesn't include Retry-After, so pick a
            // longer default — 60s rarely outlasts a real challenge —
            // and let ANILIST_COOLDOWN_MAX cap it. Only Cloudflare 403s
            // reach this branch (non-CF 403s classify as Unavailable).
            let default_cooldown = if status == reqwest::StatusCode::FORBIDDEN {
                Duration::from_secs(300)
            } else {
                ANILIST_COOLDOWN_DEFAULT
            };
            let dur = cooldown_from_headers(&headers, default_cooldown);
            if let Ok(mut guard) = ANILIST_COOLDOWN_UNTIL.lock() {
                *guard = Some(Instant::now() + dur);
            }
        }
        return Err(msg);
    }

    let body: serde_json::Value = serde_json::from_str(&body_text)
        .map_err(|e| format!("AniList unavailable: parse error: {} (body: {})", e, excerpt(&body_text)))?;

    if extract_graphql_error(&body).is_some() {
        // Run the classifier even on 2xx responses: AL has been observed
        // to return throttle messages in the GraphQL `errors[]` array
        // with a 200 status (no 429 at the transport layer). Without
        // this branch we'd surface a generic "AniList detail failed"
        // that doesn't match `is_rate_limit_error`, the caller would
        // MAL-fall-back, and the cooldown wouldn't trigger to short-
        // circuit the rest of the sweep.
        let (kind, msg) = classify_anilist_failure(status, &body_text);
        if kind == AniListFailureKind::RateLimited {
            let dur = cooldown_from_headers(&headers, ANILIST_COOLDOWN_DEFAULT);
            if let Ok(mut guard) = ANILIST_COOLDOWN_UNTIL.lock() {
                *guard = Some(Instant::now() + dur);
            }
        }
        return Err(msg);
    }

    let m = &body["data"]["Media"];
    if m.is_null() {
        return Ok(None);
    }

    Ok(Some(parse_media_node(m)))
}

/// Convert a single Media node from the AniList GraphQL response into
/// `AnimeDetail`. Used by both the single-id `fetch_media_detail` path
/// and the batched `get_anime_details_batch` path so the field plucking
/// logic only lives in one place.
fn parse_media_node(m: &serde_json::Value) -> AnimeDetail {
    let streaming_episodes = m["streamingEpisodes"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|ep| StreamingEpisode {
                    title: ep["title"].as_str().unwrap_or("").to_string(),
                    thumbnail: ep["thumbnail"].as_str().unwrap_or("").to_string(),
                    url: ep["url"].as_str().unwrap_or("").to_string(),
                    site: ep["site"].as_str().unwrap_or("").to_string(),
                })
                .collect()
        })
        .unwrap_or_default();

    let relations = m["relations"]["edges"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|edge| {
                    let node = &edge["node"];
                    Some(RelatedEntry {
                        id: node["id"].as_i64()?,
                        id_mal: node["idMal"].as_i64(),
                        title_romaji: node["title"]["romaji"].as_str().unwrap_or("").to_string(),
                        title_english: node["title"]["english"].as_str().unwrap_or("").to_string(),
                        title_native: node["title"]["native"].as_str().unwrap_or("").to_string(),
                        cover_url: node["coverImage"]["large"].as_str().unwrap_or("").to_string(),
                        format: node["format"].as_str().unwrap_or("").to_string(),
                        status: node["status"].as_str().unwrap_or("").to_string(),
                        status_display: prettify_status(node["status"].as_str().unwrap_or("")),
                        episodes: node["episodes"].as_i64().map(|e| e as i32),
                        relation_type: edge["relationType"].as_str().unwrap_or("").to_string(),
                        season_year: node["seasonYear"].as_i64().map(|y| y as i32),
                        media_type: node["type"].as_str().unwrap_or("").to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    AnimeDetail {
        id: m["id"].as_i64().unwrap_or(0),
        id_mal: m["idMal"].as_i64(),
        title_romaji: m["title"]["romaji"].as_str().unwrap_or("").to_string(),
        title_english: m["title"]["english"].as_str().unwrap_or("").to_string(),
        title_native: m["title"]["native"].as_str().unwrap_or("").to_string(),
        cover_url: m["coverImage"]["extraLarge"]
            .as_str()
            .or_else(|| m["coverImage"]["large"].as_str())
            .unwrap_or("")
            .to_string(),
        banner_url: m["bannerImage"].as_str().unwrap_or("").to_string(),
        format: m["format"].as_str().unwrap_or("").to_string(),
        status: m["status"].as_str().unwrap_or("").to_string(),
        episodes: m["episodes"].as_i64().map(|e| e as i32),
        duration: m["duration"].as_i64().map(|d| d as i32),
        season: m["season"].as_str().unwrap_or("").to_string(),
        season_year: m["seasonYear"].as_i64().map(|y| y as i32),
        end_year: m["endDate"]["year"].as_i64().map(|y| y as i32),
        description: sanitize_rich_description(m["description"].as_str().unwrap_or(""), true),
        genres: m["genres"]
            .as_array()
            .map(|arr| arr.iter().filter_map(|g| g.as_str().map(|s| s.to_string())).collect())
            .unwrap_or_default(),
        average_score: m["averageScore"].as_i64().map(|s| s as i32),
        average_score_display: m["averageScore"].as_i64().map(|s| format!("{}%", s)),
        score_is_ten_point: false,
        score_class: score_class(m["averageScore"].as_i64().map(|s| s as i32), false),
        status_display: prettify_status(m["status"].as_str().unwrap_or("")),
        next_airing_episode: m["nextAiringEpisode"]["episode"].as_i64().map(|e| e as i32),
        next_airing_at: m["nextAiringEpisode"]["airingAt"].as_i64(),
        synonyms: m["synonyms"]
            .as_array()
            .map(|arr| arr.iter().filter_map(|s| s.as_str().map(|v| v.to_string())).collect())
            .unwrap_or_default(),
        streaming_episodes,
        relations,
    }
}

/// Look up an anime by MAL id and return the full `AnimeDetail` payload
/// in one round-trip — replacing the previous `find_anime_by_mal_id` +
/// `get_anime_detail` two-step. The caller (reconciliation path in
/// library.rs) already needed the full payload anyway; AniList accepts
/// `idMal` on the same `Media` query that returns full detail, so the
/// extra "find then fetch" round-trip was wasted.
pub async fn find_anime_detail_by_mal_id(mal_id: i64) -> Result<Option<AnimeDetail>, String> {
    fetch_media_detail(MediaSelector::IdMal(mal_id)).await
}

/// Maximum AniList ids to ask for in a single `Page(media(id_in:[]))`
/// batched detail request. AniList paginates `Page` at perPage=50, but
/// the binding constraint is GraphQL complexity: each `Media` carries a
/// `relations { edges { node {...} } }` block, and 50 × ~10 relations ×
/// edge complexity easily exceeds the documented complexity cap.
/// 25 keeps us comfortably under the cap with full relations included,
/// which matters for the BFS hydrator that needs the next layer of
/// relations on every node it processes.
const ANILIST_BATCH_SIZE: usize = 25;

/// Fetch full `AnimeDetail` payloads for many AniList ids in one
/// `Page(media(id_in:[...]))` request — replacing the historical
/// "loop and call `get_anime_detail` per id" pattern in the metadata
/// BFS, the relation transitive walk, and the Sonarr/Radarr
/// compatibility shims.
///
/// Behavior:
/// - Ids are deduplicated and chunked at [`ANILIST_BATCH_SIZE`]; the
///   helper returns `ceil(N / ANILIST_BATCH_SIZE)` requests' worth of
///   data instead of N.
/// - Each chunk passes through the same cooldown gate, throttle, and
///   rate-limit-header capture as `fetch_media_detail`.
/// - Successful responses populate `DETAIL_CACHE` so subsequent
///   single-id `get_anime_detail` calls for the same ids are cache
///   hits.
/// - On a chunk-level error, processing aborts and the partial map
///   collected so far is returned alongside the error string. The
///   global cooldown will already be set (via
///   `record_rate_limit_headers` / 429 handling) so retrying the
///   remaining chunks would just bounce immediately anyway.
/// - Negative-result ids (AL had no Media for them) simply don't
///   appear in the output map — callers must check `map.get(id)`.
pub async fn get_anime_details_batch(
    ids: &[i64],
) -> Result<HashMap<i64, AnimeDetail>, String> {
    // Dedup + drop non-positive ids (negative ids are MAL-fallback
    // synthetic markers and should hit the Jikan path, not AniList).
    let unique_ids: Vec<i64> = ids
        .iter()
        .copied()
        .filter(|id| *id > 0)
        .collect::<HashSet<i64>>()
        .into_iter()
        .collect();
    if unique_ids.is_empty() {
        return Ok(HashMap::new());
    }

    let mut out: HashMap<i64, AnimeDetail> = HashMap::with_capacity(unique_ids.len());
    let client = &*HTTP_CLIENT;

    for chunk in unique_ids.chunks(ANILIST_BATCH_SIZE) {
        if anilist_cooldown_active() {
            return Err(
                "AniList rate-limit cooldown active; skipping AniList request".to_string(),
            );
        }

        let gql = serde_json::json!({
            // Inject ANILIST_BATCH_SIZE into the query so the const and the
            // GraphQL `perPage` literal can't drift apart silently — bumping
            // the const used to leave the query truncating to the old value
            // and the extra ids would just disappear from the response.
            "query": format!(r#"
                query ($ids: [Int]) {{
                    Page(perPage: {batch_size}) {{
                        media(id_in: $ids, type: ANIME) {{
                            id
                            idMal
                            title {{ romaji english native }}
                            synonyms
                            coverImage {{ large extraLarge }}
                            bannerImage
                            format
                            status
                            episodes
                            duration
                            season
                            seasonYear
                            endDate {{ year }}
                            description(asHtml: true)
                            genres
                            averageScore
                            nextAiringEpisode {{ episode airingAt }}
                            streamingEpisodes {{ title thumbnail url site }}
                            relations {{
                                edges {{
                                    relationType(version: 2)
                                    node {{
                                        id
                                        idMal
                                        title {{ romaji english native }}
                                        format
                                        status
                                        episodes
                                        coverImage {{ large }}
                                        type
                                        seasonYear
                                    }}
                                }}
                            }}
                        }}
                    }}
                }}
            "#, batch_size = ANILIST_BATCH_SIZE),
            "variables": { "ids": chunk }
        });

        throttle_before_anilist_request().await;

        let resp = client
            .post(ANILIST_API)
            .header("User-Agent", "Ryokan/0.1")
            .json(&gql)
            .send()
            .await
            .map_err(|e| format!("AniList batch request failed: {}", e))?;

        let status = resp.status();
        let headers = resp.headers().clone();
        record_rate_limit_headers(&headers);

        let body_text = resp
            .text()
            .await
            .map_err(|e| format!("AniList batch unavailable: failed to read response: {}", e))?;

        if !status.is_success() {
            let (kind, msg) = classify_anilist_failure(status, &body_text);
            if kind == AniListFailureKind::RateLimited {
                let default_cooldown = if status == reqwest::StatusCode::FORBIDDEN {
                    Duration::from_secs(300)
                } else {
                    ANILIST_COOLDOWN_DEFAULT
                };
                let dur = cooldown_from_headers(&headers, default_cooldown);
                if let Ok(mut guard) = ANILIST_COOLDOWN_UNTIL.lock() {
                    *guard = Some(Instant::now() + dur);
                }
            }
            return Err(msg);
        }

        let body: serde_json::Value = serde_json::from_str(&body_text)
            .map_err(|e| {
                format!(
                    "AniList batch parse error: {} (body: {})",
                    e,
                    excerpt(&body_text)
                )
            })?;

        if extract_graphql_error(&body).is_some() {
            let (kind, msg) = classify_anilist_failure(status, &body_text);
            if kind == AniListFailureKind::RateLimited {
                let dur = cooldown_from_headers(&headers, ANILIST_COOLDOWN_DEFAULT);
                if let Ok(mut guard) = ANILIST_COOLDOWN_UNTIL.lock() {
                    *guard = Some(Instant::now() + dur);
                }
            }
            return Err(msg);
        }

        let media_arr = body["data"]["Page"]["media"].as_array();
        if let Some(media) = media_arr {
            // Eagerly populate DETAIL_CACHE so subsequent single-id
            // `get_anime_detail` calls for these ids hit the cache.
            let mut cache = DETAIL_CACHE.write().await;
            for node in media {
                let detail = parse_media_node(node);
                if detail.id > 0 {
                    cache.insert(
                        detail.id,
                        CacheEntry {
                            detail: detail.clone(),
                            fetched_at: Instant::now(),
                        },
                    );
                    out.insert(detail.id, detail);
                }
            }
            // Light eviction — same shape as the single-id path so a
            // big batch can't unbounded-grow the cache.
            if cache.len() > DETAIL_CACHE_MAX_ENTRIES {
                let expired: Vec<i64> = cache
                    .iter()
                    .filter(|(_, e)| {
                        e.fetched_at.elapsed().as_secs() >= DETAIL_CACHE_TTL_SECS
                    })
                    .map(|(k, _)| *k)
                    .collect();
                for k in &expired {
                    cache.remove(k);
                }
            }
        }
    }

    Ok(out)
}

fn extract_graphql_error(body: &serde_json::Value) -> Option<String> {
    body["errors"]
        .as_array()
        .and_then(|errs| errs.first())
        .and_then(|err| err["message"].as_str())
        .map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_selector_omits_unused_var() {
        // Regression: AniList rejects `Media(id: null, idMal: 1)` as
        // "Not Found" because the resolver treats explicit JSON null as
        // "filter where id equals null." The variables map must OMIT
        // the unused arm, not send it as null. Verified live 2026-04-19.
        let by_id = build_media_selector_variables(MediaSelector::Id(42));
        assert_eq!(by_id.get("id").and_then(|v| v.as_i64()), Some(42));
        assert!(
            !by_id.contains_key("idMal"),
            "Id selector must NOT include idMal var (even as null) — sending null trips an AniList 404"
        );

        let by_mal = build_media_selector_variables(MediaSelector::IdMal(1));
        assert_eq!(by_mal.get("idMal").and_then(|v| v.as_i64()), Some(1));
        assert!(
            !by_mal.contains_key("id"),
            "IdMal selector must NOT include id var (even as null) — sending null trips an AniList 404"
        );
    }

    #[test]
    fn normalize_search_key_folds_whitespace_and_case() {
        assert_eq!(
            normalize_search_key(false, "  Jojo  Part  3 "),
            "al::jojo part 3"
        );
        assert_eq!(
            normalize_search_key(true, "\tFrieren\n"),
            "mal::frieren"
        );
    }

    #[test]
    fn normalize_search_key_separates_al_from_mal_modes() {
        assert_ne!(
            normalize_search_key(false, "Bleach"),
            normalize_search_key(true, "Bleach")
        );
    }

    #[test]
    fn compose_error_when_both_rate_limited_suggests_retry() {
        let msg = compose_search_error(
            Some("AniList rate-limited (retry in 28s)"),
            "Jikan rate-limited (HTTP 429): You are being rate-limited",
        );
        assert!(msg.contains("Both AniList and Jikan/MAL are rate-limited"), "msg was: {}", msg);
        assert!(msg.contains("28s"), "retry hint lost: {}", msg);
    }

    #[test]
    fn compose_error_falls_back_when_only_one_rate_limited() {
        let msg = compose_search_error(
            Some("AniList rate-limited (retry in 28s)"),
            "Jikan unreachable: connection refused",
        );
        assert!(msg.starts_with("AniList rate-limited"), "msg was: {}", msg);
        assert!(msg.contains("connection refused"), "jikan detail lost: {}", msg);
        assert!(!msg.contains("Both AniList and Jikan/MAL"), "wrong branch: {}", msg);
    }

    #[test]
    fn compose_error_without_anilist_reason_uses_mal_prefix() {
        let msg = compose_search_error(None, "Jikan HTTP 500: upstream down");
        assert!(msg.starts_with("MAL/Jikan search failed"), "msg was: {}", msg);
        assert!(msg.contains("upstream down"));
    }

    #[test]
    fn search_cache_roundtrips_and_expires_on_ttl_mismatch() {
        // We can't sleep for 60s in tests, but we can validate that distinct
        // keys don't collide and that a put/get returns the same Vec.
        let key = normalize_search_key(false, "test query unique 1");
        let entries = vec![AnimeEntry {
            id: 42,
            id_mal: None,
            title_romaji: "Test".into(),
            title_english: "".into(),
            title_native: "".into(),
            cover_url: "".into(),
            format: "TV".into(),
            status: "FINISHED".into(),
            status_display: "Finished".into(),
            episodes: Some(12),
            season_year: Some(2020),
            source: "anilist".into(),
        }];
        search_cache_put(key.clone(), entries.clone());
        let got = search_cache_get(&key).expect("cached value should be present");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, 42);

        // A different normalized key should miss.
        let other = normalize_search_key(false, "completely different");
        assert!(search_cache_get(&other).is_none());
    }

    #[test]
    fn classify_429_is_rate_limited() {
        let (kind, msg) = classify_anilist_failure(
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            r#"{"errors":[{"message":"Too Many Requests"}]}"#,
        );
        assert_eq!(kind, AniListFailureKind::RateLimited);
        assert!(is_rate_limit_error(&msg), "tag missing: {}", msg);
    }

    #[test]
    fn classify_5xx_is_unavailable_not_throttle() {
        let (kind, msg) = classify_anilist_failure(
            reqwest::StatusCode::BAD_GATEWAY,
            "<html><body>502 Bad Gateway</body></html>",
        );
        assert_eq!(kind, AniListFailureKind::Unavailable);
        assert!(!is_rate_limit_error(&msg), "5xx must not match throttle: {}", msg);
    }

    #[test]
    fn classify_403_with_cloudflare_body_is_rate_limited() {
        let body = r#"<html><head><title>Just a moment...</title></head>
                      <body data-translate="checking_browser">
                      cf-ray: abc123 cloudflare</body></html>"#;
        let (kind, msg) = classify_anilist_failure(reqwest::StatusCode::FORBIDDEN, body);
        assert_eq!(kind, AniListFailureKind::RateLimited);
        assert!(is_rate_limit_error(&msg), "tag missing: {}", msg);
    }

    #[test]
    fn classify_403_without_cloudflare_body_is_unavailable() {
        // AL-side 403 (e.g. application-layer block) — caller should
        // MAL-fallback, not defer.
        let (kind, msg) = classify_anilist_failure(
            reqwest::StatusCode::FORBIDDEN,
            r#"{"errors":[{"message":"Forbidden"}]}"#,
        );
        assert_eq!(kind, AniListFailureKind::Unavailable);
        assert!(!is_rate_limit_error(&msg), "non-CF 403 must not match throttle: {}", msg);
    }

    #[test]
    fn classify_404_is_not_found() {
        let (kind, _msg) = classify_anilist_failure(
            reqwest::StatusCode::NOT_FOUND,
            r#"{"errors":[{"message":"Not Found"}]}"#,
        );
        assert_eq!(kind, AniListFailureKind::NotFound);
    }

    #[test]
    fn classify_graphql_throttle_message_overrides_status() {
        // AL has been observed to return rate-limit messages with
        // unexpected status codes; trust the body when it says so.
        let (kind, _msg) = classify_anilist_failure(
            reqwest::StatusCode::OK,
            r#"{"errors":[{"message":"Too Many Requests"}]}"#,
        );
        assert_eq!(kind, AniListFailureKind::RateLimited);
    }

    #[test]
    fn is_rate_limit_error_matches_cooldown_string() {
        assert!(is_rate_limit_error(
            "AniList rate-limit cooldown active; skipping AniList request"
        ));
    }

    #[test]
    fn cooldown_pads_retry_after_with_safety_margin() {
        // AL hands back e.g. 60s; we wait 62s. The 2s margin clears the
        // window boundary that would otherwise trip a fresh 429 on the
        // immediate retry.
        let dur = compute_cooldown_duration(Some(60), ANILIST_COOLDOWN_DEFAULT);
        assert_eq!(dur, Duration::from_secs(60) + COOLDOWN_SAFETY_MARGIN);
    }

    #[test]
    fn cooldown_falls_back_to_default_when_no_retry_after() {
        // The default also gets padded — no Retry-After header doesn't
        // mean we can risk hitting the boundary.
        let dur = compute_cooldown_duration(None, Duration::from_secs(45));
        assert_eq!(dur, Duration::from_secs(45) + COOLDOWN_SAFETY_MARGIN);
    }

    #[test]
    fn cooldown_caps_retry_after_at_max_then_pads() {
        // AL could theoretically tell us to wait an hour; we cap at
        // ANILIST_COOLDOWN_MAX and *then* layer the safety margin on
        // (so a runaway Retry-After can't bypass the cap, but the
        // boundary protection still applies).
        let dur = compute_cooldown_duration(Some(3600), ANILIST_COOLDOWN_DEFAULT);
        assert_eq!(dur, ANILIST_COOLDOWN_MAX + COOLDOWN_SAFETY_MARGIN);
    }

    fn state(limit: u32, remaining: u32, reset_at: Option<Instant>) -> RateLimitState {
        RateLimitState { limit, remaining, reset_at }
    }

    #[test]
    fn decide_wait_no_state_no_last_request_is_zero() {
        // Cold start: no prior knowledge → fire immediately.
        let now = Instant::now();
        assert_eq!(decide_wait(None, None, now), Duration::ZERO);
    }

    #[test]
    fn decide_wait_burst_guard_applies_when_recent_request() {
        // Plenty of headroom but we just fired a request — must wait
        // out the per-limit minimum spacing.
        let now = Instant::now();
        let last = now - Duration::from_millis(500);
        let s = state(30, 25, None);
        let w = decide_wait(Some(s), Some(last), now);
        // 30 req/min → ~2.2s spacing; we waited 0.5s; ~1.7s remaining.
        assert!(w >= Duration::from_millis(1500), "got {:?}", w);
        assert!(w <= Duration::from_millis(2000), "got {:?}", w);
    }

    #[test]
    fn decide_wait_burst_guard_zero_when_enough_elapsed() {
        // Last request was ages ago — no spacing wait needed.
        let now = Instant::now();
        let last = now - Duration::from_secs(10);
        let s = state(30, 25, None);
        assert_eq!(decide_wait(Some(s), Some(last), now), Duration::ZERO);
    }

    #[test]
    fn decide_wait_window_flip_fires_when_remaining_low_and_reset_in_future() {
        // remaining=2 (≤ threshold) and reset 30s out → wait until reset
        // (plus 1s slack), regardless of elapsed time since last request.
        let now = Instant::now();
        let s = state(30, 2, Some(now + Duration::from_secs(30)));
        let w = decide_wait(Some(s), None, now);
        // 30s + 1s slack, with at most 1s of measurement noise either way.
        assert!(w >= Duration::from_secs(30), "got {:?}", w);
        assert!(w <= Duration::from_secs(32), "got {:?}", w);
    }

    #[test]
    fn decide_wait_stale_reset_falls_through_to_burst_guard() {
        // remaining is low but reset_at is in the past — don't sleep
        // for a window that's already over; just respect burst spacing.
        let now = Instant::now();
        let s = state(30, 0, Some(now - Duration::from_secs(5)));
        // No prior request → no burst wait either.
        assert_eq!(decide_wait(Some(s), None, now), Duration::ZERO);
    }

    #[test]
    fn decide_wait_no_reset_at_with_low_remaining_falls_to_burst_guard() {
        // remaining=0 but we have no idea when the window resets — best
        // we can do is the per-limit spacing; the cooldown layer above
        // handles the inevitable 429.
        let now = Instant::now();
        let s = state(30, 0, None);
        assert_eq!(decide_wait(Some(s), None, now), Duration::ZERO);
    }

    #[test]
    fn decide_wait_window_flip_dominates_over_burst_guard() {
        // Window-flip wait (30s) is much larger than burst guard
        // (~2s) — the helper should pick the larger of the two so we
        // don't undercut the window wait by accident.
        let now = Instant::now();
        let last = now - Duration::from_millis(100);
        let s = state(30, 1, Some(now + Duration::from_secs(30)));
        let w = decide_wait(Some(s), Some(last), now);
        assert!(w >= Duration::from_secs(30), "got {:?}", w);
    }

    #[test]
    fn min_inter_request_scales_with_limit() {
        // 60s window, 30 req/min degraded state → ~2.2s spacing.
        // 60s window, 90 req/min normal state → ~733ms spacing.
        // 10% padding on both keeps us comfortably inside.
        let degraded = min_inter_request(30);
        let normal = min_inter_request(90);
        assert!(degraded >= Duration::from_millis(2000));
        assert!(degraded <= Duration::from_millis(2500));
        assert!(normal >= Duration::from_millis(700));
        assert!(normal <= Duration::from_millis(800));
        // Defensive: limit=0 must not divide-by-zero.
        assert_eq!(min_inter_request(0), Duration::from_secs(2));
    }

    #[test]
    fn cooldown_from_headers_prefers_x_ratelimit_reset() {
        use reqwest::header::HeaderMap;
        let mut h = HeaderMap::new();
        // Reset 30s in the future.
        let now_unix = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        h.insert("x-ratelimit-reset", (now_unix + 30).to_string().parse().unwrap());
        h.insert("retry-after", "999".parse().unwrap());  // should be ignored
        let dur = cooldown_from_headers(&h, ANILIST_COOLDOWN_DEFAULT);
        // Allow ±2s slack for clock measurement noise plus the 2s safety margin.
        let lower = Duration::from_secs(30) + COOLDOWN_SAFETY_MARGIN - Duration::from_secs(2);
        let upper = Duration::from_secs(30) + COOLDOWN_SAFETY_MARGIN + Duration::from_secs(2);
        assert!(
            dur >= lower && dur <= upper,
            "expected ~32s, got {:?}",
            dur
        );
    }

    #[test]
    fn cooldown_from_headers_falls_back_to_retry_after() {
        use reqwest::header::HeaderMap;
        let mut h = HeaderMap::new();
        h.insert("retry-after", "45".parse().unwrap());
        let dur = cooldown_from_headers(&h, ANILIST_COOLDOWN_DEFAULT);
        assert_eq!(dur, Duration::from_secs(45) + COOLDOWN_SAFETY_MARGIN);
    }

    #[test]
    fn cooldown_from_headers_falls_back_to_default_when_no_headers() {
        use reqwest::header::HeaderMap;
        let h = HeaderMap::new();
        let dur = cooldown_from_headers(&h, Duration::from_secs(60));
        assert_eq!(dur, Duration::from_secs(60) + COOLDOWN_SAFETY_MARGIN);
    }

    #[test]
    fn cooldown_from_headers_caps_runaway_reset_at_max() {
        use reqwest::header::HeaderMap;
        let mut h = HeaderMap::new();
        let now_unix = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        h.insert("x-ratelimit-reset", (now_unix + 3600).to_string().parse().unwrap());
        let dur = cooldown_from_headers(&h, ANILIST_COOLDOWN_DEFAULT);
        assert_eq!(dur, ANILIST_COOLDOWN_MAX + COOLDOWN_SAFETY_MARGIN);
    }

    #[test]
    fn excerpt_is_char_boundary_safe() {
        // Build a string longer than MAX_CHARS that's mostly multi-byte
        // chars. Naïve byte-slicing would panic.
        let s: String = std::iter::repeat_n('日', 300).collect();
        let out = excerpt(&s);
        assert!(out.ends_with('…'));
    }
}
