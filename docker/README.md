# fscache Docker Sidecar

Run fscache alongside Plex (or any media server) as a Docker sidecar container. No host-level setup required beyond creating a few directories.

## Prerequisites

1. **Docker Engine** with Compose v2 (`docker compose` without the hyphen).

2. **Host directories** for the cache and state. Create them before first run:
   ```bash
   mkdir -p /ssd/fscache/cache /ssd/fscache/state
   ```

3. **Mount propagation** must be enabled on the host. Most systemd-based distros already have this. If you're unsure:
   ```bash
   sudo mount --make-rshared /
   ```
   This is a one-time command. It enables FUSE mounts inside the container to propagate back to the host.

## Quick Start

### 1. Add the fscache service

```yaml
services:
  fscache:
    image: dudecmonman/fscache:latest
    container_name: fscache
    restart: unless-stopped
    cap_add: [SYS_ADMIN]
    devices: ["/dev/fuse:/dev/fuse"]
    security_opt: ["apparmor:unconfined"]
    pid: host
    environment:
      FSCACHE_MAX_SIZE_GB: "200"     # total cache budget in GB
      FSCACHE_EXPIRY_HOURS: "72"     # evict files not accessed within this window
    volumes:
      - /mnt/media:/media:rshared           # <-- Your media path
      - /ssd/fscache/cache:/cache           # <-- Your SSD cache path
      - /ssd/fscache/state:/var/lib/fscache # <-- Keep alongside cache dir
    healthcheck:
      test: ["CMD-SHELL", "grep -q fscache /proc/mounts"]
      interval: 5s
      timeout: 3s
      retries: 20
      start_period: 10s
```

Edit the left side of the three volume lines to match your system:

| Volume | What to set the left side to |
|---|---|
| `/mnt/media:/media:rshared` | Your media path (SMB/NFS/MergerFS mount, etc.) |
| `/ssd/fscache/cache:/cache` | Fast local directory for cached files (SSD recommended) |
| `/ssd/fscache/state:/var/lib/fscache` | Persistent state directory (keep it alongside the cache dir) |

> See [Example config](#example-config) for more env var options.

### 2. Update your Plex service

Add `depends_on` and switch the media volume to `:rslave`:

```yaml
  plex:
    depends_on:
      fscache:
        condition: service_healthy
    volumes:
      - /mnt/media:/media:rslave   # must be the exact same host path as fscache's first volume
```

### 3. Start the stack

```bash
docker compose up -d
docker logs fscache    # confirm fscache mounted successfully
```

## Example config

A fuller `environment:` block covering the most common knobs:

```yaml
    environment:
      FSCACHE_MAX_SIZE_GB: "200"          # total cache budget in GB
      FSCACHE_EXPIRY_HOURS: "72"          # evict files not accessed within this window
      FSCACHE_MIN_FREE_SPACE_GB: "10"     # always keep this much free on the cache volume
      FSCACHE_PLEX_LOOKAHEAD: "4"         # episodes to prefetch ahead
      FSCACHE_MIN_FILE_SIZE_MB: "0"       # skip files smaller than this (e.g. "5" ignores subtitles)
      FSCACHE_CONSOLE_LEVEL: info         # debug / info / warn / error
      FSCACHE_CACHE_WINDOW_START: "08:00"
      FSCACHE_CACHE_WINDOW_END: "02:00"   # caching window (wraps past midnight)
```

For the full list of all 17 env vars and the bind-mount escape hatch, see [Customizing](#customizing) below.

## How It Works

- fscache runs inside its own container with FUSE capabilities.
- It overmounts `/media` (inside the container) with a FUSE filesystem that transparently caches reads to your SSD.
- The `:rshared` propagation on the volume carries that FUSE mount back to the host.
- Plex's `:rslave` volume picks up the FUSE overlay from the host.
- Plex reads files normally — the caching is completely transparent.

`pid: host` lets fscache see host process names, so the Plex preset can filter out scanner/fingerprinter reads the same way it does on a bare-metal install.

## Customizing

### Environment variables (common tuning)

Set `FSCACHE_*` environment variables in your compose file. All knobs have sensible defaults — only set what you want to change.

| Variable | Default | Description |
|---|---|---|
| `FSCACHE_MAX_SIZE_GB` | `200.0` | Total cache budget in GB |
| `FSCACHE_EXPIRY_HOURS` | `72` | Evict files not accessed within this window |
| `FSCACHE_MIN_FREE_SPACE_GB` | `10.0` | Always keep at least this much free on the cache volume |
| `FSCACHE_PLEX_LOOKAHEAD` | `4` | Episodes to prefetch ahead of what Plex is playing |
| `FSCACHE_PLEX_MODE` | `miss-only` | When fscache intercepts Plex reads (`miss-only` or `always`) |
| `FSCACHE_PRESET` | `plex-episode-prediction` | Behavior preset |
| `FSCACHE_PREFETCH_MODE` | `cache-hit-only` | When to trigger prefetch (`cache-hit-only` or `always`) |
| `FSCACHE_PREFETCH_MAX_DEPTH` | `3` | How many episodes deep to prefetch |
| `FSCACHE_MAX_CACHE_PULL_PER_MOUNT_GB` | `0.0` | Per-session prefetch budget per mount (0 = unlimited) |
| `FSCACHE_DEFERRED_TTL_MINUTES` | `1440` | Discard buffered events older than this on startup |
| `FSCACHE_MIN_ACCESS_SECS` | `2` | Minimum seconds a file must stay open before prediction triggers (Docker default is 2 to filter scanner stat-ahead and thumbnail probes; bare-metal default is 0) |
| `FSCACHE_MIN_FILE_SIZE_MB` | `0` | Skip files smaller than this — useful to ignore subtitle/metadata files |
| `FSCACHE_CACHE_WINDOW_START` | `08:00` | Start of active caching window |
| `FSCACHE_CACHE_WINDOW_END` | `02:00` | End of active caching window |
| `FSCACHE_CONSOLE_LEVEL` | `info` | Console log level (`debug`, `info`, `warn`, `error`) |
| `FSCACHE_FILE_LEVEL` | `debug` | Log file level |
| `FSCACHE_REPEAT_LOG_WINDOW_SECS` | `300` | Suppress repeated log lines within this window |

For multi-target setups, see [Multiple Libraries](#multiple-libraries) below.

### Bind-mount escape hatch (advanced)

Fields not exposed as env vars (`passthrough_mode`, process blocklists, file whitelist/blacklist) can only be changed by mounting a full config file:

```yaml
    volumes:
      - ./my-config.toml:/etc/fscache/config.toml:ro
```

When `/etc/fscache/config.toml` exists at container start, the entrypoint uses it as-is and all `FSCACHE_*` env vars are ignored. Use `docker/config.template.toml` from this repo as a starting point.

## Multiple Libraries

If your media is split across multiple host paths (e.g., `/mnt/movies` and `/mnt/tv`), you have two options:

**Option A: One fscache container per library** (simpler)

Duplicate the fscache service with different names, volumes, and a custom config.toml for each.

**Option B: Multi-target in one container** (fewer containers)

Use `FSCACHE_TARGET` for the first path and `FSCACHE_TARGET_2`, `FSCACHE_TARGET_3`, … for additional paths, with matching volume mounts:

```yaml
  fscache:
    image: dudecmonman/fscache:latest
    environment:
      FSCACHE_TARGET:   /mnt/movies
      FSCACHE_TARGET_2: /mnt/tv
      FSCACHE_MAX_SIZE_GB: "500"
    volumes:
      - /mnt/movies:/mnt/movies:rshared
      - /mnt/tv:/mnt/tv:rshared
      - /ssd/fscache/cache:/cache
      - /ssd/fscache/state:/var/lib/fscache
    # caps, healthcheck, pid: host unchanged
```

Note: for multi-target, use the same path inside and outside the container (e.g., `/mnt/movies:/mnt/movies`) so that the overmount trick works cleanly with `:rshared` propagation.

## Monitoring

Attach the TUI dashboard to a running fscache container:

```bash
docker exec -it fscache fscache watch
```

## Troubleshooting

**Plex sees an empty media directory**
- Check that fscache's first volume and Plex's media volume use the **exact same host path** on the left side.
- Check that fscache's volume has `:rshared` and Plex's volume has `:rslave`.
- Verify fscache actually mounted: `docker exec fscache grep fscache /proc/mounts`

**fscache container keeps restarting**
- Check logs: `docker logs fscache`
- Verify the host media path exists and contains files.
- Verify the cache and state directories exist on the host.

**Mount propagation not working**
- Run `sudo mount --make-rshared /` on the host and try again.
- On some systems (Synology, older Unraid), this may need to be added to a startup script.

**Plex loses access to media after fscache restarts**
- This is expected. When fscache restarts, it gets a new mount namespace and the FUSE overlay does not automatically re-propagate to Plex's container.
- Restart Plex after fscache is healthy: `docker compose restart plex`
- To avoid this, always restart the full stack together: `docker compose restart` (restarts all services in dependency order).

**Cache not being used (files always read from backing store)**
- Check that `pid: host` is set on the fscache service. Without it, the Plex preset can't identify Plex processes and may filter incorrectly.
- Check `docker exec fscache fscache watch` to see cache activity in real time.
