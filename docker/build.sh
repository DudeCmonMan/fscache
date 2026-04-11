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

echo "Building ${FULL_TAG}..."
docker buildx build -f docker/Dockerfile -t "$FULL_TAG" .
echo ""
echo "Build succeeded: ${FULL_TAG}"

if [[ "$PUSH" == true ]]; then
    echo ""
    echo "Pushing ${FULL_TAG}..."
    docker push "$FULL_TAG"
    echo "Pushed: ${FULL_TAG}"
fi
