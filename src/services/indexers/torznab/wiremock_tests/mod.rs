//! HTTP-shape wiremock tests for the [`TorznabIndexer`] client.
//!
//! Topic-split per the existing convention in
//! `services/download_client/*/wiremock_tests/`. Each file
//! covers one slice of behavior so a failure points cleanly at
//! the affected surface.

mod auth_failures;
mod caps;
mod fixture;
mod rate_limit;
mod rss;
mod search;
