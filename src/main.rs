mod cache;
mod config;
mod copier;
mod fuse_fs;
mod inode;
mod utils;

// Phase 3+ stubs (uncomment as phases are implemented)
// mod predictor;
// mod plex_db;
// mod scheduler;

use clap::Parser;
use fuser::{MountOption, SessionACL};
use std::path::PathBuf;

const BUILD_VERSION: &str = env!("BUILD_VERSION");

#[derive(Parser, Debug)]
#[command(
    name = "plex-hot-cache",
    version = BUILD_VERSION,
    about = "Predictive SSD caching for Plex media — transparent FUSE overmount"
)]
struct Args {
    /// Path to config file (default: look next to binary or in current directory)
    #[arg(short, long)]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    tracing::info!("plex-hot-cache {} starting", BUILD_VERSION);

    let args = Args::parse();

    let (config, config_path) = match args.config {
        Some(ref path) => config::load_from(path)?,
        None => config::load()?,
    };

    tracing::info!("Config: {}", config_path.display());
    tracing::info!("Target: {}", config.paths.target_directory);
    tracing::info!("Cache:  {}", config.paths.cache_directory);

    if config.cache.passthrough_mode {
        tracing::warn!("passthrough_mode = true — cache is bypassed, acting as pure proxy");
    }

    // Ensure cache directory exists
    std::fs::create_dir_all(&config.paths.cache_directory)?;

    let target = PathBuf::from(&config.paths.target_directory);
    if !target.exists() {
        anyhow::bail!("target_directory does not exist: {}", target.display());
    }

    // Open O_PATH fd to target BEFORE mounting FUSE over it
    let mut fs = fuse_fs::PlexHotCacheFs::new(&target)?;
    fs.passthrough_mode = config.cache.passthrough_mode;

    // Set up SSD cache overlay.
    let cache_manager = cache::CacheManager::new(
        PathBuf::from(&config.paths.cache_directory),
        config.cache.max_size_gb,
        config.cache.expiry_hours,
        config.cache.min_free_space_gb,
    );
    cache_manager.startup_cleanup();
    fs.cache = Some(cache_manager);

    // SessionACL::All is equivalent to 'allow_other' — lets Plex (a different user)
    // access the FUSE mount. Requires either root or 'user_allow_other' in /etc/fuse.conf.
    let mut fuse_config = fuser::Config::default();
    fuse_config.mount_options = vec![
        MountOption::RO,
        MountOption::AutoUnmount,
        MountOption::FSName("plex-hot-cache".to_string()),
    ];
    fuse_config.acl = SessionACL::All;

    tracing::info!("Mounting FUSE over {}", target.display());
    let _session = fuser::spawn_mount2(fs, &target, &fuse_config)
        .map_err(|e| anyhow::anyhow!("FUSE mount failed: {e}\nHint: run as root or set 'user_allow_other' in /etc/fuse.conf"))?;

    tracing::info!("Mount active. Waiting for shutdown signal...");

    // Wait for SIGTERM or Ctrl-C
    let mut sigterm = tokio::signal::unix::signal(
        tokio::signal::unix::SignalKind::terminate(),
    )?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => tracing::info!("Received SIGINT"),
        _ = sigterm.recv() => tracing::info!("Received SIGTERM"),
    }

    // _session drops here → triggers fusermount -u, original directory reappears
    tracing::info!("Unmounted. Goodbye.");
    Ok(())
}
