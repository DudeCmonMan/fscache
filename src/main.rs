mod backing_store;
mod cache;
mod config;
mod discovery;
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
    /// Manage process-discovery recording
    Discover {
        /// Instance name (resolves to /run/fscache/{name}.sock)
        #[arg(short, long)]
        instance: Option<String>,
        #[command(subcommand)]
        action: DiscoverAction,
    },
}

#[derive(Subcommand, Debug)]
enum DiscoverAction {
    /// Start recording process activity
    Start {
        /// Auto-stop duration (e.g. "1h", "30m"). Omit to use configured default.
        #[arg(long)]
        duration: Option<String>,
        /// Record indefinitely, overriding any configured auto-stop duration
        #[arg(long)]
        indefinite: bool,
    },
    /// Stop recording
    Stop,
    /// Show current recording status
    Status,
    /// Query historical process access data (reads DB directly — daemon optional)
    Stat {
        /// Lookback window (e.g. "5m", "1h", "24h"). Default: 1h
        window: Option<String>,
        /// Filter by operation kind: hit, miss, or meta
        #[arg(long)]
        kind: Option<String>,
        /// Show top N processes
        #[arg(long, default_value_t = 20)]
        top: usize,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Start { config } => run_daemon(config).await,
        Command::Watch { instance, socket } => run_watch(instance, socket).await,
        Command::Discover { instance, action } => run_discover(instance, action).await,
    }
}

// ---------------------------------------------------------------------------
// Daemon (fscache start)
// ---------------------------------------------------------------------------

async fn run_daemon(config_path: Option<PathBuf>) -> anyhow::Result<()> {
    let (mut config, cfg_path) = match config_path {
        Some(ref path) => config::load_from(path)?,
        None => config::load()?,
    };

    // Capacity 1024: lagged TUI clients miss intermediate counters but self-correct.
    let (ipc_tx, _) = tokio::sync::broadcast::channel::<ipc::protocol::DaemonMessage>(1024);

    let instance_name = config.paths.instance_name.clone();

    let ipc_log_level: Level = config
        .logging
        .console_level
        .parse()
        .unwrap_or(Level::INFO);

    let recent_logs: std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<ipc::protocol::LogLine>>>
        = std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new()));

    let console_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(&config.logging.console_level));
    let file_filter = tracing_subscriber::EnvFilter::new(&config.logging.file_level);

    std::fs::create_dir_all(&config.logging.log_directory).ok();
    let file_appender = tracing_appender::rolling::daily(&config.logging.log_directory, "fscache.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    let discovery_appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix(format!("{instance_name}-discovery"))
        .filename_suffix("log")
        .max_log_files(config.discovery.window_days as usize)
        .build(&config.logging.log_directory)
        .expect("failed to create discovery log appender");
    let (discovery_nonblocking, _discovery_guard) = tracing_appender::non_blocking(discovery_appender);

    // Four subscribers sharing the same tracing events:
    //   1. fmt → console
    //   2. fmt → rolling log file
    //   3. IpcBroadcastLayer → connected TUI clients (via Unix socket)
    //   4. DiscoveryFormatter → dedicated discovery log (fscache::discovery target only)
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
        ))
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(discovery_nonblocking)
                .event_format(discovery::DiscoveryFormatter)
                .with_filter(
                    tracing_subscriber::filter::Targets::new()
                        .with_target("fscache::discovery", tracing::Level::INFO)
                ),
        )
        .init();

    ipc::recent_logs::spawn_recent_logs_task(ipc_tx.subscribe(), recent_logs.clone());

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
    // Write resolved eviction values back so Hello carries effective values,
    // not the raw (possibly legacy-field) originals.
    config.eviction.max_size_gb = eviction.max_size_gb;
    config.eviction.expiry_hours = eviction.expiry_hours;
    config.eviction.min_free_space_gb = eviction.min_free_space_gb;

    let max_cache_pull_bytes =
        (config.cache.max_cache_pull_per_mount_gb * 1_073_741_824.0) as u64;

    // Shared with all background tasks so any of them (or a signal) can
    // trigger a clean shutdown. A JoinSet tracks their handles for draining.
    let shutdown_token = tokio_util::sync::CancellationToken::new();
    let mut background: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();

    // Combined blocklist for discovery (union of plex + prefetch process blocklists).
    let mut discovery_blocklist = config.plex.process_blocklist.clone();
    for name in &config.prefetch.process_blocklist {
        if !discovery_blocklist.contains(name) {
            discovery_blocklist.push(name.clone());
        }
    }
    let discovery_ctrl = discovery::DiscoveryController::new(
        config.discovery.clone(),
        Arc::clone(&db),
        Arc::new(discovery_blocklist),
        shutdown_token.clone(),
        ipc_tx.clone(),
    );

    if config.discovery.enabled {
        if let Err(e) = discovery_ctrl.start(None) {
            tracing::warn!("discovery auto-arm failed: {e}");
        }
    }

    let socket_path = ipc::server::socket_path(&instance_name);

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
        version:       BUILD_VERSION.to_string(),
        instance_name: instance_name.clone(),
        mounts:        mount_info_wire,
        db_path:       db_path.to_string_lossy().into_owned(),
        config:        config.clone(),
    });

    // Bind early (before FUSE mounts) to minimise discovery gap for `fscache watch`.
    let ipc_token    = shutdown_token.clone();
    let ipc_db       = Arc::clone(&db);
    let ipc_disc     = Arc::clone(&discovery_ctrl);
    background.spawn(async move {
        let _ = ipc::server::run_ipc_server(
            socket_path,
            hello,
            ipc_tx,
            ipc_token,
            recent_logs,
            ipc_db,
            ipc_disc,
        ).await;
    });

    // Prune old process_access rows on a regular interval.
    {
        let prune_db  = Arc::clone(&db);
        let prune_tok = shutdown_token.clone();
        let window_days = config.discovery.window_days;
        background.spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(300));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = ticker.tick() => {
                        let cutoff = discovery::now_unix_sec() as i64
                            - (window_days * 86400) as i64;
                        let _ = tokio::task::spawn_blocking({
                            let db = Arc::clone(&prune_db);
                            move || db.prune_process_access(cutoff)
                        }).await;
                    }
                    _ = prune_tok.cancelled() => return,
                }
            }
        });
    }

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
        fs.preset     = Some(Arc::clone(&preset));
        fs.discovery  = Some(Arc::clone(&discovery_ctrl));

        let cache_manager = Arc::new(cache::manager::CacheManager::new(
            mount_cache_dir.clone(),
            Arc::clone(&db),
            base_cache_dir.clone(),
            eviction.max_size_gb,
            eviction.expiry_hours,
            eviction.min_free_space_gb,
            Some(Arc::clone(&fs.backing_store)),
            &config.invalidation,
        ));
        cache_manager.startup_cleanup();
        fs.cache = Some(Arc::clone(&cache_manager));

        let (access_tx, access_rx) =
            tokio::sync::mpsc::unbounded_channel();
        fs.access_tx = Some(access_tx);

        let backing_store = Arc::clone(&fs.backing_store);

        let scheduler = engine::scheduler::Scheduler::new(
            &config.schedule.cache_window_start,
            &config.schedule.cache_window_end,
        )?;

        let (cache_io, io_handles) = cache::io::CacheIO::spawn(
            cache::io::CacheIoConfig {
                max_concurrent_copies: config.cache.max_concurrent_copies,
                eviction_interval_secs: config.eviction.poll_interval_secs,
                deferred_ttl_minutes: config.cache.deferred_ttl_minutes,
            },
            Arc::clone(&cache_manager),
            Arc::clone(&backing_store),
            scheduler,
            shutdown_token.clone(),
        );
        for h in io_handles {
            background.spawn(async move { let _ = h.await; });
        }

        let engine = engine::action::ActionEngine::new(
            access_rx,
            cache_io,
            Arc::clone(&cache_manager),
            Some(preset),
            Arc::clone(&backing_store),
            max_cache_pull_bytes,
            config.cache.min_access_secs,
            config.cache.min_file_size_mb,
        );
        background.spawn(engine.run(shutdown_token.clone()));
        if config.eviction.poll_interval_secs > 0 && config.invalidation.check_on_maintenance {
            background.spawn(engine::action::run_maintenance_task(
                Arc::clone(&cache_manager),
                config.eviction.poll_interval_secs,
                shutdown_token.clone(),
            ));
        }

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
        _ = tokio::signal::ctrl_c() => tracing::info!("Received SIGINT"),
        _ = sigterm.recv()          => tracing::info!("Received SIGTERM"),
        _ = shutdown_token.cancelled() => tracing::info!("Shutdown requested via IPC"),
    }

    shutdown_token.cancel();

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

    // Drain background tasks. Each task observes the token and breaks its loop;
    // any in-flight spawn_blocking work (one sweep / one eviction pass) is allowed
    // to finish. The 15s timeout backstops a sweep stuck on slow stat() calls.
    let drain = async {
        while let Some(res) = background.join_next().await {
            if let Err(e) = res {
                tracing::warn!("background task join error: {e}");
            }
        }
    };
    if tokio::time::timeout(std::time::Duration::from_secs(15), drain).await.is_err() {
        tracing::warn!(
            "background tasks did not drain within 15s; {} still pending",
            background.len()
        );
        background.abort_all();
    }

    // Drop mounts AFTER drain so the FUSE destroy() callback fires before
    // "Shutdown complete." is logged.
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

// ---------------------------------------------------------------------------
// Discover client (fscache discover ...)
// ---------------------------------------------------------------------------

async fn run_discover(
    instance: Option<String>,
    action: DiscoverAction,
) -> anyhow::Result<()> {
    use ipc::protocol::ClientMessage;

    if let DiscoverAction::Stat { window, kind, top } = action {
        // Direct DB read — no daemon connection needed.
        let name = resolve_instance_name(instance).await?;
        let db_path = std::path::PathBuf::from(format!("/var/lib/fscache/db/{name}.db"));
        let db = cache::db::CacheDb::open_readonly(&db_path)
            .map_err(|e| anyhow::anyhow!("failed to open DB {}: {e}", db_path.display()))?;

        let window_secs = if let Some(w) = &window {
            humantime::parse_duration(w)
                .map_err(|e| anyhow::anyhow!("invalid window {:?}: {e}", w))?
                .as_secs()
        } else {
            3600
        };
        let cutoff = discovery::now_unix_sec() as i64 - window_secs as i64;
        let rows = db.top_processes(cutoff, kind.as_deref(), top)?;

        let win_str = window.as_deref().unwrap_or("1h");
        println!("{:<32}  {:>8}  {:>8}  {:>8}  {:>8}", "PROCESS", "HIT", "MISS", "META", "TOTAL");
        println!("{}", "-".repeat(72));
        if rows.is_empty() {
            println!("(no data in the last {win_str})");
        }
        for row in &rows {
            println!("{:<32}  {:>8}  {:>8}  {:>8}  {:>8}",
                row.process_name, row.hit, row.miss, row.meta, row.total);
        }
        return Ok(());
    }

    // IPC dispatch for start / stop / status.
    let socket = resolve_socket(instance, None).await?;
    let (_hello, mut reader, mut writer) = ipc::client::connect(&socket).await?;

    match action {
        DiscoverAction::Status => {
            print_discovery_status(&mut reader).await?;
        }
        DiscoverAction::Start { duration, indefinite } => {
            let duration_secs = if indefinite {
                Some(0u64)
            } else if let Some(d) = &duration {
                Some(
                    humantime::parse_duration(d)
                        .map_err(|e| anyhow::anyhow!("invalid duration {:?}: {e}", d))?
                        .as_secs(),
                )
            } else {
                None
            };
            // Consume the initial DiscoveryStatus sent on connect before sending command.
            drain_to_discovery_status(&mut reader).await?;
            ipc::send_msg(&mut writer, &ClientMessage::DiscoveryStart { duration_secs }).await?;
            print_discovery_status(&mut reader).await?;
        }
        DiscoverAction::Stop => {
            drain_to_discovery_status(&mut reader).await?;
            ipc::send_msg(&mut writer, &ClientMessage::DiscoveryStop).await?;
            print_discovery_status(&mut reader).await?;
        }
        DiscoverAction::Stat { .. } => unreachable!(),
    }

    Ok(())
}

/// Drain incoming messages until the first DiscoveryStatus is received, then print it.
async fn print_discovery_status(reader: &mut ipc::IpcFramedReader) -> anyhow::Result<()> {
    use ipc::protocol::{DaemonMessage, TelemetryEvent};
    use tokio::time::{timeout, Duration};
    loop {
        match timeout(Duration::from_secs(3), ipc::recv_msg::<DaemonMessage>(reader)).await {
            Ok(Ok(Some(DaemonMessage::Event(TelemetryEvent::DiscoveryStatus {
                enabled, started_at, auto_stop_at,
            })))) => {
                let state = if enabled { "armed" } else { "disarmed" };
                println!("Discovery: {state}");
                if let Some(at) = started_at {
                    let elapsed = discovery::now_unix_sec() as i64 - at;
                    println!("  Started: {}s ago", elapsed.max(0));
                }
                if let Some(stop) = auto_stop_at {
                    let remaining = stop - discovery::now_unix_sec() as i64;
                    if remaining > 0 {
                        println!("  Auto-stop in: {}s", remaining);
                    } else {
                        println!("  Auto-stop: expired");
                    }
                }
                return Ok(());
            }
            Ok(Ok(Some(DaemonMessage::Goodbye))) | Ok(Ok(None)) => {
                anyhow::bail!("daemon disconnected before sending DiscoveryStatus");
            }
            Ok(Ok(Some(_))) => {} // skip Hello, Log, other Event variants
            Ok(Err(e)) => return Err(e),
            Err(_) => anyhow::bail!("timed out waiting for discovery status from daemon"),
        }
    }
}

/// Consume messages until the first DiscoveryStatus (discards it — used before sending a command).
async fn drain_to_discovery_status(reader: &mut ipc::IpcFramedReader) -> anyhow::Result<()> {
    use ipc::protocol::{DaemonMessage, TelemetryEvent};
    use tokio::time::{timeout, Duration};
    loop {
        match timeout(Duration::from_secs(3), ipc::recv_msg::<DaemonMessage>(reader)).await {
            Ok(Ok(Some(DaemonMessage::Event(TelemetryEvent::DiscoveryStatus { .. })))) => {
                return Ok(());
            }
            Ok(Ok(Some(DaemonMessage::Goodbye))) | Ok(Ok(None)) => {
                anyhow::bail!("connection closed before DiscoveryStatus");
            }
            Ok(Ok(Some(_))) => {}
            Ok(Err(e)) => return Err(e),
            Err(_) => anyhow::bail!("timed out waiting for initial discovery status"),
        }
    }
}

/// Resolve an instance name from the option or by auto-discovering running daemons.
async fn resolve_instance_name(instance: Option<String>) -> anyhow::Result<String> {
    if let Some(name) = instance {
        return Ok(name);
    }
    let found = ipc::client::discover().await;
    match found.len() {
        0 => anyhow::bail!(
            "No running fscache instances found. \
             Use --instance to specify one by name."
        ),
        1 => Ok(found[0].0.clone()),
        _ => {
            let names: Vec<_> = found.iter().map(|(n, _)| n.as_str()).collect();
            anyhow::bail!(
                "Multiple instances running: {}. Use --instance to specify one.",
                names.join(", ")
            )
        }
    }
}
