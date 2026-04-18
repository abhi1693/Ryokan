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
use crate::services::source::{Origin, SourceEvidence, WebKind};

const ORIGIN: Origin = Origin::Group;

/// Group-table classification output. Bundles the source evidence record
/// the aggregator consumes with a Web sub-tier hint that the aggregator
/// applies when the filename didn't determine WEB-DL vs WEB-Rip on its
/// own — see [`WebKind`] and the `classify_release` comment block.
///
/// `web_kind` is `WebKind::Unknown` for the vast majority of groups; only
/// a handful ship exclusively one Web sub-tier (SubsPlease, HorribleSubs
/// — both direct CR/HIDIVE stream remuxes).
#[derive(Debug, Clone)]
pub struct GroupClassification {
    pub evidence: SourceEvidence,
    pub web_kind: WebKind,
}

/// Look up a release group and, if known, return a source-evidence
/// record plus a Web sub-tier hint.
///
/// Returns `None` when:
/// - the group name is empty
/// - the group isn't in the table
/// - the database lookup fails (logged, not bubbled up — classification
///   should degrade gracefully if the table becomes unavailable)
pub async fn classify_group(db: &SqlitePool, group_name: &str) -> Option<GroupClassification> {
    let trimmed = group_name.trim();
    if trimmed.is_empty() {
        return None;
    }

    match group_source_map::get(db, trimmed).await {
        Ok(Some(entry)) => Some(GroupClassification {
            evidence: SourceEvidence::new(
                entry.source,
                entry.confidence,
                ORIGIN,
                format!("group table: {}", entry.group_name),
            ),
            web_kind: entry.web_kind,
        }),
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
        let cls = classify_group(&db, "VCB-Studio")
            .await
            .expect("VCB-Studio should be seeded");
        assert_eq!(cls.evidence.source, Source::BluRay);
        assert!((cls.evidence.confidence - 0.95).abs() < f32::EPSILON);
        assert_eq!(cls.evidence.origin, Origin::Group);
        assert!(cls.evidence.detail.contains("VCB-Studio"));
        // BD-only group → no Web sub-tier hint.
        assert_eq!(cls.web_kind, WebKind::Unknown);
    }

    #[tokio::test]
    async fn subsplease_is_tagged_webdl() {
        // Motivating case for the web_kind column: SubsPlease filenames
        // carry no "WEB-DL" / "WEBRip" token, so the filename layer
        // leaves web_kind Unknown. The group table provides the hint
        // so the aggregator can label the release as WEBDL-1080p
        // instead of the generic WEB-1080p.
        let db = test_pool().await;
        let cls = classify_group(&db, "SubsPlease")
            .await
            .expect("SubsPlease should be seeded");
        assert_eq!(cls.evidence.source, Source::Web);
        assert_eq!(cls.web_kind, WebKind::WebDl);
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
        let cls = classify_group(&db, "  VCB-Studio  ")
            .await
            .expect("trimmed lookup should hit");
        assert_eq!(cls.evidence.source, Source::BluRay);
    }

    #[tokio::test]
    async fn lookup_is_case_insensitive() {
        let db = test_pool().await;
        // The table uses `COLLATE NOCASE` on the primary key, so any
        // casing of a known group name should resolve.
        for variant in ["vcb-studio", "VCB-STUDIO", "Vcb-Studio"] {
            let cls = classify_group(&db, variant)
                .await
                .unwrap_or_else(|| panic!("case-insensitive lookup failed for {variant}"));
            assert_eq!(cls.evidence.source, Source::BluRay);
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

        let cls = classify_group(&db, "VCB-Studio")
            .await
            .expect("user edit should be visible");
        assert_eq!(cls.evidence.source, Source::Web);
        assert!((cls.evidence.confidence - 0.80).abs() < f32::EPSILON);
    }

    #[tokio::test]
    async fn user_edited_row_keeps_web_kind_across_reseed() {
        // SEED_WEB_KIND's UPDATE is gated on `is_user_edit = 0`, so a
        // user-edited row should hang onto whatever web_kind it had
        // before the user edit — including the empty default, which
        // matters for groups the user wants to *remove* the WEB-DL
        // hint from (e.g. reclassifying a group we tagged as WEB-DL
        // that actually ships WEB-Rips).
        let db = test_pool().await;

        // Seed pass populated SubsPlease → (Web, WebDl).
        let seeded = classify_group(&db, "SubsPlease")
            .await
            .expect("SubsPlease seeded");
        assert_eq!(seeded.web_kind, WebKind::WebDl);

        // User overrides source → is_user_edit flips to 1.
        // `upsert_user_edit` doesn't touch `web_kind`, so whatever was
        // there stays — that's the pin.
        group_source_map::upsert_user_edit(
            &db,
            "SubsPlease",
            Source::BluRay,
            0.90,
            "imaginary override",
        )
        .await
        .expect("upsert user edit");

        // Re-run the migration → seed pass attempts SEED_WEB_KIND
        // update but the `is_user_edit = 0` filter excludes this row.
        group_source_map::migrate(&db)
            .await
            .expect("re-migrate");

        let after = classify_group(&db, "SubsPlease")
            .await
            .expect("user row survives re-migrate");
        assert_eq!(after.evidence.source, Source::BluRay, "user source wins");
        assert_eq!(
            after.web_kind,
            WebKind::WebDl,
            "user-edited row's web_kind must survive the re-seed pass"
        );
    }
}
