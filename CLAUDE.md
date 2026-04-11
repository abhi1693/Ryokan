# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Ryokan is a self-hosted anime PVR (personal video recorder) written in Rust. It searches Nyaa for anime torrent releases, scores them by quality, and sends them to qBittorrent for download. It uses AniList as the primary metadata source with MAL (via Jikan) and Kitsu as fallbacks.

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

Requires Rust 1.85+, a C linker, and OpenSSL dev headers for local builds.

## Environment Variables

| Variable | Default | Purpose |
|---|---|---|
| `LISTEN_ADDR` | `0.0.0.0:8978` | Bind address |
| `DATABASE_URL` | `sqlite://data/ryokan.db?mode=rwc` | SQLite connection string |
| `RUST_LOG` | `ryokan=debug,tower_http=debug` (local) / `ryokan=info` (Docker) | Log filter |
| `JIKAN_API_BASE` | `https://api.jikan.moe/v4` | Override for self-hosted Jikan |

## Architecture

### Stack
- **Web framework**: Axum 0.8 with Tokio async runtime
- **Database**: SQLite via sqlx (no compile-time query checking — all queries are runtime strings)
- **Templating**: Askama (Jinja2-like, compiled into the binary). Templates live in `templates/`
- **Styling**: Single `static/css/style.css`, no build step for frontend assets

### Code Layout (`src/`)

Three top-level modules, each in its own directory:

- **`handlers/`** — Axum route handlers (request → response). One file per page/feature area: `library`, `search`, `downloads`, `settings`, `system`, `auth`, `media`, `help`.
- **`services/`** — Business logic and external API clients. Key services:
  - `anilist`, `jikan`, `kitsu` — metadata providers (AniList is primary, others are fallbacks)
  - `nyaa` — Nyaa torrent search via HTML scraping
  - `scoring` — release quality scoring (seeders, group preference, resolution, quality tier)
  - `quality` — quality tier enum (`QualityTier`: Web480 through Remux1080, 9 tiers)
  - `rss` — RSS auto-sync for new episodes
  - `qbit`, `jellyfin` — download client and media server integrations
  - `post_processing` — moves/renames completed downloads into the media library
  - `artwork` — artwork caching with content-addressed blob storage
- **`models/`** — Database access layer and schema. `models/mod.rs` contains the `migrate()` function with all `CREATE TABLE` and `ALTER TABLE` statements (no migration files — migrations are idempotent SQL in code).

### Key Patterns

- **AppState**: Shared via Axum's `State` extractor. Contains the `SqlitePool` and optional `Arc<RwLock<>>` clients for qBittorrent and Jellyfin (initialized from saved config at startup).
- **Auth**: Cookie-based sessions. `require_auth` middleware on protected routes redirects to `/login`. First-run setup at `/setup` creates the admin account.
- **Background tasks**: Spawned as `tokio::spawn` loops in `main.rs` — RSS sync, metadata refresh (12h), log cleanup (1h), post-processing (1m). Each reports status to the `scheduled_task_runs` table.
- **Metadata fallback chain**: AniList → Jikan (MAL) → Kitsu. Fallbacks activate on AniList 403s or when force flags are set in config.
- **Database migrations**: All in `models/mod.rs::migrate()`. New columns use `ALTER TABLE ... ADD COLUMN` with `.ok()` to silently ignore if already present. No separate migration files.

### CI

GitHub Actions (`.github/workflows/rust.yml`): runs `cargo build` and `cargo test` on pushes/PRs to `main`.
