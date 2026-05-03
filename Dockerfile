# Build stage
FROM rust:1-trixie AS builder

WORKDIR /app

# Cache dependency builds: copy manifests first, build deps, then copy source.
COPY Cargo.toml Cargo.lock* ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs
RUN cargo build --release

# Now copy the real source and build.
# static/ is needed at compile time: src/handlers/settings.rs uses
# include_str!("../../static/default_custom_formats.json") to embed the
# bundled CF defaults into the binary.
COPY src/ src/
COPY templates/ templates/
COPY static/ static/
RUN touch src/main.rs && cargo build --release

# Runtime stage
FROM debian:trixie-slim

# ca-certificates: outbound HTTPS to AniList / Jikan / Kitsu / Nyaa.
# curl:            used by the compose healthcheck.
# gosu:            drops privileges from root to the ryokan user in the entrypoint.
# passwd:          provides useradd/groupadd/usermod/groupmod for the entrypoint.
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    curl \
    gosu \
    passwd \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=builder /app/target/release/ryokan /app/ryokan
COPY static/ /app/static/
# License notices required by MIT/Apache/BSD/ISC deps that ship inside
# the binary. `cargo about generate` produces this file from Cargo.lock;
# regenerate when deps change. Living at /app/THIRD_PARTY_LICENSES.html
# so receivers of the Docker image can `docker cp` it out.
COPY LICENSE /app/LICENSE
COPY THIRD_PARTY_LICENSES.html /app/THIRD_PARTY_LICENSES.html
COPY docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh
RUN chmod +x /usr/local/bin/docker-entrypoint.sh

RUN mkdir -p /data

ENV DATABASE_URL=sqlite:///data/ryokan.db?mode=rwc
ENV LISTEN_ADDR=0.0.0.0:8978
ENV RUST_LOG=ryokan=info
# Persist the on-disk artwork blob cache alongside the SQLite database so
# image_blobs rows keep matching real files across container restarts.
ENV RYOKAN_MEDIA_CACHE_DIR=/data/cache/artwork
# Encryption-key file lives next to the SQLite DB on the persistent
# /data volume. The default `data/.ryokan-key` is CWD-relative
# (matching `cargo run`'s working tree), but this image's WORKDIR is
# /app and the entrypoint chowns /data not /app/data — so without
# this override the ryokan user can't write the key file at boot
# and `services::crypto` panics with `Permission denied (os error 13)`.
ENV RYOKAN_KEY_FILE_PATH=/data/.ryokan-key
# Anibridge mappings cache — same CWD-relative footgun as the key
# file. Without this override the ~9MB mappings blob silently
# fails to persist on every fetch (write to /app/data/... 13s),
# meaning every container restart re-downloads from the upstream
# GitHub-hosted JSON instead of using the conditional-GET cache.
ENV RYOKAN_ANIBRIDGE_CACHE_DIR=/data/cache/anibridge
# Default UID/GID for the ryokan user. Override via -e PUID=... / PGID=...
# to match the ownership of host-mounted media and download directories.
ENV PUID=1000
ENV PGID=1000

EXPOSE 8978

# start-period covers cold first-boot work before axum::serve binds:
# `models::migrate` (idempotent ALTER TABLEs across the schema),
# `bcrypt::warm_timing_equalizer` spawn_blocking, `rebuild_clients_cache`,
# optional Jellyfin client init. 10s was tight on a cold-data ARM64
# first-run; 30s matches the CI smoke-test poll budget.
HEALTHCHECK --interval=30s --timeout=5s --start-period=30s --retries=3 \
    CMD curl -fsS http://localhost:8978/login || exit 1

ENTRYPOINT ["/usr/local/bin/docker-entrypoint.sh"]
CMD ["/app/ryokan"]
