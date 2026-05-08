# Welcome

**Ryokan** is a self-hosted **anime PVR** — a personal recorder for anime releases. You add the shows you want to your library, Ryokan watches for new episodes, picks the best release of each one, and sends it to your download client. Once the file lands, it's renamed and dropped into your media library so Jellyfin (or whatever you use) sees it as a normal episode.

If you've used Sonarr for TV, the shape is the same. Ryokan is just the anime-tuned version of that idea — release-group reputation, batch packs, fansub conventions, [SeaDex](https://releases.moe) authoritative picks, AniList as the metadata source.

!!! info "Project status"
    Ryokan is a one-person project; expect rough edges. v1.X handles anime only; manga and light novels are on the roadmap for v2.X.

## What you can do with it

- **Pick a show on AniList or MAL → it gets added to your Ryokan library.** The optional watch-list sync runs every 30 minutes and pulls new entries automatically.
- **Search a network of sources at once.** Built-in [Nyaa](https://nyaa.si) search, plus any torznab or newznab indexer you have set up through Prowlarr or Jackett, plus direct RSS feeds, plus push notifications from autobrr.
- **Score every release and pick the best one.** Combine a quality profile, [TRaSH-Guides](https://trash-guides.info)-compatible Custom Formats, and SeaDex picks to choose what gets grabbed.
- **Send grabs to whichever download client you use.** qBittorrent, Deluge, Transmission, rTorrent, or SABnzbd. Multiple clients work at once.
- **Land files in your library automatically.** Hardlink (default — keeps the torrent seeding), copy, or move.
- **Plug into Seerr.** Ryokan exposes a Sonarr/Radarr-shaped API so Seerr can request anime through it just like it asks Sonarr for TV.

## System requirements

Ryokan runs as a single Docker container. Modest:

- **CPU**: anything from the last decade. ARM64 (Raspberry Pi, Apple Silicon under Docker Desktop) and x86_64 both supported.
- **RAM**: ~150 MB at idle. Spikes briefly during library scans.
- **Storage**: the binary itself is small (~50 MB). The SQLite database, artwork cache, and OAuth key live under `/data` and stay under 100 MB for typical libraries.
- **Network**: outbound HTTPS to AniList, Nyaa, your indexers, and your download client. No inbound port-forwarding needed unless you're exposing the web UI to the internet.

## Get started

!!! tip "Most users should start here"
    The **[Stack builder](stack-builder.md)** generates a complete `docker-compose.yml` for Ryokan **plus** your download client, Jellyfin, Seerr, and a reverse proxy if you want one. Click through a checkbox form, copy the result, run `docker compose up -d`. Paths line up so post-processing works without fiddling, and the matching Ryokan settings are printed alongside so you know what to paste in once Ryokan is up.

If you'd rather build the stack yourself, work through these in order:

1. **[Quick start](quick-start.md)** — get Ryokan running and grabbing in about 10 minutes. Hands-held end to end.
2. **[Docker installation](install.md)** — the Docker-only details. Skip if the Stack builder generated your compose for you.
3. **[Configuration](configuration.md)** — the tabs in Settings, what each does, what to leave alone.
4. **[Download clients](download-clients.md)** — per-client setup notes (qBit, Deluge, Transmission, rTorrent, SABnzbd).
5. **[External accounts](external-accounts.md)** — link AniList or MAL so your watch list pulls into Ryokan automatically.

## When something's wrong

- **[Troubleshooting](troubleshooting.md)** — concrete diagnostic steps for the most common "why didn't this work" cases.
- **[FAQ](faq.md)** — how Ryokan compares to Sonarr, multi-user, manga support, API access, backup.

## Demo

There isn't a hosted demo (Ryokan is an admin-tool that talks to your private library and download clients — nothing to safely show off without signing visitors into a real account). The fastest way to see the UI is to run the Docker container locally; it works offline against the bundled defaults so you can poke around before configuring anything real.
