//! Throttle tier + failure-bookkeeping coverage for `login_check`,
//! `login_record_failure`, `login_clear`, and `sweep_login_failures`.
//!
//! `LOGIN_FAILURES` is a process-global `Mutex<HashMap<String, _>>`.
//! Parallel tests can safely share it as long as each test uses its
//! own unique key namespace — the keys are strings, buckets are
//! created on demand, and mutations to different keys don't affect
//! each other. Every test here derives its keys from the test
//! function name so concurrent runs don't interleave.
//!
//! For tests that care about "no prior state," a local call to
//! `reset_login_failures_for_test()` wipes the map; that's
//! process-wide so only use it when nothing else should race —
//! currently the empty-state tests guard against a prior-test
//! leak by inspecting their own key rather than by resetting.

use std::time::{Duration, Instant};

use crate::handlers::auth::{
    LOGIN_HARD_CAP, LOGIN_MAX_FAILURES, LOGIN_WINDOW, LoginCheck, login_check, login_clear,
    login_failure_count_for_test, login_record_failure, seed_login_failure_for_test,
    sweep_login_failures,
};

// ─── login_check tier thresholds ───────────────────────────────────

#[test]
fn login_check_returns_allow_when_no_failures_recorded() {
    let key = "test:login_check_returns_allow_when_no_failures_recorded";
    assert_eq!(login_check(key), LoginCheck::Allow);
}

#[test]
fn login_check_returns_allow_just_under_soft_cap() {
    let key = "test:login_check_returns_allow_just_under_soft_cap";
    for _ in 0..(LOGIN_MAX_FAILURES - 1) {
        login_record_failure(key);
    }
    assert_eq!(login_check(key), LoginCheck::Allow);
}

#[test]
fn login_check_flips_to_soft_throttle_at_the_soft_cap() {
    let key = "test:login_check_flips_to_soft_throttle_at_the_soft_cap";
    for _ in 0..LOGIN_MAX_FAILURES {
        login_record_failure(key);
    }
    assert_eq!(login_check(key), LoginCheck::SoftThrottled);
}

#[test]
fn login_check_stays_soft_throttled_just_under_hard_cap() {
    let key = "test:login_check_stays_soft_throttled_just_under_hard_cap";
    for _ in 0..(LOGIN_HARD_CAP - 1) {
        login_record_failure(key);
    }
    assert_eq!(login_check(key), LoginCheck::SoftThrottled);
}

#[test]
fn login_check_flips_to_hard_throttle_at_the_hard_cap() {
    let key = "test:login_check_flips_to_hard_throttle_at_the_hard_cap";
    for _ in 0..LOGIN_HARD_CAP {
        login_record_failure(key);
    }
    assert_eq!(login_check(key), LoginCheck::HardThrottled);
}

// ─── Bookkeeping side effects ──────────────────────────────────────

#[test]
fn login_record_failure_increments_the_recorded_count() {
    let key = "test:login_record_failure_increments_the_recorded_count";
    assert_eq!(login_failure_count_for_test(key), 0);
    login_record_failure(key);
    assert_eq!(login_failure_count_for_test(key), 1);
    login_record_failure(key);
    assert_eq!(login_failure_count_for_test(key), 2);
}

#[test]
fn login_clear_removes_all_recorded_failures_for_the_key() {
    let key = "test:login_clear_removes_all_recorded_failures_for_the_key";
    for _ in 0..3 {
        login_record_failure(key);
    }
    assert_eq!(login_failure_count_for_test(key), 3);
    login_clear(key);
    assert_eq!(login_failure_count_for_test(key), 0);
}

#[test]
fn login_clear_on_unseen_key_is_a_noop() {
    let key = "test:login_clear_on_unseen_key_is_a_noop";
    // No record ever written; clearing should not panic or error.
    login_clear(key);
    assert_eq!(login_failure_count_for_test(key), 0);
}

// ─── Per-key isolation ─────────────────────────────────────────────

#[test]
fn recording_failures_on_one_key_does_not_affect_another() {
    let key_a = "test:recording_failures_on_one_key_does_not_affect_another:A";
    let key_b = "test:recording_failures_on_one_key_does_not_affect_another:B";
    for _ in 0..LOGIN_MAX_FAILURES {
        login_record_failure(key_a);
    }
    assert_eq!(login_check(key_a), LoginCheck::SoftThrottled);
    // B never recorded anything → still Allow.
    assert_eq!(login_check(key_b), LoginCheck::Allow);
}

#[test]
fn clearing_one_key_does_not_clear_another() {
    let key_a = "test:clearing_one_key_does_not_clear_another:A";
    let key_b = "test:clearing_one_key_does_not_clear_another:B";
    login_record_failure(key_a);
    login_record_failure(key_b);
    login_record_failure(key_b);
    login_clear(key_a);
    assert_eq!(login_failure_count_for_test(key_a), 0);
    assert_eq!(login_failure_count_for_test(key_b), 2);
}

// ─── Sliding-window expiration ─────────────────────────────────────

#[test]
fn login_check_prunes_entries_older_than_the_window() {
    let key = "test:login_check_prunes_entries_older_than_the_window";
    // Seed a failure at "60s + 1ms ago" — past the cutoff.
    let stale = Instant::now() - LOGIN_WINDOW - Duration::from_millis(1);
    seed_login_failure_for_test(key, stale);
    assert_eq!(login_failure_count_for_test(key), 1);
    // login_check's per-key sweep should drop the stale entry.
    assert_eq!(login_check(key), LoginCheck::Allow);
    assert_eq!(
        login_failure_count_for_test(key),
        0,
        "stale entry should have been pruned by login_check"
    );
}

#[test]
fn login_check_keeps_entries_inside_the_window() {
    let key = "test:login_check_keeps_entries_inside_the_window";
    // Seed at "30s ago" — well inside the 60s window.
    let fresh = Instant::now() - Duration::from_secs(30);
    seed_login_failure_for_test(key, fresh);
    assert_eq!(login_check(key), LoginCheck::Allow);
    assert_eq!(
        login_failure_count_for_test(key),
        1,
        "fresh entry should NOT have been pruned"
    );
}

#[test]
fn sweep_login_failures_drops_buckets_that_fully_expire() {
    let key = "test:sweep_login_failures_drops_buckets_that_fully_expire";
    let stale = Instant::now() - LOGIN_WINDOW - Duration::from_millis(1);
    seed_login_failure_for_test(key, stale);
    seed_login_failure_for_test(key, stale);
    assert_eq!(login_failure_count_for_test(key), 2);
    sweep_login_failures();
    assert_eq!(
        login_failure_count_for_test(key),
        0,
        "sweep should prune the stale entries AND drop the empty bucket"
    );
}

#[test]
fn sweep_login_failures_preserves_partial_buckets() {
    let key = "test:sweep_login_failures_preserves_partial_buckets";
    let stale = Instant::now() - LOGIN_WINDOW - Duration::from_millis(1);
    let fresh = Instant::now() - Duration::from_secs(10);
    seed_login_failure_for_test(key, stale);
    seed_login_failure_for_test(key, fresh);
    assert_eq!(login_failure_count_for_test(key), 2);
    sweep_login_failures();
    assert_eq!(
        login_failure_count_for_test(key),
        1,
        "sweep should prune only the stale entry, keeping the fresh one"
    );
}
