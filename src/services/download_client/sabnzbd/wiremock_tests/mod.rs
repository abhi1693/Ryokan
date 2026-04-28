//! Wiremock-backed `SabClient` tests. Same layout as the BT impls'
//! topic-split: fixture builder + per-feature topic file. Parallel
//! to Sonarr's `DownloadClientFixtureBase<TSubject>` (setup method
//! on a base class) but shaped to Rust's factory-function style.
//!
//!   * `fixture.rs` — spins up a wiremock server, returns a
//!     `SabClient` pointing at it. SAB has no login handshake, so
//!     the fixture is simpler than the BT equivalents — just the
//!     server + client pair.
//!   * `add.rs` — `add_torrent_returning_id` happy path (extracts
//!     `nzo_id` from the JSON body), empty-`nzo_ids` →
//!     `AlreadyPresent` fallback via queue scan, `priority=-1` for
//!     the paused-add variant.
//!   * `list.rs` — `list_scoped` merges queue + history slots,
//!     filters by category, maps SAB status strings through the
//!     normalized state enum.
//!   * `control.rs` — `pause` / `resume` / `delete` (queue-then-
//!     history fallback for delete).

mod add;
mod auth_test;
mod control;
mod fixture;
mod list;
