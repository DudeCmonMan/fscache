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

Paste the fscache service into your existing `docker-compose.yml`, then make the edits described below.

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
    volumes:
      - /mnt/media:/media:rshared                  # <-- Your media path
      - /ssd/fscache/cache:/cache                  # <-- Your SSD cache path
      - /ssd/fscache/state:/var/lib/fscache        # <-- Keep alongside cache dir
    healthcheck:
      test: ["CMD-SHELL", "grep -q fscache /proc/mounts"]
      interval: 5s
      timeout: 3s
      retries: 20
      start_period: 10s
```

**Edit the left side of the three volume lines** to match your system:

| Volume | What to set the left side to |
|---|---|
| `/mnt/media:/media:rshared` | The host path where your media lives (your existing SMB/NFS/MergerFS mount) |
| `/ssd/fscache/cache:/cache` | A fast local directory for cached files (SSD strongly recommended) |
| `/ssd/fscache/state:/var/lib/fscache` | Persistent state directory (keep it alongside the cache dir) |

### 2. Update your existing Plex service

Add `depends_on` so Plex waits for fscache's FUSE mount to be ready:

```yaml
  plex:
    depends_on:
      fscache:
        condition: service_healthy
```

Change your Plex media volume to use `:rslave` propagation:

```yaml
    volumes:
      - /mnt/media:/media:rslave
```

The **host path** (left side) must be the **exact same path** as fscache's first volume. This is how Plex picks up fscache's FUSE overlay.

### 3. Start the stack

```bash
docker compose up -d
docker logs fscache    # confirm fscache mounted successfully
```

## How It Works

- fscache runs inside its own container with FUSE capabilities.
- It overmounts `/media` (inside the container) with a FUSE filesystem that transparently caches reads to your SSD.
- The `:rshared` propagation on the volume carries that FUSE mount back to the host.
- Plex's `:rslave` volume picks up the FUSE overlay from the host.
- Plex reads files normally — the caching is completely transparent.

`pid: host` lets fscache see host process names, so the Plex preset can filter out scanner/fingerprinter reads the same way it does on a bare-metal install.

## Customizing

For vanilla installs, the baked-in config works out of the box. To tune cache behavior, mount a custom config over the default:

```yaml
    volumes:
      - ./my-config.toml:/etc/fscache/config.toml:ro
```

Copy `docker/default-config.toml` from this repo as a starting point.

Common things to tune:
- `[eviction] max_size_gb` — total cache budget (default: 200 GB)
- `[eviction] expiry_hours` — evict files not accessed within this window (default: 72h)
- `[plex] lookahead` — episodes to cache ahead (default: 4)
- `[cache] min_file_size_mb` — skip small files like subtitles (default: 0, no filter)

## Multiple Libraries

If your media is split across multiple host paths (e.g., `/mnt/movies` and `/mnt/tv`), you have two options:

**Option A: One fscache container per library** (simpler)

Duplicate the fscache service with different names, volumes, and a custom config.toml for each.

**Option B: Multi-target in one container** (fewer containers)

Mount a custom config.toml with multiple `target_directories` and add matching volume mounts:

```yaml
    volumes:
      - /mnt/movies:/mnt/movies:rshared
      - /mnt/tv:/mnt/tv:rshared
      - /ssd/fscache/cache:/cache
      - /ssd/fscache/state:/var/lib/fscache
      - ./multi-library-config.toml:/etc/fscache/config.toml:ro
```

```toml
[paths]
target_directories = ["/mnt/movies", "/mnt/tv"]
cache_directory    = "/cache"
instance_name      = "fscache"
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
