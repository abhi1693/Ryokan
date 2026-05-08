# Quick start

End-to-end: deploy Ryokan + Jellyfin, configure a download client, add a show, watch it land in your library.

If you want a complete stack with extras (Seerr, reverse proxy, VPN), use the **[Stack builder](stack-builder.md)** instead. It generates the whole `docker-compose.yml` for you.

## What you'll need

- **Docker** and **Docker Compose** installed (`docker --version` and `docker compose version` should both work at the terminal). New to Docker? Read [Docker's overview](https://docs.docker.com/get-started/docker-overview/) first; it covers what containers, images, and volumes are, which the rest of this page assumes you know.
- **A torrent or usenet client** running somewhere your machine can reach. Doesn't have to be on the same host. Supported: qBittorrent, Deluge, Transmission, rTorrent, SABnzbd.

You don't need a Prowlarr or AniList account for this walkthrough; the built-in Nyaa search works without either.

## 1. Run Ryokan and Jellyfin

Save this as `docker-compose.yml` somewhere; your home directory is fine for now. The compose deploys both Ryokan and Jellyfin together with a shared media folder so files Ryokan imports show up in Jellyfin automatically.

```yaml
services:
  ryokan:
    image: ghcr.io/johnthreekay/ryokan:latest
    container_name: ryokan
    ports:
      - "8978:8978"
    volumes:
      - ryokan-data:/data
      - /srv/downloads:/downloads        # where your download client puts completed files
      - /srv/media/anime:/media/anime    # where Ryokan should land imported episodes
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
      - /srv/media/anime:/media/anime    # same library Ryokan writes to
    environment:
      - PUID=1000
      - PGID=1000
      - TZ=Etc/UTC
    restart: unless-stopped

volumes:
  ryokan-data:
  jellyfin-config:
  jellyfin-cache:
```

Adjust `/srv/media/anime` and `/srv/downloads` to wherever you want those folders on your host. Then:

```sh
docker compose up -d
```

Ryokan is now running on port 8978, Jellyfin on 8096.

!!! tip "Already running Jellyfin elsewhere?"
    Drop the `jellyfin` service from the compose above and the volume entries that go with it. Make sure your existing Jellyfin can read `/srv/media/anime` (or wherever you're putting the library) and skip ahead to step 4.

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
    - **Folder**: click the `+` and add `/media/anime`. This is the path Jellyfin sees inside its container; it maps to `/srv/media/anime` on your host, the same folder Ryokan writes to.
4. Accept the metadata defaults; you can tweak per-library later.
5. Finish the wizard.

Jellyfin's library will be empty for now. That's fine; once Ryokan grabs and imports its first episode, Jellyfin's scheduled scan will pick it up. (Jellyfin scans automatically every couple of hours by default; you can also click **Scan All Libraries** in Dashboard → Libraries to force one immediately after a grab.)

## 4. Add a download client to Ryokan

Back in Ryokan, go to **Settings → Download Clients** in the top-right menu, then **Add download client**. Pick the kind that matches what you're running and fill in:

- **URL**: where Ryokan can reach it. If your client is also in Docker on the same machine, this is usually `http://<container-name>:<port>`. If it's on another host on your network, use that host's IP. If both Ryokan and the client are running on Docker Desktop on the same machine, see the per-client notes in the [Download clients](download-clients.md) page.
- **Username and password**: what you'd use to log into the client's own web UI.
- **Category** (torrent clients) or **Category** (SABnzbd): pick something distinctive like `ryokan-anime`. Ryokan only sees its own grabs, so this name keeps things scoped.
- **Default for this protocol**: turn this on for your first client. Without a default, Ryokan won't know where to send grabs.

Click **Test connection**. You should see "Connected" with a version number. If you see an error, the [Download clients page](download-clients.md) has per-client notes.

Save the row.

## 5. Set the media root

In Ryokan, go to **Settings → General → Media Root Path** and set it to `/media/anime`. That's the path inside Ryokan's container; it maps to `/srv/media/anime` on your host (the same folder Jellyfin reads from).

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

1. Post-processing hardlinks the file into `/srv/media/anime/<show name>/Season 01/<episode>.mkv` on your host.
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
