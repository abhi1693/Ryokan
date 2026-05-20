# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with this repository. **It's the base layer**; deeper notes for specific subsystems live in nested CLAUDE.md files (linked under "Code Layout" below) and load automatically when work touches those subtrees.

## Project Overview

Ryokan is a self-hosted anime PVR (personal video recorder) written in Rust. It searches Nyaa for anime releases, scores them by quality, and dispatches grabs to a download client. Five clients are supported behind a common `DownloadClient` trait — **four BT clients** (qBittorrent, Deluge, Transmission, rTorrent) and **one Usenet client** (SABnzbd) — with a multi-client routing pool keyed by indexer pin and per-protocol default. Acquisition surfaces beyond the built-in Nyaa search: a **torznab/newznab indexer system** for Prowlarr/Jackett (`services/indexers/`), **direct RSS feeds** (e.g. SubsPlease), and an **autobrr webhook** (`/api/webhook/autobrr`) for IRC-announce push. AniList is the primary metadata source with MAL (via Jikan) and Kitsu as fallbacks.

Release scoring combines Sonarr-style **Custom Formats** (TRaSH-Guides-compatible), a multi-layer **source classification pipeline**, and optional **SeaDex** (releases.moe) authoritative picks. Ryokan also exposes a Sonarr/Radarr-compatible API shim (**anibridge**) so Seerr and similar tools can request anime through it.

**Nyaa stays out-of-band.** The torznab indexer system runs *alongside* the direct Nyaa scraper, not in place of it — the search pipeline dispatches to Nyaa-direct + fans out to `Indexer` impls in parallel and merges. Source classification reads Nyaa's description body directly; conforming Nyaa to the generic trait would have hidden that coupling.

## Build & Run

```bash
cargo build [--release]
cargo run                                                                # 0.0.0.0:8978, creates data/ryokan.db
docker compose up -d --build

# Tests — `cargo t` is the canonical alias (defined in .cargo/config.toml)
cargo t                                                                  # = cargo nextest run --features test-support
cargo t <test_name>                                                      # single-test filter (same syntax as cargo test <name>)
cargo nextest run --workspace --features test-support                    # explicit form
cargo test --workspace --locked --features test-support                  # CI-shape fallback (doc tests, --locked)

# Lint / format / coverage
cargo fmt --all -- --check                                               # CI runs this first; short-circuits on failure
cargo clippy --workspace --all-targets --features test-support -- -D warnings   # CI form — `--all-targets` covers tests too
cargo llvm-cov --workspace --features test-support                       # coverage; cargo-llvm-cov 0.8+ installed locally
```

**Toolchain requirements:**

- **Rust 1.95+** (enforced via `package.rust-version`).
- **C/C++ toolchain** + **`cmake`** — needed because two crates compile native code at build time: `anitomy-sys` ships C++ source it builds via `cc` (anime title tokenization), and `aws-lc-sys` builds aws-lc via cmake (rustls' crypto provider since reqwest 0.13). SQLite is statically bundled by sqlx's `sqlite` feature; no system `libsqlite3` is needed at runtime. No OpenSSL headers either — TLS is pure-Rust rustls + aws-lc.
- **`mold` + `clang`** — `.cargo/config.toml` pins `linker = "clang"` with `-fuse-ld=mold` for x86_64 + aarch64 Linux. Cuts incremental link time 3-5× vs ld/lld; without them a build fails with `"linker 'clang' not found"` or `"ld.mold not found"`. Install: `sudo pacman -S mold clang` (Arch) / `sudo apt install mold clang` (Debian/Ubuntu). CI installs both via apt before the build steps.
- **`cargo-nextest`** for `cargo t`. Falls through to `cargo test` if not installed, but nextest is the default. Install: `cargo install cargo-nextest --locked`.
- **`cargo-llvm-cov`** for coverage (optional). Install: `cargo install cargo-llvm-cov --locked`.

**Test profile / nextest config:**

- `[profile.test.package."*"] opt-level = 1` in `Cargo.toml` — dependencies build optimized so wiremock/sqlx/regex hot paths run 2-3× faster; Ryokan's own code stays at `opt-level = 0` for fast incremental rebuilds.
- `.config/nextest.toml`: default profile retries failures once (matches `cargo test` behavior so a flaky wiremock port-bind doesn't fail the whole run), slow-warn at 60s and terminate at 180s. The `ci` profile bumps retries to 2, `fail-fast = false`, and emits `junit.xml`.

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
| `RYOKAN_KEY_FILE_PATH` | `data/.ryokan-key` | Override for the auto-generated key file path. Default is CWD-relative, which works for `cargo run` (CWD = repo root) but breaks under Docker where WORKDIR is `/app` and the entrypoint chowns `/data` (absolute) not `/app/data` — the Docker image sets this to `/data/.ryokan-key` so the key co-locates with the SQLite DB on the persistent volume. Ignored when `RYOKAN_ENCRYPTION_KEY` is set (env-key wins). |
| `RYOKAN_ANIBRIDGE_CACHE_DIR` | `data/cache/anibridge` | Override for the anibridge TMDB↔AL mappings cache directory (the ~9MB JSON blob anibridge fetches from GitHub). Same CWD-relative-vs-Docker-volume footgun as `RYOKAN_KEY_FILE_PATH`. Docker sets this to `/data/cache/anibridge`; if unset and CWD doesn't match the data volume, the disk cache silently fails to persist and every restart re-downloads. |
| `RYOKAN_NYAA_API_BASE` | `https://nyaa.si` | Nyaa scraper base URL override. **Test-only seam** for the `tests/auto_search_e2e.rs` wiremock fixture; production shouldn't set this. Re-read on every Nyaa request, not cached. |
| `RYOKAN_KITSU_API_BASE` | `https://kitsu.io/api/edge` | Kitsu fallback API base override. **Test-only seam** for `tests/metadata_sync_e2e.rs`; production shouldn't set this. Re-read on every Kitsu request, not cached. |
| `RYOKAN_ANIBRIDGE_MAPPINGS_URL` | GitHub raw URL | anibridge TMDB↔AL mappings JSON URL override. **Test-only seam** for the wiremock fixture in `services/anibridge` tests; production shouldn't set this. |
| `RYOKAN_QBIT_E2E` / `RYOKAN_DELUGE_E2E` / `RYOKAN_TRANSMISSION_E2E` / `RYOKAN_RTORRENT_E2E` / `RYOKAN_SAB_E2E` | unset | Test-only. Opt inline `live_smoke` tests into running against a real client on localhost. See `src/services/download_client/CLAUDE.md`. |
| `QBIT_PASS` | `adminadmin` | Test-only; qBit live_smoke password (qBit-on-first-start default). |
| `RYOKAN_SAB_URL` / `RYOKAN_SAB_API_KEY` / `RYOKAN_SAB_CAT` | `http://localhost:8080` / unset / `ryokan-test` | Test-only; SAB live_smoke. API key has no usable default. |

There is no `.env` loader — env vars come from the process environment directly (`docker-compose.yml` in prod, the user's shell in dev). The choice is deliberate; don't add one.

## Stack at a glance

Axum 0.8 + Tokio · SQLite via sqlx 0.8 (`default-features = false` + `runtime-tokio`,`sqlite`,`macros`; **no compile-time query checking** — all queries are runtime strings; the `macros` feature is kept only for the derive macros) · reqwest 0.13 + rustls + aws-lc + rustls-platform-verifier · Askama 0.16 templates (compiled in at build time) · HTMX 2.x vendored under `static/vendor/`, body-wide `hx-boost="true"` active · `regex-lite` default, `fancy-regex` only inside `services/custom_formats/` (PCRE look-around required by TRaSH CFs) · `anitomy` 0.2 for release-title tokenization (the `anitomy-sys` crate builds bundled C++ source via `cc` at build time — not vendored into Ryokan) · `ammonia` 4 for HTML sanitize · `bcrypt` 0.19 cost 10, `subtle` 2 for constant-time compare, `rand` 0.10 + `hex` for session tokens · `utoipa` 5 + `utoipa-swagger-ui` 9 mount Swagger UI at `/api-docs`, JSON at `/api-docs/openapi.json` · `tracing` 0.1 + `tracing-subscriber` 0.3 (env-filter) + a separate DB-backed `logs` table via `services::logger`.

`async-trait` 0.1 is used specifically for `DownloadClient` because native async-fn-in-traits doesn't yet produce `Send`-bound futures by default, which is required for `Arc<dyn DownloadClient>` storage on a multi-threaded Tokio runtime. Non-object-safe traits use native syntax fine.

## Code Layout (`src/`)

- **`handlers/`** — Axum route handlers. Page/feature modules: `library`, `search`, `downloads`, `settings`, `system`, `auth`, `media`, `help`, `progress`, `grab` (interactive file-picker — `/api/grab/{preview,confirm,heartbeat,cancel}`), `oauth` (AL+MAL link/submit/unlink/sync-now under `/settings/oauth/*`), `webhook/` (autobrr push at `/api/webhook/autobrr`), `calendar` (issues #115 + #116 — in-app `/calendar` page + iCal feed at `/api/calendar.ics`), `api_keys` (issue #114 scoped API keys CRUD), `scoped_auth` (`require_calendar_scope` and friends), `notifications` (issue #118 test-provider endpoint), plus `sonarr_compat` / `radarr_compat` (the anibridge shims), `arr_auth` (shared API-key middleware), `arr_shared` (DTOs shared between shim sides). Auth deep dive: **`src/handlers/auth/CLAUDE.md`**.
- **`services/`** — Business logic + external API clients.
  - Metadata chain: `anilist/` → `jikan` → `kitsu`, plus `mal` (OAuth-authenticated, distinct from `jikan` which is the public-fallback path).
  - `download_client/` — trait + **5 client impls** (qBit / Deluge / Transmission / rtorrent / **sabnzbd**) + `DownloadClientPool` for multi-client routing. Wire quirks + pool: **`src/services/download_client/CLAUDE.md`**.
  - `indexers/` — torznab/newznab indexer abstraction (`Indexer` trait, `Release`/`SearchQuery`/`IndexerCaps`, `torznab/` impl). Runs alongside the Nyaa hot path. Deep dive: **`src/services/indexers/CLAUDE.md`**.
  - `source/` + sibling `source_*.rs` — multi-layer source classification pipeline. `quality.rs`, `scoring.rs`, `custom_formats/`, `upgrade.rs`, `auto_search/`, `auto_expand.rs`. Classifier deep dive: **`src/services/source/CLAUDE.md`**.
  - `external_sync/` — AL/MAL watch-list sync (delta cursor, force-full-resync, removal detection, auth-rejection taxonomy).
  - `crypto.rs` — AEAD wrapper for OAuth tokens. `oauth_state.rs` (PKCE verifier store), `user_score.rs`.
  - `nyaa/`, `seadex.rs`, `rss/`, `anibridge.rs`, `interactive_search_cache.rs`, `indexer_catalog.rs` (provisioning), `task_registry.rs` (in-memory supervised-task lifecycle).
  - `post_processing/` — moves/renames completed downloads (hardlink/copy/move). `nfo.rs`, `artwork.rs`, `media.rs`, `jellyfin.rs`.
  - `calendar.rs` (calendar reader — joins `episode_airings ⨝ series`), `airing_refresh.rs` (12h supervised stamper that walks AL `Page.airingSchedules` and writes `episode_airings`).
  - `notifications/` — outbound providers (`webhook.rs`, `discord.rs`), `event.rs` event taxonomy, `store.rs` cache.
  - `monitoring.rs`, `progress.rs`, `library_link.rs`, `grab_commit.rs`, `grab_sweep.rs`, `metadata_sync.rs`, `html.rs`, `sanitize.rs`, `logger.rs`, `relative_time.rs`.
  - AL deep dive: **`src/services/anilist/CLAUDE.md`**.
- **`models/`** — DB access + schema. `migrations/` owns most `CREATE TABLE` / `ALTER TABLE`. A few tables own their `CREATE TABLE` next to their model module (`custom_formats`, `group_source_map` + `schema_migrations`, `media_probe_cache`, `nyaa_description_cache`). Notable additions beyond the obvious: `download_clients` (one row per configured client, keyed by id; `is_default` rows are scoped per-protocol — torrent vs usenet — both can coexist), `indexers` (torznab/newznab rows with caps cache + per-indexer `download_client_id` pin), `direct_rss_feeds` (per-feed configuration for direct sources like SubsPlease), `episode_airings` (#115/#116 — local cache of AL airing schedules; FK on `series_id` is `ON DELETE CASCADE`, range-scan index on `airing_at`; the calendar reader joins this against `series` instead of round-tripping to AL per-request).
- **`templates/`** — Askama templates. HTMX patterns + per-page JS lifecycle: **`templates/CLAUDE.md`**.
- **`static/`** — Modular CSS (`base.css`, `topbar.css`, `forms.css`, `badges.css`, `modals.css` loaded everywhere + per-page `pages/<name>.css`) and per-page `static/js/*.js`. No bundler. Non-CSS/JS assets: `static/default_custom_formats.json` (embedded via `include_str!` at compile time), `static/fonts/` (Murecho woff2 subsets — Latin + Japanese, weights 500-800), `static/licenses/` (third-party LICENSE texts surfaced in System → About), `static/vendor/` (HTMX bundles).
- **`tests/`** — Integration tests (binary crates importing `ryokan` as a lib). Browser-e2e harness for HTMX UI assertions. **`tests/CLAUDE.md`**.
- **`tests/fixtures/trash-guides-anime/`** — 29 vendored real-shape TRaSH-Guides anime CF JSONs. **Test-only corpus** for the CF parser (`services/custom_formats/parser.rs::TRASH_ANIME_FIXTURES`); each is `include_str!`'d so the test needs no filesystem / network access. Doubles as a regression guard for the object/array `fields` bug — if either exporter shape stops parsing, every fixture breaks at once. Not user-facing defaults (those live in `static/default_custom_formats.json`).

## AppState

Defined in `src/lib.rs`. Fields:

- `db: SqlitePool`.
- `download_clients: DownloadClientsCache = Arc<RwLock<Arc<DownloadClientPool>>>` — **multi-client routing pool** (id-keyed `HashMap<i64, Arc<dyn DownloadClient>>` plus `default_torrent_id` and `default_usenet_id`). The whole `Arc<DownloadClientPool>` swaps atomically on `services::download_client::rebuild_clients_cache` (Settings → Connections → Downloads add/edit/delete). Lookup at grab time is a `HashMap::get` against the cheap-cloned inner Arc — read lock releases before dispatch. Resolved through `AppState::client_for_indexer` / `client_for_nyaa` / `default_download_client` / `client_by_id` / `resolve_grab_client`.
- `jellyfin: Arc<RwLock<Option<JellyfinClient>>>`.
- `custom_formats: CompiledCfCache = Arc<RwLock<Arc<Vec<_>>>>` — swap-on-write so the scoring hot path runs lock-free. Handlers clone the inner `Arc` under read lock; rebuilt by `custom_formats::rebuild_cf_cache` on CF create/update/delete.
- `indexers: IndexerCache = Arc<RwLock<Arc<Vec<Arc<dyn Indexer>>>>>` — same swap-on-write shape as `custom_formats`. Avoids rebuilding `reqwest::Client` instances on every per-target search.
- `progress: ProgressRegistry` — long-running user-triggered jobs (currently manual auto-search). The frontend mints an opaque `progress_id`, the trigger handler binds it via `register(...).await`, and `/api/progress/{id}` drains buffered events.
- `users_exist: Arc<AtomicBool>` — flip-to-true-once cache so `require_auth` skips a `SELECT COUNT(*) FROM users` per protected request once setup is complete. While false, the middleware still hits the DB on the setup-pending path so a fresh `/setup` submission is picked up on the very next request.
- `interactive_search_cache: InteractiveSearchCache` — 5-minute TTL for interactive-search results so rapid modal reloads during UI iteration reuse the previous Nyaa hit. Scoped to interactive only; auto-search / RSS / manual grabs continue to hit Nyaa directly.
- `oauth_state: OAuthStateStore` — in-memory store for pending MAL OAuth attempts (PKCE verifier between `/start` and `/submit`). 10-minute TTL. AL has no entry — implicit grant, no per-attempt server state.
- `start_time: chrono::DateTime<Utc>` — wall-clock timestamp captured at boot. Used by Sonarr/Radarr `system_status` so Seerr's UI pill reports actual liveness; the prior hardcoded `2024-01-01T00:00:00Z` claimed the indexer had been up over a year regardless of when Ryokan restarted.
- `tasks: TaskRegistry = Arc<RwLock<HashMap<&'static str, Arc<TaskState>>>>` — supervised-task lifecycle metadata. Each `supervise()` loop registers itself once at startup, mutates atomics on its `Arc<TaskState>` on every iteration (no further locking until a snapshot read). Distinct from the DB-backed `scheduled_task_runs` table — `tasks` is the in-memory live status view (running / backoff, restart count, last exit kind, current backoff) served at `/api/system/tasks` for the System page.
- `dc_status_cache: DcStatusCache = Arc<Mutex<HashMap<i64, (Instant, DcStatusEntry)>>>` — per-download-client probe-result cache, **10-min TTL** (`DC_STATUS_CACHE_TTL`). Without it, every page load and every hx-boost re-entry into Settings → Download Clients re-runs the network probe (50-500ms healthy / up to 5s unreachable) and the "Probing…" pills flash to real status at staggered times. Wiped per-id on every `download_clients` row CRUD so a credential edit re-probes immediately rather than masking failures for the full TTL.
- `notification_providers: NotificationProviders` — issue #118 swap-on-write cache for outbound notification providers (webhook / Discord). Same `Arc<RwLock<Arc<Vec<_>>>>` shape as `custom_formats` / `indexers` — read lock releases before the per-provider fan-out begins. `services::notifications::dispatch` early-returns on empty so every hook point is a no-op until at least one provider is configured.

## Download-client routing (multi-client pool)

`AppState::client_for_indexer_with_id` resolves:

1. **Indexer's `download_client_id` pin** if set and present in the pool.
2. **Per-protocol default** — torznab indexer with no pin → `default_torrent_id`; newznab → `default_usenet_id`. Unknown indexer kind / not in cache snapshot → torrent default.
3. None — caller surfaces "no download client configured."

`client_for_nyaa` reads `config.nyaa_download_client_id` then **always falls back to torrent default** (Nyaa items are magnets / .torrent URLs; usenet fallback would just trip the protocol guard at add-time).

`default_download_client` returns the **torrent** default — every internal default-only call site is torrent-flavored (manual grabs, library re-grab, RSS / upgrade "is anything configured" gates). Usenet routing always goes through an indexer pin or its protocol default.

**`resolve_grab_client(download_client_id, hash)`** — used by post-processing's per-grab routing. Three-layer fallback:
1. The stamped client id, if it still exists in the pool.
2. **Hash-shape heuristic** — `hash.starts_with("SABnzbd_nzo_")` routes to ANY usenet client in the pool. Old grabs predating the `download_client_id` stamp migration have NULL stamps; without this branch, a SAB nzo_id sent to qBit's `delete` endpoint silently 200s (qBit ignores unknown hashes), and the user's symptom is "delete-from-disk leaves the SAB job alive forever."
3. The torrent default (legacy fall-through; correct for BT v1 infohashes).

`grabbed_torrents.download_client_id` is stamped at grab time so post-processing routes back through the same client even if defaults change.

## Background tasks

Each runs as a `tokio::spawn` loop in `main.rs` wrapped in `supervise()`. The supervised list (11 tasks: `progress_sweep`, `rss_sync`, `post_processing`, `grab_sweep`, `external_sync`, `cleanup`, `library_classify`, `metadata_refresh`, `upgrade_search`, `anibridge_refresh`, `airing_refresh`) and intervals are inline in `main.rs` — grep `supervise(&` for the canonical names. Non-obvious bits the code doesn't explain on its own:

- **Restart policy is exponential backoff**, not flat 5s: `MIN_BACKOFF = 5s`, `MAX_BACKOFF = 30 min`, `HEALTHY_RUNTIME = 60s`. Healthy ≥60s run resets to 5s; <60s exit doubles up to 30 min.
- **Two layers of status tracking** — `scheduled_task_runs` DB table is the historical record (System page per-task history pane); `AppState.tasks: TaskRegistry` is the in-memory live status served at `/api/system/tasks` (lock-free atomics, snapshot-on-read).
- **`external_sync` quirks**: 1-min outer tick re-reads `config.external_sync_interval_minutes` (Settings change takes effect within 60s); `consecutive_errors`-driven exponential skip (2^errors intervals, capped at 5 → 32× multiplier, with 24h ceiling so a 7-day cadence can't get pushed seven months by five errors); `has_linked_account` early-out so no `scheduled_task_runs` row burns when no account is linked.
- **`airing_refresh` shape (#115/#116)**: 12h supervised tick + manual trigger at `POST /api/tasks/airing-refresh`; both share `services::airing_refresh::AIRING_REFRESH_LOCK` (`tokio::sync::Mutex<()>`) so a manual click during the scheduled tick returns "already running" rather than queuing. Library add path also calls `refresh_for_series` inline (fire-and-forget) so freshly-added series show up in the calendar without waiting for the next 12h tick. Past-window prune retains 14 days. Mirrors Sonarr's `Episode.AirDateUtc` shape — stamp once, serve from DB on the calendar's hot path; the calendar reader has no in-process cache because the indexed range-scan is cheap enough on its own.

## Cross-cutting conventions

- **Error type: `Result<_, String>`** end-to-end. Errors flow into `logger::*` or HTTP bodies; downstream code matches on **tag-prefix strings** (`"AniList rate-limited"`, `"AniList unavailable"`, `"AniList not found"`, `"Download client not configured"`, MAL refresh-failure prefixes, etc.) not enum discriminants. New errors keep the prefix. If you introduce a typed error, carry the prefix in its `Display`.
- **`spawn_blocking` discipline**: anything that can block >5ms goes through `tokio::task::spawn_blocking`. Current sites: `bcrypt::hash` / `verify` (~50ms), post-processing file ops (BD episodes are 1–4 GB), rtorrent recursive `fs::remove_*` after `d.erase`, the directory walk in `handlers::library::pages`, the `warm_timing_equalizer` startup pre-pay.
- **Mutex poisoning**: default is `.lock().unwrap()` (crash-loop on programmer error). The one deliberate-recovery site is `HYDRATED_CUMULATIVE` in `handlers::library::reconcile`. Don't add `.into_inner()` recovery to security-adjacent state (`LOGIN_FAILURES`).
- **FK policy**: every child of `series(id)` is `ON DELETE CASCADE` *except* `rss_seen` (NO ACTION — keep audit trail; `series_title` is stored alongside `series_id` so the trail stays readable when the FK is broken). `series::remove` must NULL out `rss_seen.series_id` for the series **before** the final DELETE — `PRAGMA foreign_keys = ON` is the sqlx default and a missed NULL surfaces as a hard DELETE failure.
- **Outbound `User-Agent: Ryokan/0.1`** is hardcoded at every call site (AL, Jikan, Kitsu, SeaDex, Nyaa, RSS, anibridge, artwork). Not tied to crate version. If a provider starts UA-filtering, grep `"Ryokan/0.1"`.
- **Logging**: `services::logger::{trace,debug,info,warn,error}(&db, category, message, detail).await` dual-emits to `tracing` + the `logs` table. Console filtering is `RUST_LOG`; table filtering is `RYOKAN_DB_LOG_LEVEL` (write-side floor). The 19-variant `LogCategory` enum (`models/log.rs`) is what System → Logs uses verbatim — adding a category requires updating `as_str` / `from_str` / display-name match arms. Categories: `Search`, `Grab`, `AutoSearch`, `Nyaa`, `AniList`, `Jikan`, `Kitsu`, `DownloadClient`, `Jellyfin`, `Media`, `Library`, `Auth`, `System`, `PostProcess`, `Quality`, `Scoring`, `ExternalSync`, `Rss`, `Notifications`. Legacy `qbit` strings still parse to `DownloadClient` for old-URL compat.
- **Metadata fallback chain**: AniList → Jikan (MAL) → Kitsu. Activates on AL 403s or force flags. Series added via Jikan with no AL mapping store as `series.anilist_id = -mal_id` (negative-ID sentinel); every AL call site filters `id > 0`. **Consequence**: SeaDex (keyed by positive AL id) and AL airing-schedule queries are silently invisible to these series. Jikan/Kitsu episode caches use a negative-cache sentinel (`episode_number = 0, title = "__RYOKAN_EMPTY__"`) — read sites must special-case it or the chain hot-loops.
- **Parse-ordering in `services/media.rs` is load-bearing.** `parse_episode_number` / `parse_quality` regex branches have explicit ordering: `RE_SXEX` before bare-number, `OVA NN` before generic bare-number, trailing-marker ranges before the marker, WebRip before unified Web (issue #48), and the `RE_SXEX` guard before any dash-delimited branch. Each branch has a regression-guard test. Don't "tidy" the order; new branches go with a pinning test.
- **HTMX-aware redirects**: `handlers::responses::htmx_aware_redirect` is mandatory for any handler that does `Redirect::to`. Bare `Redirect::to` under body-wide hx-boost gets nested-rendered into the source page. `tests/htmx_redirect_audit.rs` is a CI-enforced lint. See `templates/CLAUDE.md` for the full rationale.
- **Hardcoded Nyaa hot path**: when adding indexer support, never refactor Nyaa into a generic Indexer trait — Nyaa stays out-of-band as the protected hot path.
- **Sonarr/Radarr shim auth**: `arr_auth::check_api_key` middleware accepts `X-Api-Key` header *or* `?apikey=` query (percent-encoded), constant-time compared via `subtle` against `config.sonarr_api_key` / `config.radarr_api_key`. Transient config-load failures return **503 + `Retry-After`** (not 500) so Seerr doesn't long-back-off the indexer. Sonarr at `/api/v3/...`; Radarr at `/radarr/api/v3/...` (Seerr only allows two Sonarr + two Radarr slots; both shims must coexist on one host/port). `aliased(&["/camelCase", "/lowercase"], handler)` collapses Seerr's case-variant doublings — don't redirect, some clients won't follow. Provider order is fixed AL-first then MAL — never honor user-facing source toggles for the shim. The `system_status` endpoint reports `AppState.start_time` (boot wall-clock) for the Seerr liveness pill.
- **Webhook auth**: `handlers::webhook::autobrr` accepts `X-Api-Key` header *or* `?apikey=` query, constant-time compared against `config.autobrr_api_key`. Empty configured key returns **503 + Retry-After** ("autobrr webhook is disabled") rather than treating empty-key match as success. Mismatch is 401. Key rotation is a separate handler at `POST /settings/autobrr/regenerate-key` (`handlers/settings/autobrr_key.rs`) — distinct from the regular Settings save so an accidental tab POST can't silently rotate or wipe the key. Future webhook receivers (e.g. radarr companion) land as siblings under `handlers/webhook/` so the auth + body-shape pattern stays consistent.
- **No em dashes in user-facing prose** (templates, README, error messages, toast text). Use `;` or `.` Internal Rust comments / CLAUDE.md / commit messages are exempt. **US English** spellings (color, honor, favorite — not colour, honour, favourite).
- **Custom Formats** are Sonarr-style (TRaSH-Guides-compatible). The `SpecKind::SeaDexBest` variant in `services/custom_formats/` is a Ryokan-only extension matched against SeaDex picks at scoring time — accepted under both `Ryokan.SeaDexBestSpecification` and the shorter `SeaDexBestSpecification` implementation name, emitted in the long form on export. **Presence of a CF using `SeaDexBest` suppresses the separate `seadex_enabled` config toggle** so the CF and toggle don't double-count. Preserve that one-or-the-other invariant.
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
- `services::jikan::DETAIL_CACHE` — per-MAL-id memoization (parallel to AL's).
- `services::jikan::JIKAN_COOLDOWN_UNTIL` — Jikan's equivalent of AL's rate-limit machine. When Jikan 429s, sets "unavailable until Instant" so subsequent calls return a clean cooldown error instead of hammering and piling up more 429s. **60s default, 300s max.** Honors the response's `Retry-After` when present.
- `services::source_description::LAST_FETCH` — Process-global throttle for live Nyaa description fetches. **`MIN_FETCH_INTERVAL = 1s`** (Nyaa doesn't publish a rate limit but tarpits scrape-looking patterns fast). Holds the mutex *across the sleep*, deliberately serializing all fetches so two concurrent classifier calls can't burst.
- `services::anibridge::CACHE` (`RwLock<Option<CacheState>>`) + `DOWNLOAD_LOCK` (`Mutex<()>`) — In-process TMDB↔AniList mapping cache + single-flight download serializer (no TOCTOU race). The `anibridge_refresh` 24h task warms `CACHE`; the disk cache underneath uses conditional GET via stored meta, with multiple `spawn_blocking` sites for read/meta/write paths and an unconditional-read fallback on the 304 path.
- `services::auto_search::seadex_lookup::SEADEX_CACHE` + `SEADEX_INFLIGHT` — per-AL-id memoization for releases.moe lookups + thundering-herd guard so a burst of concurrent scoring calls share a single in-flight fetch.
- `services::anilist::SEARCH_CACHE` — per-query AL search memoization (parallel to the listed `DETAIL_CACHE`).
- `services::post_processing::POST_PROC_LOCK` — serializes the post-processing task. Held across the full import sweep including the 60s ffprobe timeout.
- `services::rss::RSS_SYNC_LOCK` — `try_lock` + readable error so manual run during a tick doesn't queue.
- `services::external_sync::EXTERNAL_SYNC_LOCK` — same shape; Sync-now click during the supervised loop returns "already running."
- `services::upgrade::UPGRADE_LOCK` — same `try_lock` shape; manual upgrade-search button returns "already running" rather than queuing during the supervised tick.
- `services::airing_refresh::AIRING_REFRESH_LOCK` — same `try_lock` shape (#115/#116); the 12h supervised tick and the `POST /api/tasks/airing-refresh` manual trigger share this lock so a click during the scheduled tick returns "already running" rather than double-stamping the AL budget.
- `services::nyaa::NYAA_CONCURRENCY` (`Semaphore`) — bounds Nyaa fan-out concurrency so a wide auto-search target list can't burst-attack the scraper.
- `services::crypto::ENCRYPTION_KEY` — the AEAD key. Crashing at LazyLock force is intentional.
- `handlers::settings::CONFIG_WRITE_LOCK` (`tokio::sync::Mutex<()>`) — serializes handler-level read-modify-write of `Config`. Multi-process deployment (which Ryokan doesn't support) would need DB-level locking instead.
- `handlers::auth::LOGIN_FAILURES` — per-IP throttle map. See `src/handlers/auth/CLAUDE.md`.
- `handlers::auth::TRUST_PROXY_HEADERS` / `COOKIE_SECURE` — env snapshots.
- `handlers::system::CLIENT_LOG_HITS` — sliding-window rate limit on the client-side log-ingest endpoint.
- `handlers::library::reconcile::HYDRATED_CUMULATIVE` — first-grab `cumulative_prior_episodes` lazy-hydration dedup. Uses `.unwrap_or_else(|p| p.into_inner())` recovery.
- `handlers::oauth::OAUTH_HTTP_CLIENT`, `services::rss::feed::RSS_HTTP_CLIENT`, `services::jikan::HTTP_CLIENT`, `services::mal::HTTP_CLIENT`, `services::artwork::HTTP_CLIENT`, `services::source_description::HTTP_CLIENT`, `services::nyaa::HTTP_CLIENT`, `services::seadex::HTTP_CLIENT` — per-service pre-configured `LazyLock<reqwest::Client>` instances. Each owns its own UA / timeout / TLS config; sharing one client across services would cross-pollinate timeouts. Standard reqwest hygiene — connection-pool reuse means do NOT build a `Client` per request.

## Database & migrations

- sqlx 0.8 with **no compile-time query checking** (zero `sqlx::query!` invocations); `macros` feature kept for derive macros only. SQLite statically bundled; no system `libsqlite3` needed at runtime.
- Migrations in `models/migrations/`. New columns use `ALTER TABLE ... ADD COLUMN ... .ok()` to ignore-if-present (idempotent, file-free). The exception is **one-shot migrations** that must run exactly once and can't self-guard (data rewrites, seed-table fixups). Those live next to their model (`models/group_source_map.rs` is the current example), write an ID row into `schema_migrations` after running, and skip on subsequent boots via `migration_already_applied(db, id)`. Don't invent a per-migration config flag — that's what `schema_migrations` is for. No separate migration files on disk.

## Routes

Six route groups in `main.rs`, each with a different auth layer — pick the right group when adding a new route:

- **`public_routes`** — unauthenticated; wrapped in `csrf_public` so POSTs still enforce Origin/Referer (`/login`, `/setup`, `/forgot-password`).
- **`protected_routes`** — behind `require_auth` cookie middleware. Default for all UI routes and web-UI-facing API endpoints.
- **`sonarr_routes`** and **`radarr_routes`** — merged *outside* the cookie-auth layer; use `arr_auth::check_api_key` instead. Two separate routers because Seerr only allows two Sonarr + two Radarr indexer slots and both shims must coexist on one host/port.
- **`webhook_routes`** — outside cookie-auth; each receiver carries its own API-key middleware.
- **`calendar_routes`** — outside cookie-auth; carries `require_calendar_scope` (scoped-API-key middleware from #114) so iCal subscribers (Apple Calendar / Google Calendar / Thunderbird, which can't carry cookies) can reach `/api/calendar.ics`. Same scoped-key shape as the future `search`-scoped surfaces planned on top of #114.

There is no `/healthz`. **The Docker healthcheck deliberately probes `/login`** (200 with no auth, no config dependency, no side effects) rather than the auth-gated `/api/health`.

## Docker

`docker-entrypoint.sh` runs as root, ensures a `ryokan` user/group matching `PUID`/`PGID`, chowns `/data`, then execs under `gosu`. Two non-obvious deliberate choices (don't "fix" either of these):

- **User-mounted `/downloads` and `/media/*` paths are intentionally NOT chowned.** Chowning a 10TB media library would stall startup for hours and could clobber ownership the rest of an *arr stack relies on. The user picks a `PUID`/`PGID` that already owns those paths on the host (linuxserver.io convention).
- **`static/` is copied into BOTH stages of the Dockerfile.** Builder needs it for `include_str!("../../static/default_custom_formats.json")`; runtime needs it for `ServeDir`. Looks like duplication; isn't.

## Tests (one-paragraph summary; deep dive in `tests/CLAUDE.md`)

Most tests live inline as `#[cfg(test)] mod tests`; integration tests in `tests/` are binary crates that import `ryokan` as a library and require `--features test-support` (each `[[test]]` target declares this so plain `cargo test` silently skips them). Topic-split submodule pattern when a test module exceeds ~1500 LoC. Env-gated `live_smoke` tests in download-client impls. Browser e2e via `fantoccini` + WebDriver behind `--features browser-e2e` for HTMX UI assertions.

## CI

Seven workflows in `.github/workflows/`: `rust.yml` (fmt → clippy → build → test), `cargo-audit.yml` (weekly + on Cargo lock changes), `docker.yml` (native amd64/arm64 → GHCR on tags), `claude.yml` (`@claude` mentions), `docs.yml` (Zensical site build → `gh-pages` on push to `main`), `license-notices.yml` (auto-regen `licenses/THIRD_PARTY_LICENSES.html` on dev pushes when `Cargo.lock` / `about.toml` / `about.hbs` change; commits with `[skip ci]` to avoid recursive triggers), `mutants-weekly.yml` (Sunday 03:17 UTC cron — curated cargo-mutants subset against last week's baseline; opens an issue only on caught → missed regressions). Build & Run section above lists the four lint commands to run locally before pushing. Two non-obvious traps:

- **Don't re-enable sqlx default features.** `default-features = false` in `Cargo.toml` is what prunes the phantom `rsa` RUSTSEC-2023-0071 advisory at source. Re-enabling defaults pulls `rsa` back in and turns `cargo-audit.yml` red.
- **Don't re-enable fantoccini default features.** `fantoccini` is pinned to `default-features = false, features = ["rustls-tls"]` so its default `native-tls` stack (`hyper-tls` → `native-tls` → `openssl`) stays out of the lock. Dropping the opt-out pulls `openssl` back in (Dependabot GHSA-phqj-4mhp-q6mq) even though it would only ever build under the `browser-e2e` feature. Same source-prune rationale as the sqlx case above.
- **GHCR image name must be lowercase** (`ghcr.io/johnthreekay/ryokan`). The repo is capitalized "Ryokan" but `IMAGE_NAME` lowercases it — GHCR rejects uppercase image names.
