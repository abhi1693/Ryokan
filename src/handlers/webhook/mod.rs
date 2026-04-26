//! Webhook receivers (issue #28 PR D).
//!
//! `POST /api/webhook/autobrr` is the first webhook endpoint —
//! receives push notifications from autobrr for IRC-announced
//! releases that pass autobrr's filters. Keyed by a per-Ryokan
//! API key the user pastes from the Settings → Connections →
//! autobrr panel into autobrr's Webhook action config.
//!
//! Future webhook receivers (e.g. radarr companion) should land
//! as siblings in this module so the auth + body-shape patterns
//! stay consistent.

pub mod autobrr;

#[cfg(test)]
mod tests;
