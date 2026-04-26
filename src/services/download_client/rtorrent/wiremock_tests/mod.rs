//! rtorrent XML-RPC wiremock tests, topic-split per PR 4d.
//!
//! rtorrent is the only client in this module that speaks XML-RPC
//! (POST /RPC2 with XML request/response bodies) rather than JSON.
//! The existing inline tests above the submodule cover the codec
//! layer (encode_request / decode_response / fault parsing); these
//! wiremock-backed tests fill the trait-method integration gap.
//!
//!   * `fixture.rs` — `new_fixture()` + helpers for registering
//!     per-method XML responses against body_string_contains
//!     `<methodName>...</methodName>` matchers.
//!   * `add.rs` — URL-scheme validation, `hash_exists` pre-check +
//!     the silent-0-return duplicate detection, label-stamping
//!     via the third positional arg to load.start_verbose.
//!   * `list.rs` — `d.multicall2` shape + custom1 label filter.
//!   * `files.rs` — `f.priority.set` followed by the MANDATORY
//!     `d.update_priorities` flush that a naive impl would forget.
//!   * `control.rs` — `d.pause`/`d.resume`/`d.erase` wire shape.
//!   * `hash_case.rs` — uppercase-on-wire contract vs
//!     lowercase-at-the-trait-boundary; case-insensitive comparison
//!     during `hash_exists`.

mod add;
mod control;
mod files;
mod fixture;
mod hash_case;
mod list;
mod seed_rules;
