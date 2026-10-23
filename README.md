# Ryokan

[![CI](https://github.com/johnthreekay/Ryokan/actions/workflows/rust.yml/badge.svg)](https://github.com/johnthreekay/Ryokan/actions/workflows/rust.yml)
[![License: GPL v3](https://img.shields.io/badge/License-GPLv3-blue.svg)](LICENSE)

A self-hosted anime PVR written in Rust. Searches indexers for releases, scores them by quality, and sends them to your download client from a single web UI. Supports qBittorrent, Deluge, Transmission, rTorrent/ruTorrent, and SABnzbd.

I built this because Sonarr doesn't always work well for anime. The RSS sync for currently airing shows works just fine, but downloading season batches of shows that've finished airing almost always hangs the interactive search. Sonarr searches Nyaa using `SXEXX`-style episode identifiers, which don't match how most anime torrents are named.

Ryokan validates the complete wanted-video map before importing a batch. It
also infers batch shape from the actual file list when an indexer labels a
multi-episode release as a single episode, and excludes videos beneath
secondary-media paths such as `Extras/`, `Samples/`, and `Trailers/` from the
episode import plan.

The published container includes `ffprobe` so post-download stream metadata
and measured resolution participate in classification without extra setup.

Interrupted imports can adopt an identical destination created before durable
checkpoints were available. Set `RYOKAN_POST_PROCESSING_VERIFY_CONCURRENCY` to
run `1` to `8` read-only comparison workers per file; the default is `1` and
all library and database mutations remain serialized.

Large batches can yield between files so other completed grabs do not wait for
the whole batch. Set `RYOKAN_POST_PROCESSING_MAX_FILES_PER_GRAB=1` for one file
per grab in each pass; durable checkpoints let the next pass continue without
repeating completed file operations.

Filename parsing recognizes zero-padded trailing episode tokens such as
`Group_Title_01.mkv` while rejecting years, resolutions, and one-digit title
suffixes.

## Documentation

- [Getting Started](https://johnthreekay.github.io/Ryokan/docs/#get-started): install via Docker, configuration, FAQ.
- [Build from source](https://johnthreekay.github.io/Ryokan/docs/from-source/): for development or non-Docker deployments.

## Screenshots

<img width="1920" height="1079" alt="image" src="https://github.com/user-attachments/assets/24d59ff2-0f12-4788-b06f-d7ba7ce57812" />
<img width="1920" height="1080" alt="image" src="https://github.com/user-attachments/assets/72db83dd-0252-43c9-a5e6-7fb43a15e271" />
<img width="1920" height="1080" alt="image" src="https://github.com/user-attachments/assets/018ecb01-b434-4b3b-93d6-1cad3678bcc5" />
<img width="1920" height="1080" alt="image" src="https://github.com/user-attachments/assets/db443139-e72b-4cca-b220-feeb7b348ee6" />

---

## Status

Actively developed. Expect occasional bugs. See [Releases](https://github.com/johnthreekay/Ryokan/releases) for version-to-version changes.

## Contributing

Bug reports, feature requests, and PRs are welcome. PRs target the `dev` branch and run a verify chain (`cargo fmt`/`clippy -D warnings`/`cargo t`). See [`CLAUDE.md`](CLAUDE.md) for the build prerequisites (`mold` + `clang`, `cmake`, `cargo-nextest`) and the code conventions.

Both Rust CI jobs and weekly mutation testing install `mold` and `clang` using only the runner's official Ubuntu
`ubuntu.sources` repositories. These commands preserve normal package signature verification and
fail if an Ubuntu index fails, while leaving unrelated preconfigured package feeds unchanged.

To run the application locally:

```bash
git clone https://github.com/johnthreekay/Ryokan.git
cd Ryokan
cargo run            # serves on 0.0.0.0:8978, creates data/ryokan.db
```

## Security

Please report security issues privately. See [SECURITY.md](SECURITY.md).

## License

Ryokan is licensed under [GPL-3.0-or-later](LICENSE).

Third-party crate notices (MIT / Apache-2.0 / BSD / ISC) are bundled in [`licenses/THIRD_PARTY_LICENSES.html`](licenses/THIRD_PARTY_LICENSES.html). Dependency changes targeting `main` must carry matching notices; see the [notice workflow and membership check](licenses/README.md).

## Next patch release

Version 1.9.13 refreshes all Rust dependencies, HTMX 2.0.11, Zensical 0.0.71,
and ffprobe 9.0.2-1. The Rust 1.99.1 builder, Ubuntu 26.04 LTS runtime,
and static ffprobe sources use verified multi-platform digests. The image
rebuilds the exact upstream gosu 1.19 source with Go 1.27.3 and explicitly
includes the C++ runtime needed by the native title parser. The declared
minimum remains Rust 1.95.
The locked graph includes ctutils 0.4.4 for the upstream constant-time mask and
comparison fixes in [RUSTSEC-2026-0385](https://github.com/RustSec/advisory-db/blob/main/crates/ctutils/RUSTSEC-2026-0385.md).
See [CHANGELOG.md](CHANGELOG.md) and the browser dependency
[provenance](static/vendor/README.md). Run the canonical `cargo t` / nextest
suite with `test-support` when checking dependency upgrades so test fixtures run
in separate processes.

The Rust pull-request workflow audits locked dependencies and requires native
AMD64 and ARM64 image builds, SBOMs, strict vulnerability/secret scans (including
unfixed HIGH/CRITICAL vulnerabilities), and the real runtime smoke test before
release. Tag/manual publication reuses these gates and checks Cargo source,
lockfile, image and release versions. Branch pushes verify without publishing.
Publication requires the exact `vVERSION` Git tag, including manual dispatch.
The full tag ref, resolved tag commit, checked-out HEAD and workflow source SHA
must agree before CI or image builds; moving branch dispatches fail closed.
See the [image verification instructions](docs/docker.md#building-and-verifying-this-fork).
