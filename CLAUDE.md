# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Ryokan is a self-hosted anime PVR (personal video recorder) written in Rust. It searches Nyaa for anime torrent releases, scores them by quality, and sends them to qBittorrent for download. It uses AniList as the primary metadata source with MAL (via Jikan) and Kitsu as fallbacks.

Release scoring combines Sonarr-style **Custom Formats** (TRaSH-Guides-compatible), a multi-layer **source classification pipeline**, and optional **SeaDex** (releases.moe) authoritative picks. Ryokan also exposes a Sonarr/Radarr-compatible API shim (**anibridge**) so Seerr and similar tools can request anime through it.

## Build & Run Commands

```bash
cargo build              # build (debug)
cargo build --release    # build (release)
cargo run                # run locally (creates data/ryokan.db, listens on 0.0.0.0:8978)
cargo test               # run all tests
cargo test <test_name>   # run a single test
cargo clippy             # lint
docker compose up -d --build  # build and run in Docker
```

Requires Rust 1.88+ (enforced via `package.rust-version` in Cargo.toml), a C linker, and OpenSSL dev headers for local builds.

## Environment Variables

| Variable | Default | Purpose |
|---|---|---|
| `LISTEN_ADDR` | `0.0.0.0:8978` | Bind address |
| `DATABASE_URL` | `sqlite://data/ryokan.db?mode=rwc` | SQLite connection string |
| `RUST_LOG` | `ryokan=debug,tower_http=debug` (local) / `ryokan=info` (Docker) | Log filter |
| `JIKAN_API_BASE` | `https://api.jikan.moe/v4` | Override for self-hosted Jikan |
| `RYOKAN_MEDIA_CACHE_DIR` | `data/cache/artwork` | Artwork blob cache root (content-addressed) |

## Architecture

### Stack
- **Web framework**: Axum 0.8 with Tokio async runtime
- **Database**: SQLite via sqlx (no compile-time query checking — all queries are runtime strings)
- **Templating**: Askama (Jinja2-like, compiled into the binary). Templates live in `templates/`
- **Frontend**: Single `static/css/style.css` plus inline vanilla JS in the templates. No framework, no bundler, no build step.
- **API docs**: `utoipa` generates an OpenAPI schema; Swagger UI is mounted at `/api-docs` (JSON at `/api-docs/openapi.json`)

### Code Layout (`src/`)

Three top-level modules, each in its own directory:

- **`handlers/`** — Axum route handlers (request → response). One file per page/feature area: `library`, `search`, `downloads`, `settings`, `system`, `auth`, `media`, `help`, plus `sonarr_compat` and `radarr_compat` (the anibridge Sonarr/Radarr API shims used by Seerr).
- **`services/`** — Business logic and external API clients. Key services:
  - `anilist`, `jikan`, `kitsu` — metadata providers (AniList is primary, others are fallbacks)
  - `metadata_sync` — periodic refresh of cached metadata across the fallback chain
  - `nyaa` — Nyaa torrent search via HTML scraping
  - `seadex` — releases.moe PocketBase client for authoritative community release picks (keyed by AniList ID)
  - `scoring` — release quality scoring (seeders, group preference, resolution, source, custom format score)
  - `custom_formats` — Sonarr-style Custom Formats engine: parses TRaSH-Guides-shaped JSON and compiles regex specs into a `CompiledCfCache` that lives on `AppState` and is rebuilt on CF edits
  - `source` + `source_filename`, `source_description`, `source_temporal`, `source_groups`, `source_dir`, `source_ffprobe` — the multi-layer classification pipeline. `source.rs` orchestrates; each `source_*.rs` is one signal layer (filename tokens, Nyaa description body, release-date heuristics, release-group map, directory path, ffprobe inspection). Together they produce `(Source, Resolution, is_remux)` used by scoring and upgrade decisions.
  - `quality` — shared helpers (`preferred_group_bonus`, `FinishedSeriesMode`, Nyaa category/probe helpers)
  - `upgrade` — upgrade-decision logic for the background upgrade_search sweep
  - `auto_search` — end-to-end auto-grab pipeline (query → score → grab), powers both manual buttons and RSS/background searches
  - `rss` — RSS auto-sync for new episodes
  - `anibridge` — TMDB↔AniList mapping + request translation backing the Sonarr/Radarr shim handlers
  - `qbit`, `jellyfin` — download client and media server integrations
  - `post_processing` — moves/renames completed downloads into the media library
  - `nfo` — Jellyfin-compatible NFO generation
  - `artwork`, `media` — artwork caching with content-addressed blob storage; media-file probing helpers
- **`models/`** — Database access layer and schema. `models/mod.rs` contains the `migrate()` function with all `CREATE TABLE` and `ALTER TABLE` statements (no migration files — migrations are idempotent SQL in code). Notable tables beyond the obvious `series`/`user`/`session`/`config`: `grabbed_torrents` (grab history + blocklist), `episode_tags` (per-episode state), `custom_formats`, `group_source_map`, `media_probe_cache`, `nyaa_description_cache`, `metadata_cache`, `artwork_cache`, `local_metadata`, `monitoring`, `scheduled_task_runs`.

### Key Patterns

- **AppState**: Shared via Axum's `State` extractor. Contains the `SqlitePool`, optional `Arc<RwLock<>>` clients for qBittorrent and Jellyfin (initialized from saved config at startup), and the `CompiledCfCache` — a swap-on-write `Arc<RwLock<Arc<Vec<_>>>>` of compiled Custom Formats. Handlers clone the inner `Arc` out under the read lock so the scoring hot path runs lock-free.
- **Auth**: Cookie-based sessions for the web UI. `require_auth` middleware on protected routes redirects to `/login`; first-run setup at `/setup` creates the admin account. A `csrf_public` middleware layers over the unauthenticated `/login` and `/setup` POSTs to enforce a same-origin check. **Timing-equalized login**: `models::user::authenticate` bcrypt-verifies against a warmed dummy hash (`DUMMY_BCRYPT_HASH`) on the missing-user path so failed logins take the same ~50ms as real ones. `main()` forces the `LazyLock` to initialize via `warm_timing_equalizer` at startup, otherwise the very first probe would be a one-shot timing oracle for username enumeration.
- **Sonarr/Radarr shim auth**: The `sonarr_compat` and `radarr_compat` routers are merged into the app *outside* the cookie-auth layer and use their own `require_api_key` middleware (query-string `?apikey=` against `config.sonarr_api_key` / `config.radarr_api_key`). Sonarr routes live at `/api/v3/...`; Radarr routes are deliberately mounted under a `/radarr/` prefix (`/radarr/api/v3/...`) because Seerr only allows two Sonarr slots and two Radarr slots, so both shims must coexist on one host/port and are disambiguated by Seerr's "URL Base" field.
- **Background tasks**: Each runs as a `tokio::spawn` loop in `main.rs` wrapped in `supervise()`, which catches panics/join errors, logs them, and respawns after 5s so a stray `.unwrap()` can't silently kill a task for the rest of the process lifetime. Each task writes status to the `scheduled_task_runs` table. Current tasks and intervals:

  | Task | Interval |
  |---|---|
  | `rss_sync` | 60s |
  | `post_processing` | 60s |
  | `cleanup` (log rotation, stale rows) | 1h |
  | `library_classify` | 6h |
  | `metadata_refresh` | 12h |
  | `upgrade_search` | 24h |
  | `anibridge_refresh` (TMDB↔AniList map rebuild) | 24h |

- **Metadata fallback chain**: AniList → Jikan (MAL) → Kitsu. Fallbacks activate on AniList 403s or when force flags are set in config.
- **Database migrations**: All in `models/mod.rs::migrate()`. New columns use `ALTER TABLE ... ADD COLUMN` with `.ok()` to silently ignore if already present. No separate migration files.
- **TRaSH Guides fixtures**: `fixtures/trash-guides-anime/` contains 28 vendored TRaSH-Guides anime CF JSON files. They are bundled into the binary via `include_str!` — both as the CF defaults shipped to users (wired through `static/default_custom_formats.json` and the "Install defaults"/"Reset defaults" endpoints in `handlers/settings.rs`) and as the realistic input corpus for the custom-format parser tests.
- **Ryokan-only Custom Format spec**: The `SpecKind::SeaDexBest` variant in `services/custom_formats.rs` is a non-Sonarr extension that matches a release against SeaDex's curated picks at scoring time. It is accepted under both `Ryokan.SeaDexBestSpecification` and the shorter `SeaDexBestSpecification` implementation name, and is emitted in the long form on export. Presence of a Custom Format using this spec **suppresses the separate `seadex_enabled` config toggle** so the CF and the toggle don't double-count — if you're editing either side, preserve that one-or-the-other invariant.
- **Classifier confidence loop**: `episode_quality_tags` carries `classification_confidence`, `needs_review`, and `manual_override` columns alongside the `(source, resolution, is_remux)` verdict. When the source pipeline's layers don't agree strongly enough, the row is written with `needs_review = 1` and shows up on `/library/review`. Setting a manual override via `/api/library/manual-override` writes `manual_override = 1` and clears `needs_review` — and the upgrade sweep skips rows with `manual_override = 1` so a pinned classification is never silently re-graded by a later reclassification pass.
- **Release-group identity map**: `group_source_map` is seeded at startup from a built-in table; seeded rows are re-upserted on every boot but user-added or user-edited rows are preserved. The Settings → Release Groups tab also surfaces a "Suggested Mappings" panel derived from repeat manual overrides (`N` episodes from the same group pinned to the same source), which the user can promote into the identity map. When editing seeding logic, do not overwrite user edits, and when touching the suggestion query remember it reads from `episode_quality_tags` rows with `manual_override = 1`.
- **CF `ReleaseGroupSpecification` is title-only, not source-inferring**: The bundled `S-Tier BD groups` CF in `static/default_custom_formats.json` lists ~19 groups regex-matched against the scraped `[Group]` prefix. Several of those groups (MTBB, smol, Vodes, Okay-Subs, Arid, LYS1TH3A, sam, MiniMTBB, MegaMTBB) are intentionally **absent** from `SEED_DEFAULTS` in `models/group_source_map.rs` because TRaSH lists them in both BD and WEB tiers. That's not a contradiction: the CF applies a score bonus to the group identity, while `group_source_map` applies a BD-vs-WEB prior to source classification. An S-Tier CF match does **not** imply the release is BluRay — the source classification still comes from filename/ffprobe/temporal/dir layers. Keep these two lists editable independently and do not "sync" them.
- **Tests**: Every test lives inline as `#[cfg(test)] mod tests` in the file it covers. There is no top-level `tests/` directory; integration-style tests are built on in-memory SQLite pools inside those inline modules.

### CI

GitHub Actions in `.github/workflows/`:
- `rust.yml` — `cargo build` and `cargo test` on pushes/PRs to `main`
- `docker.yml` — multi-arch Docker image build and publish
- `claude.yml` — Claude Code workflow integration
