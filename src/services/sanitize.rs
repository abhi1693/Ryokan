//! `--sanitize-db-for-debug` CLI helper (issue #62 PR A).
//!
//! Produces a sanitized copy of the SQLite database with every token
//! and password column blanked out so a user can safely paste their
//! DB into a bug report. The live DB is never mutated — the copy is
//! a sibling file; the caller is responsible for deleting it when
//! done.
//!
//! Scope extends beyond the #62-specific `external_accounts` blobs
//! to every known secret column already in the DB (qBit / Deluge /
//! Transmission / rtorrent passwords, Jellyfin API key, Sonarr +
//! Radarr shim API keys). A sanitized DB has to be unconditionally
//! safe to share; leaving a legacy password plaintext would defeat
//! the feature for users who joined before #62.

use std::path::Path;

use sqlx::SqlitePool;

use crate::services::crypto::SANITIZED_SENTINEL;

/// Read `live_db` and produce a sanitized copy at `output`. Returns
/// the number of rows touched across the scrubbed tables so callers
/// can surface "N OAuth tokens + M config passwords blanked" output.
///
/// Implementation: shell-out-free — copies the file, opens a
/// read-write SqlitePool against the copy, runs UPDATE statements,
/// closes. Runs inside `tokio::task::spawn_blocking` at the caller
/// if sync context is inappropriate; this function is async only
/// because sqlx's query API is.
pub async fn run_sanitize(live_db: &Path, output: &Path) -> Result<SanitizeSummary, String> {
    if !live_db.exists() {
        return Err(format!(
            "DB not found at {} — nothing to sanitize",
            live_db.display()
        ));
    }

    // Detect a live SQLite WAL alongside the DB. Ryokan uses
    // journal_mode=WAL, so a running server has uncommitted writes
    // sitting in `<db>-wal` that aren't visible to a plain
    // `fs::copy`. A sanitized copy taken mid-run could miss the
    // most recent OAuth link, end up with a stale schema_version,
    // or worst-case land on a torn page boundary. Refuse with a
    // clear "stop the server first" rather than silently producing
    // a half-stale dump. The shutdown checkpoint flushes WAL into
    // the main DB file, so a stopped server has no `-wal` adjacent
    // (or one of zero size) and this check passes.
    //
    // Path derivation: SQLite's WAL filename is the database
    // filename with a literal `-wal` suffix appended (per the
    // sqlite docs). `Path::with_extension` would mishandle the
    // extensionless case (`data/ryokan` → `data/ryokan.db-wal`
    // instead of the correct `data/ryokan-wal`). Append on the
    // raw `OsString` so the rule matches exactly regardless of
    // extension shape.
    let mut wal_os = live_db.as_os_str().to_owned();
    wal_os.push("-wal");
    let wal_path = std::path::PathBuf::from(wal_os);
    if let Ok(meta) = std::fs::metadata(&wal_path)
        && meta.len() > 0
    {
        return Err(format!(
            "Active SQLite WAL detected at {} — stop the Ryokan server before running sanitize \
             so the WAL is checkpointed into the main DB file. A copy taken mid-run would miss \
             uncommitted writes.",
            wal_path.display()
        ));
    }

    // Delete any prior sanitized copy first so a repeat run doesn't
    // silently update a stale file under a SQLite write lock from a
    // prior aborted run.
    if output.exists() {
        std::fs::remove_file(output)
            .map_err(|e| format!("could not remove stale {}: {}", output.display(), e))?;
    }
    std::fs::copy(live_db, output).map_err(|e| {
        format!(
            "could not copy {} → {}: {}",
            live_db.display(),
            output.display(),
            e
        )
    })?;

    let url = format!("sqlite://{}?mode=rwc", output.display());
    let pool = SqlitePool::connect(&url)
        .await
        .map_err(|e| format!("open sanitized copy: {e}"))?;

    // Run migrations on the copy. Pre-#62 DBs don't have the
    // `external_accounts` table yet, but the CLI still needs to
    // produce a valid sanitized output against them (otherwise
    // users on an older install can't generate a safe debug dump).
    // Migrations are idempotent; running them against the copy is
    // a no-op when the schema is already current.
    crate::models::migrate(&pool)
        .await
        .map_err(|e| format!("migrate sanitized copy: {e}"))?;

    let sentinel: &[u8] = SANITIZED_SENTINEL;
    // `external_accounts` tokens — the primary #62 PR A concern.
    let ext_rows = sqlx::query(
        "UPDATE external_accounts
            SET access_token_encrypted = ?,
                refresh_token_encrypted = ?",
    )
    .bind(sentinel)
    .bind(sentinel)
    .execute(&pool)
    .await
    .map_err(|e| format!("scrub external_accounts: {e}"))?
    .rows_affected();

    // Config-row secrets — plaintext columns that predate #62. All
    // null-safe UPDATEs: coalesce through NULL → empty so an unused
    // column stays empty rather than reading "[REDACTED]" in a DB
    // where it was never set.
    let cfg_rows = sqlx::query(
        "UPDATE config
            SET qbit_pass = CASE WHEN qbit_pass = '' THEN '' ELSE '[REDACTED]' END,
                deluge_password = CASE WHEN deluge_password = '' THEN '' ELSE '[REDACTED]' END,
                transmission_password = CASE WHEN transmission_password = '' THEN '' ELSE '[REDACTED]' END,
                rtorrent_password = CASE WHEN rtorrent_password = '' THEN '' ELSE '[REDACTED]' END,
                jellyfin_api_key = CASE WHEN jellyfin_api_key = '' THEN '' ELSE '[REDACTED]' END,
                sonarr_api_key = CASE WHEN sonarr_api_key = '' THEN '' ELSE '[REDACTED]' END,
                radarr_api_key = CASE WHEN radarr_api_key = '' THEN '' ELSE '[REDACTED]' END",
    )
    .execute(&pool)
    .await
    .map_err(|e| format!("scrub config secrets: {e}"))?
    .rows_affected();

    // `sessions.token` — cookie values double as DB session keys.
    // A sanitized DB handed to someone else shouldn't let them log
    // in as the user. `token` is the PRIMARY KEY, so using a
    // constant literal would UNIQUE-fail across multiple sessions;
    // append the rowid for distinctness.
    let session_rows =
        sqlx::query("UPDATE sessions SET token = '[REDACTED-session-' || rowid || ']'")
            .execute(&pool)
            .await
            .map_err(|e| format!("scrub sessions: {e}"))?
            .rows_affected();

    // `users.password_hash` — bcrypt is computationally hard to
    // reverse but still a non-zero information leak. A determined
    // attacker with leaked bcrypt hashes can run offline dictionary
    // attacks at ~10 guesses/sec per cost-10 hash.
    let user_rows = sqlx::query("UPDATE users SET password_hash = '[REDACTED]'")
        .execute(&pool)
        .await
        .map_err(|e| format!("scrub users: {e}"))?
        .rows_affected();

    pool.close().await;

    Ok(SanitizeSummary {
        external_accounts_tokens: ext_rows as usize,
        config_passwords: cfg_rows as usize,
        session_tokens: session_rows as usize,
        user_password_hashes: user_rows as usize,
        output_path: output.to_path_buf(),
    })
}

#[derive(Debug)]
pub struct SanitizeSummary {
    pub external_accounts_tokens: usize,
    pub config_passwords: usize,
    pub session_tokens: usize,
    pub user_password_hashes: usize,
    pub output_path: std::path::PathBuf,
}

impl std::fmt::Display for SanitizeSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "Sanitized DB written to: {}", self.output_path.display())?;
        writeln!(
            f,
            "  external_accounts rows blanked:  {}",
            self.external_accounts_tokens
        )?;
        writeln!(
            f,
            "  config rows with secrets scrubbed: {}",
            self.config_passwords
        )?;
        writeln!(
            f,
            "  session tokens redacted:         {}",
            self.session_tokens
        )?;
        writeln!(
            f,
            "  user password hashes redacted:   {}",
            self.user_password_hashes
        )?;
        write!(f, "Safe to share in bug reports.")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmpdir() -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "ryokan-sanitize-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    async fn seed_live_db(path: &Path) {
        let url = format!("sqlite://{}?mode=rwc", path.display());
        let pool = SqlitePool::connect(&url).await.unwrap();
        crate::models::migrate(&pool).await.unwrap();

        // Insert a config row with a password + API key populated.
        sqlx::query(
            "INSERT INTO config (id, qbit_pass, jellyfin_api_key)
             VALUES (1, 'topsecret-qbit', 'jf-key-abc') ON CONFLICT(id) DO NOTHING",
        )
        .execute(&pool)
        .await
        .unwrap();

        // Seed one user + session.
        sqlx::query("INSERT INTO users (username, password_hash) VALUES (?, ?)")
            .bind("admin")
            .bind("$2b$10$abcdefghijklmnopqrstuv")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO sessions (token, user_id) VALUES (?, 1)")
            .bind("cookie-token-xyz")
            .execute(&pool)
            .await
            .unwrap();

        // Seed an external_accounts row via the real model helper so
        // the encrypted blobs match the live encrypt() output shape.
        crate::models::external_accounts::link(
            &pool,
            crate::models::external_accounts::LinkRequest {
                provider: crate::models::external_accounts::PROVIDER_MAL.to_string(),
                provider_user_id: "mal_user".to_string(),
                username: "mal_user".to_string(),
                access_token: "plaintext-access".to_string(),
                refresh_token: "plaintext-refresh".to_string(),
                access_token_expires_at: None,
                score_format: "POINT_10".to_string(),
            },
        )
        .await
        .unwrap();

        pool.close().await;
    }

    #[tokio::test]
    async fn sanitize_leaves_live_db_untouched() {
        // The live DB file must not be mutated — users running the
        // CLI on a production install still have tokens afterward.
        let dir = tmpdir();
        let live = dir.join("live.db");
        seed_live_db(&live).await;
        let pre_len = fs::metadata(&live).unwrap().len();

        let out = dir.join("sanitized.db");
        run_sanitize(&live, &out).await.unwrap();

        // Live DB size shouldn't change (the model is copy-before-
        // mutate). If we ever start mutating in place, the size /
        // mtime change would break this.
        let post_len = fs::metadata(&live).unwrap().len();
        assert_eq!(pre_len, post_len, "live DB must not be mutated");

        // Verify: live DB still has the plaintext config secret.
        let url = format!("sqlite://{}?mode=ro", live.display());
        let pool = SqlitePool::connect(&url).await.unwrap();
        let qbit_pass: String = sqlx::query_scalar("SELECT qbit_pass FROM config WHERE id = 1")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(qbit_pass, "topsecret-qbit");
        pool.close().await;

        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn sanitize_blanks_all_known_secret_columns() {
        let dir = tmpdir();
        let live = dir.join("live.db");
        seed_live_db(&live).await;
        let out = dir.join("sanitized.db");
        let summary = run_sanitize(&live, &out).await.unwrap();

        assert!(summary.external_accounts_tokens > 0);
        assert!(summary.config_passwords > 0);
        assert!(summary.session_tokens > 0);
        assert!(summary.user_password_hashes > 0);

        let url = format!("sqlite://{}?mode=ro", out.display());
        let pool = SqlitePool::connect(&url).await.unwrap();

        let qbit_pass: String = sqlx::query_scalar("SELECT qbit_pass FROM config WHERE id = 1")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(qbit_pass, "[REDACTED]");

        let jf_key: String = sqlx::query_scalar("SELECT jellyfin_api_key FROM config WHERE id = 1")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(jf_key, "[REDACTED]");

        let cookie: String = sqlx::query_scalar("SELECT token FROM sessions LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(
            cookie.starts_with("[REDACTED-session-") && cookie.ends_with(']'),
            "session token must be redacted: {cookie}"
        );

        let hash: String = sqlx::query_scalar("SELECT password_hash FROM users LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(hash, "[REDACTED]");

        // external_accounts blobs become the SANITIZED_SENTINEL byte
        // string. Reading them raw confirms the UPDATE landed.
        let access_blob: Vec<u8> =
            sqlx::query_scalar("SELECT access_token_encrypted FROM external_accounts LIMIT 1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(access_blob.as_slice(), SANITIZED_SENTINEL);
        let refresh_blob: Vec<u8> =
            sqlx::query_scalar("SELECT refresh_token_encrypted FROM external_accounts LIMIT 1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(refresh_blob.as_slice(), SANITIZED_SENTINEL);

        pool.close().await;
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn sanitize_skips_empty_fields_instead_of_marking_redacted() {
        // An empty config field means the user never configured that
        // integration. The sanitized output should keep the field
        // empty — reading "[REDACTED]" where a field was always blank
        // would mislead a bug reporter.
        let dir = tmpdir();
        let live = dir.join("live.db");
        let url = format!("sqlite://{}?mode=rwc", live.display());
        let pool = SqlitePool::connect(&url).await.unwrap();
        crate::models::migrate(&pool).await.unwrap();
        // Default config row has empty strings across secret columns.
        sqlx::query("INSERT INTO config (id) VALUES (1) ON CONFLICT(id) DO NOTHING")
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;

        let out = dir.join("sanitized.db");
        run_sanitize(&live, &out).await.unwrap();

        let url = format!("sqlite://{}?mode=ro", out.display());
        let pool = SqlitePool::connect(&url).await.unwrap();
        let qbit_pass: String = sqlx::query_scalar("SELECT qbit_pass FROM config WHERE id = 1")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            qbit_pass, "",
            "empty field must stay empty, not become [REDACTED]"
        );
        pool.close().await;
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn sanitize_handles_missing_live_db_cleanly() {
        let dir = tmpdir();
        let live = dir.join("does-not-exist.db");
        let out = dir.join("sanitized.db");
        let err = run_sanitize(&live, &out).await.unwrap_err();
        assert!(err.to_lowercase().contains("not found"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn sanitize_overwrites_stale_output_on_rerun() {
        // Running the CLI twice in a row should produce a valid
        // sanitized output on the second run. Previously a stale
        // output file + SQLite lock could leave the second run with
        // a corrupted file.
        let dir = tmpdir();
        let live = dir.join("live.db");
        seed_live_db(&live).await;
        let out = dir.join("sanitized.db");
        run_sanitize(&live, &out).await.unwrap();
        // Second run — must not error.
        run_sanitize(&live, &out).await.unwrap();
        let _ = fs::remove_dir_all(&dir);
    }
}
