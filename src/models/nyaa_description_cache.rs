//! Cache of torrent description bodies scraped from Nyaa `/view/{id}` pages.
//!
//! Backs Layer 2 of the classification pipeline. When Layer 1 (filename) and
//! Layer 3 (release group) fail to produce a confident source decision, the
//! classifier falls through to fetching the full Nyaa listing and scanning
//! the description for structured source metadata. Those fetches are
//! rate-limited to one per second, so caching the extracted description body
//! keyed by `info_hash` keeps repeated classifications of the same torrent
//! (RSS polling, re-scoring, upgrade detection) off the network entirely.
//!
//! Rows are keyed on `info_hash` (content-addressed) so they remain valid
//! as long as the torrent exists. The cache has no functional TTL, but the
//! hourly cleanup task calls [`cleanup`] to prune rows whose `cached_at` is
//! older than 90 days. `cached_at` is only refreshed on a live fetch (cache
//! miss path); cache hits leave it alone, so the sweep evicts any row that
//! hasn't triggered a fresh network fetch in 90 days. The consequence is a
//! single forced re-fetch on the next access, never data loss — the worst
//! case is that one Nyaa request gets re-paid for a torrent that's still
//! being actively re-classified from the cache.

use sqlx::{Row, SqlitePool};

pub async fn migrate(db: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS nyaa_description_cache (
            info_hash   TEXT PRIMARY KEY COLLATE NOCASE,
            description TEXT NOT NULL DEFAULT '',
            cached_at   DATETIME DEFAULT CURRENT_TIMESTAMP
        )
        "#,
    )
    .execute(db)
    .await?;
    Ok(())
}

/// Look up a cached description body by torrent info_hash. Returns `None`
/// when there's no row or the DB lookup fails — Layer 2 degrades to a live
/// fetch in either case.
pub async fn get(db: &SqlitePool, info_hash: &str) -> Option<String> {
    let trimmed = info_hash.trim();
    if trimmed.is_empty() {
        return None;
    }
    let row = sqlx::query("SELECT description FROM nyaa_description_cache WHERE info_hash = ?")
        .bind(trimmed)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()?;
    Some(row.get::<String, _>("description"))
}

/// Insert or replace a cached description body. Errors are ignored — cache
/// writes should never break the classification path, and the next lookup
/// will simply miss and trigger another fetch.
pub async fn upsert(db: &SqlitePool, info_hash: &str, description: &str) {
    let trimmed = info_hash.trim();
    if trimmed.is_empty() {
        return;
    }
    let _ = sqlx::query(
        r#"INSERT INTO nyaa_description_cache (info_hash, description)
           VALUES (?, ?)
           ON CONFLICT(info_hash) DO UPDATE SET
               description = excluded.description,
               cached_at = CURRENT_TIMESTAMP"#,
    )
    .bind(trimmed)
    .bind(description)
    .execute(db)
    .await;
}

/// Prune cache rows whose `cached_at` is older than `max_age_days`. Called
/// from the hourly cleanup task so long-running instances don't accumulate
/// description bodies for torrents that haven't been touched in months.
/// Active torrents (RSS sync re-hits them) get their `cached_at` refreshed
/// by `upsert`, so this only evicts rows that truly went cold.
pub async fn cleanup(db: &SqlitePool, max_age_days: i32) -> Result<u64, sqlx::Error> {
    let cutoff = format!("-{} days", max_age_days);
    let res =
        sqlx::query("DELETE FROM nyaa_description_cache WHERE cached_at < datetime('now', ?)")
            .bind(cutoff)
            .execute(db)
            .await?;
    Ok(res.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::in_memory_pool;

    /// Sentinel info_hash matching the BT v1 hex shape Layer 2's
    /// description fetcher actually feeds the cache. Mixed-case to
    /// double as the case-insensitivity fixture below.
    const HASH_A: &str = "AbCdEf0123456789aBcDeF0123456789aBcDeF01";
    const HASH_B: &str = "1111111111111111111111111111111111111111";

    #[tokio::test]
    async fn get_returns_none_for_missing_hash() {
        let db = in_memory_pool().await;
        assert!(get(&db, HASH_A).await.is_none());
    }

    #[tokio::test]
    async fn get_returns_none_for_empty_hash() {
        // Empty / whitespace-only hash short-circuits before the SQL
        // round-trip — the cache is content-addressed and an empty
        // key is meaningless. Pinned because the early return is
        // also what makes Layer 2's "info_hash unknown" code path
        // (e.g. RSS items where hash extraction failed) degrade
        // gracefully to a live fetch instead of returning whatever
        // the empty-key row happens to contain.
        let db = in_memory_pool().await;
        assert!(get(&db, "").await.is_none());
        assert!(get(&db, "   ").await.is_none());
    }

    #[tokio::test]
    async fn upsert_inserts_then_get_round_trips_the_body() {
        let db = in_memory_pool().await;
        upsert(&db, HASH_A, "Source: Blu-ray\nGroup: VCB-Studio").await;
        let got = get(&db, HASH_A).await.expect("row must be present");
        assert_eq!(got, "Source: Blu-ray\nGroup: VCB-Studio");
    }

    #[tokio::test]
    async fn upsert_replaces_on_conflict() {
        // The ON CONFLICT clause is load-bearing — Layer 2 re-fetches
        // the same `view_url` whenever the description is materially
        // updated by a release group (re-uploaded torrent, post-edit
        // tags). A failed upsert that left the stale body in place
        // would silently feed the classifier outdated source-tag
        // text. Pin both that the latest write wins AND that the
        // primary key is preserved (no second row).
        let db = in_memory_pool().await;
        upsert(&db, HASH_A, "first").await;
        upsert(&db, HASH_A, "second").await;
        let got = get(&db, HASH_A).await.expect("row must be present");
        assert_eq!(got, "second");

        // Confirm there's exactly one row for this hash — a missing
        // ON CONFLICT clause would have produced a primary-key
        // violation OR (with INSERT OR IGNORE) silently kept the
        // stale value, both worth catching.
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM nyaa_description_cache WHERE info_hash = ?")
                .bind(HASH_A)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn upsert_no_ops_on_empty_hash() {
        // Mirrors `get`'s early return — an empty key shouldn't
        // pollute the cache with a "" → body row that any other
        // empty-key Layer-2 lookup would then mistakenly hit.
        let db = in_memory_pool().await;
        upsert(&db, "", "would be a phantom row").await;
        upsert(&db, "   ", "also phantom").await;
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM nyaa_description_cache")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn lookup_is_case_insensitive() {
        // The schema declares `info_hash TEXT PRIMARY KEY COLLATE
        // NOCASE`. Layer 2's description fetcher feeds whatever
        // case the upstream source supplied; in practice Nyaa's
        // magnet URI hashes are uppercase but RSS-feed-derived
        // hashes can come through lowercase. Without NOCASE the
        // same torrent would be cached twice and the second-form
        // lookup would miss. Pin against case-flip on lookup.
        let db = in_memory_pool().await;
        upsert(&db, HASH_A, "the body").await;
        let lower = HASH_A.to_lowercase();
        let upper = HASH_A.to_uppercase();
        assert_eq!(get(&db, &lower).await.as_deref(), Some("the body"));
        assert_eq!(get(&db, &upper).await.as_deref(), Some("the body"));
        // And the PK collation must coalesce the case-variants on
        // upsert: writing under one case must update, not insert.
        upsert(&db, &lower, "updated under lowercase").await;
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM nyaa_description_cache")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(count, 1);
        assert_eq!(
            get(&db, &upper).await.as_deref(),
            Some("updated under lowercase")
        );
    }

    #[tokio::test]
    async fn cleanup_evicts_rows_older_than_cutoff() {
        // Backdate one row past the 90-day default cutoff and leave
        // a fresh one alone. The hourly cleanup task pins this
        // schedule; if the WHERE clause silently broke (wrong cutoff
        // arithmetic, dropped placeholder binding), every row would
        // either survive forever (cache bloat) or evict immediately
        // on the next sweep (cache thrash). Both fail this test.
        let db = in_memory_pool().await;
        upsert(&db, HASH_A, "stale").await;
        upsert(&db, HASH_B, "fresh").await;

        // Explicit backdate — `cached_at` defaults to CURRENT_TIMESTAMP
        // on insert so we have to manually move it backward to
        // simulate an aged row without a sleep.
        sqlx::query(
            "UPDATE nyaa_description_cache SET cached_at = datetime('now', '-100 days') \
             WHERE info_hash = ?",
        )
        .bind(HASH_A)
        .execute(&db)
        .await
        .unwrap();

        let removed = cleanup(&db, 90).await.unwrap();
        assert_eq!(removed, 1, "one stale row must be evicted; got {removed}");
        assert!(get(&db, HASH_A).await.is_none());
        assert_eq!(get(&db, HASH_B).await.as_deref(), Some("fresh"));
    }

    #[tokio::test]
    async fn cleanup_is_a_noop_when_nothing_is_stale() {
        // The hourly sweep runs unconditionally — a no-op call must
        // return 0 cleanly (not error, not nuke fresh rows).
        let db = in_memory_pool().await;
        upsert(&db, HASH_A, "fresh").await;
        let removed = cleanup(&db, 90).await.unwrap();
        assert_eq!(removed, 0);
        assert_eq!(get(&db, HASH_A).await.as_deref(), Some("fresh"));
    }

    #[tokio::test]
    async fn upsert_refreshes_cached_at_on_conflict() {
        // The doc-comment on `cleanup` explicitly says active
        // torrents stay alive because `upsert` refreshes
        // `cached_at`. Pin that contract: backdate a row past the
        // cutoff, upsert it, then run cleanup — the row must
        // survive because the upsert set `cached_at = CURRENT_TIMESTAMP`.
        // Without this guarantee, an actively-classified torrent
        // would still get evicted on the 90-day sweep.
        let db = in_memory_pool().await;
        upsert(&db, HASH_A, "v1").await;
        sqlx::query(
            "UPDATE nyaa_description_cache SET cached_at = datetime('now', '-100 days') \
             WHERE info_hash = ?",
        )
        .bind(HASH_A)
        .execute(&db)
        .await
        .unwrap();
        // Re-upsert — should bump `cached_at` back to now.
        upsert(&db, HASH_A, "v2").await;
        let removed = cleanup(&db, 90).await.unwrap();
        assert_eq!(
            removed, 0,
            "upsert-refreshed row must NOT be evicted by the 90-day sweep"
        );
        assert_eq!(get(&db, HASH_A).await.as_deref(), Some("v2"));
    }
}
