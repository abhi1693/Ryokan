//! Pure cursor-decision helpers used by `tick_once_inner` to choose
//! between full-resync and delta sync, and to drop pre-cursor
//! entries before merge. No DB, no clock — both functions take their
//! "now" timestamp as a parameter so they're trivially unit-testable.

use super::types::SyncEntry;

/// Seven days of seconds; the weekly full-resync backstop interval.
/// Made an associated constant rather than a magic number so the value
/// shows up in tests and so a future "raise this to 30 days" change is
/// trivial.
pub const FULL_RESYNC_INTERVAL_SECS: i64 = 7 * 24 * 60 * 60;

/// Decide whether the current tick should be a full resync (vs. a
/// delta from `list_last_synced_at`). True when:
///   - There's no `list_full_resync_at` yet (first sync after link).
///   - `list_full_resync_at` is older than the weekly backstop window.
///   - There's no `list_last_synced_at` either (cursor unset; nothing
///     to delta against — equivalent to a first sync).
///
/// Pure function so the cursor decision stays unit-testable without
/// mocking the clock.
pub fn should_full_resync(
    list_last_synced_at: Option<i64>,
    list_full_resync_at: Option<i64>,
    now_unix_ts: i64,
) -> bool {
    if list_last_synced_at.is_none() {
        return true;
    }
    match list_full_resync_at {
        None => true,
        Some(t) => now_unix_ts.saturating_sub(t) >= FULL_RESYNC_INTERVAL_SECS,
    }
}

/// Drop entries whose `updated_at` is strictly before the cursor —
/// the caller has already merged everything up to and including that
/// timestamp. With `cursor = None`, all entries pass through (used on
/// full-sync passes and on the first sync ever).
///
/// `>=` rather than `>` is deliberate: the cursor is captured BEFORE
/// the network fetch, so an entry the user just edited at exactly
/// `cursor` may or may not have been visible to the previous tick's
/// fetch (provider read-after-write timing, clock skew between us
/// and the provider). Re-merging an unchanged entry is idempotent
/// (existing-series → unchanged), but losing a changed entry is a
/// silent data bug — we'd never re-fetch it unless a later edit
/// bumped its timestamp. Inclusive boundary is the safe direction.
pub fn drop_entries_before_cursor(entries: Vec<SyncEntry>, cursor: Option<i64>) -> Vec<SyncEntry> {
    let Some(c) = cursor else {
        return entries;
    };
    entries.into_iter().filter(|e| e.updated_at >= c).collect()
}
