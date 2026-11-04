#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────────────────────
# Build the fscache Docker image.
#
# Usage:
#   ./docker/build.sh                        — build for this machine, tag as :dev
#   ./docker/build.sh --tag v0.3.0           — build for this machine, tag as :v0.3.0
#   ./docker/build.sh --tag latest --push    — build amd64 + arm64, tag as :latest, and push
#   ./docker/build.sh --platform linux/arm64 — build for a specific platform
#
# Local builds are loaded into the local image store. --push builds every
# platform in --platform (default: linux/amd64,linux/arm64) and pushes them as
# one multi-arch image. Multi-platform builds need a buildx builder that
# supports them (one-time: `docker buildx create --use`) and, on Linux hosts,
# QEMU emulation for foreign architectures
# (`docker run --privileged --rm tonistiigi/binfmt --install all`).
# Docker Desktop ships with emulation already set up.
#
# Run from the repo root.
# ──────────────────────────────────────────────────────────────────────────
set -euo pipefail

IMAGE="dudecmonman/fscache"
TAG="dev"
PUSH=false
PLATFORM=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --tag)      TAG="$2"; shift 2 ;;
        --push)     PUSH=true; shift ;;
        --platform) PLATFORM="$2"; shift 2 ;;
        *) echo "Unknown argument: $1" >&2; exit 1 ;;
    esac
done

FULL_TAG="${IMAGE}:${TAG}"

# .git is excluded from the build context, so resolve the commit here and pass
# it to build.rs for `fscache --version`.
GIT_HASH="$(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
if [[ "$GIT_HASH" != unknown ]] && ! git diff --quiet HEAD 2>/dev/null; then
    GIT_HASH="${GIT_HASH}-dirty"
fi

ARGS=(-f docker/Dockerfile -t "$FULL_TAG" --build-arg "FSCACHE_GIT_HASH=${GIT_HASH}")

if [[ "$PUSH" == true ]]; then
    # Push straight from buildx: multi-platform images can't be loaded into
    # the classic local image store, so a separate `docker push` wouldn't work.
    ARGS+=(--platform "${PLATFORM:-linux/amd64,linux/arm64}" --push)
else
    [[ -n "$PLATFORM" ]] && ARGS+=(--platform "$PLATFORM")
    ARGS+=(--load)
fi

echo "Building ${FULL_TAG} (commit ${GIT_HASH})..."
docker buildx build "${ARGS[@]}" .
echo ""
if [[ "$PUSH" == true ]]; then
    echo "Built and pushed: ${FULL_TAG}"
else
    echo "Build succeeded: ${FULL_TAG}"
fi
