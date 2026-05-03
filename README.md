# Ryokan

A self-hosted anime PVR written in Rust. Searches indexers for releases, scores them by quality, and sends them to your download client from a single web UI. Supports qBittorrent, Deluge, Transmission, rTorrent/ruTorrent, and SABnzbd.

I built this because Sonarr doesn't always work well for anime. The RSS sync for currently airing shows works just fine, but downloading season batches of shows that've finished airing almost always hangs the interactive search. Sonarr searches Nyaa using `SXEXX`-style episode identifiers, which don't match how most anime torrents are named.

## Documentation 
- [Getting Started](https://johnthreekay.github.io/Ryokan/docs/#get-started)

## Screenshots

<img width="1920" height="1079" alt="image" src="https://github.com/user-attachments/assets/24d59ff2-0f12-4788-b06f-d7ba7ce57812" />
<img width="1920" height="1080" alt="image" src="https://github.com/user-attachments/assets/72db83dd-0252-43c9-a5e6-7fb43a15e271" />
<img width="1920" height="1080" alt="image" src="https://github.com/user-attachments/assets/018ecb01-b434-4b3b-93d6-1cad3678bcc5" />
<img width="1920" height="1080" alt="image" src="https://github.com/user-attachments/assets/db443139-e72b-4cca-b220-feeb7b348ee6" />

This project's being actively developed. Expect some occasional bugs. See [Releases](https://github.com/johnthreekay/Ryokan/releases) for version-to-version changes.
