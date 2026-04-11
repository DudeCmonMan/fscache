#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────────────────────
# fscache Docker entrypoint
#
# Renders /etc/fscache/config.toml from a template + FSCACHE_* env vars,
# then execs `fscache start`.
#
# Escape hatch: if /etc/fscache/config.toml already exists (user bind-mounted
# their own), it is used as-is and all env vars are ignored.
#
# Target directories: use FSCACHE_TARGET for a single mount, or
# FSCACHE_TARGET_2, FSCACHE_TARGET_3, ... for additional mounts. Paths may
# contain any character except a single quote (TOML literal-string encoding).
# If a path must contain a single quote, use the bind-mount escape hatch.
# ──────────────────────────────────────────────────────────────────────────
set -euo pipefail

CONFIG=/etc/fscache/config.toml
TEMPLATE=/etc/fscache/config.template.toml

# Escape hatch: user bind-mounted their own config — use it as-is.
if [[ -f "$CONFIG" ]]; then
    echo "[entrypoint] Using user-provided config at $CONFIG"
    exec fscache start --config "$CONFIG"
fi

# ── Scalar defaults ────────────────────────────────────────────────────────
# Each var is exported so envsubst can see it. The allowlist passed to envsubst
# below ensures that *only* these vars are expanded in the template — any other
# $ (e.g. in target paths appended later) is left untouched.

export FSCACHE_PRESET="${FSCACHE_PRESET:-plex-episode-prediction}"
export FSCACHE_PLEX_LOOKAHEAD="${FSCACHE_PLEX_LOOKAHEAD:-4}"
export FSCACHE_PLEX_MODE="${FSCACHE_PLEX_MODE:-miss-only}"
export FSCACHE_PREFETCH_MODE="${FSCACHE_PREFETCH_MODE:-cache-hit-only}"
export FSCACHE_PREFETCH_MAX_DEPTH="${FSCACHE_PREFETCH_MAX_DEPTH:-3}"
export FSCACHE_MAX_SIZE_GB="${FSCACHE_MAX_SIZE_GB:-200.0}"
export FSCACHE_EXPIRY_HOURS="${FSCACHE_EXPIRY_HOURS:-72}"
export FSCACHE_MIN_FREE_SPACE_GB="${FSCACHE_MIN_FREE_SPACE_GB:-10.0}"
export FSCACHE_MAX_CACHE_PULL_PER_MOUNT_GB="${FSCACHE_MAX_CACHE_PULL_PER_MOUNT_GB:-0.0}"
export FSCACHE_DEFERRED_TTL_MINUTES="${FSCACHE_DEFERRED_TTL_MINUTES:-1440}"
export FSCACHE_MIN_ACCESS_SECS="${FSCACHE_MIN_ACCESS_SECS:-2}"
export FSCACHE_MIN_FILE_SIZE_MB="${FSCACHE_MIN_FILE_SIZE_MB:-0}"
export FSCACHE_CACHE_WINDOW_START="${FSCACHE_CACHE_WINDOW_START:-08:00}"
export FSCACHE_CACHE_WINDOW_END="${FSCACHE_CACHE_WINDOW_END:-02:00}"
export FSCACHE_CONSOLE_LEVEL="${FSCACHE_CONSOLE_LEVEL:-info}"
export FSCACHE_FILE_LEVEL="${FSCACHE_FILE_LEVEL:-debug}"
export FSCACHE_REPEAT_LOG_WINDOW_SECS="${FSCACHE_REPEAT_LOG_WINDOW_SECS:-300}"

# ── Render scalars (allowlist prevents unintended $ expansion) ─────────────
ALLOWLIST='$FSCACHE_PRESET $FSCACHE_PLEX_LOOKAHEAD $FSCACHE_PLEX_MODE
$FSCACHE_PREFETCH_MODE $FSCACHE_PREFETCH_MAX_DEPTH $FSCACHE_MAX_SIZE_GB
$FSCACHE_EXPIRY_HOURS $FSCACHE_MIN_FREE_SPACE_GB
$FSCACHE_MAX_CACHE_PULL_PER_MOUNT_GB $FSCACHE_DEFERRED_TTL_MINUTES
$FSCACHE_MIN_ACCESS_SECS $FSCACHE_MIN_FILE_SIZE_MB
$FSCACHE_CACHE_WINDOW_START $FSCACHE_CACHE_WINDOW_END
$FSCACHE_CONSOLE_LEVEL $FSCACHE_FILE_LEVEL $FSCACHE_REPEAT_LOG_WINDOW_SECS'

envsubst "$ALLOWLIST" < "$TEMPLATE" > "$CONFIG"

# ── Append [paths] section ─────────────────────────────────────────────────
# Each path is emitted as a TOML literal string (single-quoted).
# Literal strings require no escape sequences — they cannot contain single quotes.

_validate_path() {
    local val="$1" varname="$2"
    case "$val" in
        *"'"*)
            echo >&2 "[entrypoint] error: $varname contains a single quote — paths with single quotes cannot be encoded as TOML literal strings. Use the bind-mount escape hatch (mount your own /etc/fscache/config.toml)."
            exit 1
            ;;
    esac
}

{
    echo ""
    echo "[paths]"
    echo 'cache_directory = "/cache"'
    echo 'instance_name   = "fscache"'
    printf 'target_directories = ['

    first_target="${FSCACHE_TARGET:-${FSCACHE_TARGET_1:-/media}}"
    _validate_path "$first_target" "FSCACHE_TARGET"
    printf "'%s'" "$first_target"

    i=2
    while :; do
        var="FSCACHE_TARGET_$i"
        val="${!var:-}"
        [[ -z "$val" ]] && break
        _validate_path "$val" "$var"
        printf ", '%s'" "$val"
        i=$((i + 1))
    done

    printf ']\n'
} >> "$CONFIG"

echo "[entrypoint] Rendered $CONFIG from template + FSCACHE_* env vars"
exec fscache start --config "$CONFIG"
