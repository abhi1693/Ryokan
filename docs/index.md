# Ryokan

A self-hosted **anime PVR** written in Rust. Searches indexers for releases, scores them by quality, and dispatches grabs to a download client. Sonarr-style features (Custom Formats, source classification, monitored episodes, library management) tuned for anime release conventions: encoder groups, fansub pipelines, batch packs, BD vs WEB distinctions, SeaDex authoritative picks.

!!! info "Project status"
    Ryokan is a one-person project; expect rough edges. v1.X.X is anime-only; manga / light novel / web novel support is on the roadmap for 2.X.X.

## What it does

- **Pulls metadata** from AniList, with MAL (via Jikan) and Kitsu as fallbacks.
- **Searches Nyaa, torznab/newznab indexers, direct RSS feeds, and autobrr webhooks** for releases. All four sources merge in parallel.
- **Scores releases** with Sonarr-style Custom Formats (TRaSH-Guides-compatible), a multi-layer source classification pipeline, optional SeaDex picks, and a quality profile.
- **Dispatches grabs** to one of five download clients: qBittorrent, Deluge, Transmission, rTorrent, or SABnzbd. Multiple clients can be configured at once and routed per-grab.
- **Imports completed downloads** with hardlink, copy, or move modes, preserving seeding while still landing files in your library.
- **Acts as a Sonarr/Radarr API shim** (anibridge) so Seerr can request anime through Ryokan.

## Get started

<div class="grid cards" markdown>

- **[Install](install.md)**: `cargo run` for local dev, Docker Compose for prod.
- **[Docker setup](docker.md)**: volumes, PUID/PGID, environment variables.
- **[Configuration](configuration.md)**: quality profiles, Custom Formats, post-processing modes.
- **[Download clients](download-clients.md)**: per-client setup notes and quirks.
- **[External accounts](external-accounts.md)**: link AniList/MAL for watch-list sync.
- **[Troubleshooting](troubleshooting.md)**: rate limits, missing categories, the "where did my grab go" cases.

</div>
