# Ryokan

A self-hosted anime PVR written in Rust. Searches Nyaa for releases, scores them by quality, and sends them to qBittorrent from a single web UI.

I built this because Sonarr doesn't always work well for anime. The RSS sync for currently airing shows works just fine, but downloading season batches of shows that've finished airing almost always hangs the interactive search. Sonarr searches Nyaa using `SXEXX`-style episode identifiers, which don't match how most anime torrents are named.

This is a work in progress. Some features are incomplete or rough around the edges, and it's not quite a full Sonarr replacement just yet.
## Screenshots

<img width="1920" height="1080" alt="image" src="https://github.com/user-attachments/assets/ab8d0588-a896-477e-b264-79d2a44fc118" />
<img width="1920" height="1080" alt="image" src="https://github.com/user-attachments/assets/f78ae78f-08ff-49c6-801c-f92d3eb1f07d" />
<img width="1920" height="1080" alt="image" src="https://github.com/user-attachments/assets/0a54684f-8bb9-4083-a7d7-8760e35fcfef" />

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
Or, for non-clean installs:
```bash
docker compose up -d --build
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

## Seerr integration

Ryokan exposes Sonarr and Radarr v3 API compatibility layers so [Seerr](https://github.com/seerr-team/seerr) can request anime series and movies through it.

### Setup

1. In Ryokan, go to **Settings -> Connections** and enable the Sonarr API and/or Radarr API. Generate an API key for each.

2. In Seerr, add Ryokan as a **Sonarr** server (for anime series):
   - **Hostname/IP**: your Ryokan host (e.g. `192.168.67.41`)
   - **Port**: `8978`
   - **URL Base**: leave empty
   - **API Key**: the Sonarr API key from Ryokan's settings

3. In Seerr, add Ryokan as a **Radarr** server (for anime movies):
   - **Hostname/IP**: same as above
   - **Port**: `8978`
   - **URL Base**: `/radarr`
   - **API Key**: the Radarr API key from Ryokan's settings (this is a separate key)

The URL Base distinction is important: Sonarr routes live at `/api/v3/`, while Radarr routes live at `/radarr/api/v3/`. Using the wrong base will cause connection tests to fail.

### Limitations

- Seerr allows a maximum of two Sonarr servers and two Radarr servers (one non-4K, one 4K each). Adding Ryokan uses one slot for each.
- Ryokan treats each AniList entry as a single season. Multi-season TMDB shows that map to multiple AniList entries will each appear as a separate series in Ryokan.
- Some anime may not have TMDB/TVDB-to-AniList mappings in the [anibridge](https://github.com/anibridge/anibridge-mappings) dataset. Ryokan falls back to AniList title search in those cases, which usually works for single-season shows but may pick the wrong entry on series with multiple seasons.

## Self-hosting Jikan

The public Jikan API is rate-limited to roughly 3 requests per second. If you're adding a lot of series at once or want faster metadata loading, you can run a local instance:

```bash
docker run -p 6769:8080 jikanme/jikan-rest:latest
```

Then set `JIKAN_API_BASE=http://localhost:6769/v4`.
