# Quick start

End-to-end: install, log in, configure a download client and an indexer, add a show, watch it grab.

If you don't have a download client yet, or you want a complete stack with Jellyfin and Seerr in one shot, start with the **[Stack builder](stack-builder.md)** instead. It generates the whole `docker-compose.yml` for you.

## What you'll need

- **Docker** and **Docker Compose** installed (`docker --version` and `docker compose version` should both work at the terminal). New to Docker? Read [Docker's overview](https://docs.docker.com/get-started/docker-overview/) first; it covers what containers, images, and volumes are, which the rest of this page assumes you know.
- **A torrent or usenet client** running somewhere your machine can reach. Doesn't have to be on the same host. Supported: qBittorrent, Deluge, Transmission, rTorrent, SABnzbd.

You don't need a Prowlarr or AniList account for this walkthrough; the built-in Nyaa search works without either.

## 1. Run Ryokan

Save this as `docker-compose.yml` somewhere; your home directory is fine for now:

```yaml
services:
  ryokan:
    image: ghcr.io/johnthreekay/ryokan:latest
    container_name: ryokan
    ports:
      - "8978:8978"
    volumes:
      - ryokan-data:/data
    environment:
      - PUID=1000
      - PGID=1000
      - TZ=Etc/UTC
    restart: unless-stopped

volumes:
  ryokan-data:
```

Then:

```sh
docker compose up -d
```

That's it. Ryokan is now running on port 8978.

!!! note "What about my media folder?"
    The example above doesn't mount your library yet. That's deliberate; we want to confirm Ryokan boots before adding more moving parts. We'll come back to volumes once you decide where your library lives.

## 2. First login

Open <http://localhost:8978> in a browser. You'll be redirected to a setup page; pick a username and password and submit. That account is your admin account; Ryokan is single-user, so this is the only one you'll create.

Once you're logged in you'll see an empty library page. That's expected; we haven't told Ryokan about any shows yet.

## 3. Add a download client

Go to **Settings → Download Clients** in the top-right menu, then **Add download client**. Pick the kind that matches what you're running and fill in:

- **URL**: where Ryokan can reach it. If your client is also in Docker on the same machine, this is usually `http://<container-name>:<port>`. If it's on another host on your network, use that host's IP. If both Ryokan and the client are running on Docker Desktop on the same machine, see the per-client notes in the [Download clients](download-clients.md) page.
- **Username and password**: what you'd use to log into the client's own web UI.
- **Category** (torrent clients) or **Category** (SABnzbd): pick something distinctive like `ryokan-anime`. Ryokan only sees its own grabs, so this name keeps things scoped.
- **Default for this protocol**: turn this on for your first client. Without a default, Ryokan won't know where to send grabs.

Click **Test connection**. You should see "Connected" with a version number. If you see an error, the [Download clients page](download-clients.md) has per-client notes.

Save the row. You're done with this step.

## 4. (Optional) Add an indexer

You can skip this for now; Nyaa is built in and works out of the box. But if you have a Prowlarr or Jackett set up with private trackers, this is the moment to wire those in.

**Settings → Indexers → Add indexer**. Paste the URL Prowlarr or Jackett gave you (it ends in `/api`), the API key, and pick a name. The defaults handle the rest.

Click **Test connection** to confirm Ryokan can reach it.

## 5. Add a show

Go back to the library page, click **Add series**, type the name of an anime you want, and pick the right one from the dropdown. Ryokan fetches metadata from AniList by default.

When the series page opens, click an episode you want, then **Search**. You'll see a list of releases ranked by Ryokan's scoring. Pick one and click **Grab**, or hit **Auto-search** to let Ryokan pick the highest-scored release for you.

The grab fires off to your download client. Once it finishes downloading, post-processing will move (or hardlink) the file into the library, but only if you've set a media root.

## 6. (Optional) Set the media root

If you want imports to actually land somewhere your media server can see, you need two things:

1. **A volume mount** in your `docker-compose.yml` pointing at your media folder. Edit the compose file:

    ```yaml
    volumes:
      - ryokan-data:/data
      - /srv/media/anime:/media/anime         # <--- add this; left side = host path, right side = inside-container path
      - /srv/downloads:/downloads             # <--- add this if your download client also writes here
    ```

    Then `docker compose up -d` again.

2. **The same path inside Settings → General → Media Root Path**. Set it to `/media/anime` (the right side of the colon, not the host path).

Now grabs that finish will be hardlinked or copied into `/srv/media/anime/<show name>/Season 01/`.

!!! warning "PUID and PGID matter once you mount real folders"
    The `1000:1000` defaults in the compose file work for most homelabs but not all. If files Ryokan writes show up with the wrong owner, run `id -u` and `id -g` on your media-owning user and update `PUID` / `PGID` to match. [Installation → PUID and PGID](install.md#puid-and-pgid) explains why.

## 7. (Optional) Link AniList or MAL

If you want Ryokan to add new shows automatically when you mark them watching on AniList or MAL, the **[External accounts](external-accounts.md)** page walks through linking. You can do this any time; your existing manually-added series stay put.

## What next?

- **[Configuration](configuration.md)** explains every Settings tab so you can tune scoring, choose between hardlink and copy, set up a quality profile, and so on.
- **[Stack builder](stack-builder.md)** is worth a look even if you're already up and running; it generates the rest of the homelab stack (Jellyfin, Seerr, reverse proxy) in the same shape.
- **[Troubleshooting](troubleshooting.md)** has the most common stumbles and their fixes.

---

*Last updated: 2026-05-07.*
