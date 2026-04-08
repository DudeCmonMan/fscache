use anyhow::Context;
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub paths: PathsConfig,
    #[serde(default)]
    pub cache: CacheConfig,
    #[serde(default)]
    pub preset: PresetConfig,
    #[serde(default)]
    pub plex: PlexConfig,
    #[serde(default)]
    pub schedule: ScheduleConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
}

#[derive(Debug, Deserialize)]
pub struct PathsConfig {
    pub target_directories: Vec<String>,
    pub cache_directory: String,
}

#[derive(Debug, Deserialize)]
pub struct CacheConfig {
    #[serde(default = "default_max_size_gb")]
    pub max_size_gb: f64,
    #[serde(default = "default_expiry_hours", deserialize_with = "de_u64")]
    pub expiry_hours: u64,
    #[serde(default = "default_min_free_space_gb")]
    pub min_free_space_gb: f64,
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
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            max_size_gb: default_max_size_gb(),
            expiry_hours: default_expiry_hours(),
            min_free_space_gb: default_min_free_space_gb(),
            passthrough_mode: false,
            max_cache_pull_per_mount_gb: 0.0,
            deferred_ttl_minutes: default_deferred_ttl_minutes(),
            min_access_secs: 0,
            min_file_size_mb: 0,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct PresetConfig {
    /// Which preset to use: "plex-episode-prediction" (default) or "cache-on-miss".
    #[serde(default = "default_preset_name")]
    pub name: String,
}

impl Default for PresetConfig {
    fn default() -> Self {
        Self { name: default_preset_name() }
    }
}

#[derive(Debug, Deserialize)]
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

#[derive(Debug, Deserialize)]
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

#[derive(Debug, Deserialize)]
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

fn default_log_directory() -> String { "/var/log/f-cache".to_string() }
fn default_console_level() -> String { "info".to_string() }
fn default_file_level() -> String { "debug".to_string() }
fn default_repeat_log_window_secs() -> u64 { 60 }
fn default_preset_name() -> String { "plex-episode-prediction".to_string() }
fn default_lookahead() -> usize { 4 }
fn default_plex_mode() -> String { "miss-only".to_string() }
fn default_deferred_ttl_minutes() -> u64 { 1440 }
fn default_max_size_gb() -> f64 { 200.0 }
fn default_expiry_hours() -> u64 { 72 }
fn default_min_free_space_gb() -> f64 { 10.0 }
fn default_window_start() -> String { "08:00".to_string() }
fn default_window_end() -> String { "02:00".to_string() }

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

pub fn load() -> anyhow::Result<(Config, PathBuf)> {
    let path = crate::utils::find_file_near_binary("config.toml")?;
    load_from(&path).map(|(cfg, _)| (cfg, path.clone()))
}

pub fn load_from(path: &PathBuf) -> anyhow::Result<(Config, PathBuf)> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let config: Config = toml::from_str(&content)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    Ok((config, path.clone()))
}
