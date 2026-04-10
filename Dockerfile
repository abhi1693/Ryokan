# Build stage
FROM rust:1.94-bookworm AS builder

WORKDIR /app

# Cache dependency builds: copy manifests first, build deps, then copy source.
COPY Cargo.toml Cargo.lock* ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs
RUN cargo build --release 2>/dev/null || true

# Now copy the real source and build.
COPY src/ src/
COPY templates/ templates/
RUN touch src/main.rs && cargo build --release

# Runtime stage
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=builder /app/target/release/ryokan /app/ryokan
COPY templates/ /app/templates/
COPY static/ /app/static/

RUN mkdir -p /data

ENV DATABASE_URL=sqlite:///data/ryokan.db?mode=rwc
ENV LISTEN_ADDR=0.0.0.0:8978
ENV RUST_LOG=ryokan=info

EXPOSE 8978

CMD ["/app/ryokan"]
