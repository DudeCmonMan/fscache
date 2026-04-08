mod backing_store;
mod cache;
mod config;
mod engine;
mod fuse;
mod ipc;
mod prediction_utils;
mod preset;
mod presets;
mod telemetry;
mod tui;
mod utils;

use std::io::{self, BufRead, Write as IoWrite};
use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use fuser::{MountOption, SessionACL};
use tracing::Level;
use tracing_subscriber::prelude::*;

const BUILD_VERSION: &str = env!("BUILD_VERSION");

#[derive(Parser, Debug)]
#[command(
    name = "fscache",
    version = BUILD_VERSION,
    about = "Generic FUSE caching framework — transparent SSD overmount"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Start the caching daemon (FUSE mounts + cache engine)
    Start {
        /// Path to config file (default: look next to binary or in current directory)
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
    /// Attach the TUI monitoring dashboard to a running daemon
    Watch {
        /// Instance name to connect to (resolves to /run/fscache/{name}.sock)
        #[arg(short, long)]
        instance: Option<String>,
        /// Direct path to the daemon's Unix socket
        #[arg(long)]
        socket: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Start { config } => run_daemon(config).await,
        Command::Watch { instance, socket } => run_watch(instance, socket).await,
    }
}

// ---------------------------------------------------------------------------
// Daemon (fscache start)
// ---------------------------------------------------------------------------

async fn run_daemon(config_path: Option<PathBuf>) -> anyhow::Result<()> {
    let (config, cfg_path) = match config_path {
        Some(ref path) => config::load_from(path)?,
        None => config::load()?,
    };

    // Capacity 1024: lagged TUI clients miss intermediate counters but self-correct.
    let (ipc_tx, _) = tokio::sync::broadcast::channel::<ipc::protocol::DaemonMessage>(1024);

    let ipc_log_level: Level = config
        .logging
        .console_level
        .parse()
        .unwrap_or(Level::INFO);

    let recent_logs: std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<ipc::protocol::DaemonMessage>>>
        = std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new()));

    let console_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(&config.logging.console_level));
    let file_filter = tracing_subscriber::EnvFilter::new(&config.logging.file_level);

    std::fs::create_dir_all(&config.logging.log_directory).ok();
    let file_appender = tracing_appender::rolling::daily(&config.logging.log_directory, "fscache.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    // Three equal subscribers to the same tracing events:
    //   1. fmt → console
    //   2. fmt → rolling log file
    //   3. IpcBroadcastLayer → connected TUI clients (via Unix socket)
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
        // IpcBroadcastLayer is unfiltered for telemetry events (it needs debug-level
        // `caching_window`) but applies `ipc_log_level` for log forwarding internally.
        .with(ipc::broadcast_layer::IpcBroadcastLayer::new(
            ipc_tx.clone(),
            ipc_log_level,
            recent_logs.clone(),
        ))
        .init();

    tracing::info!("fscache {} starting", BUILD_VERSION);
    tracing::info!("Config: {}", cfg_path.display());
    tracing::info!("Cache:  {}", config.paths.cache_directory);
    tracing::info!("Instance: {}", config.paths.instance_name);

    if config.cache.passthrough_mode {
        tracing::warn!("passthrough_mode = true — cache is bypassed, acting as pure proxy");
    }

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
        tracing::warn!(
            "failed to open cache DB {}: {e} — falling back to in-memory DB",
            db_path.display()
        );
        cache::db::CacheDb::open(std::path::Path::new(":memory:"))
            .expect("in-memory DB must open")
    }));

    let eviction = config::EvictionConfig::resolve(&config.eviction, &config.cache);

    let max_cache_pull_bytes =
        (config.cache.max_cache_pull_per_mount_gb * 1_073_741_824.0) as u64;

    // Shared with IPC server so TUI clients can request daemon shutdown.
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);

    let socket_path = ipc::server::socket_path(instance_name);

    let mount_info_wire: Vec<ipc::protocol::MountInfoWire> = targets.iter()
        .map(|target| {
            let cache_dir = base_cache_dir.join(utils::mount_cache_name(target));
            ipc::protocol::MountInfoWire {
                target:    target.clone(),
                cache_dir: cache_dir.clone(),
                active:    true,
            }
        })
        .collect();

    let hello = ipc::protocol::DaemonMessage::Hello(ipc::protocol::HelloPayload {
        version:        BUILD_VERSION.to_string(),
        instance_name:  instance_name.clone(),
        mounts:         mount_info_wire,
        window_start:   config.schedule.cache_window_start.clone(),
        window_end:     config.schedule.cache_window_end.clone(),
        preset_name:    config.preset.name.clone(),
        budget_max_bytes: max_cache_pull_bytes,
        min_free_bytes: (eviction.min_free_space_gb * 1_073_741_824.0) as u64,
        expiry_secs:    eviction.expiry_hours * 3600,
        db_path:        db_path.to_string_lossy().into_owned(),
        cache_directory: config.paths.cache_directory.clone(),
    });

    // Bind early (before FUSE mounts) to minimise discovery gap for `fscache watch`.
    tokio::spawn(ipc::server::run_ipc_server(
        socket_path,
        hello,
        ipc_tx,
        shutdown_tx.clone(),
        shutdown_rx.clone(),
        recent_logs,
        Arc::clone(&db),
        base_cache_dir.clone(),
    ));

    tracing::info!(
        "Schedule: caching allowed {} to {}",
        config.schedule.cache_window_start,
        config.schedule.cache_window_end
    );

    let mut fuse_config = fuser::Config::default();
    fuse_config.mount_options = vec![
        MountOption::RO,
        MountOption::AutoUnmount,
        MountOption::FSName(format!("fscache-{}", instance_name)),
    ];
    fuse_config.acl = SessionACL::All;

    struct MountHandle {
        _session: fuser::BackgroundSession,
        target:   PathBuf,
    }
    let mut mounts:         Vec<MountHandle> = Vec::new();
    let mut _target_locks:  Vec<std::fs::File> = Vec::new();

    for target in &targets {
        if let Some(holder) = utils::find_fscache_mount_holder(target) {
            if holder == *instance_name {
                tracing::warn!(
                    "Stale mount on {} from previous crash — cleaning up",
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

        let mount_name     = utils::mount_cache_name(target);
        let mount_cache_dir = base_cache_dir.join(&mount_name);
        std::fs::create_dir_all(&mount_cache_dir)?;

        tracing::info!("[{}] Target: {}", mount_name, target.display());
        tracing::info!("[{}] Cache:  {}", mount_name, mount_cache_dir.display());

        let mut fs = fuse::fusefs::FsCache::new(target)?;
        fs.passthrough_mode  = config.cache.passthrough_mode;
        fs.repeat_log_window = std::time::Duration::from_secs(
            config.logging.repeat_log_window_secs,
        );

        let plex_blocklist  = config.plex.process_blocklist.clone();
        let rolling_buffer  = config.plex.mode == "rolling-buffer";

        let preset: Arc<dyn preset::CachePreset> = match config.preset.name.as_str() {
            "plex-episode-prediction" | "episode-prediction" => Arc::new(
                presets::plex_episode_prediction::PlexEpisodePrediction::new(
                    config.plex.lookahead, plex_blocklist, rolling_buffer,
                ),
            ),
            "prefetch" => {
                let mode = presets::prefetch::parse_mode(&config.prefetch.mode)
                    .map_err(|e| anyhow::anyhow!("[{}] {}", mount_name, e))?;
                Arc::new(
                    presets::prefetch::Prefetch::new(
                        mode,
                        config.prefetch.max_depth,
                        config.prefetch.process_blocklist.clone(),
                        &config.prefetch.file_whitelist,
                        &config.prefetch.file_blacklist,
                    )
                    .map_err(|e| anyhow::anyhow!("[{}] prefetch preset config error: {}", mount_name, e))?,
                )
            }
            "cache-on-miss" => {
                tracing::warn!(
                    "[{}] preset \"cache-on-miss\" is deprecated — \
                     use preset.name = \"prefetch\" with mode = \"cache-hit-only\"",
                    mount_name
                );
                Arc::new(
                    presets::prefetch::Prefetch::new(
                        presets::prefetch::PrefetchMode::CacheHitOnly,
                        3,
                        config.prefetch.process_blocklist.clone(),
                        &[],
                        &[],
                    )
                    .expect("cache-hit-only with no patterns always succeeds"),
                )
            }
            other => {
                tracing::warn!(
                    "[{}] Unknown preset {:?}, falling back to \"plex-episode-prediction\"",
                    mount_name, other
                );
                Arc::new(
                    presets::plex_episode_prediction::PlexEpisodePrediction::new(
                        config.plex.lookahead, plex_blocklist, rolling_buffer,
                    ),
                )
            }
        };
        fs.preset = Some(Arc::clone(&preset));

        let cache_manager = Arc::new(cache::manager::CacheManager::new(
            mount_cache_dir.clone(),
            Arc::clone(&db),
            base_cache_dir.clone(),
            eviction.max_size_gb,
            eviction.expiry_hours,
            eviction.min_free_space_gb,
        ));
        cache_manager.startup_cleanup();
        fs.cache = Some(Arc::clone(&cache_manager));

        let (access_tx, access_rx) =
            tokio::sync::mpsc::unbounded_channel();
        let (copy_tx, copy_rx) =
            tokio::sync::mpsc::channel::<engine::action::CopyRequest>(64);
        fs.access_tx = Some(access_tx);

        let backing_store = Arc::clone(&fs.backing_store);
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
            Arc::clone(&backing_store),
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
        let session = fuser::spawn_mount2(fs, target, &fuse_config).map_err(|e| {
            anyhow::anyhow!(
                "FUSE mount failed for {}: {e}\n\
                 Hint: run as root or set 'user_allow_other' in /etc/fuse.conf",
                target.display()
            )
        })?;

        mounts.push(MountHandle { _session: session, target: target.clone() });
    }

    tracing::info!("{} mount(s) active. Waiting for shutdown signal...", mounts.len());

    let mut sigterm = tokio::signal::unix::signal(
        tokio::signal::unix::SignalKind::terminate(),
    )?;

    tokio::select! {
        _ = tokio::signal::ctrl_c()  => tracing::info!("Received SIGINT"),
        _ = sigterm.recv()           => tracing::info!("Received SIGTERM"),
        Ok(_) = shutdown_rx.changed() => {
            if *shutdown_rx.borrow() {
                tracing::info!("Shutdown requested via IPC");
            }
        }
    }

    let _ = shutdown_tx.send(true);

    for mount in &mounts {
        let mount_name = mount.target.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("mount");
        tracing::info!(
            "[{}] Lazy unmount of {} (existing streams unaffected)",
            mount_name,
            mount.target.display()
        );
        let status = std::process::Command::new("fusermount")
            .args(["-uz", "--"])
            .arg(&mount.target)
            .status();
        match status {
            Ok(s) if s.success() => {}
            Ok(s) => tracing::warn!("[{}] fusermount -uz exited with {}", mount_name, s),
            Err(e) => {
                tracing::warn!(
                    "[{}] fusermount not available ({}), trying umount -l",
                    mount_name, e
                );
                let _ = std::process::Command::new("umount")
                    .arg("-l")
                    .arg(&mount.target)
                    .status();
            }
        }
    }

    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    drop(mounts);
    tracing::info!("Shutdown complete.");
    Ok(())
}

// ---------------------------------------------------------------------------
// Watch client (fscache watch)
// ---------------------------------------------------------------------------

async fn run_watch(
    instance: Option<String>,
    socket: Option<PathBuf>,
) -> anyhow::Result<()> {
    let socket_path = resolve_socket(instance, socket).await?;
    tui::app::run_client(socket_path).await
}

async fn resolve_socket(
    instance: Option<String>,
    socket: Option<PathBuf>,
) -> anyhow::Result<PathBuf> {
    if let Some(path) = socket {
        return Ok(path);
    }
    if let Some(name) = instance {
        return Ok(ipc::server::socket_path(&name));
    }

    let found = ipc::client::discover().await;

    match found.len() {
        0 => {
            anyhow::bail!(
                "No running fscache instances found.\n\
                 Hint: start a daemon with `fscache start` or specify \
                 `-i INSTANCE` / `--socket PATH`."
            );
        }
        1 => {
            let (name, hello) = &found[0];
            eprintln!("Connecting to instance '{name}' ({} mount(s))", hello.mounts.len());
            Ok(ipc::server::socket_path(name))
        }
        _ => {
            eprintln!("Found {} running fscache instances:\n", found.len());
            for (i, (name, hello)) in found.iter().enumerate() {
                let targets: Vec<String> = hello.mounts.iter()
                    .map(|m| m.target.to_string_lossy().into_owned())
                    .collect();
                eprintln!(
                    "  {}. {:<20}  {} mount(s)  {}",
                    i + 1,
                    name,
                    hello.mounts.len(),
                    targets.join(", "),
                );
            }
            eprint!("\nSelect instance [1-{}]: ", found.len());
            io::stderr().flush()?;

            let mut line = String::new();
            io::stdin().lock().read_line(&mut line)?;
            let choice: usize = line.trim().parse()
                .map_err(|_| anyhow::anyhow!("invalid selection"))?;

            if choice < 1 || choice > found.len() {
                anyhow::bail!("selection out of range");
            }

            let name = &found[choice - 1].0;
            Ok(ipc::server::socket_path(name))
        }
    }
}
