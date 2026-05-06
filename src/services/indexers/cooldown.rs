//! Per-indexer 429 cooldown table.
//!
//! Mirrors `services::jikan::JIKAN_COOLDOWN_UNTIL` in shape, but
//! keyed by `indexer_id` because each Prowlarr-fronted indexer has
//! its own per-tracker rate-limit budget. A single shared cooldown
//! would make AB's 429 silence a healthy NZBGeek for the same
//! window, which is wrong.
//!
//! When the torznab client sees a 429:
//!   1. Read `Retry-After` (capped at [`COOLDOWN_MAX`], defaulted
//!      to [`COOLDOWN_DEFAULT`] when the header is missing /
//!      unparseable).
//!   2. Stamp `until = now + dur` in the table under that indexer's
//!      id.
//!   3. Subsequent calls for that id short-circuit at the top of
//!      `fetch()` with a "rate-limited (cooldown Ns remaining)"
//!      error so a 429-storm doesn't pile up more 429s and so
//!      [`crate::services::auto_search`] fan-outs skip the indexer
//!      entirely while the cooldown is active.
//!
//! Per-id (not global) so a 429 on AB doesn't suppress NZBGeek.
//! 60s default / 300s max — same envelope as Jikan's machine since
//! Prowlarr-fronted upstreams typically publish "wait a minute"
//! retry windows, not "wait an hour."

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex as StdMutex};
use std::time::{Duration, Instant};

/// Default cooldown applied when a 429 lands without a parseable
/// `Retry-After` header.
pub const COOLDOWN_DEFAULT: Duration = Duration::from_secs(60);

/// Hard ceiling on cooldown duration. Honors upstream `Retry-After`
/// up to this cap so a misconfigured indexer claiming a 24h backoff
/// doesn't quarantine itself for a day.
pub const COOLDOWN_MAX: Duration = Duration::from_secs(300);

static COOLDOWN_UNTIL: LazyLock<StdMutex<HashMap<i64, Instant>>> =
    LazyLock::new(|| StdMutex::new(HashMap::new()));

/// How long until the indexer's cooldown lifts. `None` = no active
/// cooldown (or the prior one already expired). Lazy-cleans expired
/// entries off the read path so the map doesn't grow indefinitely on
/// long-lived processes.
pub fn remaining(indexer_id: i64) -> Option<Duration> {
    let mut guard = COOLDOWN_UNTIL.lock().ok()?;
    let until = *guard.get(&indexer_id)?;
    let now = Instant::now();
    if now < until {
        Some(until - now)
    } else {
        guard.remove(&indexer_id);
        None
    }
}

/// Stamp a cooldown for this indexer. `retry_after_secs = None`
/// applies [`COOLDOWN_DEFAULT`]; any value above [`COOLDOWN_MAX`]
/// is clamped. Idempotent — repeated 429s during an active window
/// just re-set the deadline (a later, longer Retry-After replaces
/// an earlier shorter one).
pub fn record_429(indexer_id: i64, retry_after_secs: Option<u64>) {
    let dur = retry_after_secs
        .map(Duration::from_secs)
        .unwrap_or(COOLDOWN_DEFAULT)
        .min(COOLDOWN_MAX);
    if let Ok(mut guard) = COOLDOWN_UNTIL.lock() {
        guard.insert(indexer_id, Instant::now() + dur);
    }
}

/// Test-only: drop the cooldown stamp for a single id. **Prefer
/// this over [`clear_all_for_tests`]** — the global table is
/// process-static, and `clear_all` racing under nextest's default
/// parallelism can blow away another test's freshly-stamped row
/// between the stamp and its read. Per-id cleanup limits each
/// test to mutating only the ids it owns.
#[cfg(any(test, feature = "test-support"))]
pub fn remove_for_tests(indexer_id: i64) {
    if let Ok(mut guard) = COOLDOWN_UNTIL.lock() {
        guard.remove(&indexer_id);
    }
}

/// Test-only: drop every cooldown stamp. **Race-prone under
/// nextest** because the cooldown table is process-static and
/// tests share the binary; prefer [`remove_for_tests`] which
/// isolates each test to its own id. Kept only as a last-resort
/// reset for callers that genuinely don't know which ids got
/// stamped.
#[cfg(any(test, feature = "test-support"))]
pub fn clear_all_for_tests() {
    if let Ok(mut guard) = COOLDOWN_UNTIL.lock() {
        guard.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Each test below uses a unique id range and `remove_for_tests`
    // for cleanup so concurrent tests under nextest's default
    // parallelism can't race on the process-global cooldown table.
    // The torznab wiremock_tests fixture uses id=7 — pick a non-7
    // base in this module so cross-module concurrency stays safe.
    // Bases: 7100/7101 (clamp), 7200 (default), 7900 (none),
    // 7300/7301 (per-id), 7400 (replace).

    #[test]
    fn record_429_with_retry_after_sets_clamped_cooldown() {
        let id = 7100;
        remove_for_tests(id);
        // 600s upstream Retry-After is clamped to COOLDOWN_MAX (300s).
        record_429(id, Some(600));
        let r = remaining(id).expect("cooldown active immediately");
        assert!(
            r <= COOLDOWN_MAX && r > COOLDOWN_MAX - Duration::from_secs(1),
            "expected ~{}s, got {:?}",
            COOLDOWN_MAX.as_secs(),
            r
        );
        remove_for_tests(id);
    }

    #[test]
    fn record_429_without_retry_after_uses_default() {
        let id = 7200;
        remove_for_tests(id);
        record_429(id, None);
        let r = remaining(id).expect("cooldown active immediately");
        assert!(
            r <= COOLDOWN_DEFAULT && r > COOLDOWN_DEFAULT - Duration::from_secs(1),
            "expected ~{}s default, got {:?}",
            COOLDOWN_DEFAULT.as_secs(),
            r
        );
        remove_for_tests(id);
    }

    #[test]
    fn remaining_returns_none_for_unstamped_indexer() {
        let id = 7900;
        remove_for_tests(id);
        assert!(remaining(id).is_none());
    }

    #[test]
    fn record_429_is_per_id_not_global() {
        let a = 7300;
        let b = 7301;
        remove_for_tests(a);
        remove_for_tests(b);
        record_429(a, Some(60));
        assert!(remaining(a).is_some(), "id {a} should be cooled down");
        assert!(
            remaining(b).is_none(),
            "id {b} must NOT inherit id {a}'s cooldown — per-tracker rate-limit budget"
        );
        remove_for_tests(a);
    }

    #[test]
    fn later_429_replaces_earlier_cooldown_window() {
        let id = 7400;
        remove_for_tests(id);
        record_429(id, Some(30));
        let first = remaining(id).expect("first cooldown active");
        // A second 429 with a longer Retry-After should extend the
        // window, not be ignored.
        record_429(id, Some(120));
        let second = remaining(id).expect("second cooldown active");
        assert!(
            second > first,
            "second cooldown ({second:?}) must extend past the first ({first:?})"
        );
        remove_for_tests(id);
    }
}
