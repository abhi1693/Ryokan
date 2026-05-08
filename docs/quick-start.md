# Quick start

End-to-end: deploy Ryokan + Jellyfin + a download client, configure them, add a show, watch it land in your library.

If you want a complete stack with extras (Seerr, reverse proxy, VPN), use the **[Stack builder](stack-builder.md)** instead. It generates the whole `docker-compose.yml` for you.

## What you'll need

- **Docker** and **Docker Compose** installed (`docker --version` and `docker compose version` should both work at the terminal). New to Docker? Read [Docker's overview](https://docs.docker.com/get-started/docker-overview/) first; it covers what containers, images, and volumes are, which the rest of this page assumes you know.

You don't need a download client running in advance; we'll deploy one alongside Ryokan and Jellyfin in the next step. You also don't need a Prowlarr or AniList account; the built-in Nyaa search works without either.

## 1. Run Ryokan, Jellyfin, and your download client

Create a folder for your stack and `cd` into it. The compose file plus the bind-mounted host folders (`./downloads`, `./media/anime`) will all live here.

```sh
mkdir -p ~/ryokan-stack/{downloads,media/anime}
cd ~/ryokan-stack
```

Pick the download client you want to use and save the matching `docker-compose.yml` in that folder. All five composes deploy Ryokan + Jellyfin alongside the chosen client; the three services share `./media/anime` so files Ryokan imports show up in Jellyfin automatically.

=== "qBittorrent"

    ```yaml
    services:
      ryokan:
        image: ghcr.io/johnthreekay/ryokan:latest
        container_name: ryokan
        ports:
          - "8978:8978"
        volumes:
          - ryokan-data:/data
          - ./downloads:/downloads
          - ./media/anime:/media/anime
        environment:
          - PUID=1000
          - PGID=1000
          - TZ=Etc/UTC
        restart: unless-stopped

      jellyfin:
        image: jellyfin/jellyfin:latest
        container_name: jellyfin
        ports:
          - "8096:8096"
        volumes:
          - jellyfin-config:/config
          - jellyfin-cache:/cache
          - ./media/anime:/media/anime
        environment:
          - PUID=1000
          - PGID=1000
          - TZ=Etc/UTC
        restart: unless-stopped

      qbittorrent:
        image: lscr.io/linuxserver/qbittorrent:latest
        container_name: qbittorrent
        ports:
          - "8080:8080"        # web UI
          - "6881:6881"        # BT
          - "6881:6881/udp"
        volumes:
          - qbittorrent-config:/config
          - ./downloads:/downloads
        environment:
          - PUID=1000
          - PGID=1000
          - TZ=Etc/UTC
          - WEBUI_PORT=8080
        restart: unless-stopped

    volumes:
      ryokan-data:
      jellyfin-config:
      jellyfin-cache:
      qbittorrent-config:
    ```

=== "Deluge"

    ```yaml
    services:
      ryokan:
        image: ghcr.io/johnthreekay/ryokan:latest
        container_name: ryokan
        ports:
          - "8978:8978"
        volumes:
          - ryokan-data:/data
          - ./downloads:/downloads
          - ./media/anime:/media/anime
        environment:
          - PUID=1000
          - PGID=1000
          - TZ=Etc/UTC
        restart: unless-stopped

      jellyfin:
        image: jellyfin/jellyfin:latest
        container_name: jellyfin
        ports:
          - "8096:8096"
        volumes:
          - jellyfin-config:/config
          - jellyfin-cache:/cache
          - ./media/anime:/media/anime
        environment:
          - PUID=1000
          - PGID=1000
          - TZ=Etc/UTC
        restart: unless-stopped

      deluge:
        image: lscr.io/linuxserver/deluge:latest
        container_name: deluge
        ports:
          - "8112:8112"        # web UI
          - "6881:6881"        # BT
          - "6881:6881/udp"
        volumes:
          - deluge-config:/config
          - ./downloads:/downloads
        environment:
          - PUID=1000
          - PGID=1000
          - TZ=Etc/UTC
        restart: unless-stopped

    volumes:
      ryokan-data:
      jellyfin-config:
      jellyfin-cache:
      deluge-config:
    ```

=== "Transmission"

    ```yaml
    services:
      ryokan:
        image: ghcr.io/johnthreekay/ryokan:latest
        container_name: ryokan
        ports:
          - "8978:8978"
        volumes:
          - ryokan-data:/data
          - ./downloads:/downloads
          - ./media/anime:/media/anime
        environment:
          - PUID=1000
          - PGID=1000
          - TZ=Etc/UTC
        restart: unless-stopped

      jellyfin:
        image: jellyfin/jellyfin:latest
        container_name: jellyfin
        ports:
          - "8096:8096"
        volumes:
          - jellyfin-config:/config
          - jellyfin-cache:/cache
          - ./media/anime:/media/anime
        environment:
          - PUID=1000
          - PGID=1000
          - TZ=Etc/UTC
        restart: unless-stopped

      transmission:
        image: lscr.io/linuxserver/transmission:latest
        container_name: transmission
        ports:
          - "9091:9091"        # web UI
          - "51413:51413"      # BT
          - "51413:51413/udp"
        volumes:
          - transmission-config:/config
          - ./downloads:/downloads
        environment:
          - PUID=1000
          - PGID=1000
          - TZ=Etc/UTC
          - USER=admin
          - PASS=changeme      # change before first start
        restart: unless-stopped

    volumes:
      ryokan-data:
      jellyfin-config:
      jellyfin-cache:
      transmission-config:
    ```

=== "rTorrent"

    ```yaml
    services:
      ryokan:
        image: ghcr.io/johnthreekay/ryokan:latest
        container_name: ryokan
        ports:
          - "8978:8978"
        volumes:
          - ryokan-data:/data
          - ./downloads:/downloads
          - ./media/anime:/media/anime
        environment:
          - PUID=1000
          - PGID=1000
          - TZ=Etc/UTC
        restart: unless-stopped

      jellyfin:
        image: jellyfin/jellyfin:latest
        container_name: jellyfin
        ports:
          - "8096:8096"
        volumes:
          - jellyfin-config:/config
          - jellyfin-cache:/cache
          - ./media/anime:/media/anime
        environment:
          - PUID=1000
          - PGID=1000
          - TZ=Etc/UTC
        restart: unless-stopped

      rutorrent:
        image: crazymax/rtorrent-rutorrent:latest
        container_name: rutorrent
        ports:
          - "8080:8080"        # ruTorrent web UI
          - "8000:8000"        # XML-RPC (Ryokan talks to this)
          - "50000:50000"      # BT incoming peer connections
          - "6881:6881/udp"    # DHT
        volumes:
          - rutorrent-data:/data
          - ./downloads:/downloads     # contains /downloads/temp and /downloads/complete after first start
        environment:
          - PUID=1000
          - PGID=1000
          - TZ=Etc/UTC
        restart: unless-stopped

    volumes:
      ryokan-data:
      jellyfin-config:
      jellyfin-cache:
      rutorrent-data:
    ```

    !!! note "Image swap from linuxserver to crazy-max"
        linuxserver/rutorrent is deprecated; their README points at crazy-max's image as the maintained alternative. Different volume layout (one `/data` instead of `/config`), different port set, but the rTorrent it wraps is the same.

=== "SABnzbd"

    ```yaml
    services:
      ryokan:
        image: ghcr.io/johnthreekay/ryokan:latest
        container_name: ryokan
        ports:
          - "8978:8978"
        volumes:
          - ryokan-data:/data
          - ./downloads:/downloads
          - ./media/anime:/media/anime
        environment:
          - PUID=1000
          - PGID=1000
          - TZ=Etc/UTC
        restart: unless-stopped

      jellyfin:
        image: jellyfin/jellyfin:latest
        container_name: jellyfin
        ports:
          - "8096:8096"
        volumes:
          - jellyfin-config:/config
          - jellyfin-cache:/cache
          - ./media/anime:/media/anime
        environment:
          - PUID=1000
          - PGID=1000
          - TZ=Etc/UTC
        restart: unless-stopped

      sabnzbd:
        image: lscr.io/linuxserver/sabnzbd:latest
        container_name: sabnzbd
        ports:
          - "8081:8080"          # host:container; SAB lives on 8080 internally
        volumes:
          - sabnzbd-config:/config
          - sabnzbd-incomplete:/incomplete-downloads   # in-progress (named volume; not on host filesystem)
          - ./downloads:/downloads                     # completed (shared with Ryokan + Jellyfin)
        environment:
          - PUID=1000
          - PGID=1000
          - TZ=Etc/UTC
        restart: unless-stopped

    volumes:
      ryokan-data:
      jellyfin-config:
      jellyfin-cache:
      sabnzbd-config:
      sabnzbd-incomplete:
    ```

    !!! note "Why two folders?"
        SAB splits in-progress (`/incomplete-downloads`) from completed (`/downloads`). The compose maps the in-progress side to a Docker named volume so it's tucked out of the way; only completed downloads land in your stack folder where Ryokan reads them.

Bring it up:

```sh
docker compose up -d
```

Ryokan is now on port 8978, Jellyfin on 8096, your download client on its default port. The named volumes (`ryokan-data`, `jellyfin-config`, etc.) are managed by Docker and live under `/var/lib/docker/volumes/`; only the bind mounts (`./downloads`, `./media/anime`) end up in your stack folder.

??? info "How each client uses `/downloads`"
    The five clients structure that folder differently. Ryokan only reads completed files, so all five layouts work transparently.

    - **qBittorrent / Deluge**: write files directly to `/downloads/<category>/<file>` (or `/downloads/<file>` if no category is set). Single shared folder.
    - **Transmission**: writes everything to `/downloads/`, with a `.part` suffix on the filename while the torrent is in flight; renames on completion.
    - **rTorrent**: splits `/downloads/temp/` (in-progress) and `/downloads/complete/` (completed); files move from temp to complete when the torrent finishes.
    - **SABnzbd**: uses a separate folder for in-progress (`/incomplete-downloads/`, mapped to a Docker named volume in the compose so it stays out of the way) and `/downloads/` for completed.

!!! tip "Already running Jellyfin or your download client elsewhere?"
    Drop the relevant service block (and its volume entry at the bottom) from the compose, and skip the corresponding setup step below. Make sure the existing instance can read `./media/anime` for Jellyfin or write to `./downloads` for the download client, or adjust paths accordingly.

## 2. First login to Ryokan

Open <http://localhost:8978> in a browser. You'll be redirected to a setup page; pick a username and password and submit. That account is your admin account; Ryokan is single-user, so this is the only one you'll create.

Once you're logged in you'll see an empty library page. That's expected; we haven't told Ryokan about any shows yet.

## 3. Set up Jellyfin

Open <http://localhost:8096> in another tab. Walk through Jellyfin's first-run wizard:

1. Pick your display language.
2. Create a Jellyfin admin account (separate from Ryokan's).
3. **Add a media library**:
    - **Content type**: Shows
    - **Display name**: Anime (or whatever you like)
    - **Folder**: click the `+` and add `/media/anime`. This is the path Jellyfin sees inside its container; it maps to `~/ryokan-stack/media/anime` on your host, the same folder Ryokan writes to.
4. Accept the metadata defaults; you can tweak per-library later.
5. Finish the wizard.

Jellyfin's library will be empty for now. That's fine; once Ryokan grabs and imports its first episode, Jellyfin's scheduled scan will pick it up. You can also click **Scan All Libraries** in Dashboard → Libraries to force one immediately after a grab.

## 4. Add a download client to Ryokan

In Ryokan, go to **Settings → Download Clients → Add download client**. Fill in the values for your chosen client below.

=== "qBittorrent"

    First, fetch qBit's randomly-generated initial password from its logs:

    ```sh
    docker compose logs qbittorrent | grep -i "temporary password"
    ```

    Open <http://localhost:8080> and log in (`admin` / that temporary password). qBit will prompt you to set a real password; do that, then come back to Ryokan.

    In Ryokan's add-client form:

    - **Kind**: qBittorrent
    - **URL**: `http://qbittorrent:8080`
    - **Username**: `admin`
    - **Password**: the password you just set in qBit
    - **Category**: `ryokan-anime`
    - **Default for this protocol**: on

=== "Deluge"

    Open <http://localhost:8112>. The default Deluge web UI password is `deluge`; set a real one when prompted.

    In Ryokan's add-client form:

    - **Kind**: Deluge
    - **URL**: `http://deluge:8112`
    - **Password**: the password you set in Deluge's web UI
    - **Label**: `ryokan-anime`
    - **Default for this protocol**: on

=== "Transmission"

    Open <http://localhost:9091> and confirm Transmission is up. Auth is the `USER` and `PASS` you set in the compose file.

    In Ryokan's add-client form:

    - **Kind**: Transmission
    - **URL**: `http://transmission:9091`
    - **Username**: `admin` (matches `USER` in the compose)
    - **Password**: whatever you set `PASS` to in the compose
    - **Label**: `ryokan-anime`
    - **Default for this protocol**: on

=== "rTorrent"

    Open <http://localhost:8080> to confirm ruTorrent's web UI loads. The crazy-max image doesn't ship with auth by default; if you're exposing this beyond localhost, mount an htpasswd file at `/passwd` (the image's README covers how).

    In Ryokan's add-client form:

    - **Kind**: rTorrent
    - **URL**: `http://rutorrent:8000/RPC2` (port 8000 is the dedicated XML-RPC port; 8080 is the web UI)
    - **Username** / **Password**: leave blank unless you've set up htpasswd
    - **Label**: `ryokan-anime`
    - **Default for this protocol**: on

=== "SABnzbd"

    Open <http://localhost:8081> and walk through SAB's first-run wizard. When you reach the final step, SAB shows you an API key; save it. (You can also pull it later from **Config → General → Security → API Key**; make sure it's the **full** API key, not the read-only `nzb_api_key`.)

    In Ryokan's add-client form:

    - **Kind**: SABnzbd
    - **URL**: `http://sabnzbd:8080`
    - **API Key**: paste the full API key
    - **Category**: `ryokan-anime` (Ryokan auto-creates this in SAB if it doesn't exist)
    - **Default for this protocol**: on

Click **Test connection** in Ryokan. You should see "Connected" with a version number. If not, the [Download clients page](download-clients.md) has per-client troubleshooting.

Save the row.

## 5. Set the media root

In Ryokan, go to **Settings → General → Media Root Path** and set it to `/media/anime`. That's the path inside Ryokan's container; it maps to `~/ryokan-stack/media/anime` on your host (the same folder Jellyfin reads from).

!!! warning "PUID and PGID matter for shared folders"
    The `1000:1000` defaults work for most homelabs but not all. If files Ryokan writes show up with the wrong owner and Jellyfin can't read them, run `id -u` and `id -g` on your media-owning user and update both services' `PUID` / `PGID`. [Installation → PUID and PGID](install.md#puid-and-pgid) explains why.

## 6. (Optional) Add an indexer

Skip this for now if you want; Nyaa is built in and works out of the box. But if you have a Prowlarr or Jackett set up with private trackers, this is the moment to wire those in.

**Settings → Indexers → Add indexer**. Paste the URL Prowlarr or Jackett gave you (it ends in `/api`), the API key, and pick a name. The defaults handle the rest.

Click **Test connection** to confirm Ryokan can reach it.

## 7. Add a show and watch it land

Go back to the library page, click **Add series**, type the name of an anime you want, and pick the right one from the dropdown. Ryokan fetches metadata from AniList by default.

When the series page opens, click an episode you want, then **Search**. You'll see a list of releases ranked by Ryokan's scoring. Pick one and click **Grab**, or hit **Auto-search** to let Ryokan pick the highest-scored release for you.

The grab fires off to your download client. When it finishes:

1. Post-processing hardlinks the file into `~/ryokan-stack/media/anime/<show name>/Season 01/<episode>.mkv` on your host.
2. Jellyfin picks it up on its next library scan (or immediately if you click **Scan All Libraries**).
3. The episode is now playable from any Jellyfin client (web, mobile, TV).

## 8. (Optional) Link AniList or MAL

If you want Ryokan to add new shows automatically when you mark them watching on AniList or MAL, the **[External accounts](external-accounts.md)** page walks through linking. You can do this any time; your existing manually-added series stay put.

## What next?

- **[Configuration](configuration.md)** explains every Settings tab so you can tune scoring, choose between hardlink and copy, set up a quality profile, and so on.
- **[Stack builder](stack-builder.md)** generates the rest of the homelab stack (Seerr for requests, Caddy / Traefik for HTTPS, Gluetun for VPN-routed grabs) in the same shape if you want to grow beyond the basics.
- **[Troubleshooting](troubleshooting.md)** has the most common stumbles and their fixes.

---

*Last updated: 2026-05-07.*
