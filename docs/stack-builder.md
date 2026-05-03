---
title: Stack builder
hide:
  - toc
---

# Stack builder

Pick the pieces you want and copy the generated `docker-compose.yml` straight into your homelab. Everything is pre-wired: paths line up so post-processing hardlinks work without fiddling, PUID/PGID is consistent, and the per-client Ryokan settings are printed alongside so you know what to paste into Settings → Download Clients.

This is opinionated. Sane defaults beat a config matrix. If you need something the picker doesn't generate, copy the output and edit by hand; the comments call out the load-bearing bits.

<form id="stack-form" class="stack-form">

<fieldset>
  <legend>Download client(s)</legend>
  <p class="hint">Multi-select. Pick more than one to set up multi-client routing (Ryokan picks one default per protocol). Indexers can be pinned per-client in Ryokan's Settings.</p>
  <label><input type="checkbox" name="dlclient" value="qbittorrent" checked> qBittorrent</label>
  <label><input type="checkbox" name="dlclient" value="deluge"> Deluge</label>
  <label><input type="checkbox" name="dlclient" value="transmission"> Transmission</label>
  <label><input type="checkbox" name="dlclient" value="rtorrent"> rTorrent (ruTorrent)</label>
  <label><input type="checkbox" name="dlclient" value="sabnzbd"> SABnzbd</label>
</fieldset>

<fieldset>
  <legend>Media server</legend>
  <p class="hint">Ryokan integrates with Jellyfin (library refresh, on-disk validation). Plex and Emby aren't supported as integrations.</p>
  <label><input type="radio" name="media_server" value="jellyfin" checked> Jellyfin</label>
  <label><input type="radio" name="media_server" value="none"> None</label>
</fieldset>

<fieldset>
  <legend>Request frontend</legend>
  <p class="hint">Seerr (the Jellyseerr + Overseerr merger, Feb 2026) requests anime through Ryokan via the Sonarr/Radarr API shim.</p>
  <label><input type="radio" name="requests" value="seerr" checked> Seerr</label>
  <label><input type="radio" name="requests" value="none"> None</label>
</fieldset>

<fieldset>
  <legend>VPN</legend>
  <p class="hint">Routes torrent download clients through Gluetun's network namespace. SAB stays outside the VPN (Usenet talks TLS to your provider, not to peers). You'll need to fill in your provider credentials in the generated compose; gluetun's wiki at <a href="https://github.com/qdm12/gluetun-wiki" target="_blank" rel="noopener">qdm12/gluetun-wiki</a> lists the env vars per provider.</p>
  <label><input type="radio" name="vpn" value="none" checked> None</label>
  <label><input type="radio" name="vpn" value="gluetun"> Gluetun (Mullvad, ProtonVPN, PIA, NordVPN, custom)</label>
</fieldset>

<fieldset>
  <legend>Reverse proxy</legend>
  <p class="hint">Adds the proxy container and stub config; you'll still need to point a real domain at it. Cloudflare Tunnel skips the proxy container entirely (Cloudflare's edge does TLS).</p>
  <label><input type="radio" name="proxy" value="none" checked> None</label>
  <label><input type="radio" name="proxy" value="caddy"> Caddy</label>
  <label><input type="radio" name="proxy" value="traefik"> Traefik</label>
  <label><input type="radio" name="proxy" value="nginx"> nginx (manual config)</label>
  <label><input type="radio" name="proxy" value="cloudflared"> Cloudflare Tunnel</label>
</fieldset>

<fieldset>
  <legend>User / group</legend>
  <p class="hint">Run <code>id -u</code> / <code>id -g</code> on the host to find these. Match the user that owns your media library so post-processed files land with the right ownership.</p>
  <label>PUID <input type="number" name="puid" value="1000" min="0"></label>
  <label>PGID <input type="number" name="pgid" value="1000" min="0"></label>
  <label>TZ <input type="text" name="tz" value="UTC" placeholder="e.g. America/Chicago"></label>
</fieldset>

<fieldset>
  <legend>Host paths</legend>
  <p class="hint">Where on your host the data lives. The defaults match the canonical layout. Both downloads and media should be on the same filesystem if you want post-processing to use hardlinks.</p>
  <label>Downloads <input type="text" name="downloads_path" value="/srv/downloads"></label>
  <label>Media library <input type="text" name="media_path" value="/srv/media/anime"></label>
  <label>App config root <input type="text" name="appdata_path" value="/srv/appdata"></label>
</fieldset>

</form>

## Generated `docker-compose.yml`

<button type="button" id="copy-compose" class="md-button md-button--primary">Copy</button>

<pre id="compose-output" class="stack-output"><code class="language-yaml">Loading…</code></pre>

## Ryokan settings to paste in

After the stack is up, log into Ryokan at `http://localhost:8978` and paste these values into the matching Settings panels.

<pre id="settings-output" class="stack-output"><code>Loading…</code></pre>

## Notes

- **Hardlinks**: post-processing defaults to hardlink mode. Both the downloads and media paths above are mounted at matching paths inside Ryokan's container, so qBit-reports `/downloads/foo.mkv` and Ryokan-sees `/downloads/foo.mkv` (no path translation needed). If you split downloads and media across different host filesystems, hardlinks will fall back to copy automatically.
- **First-run**: every container needs its `appdata` subdirectory pre-owned by the PUID/PGID you set above. `mkdir -p /srv/appdata/{ryokan,...} && chown -R 1000:1000 /srv/appdata` once before `docker compose up`.
- **Reverse proxy**: the generated config is a stub. You'll need to point a real domain (or Cloudflare Tunnel route) at the proxy and edit the proxy's config file with your actual hostname.
- **VPN**: Gluetun expects WireGuard or OpenVPN credentials in its env. Wrong/missing credentials show up as connection failures on the download client (which can't reach trackers). The gluetun container's logs are the right place to debug.
