# Build from source

You don't need this if you're running the Docker image. This page is for contributing to Ryokan or running pre-release commits straight off the `dev` branch.

## Prerequisites

- **Rust 1.95 or later** (enforced via `Cargo.toml`'s `package.rust-version`).
- **A C/C++ toolchain plus `cmake`**. Two crates compile native code at build time: `anitomy-sys` ships C++ source it builds via `cc` for anime title tokenization, and `aws-lc-sys` builds aws-lc via cmake (rustls' crypto provider since reqwest 0.13).
- **`mold` and `clang`**. Ryokan pins `linker = "clang"` with `-fuse-ld=mold` for x86_64 and aarch64 Linux. Cuts incremental link time 3–5× vs ld/lld. Without them you'll get `linker 'clang' not found` or `ld.mold not found` at first build.
- **`cargo-nextest`** for the canonical `cargo t` test alias. Falls through to plain `cargo test` if not installed.

## Install the toolchain

On Arch:

```sh
sudo pacman -S mold clang cmake
cargo install cargo-nextest --locked
```

On Debian or Ubuntu:

```sh
sudo apt install mold clang cmake
cargo install cargo-nextest --locked
```

On macOS, the toolchain story is messier: aws-lc and anitomy-sys both build cleanly under Apple Clang, but mold isn't packaged and you'll fall back to ld64. Builds are slower but functional.

## Clone and run

```sh
git clone https://github.com/johnthreekay/Ryokan.git
cd Ryokan
cargo run                # http://localhost:8978; creates data/ryokan.db on first run
```

The first build takes a while (aws-lc, anitomy C++, full dep tree). Subsequent rebuilds are fast; mold links the binary in under a second.

## Tests and lints

```sh
cargo t                                                                  # canonical test alias (uses nextest)
cargo fmt --all -- --check                                               # CI runs this first
cargo clippy --workspace --all-targets --features test-support -- -D warnings   # CI form
```

## Where next

- **[Configuration](configuration.md)**: the Settings tabs.
- **[Download clients](download-clients.md)**: per-client setup notes.
- **[Docker reference](docker.md)**: environment variable table; useful even when running from source since the `RYOKAN_*` flags work the same way.

---

*Last updated: 2026-05-07.*
