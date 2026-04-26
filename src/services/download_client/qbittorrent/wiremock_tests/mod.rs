//! Wiremock-backed QbitClient tests, topic-split per the
//! test-coverage-expansion plan (PR 4a).
//!
//!   * `fixture.rs` — `QbitTestFixture`: spins up a wiremock server
//!     on a free port, pre-seeds the `/api/v2/auth/login → Ok.` path
//!     so tests don't repeat that boilerplate, and hands back a
//!     `QbitClient` pointing at the server. Models after Sonarr's
//!     `DownloadClientFixtureBase` — shared setup, per-test canned
//!     responses.
//!   * `auth.rs` — login body handling, 403 → re-auth flow,
//!     `test()` version probe.
//!   * `add.rs` — `add_torrent` happy path (200 "Ok." → Added),
//!     v5.x `200 "Fails."` disambiguation via `/torrents/info`,
//!     form-body construction pins the `urls` + `category` keys.
//!   * `list.rs` — `list_scoped` sends `?category=`, parses JSON
//!     array, maps state strings via `to_download_item`.
//!   * `files.rs` — `get_files` priority 0/1/6/7 round-trip,
//!     `set_file_wanted` form construction (skip vs normal).
//!   * `control.rs` — `pause`/`resume`/`delete` try-new-then-fallback
//!     (qBit 5.x renamed stop/start vs 4.x pause/resume), `delete`
//!     `deleteFiles` form flag.
//!
//! Each topic file uses [`fixture::QbitTestFixture`] rather than
//! hand-rolling a wiremock server so the set-up boilerplate stays
//! out of the test bodies. Wire-format JSON is inlined as
//! `serde_json::json!` rather than checked-in fixture files —
//! easier to diff and grep, matches the convention Sonarr landed on
//! after trying checked-in JSON.

mod add;
mod auth;
mod control;
mod files;
mod fixture;
mod list;
mod seed_rules;
