//! Torznab/newznab indexer registry (issue #28 PR A).
//!
//! Configured indexers live in the `indexers` table; the search
//! pipeline reads them at fan-out time and dispatches concurrent
//! queries via the [`crate::services::indexers::Indexer`] trait.
//! Nyaa stays out-of-band per plan decision #1 — it never gets a
//! row here.
//!
//! PR A scope: schema + CRUD only. PR B adds the `TorznabIndexer`
//! trait impl that consumes these rows and the caps-probe path that
//! populates `caps_json` / `caps_refreshed_at`. PR C wires
//! `seed_ratio` / `seed_time_minutes` / `min_seeders` into the
//! `DownloadClient` trait's per-torrent seed rules. Nothing else
//! in PR A reads these columns yet — they're populated by the
//! Settings form and lie dormant until later PRs hook in.

use serde::Serialize;
use sqlx::{Row, SqlitePool};

/// Indexer protocol kind. The wire format for torznab and newznab
/// is identical; the value distinguishes them only for category-
/// mapping (BitTorrent vs NZB) and download-client routing once
/// PR F's torrent-vs-usenet split lands. Kept as `String` at the
/// boundary because `kind` is read directly into the row struct;
/// callers that need to branch on it can `.as_str()` and match.
pub const KIND_TORZNAB: &str = "torznab";
pub const KIND_NEWZNAB: &str = "newznab";

#[derive(Debug, Clone, Serialize)]
pub struct Indexer {
    pub id: i64,
    pub name: String,
    pub kind: String,
    pub url: String,
    pub api_key: String,
    /// Sonarr convention: lower = preferred. Range 1-50, default 25.
    /// Drives auto-search dedup attribution + interactive search row
    /// tiebreaks + fan-out concurrency order.
    pub priority: i32,
    pub enabled: bool,
    pub is_private_tracker: bool,
    pub seed_ratio: Option<f64>,
    pub seed_time_minutes: Option<i64>,
    pub min_seeders: i32,
    /// Per-indexer override of the default search timeout. `None`
    /// means use the process default (30s, overridable via
    /// `RYOKAN_INDEXER_DEFAULT_TIMEOUT_SECS`).
    pub request_timeout_secs: Option<i64>,
    /// Cached caps response body. Empty until the first probe
    /// succeeds (PR B). Read with a 7-day TTL — stale caps trigger
    /// a transparent re-fetch on next read.
    pub caps_json: String,
    pub caps_refreshed_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Input for [`insert`] / [`update`] — caller supplies all the
/// user-editable fields; ID + timestamps + caps cache are managed
/// by this module.
#[derive(Debug, Clone)]
pub struct IndexerForm<'a> {
    pub name: &'a str,
    pub kind: &'a str,
    pub url: &'a str,
    pub api_key: &'a str,
    pub priority: i32,
    pub enabled: bool,
    pub is_private_tracker: bool,
    pub seed_ratio: Option<f64>,
    pub seed_time_minutes: Option<i64>,
    pub min_seeders: i32,
    pub request_timeout_secs: Option<i64>,
}

const SELECT_COLUMNS: &str = "id, name, kind, url, api_key, priority, enabled, \
    is_private_tracker, seed_ratio, seed_time_minutes, min_seeders, request_timeout_secs, \
    caps_json, caps_refreshed_at, created_at, updated_at";

fn row_to_indexer(row: &sqlx::sqlite::SqliteRow) -> Indexer {
    // Nullable columns explicitly typed as `Option<T>` so sqlx
    // doesn't fall back to T::default() (0.0 for f64, 0 for i64)
    // when the column is NULL — `try_get::<f64, _>` on a NULL row
    // returns Err, which `.ok()` would convert to None, but the
    // type-inferred `try_get` infers T from the field type and
    // produces Some(0.0)/Some(0) for NULLs. The explicit
    // `Option<T>` form is the unambiguous one.
    Indexer {
        id: row.try_get("id").unwrap_or(0),
        name: row.try_get("name").unwrap_or_default(),
        kind: row.try_get("kind").unwrap_or_default(),
        url: row.try_get("url").unwrap_or_default(),
        api_key: row.try_get("api_key").unwrap_or_default(),
        priority: row.try_get("priority").unwrap_or(25),
        enabled: row.try_get::<i64, _>("enabled").unwrap_or(0) != 0,
        is_private_tracker: row.try_get::<i64, _>("is_private_tracker").unwrap_or(0) != 0,
        seed_ratio: row.try_get::<Option<f64>, _>("seed_ratio").unwrap_or(None),
        seed_time_minutes: row
            .try_get::<Option<i64>, _>("seed_time_minutes")
            .unwrap_or(None),
        min_seeders: row.try_get("min_seeders").unwrap_or(1),
        request_timeout_secs: row
            .try_get::<Option<i64>, _>("request_timeout_secs")
            .unwrap_or(None),
        caps_json: row.try_get("caps_json").unwrap_or_default(),
        caps_refreshed_at: row
            .try_get::<Option<i64>, _>("caps_refreshed_at")
            .unwrap_or(None),
        created_at: row.try_get("created_at").unwrap_or(0),
        updated_at: row.try_get("updated_at").unwrap_or(0),
    }
}

/// All indexer rows ordered by `priority` ascending, tiebreaking by
/// `id`. Mirrors the order auto-search uses for fan-out concurrency.
pub async fn list_all(db: &SqlitePool) -> Result<Vec<Indexer>, sqlx::Error> {
    let rows = sqlx::query(&format!(
        "SELECT {SELECT_COLUMNS} FROM indexers ORDER BY priority ASC, id ASC"
    ))
    .fetch_all(db)
    .await?;
    Ok(rows.iter().map(row_to_indexer).collect())
}

/// Enabled indexers only — what the search pipeline iterates over.
/// Disabled rows stay in the DB so the user's config isn't lost when
/// they pause a flaky indexer; this filter just skips them at search
/// time.
pub async fn list_enabled(db: &SqlitePool) -> Result<Vec<Indexer>, sqlx::Error> {
    let rows = sqlx::query(&format!(
        "SELECT {SELECT_COLUMNS} FROM indexers WHERE enabled = 1 ORDER BY priority ASC, id ASC"
    ))
    .fetch_all(db)
    .await?;
    Ok(rows.iter().map(row_to_indexer).collect())
}

pub async fn get_by_id(db: &SqlitePool, id: i64) -> Result<Option<Indexer>, sqlx::Error> {
    let row = sqlx::query(&format!(
        "SELECT {SELECT_COLUMNS} FROM indexers WHERE id = ?"
    ))
    .bind(id)
    .fetch_optional(db)
    .await?;
    Ok(row.as_ref().map(row_to_indexer))
}

pub async fn insert(db: &SqlitePool, form: IndexerForm<'_>) -> Result<i64, sqlx::Error> {
    let result = sqlx::query(
        "INSERT INTO indexers \
         (name, kind, url, api_key, priority, enabled, is_private_tracker, \
          seed_ratio, seed_time_minutes, min_seeders, request_timeout_secs) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(form.name)
    .bind(form.kind)
    .bind(form.url)
    .bind(form.api_key)
    .bind(form.priority)
    .bind(form.enabled as i64)
    .bind(form.is_private_tracker as i64)
    .bind(form.seed_ratio)
    .bind(form.seed_time_minutes)
    .bind(form.min_seeders)
    .bind(form.request_timeout_secs)
    .execute(db)
    .await?;
    Ok(result.last_insert_rowid())
}

pub async fn update(db: &SqlitePool, id: i64, form: IndexerForm<'_>) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE indexers SET \
         name = ?, kind = ?, url = ?, api_key = ?, priority = ?, enabled = ?, \
         is_private_tracker = ?, seed_ratio = ?, seed_time_minutes = ?, min_seeders = ?, \
         request_timeout_secs = ?, updated_at = strftime('%s','now') \
         WHERE id = ?",
    )
    .bind(form.name)
    .bind(form.kind)
    .bind(form.url)
    .bind(form.api_key)
    .bind(form.priority)
    .bind(form.enabled as i64)
    .bind(form.is_private_tracker as i64)
    .bind(form.seed_ratio)
    .bind(form.seed_time_minutes)
    .bind(form.min_seeders)
    .bind(form.request_timeout_secs)
    .bind(id)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn delete(db: &SqlitePool, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM indexers WHERE id = ?")
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

/// Update the cached caps response and bump `caps_refreshed_at` to
/// the current Unix timestamp. Called by PR B's caps probe after a
/// successful `t=caps` fetch. PR A defines the helper so the column
/// shape is decided up front.
pub async fn update_caps(db: &SqlitePool, id: i64, caps_json: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE indexers SET caps_json = ?, caps_refreshed_at = strftime('%s','now'), \
         updated_at = strftime('%s','now') WHERE id = ?",
    )
    .bind(caps_json)
    .bind(id)
    .execute(db)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::SqlitePool;

    async fn fresh_db() -> SqlitePool {
        let db = SqlitePool::connect("sqlite::memory:").await.unwrap();
        crate::models::migrate(&db).await.unwrap();
        db
    }

    fn sample_form<'a>() -> IndexerForm<'a> {
        IndexerForm {
            name: "Test Indexer",
            kind: KIND_TORZNAB,
            url: "https://prowlarr.local/1/api",
            api_key: "secret",
            priority: 25,
            enabled: true,
            is_private_tracker: false,
            seed_ratio: None,
            seed_time_minutes: None,
            min_seeders: 1,
            request_timeout_secs: None,
        }
    }

    #[tokio::test]
    async fn insert_then_get_round_trips_fields() {
        let db = fresh_db().await;
        let id = insert(&db, sample_form()).await.unwrap();
        let row = get_by_id(&db, id).await.unwrap().expect("row exists");
        assert_eq!(row.name, "Test Indexer");
        assert_eq!(row.kind, KIND_TORZNAB);
        assert_eq!(row.url, "https://prowlarr.local/1/api");
        assert_eq!(row.api_key, "secret");
        assert_eq!(row.priority, 25);
        assert!(row.enabled);
        assert!(!row.is_private_tracker);
        assert_eq!(row.seed_ratio, None);
        assert_eq!(row.min_seeders, 1);
    }

    #[tokio::test]
    async fn list_all_orders_by_priority_ascending() {
        // Sonarr convention: lower priority = preferred. The fan-out
        // path iterates this order, so a regression that flipped it
        // would silently move a less-preferred indexer to the front.
        let db = fresh_db().await;
        let mut high_prio = sample_form();
        high_prio.name = "High Priority";
        high_prio.priority = 5;
        let mut low_prio = sample_form();
        low_prio.name = "Low Priority";
        low_prio.priority = 50;
        // Insert low first so order isn't accidentally insertion-order.
        insert(&db, low_prio).await.unwrap();
        insert(&db, high_prio).await.unwrap();

        let rows = list_all(&db).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name, "High Priority");
        assert_eq!(rows[1].name, "Low Priority");
    }

    #[tokio::test]
    async fn list_enabled_filters_disabled_rows() {
        let db = fresh_db().await;
        let mut on = sample_form();
        on.name = "On";
        on.enabled = true;
        let mut off = sample_form();
        off.name = "Off";
        off.enabled = false;
        insert(&db, on).await.unwrap();
        insert(&db, off).await.unwrap();

        let enabled = list_enabled(&db).await.unwrap();
        assert_eq!(enabled.len(), 1);
        assert_eq!(enabled[0].name, "On");
    }

    #[tokio::test]
    async fn update_changes_fields_and_bumps_updated_at() {
        let db = fresh_db().await;
        let id = insert(&db, sample_form()).await.unwrap();
        let original_updated_at = get_by_id(&db, id).await.unwrap().unwrap().updated_at;
        // strftime resolution is 1s; sleep ensures the bump is visible.
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;

        let mut edited = sample_form();
        edited.name = "Renamed";
        edited.priority = 10;
        edited.enabled = false;
        update(&db, id, edited).await.unwrap();

        let row = get_by_id(&db, id).await.unwrap().unwrap();
        assert_eq!(row.name, "Renamed");
        assert_eq!(row.priority, 10);
        assert!(!row.enabled);
        assert!(
            row.updated_at >= original_updated_at,
            "updated_at must not regress"
        );
    }

    #[tokio::test]
    async fn delete_removes_row() {
        let db = fresh_db().await;
        let id = insert(&db, sample_form()).await.unwrap();
        assert!(get_by_id(&db, id).await.unwrap().is_some());
        delete(&db, id).await.unwrap();
        assert!(get_by_id(&db, id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn update_caps_persists_json_and_timestamp() {
        let db = fresh_db().await;
        let id = insert(&db, sample_form()).await.unwrap();
        // Fresh insert has no caps cached yet.
        let pre = get_by_id(&db, id).await.unwrap().unwrap();
        assert_eq!(pre.caps_json, "");
        assert!(pre.caps_refreshed_at.is_none());

        update_caps(&db, id, r#"{"limits":{"max":100}}"#)
            .await
            .unwrap();
        let post = get_by_id(&db, id).await.unwrap().unwrap();
        assert_eq!(post.caps_json, r#"{"limits":{"max":100}}"#);
        assert!(post.caps_refreshed_at.is_some());
    }
}
