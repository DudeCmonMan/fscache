# plex-hot-cache

Transparent SSD caching for Plex media. It sits between Plex and your network share / drive arrays / whatever other data backing using a FUSE overmount — Plex sees no difference, but episode files are silently pre-copied to local SSD before they're needed. It is meant to be run ON the Plex server itself, but theoretically could be added to your NAS since it relies solely on FUSE R/W events. Eventually I plan to have Plex Integrations directly.

No Plex plugins, no API wrappers, no config changes on the Plex side. Drop it in, point it at your media directory, and remove it just as easily. As long as it receives a proper signal that it can handle, you can stop it while your server is running. The current streams will crash and need to be restarted, but your server will not.

One of the main principles I had when developing this was that it is a simple "drop-in" hot cache binary that anyone can use. I try my best to make it crash gracefully.

I created this binary for a few reasons: 
    1) I wanted to better handle my array spin up / downs to save power (it really adds up in SoCal)
    2) To improve the viewing experience for myself
    3) No other library really did what I wanted it to do (straightforward, simple setup, simple teardown,)

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

## Setup

### 1. Build

```bash
cargo build --release
sudo cp target/release/plex-hot-cache /usr/local/bin/
```

### 2. Config

```bash
sudo mkdir /etc/plex-hot-cache
sudo cp config.toml /etc/plex-hot-cache/config.toml
```

Edit `/etc/plex-hot-cache/config.toml` — see the settings section below.

### 3. FUSE prerequisite

Allow non-root users (Plex) to access the mount:

```bash
echo "user_allow_other" | sudo tee -a /etc/fuse.conf
```

Or run the service as root (the default in the provided unit file).

### 3a. You can "just go" from here
```
sudo chmod plex-hot-cache
sudo ./ plex-hot-cache
```

This will launch the server manually and it "just works". You can use this to see if you like how it behaves.

### 4. systemd service

```bash
sudo cp plex-hot-cache.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now plex-hot-cache
```

To verify it's running and the mount is active:

```bash
systemctl status plex-hot-cache
mount | grep plex-hot-cache
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
