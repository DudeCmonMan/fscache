use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tokio::net::UnixListener;
use tokio::sync::broadcast;
use tokio::time::{timeout, Duration};
use tokio_util::sync::CancellationToken;

use crate::cache::db::CacheDb;
use crate::discovery::DiscoveryController;
use super::protocol::{ClientMessage, DaemonMessage, LogLine};
use super::{framed_split, recv_msg, send_msg};

/// Bind a Unix domain socket at `socket_path` and serve connected TUI clients.
///
/// Each client receives a copy of `hello` on connection, then a stream of
/// broadcast events. Clients can send `ClientMessage::Shutdown` to trigger
/// daemon shutdown. Runs until `shutdown` is cancelled, then broadcasts
/// `Goodbye` and removes the socket file.
pub async fn run_ipc_server(
    socket_path: PathBuf,
    hello: DaemonMessage,
    events: broadcast::Sender<DaemonMessage>,
    shutdown: CancellationToken,
    recent: Arc<Mutex<VecDeque<LogLine>>>,
    db: Arc<CacheDb>,
    discovery: Arc<DiscoveryController>,
) -> anyhow::Result<()> {
    // Stale socket detection: if we can connect, another daemon is alive.
    // (Normally impossible since the instance lock prevents this, but be
    // defensive.)
    if let Ok(Ok(_)) = timeout(
        Duration::from_millis(200),
        tokio::net::UnixStream::connect(&socket_path),
    )
    .await
    {
        anyhow::bail!(
            "socket {} is already in use by another daemon process",
            socket_path.display()
        );
    }

    let _ = std::fs::remove_file(&socket_path);

    let listener = UnixListener::bind(&socket_path).map_err(|e| {
        anyhow::anyhow!(
            "failed to bind IPC socket at {}: {e}",
            socket_path.display()
        )
    })?;

    tracing::info!("IPC socket listening at {}", socket_path.display());

    loop {
        tokio::select! {
            accept_result = listener.accept() => {
                match accept_result {
                    Ok((stream, _)) => {
                        let hello_clone     = hello.clone();
                        let mut rx          = events.subscribe();
                        let sd              = shutdown.clone();
                        let peer_path       = socket_path.clone();
                        let recent_clone    = Arc::clone(&recent);
                        let db_clone        = Arc::clone(&db);
                        let discovery_clone = Arc::clone(&discovery);
                        tokio::spawn(async move {
                            let peer = format!("client@{}", peer_path.display());
                            if let Err(e) = handle_client(stream, hello_clone, &mut rx, sd, recent_clone, db_clone, discovery_clone).await {
                                tracing::debug!("{peer} disconnected: {e}");
                            } else {
                                tracing::debug!("{peer} disconnected cleanly");
                            }
                        });
                    }
                    Err(e) => {
                        tracing::warn!("IPC accept error: {e}");
                    }
                }
            }

            _ = shutdown.cancelled() => break,
        }
    }

    let _ = events.send(DaemonMessage::Goodbye);

    // Brief pause to allow in-flight writes to flush.
    tokio::time::sleep(Duration::from_millis(100)).await;

    let _ = std::fs::remove_file(&socket_path);
    tracing::info!("IPC socket removed");

    Ok(())
}

async fn handle_client(
    stream: tokio::net::UnixStream,
    hello: DaemonMessage,
    rx: &mut broadcast::Receiver<DaemonMessage>,
    shutdown: CancellationToken,
    recent: Arc<Mutex<VecDeque<LogLine>>>,
    db: Arc<CacheDb>,
    discovery: Arc<DiscoveryController>,
) -> anyhow::Result<()> {
    let (mut reader, mut writer) = framed_split(stream);

    send_msg(&mut writer, &hello).await?;

    // Replay recent log lines so the client doesn't start with an empty log view.
    // The broadcast subscription (rx) was created before this point, so any
    // messages arriving during replay also queue in rx — minor duplicates are
    // acceptable.
    {
        let snapshot: Vec<LogLine> = recent.lock().unwrap().iter().cloned().collect();
        for line in snapshot {
            send_msg(&mut writer, &DaemonMessage::Log(line)).await?;
        }
    }

    // Send current discovery state so watch clients get immediate status.
    {
        let s = discovery.status();
        let _ = send_msg(&mut writer, &DaemonMessage::Event(
            crate::ipc::protocol::TelemetryEvent::DiscoveryStatus {
                enabled: s.enabled,
                started_at: s.started_at,
                auto_stop_at: s.auto_stop_at,
            }
        )).await;
    }

    loop {
        tokio::select! {
            msg = rx.recv() => {
                match msg {
                    Ok(m) => {
                        let is_goodbye = matches!(m, DaemonMessage::Goodbye);
                        send_msg(&mut writer, &m).await?;
                        if is_goodbye {
                            return Ok(());
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::debug!("IPC client lagged by {n} messages");
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        return Ok(());
                    }
                }
            }

            frame = recv_msg::<ClientMessage>(&mut reader) => {
                match frame? {
                    Some(ClientMessage::Shutdown) => {
                        tracing::info!("IPC: received Shutdown command from TUI client");
                        shutdown.cancel();
                        return Ok(());
                    }
                    Some(ClientMessage::EvictFiles { files }) => {
                        for target in &files {
                            // The mount_id stored in the DB is the absolute path of the
                            // mount's cache subdirectory. The file lives at
                            // cache_directory / <subdir-name> / rel_path.
                            let abs_path = PathBuf::from(&target.mount_id).join(&target.rel_path);
                            if let Err(e) = std::fs::remove_file(&abs_path) {
                                tracing::warn!("IPC evict: failed to delete {}: {e}", abs_path.display());
                            } else {
                                tracing::info!(
                                    event = crate::telemetry::EVENT_EVICTION,
                                    path = %abs_path.display(),
                                    reason = "manual",
                                    "evict (manual): {}",
                                    abs_path.display()
                                );
                                db.remove(&target.rel_path, &target.mount_id);
                            }
                        }
                    }
                    Some(ClientMessage::RefreshLease { files }) => {
                        for target in &files {
                            db.mark_hit(&target.rel_path, &target.mount_id);
                            tracing::info!(
                                "IPC refresh lease: {} (mount {})",
                                target.rel_path.display(),
                                target.mount_id,
                            );
                        }
                    }
                    Some(ClientMessage::DiscoveryStart { duration_secs }) => {
                        if let Err(e) = discovery.start(duration_secs) {
                            tracing::warn!("IPC discovery start failed: {e}");
                        }
                    }
                    Some(ClientMessage::DiscoveryStop) => {
                        discovery.stop();
                    }
                    None => return Ok(()), // client closed connection
                }
            }
        }
    }
}

pub fn socket_path(instance_name: &str) -> PathBuf {
    PathBuf::from("/run/fscache").join(format!("{instance_name}.sock"))
}
