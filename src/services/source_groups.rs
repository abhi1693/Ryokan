//! Layer 3 — release group identity lookup.
//!
//! Thin wrapper over [`crate::models::group_source_map`]. Given a release
//! group name extracted from a torrent title, consult the group → source
//! table and emit a single piece of evidence if the group is known.
//!
//! This layer is what lets Ryokan classify SubsPlease, HorribleSubs,
//! VCB-Studio, and other groups whose filenames carry no source tokens at
//! all. Unknown groups contribute no evidence — better to fall through to
//! post-download layers than to guess.

use sqlx::SqlitePool;

use crate::models::group_source_map;
use crate::services::source::{Origin, SourceEvidence};

const ORIGIN: Origin = Origin::Group;

/// Look up a release group and, if known, return a single
/// [`SourceEvidence`] record tagged with the group's source and confidence.
///
/// Returns `None` when:
/// - the group name is empty
/// - the group isn't in the table
/// - the database lookup fails (logged, not bubbled up — classification
///   should degrade gracefully if the table becomes unavailable)
pub async fn classify_group(db: &SqlitePool, group_name: &str) -> Option<SourceEvidence> {
    let trimmed = group_name.trim();
    if trimmed.is_empty() {
        return None;
    }

    match group_source_map::get(db, trimmed).await {
        Ok(Some(entry)) => Some(SourceEvidence::new(
            entry.source,
            entry.confidence,
            ORIGIN,
            format!("group table: {}", entry.group_name),
        )),
        Ok(None) => None,
        Err(err) => {
            tracing::warn!(
                target: "ryokan::classify",
                error = %err,
                group = %trimmed,
                "group_source_map lookup failed"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::source::Source;

    /// Build an in-memory SQLite pool with the group table migrated and
    /// seeded. Shared across all tests in this module.
    async fn test_pool() -> SqlitePool {
        let db = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite");
        group_source_map::migrate(&db)
            .await
            .expect("migrate group_source_map");
        db
    }

    #[tokio::test]
    async fn known_seeded_group_emits_evidence() {
        let db = test_pool().await;
        // VCB-Studio is a well-known legacy BD-only encoder and is always
        // part of the seed set.
        let ev = classify_group(&db, "VCB-Studio")
            .await
            .expect("VCB-Studio should be seeded");
        assert_eq!(ev.source, Source::BluRay);
        assert!((ev.confidence - 0.95).abs() < f32::EPSILON);
        assert_eq!(ev.origin, Origin::Group);
        assert!(ev.detail.contains("VCB-Studio"));
    }

    #[tokio::test]
    async fn unknown_group_returns_none() {
        let db = test_pool().await;
        assert!(
            classify_group(&db, "definitely-not-a-real-group-12345")
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn empty_group_name_returns_none() {
        let db = test_pool().await;
        assert!(classify_group(&db, "").await.is_none());
    }

    #[tokio::test]
    async fn whitespace_only_group_name_returns_none() {
        let db = test_pool().await;
        assert!(classify_group(&db, "   ").await.is_none());
        assert!(classify_group(&db, "\t\n").await.is_none());
    }

    #[tokio::test]
    async fn group_name_is_trimmed_before_lookup() {
        let db = test_pool().await;
        // Leading/trailing whitespace from noisy parsers must not break
        // lookups against otherwise-valid group names.
        let ev = classify_group(&db, "  VCB-Studio  ")
            .await
            .expect("trimmed lookup should hit");
        assert_eq!(ev.source, Source::BluRay);
    }

    #[tokio::test]
    async fn lookup_is_case_insensitive() {
        let db = test_pool().await;
        // The table uses `COLLATE NOCASE` on the primary key, so any
        // casing of a known group name should resolve.
        for variant in ["vcb-studio", "VCB-STUDIO", "Vcb-Studio"] {
            let ev = classify_group(&db, variant)
                .await
                .unwrap_or_else(|| panic!("case-insensitive lookup failed for {variant}"));
            assert_eq!(ev.source, Source::BluRay);
        }
    }

    #[tokio::test]
    async fn user_edit_overrides_seeded_value() {
        let db = test_pool().await;
        // Simulate a user overriding VCB-Studio's classification (contrived
        // example — the point is that the lookup reflects whatever is in
        // the table, not the seed constants).
        group_source_map::upsert_user_edit(
            &db,
            "VCB-Studio",
            Source::Web,
            0.80,
            "unit test override",
        )
        .await
        .expect("upsert user edit");

        let ev = classify_group(&db, "VCB-Studio")
            .await
            .expect("user edit should be visible");
        assert_eq!(ev.source, Source::Web);
        assert!((ev.confidence - 0.80).abs() < f32::EPSILON);
    }
}
