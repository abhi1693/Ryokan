//! Local cache of AniList airing schedules.
//!
//! Each row is one upcoming-or-recent episode for a Ryokan series,
//! stamped from AL's `Page.airingSchedules` query. The
//! `services::airing_refresh` supervised task writes these every
//! 12h; the `services::calendar` reader joins them against `series`
//! to render the in-app calendar + the iCal feed without round-
//! tripping to AL per-request.
//!
//! Schema lives in `models/migrations/mod.rs` (the table + the
//! `idx_episode_airings_at` range-scan index). FK on `series_id`
//! is `ON DELETE CASCADE` so deleting a series automatically purges
//! its stamped airings.
//!
//! ## Why a local table
//!
//! Sonarr's `Episode.AirDateUtc` shape: refresh once, serve from
//! the DB forever (until the next refresh tick). Saves the per-
//! request AL cost — load-bearing because AL's degraded
//! airingSchedules budget is 30/min, easy for a popping-off iCal
//! poller fleet to drain.

use sqlx::SqlitePool;

/// One upserted row. Mirrors the columns 1:1.
#[derive(Debug, Clone)]
pub struct EpisodeAiring {
    pub series_id: i64,
    pub episode: i32,
    pub airing_at: i64,
    pub duration_minutes: i32,
}

/// Upsert a batch of airings for a single series. Caller is
/// responsible for chunking by series — this lets us run one
/// transaction per series so a partial-failure on series N+1 leaves
/// series 1..N already-stamped rows intact.
///
/// On conflict (series_id, episode) the existing row is overwritten
/// — AL is the source of truth and the new row's `airing_at` /
/// `duration_minutes` may differ (a series shifted its air date).
pub async fn upsert_for_series(
    db: &SqlitePool,
    series_id: i64,
    rows: &[EpisodeAiring],
) -> Result<(), sqlx::Error> {
    if rows.is_empty() {
        return Ok(());
    }
    let mut tx = db.begin().await?;
    for r in rows {
        sqlx::query(
            "INSERT INTO episode_airings \
                (series_id, episode, airing_at, duration_minutes, refreshed_at) \
             VALUES (?, ?, ?, ?, strftime('%s','now')) \
             ON CONFLICT(series_id, episode) DO UPDATE SET \
                airing_at = excluded.airing_at, \
                duration_minutes = excluded.duration_minutes, \
                refreshed_at = excluded.refreshed_at",
        )
        .bind(series_id)
        .bind(r.episode)
        .bind(r.airing_at)
        .bind(r.duration_minutes)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await
}

/// Drop airings whose `airing_at` is more than `older_than_secs`
/// seconds in the past. Called from the refresh task to keep the
/// table from growing unbounded — there's no value in retaining
/// last-month's episodes once the calendar's "this week" / "this
/// month" windows have moved past them.
pub async fn prune_older_than(db: &SqlitePool, older_than_unix: i64) -> Result<u64, sqlx::Error> {
    let result = sqlx::query("DELETE FROM episode_airings WHERE airing_at < ?")
        .bind(older_than_unix)
        .execute(db)
        .await?;
    Ok(result.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> SqlitePool {
        crate::test_support::in_memory_pool().await
    }

    async fn add_series(pool: &SqlitePool, anilist_id: i64, title: &str) -> i64 {
        sqlx::query_scalar::<_, i64>(
            "INSERT INTO series (anilist_id, title, monitor_mode) \
             VALUES (?, ?, 'all') RETURNING id",
        )
        .bind(anilist_id)
        .bind(title)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn upsert_inserts_then_overwrites_on_conflict() {
        let pool = pool().await;
        let series_id = add_series(&pool, 1, "Test").await;

        let rows = vec![EpisodeAiring {
            series_id,
            episode: 1,
            airing_at: 1_700_000_000,
            duration_minutes: 24,
        }];
        upsert_for_series(&pool, series_id, &rows).await.unwrap();

        // Same (series_id, episode) but a shifted airing_at — the
        // upsert must overwrite, not duplicate or fail.
        let rows = vec![EpisodeAiring {
            series_id,
            episode: 1,
            airing_at: 1_700_000_000 + 86_400,
            duration_minutes: 25,
        }];
        upsert_for_series(&pool, series_id, &rows).await.unwrap();

        let row: (i64, i64) = sqlx::query_as(
            "SELECT airing_at, duration_minutes FROM episode_airings \
             WHERE series_id = ? AND episode = ?",
        )
        .bind(series_id)
        .bind(1)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.0, 1_700_000_000 + 86_400);
        assert_eq!(row.1, 25);

        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM episode_airings WHERE series_id = ?")
                .bind(series_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn series_delete_cascades_to_airings() {
        let pool = pool().await;
        // FK enforcement is opt-in per-connection in SQLite.
        sqlx::query("PRAGMA foreign_keys = ON")
            .execute(&pool)
            .await
            .unwrap();
        let series_id = add_series(&pool, 1, "Test").await;
        upsert_for_series(
            &pool,
            series_id,
            &[EpisodeAiring {
                series_id,
                episode: 1,
                airing_at: 1_700_000_000,
                duration_minutes: 24,
            }],
        )
        .await
        .unwrap();

        sqlx::query("DELETE FROM series WHERE id = ?")
            .bind(series_id)
            .execute(&pool)
            .await
            .unwrap();

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM episode_airings")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0, "FK cascade should clear airings");
    }

    #[tokio::test]
    async fn prune_drops_only_past_rows() {
        let pool = pool().await;
        let series_id = add_series(&pool, 1, "Test").await;
        upsert_for_series(
            &pool,
            series_id,
            &[
                EpisodeAiring {
                    series_id,
                    episode: 1,
                    airing_at: 1_000,
                    duration_minutes: 24,
                },
                EpisodeAiring {
                    series_id,
                    episode: 2,
                    airing_at: 2_000,
                    duration_minutes: 24,
                },
                EpisodeAiring {
                    series_id,
                    episode: 3,
                    airing_at: 3_000,
                    duration_minutes: 24,
                },
            ],
        )
        .await
        .unwrap();

        let dropped = prune_older_than(&pool, 2_500).await.unwrap();
        assert_eq!(dropped, 2);
        let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM episode_airings")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(remaining, 1);
    }
}
