//! Torznab/newznab `Indexer` impl (issue #28 PR B).
//!
//! Wire format is RSS 2.0 with `<torznab:attr name="X" value="Y"/>`
//! sibling extensions on each `<item>`. Same shape across torznab
//! and newznab — the kind only differs in download URL semantics
//! (`.torrent`/magnet vs `.nzb`) and category-mapping nuances.
//!
//! ## Parser approach
//!
//! Regex-based, mirroring [`crate::services::rss::feed`]. The
//! plan-doc non-negotiable rules out a new XML crate dep "unless
//! necessary"; the torznab shape is regular enough that
//! `regex_lite` covers it without a real XML parser. Edge cases
//! that bit the RSS parser (CDATA, `&amp;` entities) are handled
//! the same way; the new piece is the `<torznab:attr>` extraction
//! into a per-item attr map so callers pull `seeders`, `infohash`,
//! etc. by name.
//!
//! If a future indexer surfaces XML shapes the regex chain can't
//! handle (nested CDATA, attribute-order tricks), bring in
//! `quick-xml` as a follow-up — the trait surface stays the same.
//!
//! ## Error handling per protocol
//!
//! - Successful searches return RSS 2.0 + items.
//! - Failed searches return **HTTP 200** with `<error
//!   code="N" description="..."/>` body. Parse the body before
//!   trusting the status code.
//! - Some impls (Prowlarr's pre-torznab-layer auth) return non-200
//!   for bad API keys before the torznab layer sees the request.
//!   Treat both shapes as failures.
//! - 429 + `Retry-After` from upstream rate-limits surface as
//!   structured errors so the caller can apply the cooldown
//!   pattern from [`crate::services::anilist::rate_limit`].

pub mod client;
pub mod parser;

pub use client::TorznabIndexer;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod wiremock_tests;
