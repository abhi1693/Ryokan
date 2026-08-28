//! In-memory store for manual-import preview sessions (#122).
//!
//! A preview is a multi-step conversation: scan, then the user
//! corrects matches and unticks files, then (in the import step)
//! confirms. The decisions have to live somewhere between requests,
//! and nothing about them belongs in the database, so they sit in
//! `AppState.import_sessions` under an opaque id carried in the URL.
//! Same shape as `interactive_search_cache`: `Arc<Mutex<HashMap>>`,
//! stale entries evicted on the next access, small hard cap so a
//! page-refresh loop can't pile up sessions.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::ImportSession;

pub type ImportSessionStore = Arc<Mutex<HashMap<String, ImportSession>>>;

/// Idle lifetime. A user comparing a big preview against their
/// library can reasonably take a while; two hours is generous without
/// keeping 50k-file sessions around all day.
pub const SESSION_TTL: Duration = Duration::from_secs(2 * 60 * 60);

/// Most sessions kept at once. Ryokan is single-user; a handful covers
/// "I opened the wizard in two tabs".
pub const MAX_SESSIONS: usize = 8;

pub fn new_store() -> ImportSessionStore {
    Arc::new(Mutex::new(HashMap::new()))
}

/// 32 hex chars from 16 random bytes. Same shape as the grab preview id.
pub fn mint_id() -> String {
    let bytes: [u8; 16] = rand::random();
    hex::encode(bytes)
}

/// Session ids are minted here, never user-supplied, so a foreign
/// string in the URL only ever misses the map. Still, reject anything
/// that isn't 32 hex chars before it reaches a lookup or a progress
/// handle.
pub fn is_valid_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

fn evict(map: &mut HashMap<String, ImportSession>) {
    let now = Instant::now();
    map.retain(|_, s| now.duration_since(s.last_touched) < SESSION_TTL);
    while map.len() > MAX_SESSIONS {
        let oldest = map
            .iter()
            .min_by_key(|(_, s)| s.last_touched)
            .map(|(k, _)| k.clone());
        match oldest {
            Some(k) => {
                map.remove(&k);
            }
            None => break,
        }
    }
}

pub fn insert(store: &ImportSessionStore, mut session: ImportSession) {
    let mut map = store.lock().unwrap_or_else(|p| p.into_inner());
    session.last_touched = Instant::now();
    map.insert(session.id.clone(), session);
    evict(&mut map);
}

/// Clone out a session (touching it). Sessions are cloned rather than
/// borrowed so handlers never hold the store lock across an await.
pub fn get(store: &ImportSessionStore, id: &str) -> Option<ImportSession> {
    let mut map = store.lock().unwrap_or_else(|p| p.into_inner());
    evict(&mut map);
    let s = map.get_mut(id)?;
    s.last_touched = Instant::now();
    Some(s.clone())
}

/// Mutate a session in place under the lock. `f` must not block.
pub fn update<R>(
    store: &ImportSessionStore,
    id: &str,
    f: impl FnOnce(&mut ImportSession) -> R,
) -> Option<R> {
    let mut map = store.lock().unwrap_or_else(|p| p.into_inner());
    let s = map.get_mut(id)?;
    s.last_touched = Instant::now();
    Some(f(s))
}

/// Every live session, newest-touched first. The start page lists
/// them so a scan or import left running is reachable again without
/// its URL.
pub fn list(store: &ImportSessionStore) -> Vec<ImportSession> {
    let mut map = store.lock().unwrap_or_else(|p| p.into_inner());
    evict(&mut map);
    let mut all: Vec<ImportSession> = map.values().cloned().collect();
    all.sort_by_key(|s| std::cmp::Reverse(s.last_touched));
    all
}

pub fn remove(store: &ImportSessionStore, id: &str) -> bool {
    let mut map = store.lock().unwrap_or_else(|p| p.into_inner());
    map.remove(id).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::manual_import::{ImportMode, SessionStatus};

    fn blank(id: &str) -> ImportSession {
        ImportSession::new(
            id.to_string(),
            "/tmp/x".into(),
            ImportMode::Hardlink,
            false,
            false,
        )
    }

    #[test]
    fn mint_id_is_32_hex_and_validates() {
        let id = mint_id();
        assert!(is_valid_id(&id), "{id}");
        assert!(!is_valid_id("nope"));
        assert!(!is_valid_id(&"g".repeat(32)));
    }

    #[test]
    fn insert_get_update_remove_roundtrip() {
        let store = new_store();
        insert(&store, blank("a"));
        assert!(get(&store, "a").is_some());
        assert!(get(&store, "b").is_none());
        update(&store, "a", |s| s.status = SessionStatus::Ready);
        assert!(matches!(
            get(&store, "a").unwrap().status,
            SessionStatus::Ready
        ));
        assert!(remove(&store, "a"));
        assert!(!remove(&store, "a"));
    }

    #[test]
    fn list_is_newest_first_and_skips_stale() {
        let store = new_store();
        insert(&store, blank("first"));
        std::thread::sleep(Duration::from_millis(2));
        insert(&store, blank("second"));
        let ids: Vec<String> = list(&store).into_iter().map(|s| s.id).collect();
        assert_eq!(ids, vec!["second".to_string(), "first".to_string()]);
        {
            let mut map = store.lock().unwrap();
            map.get_mut("first").unwrap().last_touched =
                Instant::now() - SESSION_TTL - Duration::from_secs(1);
        }
        let ids: Vec<String> = list(&store).into_iter().map(|s| s.id).collect();
        assert_eq!(ids, vec!["second".to_string()]);
    }

    #[test]
    fn stale_sessions_evicted_on_access_and_cap_drops_oldest() {
        let store = new_store();
        let mut old = blank("old");
        insert(&store, old.clone());
        {
            let mut map = store.lock().unwrap();
            old.last_touched = Instant::now() - SESSION_TTL - Duration::from_secs(1);
            map.insert("old".into(), old);
        }
        assert!(
            get(&store, "old").is_none(),
            "stale session must be evicted"
        );

        for i in 0..(MAX_SESSIONS + 3) {
            insert(&store, blank(&format!("s{i}")));
        }
        let len = store.lock().unwrap().len();
        assert_eq!(len, MAX_SESSIONS);
        assert!(get(&store, "s0").is_none(), "oldest dropped past the cap");
    }
}
