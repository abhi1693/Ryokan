use sqlx::{Row, SqlitePool};

use crate::services::source::ClassificationResult;

/// One entry in the cross-series "needs review" list. Carries just enough
/// to render a row: series identity for the link, episode number, and the
/// current (uncertain) classification for context. Produced by
/// [`get_needs_review`].
#[derive(Debug, Clone, serde::Serialize)]
pub struct NeedsReviewEntry {
    pub series_id: i64,
    pub series_anilist_id: i64,
    pub series_title: String,
    pub cover_url: String,
    pub episode_number: i32,
    pub quality_tag: String,
    pub release_title: String,
    pub release_group: String,
    pub source: String,
    pub resolution: String,
    pub classification_confidence: f32,
}

/// Return every episode currently flagged `needs_review = true` across the
/// entire library, joined with its series info. Used by the Phase 4
/// "Needs review" list view. Excludes rows the user has already manually
/// overridden (manual_override = 1 clears `needs_review` too, but we
/// filter defensively in case an older row has both set).
pub async fn get_needs_review(
    db: &SqlitePool,
) -> Result<Vec<NeedsReviewEntry>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT t.series_id, t.episode_number, t.quality_tag, t.release_title, t.release_group,
                t.source, t.resolution, t.classification_confidence,
                s.anilist_id AS series_anilist_id,
                COALESCE(NULLIF(s.title_english, ''), NULLIF(s.title_romaji, ''), s.title) AS series_title,
                s.cover_url
         FROM episode_quality_tags t
         JOIN series s ON s.id = t.series_id
         WHERE t.needs_review = 1
           AND COALESCE(t.manual_override, 0) = 0
         ORDER BY s.title_english, t.episode_number",
    )
    .fetch_all(db)
    .await?;

    Ok(rows
        .iter()
        .map(|row| {
            let confidence: f64 = row.get("classification_confidence");
            NeedsReviewEntry {
                series_id: row.get("series_id"),
                series_anilist_id: row.get("series_anilist_id"),
                series_title: row.get("series_title"),
                cover_url: row.get("cover_url"),
                episode_number: row.get("episode_number"),
                quality_tag: row.get("quality_tag"),
                release_title: row.get("release_title"),
                release_group: row.get("release_group"),
                source: row.get("source"),
                resolution: row.get("resolution"),
                classification_confidence: confidence as f32,
            }
        })
        .collect())
}

#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct GrabHistoryEntry {
    pub id: i64,
    pub quality_tag: String,
    pub release_title: String,
    pub release_group: String,
    pub grabbed_at: String,
    pub state: String,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct EpisodeQualityTag {
    pub quality_tag: String,
    pub release_title: String,
    pub release_group: String,
    pub state: String,
    /// Structured source label ("BluRay", "Web", "DVD", "HDTV", "TV",
    /// "Unknown", or empty for rows grabbed before Phase 1b).
    pub source: String,
    /// Structured resolution label ("1080p", "720p", …, or empty).
    pub resolution: String,
    pub is_remux: bool,
    /// Sonarr-parity: true when the release is a raw BDMV / BD-Raw
    /// disc-structure release (distinct from `is_remux`). Mutually
    /// exclusive with `is_remux` at the label level.
    pub is_bdmv: bool,
    /// Sonarr-parity: WEB-DL vs WEBRip sub-classification when the
    /// filename was specific enough to tell. Empty string for legacy
    /// bare-WEB rows or non-Web sources.
    pub web_kind: String,
    pub classification_confidence: f32,
    pub needs_review: bool,
    /// True when the user has pinned this classification via the manual
    /// override picker. Prevents `update_classification` from overwriting
    /// on subsequent post-download re-classifies.
    pub manual_override: bool,
}

/// Record a new grab for an episode — inserts into history and upserts the
/// current tag.
///
/// The legacy `quality_tag` column is populated from `classification.label()`
/// so existing read paths continue to work; the new structured columns
/// (source, resolution, is_remux, classification_confidence, needs_review)
/// are written alongside it on `episode_quality_tags`. `manual_override` is
/// intentionally preserved across re-grabs — a user-set override should
/// stick even when a newer grab comes in.
pub async fn record_grab(
    db: &SqlitePool,
    series_id: i64,
    episode_number: i32,
    classification: &ClassificationResult,
    release_title: &str,
    release_group: &str,
) -> Result<i64, sqlx::Error> {
    let quality_tag = classification.label();
    let source_str = classification.source.as_str();
    let resolution_str = classification.resolution.as_str();
    let is_remux = if classification.is_remux { 1_i64 } else { 0_i64 };
    let is_bdmv = if classification.is_bdmv { 1_i64 } else { 0_i64 };
    let web_kind_str = classification.web_kind.as_str();
    let confidence = classification.confidence as f64;
    let needs_review = if classification.needs_review { 1_i64 } else { 0_i64 };

    let history_id: i64 = sqlx::query_scalar(
        "INSERT INTO episode_grab_history (series_id, episode_number, quality_tag, release_title, release_group, state)
         VALUES (?, ?, ?, ?, ?, 'grabbed')
         RETURNING id",
    )
    .bind(series_id)
    .bind(episode_number)
    .bind(&quality_tag)
    .bind(release_title)
    .bind(release_group)
    .fetch_one(db)
    .await?;

    // The `WHERE COALESCE(manual_override, 0) = 0` guard mirrors
    // `update_classification`: if the user has pinned a classification on
    // this episode, re-grabs must not silently overwrite it. Without the
    // guard the row would end up internally inconsistent — manual_override
    // still flipped on, but the columns it's supposed to protect replaced
    // by the automatic classifier's verdict. An upgrade re-grab on a
    // pinned row is a no-op on the tag row; the grab history row still
    // records the event unconditionally above.
    sqlx::query(
        "INSERT INTO episode_quality_tags (
             series_id, episode_number, quality_tag, release_title, release_group, state,
             source, resolution, is_remux, is_bdmv, web_kind,
             classification_confidence, needs_review
         )
         VALUES (?, ?, ?, ?, ?, 'grabbed', ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(series_id, episode_number) DO UPDATE SET
             quality_tag = excluded.quality_tag,
             release_title = excluded.release_title,
             release_group = excluded.release_group,
             state = 'grabbed',
             source = excluded.source,
             resolution = excluded.resolution,
             is_remux = excluded.is_remux,
             is_bdmv = excluded.is_bdmv,
             web_kind = excluded.web_kind,
             classification_confidence = excluded.classification_confidence,
             needs_review = excluded.needs_review,
             updated_at = CURRENT_TIMESTAMP
         WHERE COALESCE(episode_quality_tags.manual_override, 0) = 0",
    )
    .bind(series_id)
    .bind(episode_number)
    .bind(&quality_tag)
    .bind(release_title)
    .bind(release_group)
    .bind(source_str)
    .bind(resolution_str)
    .bind(is_remux)
    .bind(is_bdmv)
    .bind(web_kind_str)
    .bind(confidence)
    .bind(needs_review)
    .execute(db)
    .await?;

    Ok(history_id)
}

/// Get the current quality tag map for a series.
pub async fn get_for_series(
    db: &SqlitePool,
    series_id: i64,
) -> Result<std::collections::HashMap<i32, EpisodeQualityTag>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT episode_number, quality_tag, release_title, release_group, state,
                source, resolution, is_remux,
                COALESCE(is_bdmv, 0) AS is_bdmv,
                COALESCE(web_kind, '') AS web_kind,
                classification_confidence, needs_review,
                COALESCE(manual_override, 0) AS manual_override
         FROM episode_quality_tags WHERE series_id = ?",
    )
    .bind(series_id)
    .fetch_all(db)
    .await?;

    let mut map = std::collections::HashMap::new();
    for row in rows {
        let ep_num: i32 = row.get("episode_number");
        let is_remux_i: i64 = row.get("is_remux");
        let is_bdmv_i: i64 = row.get("is_bdmv");
        let needs_review_i: i64 = row.get("needs_review");
        let manual_override_i: i64 = row.get("manual_override");
        let confidence: f64 = row.get("classification_confidence");
        map.insert(
            ep_num,
            EpisodeQualityTag {
                quality_tag: row.get("quality_tag"),
                release_title: row.get("release_title"),
                release_group: row.get("release_group"),
                state: row.get("state"),
                source: row.get("source"),
                resolution: row.get("resolution"),
                is_remux: is_remux_i != 0,
                is_bdmv: is_bdmv_i != 0,
                web_kind: row.get("web_kind"),
                classification_confidence: confidence as f32,
                needs_review: needs_review_i != 0,
                manual_override: manual_override_i != 0,
            },
        );
    }
    Ok(map)
}

/// Get grab history for a specific episode (newest first, up to 10 entries).
pub async fn get_grab_history(
    db: &SqlitePool,
    series_id: i64,
    episode_number: i32,
) -> Result<Vec<GrabHistoryEntry>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id, quality_tag, release_title, release_group, grabbed_at, state
         FROM episode_grab_history
         WHERE series_id = ? AND episode_number = ?
         ORDER BY grabbed_at DESC
         LIMIT 10",
    )
    .bind(series_id)
    .bind(episode_number)
    .fetch_all(db)
    .await?;

    Ok(rows
        .iter()
        .map(|row| GrabHistoryEntry {
            id: row.get("id"),
            quality_tag: row.get("quality_tag"),
            release_title: row.get("release_title"),
            release_group: row.get("release_group"),
            grabbed_at: row.get("grabbed_at"),
            state: row.get("state"),
        })
        .collect())
}

/// Overwrite the structured classification columns on an existing tag row.
/// Called after post-download classification (Layer 5 + Layer 6) produces a
/// verdict that may differ from the pre-download one. Preserves
/// `release_title`, `release_group`, `state`, and — crucially — any
/// `manual_override` the user has set. Rows with `manual_override = 1` are
/// left entirely alone: the user's explicit tag wins over the classifier.
///
/// Also refreshes the legacy `quality_tag` string from the new classification
/// so any UI that still reads that column picks up the post-download update.
pub async fn update_classification(
    db: &SqlitePool,
    series_id: i64,
    episode_number: i32,
    classification: &ClassificationResult,
) -> Result<(), sqlx::Error> {
    let quality_tag = classification.label();
    let source_str = classification.source.as_str();
    let resolution_str = classification.resolution.as_str();
    let is_remux = if classification.is_remux { 1_i64 } else { 0_i64 };
    let is_bdmv = if classification.is_bdmv { 1_i64 } else { 0_i64 };
    let web_kind_str = classification.web_kind.as_str();
    let confidence = classification.confidence as f64;
    let needs_review = if classification.needs_review { 1_i64 } else { 0_i64 };

    sqlx::query(
        "UPDATE episode_quality_tags SET
             quality_tag = ?,
             source = ?,
             resolution = ?,
             is_remux = ?,
             is_bdmv = ?,
             web_kind = ?,
             classification_confidence = ?,
             needs_review = ?,
             updated_at = CURRENT_TIMESTAMP
         WHERE series_id = ?
           AND episode_number = ?
           AND COALESCE(manual_override, 0) = 0",
    )
    .bind(&quality_tag)
    .bind(source_str)
    .bind(resolution_str)
    .bind(is_remux)
    .bind(is_bdmv)
    .bind(web_kind_str)
    .bind(confidence)
    .bind(needs_review)
    .bind(series_id)
    .bind(episode_number)
    .execute(db)
    .await?;

    Ok(())
}

/// Apply a user's manual classification override for an episode. The row is
/// upserted (inserted if it doesn't exist yet, e.g. for externally-imported
/// files the classifier hasn't seen) with `manual_override = 1`, which
/// prevents `update_classification` from overwriting it on re-classify.
/// `needs_review` is cleared because the user has explicitly resolved it.
///
/// If `source` is empty, the override is removed: the row is updated with
/// `manual_override = 0` and kept otherwise intact, so the next
/// post-download classify pass is free to overwrite it.
#[allow(clippy::too_many_arguments)]
pub async fn set_manual_override(
    db: &SqlitePool,
    series_id: i64,
    episode_number: i32,
    source: &str,
    resolution: &str,
    is_remux: bool,
    is_bdmv: bool,
    web_kind: &str,
) -> Result<(), sqlx::Error> {
    if source.is_empty() {
        // Clear override — leave the row and its current classification in
        // place, just flip the lock off.
        sqlx::query(
            "UPDATE episode_quality_tags SET manual_override = 0, updated_at = CURRENT_TIMESTAMP
             WHERE series_id = ? AND episode_number = ?",
        )
        .bind(series_id)
        .bind(episode_number)
        .execute(db)
        .await?;
        return Ok(());
    }

    // Build a `ClassificationResult` from the manual fields and reuse its
    // `label()` — keeps the rendering rules in exactly one place so the
    // BDMV/Remux/WebKind precedence can't drift between automatic and
    // manual paths.
    let parsed_source = crate::services::source::Source::from_str(source);
    let parsed_resolution = crate::services::source::Resolution::from_str(resolution);
    let parsed_web_kind = crate::services::source::WebKind::from_str(web_kind);
    let synthetic = crate::services::source::ClassificationResult {
        source: parsed_source,
        resolution: parsed_resolution,
        is_remux,
        is_bdmv,
        web_kind: parsed_web_kind,
        confidence: 1.0,
        needs_review: false,
        evidence: Vec::new(),
    };
    let label = synthetic.label();
    let is_remux_i = if is_remux { 1_i64 } else { 0_i64 };
    let is_bdmv_i = if is_bdmv { 1_i64 } else { 0_i64 };
    let web_kind_str = parsed_web_kind.as_str();

    // Upsert: if the row doesn't exist yet (user tagging a file that the
    // classifier never saw), insert a fresh row with empty release metadata.
    // If it exists, flip to manual_override and overwrite the classification
    // columns with the user's choice.
    sqlx::query(
        "INSERT INTO episode_quality_tags (
             series_id, episode_number, quality_tag, release_title, release_group, state,
             source, resolution, is_remux, is_bdmv, web_kind,
             classification_confidence, needs_review, manual_override
         )
         VALUES (?, ?, ?, '', '', 'completed', ?, ?, ?, ?, ?, 1.0, 0, 1)
         ON CONFLICT(series_id, episode_number) DO UPDATE SET
             quality_tag = excluded.quality_tag,
             source = excluded.source,
             resolution = excluded.resolution,
             is_remux = excluded.is_remux,
             is_bdmv = excluded.is_bdmv,
             web_kind = excluded.web_kind,
             classification_confidence = 1.0,
             needs_review = 0,
             manual_override = 1,
             updated_at = CURRENT_TIMESTAMP",
    )
    .bind(series_id)
    .bind(episode_number)
    .bind(&label)
    .bind(source)
    .bind(resolution)
    .bind(is_remux_i)
    .bind(is_bdmv_i)
    .bind(web_kind_str)
    .execute(db)
    .await?;
    Ok(())
}

/// Mark episode quality tags as "completed" for the given episodes of a series.
/// Called by post-processing after a torrent is successfully imported.
pub async fn mark_completed(
    db: &SqlitePool,
    series_id: i64,
    episode_numbers: &[i32],
) -> Result<(), sqlx::Error> {
    for &ep in episode_numbers {
        sqlx::query(
            "UPDATE episode_quality_tags SET state = 'completed', updated_at = CURRENT_TIMESTAMP
             WHERE series_id = ? AND episode_number = ? AND state = 'grabbed'",
        )
        .bind(series_id)
        .bind(ep)
        .execute(db)
        .await?;
    }
    Ok(())
}

/// Clear the current quality tag for an episode (e.g. after file deletion).
pub async fn clear_episode_tag(
    db: &SqlitePool,
    series_id: i64,
    episode_number: i32,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "DELETE FROM episode_quality_tags WHERE series_id = ? AND episode_number = ?",
    )
    .bind(series_id)
    .bind(episode_number)
    .execute(db)
    .await?;
    Ok(())
}

/// Clear episode quality tags and mark grab history as "removed" for all episodes
/// associated with a grabbed torrent (identified by series_id + episode_numbers).
pub async fn clear_tags_for_removal(
    db: &SqlitePool,
    series_id: i64,
    episode_numbers: &[i32],
) -> Result<(), sqlx::Error> {
    for &ep in episode_numbers {
        // Delete the current quality tag so the episode no longer shows as grabbed.
        sqlx::query(
            "DELETE FROM episode_quality_tags WHERE series_id = ? AND episode_number = ?",
        )
        .bind(series_id)
        .bind(ep)
        .execute(db)
        .await?;

        // Mark any "grabbed" history entries for this episode as "removed".
        sqlx::query(
            "UPDATE episode_grab_history SET state = 'removed'
             WHERE series_id = ? AND episode_number = ? AND state = 'grabbed'",
        )
        .bind(series_id)
        .bind(ep)
        .execute(db)
        .await?;
    }
    Ok(())
}

/// Mark a grab history entry as failed, and update the current tag state if it matches.
pub async fn mark_grab_failed(
    db: &SqlitePool,
    history_id: i64,
) -> Result<(i64, i32, String), sqlx::Error> {
    // Fetch series_id, episode_number, release_title before marking failed.
    let row = sqlx::query(
        "SELECT series_id, episode_number, release_title FROM episode_grab_history WHERE id = ?",
    )
    .bind(history_id)
    .fetch_one(db)
    .await?;
    let series_id: i64 = row.get("series_id");
    let episode_number: i32 = row.get("episode_number");
    let release_title: String = row.get("release_title");

    sqlx::query("UPDATE episode_grab_history SET state = 'failed' WHERE id = ?")
        .bind(history_id)
        .execute(db)
        .await?;

    // Update the current tag to 'failed' so the UI reflects the state.
    sqlx::query(
        "UPDATE episode_quality_tags SET state = 'failed', updated_at = CURRENT_TIMESTAMP
         WHERE series_id = ? AND episode_number = ?",
    )
    .bind(series_id)
    .bind(episode_number)
    .execute(db)
    .await?;

    Ok((series_id, episode_number, release_title))
}
