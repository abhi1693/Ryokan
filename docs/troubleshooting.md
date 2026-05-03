# Troubleshooting

Common issues, with concrete diagnostic steps.

## Things to check first

- **System → Logs** filters per category (AniList, Jikan, Kitsu, Grab, AutoSearch, Nyaa, DownloadClient, Jellyfin, etc). Most "why didn't this work" answers live here.
- **Test connection** on each download-client row (Settings → Download Clients) and each indexer row (Settings → Indexers). Connection tests catch most config issues at config time rather than at grab time.
- **The grab-history modal** on each episode shows every release ever grabbed for that episode, with state (`grabbed` / `completed` / `failed` / `removed` / `replaced`) and timestamp. Useful for "why is this episode in this state" questions.

## SAB downloads disappear from Ryokan but still download in SAB

You'll see this in System → Logs as a debug-level line:

```
sab list_scoped: dropped every slot via category filter — configured_category="anime" queue_slots=0 history_slots=1 seen_categories={"default"}
```

What happened: SAB doesn't have the category Ryokan was configured to use. It accepted the NZB but landed it in the default bucket. Ryokan's `list_scoped` filters by category to avoid accidentally treating other tools' jobs as its own, so the job becomes invisible.

**Fix**: click Test connection on the SAB row in Settings → Download Clients. The auto-create path will create the missing category and re-tag the just-added job. After that, future grabs land correctly.

If Test connection shows `(warning: SAB rejected category creation: HTTP 403 ...)`, your SAB API key is the read-only `nzb_api_key`. Use the **full API Key** from SAB → Config → General → Security → API Key.

## Cancel Pending doesn't actually remove the SAB job

Was a real bug as of 1.4.x; fixed in 1.5.x. Update Ryokan and try again. The bug was that the SAB delete code tried `mode=history&name=delete` first, which phantom-succeeds on unknown nzo_ids; for an in-flight grab (still in queue, not history), the delete would claim success without actually touching the queue. Queue-first ordering fixes it.

## AniList keeps returning 429 Too Many Requests

Open the most recent failure in System → Logs. The detail line now carries diagnostic headers:

```
[limit=30 remaining=0 reset=1777813117 retry_after=6 ryokan_60s=27]
```

- **`remaining=0` AND `ryokan_60s` close to 30** → Ryokan over-fired. This shouldn't happen with the rate-limit clamp, but if it does, file an issue.
- **`remaining=0` AND `ryokan_60s` low** → the budget was burned outside Ryokan-this-process. Candidates: another tab on anilist.co (each profile-page render makes many GraphQL calls), a second Ryokan instance pointed at the same AL account, an extension or helper tool.
- **`no rate-limit headers`** → the 429 came from somewhere other than AL's normal rate-limiter (Cloudflare, an upstream proxy, AL's auth layer misusing 429 for token issues).

AL doesn't document a per-token quota, but in practice authenticated calls (`MediaListCollection`, `Viewer`) seem to have one separate from the global per-IP cap; unauthenticated search can succeed while authenticated calls 429.

## AniList per-account cooldown stuck past 60s

If `external_sync` keeps 429ing despite `ryokan_60s=1` and minutes between attempts, AL has likely flagged your account for an extended cooldown. The documented window is 60s rolling but the (undocumented) burst limiter can hold an account for hours.

**What to do**:

1. Stop any other Ryokan instance you might have running (`pgrep -fa ryokan`, `docker ps`, `systemctl status`).
2. Close any anilist.co tabs in your browser.
3. Don't fire manual Sync Now / Search Missing; let the supervised loop's exponential backoff carry you (15 min × 2^errors, capped after 5 errors at ~8h between attempts).
4. Wait. The cooldown clears on AL's end with no action from your side.

Re-linking the account during a cooldown won't help; the OAuth submit handler validates the new token via a `Viewer` probe that hits the same per-account quota. You'll get a "Link failed" with the same 429 in the surfaced error message.

## Episode shows "Importing…" forever

The poller saw the torrent reach 100% but the post-processing tick hasn't moved the file into the library yet. Two real causes:

- **Post-processing is disabled** in Settings → General. Ryokan correctly leaves the file at the download client's path; the row shouldn't be showing "Importing…" in this state. If it is, force-refresh the page (Ctrl+Shift+R); there's a known race where the per-row state can lag the global toggle.
- **Post-processing is on but the import is failing.** Check System → Logs filtered to `PostProcess`. Common causes: `media_root` isn't writable by the runtime user, the media filesystem is full, or Ryokan can't see the download client's complete path (per-client `download_path` mismatch; see [Download clients → Per-client download paths](download-clients.md#per-client-download-paths)).

## Series-page state is stale

Most live-state surfaces (download progress bars, season-size badge, modal-footer buttons) update via a 5s poller. If something looks wrong:

1. **Refresh the page** (F5). The server-rendered page is the ground truth; if refresh fixes it, it's a JS-side staleness bug worth filing.
2. If refresh *doesn't* fix it, the underlying DB state is what you're seeing. Check the grab-history modal for an authoritative view of that episode's grab state.

## Search returns no results

Check System → Logs filtered to `Search` and `AutoSearch`. The most common causes:

- **Profile mismatch**: your active quality profile doesn't accept any of the released qualities. Try widening the profile (Settings → Preferred Quality & Releases) or use the Interactive Search button on the episode for a one-off relaxed search.
- **Custom Format scoring threshold**: if `custom_format_minimum_score` is set (Settings → Custom Formats), releases scoring below it are silently dropped from auto-search candidates. They still show up in interactive search.
- **Indexer down**: an unreachable torznab indexer doesn't fail-fast; it just contributes nothing to the merged result set. Test connection on each indexer to verify.

## Migrations failing on first boot

Ryokan's migrations are designed to be idempotent (`ALTER TABLE … ADD COLUMN … .ok()` swallows already-exists errors), but a corrupt SQLite file from an earlier crash can wedge them. Stop Ryokan, back up `data/ryokan.db`, run `sqlite3 data/ryokan.db "PRAGMA integrity_check;"`. If it reports anything other than `ok`, restore from a backup or accept losing the DB and starting fresh.
