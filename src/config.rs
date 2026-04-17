use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    pub paths: PathsConfig,
    #[serde(default)]
    pub cache: CacheConfig,
    #[serde(default)]
    pub eviction: EvictionConfig,
    #[serde(default)]
    pub preset: PresetConfig,
    #[serde(default)]
    pub prefetch: PrefetchConfig,
    #[serde(default)]
    pub plex: PlexConfig,
    #[serde(default)]
    pub schedule: ScheduleConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
    #[serde(default)]
    pub invalidation: InvalidationConfig,
    #[serde(default)]
    pub discovery: DiscoveryConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PathsConfig {
    pub target_directories: Vec<String>,
    pub cache_directory: String,
    /// Unique name for this instance — used as the DB filename and process lock name.
    /// Must be non-empty and contain only alphanumeric characters, hyphens, or underscores.
    pub instance_name: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CacheConfig {
    /// Deprecated: use [eviction].max_size_gb instead.
    pub max_size_gb: Option<f64>,
    /// Deprecated: use [eviction].expiry_hours instead.
    #[serde(default, deserialize_with = "de_opt_u64")]
    pub expiry_hours: Option<u64>,
    /// Deprecated: use [eviction].min_free_space_gb instead.
    pub min_free_space_gb: Option<f64>,
    #[serde(default)]
    pub passthrough_mode: bool,
    /// Per-mount prediction cache budget (0.0 = unlimited).
    #[serde(default)]
    pub max_cache_pull_per_mount_gb: f64,
    /// Discard persisted deferred events older than this many minutes on startup (default 1440 = 24h).
    #[serde(default = "default_deferred_ttl_minutes", deserialize_with = "de_u64")]
    pub deferred_ttl_minutes: u64,
    /// Minimum seconds a file must remain open before prediction triggers (0 = immediate).
    #[serde(default, deserialize_with = "de_u64")]
    pub min_access_secs: u64,
    /// Skip files below this size in MB (0 = no floor).
    #[serde(default, deserialize_with = "de_u64")]
    pub min_file_size_mb: u64,
    /// Maximum number of concurrent backing→cache copies. Default 1 preserves pre-0.4 behavior.
    #[serde(default = "default_max_concurrent_copies")]
    pub max_concurrent_copies: usize,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            max_size_gb: None,
            expiry_hours: None,
            min_free_space_gb: None,
            passthrough_mode: false,
            max_cache_pull_per_mount_gb: 0.0,
            deferred_ttl_minutes: default_deferred_ttl_minutes(),
            min_access_secs: 2,
            min_file_size_mb: 0,
            max_concurrent_copies: default_max_concurrent_copies(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EvictionConfig {
    /// Eviction strategy — only "lru" supported for now (future-proofing).
    #[serde(default = "default_eviction_strategy")]
    pub strategy: String,
    #[serde(default = "default_max_size_gb")]
    pub max_size_gb: f64,
    #[serde(default = "default_expiry_hours", deserialize_with = "de_u64")]
    pub expiry_hours: u64,
    #[serde(default = "default_min_free_space_gb")]
    pub min_free_space_gb: f64,
    /// How often the maintenance task runs, in seconds. 0 disables the loop.
    /// Default 300 = 5 minutes. Drives both cache-invalidation sweeps and
    /// regular eviction when no copies are in flight (fixes lazy-eviction gap).
    #[serde(default = "default_poll_interval_secs", deserialize_with = "de_u64")]
    pub poll_interval_secs: u64,
}

impl Default for EvictionConfig {
    fn default() -> Self {
        Self {
            strategy: default_eviction_strategy(),
            max_size_gb: default_max_size_gb(),
            expiry_hours: default_expiry_hours(),
            min_free_space_gb: default_min_free_space_gb(),
            poll_interval_secs: default_poll_interval_secs(),
        }
    }
}

/// Cache-invalidation knobs — whether to revalidate cached files against their
/// backing counterparts on every FUSE hit and/or during the periodic sweep.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct InvalidationConfig {
    /// Check cached files against their backing counterparts on every FUSE open()
    /// cache hit. Adds one backing stat() per hit — cheap locally, noticeable on
    /// remote backings (SMB/NFS). Detects staleness in real time.
    #[serde(default = "default_check_on_hit")]
    pub check_on_hit: bool,
    /// Check cached files against their backing counterparts during the periodic
    /// maintenance sweep (see eviction.poll_interval_secs). Catches files that
    /// were rewritten on the backing but haven't been accessed since.
    #[serde(default = "default_check_on_maintenance")]
    pub check_on_maintenance: bool,
}

impl Default for InvalidationConfig {
    fn default() -> Self {
        Self {
            check_on_hit: default_check_on_hit(),
            check_on_maintenance: default_check_on_maintenance(),
        }
    }
}

impl EvictionConfig {
    /// Resolve eviction settings, falling back to deprecated [cache] fields if present.
    /// Logs a deprecation warning if any [cache] eviction fields are used.
    pub fn resolve(eviction: &EvictionConfig, cache: &CacheConfig) -> ResolvedEviction {
        let mut used_legacy = false;

        let max_size_gb = if let Some(v) = cache.max_size_gb {
            used_legacy = true;
            v
        } else {
            eviction.max_size_gb
        };
        let expiry_hours = if let Some(v) = cache.expiry_hours {
            used_legacy = true;
            v
        } else {
            eviction.expiry_hours
        };
        let min_free_space_gb = if let Some(v) = cache.min_free_space_gb {
            used_legacy = true;
            v
        } else {
            eviction.min_free_space_gb
        };

        if used_legacy {
            tracing::warn!(
                "Eviction settings in [cache] are deprecated. \
                 Move max_size_gb, expiry_hours, min_free_space_gb to [eviction]."
            );
        }

        ResolvedEviction { max_size_gb, expiry_hours, min_free_space_gb }
    }
}

pub struct ResolvedEviction {
    pub max_size_gb: f64,
    pub expiry_hours: u64,
    pub min_free_space_gb: f64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PrefetchConfig {
    /// "cache-hit-only" | "cache-neighbors" | "cache-parent-recursively"
    #[serde(default = "default_prefetch_mode")]
    pub mode: String,
    /// Max directory depth for cache-parent-recursively mode.
    #[serde(default = "default_max_depth")]
    pub max_depth: usize,
    /// Process binary names (and their children) that must never trigger caching.
    #[serde(default)]
    pub process_blocklist: Vec<String>,
    /// Regex patterns (matched against filename) — only matching files are cached.
    /// Ignored if file_blacklist is non-empty and the file matches the blacklist.
    #[serde(default)]
    pub file_whitelist: Vec<String>,
    /// Regex patterns (matched against filename) — matching files are never cached.
    /// Blacklist is checked before whitelist.
    #[serde(default)]
    pub file_blacklist: Vec<String>,
}

impl Default for PrefetchConfig {
    fn default() -> Self {
        Self {
            mode: default_prefetch_mode(),
            max_depth: default_max_depth(),
            process_blocklist: Vec::new(),
            file_whitelist: Vec::new(),
            file_blacklist: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PresetConfig {
    /// Which preset to use: "plex-episode-prediction" or "prefetch".
    #[serde(default = "default_preset_name")]
    pub name: String,
}

impl Default for PresetConfig {
    fn default() -> Self {
        Self { name: default_preset_name() }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PlexConfig {
    /// Episodes to cache ahead of the current one.
    #[serde(default = "default_lookahead")]
    pub lookahead: usize,
    /// "miss-only" — predict only on cache misses (default).
    /// "rolling-buffer" — also predict on hits, keeping the next N episodes always loaded.
    #[serde(default = "default_plex_mode")]
    pub mode: String,
    /// Process binary names (and their children) that must never trigger prediction.
    #[serde(default)]
    pub process_blocklist: Vec<String>,
}

impl Default for PlexConfig {
    fn default() -> Self {
        Self {
            lookahead: default_lookahead(),
            mode: default_plex_mode(),
            process_blocklist: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ScheduleConfig {
    #[serde(default = "default_window_start")]
    pub cache_window_start: String,
    #[serde(default = "default_window_end")]
    pub cache_window_end: String,
}

impl Default for ScheduleConfig {
    fn default() -> Self {
        Self {
            cache_window_start: default_window_start(),
            cache_window_end: default_window_end(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LoggingConfig {
    #[serde(default = "default_log_directory")]
    pub log_directory: String,
    #[serde(default = "default_console_level")]
    pub console_level: String,
    #[serde(default = "default_file_level")]
    pub file_level: String,
    /// Suppress repeated access/hit/miss logs for the same path within this window.
    #[serde(default = "default_repeat_log_window_secs", deserialize_with = "de_u64")]
    pub repeat_log_window_secs: u64,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            log_directory: default_log_directory(),
            console_level: default_console_level(),
            file_level: default_file_level(),
            repeat_log_window_secs: default_repeat_log_window_secs(),
        }
    }
}

fn default_log_directory() -> String { "/var/log/fscache".to_string() }
fn default_console_level() -> String { "info".to_string() }
fn default_file_level() -> String { "debug".to_string() }
fn default_repeat_log_window_secs() -> u64 { 60 }
fn default_preset_name() -> String { "plex-episode-prediction".to_string() }
fn default_lookahead() -> usize { 4 }
fn default_plex_mode() -> String { "miss-only".to_string() }
fn default_max_concurrent_copies() -> usize { 1 }
fn default_deferred_ttl_minutes() -> u64 { 1440 }
fn default_max_size_gb() -> f64 { 200.0 }
fn default_expiry_hours() -> u64 { 72 }
fn default_min_free_space_gb() -> f64 { 10.0 }
fn default_window_start() -> String { "08:00".to_string() }
fn default_window_end() -> String { "02:00".to_string() }
fn default_eviction_strategy() -> String { "lru".to_string() }
fn default_prefetch_mode() -> String { "cache-hit-only".to_string() }
fn default_max_depth() -> usize { 3 }
fn default_poll_interval_secs() -> u64 { 300 }
fn default_check_on_hit() -> bool { false }
fn default_check_on_maintenance() -> bool { true }

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct DiscoveryConfig {
    /// Arm discovery at daemon startup (runtime toggle via `fscache discover start/stop`).
    pub enabled: bool,
    /// Retention window for DB rows and log files in days.
    pub window_days: u64,
    /// How often in-memory counts are flushed to DB and emitted as SNAP log lines.
    pub bucket_interval_secs: u64,
    /// Max entries in the per-session PID info LRU.
    pub pid_lru_capacity: usize,
    /// LRU entry TTL in seconds (guards against PID reuse).
    pub pid_lru_ttl_secs: u64,
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            window_days: 7,
            bucket_interval_secs: 60,
            pid_lru_capacity: 512,
            pid_lru_ttl_secs: 300,
        }
    }
}

/// Accept both `10` and `10.0` in u64 fields — TOML floats are silently truncated.
fn de_u64<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    use serde::Deserialize;
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum NumericU64 { Int(u64), Float(f64) }
    match NumericU64::deserialize(d)? {
        NumericU64::Int(n) => Ok(n),
        NumericU64::Float(f) => Ok(f as u64),
    }
}

fn de_opt_u64<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
    use serde::Deserialize;
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum NumericU64 { Int(u64), Float(f64) }
    Ok(match Option::<NumericU64>::deserialize(d)? {
        None => None,
        Some(NumericU64::Int(n)) => Some(n),
        Some(NumericU64::Float(f)) => Some(f as u64),
    })
}

fn validate_instance_name(name: &str) -> anyhow::Result<()> {
    if name.trim().is_empty() {
        anyhow::bail!(
            "paths.instance_name is required — choose a unique name for this instance \
             (e.g. instance_name = \"plex-movies\")"
        );
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        anyhow::bail!(
            "paths.instance_name {:?} contains invalid characters — \
             use only alphanumeric characters, hyphens, or underscores",
            name
        );
    }
    Ok(())
}

pub fn load() -> anyhow::Result<(Config, PathBuf)> {
    let path = crate::utils::find_file_near_binary("config.toml")?;
    load_from(&path).map(|(cfg, _)| (cfg, path.clone()))
}

pub fn load_from(path: &PathBuf) -> anyhow::Result<(Config, PathBuf)> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let config: Config = toml::from_str(&content)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    validate_instance_name(&config.paths.instance_name)?;
    Ok((config, path.clone()))
}
