# Build stage
FROM rust:1.99.1-trixie@sha256:931c2c0531a8b143e044f0f3914dfe802f15bb3d1955f0b657e9359327ac2ffc AS builder

ARG APP_VERSION
ARG CARGO_BUILD_JOBS=2
WORKDIR /app

RUN apt-get -o APT::Update::Error-Mode=any update \
    && apt-get install -y --no-install-recommends cmake \
    && rm -rf /var/lib/apt/lists/*

# Cache dependency builds: copy manifests first, build deps, then copy source.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs
RUN test "$(cargo pkgid --locked | sed 's/.*@//')" = "$APP_VERSION" \
    && cargo build --release --locked

# Now copy the real source and build.
# static/ is needed at compile time: src/handlers/settings.rs uses
# include_str!("../../static/default_custom_formats.json") to embed the
# bundled CF defaults into the binary.
COPY src/ src/
COPY templates/ templates/
COPY static/ static/
RUN touch src/main.rs && cargo build --release --locked

# Supply ffprobe as a pinned, multi-architecture static binary instead of
# installing Debian's full ffmpeg dependency tree in the runtime image.
FROM mwader/static-ffmpeg:9.0.2-1@sha256:f5d100184884eb3e82b0eb14bb5ebf98da99826ac824989720297fb9ef21c822 AS ffmpeg

# The official gosu 1.19 release embeds Go 1.24.6. Rebuild that exact
# upstream commit with the security-patched compiler, without changing its CLI.
FROM golang:1.27.3-alpine3.24@sha256:b8c9b82affbbb82a020bbcf998dad2ba9516944ac7e883524642a481a9505e9c AS gosu-builder
ENV CGO_ENABLED=0 GOTOOLCHAIN=local GOBIN=/out
RUN go install -trimpath -ldflags='-s -w' \
      github.com/tianon/gosu@v0.0.0-20250923190938-6456aaa0f3c8 \
    && /out/gosu --version | grep -F '1.19 (go1.27.3 on ' \
    && /out/gosu nobody true \
    && mkdir -p /out/licenses \
    && cp /usr/local/go/LICENSE /out/licenses/Go-LICENSE \
    && cp /go/pkg/mod/github.com/tianon/gosu@v0.0.0-20250923190938-6456aaa0f3c8/LICENSE /out/licenses/gosu-LICENSE \
    && cp /go/pkg/mod/github.com/moby/sys/user@v0.1.0/LICENSE /out/licenses/moby-sys-user-LICENSE \
    && cp /go/pkg/mod/golang.org/x/sys@v0.1.0/LICENSE /out/licenses/golang-x-sys-LICENSE

# Runtime stage
# Ubuntu LTS supplies maintained GNU runtime libraries for the Trixie-built binary.
FROM ubuntu:26.04@sha256:c098726dd9b9733f3e885f8de0ef375206a311b0a317eadc40d347b1115fb944 AS runtime-base

# This image execs Ryokan through its own entrypoint, without Ubuntu's optional
# Pebble service supervisor. Remove that unused, unowned binary while retaining
# the complete distribution package database and failing if ownership changes.
RUN test -s /var/lib/dpkg/status \
    && ! dpkg-query -S /usr/bin/pebble 2>/dev/null \
    && rm -f /usr/bin/pebble

# ca-certificates: outbound HTTPS to AniList / Jikan / Kitsu / Nyaa.
# curl:            used by the compose healthcheck.
# gosu:            copied from the pinned source build above; drops privileges.
# passwd:          provides useradd/groupadd/usermod/groupmod for the entrypoint.
# libstdc++6:      supports the native C++ anitomy title parser.
RUN export DEBIAN_FRONTEND=noninteractive \
    && apt-get -o APT::Update::Error-Mode=any update \
    && apt-get dist-upgrade -y --no-install-recommends \
    && apt-get install -y --no-install-recommends \
    ca-certificates \
    curl \
    passwd \
    libstdc++6 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=gosu-builder /out/gosu /usr/local/bin/gosu
COPY --from=gosu-builder /out/licenses/ /usr/share/licenses/gosu/

FROM runtime-base AS runtime
ARG APP_VERSION
ARG REVISION=unknown
LABEL org.opencontainers.image.source="https://github.com/abhi1693/Ryokan" \
    org.opencontainers.image.version="${APP_VERSION}" \
    org.opencontainers.image.revision="${REVISION}"

WORKDIR /app

COPY --from=builder /app/target/release/ryokan /app/ryokan
COPY --from=ffmpeg /ffprobe /usr/local/bin/ffprobe
COPY static/ /app/static/
COPY LICENSE /app/LICENSE
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
# The entrypoint supplies this default so Docker's build checks do not
# misclassify the path as secret image metadata.
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
