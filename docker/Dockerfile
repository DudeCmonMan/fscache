# syntax=docker/dockerfile:1
# ──────────────────────────────────────────────────────────────────────────
# fscache Docker image
#
# Multi-stage build: compile from source, then package into a slim runtime
# image with only the binary, fuse3, and a default config.
#
# Build:   ./docker/build.sh            (see the script for --tag/--push)
#    or:   docker buildx build -f docker/Dockerfile \
#            --build-arg FSCACHE_GIT_HASH="$(git rev-parse --short HEAD)" \
#            -t dudecmonman/fscache .
# Run:     see docker/README.md for the compose snippet
# ──────────────────────────────────────────────────────────────────────────

# --- Stage 1: build ---
FROM rust:1-bookworm AS build
WORKDIR /src

# .git is excluded from the build context, so build.rs can't ask git for the
# commit hash. Pass it in instead; it ends up in `fscache --version`.
ARG FSCACHE_GIT_HASH=unknown
ENV FSCACHE_GIT_HASH=${FSCACHE_GIT_HASH}

# Set automatically by BuildKit; used to keep per-architecture build caches
# apart during multi-platform builds.
ARG TARGETPLATFORM

COPY Cargo.toml Cargo.lock build.rs ./
COPY src/ src/

# Cache mounts keep the cargo registry and compiled dependencies between
# builds, so a source change only recompiles fscache itself instead of every
# crate. target/ lives in the cache mount, so the binary is copied out in the
# same step.
RUN --mount=type=cache,id=cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=fscache-target-${TARGETPLATFORM},target=/src/target,sharing=locked \
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
