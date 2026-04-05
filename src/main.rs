mod cache;
mod config;
mod copier;
mod fuse_fs;
mod inode;
mod plex_db;
mod predictor;
mod scheduler;
mod utils;

use clap::Parser;
use fuser::{MountOption, SessionACL};
use std::path::PathBuf;
use std::sync::Arc;
use tracing_subscriber::prelude::*;

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
    let args = Args::parse();

    let (config, config_path) = match args.config {
        Some(ref path) => config::load_from(path)?,
        None => config::load()?,
    };

    let console_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(&config.logging.console_level));

    let file_filter = tracing_subscriber::EnvFilter::new(&config.logging.file_level);

    std::fs::create_dir_all(&config.logging.log_directory).ok();
    let file_appender = tracing_appender::rolling::daily(&config.logging.log_directory, "plex-hot-cache.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(false)
                .with_filter(console_filter),
        )
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_target(true)
                .with_writer(non_blocking)
                .with_filter(file_filter),
        )
        .init();

    tracing::info!("plex-hot-cache {} starting", BUILD_VERSION);
    tracing::info!("Config: {}", config_path.display());
    tracing::info!("Cache:  {}", config.paths.cache_directory);
    tracing::info!(
        "Log directory: {} (console={}, file={})",
        config.logging.log_directory,
        config.logging.console_level,
        config.logging.file_level
    );

    if config.cache.passthrough_mode {
        tracing::warn!("passthrough_mode = true — cache is bypassed, acting as pure proxy");
    }

    if config.paths.target_directories.is_empty() {
        anyhow::bail!("target_directories is empty — add at least one path");
    }

    // Validate all targets before mounting any of them.
    let targets: Vec<PathBuf> = config.paths.target_directories.iter()
        .map(|s| PathBuf::from(s))
        .collect();
    for target in &targets {
        if !target.exists() {
            anyhow::bail!("target_directory does not exist: {}", target.display());
        }
    }

    let base_cache_dir = PathBuf::from(&config.paths.cache_directory);
    std::fs::create_dir_all(&base_cache_dir)?;

    let trigger_strategy = match config.cache.trigger_strategy.as_str() {
        "rolling-buffer" => fuse_fs::TriggerStrategy::RollingBuffer,
        _ => fuse_fs::TriggerStrategy::CacheMissOnly,
    };
    tracing::info!("Trigger strategy: {}", config.cache.trigger_strategy);
    tracing::info!(
        "Schedule: caching allowed {} to {}",
        config.schedule.cache_window_start,
        config.schedule.cache_window_end
    );

    let plex_db_enabled = config.plex.enabled.unwrap_or(true);
    let max_cache_pull_bytes = (config.cache.max_cache_pull_per_mount_gb * 1_073_741_824.0) as u64;
    if max_cache_pull_bytes > 0 {
        tracing::info!("Max cache pull budget per mount: {:.1} GB", config.cache.max_cache_pull_per_mount_gb);
    }

    // SessionACL::All is equivalent to 'allow_other' — lets Plex (a different user)
    // access the FUSE mount. Requires either root or 'user_allow_other' in /etc/fuse.conf.
    let mut fuse_config = fuser::Config::default();
    fuse_config.mount_options = vec![
        MountOption::RO,
        MountOption::AutoUnmount,
        MountOption::FSName("plex-hot-cache".to_string()),
    ];
    fuse_config.acl = SessionACL::All;

    struct MountHandle {
        _session: fuser::BackgroundSession,
        target: PathBuf,
    }
    let mut mounts: Vec<MountHandle> = Vec::new();

    for target in &targets {
        let mount_name = target
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("mount");
        let mount_cache_dir = base_cache_dir.join(mount_name);
        std::fs::create_dir_all(&mount_cache_dir)?;

        tracing::info!("[{}] Target: {}", mount_name, target.display());
        tracing::info!("[{}] Cache:  {}", mount_name, mount_cache_dir.display());

        // Must open O_PATH fd BEFORE mounting FUSE over the target directory.
        let mut fs = fuse_fs::PlexHotCacheFs::new(target)?;
        fs.passthrough_mode = config.cache.passthrough_mode;
        fs.repeat_log_window = std::time::Duration::from_secs(config.logging.repeat_log_window_secs);
        fs.trigger_strategy = trigger_strategy;

        let cache_manager = Arc::new(cache::CacheManager::new(
            mount_cache_dir.clone(),
            config.cache.max_cache_per_mount_gb,
            config.cache.expiry_hours,
            config.cache.min_free_space_gb,
        ));
        cache_manager.startup_cleanup();
        fs.cache = Some(Arc::clone(&cache_manager));

        let plex_db = if plex_db_enabled {
            let db = plex_db::PlexDb::open(
                std::path::Path::new(&config.plex.db_path),
                target,
            ).ok();
            if db.is_some() {
                tracing::info!("[{}] Plex DB opened: {}", mount_name, config.plex.db_path);
            } else {
                tracing::warn!("[{}] Plex DB not available, using regex fallback", mount_name);
            }
            db
        } else {
            None
        };

        let (access_tx, access_rx) = tokio::sync::mpsc::unbounded_channel();
        let (copy_tx, copy_rx) = tokio::sync::mpsc::channel::<predictor::CopyRequest>(64);
        fs.access_tx = Some(access_tx);

        let backing_fd = fs.backing_fd;
        let scheduler = scheduler::Scheduler::new(
            &config.schedule.cache_window_start,
            &config.schedule.cache_window_end,
        )?;
        let predictor_instance = predictor::Predictor::new(
            access_rx,
            copy_tx,
            Arc::clone(&cache_manager),
            config.cache.lookahead,
            plex_db,
            scheduler,
            backing_fd,
            max_cache_pull_bytes,
            mount_cache_dir,
            config.cache.deferred_ttl_minutes,
        );
        tokio::spawn(predictor_instance.run());
        tokio::spawn(predictor::run_copier_task(
            backing_fd,
            copy_rx,
            Arc::clone(&cache_manager),
        ));

        tracing::info!("[{}] Mounting FUSE over {}", mount_name, target.display());
        let session = fuser::spawn_mount2(fs, target, &fuse_config)
            .map_err(|e| anyhow::anyhow!("FUSE mount failed for {}: {e}\nHint: run as root or set 'user_allow_other' in /etc/fuse.conf", target.display()))?;

        mounts.push(MountHandle { _session: session, target: target.clone() });
    }

    tracing::info!("{} mount(s) active. Waiting for shutdown signal...", mounts.len());

    let mut sigterm = tokio::signal::unix::signal(
        tokio::signal::unix::SignalKind::terminate(),
    )?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => tracing::info!("Received SIGINT"),
        _ = sigterm.recv() => tracing::info!("Received SIGTERM"),
    }

    // Lazy unmount all: detach each mount point from the namespace immediately so
    // no new opens arrive, but Plex's already-open file descriptors remain valid.
    for mount in &mounts {
        let mount_name = mount.target.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("mount");
        tracing::info!("[{}] Lazy unmount of {} (existing streams unaffected)", mount_name, mount.target.display());
        let status = std::process::Command::new("fusermount")
            .args(["-uz", "--"])
            .arg(&mount.target)
            .status();
        match status {
            Ok(s) if s.success() => tracing::info!("[{}] Lazy unmount succeeded", mount_name),
            Ok(s) => tracing::warn!("[{}] fusermount -uz exited with {}", mount_name, s),
            Err(e) => {
                tracing::warn!("[{}] fusermount not available ({}), trying umount -l", mount_name, e);
                let _ = std::process::Command::new("umount").arg("-l").arg(&mount.target).status();
            }
        }
    }

    // Brief grace period for any in-flight FUSE reads to complete.
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    // Mounts are already detached, so Drop is a no-op or benign.
    drop(mounts);
    tracing::info!("Shutdown complete.");
    Ok(())
}
