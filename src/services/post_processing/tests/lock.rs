//! `POST_PROC_LOCK` serialization. Production uses
//! `tokio::sync::Mutex::try_lock` — a second overlapping `run_once`
//! returns early instead of queuing, so two concurrent post-
//! processing runs can't double-import files.
//!
//! The test lives in a single function that walks the full state
//! machine sequentially: acquire, try-while-held (must fail),
//! drop, re-acquire. Splitting into two `#[tokio::test]` functions
//! would race — Rust's test harness runs them on separate threads
//! and `try_lock()` does not block, so a test-2 that asserts "lock
//! free at start" can panic when test-1 is still inside its guard
//! scope.

use crate::services::post_processing::POST_PROC_LOCK;

use super::POST_PROC_TEST_SERIALIZER;

#[tokio::test]
async fn post_proc_lock_serializes_via_try_lock_contention() {
    // Acquire the test-suite serializer first — `run_once.rs` tests
    // also touch `POST_PROC_LOCK` (indirectly, via the production
    // `try_lock`), and we don't want this assertion-heavy test to
    // race a peer that's mid-`list_scoped`. The serializer sits at
    // a higher tier than `POST_PROC_LOCK`; it must be acquired
    // first and dropped last (Rust's reverse-declaration drop
    // order handles this automatically when both are local lets).
    let _serializer = POST_PROC_TEST_SERIALIZER.lock().await;
    // 1. Acquire — must succeed from a fresh process state. If a
    //    prior test left the lock held this would panic, but
    //    `POST_PROC_LOCK` is a production-global that no other
    //    test touches (`run_once` is the only legitimate caller
    //    in `services::post_processing::mod.rs`, and that's not
    //    exercised from unit tests).
    let first = POST_PROC_LOCK
        .try_lock()
        .expect("lock must be free at test start");

    // 2. Second try_lock inside the first guard's scope — the
    //    contention case. This is the load-bearing property for
    //    the `run_once` drop-on-overlap semantic: if two ticks
    //    overlap, the second one's try_lock fails and `run_once`
    //    returns immediately rather than queuing.
    assert!(
        POST_PROC_LOCK.try_lock().is_err(),
        "second try_lock must fail while first guard is alive"
    );

    // 3. Drop the first guard explicitly — re-acquire semantics
    //    depend on the drop happening before the next try_lock.
    drop(first);

    // 4. Re-acquire — post-drop the lock is free again. This
    //    pins the "sticky lock" regression guard: a refactor
    //    that leaked the guard (holding it across await with
    //    an unexpected branch) would leave the lock held
    //    forever and fail this step.
    let _regained = POST_PROC_LOCK
        .try_lock()
        .expect("lock must be re-acquirable after prior holder drops");
}
