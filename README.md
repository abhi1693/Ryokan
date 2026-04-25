# Ryokan

A self-hosted anime PVR written in Rust. Searches Nyaa for releases, scores them by quality, and sends them to your download client from a single web UI. Supports qBittorrent, Deluge, Transmission, and rTorrent/ruTorrent.

I built this because Sonarr doesn't always work well for anime. The RSS sync for currently airing shows works just fine, but downloading season batches of shows that've finished airing almost always hangs the interactive search. Sonarr searches Nyaa using `SXEXX`-style episode identifiers, which don't match how most anime torrents are named.

This project's being actively developed. Expect some occasional bugs. See [Releases](https://github.com/johnthreekay/Ryokan/releases) for version-to-version changes.

## Screenshots

<img width="1920" height="1080" alt="image" src="https://github.com/user-attachments/assets/ab8d0588-a896-477e-b264-79d2a44fc118" />
<img width="1920" height="1080" alt="image" src="https://github.com/user-attachments/assets/1601ca73-1c47-4831-bd3d-4fcf3d2a6ad1" />
<img width="1920" height="1080" alt="image" src="https://github.com/user-attachments/assets/754565f7-662f-4a85-a1e2-257a251c2361" />
<img width="1920" height="1080" alt="image" src="https://github.com/user-attachments/assets/0a54684f-8bb9-4083-a7d7-8760e35fcfef" />

## What it does

- Tracks series using AniList as the primary metadata source, with MAL (via Jikan) and Kitsu as fallbacks
- **Link your AniList or MyAnimeList account** to auto-import your watch list. Picks up new entries, status changes, and per-series user scores on a configurable cadence (default 30 min). User scores render on every library card in the format your account uses (POINT_100, POINT_10, POINT_5 stars, POINT_3 smileys, or POINT_10_DECIMAL). OAuth tokens are encrypted at rest with an AEAD key (see `RYOKAN_ENCRYPTION_KEY` below)
- Searches Nyaa and scores releases by source, resolution, and release group using a multi-layer classification pipeline (filename, Nyaa description, ffprobe, temporal heuristics, group reputation, directory layout)
- Supports Sonarr-v4-compatible [Custom Formats](https://trash-guides.info/Sonarr/sonarr-collection-of-custom-formats/) for release scoring, including a one-click install of the TRaSH Guides anime defaults. Ships a Ryokan-only `SeaDexBestSpecification` spec that matches [SeaDex](https://releases.moe) best-release curation, plus a Settings toggle to apply the SeaDex boost without writing a Custom Format
- Automatically grabs new episodes via RSS and scans the existing library for quality upgrades on a schedule, with separate **preferred** and **cutoff** source/resolution targets so upgrade churn stops once an episode is "good enough"
- Tunable scoring inputs: preferred & blocked release groups, preferred source/resolution, finished-series quality mode (`Same as airing` / `Prefer BD` / `BD only`), and a prefer-subs vs prefer-dubs audio toggle
- Monitors series with Sonarr-style modes (all / future / missing / existing / none) and also supports per-episode monitoring toggles
- **Interactive search** per episode or for batches, on top of the automatic grab flow, so you can pick a specific release yourself when you want to
- **Manual classification override** and a **Needs Review** queue (`/library/review`) for episodes where the classifier wasn't confident. Pins propagate into a "suggested group→source mapping" panel so repeat overrides can teach the release-group identity map
- **Blocklist** completed/bad releases from the Downloads page or from an episode's grab history, so the upgrade sweep and RSS sync won't re-grab them
- Per-series **Allow Upgrades** toggle to opt specific titles out of the upgrade sweep without disabling it globally
- Integrates with qBittorrent, Deluge, Transmission, and rTorrent for downloads (one active client at a time) and Jellyfin for library refresh; writes Jellyfin-compatible NFO sidecars during post-processing
- Post-processes completed downloads in **hardlink** (default, seed-safe), **copy**, or **move** mode. Hardlink automatically falls back to copy when the download and media root are on different filesystems
- Caches all metadata and artwork locally (content-addressed blob store) so pages load instantly after initial setup
- Cookie-based auth with first-run admin setup, a **System** page for live logs, scheduled-task inspection/force-run, RSS grab history, a scoring inspector, and a debug tab, plus an OpenAPI/Swagger UI at `/api-docs` for everything the web UI calls

## Running with Docker

```bash
docker compose up -d
```

Listens on port `8978`. On first run, go to `http://localhost:8978` to create an admin account. Multi-arch images are published for `linux/amd64` and `linux/arm64`, so the same tag works on x86 servers, Raspberry Pi 4/5, Apple Silicon Macs under Docker Desktop, Intel Macs under Docker Desktop, and Windows under Docker Desktop (WSL2 backend, the default). Docker Desktop's Windows-containers mode is not supported.

### Enabling post-processing

Ryokan's post-processor reads completed torrents from your download client and imports them into your anime library. For that to work, both paths have to be visible inside the container at the same paths you configure in **Settings**:

1. Uncomment the two optional volume lines in `docker-compose.yml`:
   - `/srv/downloads:/downloads` (host path where your download client writes completed torrents)
   - `/srv/media/anime:/media/anime` (host path to your anime library root)
2. In **Settings → Connections**, open the field for your active download client (qBittorrent / Deluge / Transmission / rTorrent) and set *Download Path (as seen by Ryokan)* to the right-hand side (e.g. `/downloads`). Note that some clients write to a subdirectory: the `linuxserver/transmission` image defaults to `/downloads/complete`, and many `rtorrent.rc` setups move finished torrents into a per-label subdirectory like `/downloads/completed/ryokan/`.
3. In **Settings → Media**, set *Media root* to the right-hand side (e.g. `/media/anime`).
4. For Docker: set `PUID` / `PGID` in `docker-compose.yml` to the UID/GID that owns those host directories (run `id -u` / `id -g` on the host to find them). Ryokan drops privileges to that user at startup so imported files end up with the right ownership for Jellyfin and the rest of your *arr stack to read.

## Running locally

Requires Rust 1.95+, a C/C++ toolchain, and `cmake`. The toolchain and `cmake` are needed by two bundled C/C++ deps: `anitomy` (via `cc`) parses release filenames, and `aws-lc-sys` (via `cmake`) backs reqwest's rustls-with-aws-lc TLS stack.

```bash
cargo run
```

Creates `data/ryokan.db` on first run and listens on `0.0.0.0:8978`.

The local build path is primarily tested on Linux (CI gate). macOS should work with `xcode-select --install` + `brew install cmake`, and Windows should work with the MSVC build tools plus `cmake`, but neither is covered by CI. If you hit a build break on a non-Linux host, Docker is the supported fallback.

## Environment variables

| Variable | Default | Description |
|---|---|---|
| `LISTEN_ADDR` | `0.0.0.0:8978` | Bind address and port |
| `DATABASE_URL` | `sqlite://data/ryokan.db?mode=rwc` (local), `sqlite:///data/ryokan.db?mode=rwc` (Docker) | SQLite connection string |
| `RUST_LOG` | `ryokan=info` | Log filter (see [`tracing-subscriber`](https://github.com/tokio-rs/tracing) docs) |
| `RYOKAN_MEDIA_CACHE_DIR` | `data/cache/artwork` (local), `/data/cache/artwork` (Docker) | On-disk directory for the artwork blob cache |
| `JIKAN_API_BASE` | `https://api.jikan.moe/v4` | Override for a self-hosted Jikan instance. If it's HTTPS behind a private CA, install the CA into the system trust store (e.g. `/usr/local/share/ca-certificates/` + `update-ca-certificates` on Debian); reqwest's rustls backend does not read `SSL_CERT_FILE` / `SSL_CERT_DIR`. |
| `RYOKAN_ENCRYPTION_KEY` | *(unset → file fallback)* | Base64-encoded 32-byte key used to encrypt AniList / MAL OAuth tokens stored in the database. If unset, Ryokan loads (or auto-generates on first run) a 32-byte key file at `data/.ryokan-key` (mode 0600 on Unix). The env var is the Docker / Kubernetes path; the file is the bare-metal default. **Changing the key invalidates all linked accounts and requires re-linking** (key rotation isn't supported yet). |
| `RYOKAN_TRUSTED_PROXY` | *(unset → false)* | Set to `1` / `true` only when Ryokan sits behind a reverse proxy that strips and rewrites `X-Forwarded-For` / `X-Real-IP` / `X-Forwarded-Host` on ingress. Off by default because direct exposure on `0.0.0.0:8978` would let any client spoof those headers and bypass the per-IP login throttle. |
| `RYOKAN_COOKIE_SECURE` | *(unset → false)* | Set to `1` / `true` to mark the session cookie `Secure`. Off by default so `cargo run` on HTTP localhost works; flip on for any HTTPS-fronted deployment. |
| `RYOKAN_DB_LOG_LEVEL` | `info` | Floor for the DB-backed `logs` table feeding the System → Logs page (independent of `RUST_LOG`, which is console-only). Accepts `trace` / `debug` / `info` / `warn` / `error`. Raise to prune a noisy table; lower when diagnosing. |
| `RYOKAN_RESET_AUTH` | *(unset)* | Set to `1` alongside a `data/.reset-auth` sentinel file to wipe `users` / `sessions` on next boot. See [Password recovery](#password-recovery) |
| `PUID` | `1000` | *Docker only.* UID Ryokan runs as inside the container. Set to match host file ownership |
| `PGID` | `1000` | *Docker only.* GID Ryokan runs as inside the container |
| `TZ` | `UTC` | *Docker only.* Container timezone, affects log timestamps and scheduled-task anchoring |

## Configuration

All runtime settings are managed through the web UI under **Settings**: download client (qBittorrent, Deluge, Transmission, or rTorrent) and Jellyfin connections, quality profiles and cutoffs, preferred/blocked release groups, media root path, and title language preference.

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

## Password recovery

If you've forgotten the admin password, recover access with either:

**1. Reset on boot.** From the Ryokan install directory:

```bash
touch data/.reset-auth
RYOKAN_RESET_AUTH=1 ./ryokan        # or `cargo run`, or restart the container with the env var set
```

Ryokan wipes the `users` and `sessions` tables on startup, so the browser redirects you to the first-run setup page. Create a new admin account, then remove the sentinel: `rm data/.reset-auth`. The sentinel is required so a stuck-on env var in a compose file can't wipe auth on every boot.

**2. Direct sqlite3.** Shut Ryokan down, then:

```bash
sqlite3 data/ryokan.db "DELETE FROM users; DELETE FROM sessions;"
```

Start Ryokan and create a new admin account. Config (Jellyfin / download-client credentials, media root, CFs) survives either recovery path; only the admin account and active sessions get wiped.

## Self-hosting Jikan

The public Jikan API is rate-limited to roughly 3 requests per second. If you're adding a lot of series at once or want faster metadata loading, you can run a local instance:

```bash
docker run -p 6769:8080 jikanme/jikan-rest:latest
```

Then set `JIKAN_API_BASE=http://localhost:6769/v4`.
