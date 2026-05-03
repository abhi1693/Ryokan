# FAQ

## How is this different from Sonarr?

Sonarr was built for general-purpose TV, so its handling of anime isn't first class. Ryokan is anime-only and tunes its release-classification logic and metadata for that. Concretely:

- **Batch releases are a first-class search target, not a fan-out.** Sonarr's anime mode searches every episode individually, which scales poorly with multiple indexers since searches time out before results aggregate. Ryokan sends one search per batch.
- **Anime release naming is the default-handled path, not an edge case.** Anitomy parses titles, anitomy-aware Custom Formats are first-class, and AniList is the primary metadata source.
- **SeaDex is integrated as an authoritative-pick layer.** Sonarr has nothing equivalent.
- **Source classification runs through six layers.** Filename, Nyaa description scrape, release-group reputation, ffprobe, directory walk, and a temporal heuristic ("is this airing right now") before deciding BD vs WEB. Sonarr's classification is filename-only.

## Is this a fork of Sonarr?

No. Different language (Rust vs C#), different architecture, different release ecosystem. Some conventions are deliberately Sonarr/Radarr-shaped (Custom Format JSON format, source taxonomy) for compatibility with TRaSH-Guides and similar community resources, but the codebase is independent.

## Can it manage manga / light novels / webtoons?

Ryokan only manages anime right now. The metadata, classification, and naming conventions for those formats are different enough to warrant separate provider chains, which will be coming in v2.0.0.

## Can I run multiple Ryokan instances?

Technically yes, but they don't coordinate. Each instance has its own DB, its own grab history, its own AL/MAL link state. Two instances pointed at the same AL account share that account's per-token rate-limit budget. See [Troubleshooting](troubleshooting.md#anilist-per-account-cooldown-stuck-past-60s).

There's no built-in way to share a library between instances. If you need shared library state, stick to a single instance.

## Is Ryokan multi-user?

Not supported and not on the roadmap. Single-admin only. The reasoning matches Sonarr's [PR #7186](https://github.com/Sonarr/Sonarr/pull/7186) rejection (Jan 2025): private-tracker account-sharing semantics get messy fast, and the PVR-shared-with-friends case is well-served by Jellyfin on top of a single-admin PVR.

## Can I use the API directly?

Yes. Swagger UI at `/api-docs`, OpenAPI JSON at `/api-docs/openapi.json`. Cookie-auth for the web-UI-facing endpoints; the Sonarr/Radarr shim uses API-key auth (`X-Api-Key` header or `?apikey=` query).

## How do I back up?

The whole `/data` volume. That's the SQLite DB, the artwork cache, the encryption key, the OAuth tokens (encrypted), and config sentinels. Standard SQLite backup tools work, or just stop Ryokan and copy the volume.

The encryption key is the load-bearing bit. Lose it but keep the DB, and every encrypted OAuth token in `external_accounts` is unrecoverable; you'll need to re-link those accounts.
