//! AniList rate-limit pacing and cooldown machinery.
//!
//! Split out of `anilist/mod.rs` because this file owns all of the process-wide
//! mutable state around AniList throttling (burst spacing, window reset,
//! cooldown-until, the rate-limit header snapshot) plus the pure helpers that
//! decide *when* to sleep and *for how long*. The module's public surface
//! (`anilist_cooldown_active`, `is_rate_limit_error`) is re-exported from the
//! parent so existing callers still reach it at `services::anilist::foo`.
//!
//! See `throttle_before_anilist_request` for the overall pacing strategy and
//! `classify_anilist_failure` for the failure-kind taxonomy (throttle vs.
//! unavailable vs. not-found) that downstream code keys MAL-fallback decisions
//! off of.

use std::sync::{LazyLock, Mutex as StdMutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How long to treat AniList as unavailable after a 429/5xx. If AniList sends a
/// `Retry-After` header we use that value instead (capped at 5 minutes).
pub(super) const ANILIST_COOLDOWN_DEFAULT: Duration = Duration::from_secs(60);
const ANILIST_COOLDOWN_MAX: Duration = Duration::from_secs(300);

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
static LAST_AL_REQUEST: LazyLock<StdMutex<Option<Instant>>> = LazyLock::new(|| StdMutex::new(None));

/// Monotonically-set "AniList is in cooldown until Instant". Consulted at the
/// top of every search so that once we've learned AniList is rate-limiting us,
/// we stop wasting a round-trip per search (which was dragging Jikan into the
/// rate-limit bucket too).
static ANILIST_COOLDOWN_UNTIL: LazyLock<StdMutex<Option<Instant>>> =
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
pub(super) fn record_rate_limit_headers(headers: &reqwest::header::HeaderMap) {
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
            limit: limit
                .or(prev.map(|s| s.limit))
                .unwrap_or(ANILIST_LIMIT_FALLBACK),
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
pub(super) async fn throttle_before_anilist_request() {
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
pub(super) fn cooldown_from_headers(
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

pub fn anilist_cooldown_active() -> bool {
    if let Ok(guard) = ANILIST_COOLDOWN_UNTIL.lock()
        && let Some(until) = *guard
    {
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
pub(super) enum AniListFailureKind {
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
pub(super) fn classify_anilist_failure(
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
            format!(
                "AniList rate-limited: Cloudflare challenge (HTTP {})",
                status
            ),
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
pub(super) fn excerpt(text: &str) -> String {
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
fn compute_cooldown_duration(retry_after_secs: Option<u64>, default_dur: Duration) -> Duration {
    let base = retry_after_secs
        .map(Duration::from_secs)
        .unwrap_or(default_dur)
        .min(ANILIST_COOLDOWN_MAX);
    base + COOLDOWN_SAFETY_MARGIN
}

pub(super) fn set_anilist_cooldown(retry_after_secs: Option<u64>, default_dur: Duration) {
    let dur = compute_cooldown_duration(retry_after_secs, default_dur);
    set_cooldown_until_now_plus(dur);
}

/// Apply the AL rate-limit policy to a response from any AL endpoint
/// reachable outside the `services::anilist` module. Records the
/// `X-RateLimit-*` headroom counters on every response (so the next
/// in-module AL call sees a fresh-from-AL view of the window), and
/// flips the process-wide cooldown on 429 / 5xx so subsequent AL
/// calls (metadata sync, library page render, scoring path) back
/// off.
///
/// Exists so the link-flow viewer fetch in `handlers::oauth` can
/// fully participate in the same throttle state as the in-module
/// callers without exposing the internal `(super)` primitives.
/// Mirrors the per-response `record_rate_limit_headers` call plus
/// the 429/5xx branch in `fetch_media_list_collection`.
pub fn note_external_anilist_response(
    status: reqwest::StatusCode,
    headers: &reqwest::header::HeaderMap,
) {
    record_rate_limit_headers(headers);
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
        let dur = cooldown_from_headers(headers, ANILIST_COOLDOWN_DEFAULT);
        set_cooldown_until_now_plus(dur);
    }
}

/// Set the cooldown-until marker to `now + dur`. Used by `mod.rs` call sites
/// that have already computed a duration via `cooldown_from_headers` — they
/// don't need the `compute_cooldown_duration` step.
pub(super) fn set_cooldown_until_now_plus(dur: Duration) {
    if let Ok(mut guard) = ANILIST_COOLDOWN_UNTIL.lock() {
        *guard = Some(Instant::now() + dur);
    }
}

/// Test-only: clear the global cooldown + rate-limit-headroom state.
/// Both are process-wide LazyLocks, so a stray cooldown left over
/// from one test (e.g. a wiremock that returned 429) would block the
/// next test's request. Tests that exercise the AL HTTP path should
/// call this at entry and exit.
#[cfg(any(test, feature = "test-support"))]
pub fn reset_state_for_tests() {
    if let Ok(mut g) = ANILIST_COOLDOWN_UNTIL.lock() {
        *g = None;
    }
    if let Ok(mut g) = RATE_LIMIT_STATE.lock() {
        *g = None;
    }
}

/// Extract the first GraphQL-level error message from an AniList response
/// body. Shared with `mod.rs` callers that need to surface the GraphQL
/// message alongside the HTTP status.
pub(super) fn extract_graphql_error(body: &serde_json::Value) -> Option<String> {
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
        assert!(
            !is_rate_limit_error(&msg),
            "5xx must not match throttle: {}",
            msg
        );
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
        let (kind, msg) = classify_anilist_failure(
            reqwest::StatusCode::FORBIDDEN,
            r#"{"errors":[{"message":"Forbidden"}]}"#,
        );
        assert_eq!(kind, AniListFailureKind::Unavailable);
        assert!(
            !is_rate_limit_error(&msg),
            "non-CF 403 must not match throttle: {}",
            msg
        );
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
        let dur = compute_cooldown_duration(Some(60), ANILIST_COOLDOWN_DEFAULT);
        assert_eq!(dur, Duration::from_secs(60) + COOLDOWN_SAFETY_MARGIN);
    }

    #[test]
    fn cooldown_falls_back_to_default_when_no_retry_after() {
        let dur = compute_cooldown_duration(None, Duration::from_secs(45));
        assert_eq!(dur, Duration::from_secs(45) + COOLDOWN_SAFETY_MARGIN);
    }

    #[test]
    fn cooldown_caps_retry_after_at_max_then_pads() {
        let dur = compute_cooldown_duration(Some(3600), ANILIST_COOLDOWN_DEFAULT);
        assert_eq!(dur, ANILIST_COOLDOWN_MAX + COOLDOWN_SAFETY_MARGIN);
    }

    fn state(limit: u32, remaining: u32, reset_at: Option<Instant>) -> RateLimitState {
        RateLimitState {
            limit,
            remaining,
            reset_at,
        }
    }

    #[test]
    fn decide_wait_no_state_no_last_request_is_zero() {
        let now = Instant::now();
        assert_eq!(decide_wait(None, None, now), Duration::ZERO);
    }

    #[test]
    fn decide_wait_burst_guard_applies_when_recent_request() {
        let now = Instant::now();
        let last = now - Duration::from_millis(500);
        let s = state(30, 25, None);
        let w = decide_wait(Some(s), Some(last), now);
        assert!(w >= Duration::from_millis(1500), "got {:?}", w);
        assert!(w <= Duration::from_millis(2000), "got {:?}", w);
    }

    #[test]
    fn decide_wait_burst_guard_zero_when_enough_elapsed() {
        let now = Instant::now();
        let last = now - Duration::from_secs(10);
        let s = state(30, 25, None);
        assert_eq!(decide_wait(Some(s), Some(last), now), Duration::ZERO);
    }

    #[test]
    fn decide_wait_window_flip_fires_when_remaining_low_and_reset_in_future() {
        let now = Instant::now();
        let s = state(30, 2, Some(now + Duration::from_secs(30)));
        let w = decide_wait(Some(s), None, now);
        assert!(w >= Duration::from_secs(30), "got {:?}", w);
        assert!(w <= Duration::from_secs(32), "got {:?}", w);
    }

    #[test]
    fn decide_wait_stale_reset_falls_through_to_burst_guard() {
        let now = Instant::now();
        let s = state(30, 0, Some(now - Duration::from_secs(5)));
        assert_eq!(decide_wait(Some(s), None, now), Duration::ZERO);
    }

    #[test]
    fn decide_wait_no_reset_at_with_low_remaining_falls_to_burst_guard() {
        let now = Instant::now();
        let s = state(30, 0, None);
        assert_eq!(decide_wait(Some(s), None, now), Duration::ZERO);
    }

    #[test]
    fn decide_wait_window_flip_dominates_over_burst_guard() {
        let now = Instant::now();
        let last = now - Duration::from_millis(100);
        let s = state(30, 1, Some(now + Duration::from_secs(30)));
        let w = decide_wait(Some(s), Some(last), now);
        assert!(w >= Duration::from_secs(30), "got {:?}", w);
    }

    #[test]
    fn min_inter_request_scales_with_limit() {
        let degraded = min_inter_request(30);
        let normal = min_inter_request(90);
        assert!(degraded >= Duration::from_millis(2000));
        assert!(degraded <= Duration::from_millis(2500));
        assert!(normal >= Duration::from_millis(700));
        assert!(normal <= Duration::from_millis(800));
        assert_eq!(min_inter_request(0), Duration::from_secs(2));
    }

    #[test]
    fn cooldown_from_headers_prefers_x_ratelimit_reset() {
        use reqwest::header::HeaderMap;
        let mut h = HeaderMap::new();
        let now_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        h.insert(
            "x-ratelimit-reset",
            (now_unix + 30).to_string().parse().unwrap(),
        );
        h.insert("retry-after", "999".parse().unwrap());
        let dur = cooldown_from_headers(&h, ANILIST_COOLDOWN_DEFAULT);
        let lower = Duration::from_secs(30) + COOLDOWN_SAFETY_MARGIN - Duration::from_secs(2);
        let upper = Duration::from_secs(30) + COOLDOWN_SAFETY_MARGIN + Duration::from_secs(2);
        assert!(dur >= lower && dur <= upper, "expected ~32s, got {:?}", dur);
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
        let now_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        h.insert(
            "x-ratelimit-reset",
            (now_unix + 3600).to_string().parse().unwrap(),
        );
        let dur = cooldown_from_headers(&h, ANILIST_COOLDOWN_DEFAULT);
        assert_eq!(dur, ANILIST_COOLDOWN_MAX + COOLDOWN_SAFETY_MARGIN);
    }

    #[test]
    fn excerpt_is_char_boundary_safe() {
        let s: String = std::iter::repeat_n('日', 300).collect();
        let out = excerpt(&s);
        assert!(out.ends_with('…'));
    }
}
