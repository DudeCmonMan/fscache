use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Instant;

use tokio::time::{timeout, Duration};

use super::protocol::{DaemonMessage, HelloPayload, TelemetryEvent};
use super::{IpcFramedReader, IpcFramedWriter, framed_split, recv_msg};
use crate::tui::state::{CopyProgress, DashboardState};

const CONNECT_TIMEOUT_SECS: u64 = 2;

pub async fn connect(
    socket_path: &Path,
) -> anyhow::Result<(HelloPayload, IpcFramedReader, IpcFramedWriter)> {
    let stream = timeout(
        Duration::from_secs(CONNECT_TIMEOUT_SECS),
        tokio::net::UnixStream::connect(socket_path),
    )
    .await
    .map_err(|_| anyhow::anyhow!("timed out connecting to {}", socket_path.display()))?
    .map_err(|e| anyhow::anyhow!("connect to {} failed: {e}", socket_path.display()))?;

    let (mut reader, writer) = framed_split(stream);

    let hello = match recv_msg::<DaemonMessage>(&mut reader).await? {
        Some(DaemonMessage::Hello(h)) => h,
        Some(other) => anyhow::bail!("unexpected first message from daemon: {other:?}"),
        None => anyhow::bail!("daemon closed connection before sending Hello"),
    };

    Ok((hello, reader, writer))
}

/// Scan `/run/fscache/*.sock` and return metadata for every reachable daemon.
///
/// Connects briefly to each socket to read the `Hello` message, then
/// disconnects. Sockets that fail to connect (stale files) are silently skipped.
pub async fn discover() -> Vec<(String, HelloPayload)> {
    discover_from_dir(Path::new("/run/fscache")).await
}

/// Like [`discover`] but scans `dir` instead of the default `/run/fscache`.
/// Exposed for testing so tests can use a writable temp directory.
pub async fn discover_from_dir(dir: &Path) -> Vec<(String, HelloPayload)> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return vec![],
    };

    let mut found = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        // Only process *.sock files (skip *.lock, mount-*.lock, etc.).
        if !name.ends_with(".sock") {
            continue;
        }
        let instance_name = name.trim_end_matches(".sock").to_string();

        // Short-lived probe connection — read Hello only.
        match connect(&path).await {
            Ok((hello, _, _)) => found.push((instance_name, hello)),
            Err(e) => {
                tracing::debug!("IPC discover: skipping {path:?}: {e}");
            }
        }
    }
    found
}

/// Returns `Ok(())` on `Goodbye` or clean stream close, `Err` on protocol errors.
pub async fn run_client_stream(
    reader: &mut IpcFramedReader,
    state: Arc<DashboardState>,
) -> anyhow::Result<()> {
    loop {
        match recv_msg::<DaemonMessage>(reader).await? {
            Some(DaemonMessage::Event(event)) => apply_event(&event, &state),
            Some(DaemonMessage::Log(line)) => {
                state.push_log(crate::tui::state::LogEntry {
                    timestamp: line.timestamp,
                    level: line.level,
                    message: line.message,
                });
            }
            Some(DaemonMessage::Goodbye) | None => return Ok(()),
            Some(DaemonMessage::Hello(_)) => {
                // Unexpected mid-stream Hello — ignore.
            }
        }
    }
}

fn apply_event(event: &TelemetryEvent, state: &DashboardState) {
    match event {
        TelemetryEvent::FuseOpen => {
            state.fuse_opens.fetch_add(1, Relaxed);
            state.open_handles.fetch_add(1, Relaxed);
        }
        TelemetryEvent::CacheHit => {
            state.cache_hits.fetch_add(1, Relaxed);
        }
        TelemetryEvent::CacheMiss => {
            state.cache_misses.fetch_add(1, Relaxed);
        }
        TelemetryEvent::HandleClosed { bytes_read } => {
            state
                .open_handles
                .fetch_update(Relaxed, Relaxed, |v| Some(v.saturating_sub(1)))
                .ok();
            if let Some(b) = bytes_read {
                state.bytes_read.fetch_add(*b, Relaxed);
            }
        }
        TelemetryEvent::CopyQueued => {
            state.in_flight_count.fetch_add(1, Relaxed);
        }
        TelemetryEvent::CopyStarted { path, size_bytes } => {
            if let Some(path_str) = path {
                let p = PathBuf::from(path_str);
                state.active_copies.lock().unwrap().insert(
                    p.clone(),
                    CopyProgress {
                        path: p,
                        size_bytes: size_bytes.unwrap_or(0),
                        bytes_copied: 0,
                        started_at: Instant::now(),
                    },
                );
            }
        }
        TelemetryEvent::CopyProgress { path, bytes_copied, size_bytes } => {
            if let (Some(p), Some(bc)) = (path, bytes_copied) {
                let mut ac = state.active_copies.lock().unwrap();
                if let Some(cp) = ac.get_mut(std::path::Path::new(p)) {
                    cp.bytes_copied = *bc;
                    if let Some(sz) = size_bytes { cp.size_bytes = *sz; }
                }
                // If no entry exists (reconnect before CopyStarted replay), silently
                // drop — the natural lifecycle catches up within ≤500 ms.
            }
        }
        TelemetryEvent::CopyComplete { path } => {
            state.completed_copies.fetch_add(1, Relaxed);
            remove_active_copy(state, path);
        }
        TelemetryEvent::CopyFailed { path } => {
            state.failed_copies.fetch_add(1, Relaxed);
            remove_active_copy(state, path);
        }
        TelemetryEvent::DeferredChanged { count } => {
            if let Some(c) = count {
                state.deferred_count.store(*c, Relaxed);
            }
        }
        TelemetryEvent::BudgetUpdated { used_bytes, max_bytes } => {
            if let Some(u) = used_bytes {
                state.budget_used_bytes.store(*u, Relaxed);
            }
            if let Some(m) = max_bytes {
                state.budget_max_bytes.store(*m, Relaxed);
            }
        }
        TelemetryEvent::CachingWindow { allowed } => {
            state.caching_allowed.store(allowed.unwrap_or(false), Relaxed);
        }
        TelemetryEvent::Eviction { reason, .. } => match reason.as_deref() {
            Some("expired")    => { state.evictions_expired.fetch_add(1, Relaxed); }
            Some("size_limit") => { state.evictions_size.fetch_add(1, Relaxed); }
            _ => {}
        },
    }
}

fn remove_active_copy(state: &DashboardState, path: &Option<String>) {
    if let Some(path_str) = path {
        state
            .in_flight_count
            .fetch_update(Relaxed, Relaxed, |v| Some(v.saturating_sub(1)))
            .ok();
        state.active_copies.lock().unwrap().remove(Path::new(path_str));
    }
}
