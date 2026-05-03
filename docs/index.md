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

!!! tip "Fastest path: the Stack builder"
    The **[Stack builder](stack-builder.md)** generates a runnable `docker-compose.yml` from a checkbox form: pick your download client(s), media server (Jellyfin), request frontend (Seerr), VPN (Gluetun with port-forwarding), reverse proxy (Caddy / Traefik / nginx / Cloudflare Tunnel), and host paths. Paths line up so post-processing hardlinks work without fiddling, the per-protocol default download client is wired automatically, and the matching Ryokan settings are printed alongside so you know what to paste into Settings → Download Clients after first boot. **Most users should start here.**

### If you'd rather roll your own

Work through these in order; later steps assume the earlier ones are done.

1. **[Install](install.md)**: Docker pull (recommended) or build from source. Spin up Ryokan at `http://localhost:8978` and create the admin account.
2. **[Docker setup](docker.md)**: volume mounts, PUID/PGID, the full environment-variable reference. Worth skimming even if the Stack builder generated your compose, since it explains what each `RYOKAN_*` flag does.
3. **[Download clients](download-clients.md)**: point Ryokan at qBittorrent, Deluge, Transmission, rTorrent, or SABnzbd. Per-client wire quirks live here. Multiple clients are supported and route per-grab.
4. **[Configuration](configuration.md)**: quality profiles, Custom Formats, post-processing mode (default hardlink). The TRaSH-Guides anime CFs ship as one-click defaults; reach for custom ones once you have a feel for what gets grabbed.
5. **[External accounts](external-accounts.md)**: link AniList or MAL so your watch list pulls into Ryokan. Optional but the main reason most people self-host this.

### If something's wrong

- **[Troubleshooting](troubleshooting.md)**: rate limits, missing categories, "where did my grab go" cases.
- **[FAQ](faq.md)**: common questions, including the Sonarr-comparison and "does it support X" answers.
