# Install

You have two options for installing Ryokan: pulling the Docker image or building from source.

## Docker (Recommended)

The Docker image is published to GHCR on every tag at `ghcr.io/johnthreekay/ryokan:latest`. The repo also provides a `docker-compose.yml` you can copy verbatim:

```sh
curl -O https://raw.githubusercontent.com/johnthreekay/Ryokan/main/docker-compose.yml
docker compose up -d
```

Open `http://localhost:8978` and run through the first-run setup. See the [Docker page](docker.md) for volume mounts, PUID/PGID, and the full environment-variable reference.

## From source

You'll need:

- **Rust 1.95+** (enforced via `Cargo.toml`'s `package.rust-version`).
- **C / C++ toolchain + `cmake`**: two crates compile native code at build time. `anitomy-sys` ships C++ source it builds via `cc` for anime title tokenization, and `aws-lc-sys` builds aws-lc via cmake (rustls' crypto provider since reqwest 0.13).
- **`mold` + `clang`**: Ryokan pins `linker = "clang"` with `-fuse-ld=mold` for x86_64 + aarch64 Linux in `.cargo/config.toml`. Cuts incremental link time 3-5× vs ld/lld. Without them: `linker 'clang' not found` or `ld.mold not found` at first build.
- **`cargo-nextest`** for the canonical `cargo t` test alias. Falls through to plain `cargo test` if not installed.

Install the toolchain bits on Arch:

```sh
sudo pacman -S mold clang cmake
cargo install cargo-nextest --locked
```

Or Debian/Ubuntu:

```sh
sudo apt install mold clang cmake
cargo install cargo-nextest --locked
```

Then:

```sh
git clone https://github.com/johnthreekay/Ryokan.git
cd Ryokan
cargo run                # 0.0.0.0:8978, creates data/ryokan.db on first run
```

The first build takes a while (aws-lc, anitomy C++, full dep tree). Subsequent rebuilds are fast; mold links the `ryokan` binary in under a second.

## Healthcheck and ports

Ryokan listens on `0.0.0.0:8978` by default (override with `LISTEN_ADDR`). The Docker image's healthcheck probes `GET /login` since that's the canonical "is Ryokan up" endpoint. There is no `/healthz`; `/login` returns 200 once the auth UI is live, or 303 redirecting to `/setup` on a fresh container with no users yet (both are valid "up" signals).

## First-run setup

The first time Ryokan boots, navigating to `/` redirects to `/setup`. Create an admin account there. Once a user has been created, `/setup` redirects to `/login` and won't accept further submissions.

If you need to reset the admin account (forgot password, username, etc.), there's a deliberate two-step gate to avoid an accidental wipe-on-restart: see the `RYOKAN_RESET_AUTH` env var in the [Docker page](docker.md#environment-variables).
