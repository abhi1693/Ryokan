use sqlx::SqlitePool;

#[allow(dead_code)]
pub struct User {
    pub id: i64,
    pub username: String,
    pub password_hash: String,
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
        None => Ok(None),
    }
}
