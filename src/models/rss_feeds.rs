//! User-configured RSS feeds (multi-rss PR 1, Option A).
//!
//! Custom RSS sources beyond the Nyaa-direct path live here. Each row
//! is a `(name, url)` pair the sync loop fetches every tick and merges
//! into the same `rss_seen` dedup pool that already keys on
//! info_hash / GUID. Per-feed `download_client_id` lets a public-feed
//! grab route to the local qBit while a PT-flavored feed routes to
//! the seedbox; NULL falls through to the default at grab time.
//!
//! Distinct from `models::indexers`: indexers are searchable
//! torznab/newznab APIs that *also* (with `rss_enabled = 1`) drop
//! their `?t=tvsearch` endpoint into the same fan-out. `rss_feeds`
//! covers the case where the source isn't a torznab/newznab indexer
//! at all — bare RSS URLs (SubsPlease's per-quality feeds, an
//! uploader-filtered Nyaa search, an aggregator's curated feed).

use serde::Serialize;
use sqlx::{Row, SqlitePool};

#[derive(Debug, Clone, Serialize)]
pub struct RssFeed {
    pub id: i64,
    pub name: String,
    pub url: String,
    pub enabled: bool,
    /// Multi-client routing pin — id of the row in `download_clients`
    /// this feed routes to. `None` means "use the default client at
    /// grab time" (matches the indexer pin shape).
    pub download_client_id: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Input for [`insert`] / [`update`]. Caller-supplied fields only;
/// id + timestamps are managed by the model.
#[derive(Debug, Clone)]
pub struct RssFeedForm<'a> {
    pub name: &'a str,
    pub url: &'a str,
    pub enabled: bool,
    pub download_client_id: Option<i64>,
}

const SELECT_COLUMNS: &str = "id, name, url, enabled, download_client_id, created_at, updated_at";

fn row_to_feed(row: &sqlx::sqlite::SqliteRow) -> RssFeed {
    RssFeed {
        id: row.try_get("id").unwrap_or(0),
        name: row.try_get("name").unwrap_or_default(),
        url: row.try_get("url").unwrap_or_default(),
        enabled: row.try_get::<i64, _>("enabled").unwrap_or(0) != 0,
        download_client_id: row
            .try_get::<Option<i64>, _>("download_client_id")
            .unwrap_or(None),
        created_at: row.try_get("created_at").unwrap_or(0),
        updated_at: row.try_get("updated_at").unwrap_or(0),
    }
}

/// All feed rows ordered by id. Settings page reads this to render
/// the management table.
pub async fn list_all(db: &SqlitePool) -> Result<Vec<RssFeed>, sqlx::Error> {
    let rows = sqlx::query(&format!(
        "SELECT {SELECT_COLUMNS} FROM rss_feeds ORDER BY id ASC"
    ))
    .fetch_all(db)
    .await?;
    Ok(rows.iter().map(row_to_feed).collect())
}

/// Enabled feeds only — what the RSS sync loop iterates over.
pub async fn list_enabled(db: &SqlitePool) -> Result<Vec<RssFeed>, sqlx::Error> {
    let rows = sqlx::query(&format!(
        "SELECT {SELECT_COLUMNS} FROM rss_feeds WHERE enabled = 1 ORDER BY id ASC"
    ))
    .fetch_all(db)
    .await?;
    Ok(rows.iter().map(row_to_feed).collect())
}

pub async fn get_by_id(db: &SqlitePool, id: i64) -> Result<Option<RssFeed>, sqlx::Error> {
    let row = sqlx::query(&format!(
        "SELECT {SELECT_COLUMNS} FROM rss_feeds WHERE id = ?"
    ))
    .bind(id)
    .fetch_optional(db)
    .await?;
    Ok(row.as_ref().map(row_to_feed))
}

pub async fn insert(db: &SqlitePool, form: RssFeedForm<'_>) -> Result<i64, sqlx::Error> {
    let result = sqlx::query(
        "INSERT INTO rss_feeds (name, url, enabled, download_client_id) \
         VALUES (?, ?, ?, ?)",
    )
    .bind(form.name)
    .bind(form.url)
    .bind(form.enabled as i64)
    .bind(form.download_client_id)
    .execute(db)
    .await?;
    Ok(result.last_insert_rowid())
}

pub async fn update(db: &SqlitePool, id: i64, form: RssFeedForm<'_>) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE rss_feeds SET \
         name = ?, url = ?, enabled = ?, download_client_id = ?, \
         updated_at = strftime('%s','now') \
         WHERE id = ?",
    )
    .bind(form.name)
    .bind(form.url)
    .bind(form.enabled as i64)
    .bind(form.download_client_id)
    .bind(id)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn delete(db: &SqlitePool, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM rss_feeds WHERE id = ?")
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::in_memory_pool;

    fn form<'a>(name: &'a str, url: &'a str) -> RssFeedForm<'a> {
        RssFeedForm {
            name,
            url,
            enabled: true,
            download_client_id: None,
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
        assert!(r.created_at > 0);
    }

    #[tokio::test]
    async fn url_uniqueness_prevents_duplicates() {
        // The schema's UNIQUE on `url` — pinned so a settings-form
        // double-submit doesn't silently create two rows with the
        // same URL and double-fetch the feed.
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
            RssFeedForm {
                name: "Inactive",
                url: "https://b.example/rss",
                enabled: false,
                download_client_id: None,
            },
        )
        .await
        .unwrap();

        let enabled = list_enabled(&db).await.unwrap();
        assert_eq!(enabled.len(), 1);
        assert_eq!(enabled[0].name, "Active");
        // list_all still returns both.
        let all = list_all(&db).await.unwrap();
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn get_by_id_returns_none_on_miss() {
        let db = in_memory_pool().await;
        assert!(get_by_id(&db, 9999).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn update_changes_url_and_persists_download_client_pin() {
        let db = in_memory_pool().await;
        // Seed a real download_clients row so the FK accepts the pin.
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
            RssFeedForm {
                name: "Feed (renamed)",
                url: "https://example.com/v2",
                enabled: true,
                download_client_id: Some(dc_id),
            },
        )
        .await
        .unwrap();

        let row = get_by_id(&db, id).await.unwrap().unwrap();
        assert_eq!(row.name, "Feed (renamed)");
        assert_eq!(row.url, "https://example.com/v2");
        assert_eq!(row.download_client_id, Some(dc_id));
    }

    #[tokio::test]
    async fn fk_set_null_on_download_client_delete_clears_pin() {
        // Pin the FK behavior that lets a feed survive a download
        // client delete: ON DELETE SET NULL transparently flips the
        // pin to None, so the feed grabs through the default
        // afterward instead of erroring at sync time.
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
            RssFeedForm {
                name: "Feed",
                url: "https://example.com/feed",
                enabled: true,
                download_client_id: Some(dc_id),
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
    async fn delete_drops_row() {
        let db = in_memory_pool().await;
        let id = insert(&db, form("Doomed", "https://gone.example/rss"))
            .await
            .unwrap();
        delete(&db, id).await.unwrap();
        assert!(get_by_id(&db, id).await.unwrap().is_none());
    }
}
