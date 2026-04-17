/// End-to-end integration tests for the process discovery pipeline.
///
/// Exercises DiscoveryController in isolation (no real FUSE) against a real
/// CacheDb, and also exercises the DiscoveryStart/DiscoveryStop IPC round-trip
/// via a live run_ipc_server.
///
/// Tests are single-threaded (#[tokio::test] default) so tracing::subscriber::set_default
/// works correctly for tracing-capture tests.
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tempfile::TempDir;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::prelude::*;

use fscache::cache::db::{CacheDb, ProcessAccessRow};
use fscache::config::{
    CacheConfig, Config, DiscoveryConfig, EvictionConfig, InvalidationConfig, LoggingConfig,
    PathsConfig, PlexConfig, PrefetchConfig, PresetConfig, ScheduleConfig,
};
use fscache::discovery::{DiscoveryController, DiscoveryFormatter, OpKind, now_unix_sec};
use fscache::fuse::fusefs::OpenOutcome;
use fscache::ipc::protocol::{
    ClientMessage, DaemonMessage, HelloPayload, LogLine, MountInfoWire, TelemetryEvent,
};
use fscache::ipc::server::run_ipc_server;
use fscache::ipc::{framed_split, recv_msg, send_msg};
use fscache::preset::ProcessInfo;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn in_memory_db() -> Arc<CacheDb> {
    Arc::new(CacheDb::open(Path::new(":memory:")).expect("in-memory DB"))
}

fn disk_db(tmp: &TempDir) -> (Arc<CacheDb>, PathBuf) {
    let path = tmp.path().join("discovery.db");
    (Arc::new(CacheDb::open(&path).unwrap()), path)
}

fn empty_recent() -> Arc<Mutex<VecDeque<LogLine>>> {
    Arc::new(Mutex::new(VecDeque::new()))
}

fn fake_process(name: &str, pid: u32) -> ProcessInfo {
    ProcessInfo {
        pid,
        name: Some(name.to_string()),
        cmdline: Some(format!("{name}\0--flag\0").into_bytes()),
        ancestors: vec!["systemd".to_string()],
    }
}

/// Returns (controller, broadcast_sender, root_token).
fn make_controller(
    config: DiscoveryConfig,
    db: Arc<CacheDb>,
) -> (Arc<DiscoveryController>, broadcast::Sender<DaemonMessage>, CancellationToken) {
    let (tx, _rx) = broadcast::channel(64);
    let root = CancellationToken::new();
    let ctrl = DiscoveryController::new(
        config,
        db,
        Arc::new(vec![]),
        root.clone(),
        tx.clone(),
    );
    (ctrl, tx, root)
}

fn default_config() -> DiscoveryConfig {
    DiscoveryConfig {
        enabled: false,
        window_days: 7,
        bucket_interval_secs: 60,
        pid_lru_capacity: 512,
        pid_lru_ttl_secs: 300,
    }
}

/// A writer that appends to a shared buffer — used for tracing capture.
struct BufWriter(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for BufWriter {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
}

/// Install a thread-local tracing subscriber that captures `fscache::discovery`
/// events via `DiscoveryFormatter`. Returns (buf, guard); keep guard alive for
/// the duration of capture.
fn capture_discovery_trace() -> (Arc<Mutex<Vec<u8>>>, tracing::subscriber::DefaultGuard) {
    let buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let buf2 = Arc::clone(&buf);
    let make_writer = move || BufWriter(Arc::clone(&buf2));
    let layer = tracing_subscriber::fmt::layer()
        .with_writer(make_writer)
        .with_ansi(false)
        .event_format(DiscoveryFormatter)
        .with_filter(
            tracing_subscriber::filter::Targets::new()
                .with_target("fscache::discovery", tracing::Level::DEBUG),
        );
    let subscriber = tracing_subscriber::registry().with(layer);
    let guard = tracing::subscriber::set_default(subscriber);
    (buf, guard)
}

fn captured_lines(buf: &Arc<Mutex<Vec<u8>>>) -> Vec<String> {
    let data = buf.lock().unwrap().clone();
    String::from_utf8_lossy(&data)
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.to_string())
        .collect()
}

/// Drain a broadcast::Receiver until a DiscoveryStatus event arrives.
/// Panics if no matching event arrives within 500ms.
async fn next_discovery_status(
    rx: &mut broadcast::Receiver<DaemonMessage>,
) -> TelemetryEvent {
    tokio::time::timeout(Duration::from_millis(500), async {
        loop {
            match rx.recv().await {
                Ok(DaemonMessage::Event(ev @ TelemetryEvent::DiscoveryStatus { .. })) => {
                    return ev;
                }
                Ok(_) => continue,
                Err(broadcast::error::RecvError::Closed) => panic!("channel closed"),
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
            }
        }
    })
    .await
    .expect("expected DiscoveryStatus within 500ms")
}

// ---------------------------------------------------------------------------
// Helper for IPC tests
// ---------------------------------------------------------------------------

fn test_config() -> Arc<Config> {
    Arc::new(Config {
        paths: PathsConfig {
            target_directories: vec!["/mnt/test".to_string()],
            cache_directory:    "/tmp/fscache-cache".to_string(),
            instance_name:      "test-discovery".to_string(),
        },
        cache:        CacheConfig::default(),
        eviction:     EvictionConfig::default(),
        preset:       PresetConfig::default(),
        prefetch:     PrefetchConfig::default(),
        plex:         PlexConfig::default(),
        schedule:     ScheduleConfig::default(),
        logging:      LoggingConfig::default(),
        invalidation: InvalidationConfig::default(),
        discovery:    DiscoveryConfig::default(),
    })
}

fn make_hello(socket_str: &str) -> DaemonMessage {
    let payload = HelloPayload {
        version:       "test-v0".to_string(),
        instance_name: "test-discovery".to_string(),
        mounts: vec![MountInfoWire {
            target:    PathBuf::from("/mnt/test"),
            cache_dir: PathBuf::from("/tmp/fscache-cache"),
            active:    true,
        }],
        db_path: format!("{socket_str}.db"),
        config:  (*test_config()).clone(),
    };
    DaemonMessage::Hello(payload)
}

// ---------------------------------------------------------------------------
// Test 1: start/stop toggles enabled flag and broadcasts DiscoveryStatus
// ---------------------------------------------------------------------------

#[tokio::test]
async fn start_stop_toggles_status_and_broadcasts() {
    let (ctrl, tx, _root) = make_controller(default_config(), in_memory_db());
    let mut rx = tx.subscribe();

    assert!(!ctrl.status().enabled, "should start disabled");

    ctrl.start().unwrap();
    assert!(ctrl.status().enabled, "should be enabled after start()");

    let ev = next_discovery_status(&mut rx).await;
    assert!(
        matches!(ev, TelemetryEvent::DiscoveryStatus { enabled: true, started_at: Some(_), .. }),
        "expected enabled=true broadcast after start(), got {ev:?}",
    );

    ctrl.stop();
    assert!(!ctrl.status().enabled, "should be disabled after stop()");

    let ev = next_discovery_status(&mut rx).await;
    assert!(
        matches!(ev, TelemetryEvent::DiscoveryStatus { enabled: false, .. }),
        "expected enabled=false broadcast after stop(), got {ev:?}",
    );
}

// ---------------------------------------------------------------------------
// Test 2: disabled controller is a no-op
// ---------------------------------------------------------------------------

#[tokio::test]
async fn disabled_controller_is_noop() {
    let (buf, _guard) = capture_discovery_trace();
    let db = in_memory_db();
    let (ctrl, _, _root) = make_controller(default_config(), Arc::clone(&db));

    // Never call start() — controller remains disabled.
    let pid = std::process::id();
    ctrl.log_touch(pid, OpKind::Meta);
    ctrl.log_touch(pid, OpKind::Hit);
    ctrl.log_open(&fake_process("cat", pid), OpenOutcome::Hit);

    // Give any hypothetical async work time to settle.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let rows = db.top_processes(0, None, 10).unwrap();
    assert!(rows.is_empty(), "no DB rows should exist when controller is disabled");

    let lines = captured_lines(&buf);
    let has_new = lines.iter().any(|l| l.contains("NEW"));
    assert!(!has_new, "no NEW tracing lines should appear when disabled");
}

// ---------------------------------------------------------------------------
// Test 3: log_open emits NEW exactly once per process, even with multiple calls
// ---------------------------------------------------------------------------

#[tokio::test]
async fn log_open_emits_new_once_per_process() {
    let (buf, _guard) = capture_discovery_trace();
    let (ctrl, _, _root) = make_controller(default_config(), in_memory_db());

    ctrl.start().unwrap();

    let proc = fake_process("cat", 42);
    ctrl.log_open(&proc, OpenOutcome::Hit);
    ctrl.log_open(&proc, OpenOutcome::Hit);
    ctrl.log_open(&proc, OpenOutcome::Hit);

    ctrl.stop();
    // stop() cancels child_token → drain_loop flushes bucket before returning.
    tokio::time::sleep(Duration::from_millis(100)).await;

    let lines = captured_lines(&buf);
    let new_lines: Vec<_> = lines.iter().filter(|l| l.contains("NEW")).collect();
    assert_eq!(new_lines.len(), 1, "expected exactly one NEW line, got: {new_lines:?}");
    assert!(new_lines[0].contains("cat"), "NEW line should mention process name 'cat'");

    let snap_lines: Vec<_> = lines.iter().filter(|l| l.contains("SNAP")).collect();
    assert!(!snap_lines.is_empty(), "expected at least one SNAP line after stop flush");
    // The SNAP line for cat should be present.
    assert!(
        snap_lines.iter().any(|l| l.contains("cat")),
        "SNAP line should mention 'cat'; got: {snap_lines:?}",
    );
}

// ---------------------------------------------------------------------------
// Test 4: drain_loop writes rows to DB after bucket_interval_secs
// ---------------------------------------------------------------------------

#[tokio::test]
async fn drain_writes_rows_to_db() {
    let db = in_memory_db();
    let (ctrl, _, root) = make_controller(
        DiscoveryConfig {
            bucket_interval_secs: 1,
            ..default_config()
        },
        Arc::clone(&db),
    );

    ctrl.start().unwrap();

    ctrl.log_open(&fake_process("plex_scanner", 10), OpenOutcome::Miss);
    ctrl.log_open(&fake_process("plex_scanner", 10), OpenOutcome::Miss);
    ctrl.log_open(&fake_process("cat", 11), OpenOutcome::Hit);

    // Wait for the drain tick (1s interval + buffer).
    tokio::time::sleep(Duration::from_millis(1400)).await;

    let since = now_unix_sec() as i64 - 60;
    let rows = db.top_processes(since, None, 10).unwrap();
    assert!(!rows.is_empty(), "DB should have rows after drain tick");

    let scanner = rows.iter().find(|r| r.process_name == "plex_scanner");
    let cat = rows.iter().find(|r| r.process_name == "cat");

    assert!(scanner.is_some(), "plex_scanner should appear in DB; rows: {}", rows.iter().map(|r| &r.process_name).cloned().collect::<Vec<_>>().join(", "));
    assert_eq!(scanner.unwrap().miss, 2, "plex_scanner miss count should be 2");

    assert!(cat.is_some(), "cat should appear in DB");
    assert_eq!(cat.unwrap().hit, 1, "cat hit count should be 1");

    root.cancel();
}

// ---------------------------------------------------------------------------
// Test 5: tracing lines contain the right columns for NEW and SNAP
// ---------------------------------------------------------------------------

#[tokio::test]
async fn snap_and_new_tracing_format() {
    let (buf, _guard) = capture_discovery_trace();
    let (ctrl, _, _root) = make_controller(default_config(), in_memory_db());

    ctrl.start().unwrap();

    ctrl.log_open(&fake_process("vlc", 1), OpenOutcome::Hit);
    ctrl.log_open(&fake_process("ffmpeg", 2), OpenOutcome::Miss);

    ctrl.stop();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let lines = captured_lines(&buf);

    let new_names: Vec<_> = lines.iter()
        .filter(|l| l.contains("NEW"))
        .map(|l| l.clone())
        .collect();
    assert_eq!(new_names.len(), 2, "expected 2 NEW lines (one per process); got {new_names:?}");
    assert!(new_names.iter().any(|l| l.contains("vlc")));
    assert!(new_names.iter().any(|l| l.contains("ffmpeg")));

    let snap_lines: Vec<_> = lines.iter().filter(|l| l.contains("SNAP")).collect();
    assert!(!snap_lines.is_empty(), "expected SNAP lines after stop flush");

    // Each SNAP line should have at least 5 whitespace-delimited non-empty tokens
    // (time, "SNAP", process_name, blocked_marker, hit, miss, meta, total).
    for snap in &snap_lines {
        let tokens: Vec<_> = snap.split_whitespace().collect();
        assert!(
            tokens.len() >= 5,
            "SNAP line should have at least 5 tokens; line: {snap:?}",
        );
        // The count fields (tokens[4..]) should parse as numbers.
        for tok in &tokens[4..] {
            tok.parse::<u64>().unwrap_or_else(|_| panic!("SNAP token '{tok}' is not numeric in line: {snap:?}"));
        }
    }
}

// ---------------------------------------------------------------------------
// Test 6: root_token cascade stops the session
// ---------------------------------------------------------------------------

#[tokio::test]
async fn root_token_cascade_stops_session() {
    let (ctrl, _, root) = make_controller(
        DiscoveryConfig {
            bucket_interval_secs: 60,
            ..default_config()
        },
        in_memory_db(),
    );

    ctrl.start().unwrap();
    assert!(ctrl.status().enabled);

    root.cancel();
    tokio::time::sleep(Duration::from_millis(200)).await;

    // After root cancellation the drain_loop exits; the continuation flips enabled=false.
    assert!(
        !ctrl.status().enabled,
        "root_token cancellation should stop the session"
    );
}

// ---------------------------------------------------------------------------
// Test 8: open_readonly can read rows written by the primary handle
// ---------------------------------------------------------------------------

#[tokio::test]
async fn open_readonly_reads_daemon_writes() {
    let tmp = tempfile::tempdir().unwrap();
    let (db, db_path) = disk_db(&tmp);

    let now = now_unix_sec() as i64;
    db.upsert_process_access(&[ProcessAccessRow {
        bucket_epoch:     now,
        process_name:     "plex_scanner".to_string(),
        op_kind:          "miss".to_string(),
        count:            42,
        sample_cmdline:   Some("plex_scanner --scan".to_string()),
        sample_ancestors: Some("systemd".to_string()),
    }])
    .unwrap();

    let ro = CacheDb::open_readonly(&db_path).unwrap();
    let rows = ro.top_processes(now - 1, None, 10).unwrap();

    assert_eq!(rows.len(), 1, "readonly handle should see 1 row");
    assert_eq!(rows[0].process_name, "plex_scanner");
    assert_eq!(rows[0].miss, 42);
    assert_eq!(rows[0].sample_cmdline.as_deref(), Some("plex_scanner --scan"));
}

// ---------------------------------------------------------------------------
// Test 9: prune_process_access removes old rows
// ---------------------------------------------------------------------------

#[tokio::test]
async fn prune_process_access_removes_old_rows() {
    let db = in_memory_db();
    let now = now_unix_sec() as i64;

    // Two stale rows + one current row.
    db.upsert_process_access(&[
        ProcessAccessRow {
            bucket_epoch: now - 3600,
            process_name: "old_proc".to_string(),
            op_kind:      "hit".to_string(),
            count:        10,
            sample_cmdline: None,
            sample_ancestors: None,
        },
        ProcessAccessRow {
            bucket_epoch: now - 7200,
            process_name: "older_proc".to_string(),
            op_kind:      "miss".to_string(),
            count:        5,
            sample_cmdline: None,
            sample_ancestors: None,
        },
        ProcessAccessRow {
            bucket_epoch: now,
            process_name: "current_proc".to_string(),
            op_kind:      "meta".to_string(),
            count:        1,
            sample_cmdline: None,
            sample_ancestors: None,
        },
    ])
    .unwrap();

    let cutoff = now - 1800;
    let pruned = db.prune_process_access(cutoff).unwrap();
    assert_eq!(pruned, 2, "should prune 2 stale rows");

    let rows = db.top_processes(0, None, 10).unwrap();
    assert_eq!(rows.len(), 1, "only current_proc should remain");
    assert_eq!(rows[0].process_name, "current_proc");
}

// ---------------------------------------------------------------------------
// Test 10: IPC DiscoveryStart/Stop/Status round-trip
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ipc_discovery_start_stop_status_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let socket_path = tmp.path().join("discovery.sock");

    let shutdown = CancellationToken::new();
    let hello_msg = make_hello(&socket_path.to_string_lossy());

    // Controller and server must share the same broadcast channel so that
    // broadcast_status() calls from the controller reach the per-client handler.
    let (ctrl, ipc_tx, _root) = make_controller(default_config(), in_memory_db());

    let sp = socket_path.clone();
    tokio::spawn(run_ipc_server(
        sp,
        hello_msg,
        ipc_tx,
        shutdown.clone(),
        empty_recent(),
        in_memory_db(),
        Arc::clone(&ctrl),
    ));
    tokio::time::sleep(Duration::from_millis(50)).await;

    let stream = tokio::net::UnixStream::connect(&socket_path).await.unwrap();
    let (mut reader, mut writer) = framed_split(stream);

    // Consume Hello.
    let hello: DaemonMessage = recv_msg(&mut reader).await.unwrap().unwrap();
    assert!(matches!(hello, DaemonMessage::Hello(_)));

    // Drain messages until we see the initial DiscoveryStatus { enabled: false }.
    let initial_status = tokio::time::timeout(Duration::from_millis(500), async {
        loop {
            let msg: DaemonMessage = recv_msg(&mut reader).await.unwrap().unwrap();
            if let DaemonMessage::Event(TelemetryEvent::DiscoveryStatus { enabled, .. }) = msg {
                return enabled;
            }
        }
    })
    .await
    .expect("expected initial DiscoveryStatus within 500ms");
    assert!(!initial_status, "initial status should be enabled=false");

    // Send DiscoveryStart.
    send_msg(&mut writer, &ClientMessage::DiscoveryStart)
        .await
        .unwrap();

    // Drain until enabled=true broadcast arrives.
    let started = tokio::time::timeout(Duration::from_millis(500), async {
        loop {
            let msg: DaemonMessage = recv_msg(&mut reader).await.unwrap().unwrap();
            if let DaemonMessage::Event(TelemetryEvent::DiscoveryStatus {
                enabled,
                started_at,
                ..
            }) = msg
            {
                if enabled {
                    return started_at;
                }
            }
        }
    })
    .await
    .expect("expected DiscoveryStatus { enabled: true } within 500ms");
    assert!(started.is_some(), "started_at should be set after DiscoveryStart");

    // Send DiscoveryStop.
    send_msg(&mut writer, &ClientMessage::DiscoveryStop).await.unwrap();

    // Drain until enabled=false broadcast arrives.
    let stopped_enabled = tokio::time::timeout(Duration::from_millis(500), async {
        loop {
            let msg: DaemonMessage = recv_msg(&mut reader).await.unwrap().unwrap();
            if let DaemonMessage::Event(TelemetryEvent::DiscoveryStatus { enabled, .. }) = msg {
                if !enabled {
                    return false;
                }
            }
        }
    })
    .await
    .expect("expected DiscoveryStatus { enabled: false } within 500ms");
    assert!(!stopped_enabled);

    shutdown.cancel();
}

// ---------------------------------------------------------------------------
// Test 11: start() is idempotent (second call is a no-op)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn start_is_idempotent() {
    let (ctrl, _, root) = make_controller(default_config(), in_memory_db());

    ctrl.start().unwrap();
    let s1 = ctrl.status();
    assert!(s1.enabled);
    let started_at = s1.started_at;

    ctrl.start().unwrap(); // second call — should be ignored
    let s2 = ctrl.status();
    assert!(s2.enabled, "still enabled after second start()");
    assert_eq!(
        s2.started_at, started_at,
        "started_at should not change on second start()"
    );

    root.cancel();
}
