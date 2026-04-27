//! Multi-client routing — one row per *configured* download
//! client. Replaces the pre-multi-client single-slot config
//! (`config.active_client` + per-kind credentials columns) with
//! a row-per-client shape so a user can run "Local qBit" +
//! "Seedbox Deluge" + "NzbGeek SAB" simultaneously, and pin
//! individual indexers (or the built-in Nyaa search) to specific
//! clients.
//!
//! Pin resolution at grab time:
//! 1. `indexer.download_client_id` if the grab is attributable
//!    to a torznab indexer row.
//! 2. `config.nyaa_download_client_id` for built-in Nyaa hits.
//! 3. The row marked `is_default = 1`.
//! 4. Otherwise — surface "no download client configured."
//!
//! The `kind` column matches the values
//! `services::download_client::build_torrent_client` /
//! `build_usenet_client` accept (`"qbittorrent" | "deluge" |
//! "transmission" | "rtorrent"` for now; SAB lands later).
//! Validation lives at the form layer; reads here trust the DB
//! to hold a known value.

use sqlx::{Row, SqlitePool};

/// Row shape for `download_clients`. Mirrors the schema 1:1; no
/// derived columns.
#[derive(Debug, Clone)]
pub struct DownloadClientRow {
    pub id: i64,
    pub name: String,
    pub kind: String,
    pub url: String,
    pub username: String,
    pub password: String,
    pub label: String,
    pub download_path: String,
    pub enabled: bool,
    pub is_default: bool,
}

/// Insert/update payload — `&str` rather than `String` so the
/// caller can pass borrowed slices without an extra clone. Same
/// shape as the trait constructors expect.
pub struct DownloadClientForm<'a> {
    pub name: &'a str,
    pub kind: &'a str,
    pub url: &'a str,
    pub username: &'a str,
    pub password: &'a str,
    pub label: &'a str,
    pub download_path: &'a str,
    pub enabled: bool,
    pub is_default: bool,
}

const SELECT_COLS: &str = "id, name, kind, url, username, password, label, \
                           download_path, enabled, is_default";

fn map_row(r: sqlx::sqlite::SqliteRow) -> DownloadClientRow {
    DownloadClientRow {
        id: r.get("id"),
        name: r.get("name"),
        kind: r.get("kind"),
        url: r.get("url"),
        username: r.try_get("username").unwrap_or_default(),
        password: r.try_get("password").unwrap_or_default(),
        label: r.try_get("label").unwrap_or_default(),
        download_path: r.try_get("download_path").unwrap_or_default(),
        enabled: r
            .try_get::<i64, _>("enabled")
            .map(|v| v != 0)
            .unwrap_or(true),
        is_default: r
            .try_get::<i64, _>("is_default")
            .map(|v| v != 0)
            .unwrap_or(false),
    }
}

pub async fn list_all(db: &SqlitePool) -> Result<Vec<DownloadClientRow>, sqlx::Error> {
    let rows = sqlx::query(&format!(
        "SELECT {SELECT_COLS} FROM download_clients ORDER BY is_default DESC, name COLLATE NOCASE"
    ))
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(map_row).collect())
}

/// Used by the cache builder — only enabled rows get
/// instantiated as live trait impls. Disabled rows survive in
/// the DB so a user can toggle them back on without re-entering
/// credentials.
pub async fn list_enabled(db: &SqlitePool) -> Result<Vec<DownloadClientRow>, sqlx::Error> {
    let rows = sqlx::query(&format!(
        "SELECT {SELECT_COLS} FROM download_clients WHERE enabled = 1 ORDER BY id"
    ))
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(map_row).collect())
}

pub async fn get_by_id(db: &SqlitePool, id: i64) -> Result<Option<DownloadClientRow>, sqlx::Error> {
    let row = sqlx::query(&format!(
        "SELECT {SELECT_COLS} FROM download_clients WHERE id = ?"
    ))
    .bind(id)
    .fetch_optional(db)
    .await?;
    Ok(row.map(map_row))
}

/// The current default client, if any. NULL when no client has
/// been added yet (fresh install) or when a manual DB edit
/// cleared every `is_default = 1`.
pub async fn get_default(db: &SqlitePool) -> Result<Option<DownloadClientRow>, sqlx::Error> {
    let row = sqlx::query(&format!(
        "SELECT {SELECT_COLS} FROM download_clients WHERE is_default = 1 LIMIT 1"
    ))
    .fetch_optional(db)
    .await?;
    Ok(row.map(map_row))
}

/// Insert a new row. If `form.is_default` is true, the existing
/// default (if any) is cleared in the same transaction so the
/// invariant "exactly one row has is_default = 1" stays
/// recoverable. Returns the new row's id.
pub async fn insert(db: &SqlitePool, form: DownloadClientForm<'_>) -> Result<i64, sqlx::Error> {
    let mut tx = db.begin().await?;
    if form.is_default {
        sqlx::query("UPDATE download_clients SET is_default = 0 WHERE is_default = 1")
            .execute(&mut *tx)
            .await?;
    }
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO download_clients
             (name, kind, url, username, password, label, download_path, enabled, is_default)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
         RETURNING id",
    )
    .bind(form.name.trim())
    .bind(form.kind)
    .bind(form.url.trim())
    .bind(form.username)
    .bind(form.password)
    .bind(form.label)
    .bind(form.download_path.trim().trim_end_matches('/'))
    .bind(if form.enabled { 1_i64 } else { 0_i64 })
    .bind(if form.is_default { 1_i64 } else { 0_i64 })
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(id)
}

pub async fn update(
    db: &SqlitePool,
    id: i64,
    form: DownloadClientForm<'_>,
) -> Result<(), sqlx::Error> {
    let mut tx = db.begin().await?;
    if form.is_default {
        sqlx::query("UPDATE download_clients SET is_default = 0 WHERE is_default = 1 AND id != ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query(
        "UPDATE download_clients
         SET name = ?, kind = ?, url = ?, username = ?, password = ?, label = ?,
             download_path = ?, enabled = ?, is_default = ?,
             updated_at = strftime('%s','now')
         WHERE id = ?",
    )
    .bind(form.name.trim())
    .bind(form.kind)
    .bind(form.url.trim())
    .bind(form.username)
    .bind(form.password)
    .bind(form.label)
    .bind(form.download_path.trim().trim_end_matches('/'))
    .bind(if form.enabled { 1_i64 } else { 0_i64 })
    .bind(if form.is_default { 1_i64 } else { 0_i64 })
    .bind(id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Delete the row and NULL out every dangling pin in one
/// transaction. Pins live on `indexers.download_client_id` and
/// `config.nyaa_download_client_id`. Without the NULL-out, FK-less
/// SQLite would leave dangling ids that resolve to None at routing
/// time (silent fall-through to default; surprising) and the row
/// would still appear in queries that join on the pin.
///
/// If the deleted row was the default, the caller is responsible
/// for picking a new default (or accepting "no default until the
/// user picks one"). Most user-facing flows just leave the
/// default empty and let the user pick from the remaining rows.
pub async fn delete(db: &SqlitePool, id: i64) -> Result<(), sqlx::Error> {
    let mut tx = db.begin().await?;
    sqlx::query("UPDATE indexers SET download_client_id = NULL WHERE download_client_id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "UPDATE config SET nyaa_download_client_id = NULL WHERE nyaa_download_client_id = ?",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM download_clients WHERE id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// Mark `id` as the default and clear every other row's flag.
/// Idempotent — calling on an already-default row is a no-op
/// at the value level (still does two UPDATEs that touch zero
/// rows).
pub async fn set_default(db: &SqlitePool, id: i64) -> Result<(), sqlx::Error> {
    let mut tx = db.begin().await?;
    sqlx::query("UPDATE download_clients SET is_default = 0 WHERE is_default = 1 AND id != ?")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "UPDATE download_clients SET is_default = 1, updated_at = strftime('%s','now') WHERE id = ?",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::in_memory_pool;

    fn form<'a>(name: &'a str, kind: &'a str, url: &'a str) -> DownloadClientForm<'a> {
        DownloadClientForm {
            name,
            kind,
            url,
            username: "",
            password: "",
            label: "",
            download_path: "",
            enabled: true,
            is_default: false,
        }
    }

    #[tokio::test]
    async fn insert_and_get_roundtrip() {
        let db = in_memory_pool().await;
        let id = insert(
            &db,
            form("Local qBit", "qbittorrent", "http://localhost:8080"),
        )
        .await
        .unwrap();
        let row = get_by_id(&db, id).await.unwrap().unwrap();
        assert_eq!(row.name, "Local qBit");
        assert_eq!(row.kind, "qbittorrent");
        assert_eq!(row.url, "http://localhost:8080");
        assert!(row.enabled);
        assert!(!row.is_default);
    }

    #[tokio::test]
    async fn insert_with_is_default_clears_prior_default() {
        let db = in_memory_pool().await;
        let mut f = form("First", "qbittorrent", "http://1");
        f.is_default = true;
        let first = insert(&db, f).await.unwrap();

        let mut f2 = form("Second", "deluge", "http://2");
        f2.is_default = true;
        let second = insert(&db, f2).await.unwrap();

        // Only `second` should still be default.
        let first_row = get_by_id(&db, first).await.unwrap().unwrap();
        let second_row = get_by_id(&db, second).await.unwrap().unwrap();
        assert!(!first_row.is_default, "first must lose its default flag");
        assert!(second_row.is_default);

        let default_row = get_default(&db).await.unwrap().unwrap();
        assert_eq!(default_row.id, second);
    }

    #[tokio::test]
    async fn set_default_is_idempotent_and_unique() {
        let db = in_memory_pool().await;
        let a = insert(&db, form("A", "qbittorrent", "http://a"))
            .await
            .unwrap();
        let b = insert(&db, form("B", "deluge", "http://b")).await.unwrap();

        set_default(&db, a).await.unwrap();
        set_default(&db, a).await.unwrap(); // idempotent
        let default_row = get_default(&db).await.unwrap().unwrap();
        assert_eq!(default_row.id, a);

        set_default(&db, b).await.unwrap();
        // Only one row has is_default = 1.
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM download_clients WHERE is_default = 1")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn delete_nulls_out_indexer_and_nyaa_pins() {
        let db = in_memory_pool().await;
        let id = insert(&db, form("X", "qbittorrent", "http://x"))
            .await
            .unwrap();

        // Create an indexer row pinned to this client.
        sqlx::query(
            "INSERT INTO indexers (name, kind, url, api_key, download_client_id) \
             VALUES ('AB', 'torznab', 'http://prowlarr/1/api', 'k', ?)",
        )
        .bind(id)
        .execute(&db)
        .await
        .unwrap();

        // Pin Nyaa to it as well (config row needs to exist first).
        sqlx::query("INSERT INTO config (id) VALUES (1)")
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("UPDATE config SET nyaa_download_client_id = ? WHERE id = 1")
            .bind(id)
            .execute(&db)
            .await
            .unwrap();

        delete(&db, id).await.unwrap();

        // Pin columns are NULL.
        let indexer_pin: Option<i64> =
            sqlx::query_scalar("SELECT download_client_id FROM indexers WHERE name = 'AB'")
                .fetch_one(&db)
                .await
                .unwrap();
        assert!(
            indexer_pin.is_none(),
            "indexer pin must be NULLed on delete"
        );

        let nyaa_pin: Option<i64> =
            sqlx::query_scalar("SELECT nyaa_download_client_id FROM config WHERE id = 1")
                .fetch_one(&db)
                .await
                .unwrap();
        assert!(nyaa_pin.is_none(), "Nyaa pin must be NULLed on delete");

        // Row itself is gone.
        assert!(get_by_id(&db, id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn list_enabled_filters_disabled_rows() {
        let db = in_memory_pool().await;
        insert(&db, form("On", "qbittorrent", "http://a"))
            .await
            .unwrap();
        let mut off = form("Off", "deluge", "http://b");
        off.enabled = false;
        insert(&db, off).await.unwrap();

        let enabled = list_enabled(&db).await.unwrap();
        assert_eq!(enabled.len(), 1);
        assert_eq!(enabled[0].name, "On");
    }

    #[tokio::test]
    async fn update_round_trip() {
        let db = in_memory_pool().await;
        let id = insert(&db, form("Initial", "qbittorrent", "http://1"))
            .await
            .unwrap();
        let mut f = form("Renamed", "deluge", "http://2");
        f.username = "u";
        f.password = "p";
        f.label = "ryokan";
        update(&db, id, f).await.unwrap();
        let row = get_by_id(&db, id).await.unwrap().unwrap();
        assert_eq!(row.name, "Renamed");
        assert_eq!(row.kind, "deluge");
        assert_eq!(row.url, "http://2");
        assert_eq!(row.username, "u");
        assert_eq!(row.password, "p");
        assert_eq!(row.label, "ryokan");
    }

    #[tokio::test]
    async fn list_all_orders_default_first_then_alphabetical() {
        let db = in_memory_pool().await;
        insert(&db, form("zeta", "qbittorrent", "http://z"))
            .await
            .unwrap();
        insert(&db, form("alpha", "deluge", "http://a"))
            .await
            .unwrap();
        let mut def = form("middle", "transmission", "http://m");
        def.is_default = true;
        insert(&db, def).await.unwrap();

        let rows = list_all(&db).await.unwrap();
        assert_eq!(rows[0].name, "middle"); // default first
        assert_eq!(rows[1].name, "alpha"); // then case-insensitive name
        assert_eq!(rows[2].name, "zeta");
    }
}
