# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with this repository. **It's the base layer**; deeper notes for specific subsystems live in nested CLAUDE.md files (linked under "Code Layout" below) and load automatically when work touches those subtrees.

## Project Overview

Ryokan is a self-hosted anime PVR (personal video recorder) written in Rust. It searches Nyaa for anime torrent releases, scores them by quality, and sends them to a torrent client for download. Four clients are supported behind a common `DownloadClient` trait — qBittorrent, Deluge, Transmission, and rTorrent — with one active at a time per instance. AniList is the primary metadata source with MAL (via Jikan) and Kitsu as fallbacks.

Release scoring combines Sonarr-style **Custom Formats** (TRaSH-Guides-compatible), a multi-layer **source classification pipeline**, and optional **SeaDex** (releases.moe) authoritative picks. Ryokan also exposes a Sonarr/Radarr-compatible API shim (**anibridge**) so Seerr and similar tools can request anime through it.

## Build & Run

```bash
cargo build [--release]
cargo run                                              # 0.0.0.0:8978, creates data/ryokan.db
cargo nextest run --workspace --features test-support  # canonical test entry; ~2-3× faster than cargo test
cargo nextest run --workspace --features test-support <test_name>   # single-test filter
cargo test --workspace --features test-support         # CI-shape fallback (doc tests, --locked)
cargo clippy
docker compose up -d --build
```

Toolchain: Rust 1.95+ (enforced via `package.rust-version`), C/C++ toolchain (vendored anitomy + bundled SQLite), `cmake` (aws-lc-sys for rustls). No OpenSSL needed — TLS is pure-Rust rustls + aws-lc.

## Environment Variables

| Variable | Default | Purpose |
|---|---|---|
| `LISTEN_ADDR` | `0.0.0.0:8978` | Bind address. |
| `DATABASE_URL` | `sqlite://data/ryokan.db?mode=rwc` | SQLite connection string. |
| `RUST_LOG` | `ryokan=debug,tower_http=debug` (local) / `ryokan=info` (Docker) | Console-only log filter. |
| `JIKAN_API_BASE` | `https://api.jikan.moe/v4` | Self-hosted Jikan override. HTTPS-behind-private-CA: CA must be in system trust store; rustls-platform-verifier doesn't honor `SSL_CERT_FILE` / `SSL_CERT_DIR`. |
| `RYOKAN_ANILIST_API_BASE` | `https://graphql.anilist.co` | AL GraphQL override. **Primary use is the `tests/external_sync_e2e.rs` wiremock fixture**; production shouldn't set this. Re-read on every AL request, not cached. |
| `RYOKAN_MEDIA_CACHE_DIR` | `data/cache/artwork` | Artwork blob cache root (content-addressed). |
| `RYOKAN_TRUSTED_PROXY` | unset → false | Read once at startup. When truthy, `handlers::auth` prefers `X-Forwarded-For` (leftmost) then `X-Real-IP` over the TCP peer for client IP. **Default off**: the default bind is direct-exposure; trusting these by default would let an attacker spoof a fresh IP per attempt and bypass per-IP login throttle. Flip on only behind a reverse proxy that overwrites the headers on ingress. |
| `RYOKAN_COOKIE_SECURE` | unset → false | Read once at startup. Appends `Secure` to the session cookie when truthy. Default off so `cargo run` on HTTP localhost works; flip on for HTTPS. |
| `RYOKAN_RESET_AUTH` | unset | Set to `1` (or pass `--reset-auth` argv) **alongside a `data/.reset-auth` sentinel file** to wipe `users` + `sessions` on next boot. The sentinel is required so a stuck-on env var can't silently wipe auth on every boot. |
| `RYOKAN_DB_LOG_LEVEL` | `info` | Write-side floor for the DB-backed `logs` table (separate from `RUST_LOG`). `trace` / `debug` / `info` / `warn` / `error`; unknown coerces to `info`. Read into a process-wide `AtomicU8`. The read-side System → Logs filters are independent. |
| `RYOKAN_ENCRYPTION_KEY` | unset → file fallback | Base64-encoded 32-byte AEAD key (padded or unpadded both accepted) for `services::crypto`. Used to encrypt OAuth tokens in `external_accounts`. **Loading priority**: env var → `data/.ryokan-key` (raw 32 bytes, mode 0600) → auto-generated on first run with the same path. **Key rotation isn't supported** — changing it invalidates all encrypted tokens. **Key-load failure at startup panics** rather than silently running with an empty token store. |
| `RYOKAN_QBIT_E2E` / `RYOKAN_DELUGE_E2E` / `RYOKAN_TRANSMISSION_E2E` / `RYOKAN_RTORRENT_E2E` / `RYOKAN_SAB_E2E` | unset | Test-only. Opt inline `live_smoke` tests into running against a real client on localhost. See `src/services/download_client/CLAUDE.md`. |
| `QBIT_PASS` | `adminadmin` | Test-only; qBit live_smoke password (qBit-on-first-start default). |
| `RYOKAN_SAB_URL` / `RYOKAN_SAB_API_KEY` / `RYOKAN_SAB_CAT` | `http://localhost:8080` / unset / `ryokan-test` | Test-only; SAB live_smoke. API key has no usable default. |

There is no `.env` loader — env vars come from the process environment directly (`docker-compose.yml` in prod, the user's shell in dev). The choice is deliberate; don't add one.

## Stack at a glance

Axum 0.8 + Tokio · SQLite via sqlx 0.8 (`default-features = false` + `runtime-tokio`,`sqlite`,`macros`; **no compile-time query checking** — all queries are runtime strings; the `macros` feature is kept only for the derive macros) · reqwest 0.13 + rustls + aws-lc + rustls-platform-verifier · Askama 0.16 templates (compiled in at build time) · HTMX 2.x vendored under `static/vendor/`, body-wide `hx-boost="true"` active · `regex-lite` default, `fancy-regex` only inside `services/custom_formats.rs` (PCRE look-around required by TRaSH CFs) · `anitomy` 0.2 (vendored C++) for release-title tokenization · `ammonia` 4 for HTML sanitize · `bcrypt` 0.19 cost 10, `subtle` 2 for constant-time compare, `rand` 0.10 + `hex` for session tokens · `utoipa` 5 + `utoipa-swagger-ui` 9 mount Swagger UI at `/api-docs`, JSON at `/api-docs/openapi.json` · `tracing` 0.1 + `tracing-subscriber` 0.3 (env-filter) + a separate DB-backed `logs` table via `services::logger`.

`async-trait` 0.1 is used specifically for `DownloadClient` because native async-fn-in-traits doesn't yet produce `Send`-bound futures by default, which is required for `Arc<dyn DownloadClient>` storage on a multi-threaded Tokio runtime. Non-object-safe traits use native syntax fine.

## Code Layout (`src/`)

- **`handlers/`** — Axum route handlers. One module per page/feature: `library`, `search`, `downloads`, `settings`, `system`, `auth`, `media`, `help`, `progress`, `grab` (interactive file-picker — `/api/grab/{preview,confirm,heartbeat,cancel}`), `oauth` (AL+MAL link/submit/unlink/sync-now under `/settings/oauth/*`), plus `sonarr_compat` / `radarr_compat` (the anibridge shims), `arr_auth` (shared API-key middleware), `arr_shared` (DTOs shared between shim sides). Auth deep dive: **`src/handlers/auth/CLAUDE.md`**.
- **`services/`** — Business logic + external API clients.
  - Metadata chain: `anilist/` → `jikan` → `kitsu`, plus `mal` (OAuth-authenticated, distinct from `jikan` which is the public-fallback path).
  - `download_client/` — trait + 4 client impls (qBit / Deluge / Transmission / rtorrent). Wire quirks deep dive: **`src/services/download_client/CLAUDE.md`**.
  - `source/` + sibling `source_*.rs` — multi-layer source classification pipeline. `quality.rs`, `scoring.rs`, `custom_formats/`, `upgrade.rs`, `auto_search/`, `auto_expand.rs`. Classifier deep dive: **`src/services/source/CLAUDE.md`**.
  - `external_sync/` — AL/MAL watch-list sync (delta cursor, force-full-resync, removal detection, auth-rejection taxonomy).
  - `crypto.rs` — AEAD wrapper for OAuth tokens. `oauth_state.rs`, `user_score.rs`.
  - `nyaa/`, `seadex.rs`, `rss/`, `anibridge.rs`, `indexers/`, `indexer_catalog.rs`, `interactive_search_cache.rs`.
  - `post_processing/` — moves/renames completed downloads (hardlink/copy/move). `nfo.rs`, `artwork.rs`, `media.rs`, `jellyfin.rs`.
  - `monitoring.rs`, `progress.rs`, `task_registry.rs`, `library_link.rs`, `grab_commit.rs`, `grab_sweep.rs`, `metadata_sync.rs`, `html.rs`, `sanitize.rs`, `logger.rs`.
  - AL deep dive: **`src/services/anilist/CLAUDE.md`**.
- **`models/`** — DB access + schema. `migrations/` owns most `CREATE TABLE` / `ALTER TABLE`. A few tables own their `CREATE TABLE` next to their model module (`custom_formats`, `group_source_map` + `schema_migrations`, `media_probe_cache`, `nyaa_description_cache`).
- **`templates/`** — Askama templates. HTMX patterns + per-page JS lifecycle: **`templates/CLAUDE.md`**.
- **`static/`** — Modular CSS (`base.css`, `topbar.css`, `forms.css`, `badges.css`, `modals.css` loaded everywhere + per-page `pages/<name>.css`) and per-page `static/js/*.js`. No bundler. `static/default_custom_formats.json` is the only non-CSS / non-JS asset (also embedded via `include_str!` at compile time).
- **`tests/`** — Integration tests (binary crates importing `ryokan` as a lib). Browser-e2e harness for HTMX UI assertions. **`tests/CLAUDE.md`**.
- **`fixtures/trash-guides-anime/`** — 28 vendored TRaSH-Guides anime CF JSONs, bundled via `include_str!`.

## AppState

`SqlitePool` + `Arc<RwLock<Option<Arc<dyn DownloadClient>>>>` (swapped on Settings save via `build_download_client`) + `Arc<RwLock<Option<JellyfinClient>>>` + `CompiledCfCache` (`Arc<RwLock<Arc<Vec<_>>>>` swap-on-write so the scoring hot path runs lock-free; handlers clone the inner `Arc` under read lock) + `ProgressRegistry` for long-running user-triggered jobs + `users_exist: Arc<AtomicBool>` flip-to-true-once cache so the auth middleware can skip a `SELECT COUNT(*) FROM users` on every protected request once setup is complete.

## Background tasks

Each runs as a `tokio::spawn` loop in `main.rs` wrapped in `supervise()`, which catches panics/join errors, logs them, and respawns. **Restart policy is exponential backoff**, not flat 5s: `MIN_BACKOFF = 5s`, `MAX_BACKOFF = 30 min`, `HEALTHY_RUNTIME = 60s`. Healthy ≥60s run resets to 5s; <60s exit doubles up to 30 min. Status into `scheduled_task_runs`.

| Task | Interval |
|---|---|
| `progress_sweep` (drop terminal jobs >60s past final event) | 30s |
| `rss_sync` | 60s |
| `post_processing` | 60s |
| `grab_sweep` (auto-commit grabs after `HEARTBEAT_TTL_SECS + SWEEP_INTERVAL ≈ 2 min`) | 60s |
| `external_sync` (AL/MAL watch-list) | user-configurable 15min–7d, 30min default |
| `cleanup` (log rotation, stale rows, login-throttle prune) | 1h |
| `library_classify` | 6h |
| `metadata_refresh` | 12h |
| `upgrade_search` | 24h |
| `anibridge_refresh` (TMDB↔AniList map rebuild) | 24h |

`external_sync` quirks: 1-min outer tick re-reads `config.external_sync_interval_minutes` (Settings change takes effect within 60s); extra `consecutive_errors`-driven exponential skip (2^errors intervals, capped at 5 → 32× multiplier, with outer 24h ceiling so a 7-day cadence can't get pushed seven months by five errors); `has_linked_account` early-out so no `scheduled_task_runs` row burns when no account is linked.

## Cross-cutting conventions

- **Error type: `Result<_, String>`** end-to-end. Errors flow into `logger::*` or HTTP bodies; downstream code matches on **tag-prefix strings** (`"AniList rate-limited"`, `"AniList unavailable"`, `"AniList not found"`, `"Download client not configured"`, MAL refresh-failure prefixes, etc.) not enum discriminants. New errors keep the prefix. If you introduce a typed error, carry the prefix in its `Display`.
- **`spawn_blocking` discipline**: anything that can block >5ms goes through `tokio::task::spawn_blocking`. Current sites: `bcrypt::hash` / `verify` (~50ms), post-processing file ops (BD episodes are 1–4 GB), rtorrent recursive `fs::remove_*` after `d.erase`, the directory walk in `handlers::library::pages`, the `warm_timing_equalizer` startup pre-pay.
- **Mutex poisoning**: default is `.lock().unwrap()` (crash-loop on programmer error). The one deliberate-recovery site is `HYDRATED_CUMULATIVE` in `handlers::library::reconcile`. Don't add `.into_inner()` recovery to security-adjacent state (`LOGIN_FAILURES`).
- **FK policy**: every child of `series(id)` is `ON DELETE CASCADE` *except* `rss_seen` (NO ACTION — keep audit trail; `series_title` is stored alongside `series_id` so the trail stays readable when the FK is broken). `series::remove` must NULL out `rss_seen.series_id` for the series **before** the final DELETE — `PRAGMA foreign_keys = ON` is the sqlx default and a missed NULL surfaces as a hard DELETE failure.
- **Outbound `User-Agent: Ryokan/0.1`** is hardcoded at every call site (AL, Jikan, Kitsu, SeaDex, Nyaa, RSS, anibridge, artwork). Not tied to crate version. If a provider starts UA-filtering, grep `"Ryokan/0.1"`.
- **Logging**: `services::logger::{trace,debug,info,warn,error}(&db, category, message, detail).await` dual-emits to `tracing` + the `logs` table. Console filtering is `RUST_LOG`; table filtering is `RYOKAN_DB_LOG_LEVEL` (write-side floor). The 18-variant `LogCategory` enum (`models/log.rs`) is what System → Logs uses verbatim — adding a category requires updating `as_str` / `from_str` / display-name match arms. Categories: `Search`, `Grab`, `AutoSearch`, `Nyaa`, `AniList`, `Jikan`, `Kitsu`, `DownloadClient`, `Jellyfin`, `Media`, `Library`, `Auth`, `System`, `PostProcess`, `Quality`, `Scoring`, `ExternalSync`, `Rss`. Legacy `qbit` strings still parse to `DownloadClient` for old-URL compat.
- **Metadata fallback chain**: AniList → Jikan (MAL) → Kitsu. Activates on AL 403s or force flags. Series added via Jikan with no AL mapping store as `series.anilist_id = -mal_id` (negative-ID sentinel); every AL call site filters `id > 0`. **Consequence**: SeaDex (keyed by positive AL id) and AL airing-schedule queries are silently invisible to these series. Jikan/Kitsu episode caches use a negative-cache sentinel (`episode_number = 0, title = "__RYOKAN_EMPTY__"`) — read sites must special-case it or the chain hot-loops.
- **Parse-ordering in `services/media.rs` is load-bearing.** `parse_episode_number` / `parse_quality` regex branches have explicit ordering: `RE_SXEX` before bare-number, `OVA NN` before generic bare-number, trailing-marker ranges before the marker, WebRip before unified Web (issue #48), and the `RE_SXEX` guard before any dash-delimited branch. Each branch has a regression-guard test. Don't "tidy" the order; new branches go with a pinning test.
- **HTMX-aware redirects**: `handlers::responses::htmx_aware_redirect` is mandatory for any handler that does `Redirect::to`. Bare `Redirect::to` under body-wide hx-boost gets nested-rendered into the source page. `tests/htmx_redirect_audit.rs` is a CI-enforced lint. See `templates/CLAUDE.md` for the full rationale.
- **Hardcoded Nyaa hot path**: when adding indexer support, never refactor Nyaa into a generic Indexer trait — Nyaa stays out-of-band as the protected hot path.
- **Sonarr/Radarr shim auth**: `arr_auth::check_api_key` middleware accepts `X-Api-Key` header *or* `?apikey=` query (percent-encoded), constant-time compared via `subtle` against `config.sonarr_api_key` / `config.radarr_api_key`. Transient config-load failures return **503 + `Retry-After`** (not 500) so Seerr doesn't long-back-off the indexer. Sonarr at `/api/v3/...`; Radarr at `/radarr/api/v3/...` (Seerr only allows two Sonarr + two Radarr slots; both shims must coexist on one host/port). `aliased(&["/camelCase", "/lowercase"], handler)` collapses Seerr's case-variant doublings — don't redirect, some clients won't follow. Provider order is fixed AL-first then MAL — never honor user-facing source toggles for the shim.
- **No em dashes in user-facing prose** (templates, README, error messages, toast text). Use `;` or `.` Internal Rust comments / CLAUDE.md / commit messages are exempt. **US English** spellings (color, honor, favorite — not colour, honour, favourite).
- **Custom Formats** are Sonarr-style (TRaSH-Guides-compatible). The `SpecKind::SeaDexBest` variant in `services/custom_formats.rs` is a Ryokan-only extension matched against SeaDex picks at scoring time — accepted under both `Ryokan.SeaDexBestSpecification` and the shorter `SeaDexBestSpecification` implementation name, emitted in the long form on export. **Presence of a CF using `SeaDexBest` suppresses the separate `seadex_enabled` config toggle** so the CF and toggle don't double-count. Preserve that one-or-the-other invariant.
- **Post-processing import modes** (`services/post_processing/mod.rs::do_file_op`) keyed off `config.post_processing_mode` (validated to `hardlink` / `copy` / `move`):
  - `hardlink` (default, seed-safe — original stays for the client to keep seeding). Falls back to copy when `fs::hard_link` errors (cross-filesystem common case).
  - `copy`.
  - `move` — `fs::rename` same-fs; cross-fs uses copy-to-`.ryokan-tmp`-then-rename so a partial copy can't be observed at dst by a subsequent pass; source-remove after cross-fs move only logs a warning on failure.
  - The full op runs in `spawn_blocking`. Artwork fan-out uses `fs::hard_link` (banner ↔ backdrop sharing inode) with `fs::write` fallback.
- **Title language preference**: `config.title_language` is `romaji` / `english` / `native`; anything else coerces to `romaji` on save. Display-only — scoring and search are unaffected.
- **Nyaa category selection**: `services::quality::nyaa_categories_for_format` maps AniList `format` + `allow_non_english` into Nyaa category-ids. `MUSIC` always searches `1_1` (AMV) + `2_0` (Audio); everything else `1_2` (English-translated) by default or `1_0` (Anime All) when non-English is opted-in.
- **RSS dedup** (`services/rss/feed.rs`) keys `rss_seen` entries by `guid:<item.guid>` when present, falling back to item link. Tombstones survive across restarts. The RSS task uses `RSS_SYNC_LOCK` with `try_lock` so a manual "run now" during the 60s tick returns "RSS sync is already running" rather than queuing.
- **Auto-expand** (`services::auto_expand::expand_from_files`) is sibling-series detection inside a batch pack — distinct from megapack narrowing in the `DownloadClient` trait. Two call sites: grab-time (180s metadata wait inside a `tokio::spawn` after the HTTP handler returned) and import-time (safety net when grab-time bailed). Writes per-sibling rows into `grabbed_torrent_series` so post-processing routes each file into the correct sibling folder. **Transitive relation walk**: AL's relation graph has missing direct edges across split sagas (Monogatari case); `expand_from_files` fetches walkable neighbors capped at `auto_search::TRANSITIVE_WALK_MAX_FETCHES`, grafts their relations onto the parent before sibling detection. Failed neighbor fetches are soft. **Absolute-numbering offsets**: each sibling route carries `episode_offset` so stored episode numbers are *effective* (post-offset) values — upgrade-search for "Egypt-hen E1" finds the route whose files were originally numbered E25 on disk. **Grab-tag backfill** writes `episode_quality_tags` + `episode_grab_history` rows for every sibling at grab time so each sibling's page shows `grabbed` immediately. **AL-overflow** episodes (parsed episode > `parent_episode_numbers`, e.g. smol Owarimonogatari BD splitting aired E1 into two files producing disk-level E13) get the same backfill so the overflow row renders during download rather than after import. **Parent-route fallback**: every unclaimed media file routes to the parent series; unclaimed alongside sibling routes triggers a `warn` log so misdetection surfaces in System → Logs. **Not load-bearing under failure** — if grab-time aborts, the grab row is already recorded and import-time rescues it; never synthesize an empty grab row to mark auto-expand "complete."

## Process-wide global state

Static `LazyLock`s crossing request boundaries — inventory:

- `services::anilist::rate_limit::*` — throttle state. Deep dive: `src/services/anilist/CLAUDE.md`.
- `services::anilist::DETAIL_CACHE` — per-AL-id memoization; partial-recovery paths read from it after a failed batch fetch.
- `services::post_processing::POST_PROC_LOCK` — serializes the post-processing task. Held across the full import sweep including the 60s ffprobe timeout.
- `services::rss::RSS_SYNC_LOCK` — `try_lock` + readable error so manual run during a tick doesn't queue.
- `services::external_sync::EXTERNAL_SYNC_LOCK` — same shape; Sync-now click during the supervised loop returns "already running."
- `services::crypto::ENCRYPTION_KEY` — the AEAD key. Crashing at LazyLock force is intentional.
- `handlers::auth::LOGIN_FAILURES` — per-IP throttle map. See `src/handlers/auth/CLAUDE.md`.
- `handlers::auth::TRUST_PROXY_HEADERS` / `COOKIE_SECURE` — env snapshots.
- `handlers::system::CLIENT_LOG_HITS` — sliding-window rate limit on the client-side log-ingest endpoint.
- `handlers::library::reconcile::HYDRATED_CUMULATIVE` — first-grab `cumulative_prior_episodes` lazy-hydration dedup. Uses `.unwrap_or_else(|p| p.into_inner())` recovery.
- `services::rss::feed::RSS_HTTP_CLIENT` — pre-configured reqwest client.

## Database & migrations

- sqlx 0.8 with **no compile-time query checking** (zero `sqlx::query!` invocations); `macros` feature kept for derive macros only. SQLite statically bundled; no system `libsqlite3` needed at runtime.
- Migrations in `models/migrations/`. New columns use `ALTER TABLE ... ADD COLUMN ... .ok()` to ignore-if-present (idempotent, file-free). The exception is **one-shot migrations** that must run exactly once and can't self-guard (data rewrites, seed-table fixups). Those live next to their model (`models/group_source_map.rs` is the current example), write an ID row into `schema_migrations` after running, and skip on subsequent boots via `migration_already_applied(db, id)`. Don't invent a per-migration config flag — that's what `schema_migrations` is for. No separate migration files on disk.

## Routes

`main.rs` merges three logical groups:

- `public_routes` — unauthenticated endpoints (`/login`, `/setup`, `/forgot-password`) wrapped in `csrf_public` so POST paths still enforce Origin/Referer.
- `protected_routes` — everything behind `require_auth` (library, search, downloads, settings, system, the API endpoints the web UI calls — `/api/health`, `/api/library/*`, `/api/progress/*`).
- `sonarr_routes` + `radarr_routes` — merged **outside** the cookie-auth layer with `arr_auth` middleware instead.

Compression layer wraps everything (`CompressionLayer::new().br(true).gzip(true)`). `/static/*` is served via `ServeDir` with `Cache-Control: public, max-age=3600` (one hour — short enough to pick up edited CSS during local dev on hard reload).

There is no `/healthz`. The closest is `/api/health` (auth-gated, JSON status). **The Docker healthcheck deliberately probes `/login`** (200 with no auth, no config dependency, no side effects).

## Docker

- `docker-entrypoint.sh` runs as root, ensures a `ryokan` user/group with UID/GID matching `PUID` / `PGID` (creating or `usermod`/`groupmod`-ing in place), chowns `/data` to that user, then execs Ryokan under `gosu`. Data-volume chown is gated on "not already correctly owned" so a warm start is a no-op scan; on a PUID change it quietly re-chowns. **User-mounted `/downloads` and `/media/*` paths are intentionally left alone** — chowning a 10TB media library would stall startup for hours and could clobber ownership the rest of an *arr stack relies on. User picks PUID/PGID that already owns those paths on the host (matches linuxserver.io convention).
- Dockerfile cache: `Cargo.toml` + `Cargo.lock*` (glob — tolerates first build without lockfile) copied first, empty `fn main() {}` written to `src/main.rs`, `cargo build --release` primes the dependency cache. Real `src/`, `templates/`, `static/` copied after, `touch src/main.rs` + second `cargo build --release` rebuilds only Ryokan's own code. **`static/` is copied twice** — once into builder (for `include_str!` of `default_custom_formats.json`), once into runtime (for ServeDir). Not a bug.

## Tests (one-paragraph summary; deep dive in `tests/CLAUDE.md`)

Most tests live inline as `#[cfg(test)] mod tests`; integration tests in `tests/` are binary crates that import `ryokan` as a library and require `--features test-support` (each `[[test]]` target declares this so plain `cargo test` silently skips them). Topic-split submodule pattern when a test module exceeds ~1500 LoC. Env-gated `live_smoke` tests in download-client impls. Browser e2e via `fantoccini` + WebDriver behind `--features browser-e2e` for HTMX UI assertions.

## CI

- **`rust.yml`** — push to main/dev + PR to main, `rust-${{ github.ref }}` concurrency with `cancel-in-progress: true`. Order: `cargo fmt --all -- --check` → `cargo clippy --workspace --all-targets --features test-support -- -D warnings` → `cargo build --workspace --locked` → `cargo test --workspace --locked --features test-support`. Always run all four locally before pushing — fmt is easy to forget and the pipeline short-circuits on it before the slower steps.
- **`cargo-audit.yml`** — push to main, PRs to main touching `Cargo.lock` / `Cargo.toml`, weekly cron Mondays 09:00 UTC. The sqlx `default-features = false` exists to prune the phantom `rsa` RUSTSEC-2023-0071 advisory at source — **don't re-enable sqlx defaults** without confirming upstream resolution.
- **`docker.yml`** — push to main/dev + `v*.*.*` tags. Per-platform matrix on native amd64 + arm64 runners (no QEMU). Pushes digest-only refs to `ghcr.io/johnthreekay/ryokan` (lowercased — GHCR requires lowercase image names; the repo is capitalized "Ryokan").
- **`claude.yml`** — Claude Code workflow integration on `@claude` mentions. `actions: read` is required for reading CI results on the PR being commented on.
