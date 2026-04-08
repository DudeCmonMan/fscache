use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::SystemTime;

use tracing::Level;
use tracing_subscriber::Layer;

use super::state::{DashboardState, LogEntry};

/// A `tracing::Layer` that captures formatted log messages into the
/// `DashboardState` ring buffer for display in the TUI log panels.
///
/// Replaces the console fmt layer when `--tui` is active so log output
/// doesn't corrupt the terminal display.
pub struct LoggingLayer {
    state: Arc<DashboardState>,
}

impl LoggingLayer {
    pub fn new(state: Arc<DashboardState>) -> Self {
        Self { state }
    }
}

impl<S> Layer<S> for LoggingLayer
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: tracing_subscriber::layer::Context<'_, S>) {
        let level = match *event.metadata().level() {
            Level::ERROR => "ERROR",
            Level::WARN  => "WARN ",
            Level::INFO  => "INFO ",
            Level::DEBUG => "DEBUG",
            Level::TRACE => "TRACE",
        };

        let mut msg_visitor = MessageVisitor::default();
        event.record(&mut msg_visitor);

        let mut message = msg_visitor.message;
        if let Some(path) = msg_visitor.path {
            message.push_str(&format!("  [{}]", path));
        }
        if let Some(reason) = msg_visitor.reason {
            message.push_str(&format!("  ({})", reason));
        }

        let now = super::ui::fmt_time(SystemTime::now());
        if self.state.tui_exited.load(Relaxed) {
            eprintln!("{} {} {}", now, level.trim(), message);
        } else {
            self.state.push_log(LogEntry {
                timestamp: now,
                level:     level.to_string(),
                message,
            });
        }
    }
}

#[derive(Default)]
struct MessageVisitor {
    message: String,
    path:    Option<String>,
    reason:  Option<String>,
}

impl tracing::field::Visit for MessageVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        let s = format!("{:?}", value);
        let s = s.trim_matches('"');
        match field.name() {
            "message" => self.message = s.to_string(),
            "path"    => self.path    = Some(s.to_string()),
            "reason"  => self.reason  = Some(s.to_string()),
            _ => {}
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        match field.name() {
            "message" => self.message = value.to_string(),
            "path"    => self.path    = Some(value.to_string()),
            "reason"  => self.reason  = Some(value.to_string()),
            _ => {}
        }
    }
}
