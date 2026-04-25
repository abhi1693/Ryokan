//! Genre side table (issue #62 PR E).
//!
//! AnimeDetail's `genres: Vec<String>` is the canonical source. This
//! table extracts that into per-row form so the library filter +
//! the per-series detail page (in the future) can query without
//! unmarshalling the cached JSON. Populated on every metadata
//! refresh + every successful sync merge, plus a one-shot backfill
//! at startup against `series_metadata_cache` so existing libraries
//! light up immediately.
//!
//! Provider-agnostic — both AL and Jikan emit the same genre
//! vocabulary, so unlike `series_custom_lists` this table doesn't
//! carry a `provider` column. Replace-on-write is again the right
//! shape: a metadata refresh might add or remove genres for a
//! series whose AL classification changed, and an upsert path
//! would leak stale rows.
//!
//! **Read paths currently have no consumer.** [`distinct_genres`]
//! and [`series_ids_in_genre`] were originally written for a genre
//! dropdown on the library page; that filter was replaced by the
//! full-text library search before #62 PR E shipped. The write
//! paths + the one-shot backfill are kept on so the table stays
//! warm against a future detail-page genre row or an advanced
//! filter. Don't delete them assuming they're orphaned — check the
//! roadmap first.

use sqlx::SqlitePool;

/// Replace this series's genre rows with the supplied list. Empty
/// `genres` clears every row (e.g. AnimeDetail's genre field came
/// back empty for whatever reason — the library filter shouldn't
/// keep showing stale genre rows from a prior cache).
///
/// Two-step inside a transaction so a concurrent reader never sees
/// a half-cleared set.
pub async fn replace_for_series(
    db: &SqlitePool,
    series_id: i64,
    genres: &[String],
) -> Result<(), sqlx::Error> {
    let mut tx = db.begin().await?;

    sqlx::query("DELETE FROM series_genres WHERE series_id = ?")
        .bind(series_id)
        .execute(&mut *tx)
        .await?;

    for raw in genres {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        // INSERT OR IGNORE because AL has been observed to return a
        // genre list with case-insensitive duplicates ("Action",
        // "action") on community-edited entries; the PK collapses
        // them rather than failing the merge.
        sqlx::query("INSERT OR IGNORE INTO series_genres (series_id, genre) VALUES (?, ?)")
            .bind(series_id)
            .bind(trimmed)
            .execute(&mut *tx)
            .await?;
    }

    tx.commit().await?;
    Ok(())
}

/// Read every genre row for `series_id`. Sorted alphabetically so
/// any future detail-page render is stable across loads.
pub async fn list_for_series(db: &SqlitePool, series_id: i64) -> Result<Vec<String>, sqlx::Error> {
    let rows = sqlx::query_scalar::<_, String>(
        "SELECT genre FROM series_genres WHERE series_id = ? ORDER BY genre",
    )
    .bind(series_id)
    .fetch_all(db)
    .await?;
    Ok(rows)
}

/// Distinct genre names across the whole library, sorted
/// alphabetically. Powers the library filter's `<datalist>`
/// autocomplete + the dropdown when one is shown.
pub async fn distinct_genres(db: &SqlitePool) -> Result<Vec<String>, sqlx::Error> {
    let rows =
        sqlx::query_scalar::<_, String>("SELECT DISTINCT genre FROM series_genres ORDER BY genre")
            .fetch_all(db)
            .await?;
    Ok(rows)
}

/// Series ids tagged with `genre`. Used by the library handler's
/// in-memory filter step — same shape as the custom-list filter so
/// both controls compose cleanly.
pub async fn series_ids_in_genre(db: &SqlitePool, genre: &str) -> Result<Vec<i64>, sqlx::Error> {
    let rows = sqlx::query_scalar::<_, i64>("SELECT series_id FROM series_genres WHERE genre = ?")
        .bind(genre)
        .fetch_all(db)
        .await?;
    Ok(rows)
}

/// Stable ID for the one-shot genre-backfill migration. The table
/// is empty for any DB created before #62 PR E even though the
/// canonical genre data has been sitting in `series_metadata_cache`
/// the whole time. This pre-populates the side table from the
/// existing cache so the library filter dropdown lights up
/// immediately on first boot after upgrade — without it the
/// dropdown would stay empty until the user triggers a metadata
/// refresh or watch-list sync, which can be days away on default
/// cadences.
const BACKFILL_MIGRATION_ID: &str = "series_genres_backfill_from_cache_v1";

/// Idempotent backfill: parses every cached AnimeDetail's
/// `genres` field and writes one `series_genres` row per (series,
/// genre). Skips on subsequent boots via the `schema_migrations`
/// ledger row written at the end. Non-fatal — a parse failure on
/// one cache row logs and skips that row rather than aborting the
/// migration.
pub async fn backfill_from_metadata_cache_once(db: &SqlitePool) -> Result<(), String> {
    use crate::models::group_source_map::{
        ensure_schema_migrations_table, migration_already_applied,
    };

    ensure_schema_migrations_table(db)
        .await
        .map_err(|e| format!("schema_migrations create: {e}"))?;

    if migration_already_applied(db, BACKFILL_MIGRATION_ID)
        .await
        .map_err(|e| format!("schema_migrations probe: {e}"))?
    {
        return Ok(());
    }

    let rows = sqlx::query_as::<_, (i64, String)>(
        "SELECT series_id, detail_json FROM series_metadata_cache",
    )
    .fetch_all(db)
    .await
    .map_err(|e| format!("metadata_cache scan: {e}"))?;

    let mut updated = 0_usize;
    for (series_id, detail_json) in rows {
        let parsed: serde_json::Value = match serde_json::from_str(&detail_json) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    "series_genres backfill: skipping series_id={series_id} (parse error: {e})"
                );
                continue;
            }
        };
        let genres: Vec<String> = parsed
            .get("genres")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|g| g.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        if genres.is_empty() {
            continue;
        }
        if let Err(e) = replace_for_series(db, series_id, &genres).await {
            tracing::warn!("series_genres backfill: replace failed for series_id={series_id}: {e}");
            continue;
        }
        updated += 1;
    }

    sqlx::query("INSERT OR IGNORE INTO schema_migrations (id) VALUES (?)")
        .bind(BACKFILL_MIGRATION_ID)
        .execute(db)
        .await
        .map_err(|e| format!("schema_migrations record: {e}"))?;

    tracing::info!(
        "series_genres: backfilled genres for {updated} existing series from metadata_cache"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{in_memory_pool, seed_series};

    #[tokio::test]
    async fn replace_inserts_initial_genres() {
        let db = in_memory_pool().await;
        let id = seed_series(&db, 100, "Show").await;
        replace_for_series(&db, id, &["Action".into(), "Comedy".into()])
            .await
            .unwrap();

        let got = list_for_series(&db, id).await.unwrap();
        assert_eq!(got, vec!["Action", "Comedy"]);
    }

    #[tokio::test]
    async fn replace_drops_removed_genres() {
        // Series got reclassified on AL — used to be Romance, now
        // listed as Drama. The replace-on-write must drop the old
        // row; an upsert path would leak it.
        let db = in_memory_pool().await;
        let id = seed_series(&db, 100, "Show").await;
        replace_for_series(&db, id, &["Romance".into()])
            .await
            .unwrap();
        replace_for_series(&db, id, &["Drama".into()])
            .await
            .unwrap();
        assert_eq!(list_for_series(&db, id).await.unwrap(), vec!["Drama"]);
    }

    #[tokio::test]
    async fn replace_with_empty_clears() {
        let db = in_memory_pool().await;
        let id = seed_series(&db, 100, "Show").await;
        replace_for_series(&db, id, &["Action".into()])
            .await
            .unwrap();
        replace_for_series(&db, id, &[]).await.unwrap();
        assert!(list_for_series(&db, id).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn replace_skips_blank_and_dedups() {
        // Defensive: AL has emitted case-variants on community-edited
        // entries; the PK collapses them via INSERT OR IGNORE. Blank
        // / whitespace-only entries are dropped to avoid a useless
        // empty-genre row.
        let db = in_memory_pool().await;
        let id = seed_series(&db, 100, "Show").await;
        replace_for_series(
            &db,
            id,
            &[
                "".into(),
                "  ".into(),
                "Action".into(),
                "Action".into(),
                "Comedy".into(),
            ],
        )
        .await
        .unwrap();
        let got = list_for_series(&db, id).await.unwrap();
        assert_eq!(got, vec!["Action", "Comedy"]);
    }

    #[tokio::test]
    async fn distinct_genres_returns_sorted_unique() {
        let db = in_memory_pool().await;
        let s1 = seed_series(&db, 100, "Show 1").await;
        let s2 = seed_series(&db, 200, "Show 2").await;
        replace_for_series(&db, s1, &["Romance".into(), "Action".into()])
            .await
            .unwrap();
        replace_for_series(&db, s2, &["Comedy".into(), "Action".into()])
            .await
            .unwrap();
        assert_eq!(
            distinct_genres(&db).await.unwrap(),
            vec!["Action", "Comedy", "Romance"]
        );
    }

    #[tokio::test]
    async fn series_ids_in_genre_matches_rows() {
        let db = in_memory_pool().await;
        let s1 = seed_series(&db, 100, "Show 1").await;
        let s2 = seed_series(&db, 200, "Show 2").await;
        let s3 = seed_series(&db, 300, "Show 3").await;
        replace_for_series(&db, s1, &["Action".into()])
            .await
            .unwrap();
        replace_for_series(&db, s2, &["Comedy".into()])
            .await
            .unwrap();
        replace_for_series(&db, s3, &["Action".into(), "Comedy".into()])
            .await
            .unwrap();

        let mut action = series_ids_in_genre(&db, "Action").await.unwrap();
        action.sort();
        assert_eq!(action, vec![s1, s3]);
    }

    #[tokio::test]
    async fn series_delete_cascades_to_genres() {
        let db = in_memory_pool().await;
        let id = seed_series(&db, 100, "Show").await;
        replace_for_series(&db, id, &["Action".into()])
            .await
            .unwrap();
        sqlx::query("DELETE FROM series WHERE id = ?")
            .bind(id)
            .execute(&db)
            .await
            .unwrap();
        assert!(list_for_series(&db, id).await.unwrap().is_empty());
    }

    /// Helper: insert a fake metadata_cache row with a synthesized
    /// AnimeDetail JSON carrying the given genres. Mirrors the
    /// shape `metadata_cache::upsert` writes (just enough fields
    /// for the backfill's `genres` array extraction).
    async fn seed_metadata_cache_with_genres(
        db: &SqlitePool,
        series_id: i64,
        anilist_id: i64,
        genres: &[&str],
    ) {
        let detail = serde_json::json!({
            "id": anilist_id,
            "id_mal": null,
            "title_romaji": "",
            "title_english": "",
            "title_native": "",
            "cover_url": "",
            "banner_url": "",
            "format": "TV",
            "status": "FINISHED",
            "status_display": "Finished",
            "episodes": null,
            "duration": null,
            "season": "",
            "season_year": null,
            "description": "",
            "genres": genres,
            "average_score": null,
            "average_score_display": null,
            "score_is_ten_point": false,
            "score_class": "",
            "next_airing_episode": null,
            "next_airing_at": null,
            "synonyms": [],
            "streaming_episodes": [],
            "relations": []
        });
        sqlx::query(
            "INSERT INTO series_metadata_cache (series_id, provider_id, mal_id, detail_json) \
             VALUES (?, ?, NULL, ?)",
        )
        .bind(series_id)
        .bind(anilist_id)
        .bind(detail.to_string())
        .execute(db)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn backfill_populates_from_existing_cache_rows() {
        let db = in_memory_pool().await;
        let s1 = seed_series(&db, 100, "Show 1").await;
        let s2 = seed_series(&db, 200, "Show 2").await;
        seed_metadata_cache_with_genres(&db, s1, 100, &["Action", "Comedy"]).await;
        seed_metadata_cache_with_genres(&db, s2, 200, &["Romance"]).await;

        backfill_from_metadata_cache_once(&db).await.unwrap();

        assert_eq!(
            list_for_series(&db, s1).await.unwrap(),
            vec!["Action", "Comedy"]
        );
        assert_eq!(list_for_series(&db, s2).await.unwrap(), vec!["Romance"]);
    }

    #[tokio::test]
    async fn backfill_is_idempotent_via_ledger() {
        // Second invocation must NOT touch the table (would clobber
        // any post-backfill writes from a metadata refresh that
        // happened between the first and second call). The ledger
        // row written at the end of the first call short-circuits.
        let db = in_memory_pool().await;
        let id = seed_series(&db, 100, "Show").await;
        seed_metadata_cache_with_genres(&db, id, 100, &["Action"]).await;
        backfill_from_metadata_cache_once(&db).await.unwrap();

        // Simulate a metadata refresh that changed the genre.
        replace_for_series(&db, id, &["Drama".into()])
            .await
            .unwrap();

        // Second backfill run — the ledger should short-circuit
        // before the DELETE-then-INSERT, leaving "Drama" in place.
        backfill_from_metadata_cache_once(&db).await.unwrap();
        assert_eq!(list_for_series(&db, id).await.unwrap(), vec!["Drama"]);
    }

    #[tokio::test]
    async fn backfill_skips_rows_with_corrupt_json() {
        // A garbled detail_json shouldn't abort the migration —
        // unaffected series should still get their genres written.
        let db = in_memory_pool().await;
        let s1 = seed_series(&db, 100, "Bad").await;
        let s2 = seed_series(&db, 200, "Good").await;
        sqlx::query(
            "INSERT INTO series_metadata_cache (series_id, provider_id, mal_id, detail_json) \
             VALUES (?, ?, NULL, ?)",
        )
        .bind(s1)
        .bind(100_i64)
        .bind("not json")
        .execute(&db)
        .await
        .unwrap();
        seed_metadata_cache_with_genres(&db, s2, 200, &["Action"]).await;

        backfill_from_metadata_cache_once(&db).await.unwrap();

        assert!(list_for_series(&db, s1).await.unwrap().is_empty());
        assert_eq!(list_for_series(&db, s2).await.unwrap(), vec!["Action"]);
    }
}
