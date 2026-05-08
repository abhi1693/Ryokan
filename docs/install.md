# Docker installation

Ryokan ships as a Docker image at `ghcr.io/johnthreekay/ryokan:latest`. The image runs on `linux/amd64` and `linux/arm64`. The fastest way to get going is `docker compose up`.

If you're new to Docker, skim [Docker's getting-started guide](https://docs.docker.com/get-started/) first; this page assumes you know what `docker compose up -d` does.

## Quick install

Save this as `docker-compose.yml`:

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

Open <http://localhost:8978> and create your admin account on the setup page. You're done.

## Recommended install (with media folders)

The Quick install doesn't mount your media library or your download client's complete folder yet. To make post-processing actually move files into your library, you need two more volume mounts:

```yaml
services:
  ryokan:
    image: ghcr.io/johnthreekay/ryokan:latest
    container_name: ryokan
    ports:
      - "8978:8978"
    volumes:
      - ryokan-data:/data
      - /srv/downloads:/downloads          # where your download client puts completed files
      - /srv/media/anime:/media/anime      # where Ryokan should land imported episodes
    environment:
      - PUID=1000
      - PGID=1000
      - TZ=America/Chicago
    restart: unless-stopped

volumes:
  ryokan-data:
```

The colon in volume mounts means: left side is your host filesystem path, right side is the path inside the container. Ryokan only sees the right side. So:

- Inside the container, "complete downloads" live at `/downloads`. Configure your download client to put its completed files here.
- Inside the container, "the library" lives at `/media/anime`. Set Settings → General → Media Root Path to `/media/anime`.

These paths must match what your download client sees. If qBittorrent thinks files are at `/downloads/movies/foo.mkv` but Ryokan thinks they're at `/srv/downloads/foo.mkv`, post-processing fails. The [Stack builder](stack-builder.md) generates compose files with matching paths automatically.

## PUID and PGID

```yaml
environment:
  - PUID=1000
  - PGID=1000
```

These set the user/group ID Ryokan runs as inside the container. Set them to match the user that owns your media files on the host.

To find the right values: run `id -u` and `id -g` on the host as the user who owns `/srv/media/anime` (or wherever your library lives). Most homelabs run everything as a single non-root user with ID 1000, in which case the defaults are fine.

This is the [linuxserver.io convention](https://docs.linuxserver.io/general/understanding-puid-and-pgid/), shared with the rest of the *arr stack. Setting them right means files Ryokan writes match the rest of your media's ownership.

!!! warning "User-mounted paths are NOT chowned"
    The container chowns `/data` to the supplied PUID/PGID, but does not chown `/downloads` or `/media/...`. Chowning a 10TB media library would stall startup for hours and could clobber ownership the rest of your *arr stack relies on. Pick PUID/PGID that already match your media's owner instead.

## First-run setup

The first time Ryokan boots, navigating to `/` redirects to `/setup`. Create an admin account there. Once an account exists, `/setup` redirects to `/login` and won't accept further submissions.

If you need to reset the admin account later (forgot password, etc.), there's a deliberate two-step gate to avoid an accidental wipe-on-restart. See [Docker reference → Reset auth](docker.md#reset-auth).

## Updating

```sh
docker compose pull
docker compose up -d
```

The named volume preserves your data. Migrations run automatically on next boot and are idempotent (applying twice is a no-op). For more on migrations and `docker compose down -v` safety, see [Docker reference → Updating](docker.md#updating).

## Build from source

You don't need this if you're running the Docker image. This is for contributing to Ryokan or running pre-release commits.

You'll need:

- **Rust 1.95 or later** (enforced via `Cargo.toml`'s `package.rust-version`).
- **A C/C++ toolchain plus `cmake`**. Two crates compile native code at build time: `anitomy-sys` ships C++ source it builds via `cc` for anime title tokenization, and `aws-lc-sys` builds aws-lc via cmake (rustls' crypto provider since reqwest 0.13).
- **`mold` and `clang`**. Ryokan pins `linker = "clang"` with `-fuse-ld=mold` for x86_64 and aarch64 Linux. Cuts incremental link time 3–5× vs ld/lld. Without them you'll get `linker 'clang' not found` or `ld.mold not found` at first build.
- **`cargo-nextest`** for the canonical `cargo t` test alias. Falls through to plain `cargo test` if not installed.

On Arch:

```sh
sudo pacman -S mold clang cmake
cargo install cargo-nextest --locked
```

On Debian/Ubuntu:

```sh
sudo apt install mold clang cmake
cargo install cargo-nextest --locked
```

Then:

```sh
git clone https://github.com/johnthreekay/Ryokan.git
cd Ryokan
cargo run                # http://localhost:8978; creates data/ryokan.db on first run
```

The first build takes a while (aws-lc, anitomy C++, full dep tree). Subsequent rebuilds are fast; mold links the binary in under a second.

## Where next

- **[Configuration](configuration.md)**: the Settings tabs.
- **[Download clients](download-clients.md)**: per-client setup notes.
- **[External accounts](external-accounts.md)**: link AniList or MAL.
- **[Docker reference](docker.md)**: full environment variable table, healthcheck shape, advanced topics.

---

*Last updated: 2026-05-07.*
