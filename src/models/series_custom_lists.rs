//! AniList custom-list membership side table (issue #62 PR D).
//!
//! Sync writes one row per (series, provider, custom-list-name) on
//! every merge action. The detail page renders a badge row from
//! `list_for_series`; the library page filter dropdown reads
//! `distinct_list_names`.
//!
//! AL-only by data: MAL has no custom-list concept and the sync
//! engine's `entries_from_mal` path always returns an empty
//! `custom_lists` Vec. The schema's `provider` column exists as
//! future-proofing — if a provider with its own custom-list shape
//! ever appears, its memberships get their own namespace per row.
//!
//! Reconciliation strategy is replace-on-merge rather than
//! incremental upsert: AL's GraphQL response carries the FULL
//! membership map per entry (the `customLists: { name: bool }`
//! object), so doing a clear+insert per series is both simpler and
//! correct against the "user removed from a custom list" case.
//! Without the clear, a series the user moved out of "Hidden gems"
//! would keep its stale membership row forever.

use sqlx::SqlitePool;

/// One row of `series_custom_lists` projected for read paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomListMembership {
    pub series_id: i64,
    pub provider: String,
    pub list_name: String,
}

/// Replace the full custom-list membership set for `series_id` +
/// `provider` with the supplied `list_names`. Any existing row
/// scoped to the same (series, provider) that isn't in the new set
/// is dropped, and any name not yet present is inserted. Idempotent
/// — running twice with the same input is a no-op.
///
/// Empty `list_names` clears all memberships for this series under
/// this provider, which is the right behavior for "the user removed
/// the entry from every custom list" (or unrated their entries
/// entirely on AL).
pub async fn replace_for_series(
    db: &SqlitePool,
    series_id: i64,
    provider: &str,
    list_names: &[String],
) -> Result<(), sqlx::Error> {
    // Two-step inside a transaction so a concurrent reader never
    // sees a half-cleared membership set. The clear is scoped to
    // (series_id, provider) so a series synced from AL keeps any
    // hypothetical future MAL-side list memberships intact (today
    // MAL never writes any, but the schema supports it).
    let mut tx = db.begin().await?;

    sqlx::query("DELETE FROM series_custom_lists WHERE series_id = ? AND provider = ?")
        .bind(series_id)
        .bind(provider)
        .execute(&mut *tx)
        .await?;

    for name in list_names {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            continue;
        }
        // INSERT OR IGNORE because the input vector might contain
        // the same list name twice (defensive — AL's response
        // shouldn't, but a future schema change could).
        sqlx::query(
            "INSERT OR IGNORE INTO series_custom_lists (series_id, provider, list_name) \
             VALUES (?, ?, ?)",
        )
        .bind(series_id)
        .bind(provider)
        .bind(trimmed)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(())
}

/// Read every (series_id, provider, list_name) row for a given
/// series. Sorted by `list_name` so the rendered badge order is
/// stable regardless of insertion order.
pub async fn list_for_series(
    db: &SqlitePool,
    series_id: i64,
) -> Result<Vec<CustomListMembership>, sqlx::Error> {
    let rows = sqlx::query_as::<_, (i64, String, String)>(
        "SELECT series_id, provider, list_name FROM series_custom_lists \
         WHERE series_id = ? ORDER BY list_name",
    )
    .bind(series_id)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(sid, provider, list_name)| CustomListMembership {
            series_id: sid,
            provider,
            list_name,
        })
        .collect())
}

/// Distinct list names across the entire library, sorted
/// alphabetically. Powers the library-page filter dropdown — only
/// names that have at least one membership row appear, so an empty
/// dropdown means "no custom lists synced yet."
pub async fn distinct_list_names(db: &SqlitePool) -> Result<Vec<String>, sqlx::Error> {
    let rows = sqlx::query_scalar::<_, String>(
        "SELECT DISTINCT list_name FROM series_custom_lists ORDER BY list_name",
    )
    .fetch_all(db)
    .await?;
    Ok(rows)
}

/// Drop every membership row for the given provider. Used on
/// account unlink so the library filter dropdown stops showing list
/// names that came from an account the user no longer has linked
/// (and so a re-link to a different AL account starts from a clean
/// slate). Today's only producer is "anilist"; the schema's
/// `provider` column scopes the wipe so a hypothetical future
/// provider's memberships don't get cleared by another's unlink.
pub async fn clear_for_provider(db: &SqlitePool, provider: &str) -> Result<u64, sqlx::Error> {
    let result = sqlx::query("DELETE FROM series_custom_lists WHERE provider = ?")
        .bind(provider)
        .execute(db)
        .await?;
    Ok(result.rows_affected())
}

/// Series ids that belong to `list_name`. Used by the library
/// filter — handler reads this set, then filters the in-memory
/// `Vec<Series>` against it. Cheaper than re-querying `series` with
/// a join when the library is already loaded.
pub async fn series_ids_in_list(db: &SqlitePool, list_name: &str) -> Result<Vec<i64>, sqlx::Error> {
    let rows = sqlx::query_scalar::<_, i64>(
        "SELECT series_id FROM series_custom_lists WHERE list_name = ?",
    )
    .bind(list_name)
    .fetch_all(db)
    .await?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{in_memory_pool, seed_series};

    #[tokio::test]
    async fn replace_inserts_initial_memberships() {
        let db = in_memory_pool().await;
        let series_id = seed_series(&db, 100, "Show").await;

        replace_for_series(
            &db,
            series_id,
            "anilist",
            &["Hidden Gems".into(), "Rewatching".into()],
        )
        .await
        .unwrap();

        let rows = list_for_series(&db, series_id).await.unwrap();
        assert_eq!(rows.len(), 2);
        // Sorted alphabetically by list_name.
        assert_eq!(rows[0].list_name, "Hidden Gems");
        assert_eq!(rows[1].list_name, "Rewatching");
        assert_eq!(rows[0].provider, "anilist");
    }

    #[tokio::test]
    async fn replace_drops_removed_lists() {
        // The user moved a series out of "Hidden Gems" on AL and
        // into "Top 10" instead. The next sync's replace-on-merge
        // must drop the old membership; without this the stale row
        // would persist forever.
        let db = in_memory_pool().await;
        let series_id = seed_series(&db, 100, "Show").await;
        replace_for_series(&db, series_id, "anilist", &["Hidden Gems".into()])
            .await
            .unwrap();
        replace_for_series(&db, series_id, "anilist", &["Top 10".into()])
            .await
            .unwrap();

        let rows = list_for_series(&db, series_id).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].list_name, "Top 10");
    }

    #[tokio::test]
    async fn replace_with_empty_list_clears_memberships() {
        let db = in_memory_pool().await;
        let series_id = seed_series(&db, 100, "Show").await;
        replace_for_series(&db, series_id, "anilist", &["Hidden Gems".into()])
            .await
            .unwrap();
        replace_for_series(&db, series_id, "anilist", &[])
            .await
            .unwrap();

        let rows = list_for_series(&db, series_id).await.unwrap();
        assert!(rows.is_empty());
    }

    #[tokio::test]
    async fn replace_skips_blank_names() {
        // Defensive: AL shouldn't return blank custom-list keys, but
        // a future schema bug or a user-edited list name with only
        // whitespace shouldn't produce a useless empty row.
        let db = in_memory_pool().await;
        let series_id = seed_series(&db, 100, "Show").await;
        replace_for_series(
            &db,
            series_id,
            "anilist",
            &["".into(), "   ".into(), "Real List".into()],
        )
        .await
        .unwrap();

        let rows = list_for_series(&db, series_id).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].list_name, "Real List");
    }

    #[tokio::test]
    async fn replace_dedups_repeat_names() {
        let db = in_memory_pool().await;
        let series_id = seed_series(&db, 100, "Show").await;
        replace_for_series(
            &db,
            series_id,
            "anilist",
            &["Top 10".into(), "Top 10".into()],
        )
        .await
        .unwrap();

        let rows = list_for_series(&db, series_id).await.unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[tokio::test]
    async fn replace_per_provider_is_namespaced() {
        // Provider scope: replacing AL memberships must NOT touch a
        // hypothetical second-provider's rows. Guards against a
        // future provider being added and the AL replace silently
        // wiping the other side.
        let db = in_memory_pool().await;
        let series_id = seed_series(&db, 100, "Show").await;
        replace_for_series(&db, series_id, "anilist", &["AL List".into()])
            .await
            .unwrap();
        // Direct insert simulating a future provider.
        sqlx::query(
            "INSERT INTO series_custom_lists (series_id, provider, list_name) VALUES (?, ?, ?)",
        )
        .bind(series_id)
        .bind("future_provider")
        .bind("FP List")
        .execute(&db)
        .await
        .unwrap();

        // Replace AL only — should leave the future_provider row alone.
        replace_for_series(&db, series_id, "anilist", &["AL List 2".into()])
            .await
            .unwrap();

        let rows = list_for_series(&db, series_id).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert!(
            rows.iter()
                .any(|r| r.provider == "anilist" && r.list_name == "AL List 2")
        );
        assert!(rows.iter().any(|r| r.provider == "future_provider"));
    }

    #[tokio::test]
    async fn distinct_list_names_returns_sorted_unique() {
        let db = in_memory_pool().await;
        let s1 = seed_series(&db, 100, "Show 1").await;
        let s2 = seed_series(&db, 200, "Show 2").await;
        replace_for_series(&db, s1, "anilist", &["B".into(), "A".into()])
            .await
            .unwrap();
        replace_for_series(&db, s2, "anilist", &["A".into(), "C".into()])
            .await
            .unwrap();

        let names = distinct_list_names(&db).await.unwrap();
        assert_eq!(names, vec!["A", "B", "C"]);
    }

    #[tokio::test]
    async fn distinct_list_names_empty_when_nothing_synced() {
        let db = in_memory_pool().await;
        // No memberships → empty dropdown → library page hides the
        // filter control entirely (handler-side gate).
        assert!(distinct_list_names(&db).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn series_ids_in_list_returns_matching_rows() {
        let db = in_memory_pool().await;
        let s1 = seed_series(&db, 100, "Show 1").await;
        let s2 = seed_series(&db, 200, "Show 2").await;
        let s3 = seed_series(&db, 300, "Show 3").await;
        replace_for_series(&db, s1, "anilist", &["Top 10".into()])
            .await
            .unwrap();
        replace_for_series(&db, s2, "anilist", &["Hidden Gems".into()])
            .await
            .unwrap();
        replace_for_series(&db, s3, "anilist", &["Top 10".into()])
            .await
            .unwrap();

        let mut ids = series_ids_in_list(&db, "Top 10").await.unwrap();
        ids.sort();
        assert_eq!(ids, vec![s1, s3]);
    }

    #[tokio::test]
    async fn series_delete_cascades_to_custom_lists() {
        // The FK has ON DELETE CASCADE so dropping a series wipes
        // its membership rows without a hand-tracked cleanup.
        let db = in_memory_pool().await;
        let series_id = seed_series(&db, 100, "Show").await;
        replace_for_series(&db, series_id, "anilist", &["Top 10".into()])
            .await
            .unwrap();
        sqlx::query("DELETE FROM series WHERE id = ?")
            .bind(series_id)
            .execute(&db)
            .await
            .unwrap();
        let rows = list_for_series(&db, series_id).await.unwrap();
        assert!(rows.is_empty());
    }
}
