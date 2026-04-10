# Ryokan

A self-hosted anime PVR written in Rust. Searches Nyaa for releases, scores them by quality, and sends them to qBittorrent from a single web UI.

I built this because Sonarr doesn't always work well for anime. The RSS sync for currently airing shows works just fine, but downloading season batches of shows that've finished airing almost always hangs the interactive search. Sonarr searches Nyaa using `SXEXX`-style episode identifiers, which don't match how most anime torrents are named.

This is a work in progress. Some features are incomplete or rough around the edges, and it's not quite a full Sonarr replacement just yet.
## Screenshots

<img width="1920" height="1080" alt="Series list" src="https://github.com/user-attachments/assets/0e557ff2-c074-453a-a49b-a5c4f3c8789e" />
<img width="1920" height="1080" alt="Series detail" src="https://github.com/user-attachments/assets/621235cc-ea69-4b23-bac5-1a516e17e8bb" />

## What it does

- Tracks series using AniList as the primary metadata source, with MAL (via Jikan) and Kitsu as fallbacks
- Searches Nyaa and scores releases across nine quality tiers (WEB 480p through BD Remux 1080p)
- Automatically grabs new episodes and quality upgrades via RSS
- Monitors series with Sonarr-style modes: all, future, missing, existing, or none
- Integrates with qBittorrent for downloads and Jellyfin for library refresh
- Caches all metadata locally so pages load instantly after initial setup

## Running with Docker

```bash
docker compose up -d
```

Listens on port `8978`. On first run, go to `http://localhost:8978` to create an admin account.

## Running locally

Requires Rust 1.85+, a C linker, and OpenSSL dev headers.

```bash
cargo run
```

Creates `data/ryokan.db` on first run and listens on `0.0.0.0:8978`.

## Environment variables

| Variable | Default | Description |
|---|---|---|
| `LISTEN_ADDR` | `0.0.0.0:8978` | Bind address and port |
| `DATABASE_URL` | `sqlite://data/ryokan.db?mode=rwc` | SQLite connection string |
| `RUST_LOG` | `ryokan=info` | Log filter (see `tracing-subscriber` docs) |
| `JIKAN_API_BASE` | `https://api.jikan.moe/v4` | Override for a self-hosted Jikan instance |

## Configuration

All runtime settings are managed through the web UI under **Settings**: qBittorrent and Jellyfin connections, quality profiles and cutoffs, preferred/blocked release groups, media root path, and title language preference.

## Self-hosting Jikan

The public Jikan API is rate-limited to roughly 3 requests per second. If you're adding a lot of series at once or want faster metadata loading, you can run a local instance:

```bash
docker run -p 6769:8080 jikanme/jikan-rest:latest
```

Then set `JIKAN_API_BASE=http://localhost:6769/v4`.
