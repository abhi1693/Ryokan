use sqlx::SqlitePool;

/// Create a new session token for a user.
pub async fn create_session(db: &SqlitePool, user_id: i64) -> Result<String, sqlx::Error> {
    let token = generate_token();

    sqlx::query("INSERT INTO sessions (token, user_id) VALUES (?, ?)")
        .bind(&token)
        .bind(user_id)
        .execute(db)
        .await?;

    Ok(token)
}

/// Validate a session token and return the user_id if valid. Sessions older
/// than 7 days are treated as invalid to match the 604800-second Max-Age
/// sent on the cookie itself — without this the server-side row was valid
/// forever and a stolen token never expired. Expired rows are swept by
/// [`cleanup`] from the hourly background task.
pub async fn validate_session(db: &SqlitePool, token: &str) -> Result<Option<i64>, sqlx::Error> {
    let row: Option<(i64,)> = sqlx::query_as(
        "SELECT user_id FROM sessions WHERE token = ? AND created_at > datetime('now', '-7 days')",
    )
    .bind(token)
    .fetch_optional(db)
    .await?;

    Ok(row.map(|(id,)| id))
}

/// Drop session rows whose `created_at` is older than `max_age_days`.
/// Called from the hourly cleanup task so the `sessions` table doesn't
/// accumulate expired rows indefinitely — without this sweep, every login
/// leaves a permanent row that [`validate_session`] simply ignores once
/// stale. Use 7 days to match the cookie Max-Age and the TTL check above.
pub async fn cleanup(db: &SqlitePool, max_age_days: i32) -> Result<u64, sqlx::Error> {
    let cutoff = format!("-{} days", max_age_days);
    let res = sqlx::query("DELETE FROM sessions WHERE created_at < datetime('now', ?)")
        .bind(cutoff)
        .execute(db)
        .await?;
    Ok(res.rows_affected())
}

/// Delete a session (logout).
pub async fn delete_session(db: &SqlitePool, token: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM sessions WHERE token = ?")
        .bind(token)
        .execute(db)
        .await?;
    Ok(())
}

/// Generate a cryptographically random session token.
fn generate_token() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let bytes: Vec<u8> = (0..32).map(|_| rng.r#gen()).collect();
    hex::encode(bytes)
}
