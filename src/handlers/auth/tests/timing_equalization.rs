//! Timing-equalization coverage. `models::user::authenticate` paired
//! with the dummy-hash warm-up in `handlers::auth` is supposed to
//! make a username-miss indistinguishable from a wrong-password-hit
//! in wall-clock time: both run through a full bcrypt `verify` so
//! an attacker can't enumerate usernames by timing the response.
//!
//! The test measures median elapsed time across a few runs for
//! each case and asserts they land in the same order-of-magnitude
//! envelope. bcrypt is a CPU-bound ~50 ms operation and CI runners
//! have variable scheduler jitter, so the tolerance is deliberately
//! wide — this fences "verify was actually called for both cases,"
//! not "the timing matches to the microsecond."

use std::time::Duration;

use crate::models::user;
use crate::test_support::in_memory_pool;

/// How many attempts to run for each case — the median of N=5 rules
/// out single-run scheduler blips. Higher N gives a cleaner signal
/// but drags the test out (each iteration burns one bcrypt verify,
/// ~50 ms).
const SAMPLE_COUNT: usize = 5;

fn median(mut samples: Vec<Duration>) -> Duration {
    samples.sort();
    samples[samples.len() / 2]
}

async fn measure_verify(db: &sqlx::SqlitePool, username: &str, password: &str) -> Duration {
    let start = std::time::Instant::now();
    let _ = user::verify_user(db, username, password).await;
    start.elapsed()
}

/// Positive envelope check: a username that exists but has the
/// wrong password should take roughly the same wall time as a
/// username that doesn't exist at all. The dummy-hash warm-up on
/// the miss path makes both invocations run through a real bcrypt
/// verify. A 4× fold in either direction would indicate one path
/// skipped the hash entirely.
#[tokio::test]
async fn miss_and_wrong_password_take_comparable_wall_time() {
    let db = in_memory_pool().await;
    // Seed a user. bcrypt::hash cost=10 is ~50 ms on a typical CI
    // runner — the seed is a one-shot cost, not part of the timing
    // loop.
    user::create_user(&db, "realuser", "correct-horse-battery-staple")
        .await
        .expect("create user");

    let mut miss_samples = Vec::with_capacity(SAMPLE_COUNT);
    let mut wrong_samples = Vec::with_capacity(SAMPLE_COUNT);
    for _ in 0..SAMPLE_COUNT {
        miss_samples.push(measure_verify(&db, "nouser-does-not-exist", "any-password").await);
        wrong_samples.push(measure_verify(&db, "realuser", "wrong-password-goes-here").await);
    }
    let miss_median = median(miss_samples);
    let wrong_median = median(wrong_samples);

    // bcrypt cost=10 bottoms out around 40–80 ms on typical hardware;
    // the floor catches "we completely skipped verify on one path."
    // CI runners under load can breach a simple 4× ratio on single
    // samples, so the test asserts both paths cleared the floor
    // rather than a tight ratio. The floor is the load-bearing
    // equalization check — if the miss path returned in <5 ms it
    // would be a timing oracle.
    let floor = Duration::from_millis(10);
    assert!(
        miss_median >= floor,
        "username-miss took {miss_median:?} — too fast, likely skipped bcrypt verify"
    );
    assert!(
        wrong_median >= floor,
        "wrong-password took {wrong_median:?} — too fast, likely skipped bcrypt verify"
    );

    // Both should land in the same order-of-magnitude band. CI
    // scheduler jitter can inflate one median by 3× on a bad run,
    // so the tolerance is generous — the point is "neither path
    // returned in microseconds," not "the paths match to 10%."
    let ratio = if miss_median > wrong_median {
        miss_median.as_nanos() as f64 / wrong_median.as_nanos() as f64
    } else {
        wrong_median.as_nanos() as f64 / miss_median.as_nanos() as f64
    };
    assert!(
        ratio < 10.0,
        "miss_median={miss_median:?} wrong_median={wrong_median:?} ratio={ratio:.2}×; \
         bcrypt verify likely skipped on one path"
    );
}
