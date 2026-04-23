//! `POST_PROC_LOCK` serialization. Production uses
//! `tokio::sync::Mutex::try_lock` — a second overlapping `run_once`
//! returns early instead of queuing, so two concurrent post-
//! processing runs can't double-import files.
//!
//! The lock is a process-global `LazyLock<Mutex>`, so parallel tests
//! that acquire it will serialize against each other. Both tests
//! below hold the lock briefly enough (< ~100 ms) that they don't
//! slow the suite meaningfully, and each completes before releasing
//! the guard so there's no cross-test interference.

use std::time::Duration;

use crate::services::post_processing::POST_PROC_LOCK;

#[tokio::test]
async fn try_lock_returns_err_while_lock_is_held() {
    // Acquire the lock manually, simulating a `run_once` in flight.
    let _held = POST_PROC_LOCK
        .try_lock()
        .expect("lock should be free at test start");
    // A second try_lock must fail while the first guard is alive.
    let second = POST_PROC_LOCK.try_lock();
    assert!(
        second.is_err(),
        "second try_lock should fail while first is held"
    );
    // Explicit drop so the lock frees before the next test runs.
    drop(_held);
}

#[tokio::test]
async fn try_lock_succeeds_after_prior_holder_drops() {
    {
        let _held = POST_PROC_LOCK
            .try_lock()
            .expect("lock should be free at test start");
        // scope drops the guard here
    }
    // Give the scheduler a beat to unwind any contending acquires
    // from the serialized sibling test above.
    tokio::time::sleep(Duration::from_millis(5)).await;
    let acquired = POST_PROC_LOCK.try_lock();
    assert!(
        acquired.is_ok(),
        "lock should be re-acquirable after prior holder drops"
    );
}
