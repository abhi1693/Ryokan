// Phase 1a foundation: nothing in production calls into this module yet — it
// is exercised only by unit tests until Phase 1b wires the classifier into
// `auto_search`, `rss`, and `upgrade`. Remove this allow when that happens.
#![allow(dead_code)]

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
use crate::services::source::SourceEvidence;

const ORIGIN: &str = "group";

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
