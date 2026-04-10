# Ryokan

A self-hosted anime PVR and media manager written in Rust. Tracks series from AniList, searches Nyaa for releases, scores them by quality, and sends them to qBittorrent — all from a single web UI with no external dependencies at runtime.

Built as a practical replacement for Sonarr + Prowlarr for anime. Sonarr searches Nyaa using `SXEXX`-style episode identifiers, which don't match how most anime torrents are named — leading to missed releases or suboptimal grabs. Batch/season pack searches are worse: Sonarr has no real concept of them for anime and largely fails to find or handle them correctly. Ryokan is built around how Nyaa actually works.

## Screenshots
<img width="1920" height="1080" alt="2026-04-02_17-44-59" src="https://github.com/user-attachments/assets/0e557ff2-c074-453a-a49b-a5c4f3c8789e" />
<img width="1920" height="1080" alt="2026-04-02_17-45-39" src="https://github.com/user-attachments/assets/621235cc-ea69-4b23-bac5-1a516e17e8bb" />



## Features

- **AniList-native metadata** — search and track series using AniList IDs; titles, covers, episode counts, relations, and scores are cached locally at add-time
- **Nyaa torrent search** — search by series title with quality scoring, group filtering, and one-click grab
- **Quality tier system** — nine tiers from WEB 480p to BD Remux 1080p; configurable quality profile, cutoff, and finished-series preference
- **RSS auto-grab** — polls the Nyaa RSS feed, matches releases to tracked series, and grabs the best candidate automatically
- **Quality upgrades** — RSS pipeline detects below-cutoff on-disk episodes and grabs upgrades when they appear
- **Series monitoring** — Sonarr-style per-series monitoring modes (`all`, `future`, `missing`, `existing`, `none`)
- **Metadata fallback chain** — AniList → Jikan/MAL → Kitsu; episode titles and air dates cached locally with 7-day TTL per source
- **Local metadata cache** — all metadata, episode data, relation cards, and artwork stored in SQLite; tracked series pages served from local DB with zero network round-trips
- **Jellyfin integration** — trigger library refresh after a grab
- **Structured logging** — SQLite-backed log viewer in the UI with level/category filtering and live poll

## Running with Docker

```bash
docker compose up
```

The app listens on port `8978` and stores all data in a named Docker volume. On first run, navigate to `http://localhost:8978` to complete setup (create an admin account).

To use a pre-built image or pin a version, edit `docker-compose.yml`.

## Running locally

**Prerequisites:** Rust toolchain (1.85+), a C linker, OpenSSL dev headers.

```bash
cargo run
```

The app will create `data/ryokan.db` on first run and listen on `0.0.0.0:8978`.

## Environment variables

| Variable | Default | Description |
|---|---|---|
| `LISTEN_ADDR` | `0.0.0.0:8978` | Bind address and port |
| `DATABASE_URL` | `sqlite://data/ryokan.db?mode=rwc` | SQLite connection string |
| `RUST_LOG` | `ryokan=info` | Log filter (see `tracing-subscriber` docs) |
| `JIKAN_API_BASE` | `https://api.jikan.moe/v4` | Override for a self-hosted Jikan instance |

## Configuration

All runtime settings are managed through the web UI under **Settings**:

- **Connections** — qBittorrent URL/credentials, Jellyfin URL/API key
- **Quality & Scoring** — quality profile, quality cutoff, finished-series quality, preferred/blocked release groups
- **General** — media root path, title language preference, metadata source toggles

## Self-hosting Jikan

The public Jikan API has rate limits (~3 req/s). For heavy use or initial library hydration, run a local instance:

```bash
docker run -p 8080:8080 jikanme/jikan-rest:latest
```

Then set `JIKAN_API_BASE=http://localhost:8080/v4`.

## Tech stack

- **Runtime:** Rust, Tokio, Axum
- **Database:** SQLite via sqlx
- **Templating:** Askama (Jinja2-style, compiled at build time)
- **HTTP client:** reqwest
- **Auth:** bcrypt password hashing, cookie-based sessions
