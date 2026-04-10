#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────────────────────
# fscache Docker E2E test
#
# Builds the image from source and verifies:
#   1. Dockerfile + cargo build succeed
#   2. Every hardcoded binary path exists inside the container
#   3. FUSE mount propagates to a consumer via :rslave
#   4. Files read through FUSE are cached to the SSD directory
#   5. Cached file content is byte-identical to the original
#   6. State DB survives a container restart
#
# Usage:  sudo ./tests/docker/run.sh
# Requires: Docker with Compose v2, root (for FUSE / SYS_ADMIN)
# ──────────────────────────────────────────────────────────────────────────
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
COMPOSE_FILE="$SCRIPT_DIR/docker-compose.test.yml"

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; NC='\033[0m'
pass=0; fail=0

log()  { echo -e "${YELLOW}[test]${NC} $*"; }
pass() { echo -e "  ${GREEN}PASS${NC}  $*"; pass=$((pass + 1)); }
fail() { echo -e "  ${RED}FAIL${NC}  $*"; fail=$((fail + 1)); }

check() {
    local desc="$1"; shift
    if "$@" &>/dev/null; then pass "$desc"; else fail "$desc"; fi
}

# ── Preflight ─────────────────────────────────────────────────────────────

[[ $EUID -eq 0 ]] || { echo "Run with sudo." >&2; exit 1; }
command -v docker &>/dev/null || { echo "Docker not found." >&2; exit 1; }
docker compose version &>/dev/null || { echo "Docker Compose v2 required." >&2; exit 1; }

mount --make-rshared / 2>/dev/null || true

# ── Setup ─────────────────────────────────────────────────────────────────

log "Creating temp directories..."
export TEST_MEDIA=$(mktemp -d /tmp/fscache-test-media.XXXXXX)
export TEST_CACHE=$(mktemp -d /tmp/fscache-test-cache.XXXXXX)
export TEST_STATE=$(mktemp -d /tmp/fscache-test-state.XXXXXX)

cleanup() {
    log "Tearing down..."
    docker compose -f "$COMPOSE_FILE" down --volumes --remove-orphans 2>/dev/null || true
    rm -rf "$TEST_MEDIA" "$TEST_CACHE" "$TEST_STATE"
}
trap cleanup EXIT

log "Creating synthetic test media..."
mkdir -p "$TEST_MEDIA/Movies" "$TEST_MEDIA/TV"
dd if=/dev/urandom of="$TEST_MEDIA/Movies/test-movie.bin" bs=1M count=5  2>/dev/null
dd if=/dev/urandom of="$TEST_MEDIA/TV/test-episode.bin"   bs=1M count=5  2>/dev/null
echo "small metadata" > "$TEST_MEDIA/Movies/metadata.nfo"

MOVIE_HASH=$(md5sum "$TEST_MEDIA/Movies/test-movie.bin" | awk '{print $1}')
EPISODE_HASH=$(md5sum "$TEST_MEDIA/TV/test-episode.bin"  | awk '{print $1}')

# ── Phase 1: Build ────────────────────────────────────────────────────────

log "Phase 1: Building Docker image from source (Dockerfile + cargo build)..."

if docker compose -f "$COMPOSE_FILE" build --no-cache fscache 2>&1; then
    pass "Docker image built from source"
else
    fail "Docker image build failed — aborting"
    exit 1
fi

# ── Phase 2: Start ────────────────────────────────────────────────────────

log "Phase 2: Starting test stack..."
docker compose -f "$COMPOSE_FILE" up -d

log "Waiting for fscache healthcheck (FUSE mount up)..."
TIMEOUT=90; ELAPSED=0; STATUS="starting"
while [[ $ELAPSED -lt $TIMEOUT ]]; do
    STATUS=$(docker inspect --format='{{.State.Health.Status}}' fscache-test 2>/dev/null || echo "missing")
    [[ "$STATUS" == "healthy" ]] && break
    sleep 2; ((ELAPSED+=2))
done

if [[ "$STATUS" == "healthy" ]]; then
    pass "fscache container healthy (FUSE mounted)"
else
    fail "fscache did not become healthy within ${TIMEOUT}s (status: $STATUS)"
    docker logs fscache-test 2>&1 | tail -30
    exit 1
fi

# ── Phase 3: Binary path verification ────────────────────────────────────

log "Phase 3: Verifying hardcoded binary paths inside the container..."

# Paths baked into the Dockerfile
check "binary at /usr/local/bin/fscache" \
    docker exec fscache-test test -x /usr/local/bin/fscache

check "config at /etc/fscache/config.toml" \
    docker exec fscache-test test -r /etc/fscache/config.toml

# Paths written by fscache at runtime (src/main.rs:141, src/utils.rs:91, src/ipc/server.rs:187)
check "state DB at /var/lib/fscache/db/fscache.db" \
    docker exec fscache-test test -f /var/lib/fscache/db/fscache.db

check "instance lock at /run/fscache/fscache.lock" \
    docker exec fscache-test test -f /run/fscache/fscache.lock

check "IPC socket at /run/fscache/fscache.sock" \
    docker exec fscache-test test -S /run/fscache/fscache.sock

check "cache directory /cache is writable" \
    docker exec fscache-test test -w /cache

# FUSE mount at /media (src/main.rs MountOption::FSName)
check "FUSE mount visible in /proc/mounts" \
    docker exec fscache-test grep -q fscache /proc/mounts

# ── Phase 4: FUSE propagation ────────────────────────────────────────────

log "Phase 4: Verifying FUSE mount propagates to consumer via :rslave..."

check "consumer sees Movies/test-movie.bin" \
    docker exec fscache-test-consumer test -f /media/Movies/test-movie.bin

check "consumer sees TV/test-episode.bin" \
    docker exec fscache-test-consumer test -f /media/TV/test-episode.bin

# File content must be intact through FUSE
FUSE_MOVIE=$(docker exec fscache-test-consumer md5sum /media/Movies/test-movie.bin | awk '{print $1}')
if [[ "$FUSE_MOVIE" == "$MOVIE_HASH" ]]; then
    pass "Movie file content correct through FUSE (md5: $MOVIE_HASH)"
else
    fail "Movie file content mismatch (orig: $MOVIE_HASH, fuse: $FUSE_MOVIE)"
fi

FUSE_EPISODE=$(docker exec fscache-test-consumer md5sum /media/TV/test-episode.bin | awk '{print $1}')
if [[ "$FUSE_EPISODE" == "$EPISODE_HASH" ]]; then
    pass "Episode file content correct through FUSE (md5: $EPISODE_HASH)"
else
    fail "Episode file content mismatch (orig: $EPISODE_HASH, fuse: $FUSE_EPISODE)"
fi

# ── Phase 5: Cache behavior ──────────────────────────────────────────────

log "Phase 5: Reading files through consumer to trigger caching..."

docker exec fscache-test-consumer cat /media/Movies/test-movie.bin > /dev/null
docker exec fscache-test-consumer cat /media/TV/test-episode.bin   > /dev/null

# Give the cache engine time to write (copier task is async)
log "Waiting for cache writes to settle..."
sleep 5

# With cache-on-miss + min_access_secs=0, both files must be cached — hard fail
CACHED_FILES=$(find "$TEST_CACHE" -type f \
    ! -name "*.db" ! -name "*.db-wal" ! -name "*.db-shm" \
    ! -name "*.lock" ! -name "*.sock" \
    2>/dev/null | wc -l)

if [[ "$CACHED_FILES" -gt 0 ]]; then
    pass "Cache directory has $CACHED_FILES cached file(s)"
else
    fail "No cached files in $TEST_CACHE — cache-on-miss preset should have written files"
    docker logs fscache-test 2>&1 | tail -30
fi

# Verify a cached file is byte-identical to the original (full round-trip integrity)
CACHED_MOVIE=$(find "$TEST_CACHE" -type f -name "test-movie.bin" 2>/dev/null | head -1)
if [[ -n "$CACHED_MOVIE" ]]; then
    CACHED_HASH=$(md5sum "$CACHED_MOVIE" | awk '{print $1}')
    if [[ "$CACHED_HASH" == "$MOVIE_HASH" ]]; then
        pass "Cached file is byte-identical to original (md5: $MOVIE_HASH)"
    else
        fail "Cached file content mismatch (orig: $MOVIE_HASH, cached: $CACHED_HASH)"
    fi
else
    fail "test-movie.bin not found in cache — cannot verify integrity"
fi

# ── Phase 6: Persistence across restart ──────────────────────────────────

log "Phase 6: Testing state DB persistence across container restart..."

DB_SIZE_BEFORE=$(stat -c%s "$TEST_STATE/db/fscache.db" 2>/dev/null || echo "0")

docker compose -f "$COMPOSE_FILE" restart fscache

ELAPSED=0
while [[ $ELAPSED -lt $TIMEOUT ]]; do
    STATUS=$(docker inspect --format='{{.State.Health.Status}}' fscache-test 2>/dev/null || echo "missing")
    [[ "$STATUS" == "healthy" ]] && break
    sleep 2; ((ELAPSED+=2))
done

if [[ "$STATUS" == "healthy" ]]; then
    pass "fscache recovered after restart"
else
    fail "fscache did not recover after restart (status: $STATUS)"
fi

DB_SIZE_AFTER=$(stat -c%s "$TEST_STATE/db/fscache.db" 2>/dev/null || echo "0")
if [[ "$DB_SIZE_AFTER" -gt 0 && "$DB_SIZE_AFTER" -ge "$DB_SIZE_BEFORE" ]]; then
    pass "State DB persisted across restart (${DB_SIZE_AFTER} bytes)"
else
    fail "State DB lost or reset (before: ${DB_SIZE_BEFORE}B, after: ${DB_SIZE_AFTER}B)"
fi

# After a container restart, the fscache container gets a new mount namespace.
# The consumer's :rslave peer group relationship is with the old namespace and
# does not automatically re-establish. In production, consumers (e.g. Plex)
# must be restarted after fscache restarts to re-receive the FUSE propagation.
# We verify fscache itself serves files correctly after restart instead.
check "fscache serves media after restart" \
    docker exec fscache-test test -f /media/Movies/test-movie.bin

# ── Results ───────────────────────────────────────────────────────────────

echo ""
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
if [[ $fail -eq 0 ]]; then
    echo -e "  ${GREEN}All $pass tests passed.${NC}"
else
    echo -e "  ${GREEN}$pass passed${NC}  ${RED}$fail failed${NC}"
fi
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"

exit $fail
