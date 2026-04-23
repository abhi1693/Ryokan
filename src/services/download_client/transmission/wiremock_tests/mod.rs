//! Transmission wiremock tests, topic-split per the test-coverage-
//! expansion plan (PR 4c).
//!
//!   * `fixture.rs` — `new_fixture()` spins up a mock server and
//!     seeds `X-Transmission-Session-Id: <id>` on the first
//!     request (via a 409 on requests without the header),
//!     matching Transmission's CSRF handshake exactly.
//!   * `session_handshake.rs` — the 409-plus-header handshake +
//!     mid-stream session-id rotation.
//!   * `add.rs` — `torrent-add` happy path, `torrent-duplicate`
//!     envelope disambiguation (inside a `result: "success"`
//!     response), paused option pass-through.
//!   * `list.rs` — `torrent-get` response parsing, client-side
//!     label filter semantics (`labels` array contains Ryokan's
//!     label), state mapping.
//!   * `files.rs` — `torrent-set` with `files-wanted` /
//!     `files-unwanted` keys (separate axes, unlike qBit's
//!     single-priority field).
//!   * `control.rs` — `torrent-stop` / `torrent-start` /
//!     `torrent-remove` wire shape + `delete-local-data` flag.

mod add;
mod control;
mod files;
mod fixture;
mod list;
mod session_handshake;
