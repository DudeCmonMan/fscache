use anyhow::Context;
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub paths: PathsConfig,
    #[serde(default)]
    pub plex: PlexConfig,
    #[serde(default)]
    pub cache: CacheConfig,
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

#[derive(Debug, Deserialize, Default)]
pub struct PlexConfig {
    #[serde(default = "default_plex_db_path")]
    pub db_path: String,
    #[serde(default)]
    pub enabled: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct CacheConfig {
    #[serde(default = "default_max_size_gb")]
    pub max_size_gb: f64,
    #[serde(default = "default_lookahead")]
    pub lookahead: usize,
    #[serde(default = "default_expiry_hours")]
    pub expiry_hours: u64,
    #[serde(default = "default_min_free_space_gb")]
    pub min_free_space_gb: f64,
    #[serde(default)]
    pub passthrough_mode: bool,
    /// When to notify the predictor. "cache-miss-only" (default) or "rolling-buffer".
    #[serde(default = "default_trigger_strategy")]
    pub trigger_strategy: String,
    /// Per-mount prediction cache budget (0.0 = unlimited).
    #[serde(default)]
    pub max_cache_pull_per_mount_gb: f64,
    /// Discard persisted deferred events older than this many minutes on startup (default 1440 = 24h).
    #[serde(default = "default_deferred_ttl_minutes")]
    pub deferred_ttl_minutes: u64,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            max_size_gb: default_max_size_gb(),
            lookahead: default_lookahead(),
            expiry_hours: default_expiry_hours(),
            min_free_space_gb: default_min_free_space_gb(),
            passthrough_mode: false,
            trigger_strategy: default_trigger_strategy(),
            max_cache_pull_per_mount_gb: 0.0,
            deferred_ttl_minutes: default_deferred_ttl_minutes(),
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
    /// Suppress repeated access/hit/miss logs for the same exact file path within this window.
    /// First open logs at INFO; subsequent opens within the window log at DEBUG.
    /// Set to 0 to disable (always log at INFO).
    #[serde(default = "default_repeat_log_window_secs")]
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

fn default_log_directory() -> String { "/var/log/plex-hot-cache".to_string() }
fn default_console_level() -> String { "info".to_string() }
fn default_file_level() -> String { "debug".to_string() }
fn default_repeat_log_window_secs() -> u64 { 60 }

fn default_plex_db_path() -> String {
    "/var/lib/plexmediaserver/Library/Application Support/Plex Media Server/Plug-in Support/Databases/com.plexapp.plugins.library.db".to_string()
}
fn default_trigger_strategy() -> String { "cache-miss-only".to_string() }
fn default_deferred_ttl_minutes() -> u64 { 1440 }
fn default_max_size_gb() -> f64 { 200.0 }
fn default_lookahead() -> usize { 4 }
fn default_expiry_hours() -> u64 { 72 }
fn default_min_free_space_gb() -> f64 { 10.0 }
fn default_window_start() -> String { "08:00".to_string() }
fn default_window_end() -> String { "02:00".to_string() }

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
