//! Outbound notification dispatch (issue #118).
//!
//! Foundation for the per-provider issues (#119 webhook, #120 Discord)
//! and the settings UI (#121). This module ships the trait, the
//! event taxonomy, the cache shape, and the per-provider fan-out
//! dispatcher — but no concrete provider impls. With an empty cache
//! every `dispatch` call is a no-op `tokio::spawn` that exits on the
//! `pool.is_empty()` early-return.
//!
//! ## Storage shape
//!
//! ```text
//! AppState.notification_providers : Arc<RwLock<Arc<Vec<Arc<dyn NotificationProvider>>>>>
//!         └── outer RwLock owns swap (Settings save → rebuild)
//!             └── inner Arc<Vec<_>> cheap-cloned out per dispatch
//!                 └── per-provider Arc<dyn NotificationProvider>
//! ```
//!
//! Mirrors `CompiledCfCache` / `DownloadClientsCache` / `IndexerCache`
//! exactly. The dispatch hot path clones the inner `Arc` once under
//! the read lock and runs lock-free over the snapshot.
//!
//! ## Dispatch is fire-and-forget
//!
//! `dispatch` spawns a task and returns immediately so a hung
//! receiver can't block the user-visible operation that triggered
//! the event. Per-provider `send` is wrapped in
//! `tokio::time::timeout(5s)` so even one wedged Discord webhook
//! can't keep the dispatch task alive forever.
//!
//! ## No persistent retry queue
//!
//! Failed sends log via `LogCategory::Notifications` and drop. A
//! durable queue (dedup, backoff, ordering) is real follow-up work
//! and gets deferred until users report dropped events as a real
//! problem.

use async_trait::async_trait;
use sqlx::SqlitePool;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

pub mod event;
pub mod store;
pub mod webhook;

#[cfg(test)]
mod wiremock_tests;

pub use event::{ALL_EVENT_KINDS, DEFAULT_ON_EVENT_KINDS, NotificationEvent};

use crate::models::log::LogCategory;

/// Per-provider send budget. One slow / hung receiver must not stall
/// the dispatch task indefinitely. 5 s matches the issue spec; the
/// actual receiver-side budget is whatever the provider impl picks
/// for its `reqwest::Client::timeout` — this is an outer ceiling.
const PROVIDER_SEND_TIMEOUT: Duration = Duration::from_secs(5);

/// One configured outbound destination (one webhook URL, one
/// Discord webhook, one future Telegram bot, etc.). Object-safe
/// so `Arc<dyn NotificationProvider>` storage on the cache works.
#[async_trait]
pub trait NotificationProvider: Send + Sync {
    /// Stable id from `notification_providers.id`. Lets the per-event
    /// matrix key by provider without round-tripping through `name`,
    /// and lets the test-send endpoint (`/api/notifications/{id}/test`,
    /// landing in the per-provider issues) target a single provider
    /// from the snapshot.
    fn id(&self) -> i64;

    /// User-given label. Used in log lines + Settings UI.
    fn name(&self) -> &str;

    /// Trait-impl discriminator. `&'static str` so we get it for
    /// log-line tagging at zero cost. Must match the `kind` column
    /// in `notification_providers` so the cache rebuild can pick the
    /// right impl per row.
    fn kind(&self) -> &'static str;

    /// Per-provider send. `Result<_, String>` matches the project
    /// convention; the dispatcher prefix-tags failures into the
    /// `Notifications` log category. Implementations return Err on
    /// transport failures and on receiver-returned errors that are
    /// not 2xx; they should not return Err for "I logged it locally
    /// instead" — that path is for the dispatcher.
    async fn send(&self, event: &NotificationEvent) -> Result<(), String>;
}

/// Same swap-on-write shape as `IndexerCache` / `CompiledCfCache`.
/// Outer `RwLock` owns swap; the inner `Arc<Vec<_>>` is cheap-cloned
/// out under the read lock and walked lock-free.
pub type NotificationProviders = Arc<RwLock<Arc<Vec<Arc<dyn NotificationProvider>>>>>;

/// Empty cache, used at process boot before
/// `rebuild_notification_providers_cache` runs and as the default
/// for tests that don't care about notifications.
pub fn empty_cache() -> NotificationProviders {
    Arc::new(RwLock::new(Arc::new(Vec::new())))
}

/// Production dispatch. Fire-and-forget — spawns a task, returns
/// immediately. The user-visible operation (grab, import, classify)
/// must not block on Discord webhook latency.
///
/// 1. Cheap-clone the inner `Arc<Vec<_>>` under the read lock.
/// 2. Per-provider, look up the per-event opt-in from
///    `notification_settings`. Default-deny on missing rows.
/// 3. Per-provider, call `send` wrapped in
///    `tokio::time::timeout(PROVIDER_SEND_TIMEOUT)`.
/// 4. Per-provider failures (timeout or `Err`) emit a
///    `LogCategory::Notifications` warn row with the provider name
///    + kind + event kind + truncated error.
pub fn dispatch(cache: &NotificationProviders, db: SqlitePool, event: NotificationEvent) {
    let cache = cache.clone();
    tokio::spawn(async move {
        let providers = cache.read().await.clone();
        if providers.is_empty() {
            return;
        }
        // Build per-provider futures concurrently. Each runs through
        // its own matrix-check + send + timeout + logging path so a
        // panic / error on one doesn't poison the others.
        let mut handles = Vec::with_capacity(providers.len());
        for provider in providers.iter().cloned() {
            let db = db.clone();
            let event = event.clone();
            handles.push(tokio::spawn(async move {
                fan_out_one(provider, db, event).await;
            }));
        }
        for h in handles {
            let _ = h.await;
        }
    });
}

/// Single-provider awaited send for the test endpoint
/// (`POST /api/notifications/{id}/test` — landing in the per-provider
/// issues). Bypasses the per-event matrix so testing a `Health`
/// event from the Settings UI fires even when Health is default-off,
/// and returns the provider's `send` result so the caller can render
/// it in the UI.
pub async fn send_to(
    cache: &NotificationProviders,
    provider_id: i64,
    event: NotificationEvent,
) -> Result<(), String> {
    let providers = cache.read().await.clone();
    let provider = providers
        .iter()
        .find(|p| p.id() == provider_id)
        .cloned()
        .ok_or_else(|| format!("notification provider #{provider_id} not in cache"))?;
    match tokio::time::timeout(PROVIDER_SEND_TIMEOUT, provider.send(&event)).await {
        Ok(r) => r,
        Err(_) => Err(format!(
            "notification provider {} ({}) timed out after {}s",
            provider.name(),
            provider.kind(),
            PROVIDER_SEND_TIMEOUT.as_secs(),
        )),
    }
}

async fn fan_out_one(
    provider: Arc<dyn NotificationProvider>,
    db: SqlitePool,
    event: NotificationEvent,
) {
    // Per-event opt-in matrix. Default-deny on missing rows: the
    // settings handler seeds `DEFAULT_ON_EVENT_KINDS` rows at
    // provider creation, so a fresh provider receives the
    // conservative defaults; everything else is explicitly opted
    // in via the Settings UI.
    let matrix = match store::matrix_for_provider(&db, provider.id()).await {
        Ok(m) => m,
        Err(e) => {
            crate::services::logger::warn(
                &db,
                LogCategory::Notifications,
                "matrix lookup failed",
                &format!(
                    "provider={} kind={} err={}",
                    provider.name(),
                    provider.kind(),
                    truncate(&e.to_string(), 200),
                ),
            )
            .await;
            return;
        }
    };
    let event_kind = event.kind();
    if !matrix.get(event_kind).copied().unwrap_or(false) {
        return;
    }

    let send_fut = provider.send(&event);
    match tokio::time::timeout(PROVIDER_SEND_TIMEOUT, send_fut).await {
        Ok(Ok(())) => {
            crate::services::logger::info(
                &db,
                LogCategory::Notifications,
                "sent",
                &format!(
                    "provider={} kind={} event={}",
                    provider.name(),
                    provider.kind(),
                    event_kind,
                ),
            )
            .await;
        }
        Ok(Err(e)) => {
            crate::services::logger::warn(
                &db,
                LogCategory::Notifications,
                "send failed",
                &format!(
                    "provider={} kind={} event={} err={}",
                    provider.name(),
                    provider.kind(),
                    event_kind,
                    truncate(&e, 500),
                ),
            )
            .await;
        }
        Err(_) => {
            crate::services::logger::warn(
                &db,
                LogCategory::Notifications,
                "send timed out",
                &format!(
                    "provider={} kind={} event={} after={}s",
                    provider.name(),
                    provider.kind(),
                    event_kind,
                    PROVIDER_SEND_TIMEOUT.as_secs(),
                ),
            )
            .await;
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// Convenience: build a `Grabbed` event from the call-site context
/// and dispatch it through the cache. Centralizes the field shape so
/// every call site builds the same struct — adding a field on the
/// event later means updating one place + one signature instead of
/// chasing every caller.
///
/// `series_title` is fetched lazily from `series.title_romaji` falling
/// back to `title_native` so callers don't have to `JOIN` it in just
/// for the event. Failure to resolve the title logs at debug and
/// short-circuits the dispatch — the event would render with an
/// empty title which is a worse UX signal than no event at all.
#[allow(clippy::too_many_arguments)]
pub async fn emit_grabbed(
    state: &crate::AppState,
    series_id: i64,
    episode_number: i32,
    release_title: &str,
    indexer: Option<String>,
    score: Option<i32>,
    client_kind: Option<String>,
) {
    let providers = state.notification_providers.read().await.clone();
    if providers.is_empty() {
        return;
    }
    let title: Option<String> = sqlx::query_scalar(
        "SELECT CASE
                  WHEN COALESCE(title_romaji, '') <> '' THEN title_romaji
                  WHEN COALESCE(title_english, '') <> '' THEN title_english
                  WHEN COALESCE(title_native, '') <> '' THEN title_native
                  ELSE title
                END
         FROM series WHERE id = ?",
    )
    .bind(series_id)
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();
    let Some(series_title) = title else {
        tracing::debug!(
            "notifications::emit_grabbed: series #{series_id} not found, skipping dispatch"
        );
        return;
    };
    dispatch(
        &state.notification_providers,
        state.db.clone(),
        NotificationEvent::Grabbed {
            series_id,
            series_title,
            episode_number,
            release_title: release_title.to_string(),
            indexer,
            score,
            client_kind,
        },
    );
}

/// Convenience: dispatch an `ExternalSyncReLinkRequired` event for a
/// given provider string (`"anilist"` / `"mal"`). Fired at the same
/// point the sticky `last_sync_auth_failed` flag is flipped on.
pub fn emit_external_sync_relink_required(state: &crate::AppState, provider: &str) {
    dispatch(
        &state.notification_providers,
        state.db.clone(),
        NotificationEvent::ExternalSyncReLinkRequired {
            provider: provider.to_string(),
        },
    );
}

/// Atomically swap in a fresh `Vec<Arc<dyn NotificationProvider>>`
/// built from every enabled row in `notification_providers`. Called
/// at startup (after `migrations::migrate`) and from the Settings
/// handler that mutates the table.
///
/// Until provider impls land in the per-provider issues, this just
/// loads the rows and logs an unknown-kind warning for each one
/// before installing an empty snapshot. The shape is in place so
/// follow-up PRs only need to add a per-kind constructor arm here.
pub async fn rebuild_notification_providers_cache(cache: &NotificationProviders, db: &SqlitePool) {
    let rows = match store::list_enabled(db).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("notification_providers: failed to load from DB: {e}");
            Vec::new()
        }
    };
    // Per-kind constructor dispatch. New `kind` strings land here
    // alongside their per-impl module — `webhook` is #119, `discord`
    // is #120. A row with an unrecognized kind is logged and dropped
    // so a hand-edited DB / pre-provider Settings save surfaces in
    // System → Logs rather than silently swallowing.
    let mut providers: Vec<Arc<dyn NotificationProvider>> = Vec::new();
    for row in rows {
        let built: Option<Arc<dyn NotificationProvider>> = match row.kind.as_str() {
            "webhook" => {
                match webhook::WebhookProvider::from_row(row.id, row.name.clone(), &row.config_json)
                {
                    Ok(p) => Some(Arc::new(p)),
                    Err(e) => {
                        tracing::warn!(
                            "notification_providers: skipping webhook #{} ({}): {}",
                            row.id,
                            row.name,
                            e,
                        );
                        None
                    }
                }
            }
            other => {
                tracing::warn!(
                    "notification_providers: skipping #{} ({}) — unknown kind {:?}",
                    row.id,
                    row.name,
                    other,
                );
                None
            }
        };
        if let Some(p) = built {
            providers.push(p);
        }
    }
    *cache.write().await = Arc::new(providers);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::in_memory_pool;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Mock provider that records every event it received. Used to
    /// cover dispatch concurrency, isolation, and the per-event
    /// matrix.
    struct RecordingProvider {
        id: i64,
        name: String,
        sent: Arc<AtomicUsize>,
        behavior: Behavior,
    }

    enum Behavior {
        Ok,
        Err(String),
        Hang,
    }

    #[async_trait]
    impl NotificationProvider for RecordingProvider {
        fn id(&self) -> i64 {
            self.id
        }
        fn name(&self) -> &str {
            &self.name
        }
        fn kind(&self) -> &'static str {
            "test"
        }
        async fn send(&self, _event: &NotificationEvent) -> Result<(), String> {
            self.sent.fetch_add(1, Ordering::SeqCst);
            match &self.behavior {
                Behavior::Ok => Ok(()),
                Behavior::Err(e) => Err(e.clone()),
                Behavior::Hang => {
                    // Hang past the dispatcher's per-provider timeout.
                    tokio::time::sleep(PROVIDER_SEND_TIMEOUT * 3).await;
                    Ok(())
                }
            }
        }
    }

    async fn build_provider(db: &SqlitePool, id: i64, name: &str, seed_defaults: bool) -> i64 {
        let row: (i64,) = sqlx::query_as(
            "INSERT INTO notification_providers (id, name, kind, enabled, config_json)
             VALUES (?, ?, 'test', 1, '{}') RETURNING id",
        )
        .bind(id)
        .bind(name)
        .fetch_one(db)
        .await
        .unwrap();
        if seed_defaults {
            store::seed_default_matrix(db, row.0).await.unwrap();
        }
        row.0
    }

    fn cache_with(providers: Vec<Arc<dyn NotificationProvider>>) -> NotificationProviders {
        Arc::new(RwLock::new(Arc::new(providers)))
    }

    fn grabbed() -> NotificationEvent {
        NotificationEvent::Grabbed {
            series_id: 1,
            series_title: "Test".into(),
            episode_number: 7,
            release_title: "Test - 07".into(),
            indexer: None,
            score: None,
            client_kind: None,
        }
    }

    #[tokio::test]
    async fn dispatch_fans_out_to_every_opted_in_provider() {
        let db = in_memory_pool().await;
        let id_a = build_provider(&db, 1, "a", true).await;
        let id_b = build_provider(&db, 2, "b", true).await;
        let sent_a = Arc::new(AtomicUsize::new(0));
        let sent_b = Arc::new(AtomicUsize::new(0));
        let cache = cache_with(vec![
            Arc::new(RecordingProvider {
                id: id_a,
                name: "a".into(),
                sent: sent_a.clone(),
                behavior: Behavior::Ok,
            }),
            Arc::new(RecordingProvider {
                id: id_b,
                name: "b".into(),
                sent: sent_b.clone(),
                behavior: Behavior::Ok,
            }),
        ]);

        dispatch(&cache, db, grabbed());
        // Allow the spawned dispatch task to run.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(sent_a.load(Ordering::SeqCst), 1);
        assert_eq!(sent_b.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn dispatch_isolates_per_provider_failures() {
        // Provider A returns Err, provider B must still receive the
        // event. Per-provider failure isolation is the core invariant
        // that prevents a single bad webhook from blackholing every
        // other receiver.
        let db = in_memory_pool().await;
        let id_a = build_provider(&db, 1, "a", true).await;
        let id_b = build_provider(&db, 2, "b", true).await;
        let sent_a = Arc::new(AtomicUsize::new(0));
        let sent_b = Arc::new(AtomicUsize::new(0));
        let cache = cache_with(vec![
            Arc::new(RecordingProvider {
                id: id_a,
                name: "a".into(),
                sent: sent_a.clone(),
                behavior: Behavior::Err("nope".into()),
            }),
            Arc::new(RecordingProvider {
                id: id_b,
                name: "b".into(),
                sent: sent_b.clone(),
                behavior: Behavior::Ok,
            }),
        ]);

        dispatch(&cache, db, grabbed());
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(sent_a.load(Ordering::SeqCst), 1);
        assert_eq!(sent_b.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn matrix_default_deny_skips_provider_without_rows() {
        // No matrix rows seeded — provider must be skipped, not
        // default-on'd. Pinned because a regression that flipped
        // default to "on" would suddenly fan out every event to
        // every provider on a fresh schema, defeating the
        // per-event matrix entirely.
        let db = in_memory_pool().await;
        let id = build_provider(&db, 1, "a", false).await;
        let sent = Arc::new(AtomicUsize::new(0));
        let cache = cache_with(vec![Arc::new(RecordingProvider {
            id,
            name: "a".into(),
            sent: sent.clone(),
            behavior: Behavior::Ok,
        })]);
        dispatch(&cache, db, grabbed());
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(sent.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn matrix_skips_opted_out_event_kinds() {
        let db = in_memory_pool().await;
        let id = build_provider(&db, 1, "a", true).await;
        // Default-on includes Grabbed; flip it off to assert opt-out.
        sqlx::query(
            "UPDATE notification_settings SET enabled = 0
             WHERE provider_id = ? AND event_kind = 'Grabbed'",
        )
        .bind(id)
        .execute(&db)
        .await
        .unwrap();
        let sent = Arc::new(AtomicUsize::new(0));
        let cache = cache_with(vec![Arc::new(RecordingProvider {
            id,
            name: "a".into(),
            sent: sent.clone(),
            behavior: Behavior::Ok,
        })]);
        dispatch(&cache, db, grabbed());
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(sent.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn dispatch_does_not_block_on_a_hung_provider() {
        // The hung provider's send sleeps past `PROVIDER_SEND_TIMEOUT`.
        // The healthy provider must still receive the event well
        // within that window — pinned at "the dispatch task scheduled
        // both before sleeping," not "the dispatch task blocked
        // serially on the hung send." The fan-out shape uses a per-
        // provider tokio::spawn so this is a reachable state.
        let db = in_memory_pool().await;
        let id_hang = build_provider(&db, 1, "hung", true).await;
        let id_ok = build_provider(&db, 2, "ok", true).await;
        let sent_ok = Arc::new(AtomicUsize::new(0));
        let cache = cache_with(vec![
            Arc::new(RecordingProvider {
                id: id_hang,
                name: "hung".into(),
                sent: Arc::new(AtomicUsize::new(0)),
                behavior: Behavior::Hang,
            }),
            Arc::new(RecordingProvider {
                id: id_ok,
                name: "ok".into(),
                sent: sent_ok.clone(),
                behavior: Behavior::Ok,
            }),
        ]);
        dispatch(&cache, db, grabbed());
        // Healthy provider must complete well before the hung
        // provider's timeout (5s).
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            sent_ok.load(Ordering::SeqCst),
            1,
            "healthy provider must not be blocked by the hung one"
        );
    }

    #[tokio::test]
    async fn dispatch_with_empty_cache_is_a_no_op() {
        let db = in_memory_pool().await;
        let cache = empty_cache();
        // Just must not panic / hang. No assertion necessary beyond
        // "this returns within the test budget."
        dispatch(&cache, db, grabbed());
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    #[tokio::test]
    async fn send_to_bypasses_matrix() {
        // Test endpoint should fire even when the event_kind is
        // default-off. Pinned because the Settings UI surfaces this
        // as a "Send test" button — defaulting it to a Health event
        // (default-off) and silently no-op'ing through the matrix
        // would misrepresent the receiver as broken.
        let db = in_memory_pool().await;
        let id = build_provider(&db, 1, "a", false).await;
        let sent = Arc::new(AtomicUsize::new(0));
        let cache = cache_with(vec![Arc::new(RecordingProvider {
            id,
            name: "a".into(),
            sent: sent.clone(),
            behavior: Behavior::Ok,
        })]);
        send_to(
            &cache,
            id,
            NotificationEvent::Health {
                kind: "test".into(),
                message: "hello".into(),
            },
        )
        .await
        .expect("send_to ok");
        assert_eq!(sent.load(Ordering::SeqCst), 1);
        let _ = db;
    }

    #[tokio::test]
    async fn send_to_unknown_provider_returns_err() {
        let cache = empty_cache();
        let res = send_to(
            &cache,
            999,
            NotificationEvent::Health {
                kind: "test".into(),
                message: "x".into(),
            },
        )
        .await;
        assert!(res.is_err());
    }

    #[test]
    fn truncate_handles_unicode_grapheme_count() {
        // The `Notifications` log lines pass receiver error bodies
        // through `truncate`. A naive byte-slice would panic on a
        // multi-byte UTF-8 boundary; the chars-based form must hold.
        let long = "あいうえお".repeat(100);
        let out = truncate(&long, 5);
        assert!(out.ends_with('…'));
        assert_eq!(out.chars().count(), 6);
    }
}
