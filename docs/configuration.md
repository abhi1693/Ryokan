# Configuration

Settings live in the SQLite DB, edited through the web UI under `/settings`. The settings page is split into tabs by concern — General, Connections, Quality, Custom Formats, Media, Integrations.

## General

- **RSS sync interval** — how often the background poller pulls from configured RSS sources. Default 15 minutes; minimum 1, maximum 60. Direct-RSS publishers (SubsPlease and similar) tend to rate-limit at five-minute polling, which is why the default isn't lower.
- **External-account sync interval** — how often AniList / MAL watch-list sync runs. Minimum 15 minutes (clamped — you can't go lower no matter what you put in the field).
- **Title language** — `romaji` / `english` / `native`. Display-only; scoring and search aren't affected.
- **Allow non-English releases** — when off, Nyaa search restricts to category `1_2` (English-translated). When on, Ryokan also accepts `1_0` (Anime All). Music releases always search `1_1` + `2_0` regardless.

## Connections → Downloads

The five supported clients: qBittorrent, Deluge, Transmission, rTorrent, SABnzbd. Multiple clients can be configured at once and Ryokan routes per-grab — see [Download clients](download-clients.md) for the full per-client setup.

The "default" toggle is **per protocol**, not global. You can have one default torrent client and one default usenet client coexisting. Indexers can pin a specific client for routing (Settings → Connections → Indexers).

## Quality

- **Quality profile** — preferred resolution / source combo. Used as a coarse first-pass filter on releases.
- **Cutoff** — once an episode has a release at or above the cutoff source/resolution, upgrade-search stops looking for better.
- **Finished-series quality** — separate cutoff that applies to series with status `FINISHED` on AniList. Useful for "I'll grab WEB for currently-airing but want BluRay once it's done."
- **Custom Formats** — Sonarr-style scoring rules, TRaSH-Guides-compatible. Releases score against every CF; cumulative score becomes a tiebreaker on top of the quality profile.
- **SeaDex enabled** — when on, Ryokan consults [SeaDex](https://releases.moe) for community-curated "best" releases. Presence of a Custom Format using the `SeaDexBest` spec automatically suppresses this toggle (so you don't double-count).

## Media

- **Media root** — where Ryokan imports completed downloads. Visible inside the container at the path you configured the volume mount to.
- **Post-processing mode** — `hardlink` (default, seed-safe), `copy`, or `move`. Hardlink falls back to copy when `fs::hard_link` errors (cross-filesystem common case).

## Integrations

- **AniList / MyAnimeList accounts** — OAuth-linked for watch-list sync. See [External accounts](external-accounts.md).
- **Jellyfin** — server URL + API key. Used for library-side metadata and on-disk validation.
- **Sonarr / Radarr API shim (anibridge)** — exposes a Sonarr/Radarr-compatible API on `/api/v3/...` and `/radarr/api/v3/...`. Lets Seerr / Ombi / similar request anime through Ryokan. Each side has its own API key.
- **autobrr webhook** — accepts IRC-announce push from autobrr at `/api/webhook/autobrr`. Has its own API key for auth.

## Reset / wipe state

- **Wipe library + grab history** — there's no UI for this. The closest is per-series "Remove from library" which cleans up that series' rows and optional disk files.
- **Wipe everything** — stop Ryokan, remove the `/data` volume (`docker volume rm ryokan-data` or delete the bind-mounted dir), restart. First-run setup runs again.
- **Reset auth only** — set `RYOKAN_RESET_AUTH=1` *and* create `data/.reset-auth`. Both are required (sentinel-file gate prevents a stuck env var from wiping auth on every restart).
