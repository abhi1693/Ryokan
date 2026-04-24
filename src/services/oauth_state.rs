//! Short-lived in-memory store for pending OAuth attempts.
//!
//! Issue #62 PR A: MAL's Auth Code + PKCE flow needs the PKCE verifier
//! to survive between `/settings/oauth/mal/start` (generate + store +
//! redirect user to MAL) and `/settings/oauth/mal/submit` (user pastes
//! code → Ryokan exchanges code + verifier for tokens). Between those
//! two calls the verifier lives here.
//!
//! Scope: one pending attempt per provider. A second `/start` call
//! for the same provider overwrites the first — which is intentional:
//! the previous attempt is effectively abandoned the moment the user
//! clicks Link again. 10-minute TTL trims forgotten attempts so the
//! map can't grow unboundedly.
//!
//! In-memory only. Process restart drops everything, which is fine:
//! the OAuth flow end-to-end takes under a minute for an attentive
//! user, and the worst case is "user has to click Link again."
//!
//! AL uses implicit grant and has no per-attempt server-side state
//! to track — the user's browser does the redirect dance and pastes
//! the token at the end. No store entry gets created for AL.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const OAUTH_STATE_TTL: Duration = Duration::from_secs(10 * 60);

/// What survives between `/start` and `/submit` for a single pending
/// attempt. Only `verifier` is load-bearing today; `started_at`
/// powers TTL eviction.
#[derive(Debug, Clone)]
pub struct OAuthAttempt {
    /// PKCE code verifier (random 43-char base64url string). MAL uses
    /// `code_challenge_method = plain`, so the challenge sent to MAL
    /// *is* this verifier. Kept here so the submit path can echo it
    /// back to MAL's token endpoint.
    pub verifier: String,
    pub started_at: Instant,
}

/// Per-provider pending attempt. Key is `"anilist"` / `"mal"`; value
/// is the most recent attempt for that provider.
pub type OAuthStateStore = Arc<Mutex<HashMap<String, OAuthAttempt>>>;

pub fn new() -> OAuthStateStore {
    Arc::new(Mutex::new(HashMap::new()))
}

/// Store a fresh attempt for `provider`, overwriting any previous
/// pending attempt for the same provider.
pub fn stash(store: &OAuthStateStore, provider: &str, verifier: String) {
    let mut guard = store.lock().unwrap_or_else(|p| p.into_inner());
    // Opportunistic eviction on every write — cheap and keeps the map
    // bounded even if `take` never gets called (user closes the tab
    // mid-flow).
    evict_expired(&mut guard);
    guard.insert(
        provider.to_string(),
        OAuthAttempt {
            verifier,
            started_at: Instant::now(),
        },
    );
}

/// Consume the pending attempt for `provider`, returning its verifier.
/// Returns `None` when no attempt exists or the stored one is stale
/// (past [`OAUTH_STATE_TTL`]). Single-use — a second `take` for the
/// same provider returns `None` until another `stash` fires.
pub fn take(store: &OAuthStateStore, provider: &str) -> Option<String> {
    let mut guard = store.lock().unwrap_or_else(|p| p.into_inner());
    let attempt = guard.remove(provider)?;
    if attempt.started_at.elapsed() > OAUTH_STATE_TTL {
        return None;
    }
    Some(attempt.verifier)
}

fn evict_expired(map: &mut HashMap<String, OAuthAttempt>) {
    map.retain(|_, a| a.started_at.elapsed() <= OAUTH_STATE_TTL);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn take_on_empty_store_returns_none() {
        let store = new();
        assert!(take(&store, "mal").is_none());
    }

    #[test]
    fn stash_then_take_returns_verifier_once() {
        let store = new();
        stash(&store, "mal", "abc-verifier".into());
        assert_eq!(take(&store, "mal"), Some("abc-verifier".into()));
        // Single-use: second take returns None. Prevents an attacker
        // who intercepts the code from using a stale verifier that's
        // still sitting in memory — once the legitimate submit
        // consumes it, nobody else can replay against the same slot.
        assert!(take(&store, "mal").is_none());
    }

    #[test]
    fn providers_have_independent_slots() {
        let store = new();
        stash(&store, "anilist", "al-v".into());
        stash(&store, "mal", "mal-v".into());
        assert_eq!(take(&store, "anilist"), Some("al-v".into()));
        // Taking `anilist` must not evict `mal`'s slot.
        assert_eq!(take(&store, "mal"), Some("mal-v".into()));
    }

    #[test]
    fn second_stash_overwrites_first_for_same_provider() {
        // User clicks Link, gets redirected to MAL, changes their
        // mind, closes the tab, clicks Link again. Second /start
        // stash must win — the first verifier is dead the moment
        // MAL's authorize page closes.
        let store = new();
        stash(&store, "mal", "first".into());
        stash(&store, "mal", "second".into());
        assert_eq!(take(&store, "mal"), Some("second".into()));
    }

    #[test]
    fn stale_attempt_is_evicted_on_take() {
        let store = new();
        // Manually insert a stale attempt past the public API — same
        // pattern as the interactive_search_cache tests.
        let stale = OAuthAttempt {
            verifier: "old-verifier".into(),
            started_at: Instant::now() - (OAUTH_STATE_TTL + Duration::from_secs(1)),
        };
        store.lock().unwrap().insert("mal".into(), stale);
        assert!(
            take(&store, "mal").is_none(),
            "past-TTL attempt must not be returned"
        );
    }
}
