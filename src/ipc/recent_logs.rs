use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use tokio::sync::broadcast;

use super::protocol::{DaemonMessage, LogLine};

const RECENT_LOG_CAP: usize = 100;

/// Spawn a long-lived task that subscribes to the IPC broadcast channel and
/// maintains a ring buffer of the last `RECENT_LOG_CAP` log lines.
///
/// The ring is read-only during normal operation — `handle_client` takes a
/// snapshot when a new watch client connects and replays it before live streaming
/// begins. Telemetry events are ignored here, so they can never starve logs out
/// of the replay buffer.
pub fn spawn_recent_logs_task(
    mut rx: broadcast::Receiver<DaemonMessage>,
    recent: Arc<Mutex<VecDeque<LogLine>>>,
) {
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(DaemonMessage::Log(line)) => {
                    let mut buf = recent.lock().unwrap();
                    if buf.len() >= RECENT_LOG_CAP {
                        buf.pop_front();
                    }
                    buf.push_back(line);
                }
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    });
}
