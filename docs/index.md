# Ryokan

A self-hosted **anime PVR** written in Rust. Searches Nyaa for releases, scores them by quality, and dispatches grabs to a download client. Sonarr-style features (Custom Formats, source classification, monitored episodes, library management) tuned for anime release conventions: encoder groups, fansub pipelines, batch packs, BD vs WEB-DL distinctions, SeaDex authoritative picks.

!!! info "Project status"
    Ryokan is a one-person project; expect rough edges. v1.0 is anime-only; manga / light novel / web novel support is on the roadmap as separate epics.

## What it does

- **Pulls metadata** from AniList, with MAL (via Jikan) and Kitsu as fallbacks.
- **Searches Nyaa, torznab/newznab indexers, direct RSS feeds, and autobrr webhooks** for releases. All four sources merge in parallel.
- **Scores releases** with Sonarr-style Custom Formats (TRaSH-Guides-compatible), a multi-layer source classification pipeline, optional SeaDex picks, and a quality profile.
- **Dispatches grabs** to one of five download clients: qBittorrent, Deluge, Transmission, rTorrent, or SABnzbd. Multiple clients can be configured at once and routed per-grab.
- **Imports completed downloads** with hardlink, copy, or move modes — preserving seeding while still landing files in your library.
- **Acts as a Sonarr/Radarr API shim** (anibridge) so Seerr, Ombi, and similar request tools can request anime through Ryokan.

## Get started

<div class="grid cards" markdown>

- :material-package-down: **[Install](install.md)** — `cargo run` for local dev, Docker Compose for prod.
- :fontawesome-brands-docker: **[Docker setup](docker.md)** — volumes, PUID/PGID, environment variables.
- :material-cog: **[Configuration](configuration.md)** — quality profiles, Custom Formats, post-processing modes.
- :material-download: **[Download clients](download-clients.md)** — per-client setup notes and quirks.
- :material-account-link: **[External accounts](external-accounts.md)** — link AniList / MAL for watch-list sync.
- :material-help-circle: **[Troubleshooting](troubleshooting.md)** — rate limits, missing categories, the "where did my grab go" cases.

</div>

## Where the docs come from

Engineering reference (architecture, internal conventions) lives in `CLAUDE.md` files inside the repo — those serve as memory for code-aware AI tools and for anyone reading the source. The user-facing docs you're reading now answer the practical questions: how do I install it, how do I configure X, why does Y happen.

If a question keeps recurring in issues or chat, it belongs here. PRs welcome.
