# fscache

tl;dr: Transparent, user-configurable SSD cache for slow storage (NAS/HDD) using FUSE — no application changes required. Caches hot files locally and serves future reads from SSD.

Transparent SSD caching for any read-heavy file workload — media servers, NAS-backed applications, anything that reads from slow storage. Sits between your application and a network share / drive array by mounting directly over your existing directories using FUSE — your application sees no difference, but files are silently pre-copied to local SSD before they're needed.

No plugins, no API wrappers, no config changes on the application side. Drop it in, point it at your media directory, and remove it just as easily. As long as it receives a proper signal, you can stop it while your server is running. Active streams will need to be restarted, but your server won't.

I created this for a few reasons:
1. Works on top of any filesystem. This is the main motivation. I have MergerFS + Snapraid and some LVM. A lot of times, I just want to add a small file cache on top of my most hotly contested files without needing to change the entire setup.
2. Better handle array spin-up/downs to save power (it really adds up in SoCal)
3. Improve the viewing experience for myself
4. No other tool did what I wanted — straightforward, simple setup and teardown

This started life as **plex-hot-cache** — a Plex-specific caching layer. As the design matured, I realized that the FUSE and caching logic was the main "magic" here, so it became **fscache** and open to all services. The Plex-specific smarts (episode prediction, transcoder filtering) are still here as a preset, but the tool works just as well for any read-heavy generic file workload.

I plan on supporting this software to flesh out even more features (write caching, more eviction algos, more caching algos, other software integrations, etc.)

**FOR DOCKER USERS:** WE HAVE A DOCKER SPECIFIC SETUP AND IMAGE! Use this Readme!
https://github.com/DudeCmonMan/fscache/blob/main/docker/README.md (I'm still looking for feedback on this, I don't run Plex on Docker, any testing is appreciated!)

If this utility helped you, I would really appreciate a star!

# WARNING: PLEASE READ THIS BEFORE TRYING

**This is a new project. I HIGHLY recommend you DISABLE automatic trash emptying in Plex while evaluating this software. Filesystem mounting/unmounting is potentially dangerous on a live server. If Plex detects a drive went down and you have automatic trash cleanup enabled, it WILL delete your Plex metadata (not the files — just watch history, ratings, etc.). My codebase has extensive automated testing that protects against this type of failure, but please be safe. If you're using this tool, you're probably hoarding data like me and I would HATE to see a critical bug break your metadata.**

---

## How it works

fscache mounts a read-only FUSE filesystem **directly over** the existing filesystem directory. Your application keeps reading from the same path it always has. When a file is opened, FUSE intercepts the request and serves it from the SSD cache if available, falling back to the original backing store transparently if not.

In the background, a preset-driven action engine watches which files are being opened and decides what else to cache — the next N episodes, neighboring files, or an entire directory tree, depending on how you configure it.

![fscache architecture diagram](docs/pages/fscache_flowchart_v032.svg)

On shutdown, the FUSE mount is lazily detached — any streams already in progress continue uninterrupted from their open file descriptors.

---

## Presets

Behavior is controlled by a **preset** — a pluggable strategy for deciding what to cache and when.

### `plex-episode-prediction` (default)

Episode lookahead tailored for Plex. Pre-caches the next N episodes after any file open. Intelligently filters out Plex's background analysis processes (Media Scanner, EAE Service, Fingerprinter, intro/credit detection transcoders) so only real user playback drives the prediction. Supports two modes:

- **`miss-only`** (default) — predict and cache ahead only on a cache miss
- **`rolling-buffer`** — keep the next N episodes loaded at all times, re-predicting on every access

The `episode-prediction` name is accepted as an alias.

Configured via the `[plex]` section.

### `prefetch`

General-purpose preset with three caching modes and optional regex filtering. Not Plex-specific — works for any file workload.

**Modes:**

- **`cache-hit-only`** — cache only the file that was accessed. Safest, no fan-out.
- **`cache-neighbors`** — cache all sibling files in the same directory. Good for media folders where related files (episodes, artwork, metadata) live side by side.
- **`cache-parent-recursively`** — recursively cache the entire parent directory up to `max_depth` levels deep. Aggressive, but useful when a single access should warm an entire show or album.

**Filtering:**

- **`file_whitelist`** — regex patterns matched against filename. Only matching files are cached. Empty list means allow all.
- **`file_blacklist`** — regex patterns matched against filename. Matching files are never cached. Blacklist is checked first and always wins over whitelist.
- **`process_blocklist`** — process names (and their children) that must never trigger caching.

All regex patterns are compiled at startup. Invalid patterns cause the daemon to refuse to start — no silent failures at runtime.

Configured via the `[prefetch]` section.

Set the preset via `[preset] name` in `config.toml`.

---

## Principles

- **Launch at any time.** The FUSE mount can go up or come down without restarting your application. Streams already in flight are not interrupted on shutdown.
- **Graceful by default.** Cache corruption, copy failures, and missing files are all handled without crashing — the worst case is a cache miss that falls back to the network share.
- **Drop-in / drop-out.** No modifications to Plex or your media library. Remove the service and your media directory is exactly as it was.
- **Multiple mounts.** Point fscache at multiple media directories simultaneously — each gets its own namespaced cache subdirectory.

---

## Quick Start

### 1. Download and extract

Grab the latest pre-built release from the [Releases page](https://github.com/DudeCmonMan/fscache/releases) and extract it wherever you want to run it from:

```bash
tar -xzf fscache-*.tar.gz -C /opt/fscache
```

The release includes the binary, a default `config.toml`, and the LICENSE.

### 2. Configure

Edit `config.toml` in the same directory as the binary. At minimum, set three values:

```toml
[paths]
target_directories = ["/mnt/media"]   # directories to mount over (can list multiple)
cache_directory    = "/mnt/ssd-cache" # SSD path for cached files
instance_name      = "my-media"       # unique name for this instance (used for DB and process lock)
```

See the full [Settings](#settings) section below for all options.

### 3. Allow FUSE access

Plex runs as a different user, so it needs permission to access the FUSE mount:

```bash
echo "user_allow_other" | sudo tee -a /etc/fuse.conf
```

### 4. Run

```bash
cd /opt/fscache
sudo ./fscache
```

That's it. The cache is active and filesystem doesn't need any changes. Stop it with `Ctrl+C` — the mount detaches cleanly. LET IT DO IT'S THING, DO NOT DOUBLE TAP CTRL+C.

To monitor cache activity in real time, run `./fscache watch` in a separate terminal — see [Monitoring with `fscache watch`](#monitoring-with-fscache-watch) below.

---

## Running with systemd (recommended)

For a persistent setup that starts on boot:

```bash
sudo cp fscache.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now fscache
```

Verify it's running:

```bash
systemctl status fscache
mount | grep fscache
```

The service file is included in the release. If your media is on a network share, edit the unit to wait for the mount — see the comments inside `fscache.service`.

---

## Building from source

```bash
cargo build --release
sudo cp target/release/fscache /usr/local/bin/
```

---

## Settings

### Required

| Setting | Description |
|---|---|
| `paths.target_directories` | List of directories to mount over using FUSE (each will be transparently intercepted) |
| `paths.cache_directory` | SSD path where cached files are stored |
| `paths.instance_name` | Unique name for this instance — used for DB filename and process lock |

### Preset

| Setting | Default | Description |
|---|---|---|
| `preset.name` | `plex-episode-prediction` | `plex-episode-prediction` or `prefetch` |

### Plex Episode Prediction

Used when `preset.name` is `plex-episode-prediction` (or `episode-prediction`).

| Setting | Default | Description |
|---|---|---|
| `plex.lookahead` | `4` | Episodes to pre-cache ahead of current position |
| `plex.mode` | `miss-only` | `miss-only` (predict on miss) or `rolling-buffer` (keep next N always loaded) |
| `plex.process_blocklist` | `[]` | Process names that must never trigger prediction |

### Prefetch

Used when `preset.name` is `prefetch`.

| Setting | Default | Description |
|---|---|---|
| `prefetch.mode` | `cache-hit-only` | `cache-hit-only`, `cache-neighbors`, or `cache-parent-recursively` |
| `prefetch.max_depth` | `3` | Max recursion depth for `cache-parent-recursively` mode |
| `prefetch.process_blocklist` | `[]` | Process names that must never trigger caching |
| `prefetch.file_whitelist` | `[]` | Regex patterns — only matching filenames are cached (empty = allow all) |
| `prefetch.file_blacklist` | `[]` | Regex patterns — matching filenames are never cached (checked before whitelist) |

### Eviction

| Setting | Default | Description |
|---|---|---|
| `eviction.max_size_gb` | `200.0` | Max total SSD cache size across all mounts |
| `eviction.expiry_hours` | `72` | Remove cached files not accessed within this window |
| `eviction.min_free_space_gb` | `10.0` | Stop caching if SSD free space drops below this |
| `eviction.poll_interval_secs` | `300` | How often the background maintenance task runs (seconds). Drives eviction sweeps and on-maintenance invalidation. Set `0` to disable. |

### Invalidation

Detects and discards cached files whose backing source has been rewritten (e.g. by rsync, re-encode, or a fresh download).

| Setting | Default | Description |
|---|---|---|
| `invalidation.check_on_hit` | `false` | Revalidate against the backing file on every FUSE cache hit. Adds one `stat()` per hit — negligible on local storage, may be noticeable on SMB/NFS at scale. Detects staleness in real time. |
| `invalidation.check_on_maintenance` | `true` | Revalidate during the periodic maintenance sweep. Catches idle files that were overwritten on the backing store since they were cached. Detection latency ≤ `eviction.poll_interval_secs`. |

### Cache

| Setting | Default | Description |
|---|---|---|
| `cache.passthrough_mode` | `false` | Bypass cache entirely — useful for debugging |
| `cache.max_cache_pull_per_mount_gb` | `0.0` (unlimited) | Cap per-mount lookahead pull per session |
| `cache.deferred_ttl_minutes` | `1440` | Discard buffered events (from outside caching window) older than this on startup |
| `cache.min_access_secs` | `0` | Minimum seconds a file must stay open before prediction triggers |
| `cache.min_file_size_mb` | `0` | Skip files below this size in MB — filters metadata/subtitle noise |

### Schedule

| Setting | Default | Description |
|---|---|---|
| `schedule.cache_window_start` | `08:00` | Start of allowed caching window (HH:MM, 24h) |
| `schedule.cache_window_end` | `02:00` | End of allowed caching window (wraps past midnight) |

Accesses outside the window are buffered and flushed when the window re-opens.

### Logging

| Setting | Default | Description |
|---|---|---|
| `logging.log_directory` | `/var/log/fscache` | Directory for rolling daily log files |
| `logging.console_level` | `info` | Terminal log level (`error`/`warn`/`info`/`debug`/`trace`) |
| `logging.file_level` | `debug` | Log file level |
| `logging.repeat_log_window_secs` | `300` | Suppress repeated access logs for the same file within this window |

---

## Example config.toml

```toml
[paths]
target_directories = ["/mnt/media", "/mnt/media2"]
cache_directory    = "/mnt/ssd-cache"
instance_name      = "plex-movies"

[preset]
name = "plex-episode-prediction"

[plex]
lookahead         = 4
mode              = "miss-only"
process_blocklist = ["Plex Media Scanner", "Plex EAE Service", "Plex Media Fingerprinter"]

[eviction]
max_size_gb         = 200.0
expiry_hours        = 72
min_free_space_gb   = 10.0
poll_interval_secs  = 300

[invalidation]
check_on_hit         = false
check_on_maintenance = true

[cache]
passthrough_mode            = false
max_cache_pull_per_mount_gb = 0.0
min_access_secs             = 0
min_file_size_mb            = 0

[schedule]
cache_window_start = "08:00"
cache_window_end   = "02:00"

[logging]
log_directory          = "/var/log/fscache"
console_level          = "info"
file_level             = "debug"
repeat_log_window_secs = 300
```

### Prefetch example

```toml
[preset]
name = "prefetch"

[prefetch]
mode              = "cache-neighbors"
max_depth         = 3
file_blacklist    = ["\\.nfo$", "\\.jpg$"]
# file_whitelist  = ["\\.mkv$", "\\.mp4$"]
```

---

## Monitoring with `fscache watch`

`fscache watch` attaches a terminal dashboard to a running daemon — it does not start one.

```bash
./fscache watch
```

It auto-discovers running instances. If you have multiple, use `-i <name>` to target a specific one, or `--socket <path>` for a direct socket path. The TUI detaches cleanly when you quit; the daemon keeps running.

### Status page

Shows the scheduler window (open/closed), a color-coded cache budget gauge, action engine state (in-flight and deferred operations), mount health, live FUSE activity counters (opens, hits, misses, bytes read), and a tail of recent log lines.

### Cache page

A browsable list of every cached file with sortable columns and a per-file detail pane (size, cached-at time, last read, eviction deadline). You can select individual files with `Space` or range-select with `Shift+↑↓`, then press `Enter` to open an action menu with two options:

- **Evict files** — removes the selected file(s) from cache
- **Refresh lease** — extends the eviction deadline for the selected file(s)

If nothing is checked when you press `Enter`, the action applies to the highlighted row only.

### Logs page

Scrollable ring buffer of the 200 most recent daemon log lines, color-coded by level (error / warn / info / debug).

### Key bindings

| Key | Where | Action |
|---|---|---|
| `q` | global | detach TUI (daemon keeps running) |
| `Ctrl+Q` | global | send shutdown to daemon, then exit |
| `←` `→` / `1` `2` `3` | global | switch pages |
| `↑` `↓` | Cache / Logs | move selection or scroll |
| `Shift+↑` `Shift+↓` | Cache | range-select while moving |
| `Space` | Cache | toggle checkbox on highlighted file |
| `Home` `End` | Cache / Logs | jump to first / last |
| `s` | Cache | cycle sort (newest / oldest / largest / smallest / name A–Z) |
| `Enter` | Cache | open action menu |
| `↑` `↓` `Enter` `Esc` | Action menu | choose action or dismiss |

> More features are planned for the monitor — this is the starting set.

<table>
<tr>
<td><img width="758" height="822" alt="image" src="https://github.com/user-attachments/assets/993d9b19-d354-45a9-9961-7abf9d86cc44" /></td>
<td><img width="758" height="822" alt="image" src="https://github.com/user-attachments/assets/c149cec9-6992-4323-91a8-fe4e80aeea74" /></td>
</tr>
</table>

---

## Discovery mode

Discovery mode answers one question: *which processes are actually touching my files?* While armed, every FUSE metadata op and open() is attributed to the calling process. Counts are aggregated in memory, flushed to the cache DB on a fixed interval, and also written as human-readable `NEW` / `SNAP` lines to a dedicated rotating log at `{log_directory}/{instance_name}-discovery.log`.

It's **off by default** and safe to flip on and off at runtime — no restart needed. Useful for tuning `process_blocklist`, spotting unexpected scanners, or just sanity-checking what Plex (or anything else) is hitting your mount.

### Quickstart

Arm it for the configured default duration (1h unless you change `default_duration_secs`):

```bash
./fscache discover start
```

Arm with an explicit window:

```bash
./fscache discover start --duration 10m
./fscache discover start --duration 2h
./fscache discover start --indefinite     # no auto-stop
```

Stop early:

```bash
./fscache discover stop
```

Check what's currently armed:

```bash
./fscache discover status
```

Query historical data from the DB — this works whether discovery is currently armed or not, and does not require a live daemon connection (it reads the DB directly):

```bash
./fscache discover stat                   # last 1h, all op kinds
./fscache discover stat 24h               # last 24 hours
./fscache discover stat 15m --kind miss   # only cache misses
./fscache discover stat 7d --top 50       # top 50 processes over a week
```

Output is a simple table:

```
PROCESS                               HIT      MISS      META     TOTAL
------------------------------------------------------------------------
Plex Media Server                    4312        18      9021     13351
Plex Media Scanner                      0         0      7410      7410
Plex Transcoder                       412         2        83       497
```

You can also enable it to arm automatically at daemon startup via `[discovery] enabled = true` in `config.toml`.

### Performance

The performance hit is **very, very minor — basically impossible to measure in practice**. The hot path stays lock-free via a thread-local PID cache; a non-cached PID lookup is on the order of tens of microseconds (one `/proc` read) and every subsequent op on that PID is a branch and an atomic increment. With caching enabled, the user-visible latency of a cache hit dwarfs any discovery overhead by orders of magnitude.

Turn it on and off at your leisure. I can't stress enough how negligible the cost is, especially relative to the IO savings the cache itself provides.

### Settings

See the `[discovery]` section in `config.toml`. For most users only `enabled` and `window_days` matter — the rest is advanced tuning.

| Setting | Default | Description |
|---|---|---|
| `discovery.enabled` | `false` | Arm at daemon startup. Use `discover start/stop` for runtime control. |
| `discovery.window_days` | `7` | Retain DB rows and discovery log files for this many days |
| `discovery.bucket_interval_secs` | `60` | How often in-memory counts are flushed to the DB (and SNAP lines emitted) |
| `discovery.default_duration_secs` | `3600` | Auto-stop after this many seconds when `--duration` is omitted from `discover start` (0 = no auto-stop) |
| `discovery.pid_lru_capacity` | `512` | Max PIDs cached per session — avoids repeated `/proc` reads |
| `discovery.pid_lru_ttl_secs` | `300` | Evict PID cache entries older than this (guards against PID reuse) |
