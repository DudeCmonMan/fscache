use std::cell::RefCell;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use arc_swap::ArcSwapOption;
use dashmap::{DashMap, DashSet};
use lru::LruCache;
use tokio_util::sync::CancellationToken;

use crate::cache::db::{CacheDb, ProcessAccessRow};
use crate::config::DiscoveryConfig;
use crate::fuse::fusefs::OpenOutcome;
use crate::ipc::protocol::{DaemonMessage, TelemetryEvent};
use crate::preset::ProcessInfo;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OpKind { Hit, Miss, Meta }

impl OpKind {
    fn as_str(self) -> &'static str {
        match self { Self::Hit => "hit", Self::Miss => "miss", Self::Meta => "meta" }
    }
}

pub struct DiscoveryStatus {
    pub enabled: bool,
    pub started_at: Option<i64>,
}

// ---------------------------------------------------------------------------
// Thread-local: last-seen PID cache (eliminates LRU lock on the hot path)
// ---------------------------------------------------------------------------

thread_local! {
    static LAST_SEEN: RefCell<Option<(u32, Arc<ProcessInfo>)>> = const { RefCell::new(None) };
}

pub struct DiscoveryController {
    enabled: Arc<AtomicBool>,
    session: ArcSwapOption<Session>,
    start_stop: Mutex<()>,
    config: DiscoveryConfig,
    db: Arc<CacheDb>,
    blocklist: Arc<Vec<String>>,
    root_token: CancellationToken,
    ipc_tx: tokio::sync::broadcast::Sender<DaemonMessage>,
}

impl DiscoveryController {
    pub fn new(
        config: DiscoveryConfig,
        db: Arc<CacheDb>,
        blocklist: Arc<Vec<String>>,
        root_token: CancellationToken,
        ipc_tx: tokio::sync::broadcast::Sender<DaemonMessage>,
    ) -> Arc<Self> {
        Arc::new(Self {
            enabled: Arc::new(AtomicBool::new(false)),
            session: ArcSwapOption::empty(),
            start_stop: Mutex::new(()),
            config,
            db,
            blocklist,
            root_token,
            ipc_tx,
        })
    }

    // -----------------------------------------------------------------------
    // Hot path: metadata ops (getattr / lookup / opendir / readdir)
    // -----------------------------------------------------------------------

    #[inline]
    pub fn log_touch(&self, pid: u32, op: OpKind) {
        if !self.enabled.load(Ordering::Relaxed) { return; }
        let Some(session) = self.session.load_full() else { return };

        LAST_SEEN.with_borrow_mut(|last| {
            let info = match last.as_ref() {
                Some((p, info)) if *p == pid => info.clone(),
                _ => {
                    let info = session.resolve_pid(pid);
                    *last = Some((pid, info.clone()));
                    info
                }
            };
            let name: Arc<str> = info.name.as_deref().unwrap_or("(unknown)").into();
            session.bump(&name, op);
        });
    }

    // -----------------------------------------------------------------------
    // Hot path: open() — ProcessInfo is already captured by the FUSE handler
    // -----------------------------------------------------------------------

    pub fn log_open(&self, info: &ProcessInfo, outcome: OpenOutcome) {
        if !self.enabled.load(Ordering::Relaxed) { return; }
        let Some(session) = self.session.load_full() else { return };

        let op = match outcome {
            OpenOutcome::Hit => OpKind::Hit,
            OpenOutcome::Miss | OpenOutcome::Filtered => OpKind::Miss,
        };

        let name: Arc<str> = info.name.as_deref().unwrap_or("(unknown)").into();

        if session.seen.insert(name.clone()) {
            let (cmdline, ancestors) = format_process_info(info);
            session.samples.insert(name.clone(), (Some(cmdline.clone()), Some(ancestors.clone())));
            let blocked = info.is_blocked_by(&session.blocklist);
            emit_new(info, &name, &ancestors, &cmdline, blocked);
        }

        session.bump(&name, op);
    }

    pub fn start(self: &Arc<Self>) -> anyhow::Result<()> {
        let _guard = self.start_stop.lock().unwrap();

        if self.enabled.load(Ordering::Relaxed) {
            return Ok(());  // already running
        }

        let child_token = self.root_token.child_token();
        let now = now_unix_sec() as i64;

        let lru_cap = NonZeroUsize::new(self.config.pid_lru_capacity.max(1)).unwrap();
        let session = Arc::new(Session {
            child_token: child_token.clone(),
            counts: Arc::new(DashMap::new()),
            samples: Arc::new(DashMap::new()),
            seen: Arc::new(DashSet::new()),
            pid_lru: Arc::new(Mutex::new(LruCache::new(lru_cap))),
            blocklist: Arc::clone(&self.blocklist),
            started_at: now,
        });

        self.session.store(Some(Arc::clone(&session)));
        self.enabled.store(true, Ordering::Release);

        let db = Arc::clone(&self.db);
        let bucket_secs = self.config.bucket_interval_secs;
        let ipc_tx = self.ipc_tx.clone();
        let ctrl = Arc::clone(self);
        tokio::spawn(async move {
            drain_loop(session, db, bucket_secs, ipc_tx).await;
            ctrl.enabled.store(false, Ordering::Release);
            ctrl.session.store(None);
            ctrl.broadcast_status();
        });

        self.broadcast_status();
        tracing::info!("discovery: armed");
        Ok(())
    }

    pub fn stop(&self) {
        let _guard = self.start_stop.lock().unwrap();
        if !self.enabled.load(Ordering::Relaxed) { return; }

        // Cancel the child token — drain_loop flushes in-flight bucket and exits.
        if let Some(session) = self.session.load_full() {
            session.child_token.cancel();
        }

        self.enabled.store(false, Ordering::Release);
        self.session.store(None);
        self.broadcast_status();
        tracing::info!("discovery: disarmed");
    }

    pub fn status(&self) -> DiscoveryStatus {
        let session = self.session.load_full();
        DiscoveryStatus {
            enabled: self.enabled.load(Ordering::Relaxed),
            started_at: session.as_ref().map(|s| s.started_at),
        }
    }

    pub fn broadcast_status(&self) {
        let s = self.status();
        let _ = self.ipc_tx.send(DaemonMessage::Event(TelemetryEvent::DiscoveryStatus {
            enabled: s.enabled,
            started_at: s.started_at,
        }));
    }
}

struct Session {
    child_token: CancellationToken,
    counts: Arc<DashMap<(Arc<str>, OpKind), AtomicU64>>,
    samples: Arc<DashMap<Arc<str>, (Option<String>, Option<String>)>>,
    seen: Arc<DashSet<Arc<str>>>,
    pid_lru: Arc<Mutex<LruCache<u32, Arc<ProcessInfo>>>>,
    blocklist: Arc<Vec<String>>,
    started_at: i64,
}

impl Session {
    /// Resolve a PID to ProcessInfo, using LRU cache to avoid repeated /proc reads.
    fn resolve_pid(&self, pid: u32) -> Arc<ProcessInfo> {
        if let Some(info) = self.pid_lru.lock().unwrap().get(&pid).cloned() {
            return info;
        }
        let info = Arc::new(ProcessInfo::capture(pid));
        self.pid_lru.lock().unwrap().put(pid, info.clone());

        let name: Arc<str> = info.name.as_deref().unwrap_or("(unknown)").into();
        if self.seen.insert(name.clone()) {
            let (cmdline, ancestors) = format_process_info(&info);
            self.samples.insert(name.clone(), (Some(cmdline.clone()), Some(ancestors.clone())));
            let blocked = info.is_blocked_by(&self.blocklist);
            emit_new(&info, &name, &ancestors, &cmdline, blocked);
        }
        info
    }

    fn bump(&self, name: &Arc<str>, op: OpKind) {
        self.counts
            .entry((name.clone(), op))
            .or_insert_with(|| AtomicU64::new(0))
            .fetch_add(1, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------
// Drain loop (one tokio task per session)
// ---------------------------------------------------------------------------

async fn drain_loop(
    session: Arc<Session>,
    db: Arc<CacheDb>,
    bucket_interval_secs: u64,
    _ipc_tx: tokio::sync::broadcast::Sender<DaemonMessage>,
) {
    let interval = tokio::time::Duration::from_secs(bucket_interval_secs.max(1));
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ticker.tick().await; // consume immediate first tick

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                flush_bucket(&session, Arc::clone(&db)).await;
            }
            _ = session.child_token.cancelled() => {
                flush_bucket(&session, Arc::clone(&db)).await;
                return;
            }
        }
    }
}

async fn flush_bucket(session: &Session, db: Arc<CacheDb>) {
    let bucket_epoch = now_unix_sec() as i64;

    let rows: Vec<ProcessAccessRow> = session.counts.iter()
        .filter_map(|entry| {
            let count = entry.value().swap(0, Ordering::Relaxed);
            if count == 0 { return None; }
            let (name, op) = entry.key();
            let (cmdline, ancestors) = session.samples
                .get(name)
                .map(|s| (s.0.clone(), s.1.clone()))
                .unwrap_or_default();
            Some(ProcessAccessRow {
                bucket_epoch,
                process_name: name.to_string(),
                op_kind: op.as_str().to_string(),
                count,
                sample_cmdline: cmdline,
                sample_ancestors: ancestors,
            })
        })
        .collect();

    if rows.is_empty() { return; }

    // Aggregate per-process for human-readable SNAP lines.
    let mut by_process: std::collections::HashMap<&str, [u64; 3]> = std::collections::HashMap::new();
    for row in &rows {
        let entry = by_process.entry(row.process_name.as_str()).or_insert([0u64; 3]);
        match row.op_kind.as_str() {
            "hit"  => entry[0] += row.count,
            "miss" => entry[1] += row.count,
            "meta" => entry[2] += row.count,
            _ => {}
        }
    }
    emit_snap(&by_process);

    let _ = tokio::task::spawn_blocking(move || db.upsert_process_access(&rows)).await;
}

fn emit_new(
    _info: &ProcessInfo,
    name: &str,
    ancestors: &str,
    cmdline: &str,
    blocked: bool,
) {
    let now = utc_time_hms();
    let blk = if blocked { "*" } else { "-" };
    let anc_str = if ancestors.is_empty() { String::new() } else { format!(" anc={ancestors}") };
    let cmd_str = if cmdline.is_empty() { String::new() } else { format!(" cmd={cmdline:?}") };
    tracing::debug!(
        target: "fscache::discovery",
        "{now}  NEW     {name:<32}  {blk}     -      -      -       -  pid=?{anc_str}{cmd_str}"
    );
}

fn emit_snap(by_process: &std::collections::HashMap<&str, [u64; 3]>) {
    let now = utc_time_hms();
    let mut entries: Vec<_> = by_process.iter().collect();
    entries.sort_unstable_by(|(_, a), (_, b)| {
        let ta: u64 = a.iter().sum();
        let tb: u64 = b.iter().sum();
        tb.cmp(&ta)
    });
    for (name, counts) in entries {
        let hit = counts[0];
        let miss = counts[1];
        let meta = counts[2];
        let total = hit + miss + meta;
        tracing::debug!(
            target: "fscache::discovery",
            "{now}  SNAP    {name:<32}  -  {hit:>6}  {miss:>6}  {meta:>6}  {total:>8}",
        );
    }
}

fn format_process_info(info: &ProcessInfo) -> (String, String) {
    let cmdline = info.cmdline.as_deref()
        .map(|b| {
            let s = b.split(|&c| c == 0)
                .filter(|s| !s.is_empty())
                .map(|s| String::from_utf8_lossy(s).into_owned())
                .collect::<Vec<_>>()
                .join(" ");
            if s.len() > 100 { format!("{}…", &s[..100]) } else { s }
        })
        .unwrap_or_default();
    let ancestors = info.ancestors.join(">");
    (cmdline, ancestors)
}

fn utc_time_hms() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let s = (secs % 60) as u8;
    let m = ((secs / 60) % 60) as u8;
    let h = ((secs / 3600) % 24) as u8;
    format!("{h:02}:{m:02}:{s:02}")
}

pub fn now_unix_sec() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ---------------------------------------------------------------------------
// FormatEvent impl for the dedicated discovery log layer
// ---------------------------------------------------------------------------

use tracing::field::{Field, Visit};
use tracing_subscriber::fmt::format::{FormatEvent, Writer};
use tracing_subscriber::fmt::FmtContext;

pub struct DiscoveryFormatter;

struct MsgCapture(String);

impl Visit for MsgCapture {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" && self.0.is_empty() {
            self.0 = value.to_string();
        }
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" && self.0.is_empty() {
            // tracing stores formatted string messages as debug values.
            let raw = format!("{value:?}");
            // Strip surrounding quotes that Debug adds to &str.
            if raw.len() >= 2 && raw.starts_with('"') && raw.ends_with('"') {
                self.0 = raw[1..raw.len()-1]
                    .replace("\\\"", "\"")
                    .replace("\\\\", "\\")
                    .replace("\\n", "\n")
                    .replace("\\t", "\t");
            } else {
                self.0 = raw;
            }
        }
    }
}

impl<S, N> FormatEvent<S, N> for DiscoveryFormatter
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    N: for<'a> tracing_subscriber::fmt::FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        _ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &tracing::Event<'_>,
    ) -> std::fmt::Result {
        let mut v = MsgCapture(String::new());
        event.record(&mut v);
        writeln!(writer, "{}", v.0)
    }
}
