# f-cache

Transparent SSD caching for Plex media (and any other file workload). Sits between your application and a network share / drive array using a FUSE overmount — Plex sees no difference, but episode files are silently pre-copied to local SSD before they're needed. Designed to run ON the Plex server itself.

No Plex plugins, no API wrappers, no config changes on the Plex side. Drop it in, point it at your media directory, and remove it just as easily. As long as it receives a proper signal, you can stop it while your server is running. Current streams will need to be restarted, but your server won't.

I created this for a few reasons:
1. Better handle array spin-up/downs to save power (it really adds up in SoCal)
2. Improve the viewing experience for myself
3. No other tool did what I wanted — straightforward, simple setup and teardown

## Now enhanced with Ratatui via --tui
<table>
<tr>
<td><img width="758" height="822" alt="image" src="https://github.com/user-attachments/assets/993d9b19-d354-45a9-9961-7abf9d86cc44" /></td>
<td><img width="758" height="822" alt="image" src="https://github.com/user-attachments/assets/c149cec9-6992-4323-91a8-fe4e80aeea74" /></td>
</tr>
</table>

# WARNING: PLEASE READ THIS BEFORE TRYING

**This is a new project. I HIGHLY recommend you DISABLE automatic trash emptying in Plex while evaluating this software. Filesystem mounting/unmounting is potentially dangerous on a live server. If Plex detects a drive went down and you have automatic trash cleanup enabled, it WILL delete your Plex metadata (not the files — just watch history, ratings, etc.). My codebase has extensive automated testing that protects against this type of failure, but please be safe. If you're using this tool, you're probably hoarding data like me and I would HATE to see a critical bug break your metadata.**

---

## How it works

f-cache mounts a read-only FUSE filesystem **directly over** the existing media directory. Plex keeps reading from the same path it always has. When a file is opened, FUSE intercepts the request and serves it from the SSD cache if available, falling back to the network share transparently if not.

In the background, an action engine watches which files are being opened and pre-copies the next N episodes to the SSD so they're ready before Plex needs them.

```
Plex → /mnt/media (FUSE overmount)
              ├─ cache hit  → /mnt/ssd-cache/...   (fast, local SSD)
              └─ cache miss → backing SMB/NFS mount (slow, network)
```

On shutdown, the FUSE mount is lazily detached — any streams already in progress continue uninterrupted from their open file descriptors.

---

## Presets

Behavior is controlled by a **preset** — a pluggable strategy for deciding what to cache and when.

**`plex-episode-prediction`** (default) — Episode lookahead tailored for Plex. Pre-caches the next N episodes after any file open. Intelligently filters out Plex's background analysis processes (Media Scanner, EAE Service, Fingerprinter, intro/credit detection transcoders) so only real user playback drives the prediction. Recommended for Plex users.

**`episode-prediction`** — Same lookahead logic without Plex-specific filtering. Good for other media players or servers.

**`cache-on-miss`** — Cache only what is accessed. No lookahead — just transparently promotes files to SSD as they're opened.

Set via `[preset] name` in `config.toml`.

---

## Principles

- **Launch at any time.** The FUSE mount can go up or come down without restarting Plex. Streams already in flight are not interrupted on shutdown.
- **Graceful by default.** Cache corruption, copy failures, and missing files are all handled without crashing — the worst case is a cache miss that falls back to the network share.
- **Drop-in / drop-out.** No modifications to Plex or your media library. Remove the service and your media directory is exactly as it was.
- **Multiple mounts.** Point f-cache at multiple media directories simultaneously — each gets its own namespaced cache subdirectory.

---

## Quick Start

### 1. Download and extract

Grab the latest release from the [Releases page](https://github.com/DudeCmonMan/plex-hot-cache/releases) and extract it wherever you want to run it from:

```bash
tar -xzf f-cache-*.tar.gz -C /opt/f-cache
```

The release includes the binary, a default `config.toml`, and the LICENSE.

### 2. Configure

Edit `config.toml` in the same directory as the binary. At minimum, set two paths:

```toml
[paths]
target_directories = ["/mnt/media"]   # media directories Plex reads from (can list multiple)
cache_directory    = "/mnt/ssd-cache" # SSD path for cached files
```

See the full [Settings](#settings) section below for all options.

### 3. Allow FUSE access

Plex runs as a different user, so it needs permission to access the FUSE mount:

```bash
echo "user_allow_other" | sudo tee -a /etc/fuse.conf
```

### 4. Run

```bash
cd /opt/f-cache
sudo ./f-cache
```

That's it. The cache is active and Plex doesn't need any changes. Stop it with `Ctrl+C` — the mount detaches cleanly.

Add `--tui` to run with a live dashboard showing cache stats, active copies, and logs.

---

## Running with systemd (recommended)

For a persistent setup that starts on boot:

```bash
sudo cp f-cache.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now f-cache
```

Verify it's running:

```bash
systemctl status f-cache
mount | grep f-cache
```

The service file is included in the release. If your media is on a network share, edit the unit to wait for the mount — see the comments inside `f-cache.service`.

---

## Building from source

```bash
cargo build --release
sudo cp target/release/f-cache /usr/local/bin/
```

---

## Settings

### Required

| Setting | Description |
|---|---|
| `paths.target_directories` | List of media directories Plex reads from (each will be FUSE overmounted) |
| `paths.cache_directory` | SSD path where cached files are stored |

### Preset

| Setting | Default | Description |
|---|---|---|
| `preset.name` | `plex-episode-prediction` | `plex-episode-prediction`, `episode-prediction`, or `cache-on-miss` |
| `preset.lookahead` | `4` | Episodes to pre-cache ahead of current position (episode prediction presets) |

### Cache

| Setting | Default | Description |
|---|---|---|
| `cache.max_size_gb` | `200.0` | Max total SSD cache size across all mounts |
| `cache.expiry_hours` | `72` | Remove cached files not accessed within this window |
| `cache.min_free_space_gb` | `10.0` | Stop caching if SSD free space drops below this |
| `cache.max_cache_pull_per_mount_gb` | `0.0` (unlimited) | Cap per-mount lookahead pull per session |
| `cache.deferred_ttl_minutes` | `1440` | Discard buffered events (from outside caching window) older than this on startup |
| `cache.passthrough_mode` | `false` | Bypass cache entirely — useful for debugging |
| `cache.process_blocklist` | `[]` | Process names (and their children) that must never trigger caching |

### Schedule

| Setting | Default | Description |
|---|---|---|
| `schedule.cache_window_start` | `08:00` | Start of allowed caching window (HH:MM, 24h) |
| `schedule.cache_window_end` | `02:00` | End of allowed caching window (wraps past midnight) |

Accesses outside the window are buffered and flushed when the window re-opens.

### Logging

| Setting | Default | Description |
|---|---|---|
| `logging.log_directory` | `/var/log/f-cache` | Directory for rolling daily log files |
| `logging.console_level` | `info` | Terminal log level (`error`/`warn`/`info`/`debug`/`trace`) |
| `logging.file_level` | `debug` | Log file level |
| `logging.repeat_log_window_secs` | `300` | Suppress repeated access logs for the same file within this window |

---

## Example config.toml

```toml
[paths]
target_directories = ["/mnt/media", "/mnt/media2"]
cache_directory    = "/mnt/ssd-cache"

[preset]
name      = "plex-episode-prediction"
lookahead = 4

[cache]
max_size_gb                 = 200.0
expiry_hours                = 72
min_free_space_gb           = 10.0
max_cache_pull_per_mount_gb = 0.0
deferred_ttl_minutes        = 1440
process_blocklist           = ["Plex Media Scanner", "Plex EAE Service", "Plex Media Fingerprinter"]

[schedule]
cache_window_start = "08:00"
cache_window_end   = "02:00"

[logging]
log_directory          = "/var/log/f-cache"
console_level          = "info"
file_level             = "debug"
repeat_log_window_secs = 300
```
