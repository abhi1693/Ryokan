use sqlx::SqlitePool;
use std::sync::LazyLock;

#[allow(dead_code)]
pub struct User {
    pub id: i64,
    pub username: String,
    pub password_hash: String,
}

/// Pre-computed bcrypt hash of a dummy password, used to equalize the
/// timing of failed logins against nonexistent usernames with failed
/// logins against existing usernames. Without this, the `None` branch
/// of [`verify_user`] would short-circuit to `Ok(None)` and finish ~50ms
/// faster than the `Some` branch that actually calls `bcrypt::verify`,
/// giving an attacker a trivial timing side channel for username
/// enumeration. Computed once at first use so the cost is paid only
/// once for the entire process.
static DUMMY_BCRYPT_HASH: LazyLock<String> = LazyLock::new(|| {
    // The input password here is irrelevant — no real user will ever
    // authenticate against this hash because the comparison always
    // happens against random attacker-supplied input.
    bcrypt::hash("ryokan-timing-equalizer-dummy-password", 10)
        .expect("bcrypt::hash of a fixed input should not fail")
});

/// Force the [`DUMMY_BCRYPT_HASH`] LazyLock to initialise. Call once during
/// startup so the ~50ms `bcrypt::hash` cost is paid before the first login
/// probe hits it — otherwise a cold process has a one-shot timing oracle on
/// its first failed-username attempt (bcrypt::hash + bcrypt::verify ≈ 100ms
/// vs the ~50ms of every subsequent attempt).
pub fn warm_timing_equalizer() {
    let _ = &*DUMMY_BCRYPT_HASH;
}

/// Check if any users exist (for first-run setup detection).
pub async fn has_users(db: &SqlitePool) -> Result<bool, sqlx::Error> {
    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users")
        .fetch_one(db)
        .await?;
    Ok(count.0 > 0)
}

/// Create a new user with a bcrypt-hashed password.
pub async fn create_user(db: &SqlitePool, username: &str, password: &str) -> Result<i64, String> {
    let hash = bcrypt::hash(password, 10).map_err(|e| format!("Hash error: {}", e))?;

    let result = sqlx::query("INSERT INTO users (username, password_hash) VALUES (?, ?)")
        .bind(username)
        .bind(&hash)
        .execute(db)
        .await
        .map_err(|e| format!("DB error: {}", e))?;

    Ok(result.last_insert_rowid())
}

/// Verify credentials and return the user if valid.
///
/// Timing note: when the username does not exist, this still runs a
/// `bcrypt::verify` against a dummy hash so the failure path takes the
/// same wall time as a real "wrong password" failure. Without this,
/// an attacker can enumerate valid usernames in a timing side channel
/// (the missing-user branch would short-circuit ~50ms faster than the
/// bcrypt-verify branch).
pub async fn verify_user(db: &SqlitePool, username: &str, password: &str) -> Result<Option<User>, String> {
    let row: Option<(i64, String, String)> =
        sqlx::query_as("SELECT id, username, password_hash FROM users WHERE username = ?")
            .bind(username)
            .fetch_optional(db)
            .await
            .map_err(|e| format!("DB error: {}", e))?;

    match row {
        Some((id, uname, hash)) => {
            let valid = bcrypt::verify(password, &hash).unwrap_or(false);
            if valid {
                Ok(Some(User {
                    id,
                    username: uname,
                    password_hash: hash,
                }))
            } else {
                Ok(None)
            }
        }
        None => {
            // Burn equivalent CPU time against the dummy hash and discard
            // the result. `.unwrap_or(false)` mirrors the Some-branch
            // handling so any bcrypt error produces the same outcome as
            // a wrong password.
            let _ = bcrypt::verify(password, &DUMMY_BCRYPT_HASH).unwrap_or(false);
            Ok(None)
        }
    }
}
