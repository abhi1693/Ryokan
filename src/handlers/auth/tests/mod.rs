//! Auth handler tests, topic-split per the test-coverage-expansion
//! plan (PR 1). The split lets each file stay under ~500 LoC and
//! keeps test failures localized to the behavior area they defend.
//!
//! Layout mirrors the decision categories in
//! `/home/john/Documents/ryokan-roadmap/test-coverage-expansion-plan.md`:
//!
//!   * `throttle.rs` — `LoginCheck` tiers, failure recording, sweep,
//!     per-key isolation. Uses unique per-test keys to avoid
//!     parallel-test contamination on the process-wide
//!     `LOGIN_FAILURES` mutex — each test's bucket is independent so
//!     concurrent runs don't step on each other.
//!   * `csrf.rs` — `verify_same_origin_with_trust` across Origin /
//!     Referer / missing-both, plus the `X-Forwarded-Host`
//!     allowed-hosts expansion. Pure unit coverage of the function;
//!     middleware wiring is exercised by `sessions.rs`.
//!   * `proxy_headers.rs` — `client_ip_from_request_with_trust`
//!     honoring `X-Forwarded-For` / `X-Real-IP` only when trust is
//!     on; TCP peer always wins otherwise.
//!   * `sessions.rs` — Cookie shape on set and clear paths with
//!     `Secure` on/off, plus a full HTTP round-trip through
//!     `handler_router` to prove `require_auth` rejects anonymous
//!     and accepts a valid `session=<token>` cookie.
//!   * `setup.rs` — First-run gate: `/setup` reachable before any
//!     user exists, `/login` redirects to `/setup` until then,
//!     post-setup `/setup` redirects to `/login`.
//!   * `timing_equalization.rs` — Wall-clock measurement on
//!     `login_submit` with hit-vs-miss usernames, wide tolerance.
//!     Kept off the hot path in CI (acceptance is a big envelope
//!     around the bcrypt cost so scheduler jitter doesn't flake it).
//!   * `forgot_password.rs` — Page renders without auth and HTML
//!     contains the recovery-recipe markers.

mod csrf;
mod forgot_password;
mod proxy_headers;
mod sessions;
mod setup;
mod throttle;
mod timing_equalization;
