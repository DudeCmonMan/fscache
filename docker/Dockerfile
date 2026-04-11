# ──────────────────────────────────────────────────────────────────────────
# fscache Docker image
#
# Multi-stage build: compile from source, then package into a slim runtime
# image with only the binary, fuse3, and a default config.
#
# Build:   docker buildx build -f docker/Dockerfile -t dudecmonman/fscache .
# Run:     see docker/README.md for the compose snippet
# ──────────────────────────────────────────────────────────────────────────

# --- Stage 1: build ---
FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock build.rs ./
COPY src/ src/
RUN cargo build --release --bin fscache

# --- Stage 2: runtime ---
FROM debian:bookworm-slim

RUN apt-get update \
 && apt-get install -y --no-install-recommends fuse3 gettext-base \
 && rm -rf /var/lib/apt/lists/*

# Ensure state and log directories exist inside the image.
RUN mkdir -p /var/lib/fscache/db /var/log/fscache /run/fscache /etc/fscache

COPY --from=build /src/target/release/fscache /usr/local/bin/fscache
COPY docker/config.template.toml /etc/fscache/config.template.toml
COPY docker/entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh

ENTRYPOINT ["/entrypoint.sh"]
