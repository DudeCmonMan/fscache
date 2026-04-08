use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tokio::net::UnixListener;
use tokio::sync::{broadcast, watch};
use tokio::time::{timeout, Duration};

use crate::cache::db::CacheDb;
use super::protocol::{ClientMessage, DaemonMessage};
use super::{framed_split, recv_msg, send_msg};

/// Bind a Unix domain socket at `socket_path` and serve connected TUI clients.
///
/// Each client receives a copy of `hello` on connection, then a stream of
/// broadcast events. Clients can send `ClientMessage::Shutdown` to trigger
/// daemon shutdown. Runs until `shutdown_rx` fires, then broadcasts `Goodbye`
/// and removes the socket file.
pub async fn run_ipc_server(
    socket_path: PathBuf,
    hello: DaemonMessage,
    events: broadcast::Sender<DaemonMessage>,
    shutdown_tx: watch::Sender<bool>,
    mut shutdown_rx: watch::Receiver<bool>,
    recent: Arc<Mutex<VecDeque<DaemonMessage>>>,
    db: Arc<CacheDb>,
    cache_directory: PathBuf,
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
                        let hello_clone  = hello.clone();
                        let mut rx       = events.subscribe();
                        let sd_tx        = shutdown_tx.clone();
                        let peer_path    = socket_path.clone();
                        let recent_clone = Arc::clone(&recent);
                        let db_clone     = Arc::clone(&db);
                        let cache_dir    = cache_directory.clone();
                        tokio::spawn(async move {
                            let peer = format!("client@{}", peer_path.display());
                            if let Err(e) = handle_client(stream, hello_clone, &mut rx, sd_tx, recent_clone, db_clone, cache_dir).await {
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

            Ok(_) = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    break;
                }
            }
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
    shutdown_tx: watch::Sender<bool>,
    recent: Arc<Mutex<VecDeque<DaemonMessage>>>,
    db: Arc<CacheDb>,
    _cache_directory: PathBuf,
) -> anyhow::Result<()> {
    let (mut reader, mut writer) = framed_split(stream);

    send_msg(&mut writer, &hello).await?;

    // Replay recent messages so the client doesn't start with an empty view.
    // The broadcast subscription (rx) was created before this point, so any
    // messages arriving during replay also queue in rx — minor duplicates are
    // acceptable.
    {
        let snapshot: Vec<DaemonMessage> = recent.lock().unwrap().iter().cloned().collect();
        for msg in snapshot {
            send_msg(&mut writer, &msg).await?;
        }
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
                        let _ = shutdown_tx.send(true);
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
                    None => return Ok(()), // client closed connection
                }
            }
        }
    }
}

pub fn socket_path(instance_name: &str) -> PathBuf {
    PathBuf::from("/run/fscache").join(format!("{instance_name}.sock"))
}
