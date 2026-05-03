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

## This project's being actively developed. Expect some occasional bugs. See [Releases](https://github.com/johnthreekay/Ryokan/releases) for version-to-version changes.

## License

Ryokan is licensed under [GPL-3.0-or-later](LICENSE).

Compiled into the binary are ~350 third-party crates under permissive licenses (MIT, Apache-2.0, BSD-3-Clause, ISC, MPL-2.0, Unicode-3.0, Zlib, CDLA-Permissive-2.0). Their copyright and permission notices are bundled in [`THIRD_PARTY_LICENSES.html`](THIRD_PARTY_LICENSES.html) at the repo root, also baked into the Docker image at `/app/THIRD_PARTY_LICENSES.html`.

Regenerate after any `Cargo.lock` change:

```sh
cargo install cargo-about --locked --features cli   # one-time
cargo about generate -c about.toml about.hbs -o THIRD_PARTY_LICENSES.html
```

`about.toml` lists the SPDX identifiers we accept; if a new dep introduces a new identifier, `cargo about generate` fails loud and you decide whether to add it to the accepted list or swap the dep.
