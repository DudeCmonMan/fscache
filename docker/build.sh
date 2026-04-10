#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────────────────────
# Build the fscache Docker image.
#
# Usage:
#   ./docker/build.sh            — build as :exp (experimental, not for release)
#   ./docker/build.sh --tag      — build as :exp, then retag and push :latest
#                                  and :<version> as an official release
#
# Run from the repo root.
# ──────────────────────────────────────────────────────────────────────────
set -euo pipefail

IMAGE="dudecmonman/fscache"
EXP_TAG="${IMAGE}:exp"
VERSION=$(grep '^version' Cargo.toml | head -1 | grep -oP '[\d]+\.[\d]+\.[\d]+')
TAG=false

for arg in "$@"; do
    [[ "$arg" == "--tag" ]] && TAG=true
done

# ── Build ─────────────────────────────────────────────────────────────────

echo "Building ${EXP_TAG} from docker/Dockerfile..."
docker buildx build -f docker/Dockerfile -t "$EXP_TAG" .
echo ""
echo "Build succeeded: ${EXP_TAG}"

# ── Tag and push official release ─────────────────────────────────────────

if [[ "$TAG" == true ]]; then
    echo ""
    echo "Tagging and pushing official release..."
    echo "  ${IMAGE}:${VERSION}"
    echo "  ${IMAGE}:latest"

    docker buildx build -f docker/Dockerfile -t "${IMAGE}:${VERSION}" -t "${IMAGE}:latest" .

    docker push "${IMAGE}:${VERSION}"
    docker push "${IMAGE}:latest"

    echo ""
    echo "Published:"
    echo "  docker pull ${IMAGE}:${VERSION}"
    echo "  docker pull ${IMAGE}:latest"
else
    echo ""
    echo "This is an experimental build — not for release."
    echo "To publish an official release, run with --tag:"
    echo "  ./docker/build.sh --tag"
fi
