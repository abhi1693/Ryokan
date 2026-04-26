//! Deluge wiremock tests, topic-split per the test-coverage-
//! expansion plan (PR 4b).
//!
//! Deluge's JSON-RPC dispatches by the `method` field inside the
//! POST body rather than the URL path — every call hits `/json`.
//! The [`fixture`] helper hides that detail behind a
//! `install_rpc(server, method, result)` registrar so test bodies
//! read as "when auth.login is called, return true" rather than
//! manually constructing `body_partial_json` matchers.
//!
//!   * `fixture.rs` — `DelugeTestFixture`: mock server + pre-seeded
//!     handshake (auth.login → web.get_hosts → web.get_plugins →
//!     web.connect → label.add) + a `DelugeClient` bound to it.
//!   * `connect.rs` — the two-step connect handshake, auth failure
//!     on `auth.login == false`, Label plugin auto-enable +
//!     reconnect workaround.
//!   * `add.rs` — `core.add_torrent_magnet` vs `core.add_torrent_url`
//!     dispatch, duplicate-add substring matching, label fan-out.
//!   * `list.rs` — `core.get_torrents_status` with label filter,
//!     dict-keyed-by-hash parsing, defensive hash injection when the
//!     inner `hash` field is missing.
//!   * `files.rs` — priority 0 (skip) vs 4 (normal) semantics —
//!     **distinct from qBit's 0/1** — and the read-patch-write
//!     sequence `set_file_wanted` uses.
//!   * `control.rs` — `core.pause_torrent` / `core.resume_torrent` /
//!     `core.remove_torrent` wire shapes. Note: `remove_torrent`
//!     (singular) not `remove_torrents`.

mod add;
mod connect;
mod control;
mod files;
mod fixture;
mod list;
mod seed_rules;
