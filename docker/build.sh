#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────────────────────
# Build the fscache Docker image.
#
# Usage:
#   ./docker/build.sh                        — build and tag as :dev
#   ./docker/build.sh --tag v0.3.0           — build and tag as :v0.3.0
#   ./docker/build.sh --tag latest --push    — build, tag as :latest, and push
#
# Run from the repo root.
# ──────────────────────────────────────────────────────────────────────────
set -euo pipefail

IMAGE="dudecmonman/fscache"
TAG="dev"
PUSH=false

while [[ $# -gt 0 ]]; do
    case "$1" in
        --tag)  TAG="$2"; shift 2 ;;
        --push) PUSH=true; shift ;;
        *) echo "Unknown argument: $1" >&2; exit 1 ;;
    esac
done

FULL_TAG="${IMAGE}:${TAG}"

# .git isn't in the build context, so pass the commit hash in for `fscache --version`.
GIT_HASH="$(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
if [[ "$GIT_HASH" != unknown ]] && ! git diff --quiet HEAD 2>/dev/null; then
    GIT_HASH="${GIT_HASH}-dirty"
fi

ARGS=(-f docker/Dockerfile -t "$FULL_TAG" --platform linux/amd64 --build-arg "FSCACHE_GIT_HASH=${GIT_HASH}")

# Push from buildx directly; a separate `docker push` fails with non-default builders.
if [[ "$PUSH" == true ]]; then
    ARGS+=(--push)
else
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
