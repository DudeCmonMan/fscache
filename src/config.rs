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
}

#[derive(Debug, Deserialize)]
pub struct PathsConfig {
    pub target_directory: String,
    pub cache_directory: String,
}

#[derive(Debug, Deserialize, Default)]
pub struct PlexConfig {
    #[serde(default = "default_plex_db_path")]
    pub db_path: String,
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
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            max_size_gb: default_max_size_gb(),
            lookahead: default_lookahead(),
            expiry_hours: default_expiry_hours(),
            min_free_space_gb: default_min_free_space_gb(),
            passthrough_mode: false,
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

fn default_plex_db_path() -> String {
    "/var/lib/plexmediaserver/Library/Application Support/Plex Media Server/Plug-in Support/Databases/com.plexapp.plugins.library.db".to_string()
}
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
