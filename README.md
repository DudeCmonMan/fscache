# plex-hot-cache

Transparent SSD caching for Plex media. It sits between Plex and your network share / drive arrays / whatever other data backing using a FUSE overmount — Plex sees no difference, but episode files are silently pre-copied to local SSD before they're needed. It is meant to be run ON the Plex server itself, but theoretically could be added to your NAS since it relies solely on FUSE R/W events. Eventually I plan to have Plex Integrations directly.

No Plex plugins, no API wrappers, no config changes on the Plex side. Drop it in, point it at your media directory, and remove it just as easily. As long as it receives a proper signal that it can handle, you can stop it while your server is running. The current streams will crash and need to be restarted, but your server will not.

One of the main principles I had when developing this was that it is a simple "drop-in" hot cache binary that anyone can use. I try my best to make it crash gracefully.

I created this binary for a few reasons: 
    1) I wanted to better handle my array spin up / downs to save power (it really adds up in SoCal)
    2) To improve the viewing experience for myself
    3) No other library really did what I wanted it to do (straightforward, simple setup, simple teardown,)

## Now enhanced with Ratatui via --tui
<table>                       
<tr>                               
<td><img width="758" height="822" alt="image" src="https://github.com/user-attachments/assets/993d9b19-d354-45a9-9961-7abf9d86cc44" /></td>
<td><img width="758" height="822" alt="image" src="https://github.com/user-attachments/assets/c149cec9-6992-4323-91a8-fe4e80aeea74" /></td>
</tr>
</table>  

# WARNING: PLEASE READ THIS BEFORE TRYING

**This is a new project, I HIGHLY recommend you DISABLE automatic trash emptying in Plex while you are evaluating this software. Filesystem mounting / unmounting is potentially dangerous when running on a live server. If for some reason Plex detects that a drive went down and you clean up your trash automatically, it WILL delete your history (not the files, just Plex metadata). My codebase has extensive and automatic testing that protects against this type of failure, but please be safe. If you're using this tool, you might be hoarding data like me and I would HATE to see a critical bug break your metadata.**

---

## How it works

plex-hot-cache mounts a read-only FUSE filesystem **directly over** the existing media directory. Plex keeps reading from the same path it always has. When a file is opened, FUSE intercepts the request and serves it from the SSD cache if available, falling back to the network share transparently if not.

In the background, a predictor watches which files are being opened and pre-copies the next N episodes to the SSD so they're ready before Plex needs them.

```
Plex → /mnt/media (FUSE overmount)
              ├─ cache hit  → /mnt/ssd-cache/...   (fast, local)
              └─ cache miss → backing SMB/NFS mount (slow, network)
```

On shutdown, the FUSE mount is lazily detached — any streams already in progress continue uninterrupted from their open file descriptors.

---

## Caching strategies

**`cache-miss-only`** (default) — The predictor fires only when Plex opens a file that isn't cached yet. A cache hit means lookahead is working; no redundant work is done.

**`rolling-buffer`** — The predictor fires on every file open, keeping the lookahead window continuously topped up ahead of the current position. Better for unpredictable viewing patterns at the cost of slightly more churn.

Set via `trigger_strategy` in `config.toml`.

---

## Principles

- **Launch at any time.** The FUSE mount can go up or come down without restarting Plex. Streams already in flight are not interrupted on shutdown.
- **Graceful by default.** Cache corruption, copy failures, and missing files are all handled without crashing — the worst case is a cache miss that falls back to the network share.
- **Drop-in / drop-out.** No modifications to Plex or your media library. Remove the service and your media directory is exactly as it was.

---

## Quick Start

### 1. Download and extract

Grab the latest release from the [Releases page](https://github.com/DudeCmonMan/plex-hot-cache/releases) and extract it wherever you want to run it from:

```bash
tar -xzf plex-hot-cache-*.tar.gz -C /opt/plex-hot-cache
```

The release includes the binary, a default `config.toml`, and the LICENSE.

### 2. Configure

Edit `config.toml` in the same directory as the binary. At minimum, set two paths:

```toml
[paths]
target_directory = "/mnt/media"      # your existing media directory that Plex reads from
cache_directory  = "/mnt/ssd-cache"  # an SSD path for cached files
```

See the full [Settings](#settings) section below for all options.

### 3. Allow FUSE access

Plex runs as a different user, so it needs permission to access the FUSE mount:

```bash
echo "user_allow_other" | sudo tee -a /etc/fuse.conf
```

### 4. Run

```bash
cd /opt/plex-hot-cache
sudo ./plex-hot-cache
```

That's it. The cache is active and Plex doesn't need any changes. Stop it with `Ctrl+C` — the mount detaches cleanly.
You can use argument --tui to run it with a GUI that shows you status and cache.

---

## Running with systemd (recommended)

For a persistent setup that starts on boot:

```bash
sudo cp plex-hot-cache.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now plex-hot-cache
```

Verify it's running:

```bash
systemctl status plex-hot-cache
mount | grep plex-hot-cache
```

The service file is included in the release. If your media is on a network share, edit the unit to wait for the mount — see the comments inside `plex-hot-cache.service`.

---

## Building from source

If you prefer to build it yourself:

```bash
cargo build --release
sudo cp target/release/plex-hot-cache /usr/local/bin/
```

---

## Settings

### Required

| Setting | Description |
|---|---|
| `paths.target_directory` | The media directory Plex reads from (will be overmounted) |
| `paths.cache_directory` | SSD path where cached files are stored |

### Recommended

| Setting | Default | Description |
|---|---|---|
| `cache.max_size_gb` | `200.0` | Max total SSD cache size. Set to what your SSD can spare. |
| `cache.lookahead` | `4` | Episodes to pre-cache ahead of current position. |
| `cache.trigger_strategy` | `cache-miss-only` | `rolling-buffer` if you want continuous top-up. |
| `cache.max_cache_pull_gb` | `0.0` (unlimited) | Cap total cached content per prediction. Useful for large libraries on small SSDs. |
| `schedule.cache_window_start/end` | `08:00`–`02:00` | Restrict caching to specific hours to avoid SMB traffic at night. Supports midnight-wrapping windows. |
| `plex.enabled` | `false` | Set to `true` to use the Plex SQLite DB for more accurate next-episode prediction. Requires the DB path to be readable. |

### Optional

| Setting | Default | Description |
|---|---|---|
| `cache.expiry_hours` | `72` | Remove cached files not accessed within this window. |
| `cache.min_free_space_gb` | `10.0` | Stop caching if SSD free space drops below this. |
| `cache.deferred_ttl_minutes` | `1440` | Discard buffered access events (from outside the caching window) older than this on startup. |
