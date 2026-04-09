/// End-to-end IPC integration test.
///
/// Verifies the full daemon→client communication path without touching FUSE or
/// real config files:
///
/// 1. Spin up an IPC server (same code path as `fscache start`) on a temp socket.
/// 2. Connect a client (same code path as `fscache watch`) via `ipc::client::connect`.
/// 3. Verify the `Hello` message is received and fields match.
/// 4. Emit telemetry events through the broadcast channel and verify they arrive
///    and are applied to a local `DashboardState`.
/// 5. Client sends `ClientMessage::Shutdown` — verify the daemon shutdown signal fires.
/// 6. Verify the server sends `Goodbye` and exits cleanly.
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::sync::atomic::Ordering::Relaxed;
use std::time::Duration;

use tokio::sync::{broadcast, watch};

use fscache::cache::db::CacheDb;
use fscache::config::{
    CacheConfig, Config, EvictionConfig, LoggingConfig, PathsConfig, PlexConfig,
    PrefetchConfig, PresetConfig, ScheduleConfig,
};
use fscache::ipc::client;
use fscache::ipc::protocol::{
    ClientMessage, DaemonMessage, FileTarget, HelloPayload, LogLine, MountInfoWire, TelemetryEvent,
};
use fscache::ipc::server::run_ipc_server;
use fscache::ipc::{framed_split, recv_msg, send_msg};
use fscache::tui::state::DashboardState;

fn test_config() -> Arc<Config> {
    Arc::new(Config {
        paths: PathsConfig {
            target_directories: vec!["/mnt/test".to_string()],
            cache_directory: "/tmp/fscache-cache".to_string(),
            instance_name: "test-instance".to_string(),
        },
        cache:    CacheConfig::default(),
        eviction: EvictionConfig::default(),
        preset:   PresetConfig::default(),
        prefetch: PrefetchConfig::default(),
        plex:     PlexConfig::default(),
        schedule: ScheduleConfig::default(),
        logging:  LoggingConfig::default(),
    })
}

fn empty_recent() -> Arc<Mutex<VecDeque<LogLine>>> {
    Arc::new(Mutex::new(VecDeque::new()))
}

fn in_memory_db() -> Arc<CacheDb> {
    Arc::new(CacheDb::open(Path::new(":memory:")).expect("in-memory DB"))
}

fn make_hello(socket_str: &str) -> (DaemonMessage, HelloPayload) {
    let payload = HelloPayload {
        version:       "test-v0".to_string(),
        instance_name: "test-instance".to_string(),
        mounts: vec![MountInfoWire {
            target:    PathBuf::from("/mnt/test"),
            cache_dir: PathBuf::from("/tmp/fscache-cache"),
            active:    true,
        }],
        db_path: format!("{socket_str}.db"),
        config:  (*test_config()).clone(),
    };
    (DaemonMessage::Hello(payload.clone()), payload)
}

#[tokio::test]
async fn ipc_hello_received_and_fields_match() {
    let tmp = tempfile::tempdir().unwrap();
    let socket_path = tmp.path().join("test.sock");

    let (ipc_tx, _) = broadcast::channel::<DaemonMessage>(64);
    let (sd_tx, sd_rx) = watch::channel(false);

    let (hello_msg, expected) = make_hello(&socket_path.to_string_lossy());

    // Spawn server.
    let sp = socket_path.clone();
    let server_task = tokio::spawn(run_ipc_server(
        sp,
        hello_msg,
        ipc_tx,
        sd_tx,
        sd_rx,
        empty_recent(),
        in_memory_db(),
    ));

    // Give the server time to bind.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Connect client.
    let (hello, _reader, _writer) = client::connect(&socket_path).await
        .expect("connect should succeed");

    assert_eq!(hello.version,       expected.version);
    assert_eq!(hello.instance_name, expected.instance_name);
    assert_eq!(hello.mounts.len(),  1);
    assert_eq!(hello.mounts[0].target, PathBuf::from("/mnt/test"));
    assert_eq!(hello.config.preset.name,       expected.config.preset.name);
    assert_eq!(hello.config.eviction.expiry_hours, expected.config.eviction.expiry_hours);

    server_task.abort();
}

#[tokio::test]
async fn ipc_events_applied_to_dashboard_state() {
    let tmp = tempfile::tempdir().unwrap();
    let socket_path = tmp.path().join("events.sock");

    let (ipc_tx, _) = broadcast::channel::<DaemonMessage>(64);
    let (sd_tx, sd_rx) = watch::channel(false);
    let (hello_msg, _) = make_hello(&socket_path.to_string_lossy());

    let sp = socket_path.clone();
    tokio::spawn(run_ipc_server(sp, hello_msg, ipc_tx.clone(), sd_tx, sd_rx, empty_recent(), in_memory_db()));

    tokio::time::sleep(Duration::from_millis(50)).await;

    // Connect and get framed reader.
    let (_hello, mut reader, _writer) = client::connect(&socket_path).await.unwrap();

    // Start stream processor on a local DashboardState.
    let state = Arc::new(DashboardState::new(test_config()));
    let state_clone = Arc::clone(&state);
    tokio::spawn(async move {
        let _ = client::run_client_stream(&mut reader, state_clone).await;
    });

    // Broadcast events from the daemon side.
    let events = vec![
        DaemonMessage::Event(TelemetryEvent::FuseOpen),
        DaemonMessage::Event(TelemetryEvent::CacheHit),
        DaemonMessage::Event(TelemetryEvent::CacheMiss),
        DaemonMessage::Event(TelemetryEvent::CacheMiss),
        DaemonMessage::Event(TelemetryEvent::HandleClosed { bytes_read: Some(1024) }),
    ];
    for ev in events {
        let _ = ipc_tx.send(ev);
    }

    // Wait for events to propagate.
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert_eq!(state.fuse_opens.load(Relaxed),   1, "fuse_opens");
    assert_eq!(state.cache_hits.load(Relaxed),   1, "cache_hits");
    assert_eq!(state.cache_misses.load(Relaxed), 2, "cache_misses");
    assert_eq!(state.bytes_read.load(Relaxed),   1024, "bytes_read");
}

#[tokio::test]
async fn ipc_log_lines_pushed_to_ring_buffer() {
    let tmp = tempfile::tempdir().unwrap();
    let socket_path = tmp.path().join("logs.sock");

    let (ipc_tx, _) = broadcast::channel::<DaemonMessage>(64);
    let (sd_tx, sd_rx) = watch::channel(false);
    let (hello_msg, _) = make_hello(&socket_path.to_string_lossy());

    let sp = socket_path.clone();
    tokio::spawn(run_ipc_server(sp, hello_msg, ipc_tx.clone(), sd_tx, sd_rx, empty_recent(), in_memory_db()));
    tokio::time::sleep(Duration::from_millis(50)).await;

    let (_hello, mut reader, _writer) = client::connect(&socket_path).await.unwrap();
    let state = Arc::new(DashboardState::new(test_config()));
    let state_clone = Arc::clone(&state);
    tokio::spawn(async move {
        let _ = client::run_client_stream(&mut reader, state_clone).await;
    });

    let _ = ipc_tx.send(DaemonMessage::Log(LogLine {
        timestamp: "12:00:00".to_string(),
        level:     "INFO ".to_string(),
        message:   "hello from daemon".to_string(),
    }));

    tokio::time::sleep(Duration::from_millis(100)).await;

    let logs = state.recent_logs.lock().unwrap();
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0].message, "hello from daemon");
}

#[tokio::test]
async fn ipc_client_shutdown_triggers_daemon_signal() {
    let tmp = tempfile::tempdir().unwrap();
    let socket_path = tmp.path().join("shutdown.sock");

    let (ipc_tx, _) = broadcast::channel::<DaemonMessage>(64);
    let (sd_tx, mut sd_rx) = watch::channel(false);
    let (hello_msg, _) = make_hello(&socket_path.to_string_lossy());

    let sp = socket_path.clone();
    tokio::spawn(run_ipc_server(sp, hello_msg, ipc_tx, sd_tx, sd_rx.clone(), empty_recent(), in_memory_db()));
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Connect with raw framed stream so we can send ClientMessage directly.
    let stream = tokio::net::UnixStream::connect(&socket_path).await.unwrap();
    let (mut reader, mut writer) = framed_split(stream);

    // Consume Hello.
    let _hello: DaemonMessage = recv_msg(&mut reader).await.unwrap().unwrap();

    // Send Shutdown.
    send_msg(&mut writer, &ClientMessage::Shutdown).await.unwrap();

    // Wait for the watch channel to flip.
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            sd_rx.changed().await.ok();
            if *sd_rx.borrow() { break; }
        }
    })
    .await
    .expect("daemon should signal shutdown within 2 seconds");
}

#[tokio::test]
async fn ipc_goodbye_on_daemon_shutdown() {
    let tmp = tempfile::tempdir().unwrap();
    let socket_path = tmp.path().join("goodbye.sock");

    let (ipc_tx, _) = broadcast::channel::<DaemonMessage>(64);
    let (sd_tx, sd_rx) = watch::channel(false);
    let (hello_msg, _) = make_hello(&socket_path.to_string_lossy());

    let sp = socket_path.clone();
    tokio::spawn(run_ipc_server(sp, hello_msg, ipc_tx, sd_tx.clone(), sd_rx, empty_recent(), in_memory_db()));
    tokio::time::sleep(Duration::from_millis(50)).await;

    let stream = tokio::net::UnixStream::connect(&socket_path).await.unwrap();
    let (mut reader, _writer) = framed_split(stream);

    // Consume Hello.
    let first: DaemonMessage = recv_msg(&mut reader).await.unwrap().unwrap();
    assert!(matches!(first, DaemonMessage::Hello(_)));

    // Signal daemon shutdown.
    let _ = sd_tx.send(true);

    // Expect Goodbye.
    let msg = tokio::time::timeout(Duration::from_secs(2), recv_msg::<DaemonMessage>(&mut reader))
        .await
        .expect("should receive Goodbye within 2 seconds")
        .unwrap()
        .unwrap();

    assert!(matches!(msg, DaemonMessage::Goodbye), "expected Goodbye, got {msg:?}");
}

#[tokio::test]
async fn ipc_discover_finds_running_instance() {
    // discover_from_dir is used so the test does not require /run/fscache
    // to be writable (i.e. does not require root).
    let tmp = tempfile::tempdir().unwrap();
    let instance = "my-instance";
    let sp = tmp.path().join(format!("{instance}.sock"));
    let (hello_msg, _) = make_hello(&sp.to_string_lossy());

    let (ipc_tx, _) = broadcast::channel::<DaemonMessage>(64);
    let (sd_tx, sd_rx) = watch::channel(false);

    let sp_clone = sp.clone();
    let server = tokio::spawn(run_ipc_server(sp_clone, hello_msg, ipc_tx, sd_tx, sd_rx, empty_recent(), in_memory_db()));

    tokio::time::sleep(Duration::from_millis(80)).await;

    let discovered = fscache::ipc::client::discover_from_dir(tmp.path()).await;

    server.abort();

    let names: Vec<&str> = discovered.iter().map(|(n, _)| n.as_str()).collect();
    assert!(
        names.contains(&instance),
        "discover_from_dir() should find '{instance}', found: {names:?}"
    );
}

// ---------------------------------------------------------------------------
// Full pipeline: tracing → IpcBroadcastLayer → broadcast → server → socket
//                → client → DashboardState
//
// Unlike the tests above (which inject DaemonMessages directly into the
// broadcast channel), this test wires up the real tracing subscriber with
// IpcBroadcastLayer and emits tracing events the same way daemon core code
// does (tracing::info!(event = "...", ...)). It then verifies the events
// travel through the entire IPC pipeline and land in a remote DashboardState.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn full_pipeline_tracing_through_ipc_to_dashboard() {
    use fscache::ipc::broadcast_layer::IpcBroadcastLayer;
    use fscache::telemetry;
    use tracing_subscriber::prelude::*;

    let tmp = tempfile::tempdir().unwrap();
    let socket_path = tmp.path().join("pipeline.sock");

    // ---- Daemon side: real DB + cache dir for EvictFiles / RefreshLease ----
    let cache_dir = tmp.path().join("cache");
    std::fs::create_dir_all(&cache_dir).unwrap();
    let evict_rel  = PathBuf::from("evict_me.mkv");
    let refresh_rel = PathBuf::from("refresh_me.mkv");
    let evict_abs  = cache_dir.join(&evict_rel);
    let refresh_abs = cache_dir.join(&refresh_rel);
    std::fs::write(&evict_abs,  b"data").unwrap();
    std::fs::write(&refresh_abs, b"data").unwrap();
    let mount_id = cache_dir.to_string_lossy().into_owned();
    let db = in_memory_db();
    db.mark_cached(&evict_rel,  4, &mount_id);
    db.mark_cached(&refresh_rel, 4, &mount_id);
    // Back-date refresh_me so we can detect the lease renewal.
    let old_ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64 - 3600;
    db.set_last_hit_at_for_test(&refresh_rel, &mount_id, old_ts);

    let (ipc_tx, _) = broadcast::channel::<DaemonMessage>(256);
    let (sd_tx, sd_rx) = watch::channel(false);
    let (hello_msg, _) = make_hello(&socket_path.to_string_lossy());

    let recent = empty_recent();

    // Install the IpcBroadcastLayer as the thread-local tracing subscriber.
    // #[tokio::test] uses a single-threaded runtime, so every spawned task
    // runs on this thread and sees this subscriber.
    let subscriber = tracing_subscriber::registry()
        .with(IpcBroadcastLayer::new(ipc_tx.clone(), tracing::Level::INFO));
    let _guard = tracing::subscriber::set_default(subscriber);

    // ---- Start IPC server ----
    let sp = socket_path.clone();
    tokio::spawn(run_ipc_server(sp, hello_msg, ipc_tx, sd_tx, sd_rx, recent, Arc::clone(&db)));

    // Let the server bind.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // ---- Client side: connect + stream reader ----
    let (_hello, mut reader, mut writer) = client::connect(&socket_path)
        .await
        .expect("connect should succeed");

    let state = Arc::new(DashboardState::new(test_config()));
    let state_clone = Arc::clone(&state);
    tokio::spawn(async move {
        let _ = client::run_client_stream(&mut reader, state_clone).await;
    });

    // Yield so the server's per-client handler subscribes to the broadcast
    // channel before we start emitting events.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // ---- Emit real tracing events (identical to how daemon core code does it) ----
    tracing::info!(event = telemetry::EVENT_FUSE_OPEN, "FUSE open");
    tracing::info!(event = telemetry::EVENT_FUSE_OPEN, "FUSE open 2");
    tracing::info!(event = telemetry::EVENT_CACHE_HIT, path = "/mnt/test/ep01.mkv", "cache hit");
    tracing::info!(event = telemetry::EVENT_CACHE_MISS, path = "/mnt/test/ep02.mkv", "cache miss");
    tracing::info!(event = telemetry::EVENT_CACHE_MISS, path = "/mnt/test/ep03.mkv", "cache miss");
    tracing::info!(
        event = telemetry::EVENT_HANDLE_CLOSED,
        bytes_read = 4096u64,
        "handle closed"
    );
    tracing::info!(event = telemetry::EVENT_COPY_QUEUED, "copy queued");
    tracing::info!(
        event = telemetry::EVENT_COPY_STARTED,
        path = "/mnt/test/ep02.mkv",
        size_bytes = 1_000_000u64,
        "copy started"
    );
    tracing::info!(
        event = telemetry::EVENT_COPY_COMPLETE,
        path = "/mnt/test/ep02.mkv",
        "copy complete"
    );
    tracing::info!(
        event = telemetry::EVENT_COPY_FAILED,
        path = "/mnt/test/ep03.mkv",
        "copy failed"
    );
    tracing::info!(
        event = telemetry::EVENT_BUDGET_UPDATED,
        used_bytes = 5_000_000u64,
        max_bytes = 10_000_000u64,
        "budget updated"
    );
    tracing::info!(
        event = telemetry::EVENT_DEFERRED_CHANGED,
        count = 7u64,
        "deferred changed"
    );
    // caching_window is emitted at debug level by the daemon — the broadcast
    // layer must be unfiltered for telemetry events to catch it.
    tracing::debug!(
        event = telemetry::EVENT_CACHING_WINDOW,
        allowed = true,
        "caching window open"
    );
    tracing::info!(
        event = telemetry::EVENT_EVICTION,
        path = "/mnt/test/old.mkv",
        reason = "expired",
        "eviction"
    );
    tracing::info!(
        event = telemetry::EVENT_EVICTION,
        path = "/mnt/test/big.mkv",
        reason = "size_limit",
        "eviction"
    );

    // ---- Wait for the full pipeline to propagate ----
    tokio::time::sleep(Duration::from_millis(200)).await;

    // ---- Verify every counter in DashboardState ----
    assert_eq!(state.fuse_opens.load(Relaxed),       2, "fuse_opens");
    assert_eq!(state.cache_hits.load(Relaxed),        1, "cache_hits");
    assert_eq!(state.cache_misses.load(Relaxed),      2, "cache_misses");
    assert_eq!(state.bytes_read.load(Relaxed),        4096, "bytes_read");
    assert_eq!(state.completed_copies.load(Relaxed),  1, "completed_copies");
    assert_eq!(state.failed_copies.load(Relaxed),     1, "failed_copies");
    assert_eq!(state.budget_used_bytes.load(Relaxed), 5_000_000, "budget_used");
    assert_eq!(state.budget_max_bytes.load(Relaxed),  10_000_000, "budget_max");
    assert_eq!(state.deferred_count.load(Relaxed),    7, "deferred_count");
    assert_eq!(state.evictions_expired.load(Relaxed), 1, "evictions_expired");
    assert_eq!(state.evictions_size.load(Relaxed),    1, "evictions_size");
    assert!(
        state.caching_allowed.load(Relaxed),
        "caching_allowed should be true (debug-level event must pass through)"
    );

    // Verify that active_copies was populated on CopyStarted and cleaned up
    // on CopyComplete / CopyFailed.
    let active = state.active_copies.lock().unwrap();
    assert!(
        active.is_empty(),
        "active_copies should be empty after complete+failed, got {} entries",
        active.len()
    );
    drop(active);

    // open_handles: 2 opens, 1 close → net 1
    assert_eq!(state.open_handles.load(Relaxed), 1, "open_handles (2 opens - 1 close)");

    // ---- Verify log lines arrived (INFO-level events produce Log messages) ----
    let logs = state.recent_logs.lock().unwrap();
    // We emitted ~14 INFO-level events + 1 DEBUG (filtered out from logs).
    // Each INFO event produces both a telemetry Event AND a Log line.
    assert!(
        logs.len() >= 10,
        "expected at least 10 log entries from INFO-level tracing events, got {}",
        logs.len()
    );
    // The debug-level caching_window event should NOT appear as a Log line
    // (log_level is INFO), though it does appear as a telemetry Event.
    let has_window_log = logs.iter().any(|l| l.message.contains("caching window open"));
    assert!(
        !has_window_log,
        "debug-level caching_window should not appear as a Log line \
         (log_level=INFO), but should still produce a telemetry Event"
    );
    drop(logs);

    // ---- TUI → Daemon: EvictFiles ----
    // Send the command through the live client writer (same path the TUI uses).
    send_msg(&mut writer, &ClientMessage::EvictFiles {
        files: vec![FileTarget { rel_path: evict_rel.clone(), mount_id: mount_id.clone() }],
    }).await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;

    assert!(!evict_abs.exists(), "EvictFiles should delete the file from disk");
    let (_, rows) = db.client_files_for_mount(&mount_id);
    assert!(
        rows.iter().all(|(p, _, _, _)| *p != evict_rel),
        "EvictFiles should remove the DB row"
    );

    // ---- TUI → Daemon: RefreshLease ----
    send_msg(&mut writer, &ClientMessage::RefreshLease {
        files: vec![FileTarget { rel_path: refresh_rel.clone(), mount_id: mount_id.clone() }],
    }).await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;

    let timestamps = db.file_timestamps(&mount_id);
    let (_, lha) = timestamps[&refresh_rel];
    let lha_secs = lha.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    assert!(
        lha_secs >= now_secs - 5,
        "RefreshLease should update last_hit_at to ~now, got {lha_secs} vs now {now_secs}"
    );
}

#[tokio::test]
async fn ipc_replay_recent_logs_on_connect() {
    let tmp = tempfile::tempdir().unwrap();
    let socket_path = tmp.path().join("replay.sock");

    let (ipc_tx, _) = broadcast::channel::<DaemonMessage>(64);
    let (sd_tx, sd_rx) = watch::channel(false);
    let (hello_msg, _) = make_hello(&socket_path.to_string_lossy());

    // Pre-populate the ring buffer with log lines before any client connects.
    // Only LogLine entries are stored — telemetry events are never in the ring.
    let recent = empty_recent();
    {
        let mut buf = recent.lock().unwrap();
        for i in 0..5 {
            buf.push_back(LogLine {
                timestamp: format!("12:00:0{i}"),
                level: "INFO ".to_string(),
                message: format!("pre-connect log {i}"),
            });
        }
    }

    let sp = socket_path.clone();
    tokio::spawn(run_ipc_server(sp, hello_msg, ipc_tx, sd_tx, sd_rx, recent, in_memory_db()));
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Connect a client — it should receive the replayed log lines.
    let (_hello, mut reader, _writer) = client::connect(&socket_path).await.unwrap();
    let state = Arc::new(DashboardState::new(test_config()));
    let state_clone = Arc::clone(&state);
    tokio::spawn(async move {
        let _ = client::run_client_stream(&mut reader, state_clone).await;
    });

    tokio::time::sleep(Duration::from_millis(150)).await;

    // Verify replayed log lines arrived.
    let logs = state.recent_logs.lock().unwrap();
    assert!(
        logs.len() >= 5,
        "expected at least 5 replayed log entries, got {}",
        logs.len()
    );
    assert_eq!(logs[0].message, "pre-connect log 0");
    assert_eq!(logs[4].message, "pre-connect log 4");
}

#[tokio::test]
async fn ipc_evict_files_removes_file_and_db_row() {
    let tmp = tempfile::tempdir().unwrap();
    let socket_path = tmp.path().join("evict.sock");

    // Create a real on-disk cache file + DB row.
    let cache_dir = tmp.path().join("cache");
    std::fs::create_dir_all(&cache_dir).unwrap();
    let rel_path = PathBuf::from("ep01.mkv");
    let abs_path = cache_dir.join(&rel_path);
    std::fs::write(&abs_path, b"fake video data").unwrap();

    let mount_id = cache_dir.to_string_lossy().into_owned();
    let db = in_memory_db();
    db.mark_cached(&rel_path, 15, &mount_id);
    assert!(abs_path.exists(), "file should exist before evict");
    let (used, files) = db.client_files_for_mount(&mount_id);
    assert_eq!(files.len(), 1, "DB row should exist before evict");
    assert_eq!(used, 15);

    let (ipc_tx, _) = broadcast::channel::<DaemonMessage>(64);
    let (sd_tx, sd_rx) = watch::channel(false);
    let (hello_msg, _) = make_hello(&socket_path.to_string_lossy());

    let sp = socket_path.clone();
    tokio::spawn(run_ipc_server(sp, hello_msg, ipc_tx, sd_tx, sd_rx, empty_recent(), Arc::clone(&db)));
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Connect and send EvictFiles.
    let stream = tokio::net::UnixStream::connect(&socket_path).await.unwrap();
    let (mut reader, mut writer) = framed_split(stream);
    let _hello: DaemonMessage = recv_msg(&mut reader).await.unwrap().unwrap();

    send_msg(&mut writer, &ClientMessage::EvictFiles {
        files: vec![FileTarget { rel_path: rel_path.clone(), mount_id: mount_id.clone() }],
    }).await.unwrap();

    tokio::time::sleep(Duration::from_millis(150)).await;

    assert!(!abs_path.exists(), "file should be deleted after evict");
    let (_, files_after) = db.client_files_for_mount(&mount_id);
    assert!(files_after.is_empty(), "DB row should be removed after evict");
}

#[tokio::test]
async fn ipc_refresh_lease_updates_last_hit_at() {
    use std::time::{SystemTime, UNIX_EPOCH};

    let tmp = tempfile::tempdir().unwrap();
    let socket_path = tmp.path().join("refresh.sock");

    let cache_dir = tmp.path().join("cache");
    std::fs::create_dir_all(&cache_dir).unwrap();
    let rel_path = PathBuf::from("ep01.mkv");

    let mount_id = cache_dir.to_string_lossy().into_owned();
    let db = in_memory_db();
    db.mark_cached(&rel_path, 100, &mount_id);

    // Back-date last_hit_at to 1 hour ago so we can confirm it changes.
    let old_ts = SystemTime::now()
        .duration_since(UNIX_EPOCH).unwrap().as_secs() as i64 - 3600;
    db.set_last_hit_at_for_test(&rel_path, &mount_id, old_ts);

    let timestamps_before = db.file_timestamps(&mount_id);
    let (_, lha_before) = timestamps_before[&rel_path];
    let lha_before_secs = lha_before.duration_since(UNIX_EPOCH).unwrap().as_secs();
    assert!(lha_before_secs <= SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() - 3500,
        "last_hit_at should be in the past before refresh");

    let (ipc_tx, _) = broadcast::channel::<DaemonMessage>(64);
    let (sd_tx, sd_rx) = watch::channel(false);
    let (hello_msg, _) = make_hello(&socket_path.to_string_lossy());

    let sp = socket_path.clone();
    tokio::spawn(run_ipc_server(sp, hello_msg, ipc_tx, sd_tx, sd_rx, empty_recent(), Arc::clone(&db)));
    tokio::time::sleep(Duration::from_millis(50)).await;

    let stream = tokio::net::UnixStream::connect(&socket_path).await.unwrap();
    let (mut reader, mut writer) = framed_split(stream);
    let _hello: DaemonMessage = recv_msg(&mut reader).await.unwrap().unwrap();

    send_msg(&mut writer, &ClientMessage::RefreshLease {
        files: vec![FileTarget { rel_path: rel_path.clone(), mount_id: mount_id.clone() }],
    }).await.unwrap();

    tokio::time::sleep(Duration::from_millis(150)).await;

    let timestamps_after = db.file_timestamps(&mount_id);
    let (_, lha_after) = timestamps_after[&rel_path];
    let lha_after_secs = lha_after.duration_since(UNIX_EPOCH).unwrap().as_secs();
    let now_secs = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    assert!(
        lha_after_secs >= now_secs - 5,
        "last_hit_at should be updated to ~now after RefreshLease, got {lha_after_secs} vs now {now_secs}"
    );
}
