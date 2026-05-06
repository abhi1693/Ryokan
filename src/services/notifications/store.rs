//! DB-side CRUD on `notification_providers` and per-event opt-in
//! lookups on `notification_settings`. Read paths land in the
//! dispatcher's hot path; write paths are settings-handler bound.

use sqlx::SqlitePool;
use std::collections::HashMap;

/// Row shape for `notification_providers`.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ProviderRow {
    pub id: i64,
    pub name: String,
    pub kind: String,
    pub enabled: bool,
    pub config_json: String,
}

/// Read every enabled provider row. Caller (the cache rebuild) walks
/// these, picks the right trait impl per `kind`, and assembles the
/// `Vec<Arc<dyn NotificationProvider>>` snapshot.
pub async fn list_enabled(db: &SqlitePool) -> Result<Vec<ProviderRow>, sqlx::Error> {
    sqlx::query_as::<_, ProviderRow>(
        "SELECT id, name, kind, enabled, config_json
         FROM notification_providers
         WHERE enabled = 1
         ORDER BY id ASC",
    )
    .fetch_all(db)
    .await
}

/// Read the per-event opt-in matrix for a single provider, returning
/// a HashMap keyed by `event_kind` string. Used by the dispatcher
/// to decide whether to fan out a given event to a given provider.
///
/// **Default-deny** — an event_kind missing from the matrix is
/// treated as opted-out by the dispatcher. The settings handler
/// seeds default-on rows at provider-creation time so a freshly
/// added provider receives the four conservative defaults
/// (Grabbed / Imported / ImportFailed / ExternalSyncReLinkRequired)
/// without further user action.
pub async fn matrix_for_provider(
    db: &SqlitePool,
    provider_id: i64,
) -> Result<HashMap<String, bool>, sqlx::Error> {
    let rows: Vec<(String, bool)> = sqlx::query_as(
        "SELECT event_kind, enabled FROM notification_settings WHERE provider_id = ?",
    )
    .bind(provider_id)
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().collect())
}

/// Seed a freshly-created provider's matrix with the conservative
/// default-on policy (`DEFAULT_ON_EVENT_KINDS`). Called from the
/// settings handler at provider-create time. Idempotent via
/// `INSERT OR IGNORE` — re-running for an existing provider is a
/// no-op.
pub async fn seed_default_matrix(db: &SqlitePool, provider_id: i64) -> Result<(), sqlx::Error> {
    for kind in super::event::DEFAULT_ON_EVENT_KINDS {
        sqlx::query(
            "INSERT OR IGNORE INTO notification_settings (provider_id, event_kind, enabled)
             VALUES (?, ?, 1)",
        )
        .bind(provider_id)
        .bind(*kind)
        .execute(db)
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::in_memory_pool;

    async fn insert_provider(db: &SqlitePool, name: &str, kind: &str, enabled: bool) -> i64 {
        let row: (i64,) = sqlx::query_as(
            "INSERT INTO notification_providers (name, kind, enabled, config_json)
             VALUES (?, ?, ?, '{}') RETURNING id",
        )
        .bind(name)
        .bind(kind)
        .bind(enabled as i64)
        .fetch_one(db)
        .await
        .unwrap();
        row.0
    }

    #[tokio::test]
    async fn list_enabled_skips_disabled_rows() {
        let db = in_memory_pool().await;
        let live = insert_provider(&db, "live", "webhook", true).await;
        let _muted = insert_provider(&db, "muted", "webhook", false).await;
        let rows = list_enabled(&db).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, live);
    }

    #[tokio::test]
    async fn seed_default_matrix_inserts_default_on_kinds() {
        let db = in_memory_pool().await;
        let id = insert_provider(&db, "p", "webhook", true).await;
        seed_default_matrix(&db, id).await.unwrap();
        let m = matrix_for_provider(&db, id).await.unwrap();
        // Every default-on kind must be present and enabled; nothing else.
        for k in super::super::event::DEFAULT_ON_EVENT_KINDS {
            assert_eq!(m.get(*k), Some(&true), "{k} should be default-on");
        }
        assert_eq!(m.len(), super::super::event::DEFAULT_ON_EVENT_KINDS.len());
    }

    #[tokio::test]
    async fn seed_default_matrix_is_idempotent() {
        // Running twice must not duplicate rows nor flip an
        // already-disabled row back to enabled — a user who turned
        // off `Grabbed` and then re-saved the provider config
        // shouldn't see their preference silently reverted.
        let db = in_memory_pool().await;
        let id = insert_provider(&db, "p", "webhook", true).await;
        seed_default_matrix(&db, id).await.unwrap();
        // User flips Grabbed off.
        sqlx::query(
            "UPDATE notification_settings SET enabled = 0
             WHERE provider_id = ? AND event_kind = 'Grabbed'",
        )
        .bind(id)
        .execute(&db)
        .await
        .unwrap();
        // Re-seed (e.g. on a settings save).
        seed_default_matrix(&db, id).await.unwrap();
        let m = matrix_for_provider(&db, id).await.unwrap();
        assert_eq!(m.get("Grabbed"), Some(&false));
    }

    #[tokio::test]
    async fn provider_delete_cascades_matrix_rows() {
        // `notification_settings.provider_id` carries
        // `ON DELETE CASCADE` in the migration. Pinned here so a
        // future migration that drops the constraint produces a
        // loud test failure rather than silently leaving orphan
        // rows that re-attach if a provider id is reused.
        let db = in_memory_pool().await;
        sqlx::query("PRAGMA foreign_keys = ON")
            .execute(&db)
            .await
            .unwrap();
        let id = insert_provider(&db, "p", "webhook", true).await;
        seed_default_matrix(&db, id).await.unwrap();
        sqlx::query("DELETE FROM notification_providers WHERE id = ?")
            .bind(id)
            .execute(&db)
            .await
            .unwrap();
        let m = matrix_for_provider(&db, id).await.unwrap();
        assert!(m.is_empty(), "matrix rows must cascade out");
    }
}
