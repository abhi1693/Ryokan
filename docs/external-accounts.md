# External accounts

Ryokan can sync your watch-list from AniList or MyAnimeList. Linked accounts auto-add series to your library based on import preferences (Watching, Planning, Paused, Dropped, Completed).

Configure under **Settings → Integrations → External Accounts**.

## AniList

- **OAuth flow**: Ryokan opens AL's authorize page in a new tab via `/start`. After you approve, AL redirects to a broker page hosted alongside Ryokan's docs at `johnthreekay.github.io/Ryokan/auth/anilist/`. Copy the access token + state from the broker page back into Ryokan's paste modal.
- **Implicit grant**: token lasts a year; no refresh-token flow.
- **Scoreformat-aware**: Ryokan reads your AL `mediaListOptions.scoreFormat` (POINT_10 / POINT_100 / etc.) on every sync, so flipping the format on AL takes effect on the next tick without unlinking.

## MyAnimeList

- **PKCE OAuth flow**. Same redirect-to-broker shape as AL, but with a code-for-token exchange and a refresh-token kept on the row.
- **Refresh on 401**: the sync engine auto-refreshes the access token when MAL returns 401 mid-fetch and retries.
- **Token expires** every ~30 days; the refresh handles this transparently.

## Watch-list sync

The supervised `external_sync` task ticks every `external_sync_interval_minutes` (clamped to ≥15). Each tick:

1. Fetches the watch-list since the last cursor (delta) or full-resync.
2. Filters by your import preferences (Watching, Planning, etc.).
3. Pre-fetches AnimeDetail for new ids in one batch.
4. Merges into the library — creates new series rows, updates monitor mode on existing ones.
5. Detects removals on full-resync ticks (delta runs can't, by definition).

Manual "Sync now" button on the External Accounts card forces an immediate tick.

## Failure modes

- **Token expired / revoked**: AniList / MAL responds with a GraphQL error containing `"token"` or with HTTP 401. Ryokan logs the failure and surfaces "user may need to re-link" in the System logs. The supervised loop's exponential backoff defers retries.
- **Rate limited**: AniList caps at 30 req/min in degraded mode (the current state). The sync hits its rate-limit machinery before firing the next request — see [Troubleshooting → AniList rate limits](troubleshooting.md#anilist-keeps-returning-429-too-many-requests).
- **Token rejected on link** (the most common 400 on the link/submit flow): the Viewer probe Ryokan uses to validate the token shares the same per-account quota. If your account is in a rate-limit cooldown, *re-linking* hits the same wall as syncing. See [Troubleshooting → Per-account AniList cooldown](troubleshooting.md#anilist-per-account-cooldown-stuck-past-60s).

## Provider order is fixed for the Sonarr / Radarr shim

The anibridge shim (Sonarr/Radarr API exposed for Seerr) does AL-first, MAL-on-AL-down. There's no user-facing toggle for this and no plan to add one — Seerr expects a stable provider behavior, and falling back inconsistently confuses its caching.
