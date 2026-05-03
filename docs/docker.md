# Docker

The published image is `ghcr.io/johnthreekay/ryokan:latest`. Architectures: `linux/amd64` and `linux/arm64`. The repo's `docker-compose.yml` is the canoncal reference. You can start there and adjust to taste.

## Volume layout

```yaml
volumes:
  - ryokan-data:/data
  # Optional, uncomment for post-processing:
  # - /srv/downloads:/downloads
  # - /srv/media/anime:/media/anime
```

**`/data` (required)** holds the SQLite DB, the artwork blob cache, the encryption key, the anibridge mappings cache, and any sentinel files. Loss of `/data` means losing your library state, queued grabs, scoring history, OAuth tokens, etc. The named-volume default (`ryokan-data`) keeps it inside Docker; bind-mount to a host path if you want the DB visible from the host filesystem.

**`/downloads`** and **`/media/...`** are the post-processing source and destination. They're optional in the sense that Ryokan boots without them, but post-processing requires both to be visible inside the container at the same paths you configure in Settings → Download Clients (per-client download path) and Settings → General (media root).

!!! warning "User-mounted paths are *not* chowned"
    The entrypoint chowns `/data` to the runtime UID/GID, but **deliberately not** `/downloads` or `/media/...`. Chowning a 10TB media library would stall startup for hours and could clobber ownership the rest of your *arr stack relies on. Set `PUID` / `PGID` to match the user that already owns those host paths. That's the linuxserver.io convention and what other *arr containers expect.

## PUID / PGID

```yaml
environment:
  - PUID=1000
  - PGID=1000
```

The entrypoint creates a `ryokan` user inside the container with the supplied UID/GID, chowns `/data`, then drops privileges via `gosu` before exec'ing the binary. Files Ryokan writes to mounted paths land with the supplied ownership.

To find the right values, run `id -u` and `id -g` on the host as the user that owns your media library. If you're running everything else as a single non-root user (typical homelab), `1000:1000` is the default and usually right.

## Environment variables

The full list lives in the repo's root `CLAUDE.md`; what follows is the user-facing subset. Most users only need `PUID` / `PGID` / `RUST_LOG`.

| Variable | Default | Purpose |
|---|---|---|
| `LISTEN_ADDR` | `0.0.0.0:8978` | TCP bind. Change the port if 8978 conflicts. |
| `PUID` / `PGID` | `1000` / `1000` | Runtime UID/GID. Match your host's media-owning user. |
| `RUST_LOG` | `ryokan=info` (image) | Log filter. `ryokan=debug` for verbose output while debugging. |
| `RYOKAN_TRUSTED_PROXY` | unset → off | Trust `X-Forwarded-For` / `X-Real-IP` for client IP. **Off by default**; flip on only behind a reverse proxy that overwrites these headers on ingress. Otherwise an attacker can spoof a fresh IP per attempt and bypass the per-IP login throttle. |
| `RYOKAN_COOKIE_SECURE` | unset → off | Append `Secure` to the session cookie. Off by default so HTTP localhost works; flip on for HTTPS. |
| `RYOKAN_RESET_AUTH` | unset | Set to `1` *and* create a `data/.reset-auth` sentinel file to wipe `users` + `sessions` on next boot. Both are required so a stuck-on env var can't silently wipe auth on every boot. |
| `RYOKAN_DB_LOG_LEVEL` | `info` | Write-side floor for the DB-backed `logs` table (separate from `RUST_LOG`). `trace` / `debug` / `info` / `warn` / `error`. |
| `RYOKAN_ENCRYPTION_KEY` | unset → file fallback | Base64-encoded 32-byte AEAD key for `services::crypto`. **Loading priority**: env var → key file → auto-generated on first run. **Key rotation isn't supported**; changing it invalidates all encrypted OAuth tokens. |
| `RYOKAN_KEY_FILE_PATH` | `/data/.ryokan-key` (Docker) | Where the auto-generated key lives. Set in the image; don't change unless you know you need to. |
| `RYOKAN_ANIBRIDGE_CACHE_DIR` | `/data/cache/anibridge` (Docker) | Where the TMDB↔AL mappings cache lives. Without this set, the cache fails to persist and every restart re-downloads ~9MB. |
| `RYOKAN_MEDIA_CACHE_DIR` | `/data/cache/artwork` (Docker) | Artwork blob cache root (content-addressed). |

## Healthcheck

The image ships with:

```dockerfile
HEALTHCHECK --interval=30s --timeout=5s --start-period=30s --retries=3 \
    CMD curl -fsS http://localhost:8978/login || exit 1
```

`start-period=30s` covers cold-boot work: `models::migrate` (idempotent ALTER TABLEs across the whole schema), `bcrypt::warm_timing_equalizer` spawn_blocking, the multi-client cache rebuild, and optional Jellyfin client init. ARM64 first-runs occasionally bumped against the prior 10s budget; 30s matches the CI smoke-test poll window.

## Updating

```sh
docker compose pull
docker compose up -d
```

The volume preserves your data. The image's binary is replaced; migrations run on next boot (idempotent. Applying twice is a no-op.)

!!! danger "Don't `docker compose down -v`"
    The `-v` flag removes named volumes. With the documented setup that means deleting your DB, encryption key, OAuth tokens, and library state. There's no undo. `down` without `-v` is safe.
