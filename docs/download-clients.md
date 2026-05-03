# Download clients

Ryokan currently supports five download clients: qBittorrent, Deluge, Transmission, rTorrent, and SABnzbd. Multiple can be configured simultaneously and routed per-grab via indexer pins or per-protocol defaults.

Add clients under **Settings → Download Clients**. Each row needs a URL, credentials (where applicable), and a label / category Ryokan uses to scope its visibility into the client (so it can't see torrents added by other tools and vice versa).

## qBittorrent

- Tested against 4.x and 5.x; `add_torrent` auto-detects 5.x's renamed `stop`/`start` endpoints (was `pause`/`resume` in 4.x) by trying new names first and falling back without a version probe.
- Scoping: per-row `category`. Ryokan's grabs all land with that category; `list_scoped` filters by it.
- Re-auth on 403 via session cookie; you don't have to manage tokens manually.

!!! warning "Silent grab failures"
    qBit's `POST /torrents/add` returns `Ok.` and fetches the `.torrent` URL **server-side**. A silent fetch failure (tracker timeout, indexer 404) looks identical to a successful add from Ryokan's perspective. If a grab "vanishes" without showing up in qBit, check qBit's own logs first since that's where the failure lives.

## Deluge

- **Two-step connect.** `auth.login(password)` establishes a session, but the freshly-authenticated session isn't connected to any daemon. Every `core.*` call fails with `Unknown method` until `web.connect(host_id)` runs. Ryokan handles this in the connection-test path; if you see "Unknown method" errors after configuring Deluge, it usually means the daemon side restarted and the web process needs a re-connect (re-clicking Test connection refreshes it).
- **Label plugin required.** It's bundled but disabled by default. Ryokan's connection test enables it via `core.enable_plugin` automatically. There's a known upstream Deluge bug where an enabled-but-not-restarted Label plugin leaves RPC methods unregistered for one session; if it doesn't take, click Test connection again.

## Transmission

- CSRF session handshake; Ryokan handles this transparently. Daemon restart rotates the session ID; mid-stream 409s are retried once automatically.
- Auth is **HTTP Basic**, not RPC-level. Wrong credentials surface as 401, not as an RPC envelope error.
- Native labels in 4.x.

## rTorrent

- Speaks XML-RPC over HTTP to `/RPC2`. Most installations expose this through ruTorrent's `httprpc` plugin or directly via SCGI fronted by nginx.
- Scoping: the `custom1` field (the ruTorrent label convention).
- **`d.erase` doesn't touch disk.** rTorrent's docs are explicit: removing a torrent leaves the data in place. Ryokan reads `content_path` first, calls `d.erase`, then recursively removes the FS path. There's a guard preventing a multi-file torrent dumped at the save root from nuking the entire download directory, but if you've configured rTorrent in an unusual way this is the bit to double-check.
- **Cold-DHT metadata fetch is slow.** rTorrent's metadata-fetch budget is 60s here vs. 10s for the BT clients with trackers; the longer budget is real, not a Ryokan-side throttle.

## SABnzbd

- **Endpoint shape**: `GET <base>/api?apikey=…&mode=…&output=json`. The user-configured base IS the base; Ryokan appends `/api`. Most SAB installs are at `http://host:8080`; the legacy `URL_BASE=/sabnzbd` configurations want `http://host:8080/sabnzbd` as the configured base.
- **API key**: SAB has two: the **full API key** and the read-only `nzb_api_key`. Ryokan needs the full one for queue management (cancel, change_cat). The Test-connection probe catches a wrong/missing key at config time instead of at first grab.
- **Auto-creates the configured category.** If the category Ryokan was configured with doesn't exist in SAB, the connection-test path creates it via `set_config`. Same auto-create runs on first grab as a safety net for users who skipped the Test button. Without this, NZBs land in SAB's default bucket, Ryokan's `list_scoped` filters them out, and grabs appear to vanish. See [Troubleshooting → SAB downloads vanish](troubleshooting.md#sab-downloads-disappear-from-ryokan-but-still-download-in-sab).
- **No per-file selection.** NZBs are opaque blobs until SAB extracts them. Interactive picker still works (it shows the file list for selection if SAB has parsed the headers), but the actual file selection is no-op'd at the wire; SAB downloads the whole NZB, then post-processing imports the files Ryokan wanted.

## Per-client download paths

Each client gets its own `*_download_path` config field for cases where Ryokan and the client see the filesystem differently: Docker volumes mounted at different host paths, seedboxes accessed over SSHFS, etc.

The translation logic: when the client reports a path, Ryokan substitutes the client's `save_path` prefix with the configured `download_path` to get a path Ryokan itself can read. If the client-reported path doesn't start with the expected prefix, Ryokan returns it unchanged rather than silently rewriting; silent rewrite would mask misconfiguration as a "file not found" later.

## Routing

- **Per-indexer pin.** A torznab/newznab indexer row has an optional `download_client_id`. Grabs from that indexer go to the pinned client.
- **Per-protocol default.** With no pin, torznab → torrent default, newznab → usenet default. Both defaults can coexist (one row per protocol marked `is_default = 1`).
- **Nyaa.** Has its own `nyaa_download_client_id` config field. Always falls back to the torrent default (Nyaa items are magnets / `.torrent` URLs; routing to a usenet client would just trip the protocol guard at add-time).
- **At grab time.** The chosen client ID is stamped on the `grabbed_torrents` row, so post-processing routes back through the same client even if defaults change later.
