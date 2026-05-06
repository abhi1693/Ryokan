//! Wiremock-driven tests for the webhook notification provider.
//!
//! Mirrors the `services/download_client/<kind>/wiremock_tests/`
//! directory shape — fixture builder in `fixture.rs`, per-topic
//! tests in topic files. The dispatcher and the trait shape have
//! their own pure-Rust tests in the parent module's inline `tests`
//! block; this module covers the wire-format end of the webhook
//! impl that needs an actual HTTP receiver to assert against.

mod discord_outcomes;
mod discord_payload;
mod fixture;
mod headers;
mod hmac;
mod outcomes;
mod payload;
