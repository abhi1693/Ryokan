//! Scoped API keys (issue #114).
//!
//! Multiple per-purpose API keys with scope-based access control. Lets
//! users hand out narrow keys to integrations (calendar-only key for
//! an iCal subscriber, search-only key for a Recyclarr-equivalent)
//! without sharing a full admin key.
//!
//! ## Storage
//!
//! `api_keys.key_hash` is the sha256 hex of the plaintext key. The
//! plaintext is shown to the user **exactly once** at creation time
//! and never recoverable from the DB — same UX as GitHub PATs. A DB
//! dump leaks hashes, not keys; an attacker would need to brute-force
//! 32 bytes of CSPRNG to forge a valid request.
//!
//! ## Scopes
//!
//! Five scope strings define the vocabulary up front so it's stable
//! from day one. Only `calendar` is wired in 1.7 (gates the iCal feed
//! at `/api/calendar.ics`); the others are reserved for future
//! consumers (Recyclarr-equivalent, etc.). See [`ALL_SCOPES`].
//!
//! Scope semantics:
//! - `calendar` — read-only access to the iCal feed.
//! - `search` — read-only access to manual search.
//! - `library:read` — read-only access to library state.
//! - `library:write` — full library mutation.
//! - `admin` — all-access; equivalent to holding every other scope.
//!
//! ## Auth model
//!
//! Endpoint wiring is **opt-in default-deny**: only routes explicitly
//! tagged with [`require_scope`](crate::handlers::scoped_auth::require_scope)
//! accept scoped-key auth. Untagged routes stay cookie-only — same
//! shape as today, no behavior change. This deliberately ducks the
//! cookie-OR-key composition problem until we have a dual-mode
//! endpoint to design against.

use sha2::{Digest, Sha256};
use sqlx::{Row, SqlitePool};

/// Every valid scope string. Source-of-truth for both the create-key
/// modal's checkbox list and the middleware's grant check. Adding a
/// new scope means adding it here, in the create-key UI's checkbox
/// row, and in `LogCategory::Auth`'s display copy if it warrants
/// special-case logging.
pub const ALL_SCOPES: &[&str] = &[
    "calendar",
    "search",
    "library:read",
    "library:write",
    "admin",
];

/// In-memory shape for an `api_keys` row. The plaintext key is NOT a
/// field here — the caller of [`create`] gets it as the second tuple
/// element exactly once and is responsible for showing it to the
/// user; subsequent reads of an [`ApiKey`] row from the DB never
/// surface the plaintext.
#[derive(Debug, Clone)]
pub struct ApiKey {
    pub id: i64,
    pub name: String,
    pub scopes: Vec<String>,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
    pub enabled: bool,
}

impl ApiKey {
    /// `true` when this row's scope set grants access to `required`.
    /// `admin` is the universal-grant scope; otherwise an exact match
    /// against the row's scope list. Scope strings are compared
    /// case-sensitively because they're internal vocabulary
    /// ([`ALL_SCOPES`]) — case-insensitive compare would imply the
    /// user-facing UI accepts capitalized variants, which it doesn't.
    pub fn grants(&self, required: &str) -> bool {
        if !self.enabled {
            return false;
        }
        self.scopes.iter().any(|s| s == "admin" || s == required)
    }
}

/// Generate a 32-byte CSPRNG plaintext key as a 64-char hex string.
/// Same shape as [`session::generate_token`](super::session) — the
/// security properties are the same (32 bytes is well above the
/// 16-byte threshold for "computationally infeasible to brute-force"
/// against any realistic attacker model).
pub fn generate_key() -> String {
    let bytes: [u8; 32] = rand::random();
    hex::encode(bytes)
}

/// Hash a plaintext key into the form stored in `api_keys.key_hash`.
/// sha256-hex by convention; 32 bytes of CSPRNG input means the hash
/// space is unique-by-construction (collision probability negligible)
/// and an unsalted hash is fine — there's no rainbow table for
/// 32-byte uniformly-random input.
pub fn hash_key(plaintext: &str) -> String {
    hex::encode(Sha256::digest(plaintext.as_bytes()))
}

/// Insert a new API key with the given name + scopes. Returns
/// `(row_id, plaintext_key)`. The plaintext is the only place this
/// value is ever surfaced; the caller is expected to render it to
/// the user immediately. Duplicate-name is permitted (users may
/// reasonably want two "iCal subscriber" keys for redundancy).
///
/// Scope strings are validated against [`ALL_SCOPES`]; an unknown
/// scope returns an error rather than silently storing it (a typo
/// in the create-key form would otherwise produce a key that grants
/// nothing and is hard to debug).
pub async fn create(
    db: &SqlitePool,
    name: &str,
    scopes: &[String],
) -> Result<(i64, String), String> {
    let trimmed_name = name.trim();
    if trimmed_name.is_empty() {
        return Err("Key name cannot be empty".to_string());
    }
    for scope in scopes {
        if !ALL_SCOPES.contains(&scope.as_str()) {
            return Err(format!("Unknown scope: {scope}"));
        }
    }
    let plaintext = generate_key();
    let key_hash = hash_key(&plaintext);
    let scopes_json =
        serde_json::to_string(scopes).map_err(|e| format!("Failed to serialize scopes: {e}"))?;
    let row: (i64,) = sqlx::query_as(
        "INSERT INTO api_keys (name, key_hash, scopes) VALUES (?, ?, ?) RETURNING id",
    )
    .bind(trimmed_name)
    .bind(&key_hash)
    .bind(&scopes_json)
    .fetch_one(db)
    .await
    .map_err(|e| format!("Failed to create API key: {e}"))?;
    Ok((row.0, plaintext))
}

/// Read every row in the `api_keys` table, sorted newest-first. Used
/// by the Settings → API Keys tab to render the list. Doesn't expose
/// the hash — the model layer knows the column shape but the UI has
/// no use for it.
pub async fn list(db: &SqlitePool) -> Result<Vec<ApiKey>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id, name, scopes, created_at, last_used_at, enabled \
         FROM api_keys \
         ORDER BY created_at DESC",
    )
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(parse_row).collect())
}

/// Look up a key by its plaintext form. Returns `None` for a missing
/// row or a row with `enabled = 0` (disabled keys behave identically
/// to deleted keys from the request path's POV). The hash compare
/// happens via SQL on a UNIQUE-indexed column — no constant-time
/// concern because the hash itself is the unforgeable part: a timing
/// oracle on the lookup leaks at most "this hash exists in the DB,"
/// which is the same answer a successful-vs-failed response
/// already conveys.
pub async fn lookup_by_plaintext(
    db: &SqlitePool,
    plaintext: &str,
) -> Result<Option<ApiKey>, sqlx::Error> {
    let key_hash = hash_key(plaintext);
    let row = sqlx::query(
        "SELECT id, name, scopes, created_at, last_used_at, enabled \
         FROM api_keys \
         WHERE key_hash = ? AND enabled = 1",
    )
    .bind(&key_hash)
    .fetch_optional(db)
    .await?;
    Ok(row.map(parse_row))
}

/// Stamp `last_used_at` after a successful request match. Best-effort:
/// callers ignore failures here so a DB hiccup on the audit-trail
/// side doesn't kill an otherwise-valid request. Called from the
/// `require_scope` middleware after the scope check passes, before
/// running the inner handler.
pub async fn touch_last_used(db: &SqlitePool, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE api_keys SET last_used_at = strftime('%s', 'now') WHERE id = ?")
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

/// Delete a key by id. Idempotent — already-deleted id returns Ok with
/// zero rows affected. Used by the Settings → API Keys delete button.
pub async fn delete(db: &SqlitePool, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM api_keys WHERE id = ?")
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

/// Toggle the `enabled` flag. Used by the Settings → API Keys tab so
/// users can revoke a key without losing its name/scopes/history (re-
/// enabling restores the row immediately). A disabled key still
/// occupies the UNIQUE(key_hash) slot — there's no resurrection-via-
/// re-create concern.
pub async fn set_enabled(db: &SqlitePool, id: i64, enabled: bool) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE api_keys SET enabled = ? WHERE id = ?")
        .bind(if enabled { 1_i64 } else { 0_i64 })
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

/// Common row → struct conversion. JSON parse failure on `scopes`
/// falls back to an empty list rather than dropping the row entirely
/// — a malformed `scopes` column shouldn't make a key disappear from
/// the Settings list, just render with no scope badges so the user
/// can delete it manually.
fn parse_row(row: sqlx::sqlite::SqliteRow) -> ApiKey {
    let scopes_json: String = row.try_get("scopes").unwrap_or_default();
    let scopes: Vec<String> = serde_json::from_str(&scopes_json).unwrap_or_default();
    let enabled: i64 = row.try_get("enabled").unwrap_or(0);
    ApiKey {
        id: row.try_get("id").unwrap_or(0),
        name: row.try_get("name").unwrap_or_default(),
        scopes,
        created_at: row.try_get("created_at").unwrap_or(0),
        last_used_at: row.try_get("last_used_at").ok(),
        enabled: enabled != 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::in_memory_pool;

    #[tokio::test]
    async fn create_returns_plaintext_and_persists_hash() {
        let pool = in_memory_pool().await;
        let (id, plaintext) = create(&pool, "Test", &["calendar".to_string()])
            .await
            .unwrap();
        assert!(id > 0);
        assert_eq!(plaintext.len(), 64, "32-byte hex = 64 chars");
        // Hash matches what we'd recompute from the plaintext.
        let stored: String = sqlx::query_scalar("SELECT key_hash FROM api_keys WHERE id = ?")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(stored, hash_key(&plaintext));
    }

    #[tokio::test]
    async fn create_rejects_unknown_scope() {
        let pool = in_memory_pool().await;
        let err = create(&pool, "Test", &["nonexistent".to_string()])
            .await
            .unwrap_err();
        assert!(err.contains("Unknown scope"));
    }

    #[tokio::test]
    async fn create_rejects_empty_name() {
        let pool = in_memory_pool().await;
        let err = create(&pool, "   ", &["calendar".to_string()])
            .await
            .unwrap_err();
        assert!(err.contains("cannot be empty"));
    }

    #[tokio::test]
    async fn lookup_by_plaintext_finds_enabled_key() {
        let pool = in_memory_pool().await;
        let (_, plaintext) = create(&pool, "Cal", &["calendar".to_string()])
            .await
            .unwrap();
        let found = lookup_by_plaintext(&pool, &plaintext).await.unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().name, "Cal");
    }

    #[tokio::test]
    async fn lookup_skips_disabled_key() {
        // Disabled keys behave identically to deleted keys from the
        // middleware's POV — pin the contract so flipping enabled
        // can't be a silent half-revoke.
        let pool = in_memory_pool().await;
        let (id, plaintext) = create(&pool, "Cal", &["calendar".to_string()])
            .await
            .unwrap();
        set_enabled(&pool, id, false).await.unwrap();
        let found = lookup_by_plaintext(&pool, &plaintext).await.unwrap();
        assert!(found.is_none());
    }

    #[tokio::test]
    async fn lookup_returns_none_for_unknown_plaintext() {
        let pool = in_memory_pool().await;
        let _ = create(&pool, "Cal", &["calendar".to_string()])
            .await
            .unwrap();
        let found = lookup_by_plaintext(&pool, "not-a-real-key").await.unwrap();
        assert!(found.is_none());
    }

    #[tokio::test]
    async fn grants_admin_satisfies_any_scope() {
        // admin is the universal-grant scope. Pin it so a future
        // refactor that special-cases scope strings doesn't quietly
        // break the all-access semantics.
        let key = ApiKey {
            id: 1,
            name: "A".into(),
            scopes: vec!["admin".to_string()],
            created_at: 0,
            last_used_at: None,
            enabled: true,
        };
        assert!(key.grants("calendar"));
        assert!(key.grants("library:write"));
        assert!(key.grants("admin"));
    }

    #[tokio::test]
    async fn grants_exact_match_required_for_non_admin() {
        let key = ApiKey {
            id: 1,
            name: "A".into(),
            scopes: vec!["calendar".to_string()],
            created_at: 0,
            last_used_at: None,
            enabled: true,
        };
        assert!(key.grants("calendar"));
        assert!(!key.grants("search"));
        assert!(!key.grants("library:read"));
    }

    #[tokio::test]
    async fn grants_returns_false_when_disabled() {
        // Defense-in-depth: even though `lookup_by_plaintext` filters
        // disabled rows, callers that hold an `ApiKey` from another
        // path (tests, in-flight reload) should still see the
        // disabled flag honored.
        let key = ApiKey {
            id: 1,
            name: "A".into(),
            scopes: vec!["admin".to_string()],
            created_at: 0,
            last_used_at: None,
            enabled: false,
        };
        assert!(!key.grants("calendar"));
    }

    #[tokio::test]
    async fn touch_last_used_stamps_recent_timestamp() {
        let pool = in_memory_pool().await;
        let (id, _) = create(&pool, "Cal", &["calendar".to_string()])
            .await
            .unwrap();
        // Initially NULL.
        let initial: Option<i64> =
            sqlx::query_scalar("SELECT last_used_at FROM api_keys WHERE id = ?")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(initial.is_none());
        touch_last_used(&pool, id).await.unwrap();
        let after: Option<i64> =
            sqlx::query_scalar("SELECT last_used_at FROM api_keys WHERE id = ?")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(after.is_some());
        let now = chrono::Utc::now().timestamp();
        assert!(
            (now - after.unwrap()).abs() < 5,
            "stamp should be within ~5s of now"
        );
    }

    #[tokio::test]
    async fn delete_removes_row() {
        let pool = in_memory_pool().await;
        let (id, _) = create(&pool, "Cal", &["calendar".to_string()])
            .await
            .unwrap();
        delete(&pool, id).await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM api_keys WHERE id = ?")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn list_returns_newest_first() {
        let pool = in_memory_pool().await;
        let (a, _) = create(&pool, "first", &["calendar".to_string()])
            .await
            .unwrap();
        // Force a measurable gap so ORDER BY created_at DESC
        // produces a stable result on fast SQLite — without the
        // sleep, both rows can land in the same epoch second and
        // the ORDER BY tiebreaker is undefined.
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        let (b, _) = create(&pool, "second", &["search".to_string()])
            .await
            .unwrap();
        let keys = list(&pool).await.unwrap();
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].id, b);
        assert_eq!(keys[1].id, a);
    }

    #[test]
    fn hash_is_deterministic() {
        // Same plaintext → same hash; different plaintext → different
        // hash. Pinned because lookup_by_plaintext relies on this.
        let h1 = hash_key("hello");
        let h2 = hash_key("hello");
        let h3 = hash_key("world");
        assert_eq!(h1, h2);
        assert_ne!(h1, h3);
        assert_eq!(h1.len(), 64, "sha256 hex is 64 chars");
    }

    #[test]
    fn generate_key_produces_distinct_values() {
        // 32 bytes of CSPRNG; collision is astronomically unlikely.
        let a = generate_key();
        let b = generate_key();
        assert_ne!(a, b);
        assert_eq!(a.len(), 64);
    }
}
