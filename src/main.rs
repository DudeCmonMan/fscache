mod backing_store;
mod cache;
mod config;
mod engine;
mod fuse;
mod prediction_utils;
mod preset;
mod presets;
mod telemetry;
mod tui;
mod utils;

use clap::Parser;
use fuser::{MountOption, SessionACL};
use std::path::PathBuf;
use std::sync::Arc;
use tracing_subscriber::prelude::*;

const BUILD_VERSION: &str = env!("BUILD_VERSION");

#[derive(Parser, Debug)]
#[command(
    name = "fscache",
    version = BUILD_VERSION,
    about = "Generic FUSE caching framework — transparent SSD overmount"
)]
struct Args {
    /// Path to config file (default: look next to binary or in current directory)
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Launch the interactive TUI monitoring dashboard
    #[arg(long)]
    tui: bool,
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
    let file_appender = tracing_appender::rolling::daily(&config.logging.log_directory, "fscache.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    let dashboard = if args.tui {
        let state = Arc::new(tui::state::DashboardState::new());
        let metrics = tui::metrics_layer::MetricsLayer::new(Arc::clone(&state));
        let logging = tui::logging::LoggingLayer::new(Arc::clone(&state));
        // MetricsLayer is unfiltered — it must see debug-level events like caching_window.
        let logging_filter = tracing_subscriber::EnvFilter::new(&config.logging.console_level);
        tracing_subscriber::registry()
            .with(metrics)
            .with(logging.with_filter(logging_filter))
            .with(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .with_target(true)
                    .with_writer(non_blocking)
                    .with_filter(file_filter),
            )
            .init();
        Some(state)
    } else {
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
        None
    };

    tracing::info!("fscache {} starting", BUILD_VERSION);
    tracing::info!("Config: {}", config_path.display());
    tracing::info!("Cache:  {}", config.paths.cache_directory);
    tracing::info!("Instance: {}", config.paths.instance_name);
    tracing::info!(
        "Log directory: {} (console={}, file={})",
        config.logging.log_directory,
        config.logging.console_level,
        config.logging.file_level
    );

    if config.cache.passthrough_mode {
        tracing::warn!("passthrough_mode = true — cache is bypassed, acting as pure proxy");
    }

    // Acquire instance lock before touching the filesystem so two daemons with the
    // same instance_name fail fast (kernel releases the lock on exit or crash).
    let _instance_lock = utils::acquire_instance_lock(&config.paths.instance_name)?;

    let targets: Vec<PathBuf> = config.paths.target_directories.iter()
        .map(|s| PathBuf::from(s))
        .collect();
    utils::validate_targets(&targets)?;

    let base_cache_dir = PathBuf::from(&config.paths.cache_directory);
    std::fs::create_dir_all(&base_cache_dir)?;

    let instance_name = &config.paths.instance_name;
    let db_dir = PathBuf::from("/var/lib/fscache/db");
    std::fs::create_dir_all(&db_dir)?;
    let db_path = db_dir.join(format!("{instance_name}.db"));
    tracing::info!("Database: {}", db_path.display());

    let db = Arc::new(cache::db::CacheDb::open(&db_path).unwrap_or_else(|e| {
        tracing::warn!("failed to open cache DB {}: {e} — falling back to in-memory DB", db_path.display());
        cache::db::CacheDb::open(std::path::Path::new(":memory:")).expect("in-memory DB must open")
    }));

    tracing::info!(
        "Schedule: caching allowed {} to {}",
        config.schedule.cache_window_start,
        config.schedule.cache_window_end
    );

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
        MountOption::FSName(format!("fscache-{}", config.paths.instance_name)),
    ];
    fuse_config.acl = SessionACL::All;

    struct MountHandle {
        _session: fuser::BackgroundSession,
        target: PathBuf,
    }
    let mut mounts: Vec<MountHandle> = Vec::new();
    let mut cache_managers: Vec<Arc<cache::manager::CacheManager>> = Vec::new();
    // Per-target lock files held for process lifetime — kernel releases on exit/crash.
    let mut _target_locks: Vec<std::fs::File> = Vec::new();

    for target in &targets {
        // Clean up a stale mount left by a previous crash of this same instance.
        // Safe because: we hold the instance lock (previous process is dead) and
        // the FSName matches our instance (not another live instance's mount).
        if let Some(holder) = utils::find_fscache_mount_holder(target) {
            if holder == config.paths.instance_name {
                tracing::warn!(
                    "Stale mount detected on {} from previous crash — cleaning up",
                    target.display()
                );
                let _ = std::process::Command::new("fusermount")
                    .args(["-uz", "--"])
                    .arg(target)
                    .status();
            }
        }

        let target_lock = utils::acquire_target_lock(target)?;
        _target_locks.push(target_lock);
        let mount_name = utils::mount_cache_name(target);
        let mount_cache_dir = base_cache_dir.join(&mount_name);
        std::fs::create_dir_all(&mount_cache_dir)?;

        tracing::info!("[{}] Target: {}", mount_name, target.display());
        tracing::info!("[{}] Cache:  {}", mount_name, mount_cache_dir.display());

        // Must open O_PATH fd BEFORE mounting FUSE over the target directory.
        let mut fs = fuse::fusefs::FsCache::new(target)?;
        fs.passthrough_mode = config.cache.passthrough_mode;
        fs.repeat_log_window = std::time::Duration::from_secs(config.logging.repeat_log_window_secs);

        let blocklist = config.plex.process_blocklist.clone();
        let rolling_buffer = config.plex.mode == "rolling-buffer";

        let preset: std::sync::Arc<dyn preset::CachePreset> = match config.preset.name.as_str() {
            "plex-episode-prediction" | "episode-prediction" => std::sync::Arc::new(
                presets::plex_episode_prediction::PlexEpisodePrediction::new(
                    config.plex.lookahead, blocklist, rolling_buffer,
                )
            ),
            "cache-on-miss" => std::sync::Arc::new(
                presets::cache_on_miss::CacheOnMiss::new(blocklist)
            ),
            other => {
                tracing::warn!(
                    "[{}] Unknown preset {:?}, falling back to \"plex-episode-prediction\"",
                    mount_name, other
                );
                std::sync::Arc::new(
                    presets::plex_episode_prediction::PlexEpisodePrediction::new(
                        config.plex.lookahead, blocklist, rolling_buffer,
                    )
                )
            }
        };
        fs.preset = Some(std::sync::Arc::clone(&preset));

        let cache_manager = Arc::new(cache::manager::CacheManager::new(
            mount_cache_dir.clone(),
            Arc::clone(&db),
            base_cache_dir.clone(),
            config.cache.max_size_gb,
            config.cache.expiry_hours,
            config.cache.min_free_space_gb,
        ));
        cache_manager.startup_cleanup();
        fs.cache = Some(Arc::clone(&cache_manager));

        let (access_tx, access_rx) = tokio::sync::mpsc::unbounded_channel();
        let (copy_tx, copy_rx) = tokio::sync::mpsc::channel::<engine::action::CopyRequest>(64);
        fs.access_tx = Some(access_tx);

        let backing_store = std::sync::Arc::clone(&fs.backing_store);
        let scheduler = engine::scheduler::Scheduler::new(
            &config.schedule.cache_window_start,
            &config.schedule.cache_window_end,
        )?;
        let engine = engine::action::ActionEngine::new(
            access_rx,
            copy_tx,
            Arc::clone(&cache_manager),
            Some(preset),
            scheduler,
            std::sync::Arc::clone(&backing_store),
            max_cache_pull_bytes,
            config.cache.deferred_ttl_minutes,
            config.cache.min_access_secs,
            config.cache.min_file_size_mb,
        );
        tokio::spawn(engine.run());
        tokio::spawn(engine::action::run_copier_task(
            backing_store,
            copy_rx,
            Arc::clone(&cache_manager),
        ));

        tracing::info!("[{}] Mounting FUSE over {}", mount_name, target.display());
        let session = fuser::spawn_mount2(fs, target, &fuse_config)
            .map_err(|e| anyhow::anyhow!("FUSE mount failed for {}: {e}\nHint: run as root or set 'user_allow_other' in /etc/fuse.conf", target.display()))?;

        mounts.push(MountHandle { _session: session, target: target.clone() });
        cache_managers.push(cache_manager);
    }

    tracing::info!("{} mount(s) active. Waiting for shutdown signal...", mounts.len());

    if let Some(ref state) = dashboard {
        let mut mount_list = state.mounts.lock().unwrap();
        for m in &mounts {
            mount_list.push(tui::state::MountInfo { target: m.target.clone(), active: true });
        }
        drop(mount_list);
        *state.window_start.lock().unwrap()  = config.schedule.cache_window_start.clone();
        *state.window_end.lock().unwrap()    = config.schedule.cache_window_end.clone();
        *state.preset_name.lock().unwrap()   = config.preset.name.clone();
        if max_cache_pull_bytes > 0 {
            use std::sync::atomic::Ordering::Relaxed;
            state.budget_max_bytes.store(max_cache_pull_bytes, Relaxed);
        }
    }

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    let tui_handle = if let Some(state) = dashboard {
        let tx  = shutdown_tx.clone();
        let rx  = shutdown_rx.clone();
        let cms = cache_managers.clone();
        Some(tokio::spawn(async move {
            if let Err(e) = tui::app::run(state, cms, rx, tx).await {
                eprintln!("TUI error: {e}");
            }
        }))
    } else {
        None
    };

    let mut sigterm = tokio::signal::unix::signal(
        tokio::signal::unix::SignalKind::terminate(),
    )?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => tracing::info!("Received SIGINT"),
        _ = sigterm.recv() => tracing::info!("Received SIGTERM"),
        _ = async {
            if let Some(handle) = tui_handle.as_ref() {
                let mut rx = shutdown_rx.clone();
                let _ = rx.wait_for(|&v| v).await;
                let _ = handle;
            } else {
                std::future::pending::<()>().await;
            }
        } => tracing::info!("TUI quit"),
    }

    let _ = shutdown_tx.send(true);

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
