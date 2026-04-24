//! AL / MAL external-account linkage (issue #62 PR A).
//!
//! Persists OAuth credentials + per-list import preferences for the
//! user's linked AniList or MyAnimeList account. Tokens are
//! AEAD-encrypted at rest via [`crate::services::crypto`]; plaintext
//! exists only briefly in memory, in the narrow window between
//! decryption (when building the outbound API call) and the request
//! actually firing.
//!
//! Plan decision #10: Ryokan supports at most one linked account at
//! a time. The schema has `UNIQUE(provider)` so AL and MAL can't
//! coexist as separate rows for the same provider, but the
//! "one-total" invariant lives in `link` — it rejects a second
//! provider when an account is already linked. Callers that want to
//! switch providers must `unlink` first.
//!
//! The model layer deliberately works in plaintext token strings at
//! its boundary. Callers (OAuth handlers, sync task) pass / receive
//! raw tokens; the encrypt/decrypt round-trip happens inside this
//! module so the call sites can't forget.

use sqlx::SqlitePool;

use crate::services::crypto;

pub const PROVIDER_ANILIST: &str = "anilist";
pub const PROVIDER_MAL: &str = "mal";

/// A linked external account, with tokens decrypted to plaintext for
/// outbound use. The DB-at-rest shape is always encrypted; this type
/// represents the in-memory post-decrypt view the sync task sees.
#[derive(Debug, Clone)]
pub struct ExternalAccount {
    pub id: i64,
    pub provider: String,
    pub provider_user_id: String,
    pub username: String,
    pub access_token: String,
    /// Empty for AL (implicit grant has no refresh token); populated
    /// for MAL.
    pub refresh_token: String,
    pub access_token_expires_at: Option<i64>,
    pub score_format: String,
    pub list_last_synced_at: Option<i64>,
    pub list_full_resync_at: Option<i64>,
    pub linked_at: i64,
    pub import_watching: bool,
    pub import_planning: bool,
    pub import_paused: bool,
    pub import_dropped: bool,
    pub import_completed: bool,
    pub skip_already_watched: bool,
}

/// Input for [`link`] — the OAuth handler populates one of these
/// after a successful token exchange + Viewer fetch and hands it to
/// the model layer to persist.
#[derive(Debug, Clone)]
pub struct LinkRequest {
    pub provider: String,
    pub provider_user_id: String,
    pub username: String,
    pub access_token: String,
    pub refresh_token: String,
    pub access_token_expires_at: Option<i64>,
    pub score_format: String,
}

/// Return the currently-linked account, or None if nothing is linked.
/// Decodes tokens with `services::crypto` — a decryption failure
/// (tampered blob, key rotation without a migration) surfaces as
/// `Err` rather than a silent None so the UI can report "re-link
/// required" instead of "not linked."
pub async fn get_current(db: &SqlitePool) -> Result<Option<ExternalAccount>, String> {
    let row: Option<ExternalAccountRaw> = sqlx::query_as::<_, ExternalAccountRaw>(
        "SELECT id, provider, provider_user_id, username,
                access_token_encrypted, refresh_token_encrypted,
                access_token_expires_at, score_format,
                list_last_synced_at, list_full_resync_at, linked_at,
                import_watching, import_planning, import_paused,
                import_dropped, import_completed, skip_already_watched
           FROM external_accounts
          LIMIT 1",
    )
    .fetch_optional(db)
    .await
    .map_err(|e| format!("external_accounts query failed: {e}"))?;
    row.map(ExternalAccountRaw::into_plaintext).transpose()
}

/// Link a new external account. Rejects when any account is already
/// linked (one-at-a-time invariant, decision #10) — callers must
/// `unlink` first to switch providers.
///
/// Re-link of the same provider is detected by `provider_user_id`
/// matching an existing row with the same provider; tokens +
/// score_format + username are updated in place instead of inserting
/// a duplicate, per decision #8. The returned id is the existing
/// row's, so callers can treat `link` as idempotent against the same
/// provider user.
pub async fn link(db: &SqlitePool, req: LinkRequest) -> Result<i64, String> {
    let now = current_unix_ts();

    // Re-link detection first. If a row exists for this (provider,
    // user_id) pair, refresh its tokens + metadata rather than
    // trying to insert another and hitting the UNIQUE constraint.
    if let Some(existing_id) = find_existing_id(db, &req.provider, &req.provider_user_id).await? {
        let access_encrypted = crypto::encrypt(req.access_token.as_bytes())
            .map_err(|e| format!("encrypt access token: {e}"))?;
        let refresh_encrypted = crypto::encrypt(req.refresh_token.as_bytes())
            .map_err(|e| format!("encrypt refresh token: {e}"))?;
        sqlx::query(
            "UPDATE external_accounts
                SET access_token_encrypted = ?,
                    refresh_token_encrypted = ?,
                    access_token_expires_at = ?,
                    score_format = ?,
                    username = ?
              WHERE id = ?",
        )
        .bind(access_encrypted)
        .bind(refresh_encrypted)
        .bind(req.access_token_expires_at)
        .bind(&req.score_format)
        .bind(&req.username)
        .bind(existing_id)
        .execute(db)
        .await
        .map_err(|e| format!("re-link UPDATE failed: {e}"))?;
        return Ok(existing_id);
    }

    // Enforce one-account-at-a-time. The schema's UNIQUE(provider)
    // would already reject a same-provider duplicate; this rejects a
    // different-provider second link too.
    let any_linked: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM external_accounts WHERE provider != ?")
            .bind(&req.provider)
            .fetch_one(db)
            .await
            .map_err(|e| format!("external_accounts count: {e}"))?;
    if any_linked > 0 {
        return Err(
            "Another external account is already linked; unlink it first before switching providers."
                .into(),
        );
    }

    let access_encrypted = crypto::encrypt(req.access_token.as_bytes())
        .map_err(|e| format!("encrypt access token: {e}"))?;
    let refresh_encrypted = crypto::encrypt(req.refresh_token.as_bytes())
        .map_err(|e| format!("encrypt refresh token: {e}"))?;

    let id: i64 = sqlx::query_scalar(
        "INSERT INTO external_accounts
            (provider, provider_user_id, username,
             access_token_encrypted, refresh_token_encrypted,
             access_token_expires_at, score_format, linked_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)
         RETURNING id",
    )
    .bind(&req.provider)
    .bind(&req.provider_user_id)
    .bind(&req.username)
    .bind(access_encrypted)
    .bind(refresh_encrypted)
    .bind(req.access_token_expires_at)
    .bind(&req.score_format)
    .bind(now)
    .fetch_one(db)
    .await
    .map_err(|e| format!("external_accounts INSERT failed: {e}"))?;

    Ok(id)
}

/// Remove the linked account. Per decision #8, preserves any
/// imported series rows — this call only drops the `external_accounts`
/// row. Callers that want to clear `series.user_score` / custom-list
/// side tables invoke those model functions separately; keeping the
/// concerns split so the unlink path can be composed from the UI
/// side without this module growing a grab-bag of cleanup args.
pub async fn unlink(db: &SqlitePool, id: i64) -> Result<(), String> {
    sqlx::query("DELETE FROM external_accounts WHERE id = ?")
        .bind(id)
        .execute(db)
        .await
        .map_err(|e| format!("external_accounts DELETE failed: {e}"))?;
    Ok(())
}

/// Update the plaintext tokens on an existing row (post-refresh).
/// Encrypts on the way in so the caller keeps working in plaintext.
pub async fn update_tokens(
    db: &SqlitePool,
    id: i64,
    access_token: &str,
    refresh_token: &str,
    expires_at: Option<i64>,
) -> Result<(), String> {
    let access_encrypted =
        crypto::encrypt(access_token.as_bytes()).map_err(|e| format!("encrypt access: {e}"))?;
    let refresh_encrypted =
        crypto::encrypt(refresh_token.as_bytes()).map_err(|e| format!("encrypt refresh: {e}"))?;
    sqlx::query(
        "UPDATE external_accounts
            SET access_token_encrypted = ?,
                refresh_token_encrypted = ?,
                access_token_expires_at = ?
          WHERE id = ?",
    )
    .bind(access_encrypted)
    .bind(refresh_encrypted)
    .bind(expires_at)
    .bind(id)
    .execute(db)
    .await
    .map_err(|e| format!("update_tokens UPDATE failed: {e}"))?;
    Ok(())
}

/// Per-list import preferences, used by the Settings UI and the sync
/// task. Each field maps 1:1 to a checkbox on the External Accounts
/// card. Passed in bulk so a partial UI update doesn't produce
/// arbitrary combinations of "old checkbox on / new checkbox off."
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImportPreferences {
    pub import_watching: bool,
    pub import_planning: bool,
    pub import_paused: bool,
    pub import_dropped: bool,
    pub import_completed: bool,
    pub skip_already_watched: bool,
}

pub async fn update_preferences(
    db: &SqlitePool,
    id: i64,
    prefs: ImportPreferences,
) -> Result<(), String> {
    sqlx::query(
        "UPDATE external_accounts
            SET import_watching = ?,
                import_planning = ?,
                import_paused = ?,
                import_dropped = ?,
                import_completed = ?,
                skip_already_watched = ?
          WHERE id = ?",
    )
    .bind(prefs.import_watching)
    .bind(prefs.import_planning)
    .bind(prefs.import_paused)
    .bind(prefs.import_dropped)
    .bind(prefs.import_completed)
    .bind(prefs.skip_already_watched)
    .bind(id)
    .execute(db)
    .await
    .map_err(|e| format!("update_preferences UPDATE failed: {e}"))?;
    Ok(())
}

async fn find_existing_id(
    db: &SqlitePool,
    provider: &str,
    provider_user_id: &str,
) -> Result<Option<i64>, String> {
    sqlx::query_scalar(
        "SELECT id FROM external_accounts WHERE provider = ? AND provider_user_id = ? LIMIT 1",
    )
    .bind(provider)
    .bind(provider_user_id)
    .fetch_optional(db)
    .await
    .map_err(|e| format!("external_accounts lookup: {e}"))
}

fn current_unix_ts() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[derive(sqlx::FromRow)]
struct ExternalAccountRaw {
    id: i64,
    provider: String,
    provider_user_id: String,
    username: String,
    access_token_encrypted: Vec<u8>,
    refresh_token_encrypted: Vec<u8>,
    access_token_expires_at: Option<i64>,
    score_format: String,
    list_last_synced_at: Option<i64>,
    list_full_resync_at: Option<i64>,
    linked_at: i64,
    import_watching: bool,
    import_planning: bool,
    import_paused: bool,
    import_dropped: bool,
    import_completed: bool,
    skip_already_watched: bool,
}

impl ExternalAccountRaw {
    fn into_plaintext(self) -> Result<ExternalAccount, String> {
        let access_bytes = crypto::decrypt(&self.access_token_encrypted)
            .map_err(|e| format!("decrypt access token: {e}"))?;
        // Refresh token is allowed to be empty (AL case). Encrypted
        // empty-plaintext is still an AEAD-tagged blob, so decrypt
        // returns `Ok(Vec::new())`. An all-zero blob — from a pre-
        // #62 row that somehow sneaks through, say — would fail the
        // tag check here and surface as "re-link required."
        let refresh_bytes = if self.refresh_token_encrypted.is_empty() {
            Vec::new()
        } else {
            crypto::decrypt(&self.refresh_token_encrypted)
                .map_err(|e| format!("decrypt refresh token: {e}"))?
        };
        let access_token = String::from_utf8(access_bytes)
            .map_err(|e| format!("access token is not UTF-8: {e}"))?;
        let refresh_token = String::from_utf8(refresh_bytes)
            .map_err(|e| format!("refresh token is not UTF-8: {e}"))?;
        Ok(ExternalAccount {
            id: self.id,
            provider: self.provider,
            provider_user_id: self.provider_user_id,
            username: self.username,
            access_token,
            refresh_token,
            access_token_expires_at: self.access_token_expires_at,
            score_format: self.score_format,
            list_last_synced_at: self.list_last_synced_at,
            list_full_resync_at: self.list_full_resync_at,
            linked_at: self.linked_at,
            import_watching: self.import_watching,
            import_planning: self.import_planning,
            import_paused: self.import_paused,
            import_dropped: self.import_dropped,
            import_completed: self.import_completed,
            skip_already_watched: self.skip_already_watched,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::SqlitePool;

    async fn in_memory_pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        crate::models::migrate(&pool).await.unwrap();
        pool
    }

    fn sample_anilist_request() -> LinkRequest {
        LinkRequest {
            provider: PROVIDER_ANILIST.to_string(),
            provider_user_id: "12345".to_string(),
            username: "user123".to_string(),
            access_token: "al-access-token-abc".to_string(),
            refresh_token: String::new(),
            access_token_expires_at: None,
            score_format: "POINT_10".to_string(),
        }
    }

    fn sample_mal_request() -> LinkRequest {
        LinkRequest {
            provider: PROVIDER_MAL.to_string(),
            provider_user_id: "mal_user".to_string(),
            username: "mal_user".to_string(),
            access_token: "mal-access-token-xyz".to_string(),
            refresh_token: "mal-refresh-token-uvw".to_string(),
            access_token_expires_at: Some(1_800_000_000),
            score_format: "POINT_10".to_string(),
        }
    }

    #[tokio::test]
    async fn get_current_returns_none_when_no_account_linked() {
        let db = in_memory_pool().await;
        assert!(get_current(&db).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn link_then_get_roundtrips_plaintext_tokens() {
        let db = in_memory_pool().await;
        let id = link(&db, sample_mal_request()).await.unwrap();
        let got = get_current(&db).await.unwrap().expect("linked");
        assert_eq!(got.id, id);
        // The whole point of the crypto layer: plaintext tokens
        // round-trip cleanly, while the DB-at-rest blob is
        // ciphertext (verified by the encryption tests in
        // `services::crypto`).
        assert_eq!(got.access_token, "mal-access-token-xyz");
        assert_eq!(got.refresh_token, "mal-refresh-token-uvw");
        assert_eq!(got.provider, PROVIDER_MAL);
        assert_eq!(got.access_token_expires_at, Some(1_800_000_000));
    }

    #[tokio::test]
    async fn link_second_provider_is_rejected_while_first_exists() {
        // Plan decision #10 — one-at-a-time. A user with AL linked
        // must unlink before linking MAL, no exception. This
        // protects against a settings-UI bug that fires two link
        // calls back-to-back.
        let db = in_memory_pool().await;
        link(&db, sample_anilist_request()).await.unwrap();
        let err = link(&db, sample_mal_request()).await.unwrap_err();
        assert!(
            err.to_lowercase().contains("already linked"),
            "error must call out the one-at-a-time rule: {err}"
        );
    }

    #[tokio::test]
    async fn link_same_provider_and_user_is_idempotent_and_updates_tokens() {
        // Re-link of the same account refreshes tokens on the same
        // row rather than tripping UNIQUE(provider). Matches
        // decision #8's re-link-same-account idempotence.
        let db = in_memory_pool().await;
        let first_id = link(&db, sample_mal_request()).await.unwrap();

        let mut updated = sample_mal_request();
        updated.access_token = "mal-rotated-access".to_string();
        updated.refresh_token = "mal-rotated-refresh".to_string();
        updated.access_token_expires_at = Some(1_900_000_000);
        let second_id = link(&db, updated).await.unwrap();
        assert_eq!(first_id, second_id, "same row re-used on re-link");

        let got = get_current(&db).await.unwrap().expect("still linked");
        assert_eq!(got.access_token, "mal-rotated-access");
        assert_eq!(got.refresh_token, "mal-rotated-refresh");
        assert_eq!(got.access_token_expires_at, Some(1_900_000_000));
    }

    #[tokio::test]
    async fn link_same_provider_different_user_id_is_rejected() {
        // UNIQUE(provider) at the schema level fires here — a user
        // switching MAL accounts can't double-link; they must
        // unlink first. The update path in `link` only triggers
        // when both provider AND user_id match.
        let db = in_memory_pool().await;
        link(&db, sample_mal_request()).await.unwrap();
        let mut other_user = sample_mal_request();
        other_user.provider_user_id = "different_mal_user".to_string();
        let err = link(&db, other_user).await.unwrap_err();
        assert!(
            err.to_lowercase().contains("unique") || err.to_lowercase().contains("constraint"),
            "schema UNIQUE(provider) must surface: {err}"
        );
    }

    #[tokio::test]
    async fn unlink_removes_row_and_leaves_slot_open() {
        let db = in_memory_pool().await;
        let id = link(&db, sample_anilist_request()).await.unwrap();
        unlink(&db, id).await.unwrap();
        assert!(get_current(&db).await.unwrap().is_none());

        // Slot is open — linking MAL now must succeed (one-at-a-time
        // rule is per "currently linked," not per "ever linked").
        link(&db, sample_mal_request()).await.unwrap();
        assert_eq!(
            get_current(&db).await.unwrap().unwrap().provider,
            PROVIDER_MAL
        );
    }

    #[tokio::test]
    async fn update_tokens_encrypts_new_plaintext_and_roundtrips() {
        let db = in_memory_pool().await;
        let id = link(&db, sample_mal_request()).await.unwrap();
        update_tokens(&db, id, "new-access", "new-refresh", Some(2_000_000_000))
            .await
            .unwrap();
        let got = get_current(&db).await.unwrap().unwrap();
        assert_eq!(got.access_token, "new-access");
        assert_eq!(got.refresh_token, "new-refresh");
        assert_eq!(got.access_token_expires_at, Some(2_000_000_000));
    }

    #[tokio::test]
    async fn update_preferences_persists_all_six_flags() {
        let db = in_memory_pool().await;
        let id = link(&db, sample_anilist_request()).await.unwrap();
        let prefs = ImportPreferences {
            import_watching: true,
            import_planning: false,
            import_paused: true,
            import_dropped: true,
            import_completed: true,
            skip_already_watched: true,
        };
        update_preferences(&db, id, prefs).await.unwrap();
        let got = get_current(&db).await.unwrap().unwrap();
        assert!(got.import_watching);
        assert!(!got.import_planning);
        assert!(got.import_paused);
        assert!(got.import_dropped);
        assert!(got.import_completed);
        assert!(got.skip_already_watched);
    }

    #[tokio::test]
    async fn default_preferences_match_the_plan_doc() {
        // Plan decision baseline: Watching + PTW on by default,
        // Paused/Dropped/Completed off, skip-already-watched off.
        // A silent change to the schema defaults would be a
        // user-visible regression at first-link time — pin them.
        let db = in_memory_pool().await;
        link(&db, sample_anilist_request()).await.unwrap();
        let got = get_current(&db).await.unwrap().unwrap();
        assert!(got.import_watching, "Watching default on");
        assert!(got.import_planning, "PTW default on");
        assert!(!got.import_paused, "Paused default off");
        assert!(!got.import_dropped, "Dropped default off");
        assert!(!got.import_completed, "Completed default off");
        assert!(!got.skip_already_watched, "Skip-watched default off");
    }

    #[tokio::test]
    async fn empty_refresh_token_roundtrips_as_empty_string() {
        // AL has no refresh token (implicit grant). Schema stores
        // empty blob; decrypt path short-circuits to empty string.
        let db = in_memory_pool().await;
        link(&db, sample_anilist_request()).await.unwrap();
        let got = get_current(&db).await.unwrap().unwrap();
        assert!(
            got.refresh_token.is_empty(),
            "AL refresh token must roundtrip as empty"
        );
    }
}
