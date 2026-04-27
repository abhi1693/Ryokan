//! User-configured RSS feeds (multi-rss commit A → renamed in
//! commit E from `rss_feeds`).
//!
//! Custom RSS sources beyond the Nyaa-direct path live here.
//! Each row is a `(name, url)` pair the sync loop fetches every
//! tick and merges into the same `rss_seen` dedup pool keyed on
//! `(source, source_id, item_key)`. Per-feed `download_client_id`
//! lets a public-feed grab route to the local qBit while a
//! PT-flavored feed routes to the seedbox; NULL falls through to
//! the default at grab time.
//!
//! Distinct from `models::indexers`: indexers are searchable
//! torznab/newznab APIs that *also* (with `rss_enabled = 1`)
//! drop their `?t=tvsearch&cat=5070` endpoint into the same
//! fan-out. `direct_rss_feeds` covers the case where the source
//! isn't a torznab/newznab indexer at all — bare RSS URLs
//! (SubsPlease's per-quality feeds, an uploader-filtered Nyaa
//! search, an aggregator's curated feed).
//!
//! Observability columns (`last_polled_at`, `last_poll_error`,
//! `last_item_count`) drive the inline status chips on the
//! Settings → Indexers / Direct RSS feeds table. Updated by the
//! sync fan-out on every poll attempt; inspected read-only by
//! the UI.
//!
//! `detected_protocol` is populated by the Test button (commit
//! G) the first time the user successfully tests a feed —
//! `"torrent"` / `"usenet"` / empty. The pin save path enforces
//! protocol match against this value when non-empty so a torrent-
//! yielding feed can't be pinned to a SAB client. Empty means
//! "not yet tested"; the pin save path permits any client until
//! the first successful test.

use serde::Serialize;
use sqlx::{Row, SqlitePool};

#[derive(Debug, Clone, Serialize)]
pub struct DirectRssFeed {
    pub id: i64,
    pub name: String,
    pub url: String,
    pub enabled: bool,
    /// Multi-client routing pin — id of the row in
    /// `download_clients` this feed routes to. `None` means "use
    /// the default client at grab time" (matches the indexer pin
    /// shape).
    pub download_client_id: Option<i64>,
    /// Per-feed override of the default RSS request timeout.
    /// `None` means use the global default (matches the
    /// per-indexer override on `indexers.request_timeout_secs`).
    pub request_timeout_secs: Option<i64>,
    /// `"torrent"` / `"usenet"` / empty. Populated by the Test
    /// button on first successful fetch. Empty = not yet tested
    /// (pin save permissive); non-empty = pin save enforces
    /// protocol match.
    pub detected_protocol: String,
    /// Unix epoch of the last poll attempt. `None` until the
    /// fan-out's first run against this feed.
    pub last_polled_at: Option<i64>,
    /// Most recent poll error, if any. Empty when the last poll
    /// succeeded — UI uses non-empty as the "show ✗ pill" signal.
    pub last_poll_error: String,
    /// Item count from the last successful poll. Used by the UI
    /// chip ("✓ 18 items"); reset to 0 when a poll errors so the
    /// chip doesn't lie about a stale count alongside a fresh
    /// error.
    pub last_item_count: i32,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Input for [`insert`] / [`update`]. Caller-supplied fields
/// only; id + timestamps + observability columns are managed by
/// the model.
#[derive(Debug, Clone)]
pub struct DirectRssFeedForm<'a> {
    pub name: &'a str,
    pub url: &'a str,
    pub enabled: bool,
    pub download_client_id: Option<i64>,
    pub request_timeout_secs: Option<i64>,
}

const SELECT_COLUMNS: &str = "id, name, url, enabled, download_client_id, \
    request_timeout_secs, detected_protocol, \
    last_polled_at, last_poll_error, last_item_count, \
    created_at, updated_at";

fn row_to_feed(row: &sqlx::sqlite::SqliteRow) -> DirectRssFeed {
    DirectRssFeed {
        id: row.try_get("id").unwrap_or(0),
        name: row.try_get("name").unwrap_or_default(),
        url: row.try_get("url").unwrap_or_default(),
        enabled: row.try_get::<i64, _>("enabled").unwrap_or(0) != 0,
        download_client_id: row
            .try_get::<Option<i64>, _>("download_client_id")
            .unwrap_or(None),
        request_timeout_secs: row
            .try_get::<Option<i64>, _>("request_timeout_secs")
            .unwrap_or(None),
        detected_protocol: row.try_get("detected_protocol").unwrap_or_default(),
        last_polled_at: row
            .try_get::<Option<i64>, _>("last_polled_at")
            .unwrap_or(None),
        last_poll_error: row.try_get("last_poll_error").unwrap_or_default(),
        last_item_count: row.try_get("last_item_count").unwrap_or(0),
        created_at: row.try_get("created_at").unwrap_or(0),
        updated_at: row.try_get("updated_at").unwrap_or(0),
    }
}

/// All feed rows ordered by id. Settings page reads this to
/// render the management table.
pub async fn list_all(db: &SqlitePool) -> Result<Vec<DirectRssFeed>, sqlx::Error> {
    let rows = sqlx::query(&format!(
        "SELECT {SELECT_COLUMNS} FROM direct_rss_feeds ORDER BY id ASC"
    ))
    .fetch_all(db)
    .await?;
    Ok(rows.iter().map(row_to_feed).collect())
}

/// Enabled feeds only — what the RSS sync loop iterates over.
pub async fn list_enabled(db: &SqlitePool) -> Result<Vec<DirectRssFeed>, sqlx::Error> {
    let rows = sqlx::query(&format!(
        "SELECT {SELECT_COLUMNS} FROM direct_rss_feeds WHERE enabled = 1 ORDER BY id ASC"
    ))
    .fetch_all(db)
    .await?;
    Ok(rows.iter().map(row_to_feed).collect())
}

pub async fn get_by_id(db: &SqlitePool, id: i64) -> Result<Option<DirectRssFeed>, sqlx::Error> {
    let row = sqlx::query(&format!(
        "SELECT {SELECT_COLUMNS} FROM direct_rss_feeds WHERE id = ?"
    ))
    .bind(id)
    .fetch_optional(db)
    .await?;
    Ok(row.as_ref().map(row_to_feed))
}

pub async fn insert(db: &SqlitePool, form: DirectRssFeedForm<'_>) -> Result<i64, sqlx::Error> {
    let result = sqlx::query(
        "INSERT INTO direct_rss_feeds \
         (name, url, enabled, download_client_id, request_timeout_secs) \
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(form.name)
    .bind(form.url)
    .bind(form.enabled as i64)
    .bind(form.download_client_id)
    .bind(form.request_timeout_secs)
    .execute(db)
    .await?;
    Ok(result.last_insert_rowid())
}

pub async fn update(
    db: &SqlitePool,
    id: i64,
    form: DirectRssFeedForm<'_>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE direct_rss_feeds SET \
         name = ?, url = ?, enabled = ?, download_client_id = ?, \
         request_timeout_secs = ?, updated_at = strftime('%s','now') \
         WHERE id = ?",
    )
    .bind(form.name)
    .bind(form.url)
    .bind(form.enabled as i64)
    .bind(form.download_client_id)
    .bind(form.request_timeout_secs)
    .bind(id)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn delete(db: &SqlitePool, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM direct_rss_feeds WHERE id = ?")
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

/// Stamp the detected protocol (`"torrent"` / `"usenet"`) on a
/// feed after a successful Test fetch. Surfaced separately from
/// the user-editable form so a Save can't accidentally clear the
/// detection result by re-submitting an empty value (the form
/// doesn't carry this field).
pub async fn set_detected_protocol(
    db: &SqlitePool,
    id: i64,
    protocol: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE direct_rss_feeds SET \
         detected_protocol = ?, updated_at = strftime('%s','now') \
         WHERE id = ?",
    )
    .bind(protocol)
    .bind(id)
    .execute(db)
    .await?;
    Ok(())
}

/// Record observability metrics from a sync-tick poll attempt.
/// Called by the fan-out (commit F) after every fetch — success
/// or failure. `error` is empty on success, populated on failure.
/// `item_count` is reset to 0 on failure so the chip doesn't lie
/// about a stale count alongside a fresh error.
pub async fn record_poll_metrics(
    db: &SqlitePool,
    id: i64,
    item_count: i32,
    error: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE direct_rss_feeds SET \
         last_polled_at = strftime('%s','now'), \
         last_poll_error = ?, last_item_count = ? \
         WHERE id = ?",
    )
    .bind(error)
    .bind(item_count)
    .bind(id)
    .execute(db)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::in_memory_pool;

    fn form<'a>(name: &'a str, url: &'a str) -> DirectRssFeedForm<'a> {
        DirectRssFeedForm {
            name,
            url,
            enabled: true,
            download_client_id: None,
            request_timeout_secs: None,
        }
    }

    #[tokio::test]
    async fn insert_then_list_round_trips() {
        let db = in_memory_pool().await;
        let id = insert(
            &db,
            form("SubsPlease 1080p", "https://subsplease.org/rss/?t&r=1080"),
        )
        .await
        .unwrap();
        assert!(id > 0);

        let rows = list_all(&db).await.unwrap();
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(r.name, "SubsPlease 1080p");
        assert_eq!(r.url, "https://subsplease.org/rss/?t&r=1080");
        assert!(r.enabled);
        assert!(r.download_client_id.is_none());
        assert!(r.request_timeout_secs.is_none());
        assert_eq!(r.detected_protocol, "");
        assert!(r.last_polled_at.is_none());
        assert_eq!(r.last_poll_error, "");
        assert_eq!(r.last_item_count, 0);
        assert!(r.created_at > 0);
    }

    #[tokio::test]
    async fn url_uniqueness_prevents_duplicates() {
        // Schema's UNIQUE on `url` survives the rename — a
        // settings-form double-submit can't create two rows
        // poll-fetching the same URL.
        let db = in_memory_pool().await;
        insert(&db, form("First", "https://example.com/rss"))
            .await
            .unwrap();
        let err = insert(&db, form("Second", "https://example.com/rss")).await;
        assert!(err.is_err(), "duplicate URL must be rejected");
    }

    #[tokio::test]
    async fn list_enabled_filters_disabled_rows() {
        let db = in_memory_pool().await;
        insert(&db, form("Active", "https://a.example/rss"))
            .await
            .unwrap();
        let off_id = insert(&db, form("Inactive", "https://b.example/rss"))
            .await
            .unwrap();
        update(
            &db,
            off_id,
            DirectRssFeedForm {
                name: "Inactive",
                url: "https://b.example/rss",
                enabled: false,
                download_client_id: None,
                request_timeout_secs: None,
            },
        )
        .await
        .unwrap();

        let enabled = list_enabled(&db).await.unwrap();
        assert_eq!(enabled.len(), 1);
        assert_eq!(enabled[0].name, "Active");
        assert_eq!(list_all(&db).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn get_by_id_returns_none_on_miss() {
        let db = in_memory_pool().await;
        assert!(get_by_id(&db, 9999).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn update_persists_download_client_pin_and_timeout_override() {
        let db = in_memory_pool().await;
        let dc_id = crate::models::download_clients::insert(
            &db,
            crate::models::download_clients::DownloadClientForm {
                name: "qbit",
                kind: "qbittorrent",
                url: "http://qbit",
                username: "",
                password: "",
                label: "",
                download_path: "",
                enabled: true,
                is_default: true,
            },
        )
        .await
        .unwrap();

        let id = insert(&db, form("Feed", "https://example.com/v1"))
            .await
            .unwrap();
        update(
            &db,
            id,
            DirectRssFeedForm {
                name: "Feed (renamed)",
                url: "https://example.com/v2",
                enabled: true,
                download_client_id: Some(dc_id),
                request_timeout_secs: Some(45),
            },
        )
        .await
        .unwrap();

        let row = get_by_id(&db, id).await.unwrap().unwrap();
        assert_eq!(row.name, "Feed (renamed)");
        assert_eq!(row.url, "https://example.com/v2");
        assert_eq!(row.download_client_id, Some(dc_id));
        assert_eq!(row.request_timeout_secs, Some(45));
    }

    #[tokio::test]
    async fn fk_set_null_on_download_client_delete_clears_pin() {
        // Pin the FK behavior that lets a feed survive a download
        // client delete: ON DELETE SET NULL transparently flips
        // the pin to None (verified post-rename — SQLite preserves
        // FKs across ALTER TABLE RENAME).
        let db = in_memory_pool().await;
        let dc_id = crate::models::download_clients::insert(
            &db,
            crate::models::download_clients::DownloadClientForm {
                name: "qbit",
                kind: "qbittorrent",
                url: "http://qbit",
                username: "",
                password: "",
                label: "",
                download_path: "",
                enabled: true,
                is_default: true,
            },
        )
        .await
        .unwrap();
        let id = insert(
            &db,
            DirectRssFeedForm {
                name: "Feed",
                url: "https://example.com/feed",
                enabled: true,
                download_client_id: Some(dc_id),
                request_timeout_secs: None,
            },
        )
        .await
        .unwrap();

        sqlx::query("DELETE FROM download_clients WHERE id = ?")
            .bind(dc_id)
            .execute(&db)
            .await
            .unwrap();

        let row = get_by_id(&db, id).await.unwrap().unwrap();
        assert!(
            row.download_client_id.is_none(),
            "FK ON DELETE SET NULL must clear the pin"
        );
    }

    #[tokio::test]
    async fn set_detected_protocol_round_trips() {
        let db = in_memory_pool().await;
        let id = insert(&db, form("Feed", "https://x.example/rss"))
            .await
            .unwrap();
        set_detected_protocol(&db, id, "torrent").await.unwrap();
        let row = get_by_id(&db, id).await.unwrap().unwrap();
        assert_eq!(row.detected_protocol, "torrent");
    }

    #[tokio::test]
    async fn record_poll_metrics_writes_success_path() {
        let db = in_memory_pool().await;
        let id = insert(&db, form("Feed", "https://x.example/rss"))
            .await
            .unwrap();
        record_poll_metrics(&db, id, 12, "").await.unwrap();
        let row = get_by_id(&db, id).await.unwrap().unwrap();
        assert!(row.last_polled_at.unwrap_or(0) > 0);
        assert_eq!(row.last_poll_error, "");
        assert_eq!(row.last_item_count, 12);
    }

    #[tokio::test]
    async fn record_poll_metrics_resets_count_on_failure() {
        // The chip mustn't lie about a stale item count alongside
        // a fresh error — set 12 items, then record an error,
        // assert count drops back to 0.
        let db = in_memory_pool().await;
        let id = insert(&db, form("Feed", "https://x.example/rss"))
            .await
            .unwrap();
        record_poll_metrics(&db, id, 12, "").await.unwrap();
        record_poll_metrics(&db, id, 0, "503 upstream")
            .await
            .unwrap();

        let row = get_by_id(&db, id).await.unwrap().unwrap();
        assert_eq!(row.last_item_count, 0);
        assert_eq!(row.last_poll_error, "503 upstream");
    }

    #[tokio::test]
    async fn delete_drops_row() {
        let db = in_memory_pool().await;
        let id = insert(&db, form("Doomed", "https://gone.example/rss"))
            .await
            .unwrap();
        delete(&db, id).await.unwrap();
        assert!(get_by_id(&db, id).await.unwrap().is_none());
    }
}
