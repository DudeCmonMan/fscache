# syntax=docker/dockerfile:1
# ──────────────────────────────────────────────────────────────────────────
# fscache Docker image
#
# Multi-stage build: compile from source, then package into a slim runtime
# image with only the binary, fuse3, and a default config.
#
# Build:   ./docker/build.sh
# Run:     see docker/README.md for the compose snippet
# ──────────────────────────────────────────────────────────────────────────

# --- Stage 1: build ---
FROM rust:1-bookworm AS build
WORKDIR /src

# .git isn't in the build context, so build.rs reads the commit hash from here.
ARG FSCACHE_GIT_HASH=unknown
ENV FSCACHE_GIT_HASH=${FSCACHE_GIT_HASH}

COPY Cargo.toml Cargo.lock build.rs ./
COPY src/ src/

# Cache the registry and target/ so source changes don't rebuild every dependency.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    cargo build --release --locked --bin fscache \
 && cp target/release/fscache /usr/local/bin/fscache

# --- Stage 2: runtime ---
FROM debian:bookworm-slim

RUN apt-get update \
 && apt-get install -y --no-install-recommends fuse3 gettext-base \
 && rm -rf /var/lib/apt/lists/* \
 && mkdir -p /var/lib/fscache/db /var/log/fscache /run/fscache /etc/fscache

COPY --from=build /usr/local/bin/fscache /usr/local/bin/fscache
COPY docker/config.template.toml /etc/fscache/config.template.toml
COPY --chmod=755 docker/entrypoint.sh /entrypoint.sh

ENTRYPOINT ["/entrypoint.sh"]
