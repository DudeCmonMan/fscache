use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

use tracing::field::{Field, Visit};
use tracing_subscriber::Layer;

use super::state::{CopyProgress, DashboardState};
use crate::telemetry;

/// A `tracing::Layer` that subscribes to structured `event = "..."` fields emitted
/// by the core code and updates `DashboardState` counters accordingly.
///
/// The core code never imports this module. If this Layer is not installed
/// (i.e. `--tui` is not passed), the structured fields are no-ops.
pub struct MetricsLayer {
    state: Arc<DashboardState>,
}

impl MetricsLayer {
    pub fn new(state: Arc<DashboardState>) -> Self {
        Self { state }
    }

    fn remove_active_copy(&self, path: &Option<String>) {
        if let Some(path_str) = path {
            self.state.in_flight_count.fetch_update(Relaxed, Relaxed, |v| Some(v.saturating_sub(1))).ok();
            self.state.active_copies.lock().unwrap().remove(std::path::Path::new(path_str));
        }
    }
}

impl<S> Layer<S> for MetricsLayer
where
    S: tracing::Subscriber,
{
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: tracing_subscriber::layer::Context<'_, S>) {
        let mut visitor = EventVisitor::default();
        event.record(&mut visitor);

        match visitor.event.as_deref() {
            Some(e) if e == telemetry::EVENT_FUSE_OPEN => {
                self.state.fuse_opens.fetch_add(1, Relaxed);
                self.state.open_handles.fetch_add(1, Relaxed);
            }
            Some(e) if e == telemetry::EVENT_CACHE_HIT => {
                self.state.cache_hits.fetch_add(1, Relaxed);
            }
            Some(e) if e == telemetry::EVENT_CACHE_MISS => {
                self.state.cache_misses.fetch_add(1, Relaxed);
            }
            Some(e) if e == telemetry::EVENT_HANDLE_CLOSED => {
                self.state.open_handles.fetch_update(Relaxed, Relaxed, |v| Some(v.saturating_sub(1))).ok();
                if let Some(bytes) = visitor.bytes_read {
                    self.state.bytes_read.fetch_add(bytes, Relaxed);
                }
            }
            Some(e) if e == telemetry::EVENT_COPY_QUEUED => {
                self.state.in_flight_count.fetch_add(1, Relaxed);
            }
            Some(e) if e == telemetry::EVENT_COPY_STARTED => {
                if let Some(ref path_str) = visitor.path {
                    let path = std::path::PathBuf::from(path_str);
                    self.state.active_copies.lock().unwrap().insert(path.clone(), CopyProgress {
                        path,
                        size_bytes: visitor.size_bytes.unwrap_or(0),
                        started_at: std::time::Instant::now(),
                    });
                }
            }
            Some(e) if e == telemetry::EVENT_COPY_COMPLETE => {
                self.state.completed_copies.fetch_add(1, Relaxed);
                self.remove_active_copy(&visitor.path);
            }
            Some(e) if e == telemetry::EVENT_COPY_FAILED => {
                self.state.failed_copies.fetch_add(1, Relaxed);
                self.remove_active_copy(&visitor.path);
            }
            Some(e) if e == telemetry::EVENT_DEFERRED_CHANGED => {
                if let Some(count) = visitor.count {
                    self.state.deferred_count.store(count, Relaxed);
                }
            }
            Some(e) if e == telemetry::EVENT_BUDGET_UPDATED => {
                if let Some(used) = visitor.used_bytes {
                    self.state.budget_used_bytes.store(used, Relaxed);
                }
                if let Some(max) = visitor.max_bytes {
                    self.state.budget_max_bytes.store(max, Relaxed);
                }
            }
            Some(e) if e == telemetry::EVENT_CACHING_WINDOW => {
                self.state.caching_allowed.store(visitor.allowed.unwrap_or(false), Relaxed);
            }
            Some(e) if e == telemetry::EVENT_EVICTION => {
                match visitor.reason.as_deref() {
                    Some("expired")    => { self.state.evictions_expired.fetch_add(1, Relaxed); }
                    Some("size_limit") => { self.state.evictions_size.fetch_add(1, Relaxed); }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

#[derive(Default)]
struct EventVisitor {
    event:      Option<String>,
    path:       Option<String>,
    reason:     Option<String>,
    bytes_read: Option<u64>,
    size_bytes: Option<u64>,
    used_bytes: Option<u64>,
    max_bytes:  Option<u64>,
    count:      Option<u64>,
    allowed:    Option<bool>,
}

impl Visit for EventVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "event"  => self.event  = Some(value.to_string()),
            "path"   => self.path   = Some(value.to_string()),
            "reason" => self.reason = Some(value.to_string()),
            _ => {}
        }
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        match field.name() {
            "bytes_read" => self.bytes_read = Some(value),
            "size_bytes" => self.size_bytes = Some(value),
            "used_bytes" => self.used_bytes = Some(value),
            "max_bytes"  => self.max_bytes  = Some(value),
            "count"      => self.count      = Some(value),
            _ => {}
        }
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        if field.name() == "allowed" {
            self.allowed = Some(value);
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        // Fallback: some tracing macros emit fields as Debug rather than &str.
        let s = format!("{:?}", value);
        let s = s.trim_matches('"');
        match field.name() {
            "event"  => self.event  = Some(s.to_string()),
            "path"   => self.path   = Some(s.to_string()),
            "reason" => self.reason = Some(s.to_string()),
            _ => {}
        }
    }
}
