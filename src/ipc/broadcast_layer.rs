use std::time::SystemTime;

use tracing::Level;
use tracing::field::{Field, Visit};
use tracing_subscriber::Layer;
use tokio::sync::broadcast;

use crate::telemetry;
use crate::utils::fmt_time;
use super::protocol::{DaemonMessage, LogLine, TelemetryEvent};

/// A `tracing::Layer` that acts as an IPC broadcast bridge.
///
/// Subscribes to the same structured events as `MetricsLayer` and `LoggingLayer`
/// did, but instead of updating in-process state it serializes each event into a
/// `DaemonMessage` and broadcasts it to all connected watch clients.
///
/// All three subscriber outputs — console `fmt`, file `fmt`, and this IPC layer —
/// receive the exact same tracing events. Future logging categories are
/// automatically available to every subscriber with no per-output wiring.
///
/// The replay ring buffer is maintained by a separate subscriber task in
/// `ipc::recent_logs`, keeping this layer as a pure forwarder.
pub struct IpcBroadcastLayer {
    tx: broadcast::Sender<DaemonMessage>,
    /// Maximum level to forward as `DaemonMessage::Log`. Telemetry `Event`s are
    /// always forwarded regardless of level. Set to `Level::INFO` for normal use;
    /// `Level::DEBUG` to forward debug output to the TUI log panel.
    log_level: Level,
}

impl IpcBroadcastLayer {
    pub fn new(
        tx: broadcast::Sender<DaemonMessage>,
        log_level: Level,
    ) -> Self {
        Self { tx, log_level }
    }
}

impl<S> Layer<S> for IpcBroadcastLayer
where
    S: tracing::Subscriber,
{
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: tracing_subscriber::layer::Context<'_, S>) {
        let mut visitor = EventVisitor::default();
        event.record(&mut visitor);

        // --- Telemetry events ---
        let telemetry_msg: Option<DaemonMessage> = match visitor.event.as_deref() {
            Some(e) if e == telemetry::EVENT_FUSE_OPEN => {
                Some(DaemonMessage::Event(TelemetryEvent::FuseOpen))
            }
            Some(e) if e == telemetry::EVENT_CACHE_HIT => {
                Some(DaemonMessage::Event(TelemetryEvent::CacheHit))
            }
            Some(e) if e == telemetry::EVENT_CACHE_MISS => {
                Some(DaemonMessage::Event(TelemetryEvent::CacheMiss))
            }
            Some(e) if e == telemetry::EVENT_HANDLE_CLOSED => Some(DaemonMessage::Event(
                TelemetryEvent::HandleClosed { bytes_read: visitor.bytes_read },
            )),
            Some(e) if e == telemetry::EVENT_COPY_QUEUED => {
                Some(DaemonMessage::Event(TelemetryEvent::CopyQueued))
            }
            Some(e) if e == telemetry::EVENT_COPY_STARTED => Some(DaemonMessage::Event(
                TelemetryEvent::CopyStarted {
                    path: visitor.path.clone(),
                    size_bytes: visitor.size_bytes,
                },
            )),
            Some(e) if e == telemetry::EVENT_COPY_COMPLETE => Some(DaemonMessage::Event(
                TelemetryEvent::CopyComplete { path: visitor.path.clone() },
            )),
            Some(e) if e == telemetry::EVENT_COPY_FAILED => Some(DaemonMessage::Event(
                TelemetryEvent::CopyFailed { path: visitor.path.clone() },
            )),
            Some(e) if e == telemetry::EVENT_DEFERRED_CHANGED => Some(DaemonMessage::Event(
                TelemetryEvent::DeferredChanged { count: visitor.count },
            )),
            Some(e) if e == telemetry::EVENT_BUDGET_UPDATED => Some(DaemonMessage::Event(
                TelemetryEvent::BudgetUpdated {
                    used_bytes: visitor.used_bytes,
                    max_bytes: visitor.max_bytes,
                },
            )),
            Some(e) if e == telemetry::EVENT_CACHING_WINDOW => Some(DaemonMessage::Event(
                TelemetryEvent::CachingWindow { allowed: visitor.allowed },
            )),
            Some(e) if e == telemetry::EVENT_EVICTION => Some(DaemonMessage::Event(
                TelemetryEvent::Eviction {
                    path: visitor.path.clone(),
                    reason: visitor.reason.clone(),
                },
            )),
            Some(e) if e == telemetry::EVENT_COPY_PROGRESS => Some(DaemonMessage::Event(
                TelemetryEvent::CopyProgress {
                    path: visitor.path.clone(),
                    bytes_copied: visitor.bytes_copied,
                    size_bytes: visitor.size_bytes,
                },
            )),
            _ => None,
        };

        if let Some(msg) = telemetry_msg {
            let _ = self.tx.send(msg);
        }

        // --- Log forwarding ---
        let level = *event.metadata().level();
        if level <= self.log_level {
            let level_str = match level {
                Level::ERROR => "ERROR",
                Level::WARN  => "WARN ",
                Level::INFO  => "INFO ",
                Level::DEBUG => "DEBUG",
                Level::TRACE => "TRACE",
            };

            let mut message = visitor.message.unwrap_or_default();
            if let Some(path) = &visitor.path {
                message.push_str(&format!("  [{}]", path));
            }
            if let Some(reason) = &visitor.reason {
                message.push_str(&format!("  ({})", reason));
            }

            let _ = self.tx.send(DaemonMessage::Log(LogLine {
                timestamp: fmt_time(SystemTime::now()),
                level: level_str.to_string(),
                message,
            }));
        }
    }
}

#[derive(Default)]
struct EventVisitor {
    event:      Option<String>,
    message:    Option<String>,
    path:       Option<String>,
    reason:     Option<String>,
    bytes_read:   Option<u64>,
    bytes_copied: Option<u64>,
    size_bytes:   Option<u64>,
    used_bytes:   Option<u64>,
    max_bytes:    Option<u64>,
    count:        Option<u64>,
    allowed:      Option<bool>,
}

impl Visit for EventVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "event"   => self.event   = Some(value.to_string()),
            "message" => self.message = Some(value.to_string()),
            "path"    => self.path    = Some(value.to_string()),
            "reason"  => self.reason  = Some(value.to_string()),
            _ => {}
        }
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        match field.name() {
            "bytes_read"   => self.bytes_read   = Some(value),
            "bytes_copied" => self.bytes_copied = Some(value),
            "size_bytes"   => self.size_bytes   = Some(value),
            "used_bytes"   => self.used_bytes   = Some(value),
            "max_bytes"    => self.max_bytes    = Some(value),
            "count"        => self.count        = Some(value),
            _ => {}
        }
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        if field.name() == "allowed" {
            self.allowed = Some(value);
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let s = format!("{value:?}");
        let s = s.trim_matches('"');
        match field.name() {
            "event"   => self.event   = Some(s.to_string()),
            "message" => self.message = Some(s.to_string()),
            "path"    => self.path    = Some(s.to_string()),
            "reason"  => self.reason  = Some(s.to_string()),
            _ => {}
        }
    }
}
