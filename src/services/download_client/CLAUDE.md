# services/download_client/CLAUDE.md

`DownloadClient` is the trait abstraction over the four supported torrent clients (qBittorrent, Deluge, Transmission, rTorrent). One client is active per Ryokan instance, swapped on Settings save via `build_download_client`. Concrete impls live in `qbittorrent/`, `deluge/`, `transmission/`, `rtorrent/`, each with a `wiremock_tests/` sibling directory of HTTP-mock tests (distinct from the env-gated `live_smoke` tests in the parent `mod.rs`).

`AppState.download_client` is `Arc<RwLock<Option<Arc<dyn DownloadClient>>>>`. Torrents are addressed by **v1 infohash, lowercase hex at the trait boundary** — each impl case-munges internally for its wire format. The `pick` callback in `add_torrent_with_file_filter` is `&mut dyn FnMut` (not generic) to keep the trait object-safe.

## Per-client scoping

Every impl has a distinct "torrents Ryokan added" filter so `list_scoped` never returns torrents from other tooling:

- **qBit**: `?category=<config.qbit_category>` (default `anime`)
- **Deluge**: Label plugin (auto-enabled on first connect; see Deluge quirks)
- **Transmission**: native labels on 4.x, save-path prefix fallback on older
- **rtorrent**: `custom1` field (the ruTorrent label convention)

Set at add-time, read at list-time.

## Per-client `download_path` + `translate_client_path`

Ryokan and the client don't always see the same filesystem (Docker volumes on different host paths, seedbox over SSHFS/NFS/rclone). Each client gets its own `{qbit,deluge,transmission,rtorrent}_download_path` config field; `per_client_download_path(&config)` resolves the right one via `active_client`.

`translate_client_path(path, client_save_path, local_download_path)` rewrites a client-reported path by replacing the client's `save_path` prefix with Ryokan's local mount. Trailing slashes normalized; empty `local_download_path` = no rewrite; a path that doesn't start with `client_save_path` is **returned unchanged** rather than silently rewritten — silent rewrite would mask misconfiguration as a later "file not found."

Do not reintroduce a single shared global remote-path mapping.

## Megapack narrowing

`add_torrent_with_file_filter` pauses the torrent, waits for metadata, runs the caller's `pick` closure over the file names, sets non-picked files to skip, resumes. Used from the interactive selective-download flow with a **10s** metadata ceiling (the user is waiting). Each impl handles its own wait-and-narrow loop and **must be idempotent on retry**: read each file's `wanted` flag back before changing it so a re-narrow doesn't clobber user edits.

Distinct from `services::auto_expand` (sibling-series detection inside a batch pack — different problem, different code path).

## qBit quirks (`qbittorrent/mod.rs`)

- `content_path` is exposed natively (≥2.6.1) — no common-prefix computation.
- File-priority scale is **0/1/6/7**; Ryokan only writes 0 (skip) or 1 (normal).
- qBit 5.x renamed pause/resume → stop/start. Impl tries the new names first and falls back to old without a version probe so 4.x and 5.x both work.
- **qBit 5.x duplicate-add returns `200 "Fails."`** indistinguishable from the body it uses for a malformed magnet. `add_torrent` disambiguates by probing `/torrents/info?hashes=<hash>` after a `Fails.` and reports `AddOutcome::AlreadyPresent` when the hash is in the session. Without this, every re-grab of an already-present torrent (RSS re-emissions, upgrade-sweep collisions, post-crash regrabs) hard-fails.
- `list_scoped` uses a 2s coalescing cache with single-flight election via `AtomicBool` + `Notify` + RAII `FetchFlightGuard`. The guard clears the in-flight flag on drop including the panic path so a panic inside the fetcher can't wedge the flag forever.
- Re-auth on 403 via session cookie.
- **When grabs vanish silently**: qBit's `POST /torrents/add` returns `Ok.` and fetches the `.torrent` async server-side. A silent fetch failure (tracker timeout, 404, etc.) masquerades as a Ryokan bug — check qBit's own logs first.

## Deluge quirks (`deluge/mod.rs`)

- **Two-step connect.** `auth.login(password)` establishes a session cookie but the freshly-authenticated session isn't connected to any daemon; every `core.*` call fails with `"Unknown method"` (NOT "not connected" — methods aren't even registered on the web process) until `web.connect(host_id)` runs. The single most common first-time integration failure.
- **Label plugin required for scoping.** Bundled but disabled by default; the connection test enables it via `core.enable_plugin` when it sees `Label` in `available_plugins` but not `enabled_plugins`. Upstream Deluge bug: an enabled-but-not-restarted Label plugin leaves RPC methods unregistered on the web process for one session — re-call `web.connect` after enabling to force method re-registration.
- File-priority scale is **0/1/4/7** (Skip/Low/Normal/High), **NOT** qBit's 0/1/6/7. Writing `1` for "wanted" would set the file to Low priority. Ryokan writes 0 for skip and 4 for wanted.
- Duplicate-add detection is substring-matching on `"Torrent already in session"` / `"Torrent already being added"` (deluge-dev/#3507 — error code fluctuates across versions).
- **No `has_metadata` field** in `core.get_torrent_status` (live-probed against 2.x + Label plugin 0.3); proxy: `files` array non-empty.
- Every deserializer uses `#[serde(default)]` because `get_torrent_status` silently drops unknown keys rather than returning an error.

## Transmission quirks (`transmission/mod.rs`)

- **CSRF session handshake.** Every first request returns 409 + `X-Transmission-Session-Id` header that must echo on every subsequent request. Session ID rotates on daemon restart; mid-stream 409 means re-capture and retry once. The `send` helper handles both transparently.
- Auth is **HTTP Basic**, not RPC-level — wrong creds surface as 401, not an RPC envelope error.
- Native labels in 4.x; Ryokan filters `labels.contains(self.label)` client-side (RPC has no server-side label filter).
- File-selection is **0/1 (unwanted/wanted)** via parallel `files-wanted: [idx]` / `files-unwanted: [idx]` arrays. Priority high/normal/low is a *separate* axis Ryokan deliberately doesn't touch.
- Duplicate-add surfaces as `torrent-duplicate` key inside `result: "success"` envelope (not as an error). No message parsing.
- **Completion is `percentDone >= 1.0`**, NOT `isFinished`. `isFinished` means "hit seed ratio/time target" (user-defined stop condition), not "download complete."
- Status codes 0..=6: 0=Stopped, 1=Queued-to-verify, 2=Verifying, 3=Queued-to-download, 4=Downloading, 5=Queued-to-seed, 6=Seeding.

## rtorrent quirks (`rtorrent/mod.rs`)

- Speaks **XML-RPC** over HTTP to `/RPC2`.
- **Hashes are UPPERCASE on the wire.** Every `d.<method>` / `f.<method>` call keyed by hash takes uppercase-hex; conversion happens inside every helper, not at call sites — trait contract says callers pass lowercase hex.
- Every method takes a target, even `d.multicall2` (empty string as target).
- **Duplicate-add is silent** — `load.start_verbose` returns `0` on both fresh and duplicate adds. Ryokan pre-checks by listing hashes and returns `AddOutcome::AlreadyPresent` when known.
- File priority is binary 0/1 (NOT Deluge's 0/4), BUT after setting priorities **you MUST call `d.update_priorities(<hash>)`** or the new priorities don't take effect. The single most common "my script sets priorities and nothing happens" bug in rtorrent automation.
- **`d.erase` does NOT touch disk** — per cmd-ref verbatim: "the data stored for the item is not touched in any way." Read `content_path` first, call `d.erase`, then recursively remove the FS path. Guard with `content_path != d.directory` so a multi-file torrent dumped at the save root doesn't nuke the entire download dir. Recursive remove runs in `tokio::task::spawn_blocking`.
- `d.base_path` is empty on closed/stopped torrents and after rtorrent restart; fall back to `d.directory + "/" + d.name` when empty.
- During metadata fetch, `base_path` ends in `.meta` (also the signal metadata hasn't arrived); post-metadata it rewrites to actual content name. Poll `!base_path.ends_with(".meta")` at 500ms cadence, **60s budget** (longer than other clients — cold DHT legitimately takes longer).
- Wire tags: rtorrent returns `<i8>` for sizes / rates / most counters; the decoder accepts both `<i4>` and `<i8>`.

## Live-smoke tests

Each impl ships a `#[ignore]`d `live_smoke` test that exercises the full trait surface against a real client on localhost. Run with `--ignored` *and* the corresponding env var set:

| Var | Default password / config |
|---|---|
| `RYOKAN_QBIT_E2E=1` | `QBIT_PASS=adminadmin` (qBit-on-first-start default) |
| `RYOKAN_DELUGE_E2E=1` | password from settings |
| `RYOKAN_TRANSMISSION_E2E=1` | settings creds |
| `RYOKAN_RTORRENT_E2E=1` | (no auth) |

CI never runs these — they're for hand-validation when touching a client impl.
