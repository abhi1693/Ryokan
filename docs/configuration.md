# Configuration

This page is the Settings reference. Once Ryokan is up and you've worked through the [quick start](quick-start.md), come here to look up what each control does. The Settings UI lives at `/settings` once you're logged in.

Changes apply on save; no restart needed.

## Connections

Third-party services Ryokan talks to.

- **AniList / MyAnimeList accounts**: OAuth-linked for watch-list sync. When linked, anime you mark "watching" (or "planning", "completed", etc.) on AniList or MAL get auto-added to your Ryokan library on the next sync tick. Setup walkthrough: [External accounts](external-accounts.md).
- **Sync interval (minutes)**: how often the watch-list sync runs. Default 30 minutes; minimum 15, maximum 10080 (7 days). The form won't let you type anything below 15. If a value somehow ends up outside that range, it falls back to 30.
- **Jellyfin**: server URL and API key. Lets Ryokan trigger a Jellyfin library refresh after each import and validate that imported files actually landed on disk. URL is `http://jellyfin:8096` when Ryokan and Jellyfin share a Docker compose; if they're on different hosts or in separate composes, use your host's LAN IP and the host-mapped port.
- **Sonarr / Radarr API shim (anibridge)**: exposes a Sonarr-compatible and Radarr-compatible API so request frontends like Seerr can ask Ryokan for anime the same way they'd ask Sonarr for TV. The Sonarr side lives at `/api/v3/...`, the Radarr side at `/radarr/api/v3/...`. Each has its own API key.
- **autobrr webhook**: accepts inbound webhooks at `/api/webhook/autobrr`. [autobrr](https://autobrr.com) is a separate self-hosted tool that watches IRC announce channels for new releases and pushes matches as HTTP webhooks; this is the receiving side. The webhook has its own API key with a dedicated regenerate button, so an accidental tab POST can't silently rotate or wipe it.

## Download Clients

Pick one or more of qBittorrent, Deluge, Transmission, rTorrent, SABnzbd. Ryokan supports running multiple at once and routes per-grab. Per-client setup notes (URLs, credentials, common gotchas) live on the [Download clients](download-clients.md) page.

The **Default for protocol** toggle is per-protocol, not global. You can have one default torrent client and one default usenet client coexisting; Nyaa and torznab indexer grabs route to the torrent default, and newznab indexer grabs route to the usenet default.

## Indexers

Add torznab indexers (typically fronted by Prowlarr) and newznab indexers (typically Jackett or direct from NZBGeek-style services) here. Direct RSS feeds from sources like SubsPlease also live in this tab.

**Indexer** here means a search source. Ryokan ships with built-in Nyaa search; everything else lands in this tab.

Each indexer row has an optional **download client pin** that overrides the per-protocol default for grabs from that indexer. Useful when you want one private tracker's grabs going to a specific qBit instance with stricter seed rules.

## Preferred Quality & Releases

The scoring inputs that decide which release wins when several match the same episode.

- **Preferred / blocked groups**: whitelists and blacklists of release-group names (e.g. `[VCB-Studio]`). Preferred groups boost scores; blocked groups exclude their releases entirely.
- **Preferred resolution / source**: a coarse first-pass filter. Releases below the resolution floor or wrong source (e.g. WEB-DL when you want BluRay) get filtered out before scoring runs.
- **Cutoff source / resolution**: once an episode has a release at or above the cutoff, the upgrade-search task stops looking for better. Set this to "I'll take 1080p WEB-DL but stop churning through grabs once I've got that."
- **Finished-series quality**: a separate cutoff that applies once AniList marks the series as `FINISHED`. Pattern: WEB while a series is airing, BluRay once the season's done.
- **Audio preference**: subtitled, dubbed, or no preference. Affects scoring, not filtering.
- **SeaDex enabled**: when on, Ryokan consults [SeaDex](https://releases.moe) (a community-curated list of "best release" picks per AniList ID) and gives matching releases a large score bonus. Adding a Custom Format that uses the `SeaDexBest` spec automatically suppresses this toggle so you don't double-count.
- **Interactive file picker mode**: controls whether the grab-picker modal opens for batch releases. `batches_only` (default) opens it for batches and one-clicks single episodes; `never` is one-click everywhere.
- **Default custom query tokens / restrict-to-uploader**: defaults pre-filled into the manual search modal so common filters don't have to be retyped.

## Custom Formats

[Sonarr-style](https://wiki.servarr.com/sonarr/settings#custom-formats) scoring rules. Each release gets scored against every Custom Format; the cumulative score is a tiebreaker on top of the resolution/source profile.

- **Custom Formats list**: add, edit, delete CFs. Includes a release-title test box (paste a release title; see which CFs match and what the cumulative score is) and an Install Defaults button that loads a bundled anime-tuned set.
- **Minimum Score**: a floor for auto-search candidates. Releases scoring below this are silently dropped from auto-search but still show up in interactive search where you can override.
- **Import / Export**: Ryokan round-trips Sonarr v4 CF JSON, so you can paste [TRaSH-Guides](https://trash-guides.info) JSON (a community-maintained set of CF presets) or copy an existing Sonarr setup. The Ryokan-native export keeps Ryokan-only specs (`Ryokan.SeaDexBestSpecification`) verbatim.

## Release Groups

A per-group reputation map. Tells the classifier things like "VCB-Studio always means BluRay encode, regardless of what the filename claims." Used as one of the layers the source classifier consults when the filename alone is ambiguous about BD vs. WEB.

The mapping auto-populates as Ryokan observes grabs (it learns from the filenames a group tends to use); manual overrides take precedence.

## API Keys

Issue API keys for outside tools that need to talk to Ryokan. Each key gets a name, a list of permissions ("scopes") that decide what the key can do, and shows you the key text once when you create it. Save it then; if you lose it, regenerate.

- **calendar**: lets the key read the iCal subscription feed. Calendar apps (Apple Calendar, Google Calendar, Thunderbird) can't log in like a browser, so the subscription URL carries the key in the URL itself. The [Calendar](calendar.md) page has a button that builds the full URL for you.
- **admin**: covers everything. Use it sparingly; prefer narrower scopes when one fits.

## General

Day-to-day knobs.

- **Media Root Path**: where Ryokan imports completed downloads. The value is the path *inside* Ryokan's container. With the default compose, `/media/anime` maps to `/srv/media/anime` on the host.
- **RSS Sync Interval (minutes)**: how often the background RSS poller runs. Default 15 minutes; minimum 1, maximum 60.
- **File operation mode**: `hardlink` (default; keeps the torrent seeding by sharing the same inode between the download folder and the library), `copy`, or `move`. Hardlink automatically falls back to copy when the source and destination are on different filesystems (where hardlinks aren't possible).
- **Preferred Title Language**: `romaji` / `english` / `native`. This is display-only. Scoring and search match across all three regardless of which one's preferred.

## On the System page (not Settings)

A few runtime toggles live on the **System** page rather than under Settings, including **Allow non-English releases** and the **Force MAL / Kitsu fallback** switches. See [System → Debug](system.md#debug) for the full list and what each does.

## Reset / wipe state

- **Wipe library + grab history**: there's no global UI button. The closest is per-series "Remove from library", which cleans up that series' rows and optionally deletes the on-disk files.
- **Wipe everything**: stop Ryokan, delete its data folder (`/srv/docker/ryokan` if you followed the [quick start](quick-start.md), or the named Docker volume otherwise), restart. First-run setup runs again.
- **Reset auth only**: when you've forgotten your admin password but want to keep your library and OAuth tokens intact. See [Docker reference → Reset auth](docker.md#reset-auth) for the two-step gate.

---

*Last updated: 2026-05-09.*
