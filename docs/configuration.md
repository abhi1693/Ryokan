# Configuration

Settings live in the SQLite DB, edited through the web UI under `/settings`. The settings page is split into seven tabs: Connections, Download Clients, Indexers, Preferred Quality & Releases, Custom Formats, Release Groups, and General.

A few runtime toggles live on the **System** page rather than Settings (notably `allow_non_english`); those are called out below.

## Connections

Third-party integrations Ryokan talks to.

- **AniList / MyAnimeList accounts**: OAuth-linked for watch-list sync. See [External accounts](external-accounts.md).
- **Sync interval (minutes)**: how often the linked-account watch-list sync runs. Default 30 minutes; minimum 15, maximum 10080 (7 days). The minimum is clamped, so values under 15 in the field are rounded up.
- **Jellyfin**: server URL + API key. Used for library-side metadata and on-disk validation.
- **Sonarr / Radarr API shim (anibridge)**: exposes a Sonarr/Radarr-compatible API on `/api/v3/...` and `/radarr/api/v3/...`. Lets Seerr / Ombi / similar request anime through Ryokan. Each side has its own API key.
- **autobrr webhook**: accepts IRC-announce push from autobrr at `/api/webhook/autobrr`. Has its own API key for auth, regenerable via a separate button so an accidental tab POST can't silently rotate or wipe the key.

## Download Clients

The five supported clients: qBittorrent, Deluge, Transmission, rTorrent, SABnzbd. Multiple clients can be configured at once and Ryokan routes per-grab. See [Download clients](download-clients.md) for the full per-client setup.

The "default" toggle is **per protocol**, not global. You can have one default torrent client and one default usenet client coexisting.

## Indexers

Torznab/newznab indexers (Prowlarr, Jackett) and direct RSS feeds. Each indexer row has an optional download-client pin so grabs from that indexer route to a specific client (overriding the per-protocol default).

## Preferred Quality & Releases

The scoring inputs that decide which release among many candidates wins.

- **Preferred / blocked groups**: whitelists and blacklists of release-group names.
- **Preferred resolution / source**: coarse first-pass filter on releases.
- **Cutoff source / resolution**: once an episode has a release at or above the cutoff, upgrade-search stops looking for better.
- **Finished-series quality**: separate cutoff that applies to series with status `FINISHED` on AniList. Useful for "I'll grab WEB for currently-airing but want BluRay once it's done."
- **Audio preference**: subs vs. dubs.
- **SeaDex enabled**: when on, Ryokan consults [SeaDex](https://releases.moe) for community-curated "best" releases. Presence of a Custom Format using the `SeaDexBest` spec automatically suppresses this toggle so you don't double-count.
- **Interactive file picker mode**: controls whether the grab-picker modal opens for batch releases (`batches_only`, the default) or never (`never`, one-click grabs).
- **Default custom query tokens / restrict-to-uploader**: defaults pre-filled into the manual search modal.

## Custom Formats

Sonarr-style scoring rules, TRaSH-Guides-compatible. Releases score against every CF; cumulative score becomes a tiebreaker on top of the quality profile.

- **Custom Formats list**: add, edit, delete CFs. Includes a release-title test box and an Install Defaults button for the bundled anime-tuned set.
- **Minimum Score (`custom_format_minimum_score`)**: floor for auto-search candidates. Releases scoring below it are silently dropped from auto-search but still show up in interactive search.
- **Import / Export**: round-trips Sonarr v4 CF JSON. The Ryokan-native export keeps Ryokan-only specs (`Ryokan.SeaDexBestSpecification`) verbatim.

## Release Groups

Per-group source-reliability mapping (e.g. "VCB-Studio always means BluRay encode"). Layer 3 of the source classification pipeline reads this. Suggested mappings auto-populate from observed grabs; manual overrides take precedence.

## General

Day-to-day knobs.

- **Media Root Path**: where Ryokan imports completed downloads. Visible inside the container at the path you configured the volume mount to.
- **RSS Sync Interval (minutes)**: how often the background poller pulls from configured RSS sources. Default 15 minutes; minimum 1, maximum 60.
- **File operation mode**: `hardlink` (default, seed-safe), `copy`, or `move`. Hardlink falls back to copy when `fs::hard_link` errors (cross-filesystem common case).
- **Preferred Title Language**: `romaji` / `english` / `native`. Display-only; scoring and search aren't affected.

## On the System page (not Settings)

- **Allow non-English releases**: when off, Nyaa search restricts to category `1_2` (English-translated). When on, Ryokan also accepts `1_0` (Anime All). Music releases always search `1_1` + `2_0` regardless. Lives on the System page because it's a runtime toggle that affects in-flight searches, not a stored config.

## Reset / wipe state

- **Wipe library + grab history**: there's no UI for this. The closest is per-series "Remove from library" which cleans up that series' rows and optional disk files.
- **Wipe everything**: stop Ryokan, remove the `/data` volume (`docker volume rm ryokan-data` or delete the bind-mounted dir), restart. First-run setup runs again.
- **Reset auth only**: set `RYOKAN_RESET_AUTH=1` *and* create `data/.reset-auth`. Both are required (sentinel-file gate prevents a stuck env var from wiping auth on every restart).
