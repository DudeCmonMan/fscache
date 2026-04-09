use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};

use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::ExecutableCommand;
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::watch;
use tokio::time::interval;

use crate::cache::db::CacheDb;
use crate::ipc::client;
use crate::ipc::protocol::{ClientMessage, FileTarget};
use crate::ipc::{send_msg, IpcFramedWriter};
use super::state::{CachedFileInfo, CacheSort, DashboardState, MountInfo, Page};
use super::ui;

const RENDER_TICK_MS: u64 = 500;
const POLL_TICK_SECS: u64 = 3;

/// Connect to a running daemon socket and launch the TUI monitoring dashboard.
///
/// `q`      — detach TUI, daemon keeps running.
/// `Ctrl+Q` — send `Shutdown` to daemon, then exit.
pub async fn run_client(socket_path: PathBuf) -> anyhow::Result<()> {
    let (hello, mut reader, writer) = client::connect(&socket_path).await?;

    let state = Arc::new(DashboardState::new(Arc::new(hello.config)));
    {
        let mut mounts = state.mounts.lock().unwrap();
        for m in &hello.mounts {
            mounts.push(MountInfo {
                target:    m.target.clone(),
                cache_dir: m.cache_dir.clone(),
                active:    m.active,
            });
        }
    }

    let db_path = PathBuf::from(&hello.db_path);
    let db = Arc::new(CacheDb::open_readonly(&db_path)?);

    let mount_dirs: Vec<PathBuf> = hello.mounts.iter()
        .map(|m| m.cache_dir.clone())
        .collect();

    let state_for_ipc = Arc::clone(&state);
    let (disc_tx, mut disc_rx) = watch::channel(false);
    tokio::spawn(async move {
        let _ = client::run_client_stream(&mut reader, state_for_ipc).await;
        let _ = disc_tx.send(true);
    });

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    stdout.execute(EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = event_loop(
        &mut terminal,
        Arc::clone(&state),
        db,
        mount_dirs,
        writer,
        &mut disc_rx,
    ).await;

    disable_raw_mode()?;
    terminal.backend_mut().execute(LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    state: Arc<DashboardState>,
    db: Arc<CacheDb>,
    mount_dirs: Vec<PathBuf>,
    mut writer: IpcFramedWriter,
    disc_rx: &mut watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let mut render_tick = interval(Duration::from_millis(RENDER_TICK_MS));
    let mut poll_tick   = interval(Duration::from_secs(POLL_TICK_SECS));
    poll_tick.tick().await;
    poll_cache_stats(&state, &db, &mount_dirs);

    let mut events     = EventStream::new();
    let mut page       = Page::Status;
    let mut log_scroll = 0usize;
    let mut cache_sel  = 0usize;
    let mut cache_sort = CacheSort::Newest;
    let mut checked:    HashSet<PathBuf> = HashSet::new();
    // None = closed; Some(idx) = menu open with that item highlighted (0=Evict, 1=Refresh)
    let mut menu:       Option<usize> = None;
    // Flash message shown after an action is dispatched; blocks Enter for 2 seconds.
    let mut status_msg: Option<(String, Instant)> = None;

    loop {
        tokio::select! {
            _ = render_tick.tick() => {
                if let Some((_, t)) = &status_msg {
                    if t.elapsed() >= Duration::from_secs(2) {
                        status_msg = None;
                    }
                }
                let status = status_msg.as_ref().map(|(s, _)| s.as_str());
                terminal.draw(|f| ui::render(f, &state, page, log_scroll, cache_sel, cache_sort, &checked, menu, status))?;
            }

            _ = poll_tick.tick() => {
                poll_cache_stats(&state, &db, &mount_dirs);
            }

            Some(Ok(event)) = events.next() => {
                if let Event::Key(key) = event {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }

                    // --- Action menu (overlay) ---
                    if let Some(sel) = menu {
                        match key.code {
                            KeyCode::Up   => { menu = Some(sel.saturating_sub(1)); }
                            KeyCode::Down => { menu = Some((sel + 1).min(1)); }
                            KeyCode::Esc  => { menu = None; }
                            KeyCode::Enter => {
                                let targets = build_targets(&state, &checked, cache_sel, cache_sort);
                                if !targets.is_empty() {
                                    let n = targets.len();
                                    let (msg, flash) = if sel == 0 {
                                        let f = format!("Evicting {} file{}…", n, if n == 1 { "" } else { "s" });
                                        (ClientMessage::EvictFiles { files: targets }, f)
                                    } else {
                                        let f = format!("Refreshing lease for {} file{}…", n, if n == 1 { "" } else { "s" });
                                        (ClientMessage::RefreshLease { files: targets }, f)
                                    };
                                    let _ = send_msg(&mut writer, &msg).await;
                                    checked.clear();
                                    status_msg = Some((flash, Instant::now()));
                                }
                                menu = None;
                            }
                            _ => {}
                        }
                        let status = status_msg.as_ref().map(|(s, _)| s.as_str());
                        terminal.draw(|f| ui::render(f, &state, page, log_scroll, cache_sel, cache_sort, &checked, menu, status))?;
                        continue;
                    }

                    // --- Normal key handling ---
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Char('Q')
                            if !key.modifiers.contains(KeyModifiers::CONTROL) =>
                        {
                            break;
                        }
                        KeyCode::Char('q') | KeyCode::Char('Q')
                            if key.modifiers.contains(KeyModifiers::CONTROL) =>
                        {
                            let _ = send_msg(&mut writer, &ClientMessage::Shutdown).await;
                            break;
                        }
                        KeyCode::Right => {
                            page = page.next();
                            log_scroll = 0;
                            checked.clear();
                            menu = None;
                        }
                        KeyCode::Left => {
                            page = page.prev();
                            log_scroll = 0;
                            checked.clear();
                            menu = None;
                        }
                        KeyCode::Char('1') => {
                            page = Page::Status;
                            log_scroll = 0;
                            checked.clear();
                            menu = None;
                        }
                        KeyCode::Char('2') => {
                            page = Page::Cache;
                            cache_sel = 0;
                            checked.clear();
                            menu = None;
                        }
                        KeyCode::Char('3') => {
                            page = Page::Logs;
                            log_scroll = 0;
                            checked.clear();
                            menu = None;
                        }
                        KeyCode::Down => {
                            let shift = key.modifiers.contains(KeyModifiers::SHIFT);
                            match page {
                                Page::Cache => {
                                    let len = state.cached_files.lock().unwrap().len();
                                    if len > 0 {
                                        cache_sel = (cache_sel + 1).min(len - 1);
                                        if shift {
                                            toggle_checked_at(&state, &mut checked, cache_sel, cache_sort);
                                        }
                                    }
                                }
                                Page::Logs => {
                                    let len = state.recent_logs.lock().unwrap().len();
                                    if len > 0 { log_scroll = (log_scroll + 1).min(len.saturating_sub(1)); }
                                }
                                _ => {}
                            }
                        }
                        KeyCode::Up => {
                            let shift = key.modifiers.contains(KeyModifiers::SHIFT);
                            match page {
                                Page::Cache => {
                                    cache_sel = cache_sel.saturating_sub(1);
                                    if shift {
                                        toggle_checked_at(&state, &mut checked, cache_sel, cache_sort);
                                    }
                                }
                                Page::Logs => { log_scroll = log_scroll.saturating_sub(1); }
                                _ => {}
                            }
                        }
                        KeyCode::Home => match page {
                            Page::Cache => { cache_sel = 0; }
                            Page::Logs  => { log_scroll = 0; }
                            _ => {}
                        }
                        KeyCode::End => match page {
                            Page::Cache => {
                                let len = state.cached_files.lock().unwrap().len();
                                cache_sel = len.saturating_sub(1);
                            }
                            Page::Logs => {
                                let len = state.recent_logs.lock().unwrap().len();
                                log_scroll = len.saturating_sub(1);
                            }
                            _ => {}
                        }
                        KeyCode::Char('s') | KeyCode::Char('S') => {
                            if page == Page::Cache {
                                cache_sort = cache_sort.next();
                                checked.clear();
                            }
                        }
                        KeyCode::Char(' ') => {
                            if page == Page::Cache {
                                toggle_checked_at(&state, &mut checked, cache_sel, cache_sort);
                            }
                        }
                        KeyCode::Enter => {
                            if page == Page::Cache && status_msg.is_none() {
                                let has_files = !state.cached_files.lock().unwrap().is_empty();
                                if has_files {
                                    menu = Some(0);
                                }
                            }
                        }
                        _ => {}
                    }
                    let status = status_msg.as_ref().map(|(s, _)| s.as_str());
                    terminal.draw(|f| ui::render(f, &state, page, log_scroll, cache_sel, cache_sort, &checked, menu, status))?;
                }
            }

            Ok(_) = disc_rx.changed() => {
                if *disc_rx.borrow() {
                    tracing::info!("Daemon disconnected");
                    break;
                }
            }
        }
    }

    Ok(())
}

/// Toggle the checked state for whichever file sits at `idx` in the sorted list.
fn toggle_checked_at(
    state: &DashboardState,
    checked: &mut HashSet<PathBuf>,
    idx: usize,
    sort: CacheSort,
) {
    let files = state.cached_files.lock().unwrap();
    let mut sorted: Vec<&CachedFileInfo> = files.iter().collect();
    sort_refs(&mut sorted, sort);
    if let Some(f) = sorted.get(idx) {
        if !checked.remove(&f.path) {
            checked.insert(f.path.clone());
        }
    }
}

/// Build the `Vec<FileTarget>` to send with an IPC command.
/// Uses the checked set if non-empty, otherwise the highlighted file.
fn build_targets(
    state: &DashboardState,
    checked: &HashSet<PathBuf>,
    sel: usize,
    sort: CacheSort,
) -> Vec<FileTarget> {
    let files = state.cached_files.lock().unwrap();
    let mut sorted: Vec<&CachedFileInfo> = files.iter().collect();
    sort_refs(&mut sorted, sort);

    if checked.is_empty() {
        sorted.get(sel).map(|f| vec![FileTarget {
            rel_path: f.path.clone(),
            mount_id: f.mount_id.clone(),
        }]).unwrap_or_default()
    } else {
        sorted.iter()
            .filter(|f| checked.contains(&f.path))
            .map(|f| FileTarget {
                rel_path: f.path.clone(),
                mount_id: f.mount_id.clone(),
            })
            .collect()
    }
}

fn sort_refs<'a>(files: &mut Vec<&'a CachedFileInfo>, sort: CacheSort) {
    match sort {
        CacheSort::Newest   => files.sort_by(|a, b| b.cached_at.cmp(&a.cached_at)),
        CacheSort::Oldest   => files.sort_by(|a, b| a.cached_at.cmp(&b.cached_at)),
        CacheSort::Largest  => files.sort_by(|a, b| b.size_bytes.cmp(&a.size_bytes)),
        CacheSort::Smallest => files.sort_by(|a, b| a.size_bytes.cmp(&b.size_bytes)),
        CacheSort::NameAz   => files.sort_by(|a, b| a.path.cmp(&b.path)),
    }
}

fn poll_cache_stats(state: &Arc<DashboardState>, db: &Arc<CacheDb>, mount_dirs: &[PathBuf]) {
    let mut total_used  = 0u64;
    let mut total_files = 0usize;
    let mut combined:   Vec<CachedFileInfo> = Vec::new();
    let mut free_bytes: Option<u64> = None;

    let expiry = Duration::from_secs(state.config.eviction.expiry_hours * 3600);

    for cache_dir in mount_dirs {
        let mount_id = cache_dir.to_string_lossy().into_owned();
        let (used, files) = db.client_files_for_mount(&mount_id);
        total_used  += used;
        total_files += files.len();

        if let Some(free) = free_space_bytes(cache_dir) {
            free_bytes = Some(match free_bytes {
                None    => free,
                Some(p) => p.min(free),
            });
        }

        for (path, size, cached_at, last_hit_at) in files {
            let evicts_at = last_hit_at + expiry;
            combined.push(CachedFileInfo {
                path,
                size_bytes: size,
                cached_at,
                last_hit_at,
                evicts_at,
                mount_id: mount_id.clone(),
            });
        }
    }

    state.cache_used_bytes.store(total_used, Relaxed);
    state.cache_free_bytes.store(free_bytes.unwrap_or(0), Relaxed);
    state.cache_file_count.store(total_files as u64, Relaxed);
    *state.cached_files.lock().unwrap() = combined;
}

fn free_space_bytes(path: &std::path::Path) -> Option<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut stat) } == 0 {
        Some(stat.f_bavail * stat.f_bsize as u64)
    } else {
        None
    }
}
