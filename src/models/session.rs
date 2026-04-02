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

/// Validate a session token and return the user_id if valid.
pub async fn validate_session(db: &SqlitePool, token: &str) -> Result<Option<i64>, sqlx::Error> {
    let row: Option<(i64,)> =
        sqlx::query_as("SELECT user_id FROM sessions WHERE token = ?")
            .bind(token)
            .fetch_optional(db)
            .await?;

    Ok(row.map(|(id,)| id))
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
    let bytes: Vec<u8> = (0..32).map(|_| rng.gen()).collect();
    hex::encode(bytes)
}
