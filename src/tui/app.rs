use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Duration;

use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::ExecutableCommand;
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::watch;
use tokio::time::interval;

use crate::cache::CacheManager;
use super::state::{CachedFileInfo, CacheSort, DashboardState, Page};
use super::ui;

const RENDER_TICK_MS: u64 = 500;
const POLL_TICK_SECS: u64 = 3;

/// Runs the TUI event loop. Returns when the user presses q or shutdown_rx fires.
pub async fn run(
    state: Arc<DashboardState>,
    cache_managers: Vec<Arc<CacheManager>>,
    mut shutdown_rx: watch::Receiver<bool>,
    shutdown_tx: watch::Sender<bool>,
) -> anyhow::Result<()> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    stdout.execute(EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = event_loop(
        &mut terminal,
        Arc::clone(&state),
        cache_managers,
        &mut shutdown_rx,
        &shutdown_tx,
    ).await;

    disable_raw_mode()?;
    terminal.backend_mut().execute(LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    // Terminal is restored — redirect subsequent tracing events to stderr
    // so shutdown messages (unmount, cleanup) are visible to the user.
    state.tui_exited.store(true, std::sync::atomic::Ordering::Relaxed);

    result
}

async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    state: Arc<DashboardState>,
    cache_managers: Vec<Arc<CacheManager>>,
    shutdown_rx: &mut watch::Receiver<bool>,
    shutdown_tx: &watch::Sender<bool>,
) -> anyhow::Result<()> {
    let mut render_tick = interval(Duration::from_millis(RENDER_TICK_MS));
    let mut poll_tick   = interval(Duration::from_secs(POLL_TICK_SECS));
    poll_tick.tick().await;
    poll_cache_stats(&state, &cache_managers);

    let mut events = EventStream::new();
    let mut page       = Page::Status;
    let mut log_scroll = 0usize;
    let mut cache_sel  = 0usize;
    let mut cache_sort = CacheSort::Newest;

    loop {
        tokio::select! {
            _ = render_tick.tick() => {
                terminal.draw(|f| ui::render(f, &state, page, log_scroll, cache_sel, cache_sort))?;
            }

            _ = poll_tick.tick() => {
                poll_cache_stats(&state, &cache_managers);
            }

            Some(Ok(event)) = events.next() => {
                if let Event::Key(key) = event {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Char('Q') => {
                            let _ = shutdown_tx.send(true);
                            break;
                        }
                        KeyCode::Right => {
                            page = page.next();
                            log_scroll = 0;
                        }
                        KeyCode::Left => {
                            page = page.prev();
                            log_scroll = 0;
                        }
                        KeyCode::Char('1') => { page = Page::Status; log_scroll = 0; }
                        KeyCode::Char('2') => { page = Page::Cache;  cache_sel = 0; }
                        KeyCode::Char('3') => { page = Page::Logs;   log_scroll = 0; }
                        KeyCode::Down => match page {
                            Page::Cache => {
                                let len = state.cached_files.lock().unwrap().len();
                                if len > 0 { cache_sel = (cache_sel + 1).min(len - 1); }
                            }
                            Page::Logs => {
                                let len = state.recent_logs.lock().unwrap().len();
                                if len > 0 { log_scroll = (log_scroll + 1).min(len.saturating_sub(1)); }
                            }
                            _ => {}
                        }
                        KeyCode::Up => match page {
                            Page::Cache => { cache_sel = cache_sel.saturating_sub(1); }
                            Page::Logs  => { log_scroll = log_scroll.saturating_sub(1); }
                            _ => {}
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
                            }
                        }
                        _ => {}
                    }
                    terminal.draw(|f| ui::render(f, &state, page, log_scroll, cache_sel, cache_sort))?;
                }
            }

            Ok(_) = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    break;
                }
            }
        }
    }

    Ok(())
}

/// Calls CacheManager::stats() and writes results into DashboardState.
/// Called once at startup and every POLL_TICK_SECS thereafter.
fn poll_cache_stats(state: &Arc<DashboardState>, cache_managers: &[Arc<CacheManager>]) {
    // Aggregate across all mounts into a combined view (single-cache design).
    let mut total_used = 0u64;
    let mut total_files = 0usize;
    let mut combined_files: Vec<CachedFileInfo> = Vec::new();
    let mut free_bytes: Option<u64> = None;
    let mut max_bytes = 0u64;
    let mut min_free = 0u64;
    let mut expiry = Duration::ZERO;

    for cm in cache_managers {
        let s = cm.stats();
        total_used  += s.used_bytes;
        total_files += s.file_count;
        free_bytes   = free_bytes.or(s.free_space_bytes).map(|prev| s.free_space_bytes.unwrap_or(prev).min(prev));
        if s.max_size_bytes > 0 { max_bytes = s.max_size_bytes; }
        if s.min_free_bytes > 0 { min_free  = s.min_free_bytes; }
        if s.expiry > Duration::ZERO { expiry = s.expiry; }

        for (path, size, cached_at, last_hit_at) in s.files {
            let evicts_at = last_hit_at + expiry;
            combined_files.push(CachedFileInfo {
                path,
                size_bytes: size,
                cached_at,
                last_hit_at,
                evicts_at,
            });
        }
    }

    state.cache_used_bytes.store(total_used, Relaxed);
    state.cache_max_bytes.store(max_bytes, Relaxed);
    state.cache_free_bytes.store(free_bytes.unwrap_or(0), Relaxed);
    state.cache_min_free_bytes.store(min_free, Relaxed);
    state.cache_file_count.store(total_files as u64, Relaxed);

    *state.cached_files.lock().unwrap() = combined_files;
}
