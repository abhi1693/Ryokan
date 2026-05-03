# FAQ

## How is this different from Sonarr?

Sonarr is general-purpose TV; Ryokan is anime-only and tunes its release-classification logic for that. Concretely:

- **Anime release naming** is the default-handled path here, not an edge case. Anitomy parses titles, anitomy-aware Custom Formats matter, AniList is the primary metadata source.
- **SeaDex** is integrated as an authoritative-pick layer; Sonarr has nothing equivalent.
- **Source classification** runs through six layers (filename, description scrape, release group, ffprobe, directory walk, temporal — i.e. "is this airing right now") before deciding BluRay vs WEB-DL. Sonarr's classification is filename-only.
- **Multi-client routing** is per-grab and per-protocol; Sonarr supports this too but Ryokan ships with usenet (SAB) as a first-class peer to the four BT clients.
- **Anibridge shim** lets Seerr / Ombi request anime through Ryokan via Sonarr/Radarr APIs, so you can keep the same request UI and not duplicate library state.

## Is this a fork of Sonarr?

No. Different language (Rust vs C#), different architecture, different release ecosystem. Some conventions are deliberately Sonarr-shaped (Custom Format JSON format, source taxonomy) for compatibility with TRaSH-Guides and similar community resources, but the codebase is independent.

## Can it manage manga / light novels / webtoons?

Anime-only for v1. There's a long-term epic to add manga + LN + WN support (Issue #25), but it's intentionally post-1.0 — the metadata, classification, and naming conventions for those formats are different enough to warrant separate provider chains.

## Why isn't Readarr supported as a provider?

Readarr was archived 2025-06-27. It's no longer maintained.

## Can I run multiple Ryokan instances?

Technically yes; they don't coordinate. Each instance has its own DB, its own grab history, its own AL/MAL link state. Two instances pointed at the same AL account share that account's per-token rate-limit budget — see [Troubleshooting](troubleshooting.md#anilist-per-account-cooldown-stuck-past-60s).

There's no built-in way to share a library between instances. If that matters, run one.

## Multi-user?

Not supported and not on the roadmap. Single-admin only. The reasoning matches Sonarr's PR #7186 rejection (Jan 2025): private-tracker account-sharing semantics get messy fast, and the PVR-shared-with-friends case is well-served by Jellyfin / Plex on top of a single-admin PVR.

## Can I use the API directly?

Yes. Swagger UI at `/api-docs`, OpenAPI JSON at `/api-docs/openapi.json`. Cookie-auth for the web-UI-facing endpoints; the Sonarr/Radarr shim uses API-key auth (`X-Api-Key` header or `?apikey=` query).

## How do I back up?

The whole `/data` volume. That's the SQLite DB, the artwork cache, the encryption key, the OAuth tokens (encrypted), and config sentinels. Standard SQLite backup tools work; or just stop Ryokan and copy the volume.

The encryption key is the load-bearing bit — if you lose it but keep the DB, every encrypted OAuth token in `external_accounts` is unrecoverable and you'll need to re-link those accounts.
