use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, SystemTime};

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Gauge, List, ListItem, Paragraph, Row, Table};

use super::state::{CacheSort, DashboardState, Page};

// ---- Top-level dispatch ----

pub fn render(
    f: &mut Frame,
    state: &DashboardState,
    page: Page,
    log_scroll: usize,
    cache_sel: usize,
    cache_sort: CacheSort,
    checked: &HashSet<PathBuf>,
    menu: Option<usize>,
    status_msg: Option<&str>,
) {
    let area = f.area();

    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
    ]).split(area);

    render_tab_bar(f, chunks[0], page);

    match page {
        Page::Status  => render_status(f, chunks[1], state),
        Page::Cache   => {
            render_cache(f, chunks[1], state, cache_sel, cache_sort, checked, status_msg);
            if let Some(sel) = menu {
                render_action_menu(f, chunks[1], sel);
            }
        }
        Page::CacheIo => render_cache_io(f, chunks[1], state),
        Page::Logs    => render_logs(f, chunks[1], state, log_scroll),
    }
}

// ---- Tab bar ----

fn render_tab_bar(f: &mut Frame, area: Rect, active: Page) {
    let tabs = [
        (Page::Status,  "1 Status"),
        (Page::Cache,   "2 Cache"),
        (Page::CacheIo, "3 Cache I/O"),
        (Page::Logs,    "4 Logs"),
    ];

    let spans: Vec<Span> = tabs.iter().enumerate().flat_map(|(i, (page, label))| {
        let style = if *page == active {
            Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let tab = if *page == active {
            Span::styled(format!("[{}]", label), style)
        } else {
            Span::styled(format!(" {} ", label), style)
        };
        if i + 1 < tabs.len() {
            vec![tab, Span::raw("  ")]
        } else {
            vec![tab]
        }
    }).collect();

    let hint = Span::styled("  ←→/1-4: pages  q: quit", Style::default().fg(Color::DarkGray));
    let mut all = spans;
    all.push(hint);

    f.render_widget(Paragraph::new(Line::from(all)), area);
}

// ---- Page 1: Status ----

fn render_status(f: &mut Frame, area: Rect, state: &DashboardState) {
    let rows = Layout::vertical([
        Constraint::Length(5),
        Constraint::Length(3),
        Constraint::Length(4),
        Constraint::Min(5),
        Constraint::Length(7),
    ]).split(area);

    render_scheduler_and_cache(f, rows[0], state);
    render_predictor(f, rows[1], state);
    render_mounts(f, rows[2], state);
    render_activity(f, rows[3], state);
    render_recent_logs(f, rows[4], state);
}

fn render_scheduler_and_cache(f: &mut Frame, area: Rect, state: &DashboardState) {
    let cols = Layout::horizontal([
        Constraint::Length(30),
        Constraint::Min(0),
    ]).split(area);

    let allowed = state.caching_allowed.load(Relaxed);
    let (status_str, status_color) = if allowed {
        ("● OPEN", Color::Green)
    } else {
        ("○ CLOSED", Color::Red)
    };
    let w_start = &state.config.schedule.cache_window_start;
    let w_end   = &state.config.schedule.cache_window_end;
    let window_str = if w_start.is_empty() {
        "not configured".to_string()
    } else {
        format!("{} – {}", w_start, w_end)
    };
    let sched_text = vec![
        Line::from(format!(" Window: {}", window_str)),
        Line::from(vec![
            Span::raw(" Status: "),
            Span::styled(status_str, Style::default().fg(status_color)),
        ]),
    ];
    f.render_widget(
        Paragraph::new(sched_text).block(Block::default().borders(Borders::ALL).title(" Scheduler ")),
        cols[0],
    );

    let used  = state.cache_used_bytes.load(Relaxed);
    let max   = (state.config.eviction.max_size_gb * 1_073_741_824.0) as u64;
    let free  = state.cache_free_bytes.load(Relaxed);
    let min_f = (state.config.eviction.min_free_space_gb * 1_073_741_824.0) as u64;
    let files = state.cache_file_count.load(Relaxed);

    let ratio = if max > 0 { (used as f64 / max as f64).clamp(0.0, 1.0) } else { 0.0 };
    let gauge_label = format!("{:.1} / {:.1} GB", gb(used), gb(max));

    let block = Block::default().borders(Borders::ALL).title(" Cache Budget ");
    let inner = cols[1];
    f.render_widget(block, inner);

    let inner = cols[1].inner(ratatui::layout::Margin { vertical: 1, horizontal: 1 });
    let sub = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ]).split(inner);

    let gauge_color = if ratio > 0.9 { Color::Red } else if ratio > 0.75 { Color::Yellow } else { Color::Green };
    f.render_widget(
        Gauge::default()
            .gauge_style(Style::default().fg(gauge_color))
            .ratio(ratio)
            .label(gauge_label),
        sub[0],
    );
    f.render_widget(
        Paragraph::new(format!(" Free: {:.1} GB  (min {:.1} GB)", gb(free), gb(min_f))),
        sub[1],
    );
    f.render_widget(
        Paragraph::new(format!(" Files: {}", files)),
        sub[2],
    );
}

fn render_predictor(f: &mut Frame, area: Rect, state: &DashboardState) {
    let in_flight = state.in_flight_count.load(Relaxed);
    let deferred  = state.deferred_count.load(Relaxed);
    let b_used    = state.budget_used_bytes.load(Relaxed);
    let b_max     = state.budget_max_bytes.load(Relaxed);
    let preset    = state.config.preset.name.clone();

    let budget_str = if b_max > 0 {
        format!("Budget: {:.1} / {:.1} GB", gb(b_used), gb(b_max))
    } else {
        "Budget: unlimited".to_string()
    };
    let preset_str = if preset.is_empty() { "—".to_string() } else { preset };
    let text = format!(" In-flight: {}   Deferred: {}   {}   Preset: {}", in_flight, deferred, budget_str, preset_str);
    f.render_widget(
        Paragraph::new(text).block(Block::default().borders(Borders::ALL).title(" Action Engine ")),
        area,
    );
}

fn render_mounts(f: &mut Frame, area: Rect, state: &DashboardState) {
    let mounts = state.mounts.lock().unwrap();
    let items: Vec<ListItem> = mounts.iter().map(|m| {
        let (dot, color) = if m.active { ("●", Color::Green) } else { ("○", Color::Red) };
        ListItem::new(Line::from(vec![
            Span::styled(format!(" {} ", dot), Style::default().fg(color)),
            Span::raw(m.target.display().to_string()),
        ]))
    }).collect();

    let title = format!(" Mounts ({}) ", mounts.len());
    drop(mounts);

    f.render_widget(
        List::new(items).block(Block::default().borders(Borders::ALL).title(title)),
        area,
    );
}

fn render_activity(f: &mut Frame, area: Rect, state: &DashboardState) {
    let opens    = state.fuse_opens.load(Relaxed);
    let hits     = state.cache_hits.load(Relaxed);
    let misses   = state.cache_misses.load(Relaxed);
    let bytes    = state.bytes_read.load(Relaxed);
    let handles  = state.open_handles.load(Relaxed);
    let done     = state.completed_copies.load(Relaxed);
    let failed   = state.failed_copies.load(Relaxed);
    let ev_exp   = state.evictions_expired.load(Relaxed);
    let ev_size  = state.evictions_size.load(Relaxed);

    let hit_rate = {
        let total = hits + misses;
        if total > 0 { format!("{:.1}%", hits as f64 / total as f64 * 100.0) } else { "—".to_string() }
    };

    let lines = vec![
        Line::from(format!(" FUSE")),
        Line::from(format!("   Opens: {}   Hits: {} ({})   Misses: {}", opens, hits, hit_rate, misses)),
        Line::from(format!("   Bytes read: {}   Open handles: {}", fmt_bytes(bytes), handles)),
        Line::from(""),
        Line::from(format!(" Copies")),
        Line::from(format!("   Completed: {}   Failed: {}", done, failed)),
        Line::from(""),
        Line::from(format!(" Evictions")),
        Line::from(format!("   Expired: {}   Size-limit: {}", ev_exp, ev_size)),
    ];

    f.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" Activity ")),
        area,
    );
}

fn render_recent_logs(f: &mut Frame, area: Rect, state: &DashboardState) {
    let logs = state.recent_logs.lock().unwrap();
    let total = logs.len();
    let items: Vec<ListItem> = logs.iter().rev().take(5).map(|e| {
        let color = log_level_color(&e.level);
        ListItem::new(Line::from(vec![
            Span::styled(format!("{} ", e.timestamp), Style::default().fg(Color::DarkGray)),
            Span::styled(format!("{} ", e.level), Style::default().fg(color)),
            Span::raw(e.message.clone()),
        ]))
    }).collect();
    let title = format!(" Recent Logs ({} total — see page 4) ", total);
    drop(logs);

    f.render_widget(
        List::new(items).block(Block::default().borders(Borders::ALL).title(title)),
        area,
    );
}

// ---- Page 2: Cache ----

fn render_cache(
    f: &mut Frame,
    area: Rect,
    state: &DashboardState,
    sel: usize,
    sort: CacheSort,
    checked: &HashSet<PathBuf>,
    status_msg: Option<&str>,
) {
    let chunks = Layout::vertical([
        Constraint::Percentage(60),
        Constraint::Percentage(40),
    ]).split(area);

    let files = sorted_files(state, sort);
    render_cache_list(f, chunks[0], &files, sel, sort, state.cache_used_bytes.load(Relaxed), checked);
    render_cache_detail(f, chunks[1], &files, sel, state, checked, status_msg);
}

fn sorted_files(state: &DashboardState, sort: CacheSort) -> Vec<super::state::CachedFileInfo> {
    let mut files = state.cached_files.lock().unwrap().iter().map(|f| {
        super::state::CachedFileInfo {
            path:        f.path.clone(),
            size_bytes:  f.size_bytes,
            cached_at:   f.cached_at,
            last_hit_at: f.last_hit_at,
            evicts_at:   f.evicts_at,
            mount_id:    f.mount_id.clone(),
        }
    }).collect::<Vec<_>>();

    match sort {
        CacheSort::Newest   => files.sort_by(|a, b| b.cached_at.cmp(&a.cached_at)),
        CacheSort::Oldest   => files.sort_by(|a, b| a.cached_at.cmp(&b.cached_at)),
        CacheSort::Largest  => files.sort_by(|a, b| b.size_bytes.cmp(&a.size_bytes)),
        CacheSort::Smallest => files.sort_by(|a, b| a.size_bytes.cmp(&b.size_bytes)),
        CacheSort::NameAz   => files.sort_by(|a, b| a.path.cmp(&b.path)),
    }
    files
}

fn render_cache_list(
    f: &mut Frame,
    area: Rect,
    files: &[super::state::CachedFileInfo],
    sel: usize,
    sort: CacheSort,
    total_bytes: u64,
    checked: &HashSet<PathBuf>,
) {
    let visible_rows = (area.height as usize).saturating_sub(4).max(1);
    let max_scroll = files.len().saturating_sub(visible_rows);
    let scroll = if files.len() <= visible_rows {
        0
    } else if sel + 3 >= visible_rows {
        (sel + 4).saturating_sub(visible_rows).min(max_scroll)
    } else {
        0
    };

    let checked_count = checked.len();
    let title = if checked_count > 0 {
        format!(
            " Cached Files   sort: [{}]   {} files   {:.1} GB   {} selected   │  Space: toggle  Shift+↑↓: range  Enter: actions ",
            sort.label(), files.len(), gb(total_bytes), checked_count
        )
    } else {
        format!(
            " Cached Files   sort: [{}]   {} files   {:.1} GB   │  Space: select  Enter: actions  s: sort ",
            sort.label(), files.len(), gb(total_bytes)
        )
    };

    let header = Row::new(vec!["  ", "File", "Size", "Cached At", "Evicts In"])
        .style(Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED));

    let rows: Vec<Row> = files.iter().enumerate().skip(scroll).take(visible_rows).map(|(i, f)| {
        let is_sel     = i == sel;
        let is_checked = checked.contains(&f.path);
        let marker  = if is_sel { "▸" } else { " " };
        let checkbox = if is_checked { "[x]" } else { "[ ]" };
        let name    = format!("{} {}", marker, f.path.display());
        let size    = fmt_bytes(f.size_bytes);
        let cached_at  = fmt_time(f.cached_at);
        let evicts_in  = fmt_duration_until(f.evicts_at);
        let style = if is_sel {
            Style::default().bg(Color::DarkGray)
        } else {
            Style::default()
        };
        Row::new(vec![checkbox.to_string(), name, size, cached_at, evicts_in]).style(style)
    }).collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(5),
            Constraint::Min(20),
            Constraint::Length(10),
            Constraint::Length(10),
            Constraint::Length(10),
        ],
    )
    .header(header)
    .block(Block::default().borders(Borders::ALL).title(title));

    f.render_widget(table, area);
}

fn render_cache_detail(
    f: &mut Frame,
    area: Rect,
    files: &[super::state::CachedFileInfo],
    sel: usize,
    state: &DashboardState,
    checked: &HashSet<PathBuf>,
    status_msg: Option<&str>,
) {
    let title = " Details ";

    let mut lines = if let Some(f) = files.get(sel) {
        let mount = state.mounts.lock().unwrap()
            .first()
            .map(|m| m.target.display().to_string())
            .unwrap_or_else(|| "—".to_string());
        vec![
            Line::from(format!(" File:       {}", f.path.display())),
            Line::from(format!(" Mount:      {}", mount)),
            Line::from(format!(" Size:       {}", fmt_bytes(f.size_bytes))),
            Line::from(format!(" Cached at:  {}  ({})", fmt_datetime(f.cached_at), fmt_relative(f.cached_at))),
            Line::from(format!(" Last read:  {}  ({})", fmt_datetime(f.last_hit_at), fmt_relative(f.last_hit_at))),
            Line::from(format!(" Evicts in:  {}  ({})", fmt_duration_until(f.evicts_at), fmt_datetime(f.evicts_at))),
        ]
    } else {
        vec![Line::from(" No files cached.")]
    };

    // Flash message at the bottom of the detail panel when an action is in flight.
    if let Some(msg) = status_msg {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!(" ⏳ {}", msg),
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        )));
    }

    f.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(title)),
        area,
    );
}

/// Centered popup with two action items (Evict / Refresh Lease).
fn render_action_menu(f: &mut Frame, area: Rect, sel: usize) {
    const MENU_W: u16 = 30;
    const MENU_H: u16 = 6;

    let x = area.x + area.width.saturating_sub(MENU_W) / 2;
    let y = area.y + area.height.saturating_sub(MENU_H) / 2;
    let popup = Rect { x, y, width: MENU_W.min(area.width), height: MENU_H.min(area.height) };

    f.render_widget(Clear, popup);

    let items = ["Evict files", "Refresh lease"];
    let list_items: Vec<ListItem> = items.iter().enumerate().map(|(i, label)| {
        let style = if i == sel {
            Style::default().bg(Color::Cyan).fg(Color::Black).add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        ListItem::new(Line::from(Span::styled(format!("  {}  ", label), style)))
    }).collect();

    f.render_widget(
        List::new(list_items)
            .block(Block::default().borders(Borders::ALL).title(" Action  (↑↓ Enter Esc) ")),
        popup,
    );
}

// ---- Page 3: Cache I/O ----

fn render_cache_io(f: &mut Frame, area: Rect, state: &DashboardState) {
    let rows = Layout::vertical([
        Constraint::Length(5),
        Constraint::Length(5),
        Constraint::Min(10),
    ]).split(area);

    render_cacheio_usage(f, rows[0], state);
    render_cacheio_pipeline(f, rows[1], state);
    render_cacheio_inflight(f, rows[2], state);
}

fn render_cacheio_usage(f: &mut Frame, area: Rect, state: &DashboardState) {
    let used  = state.cache_used_bytes.load(Relaxed);
    let max   = (state.config.eviction.max_size_gb * 1_073_741_824.0) as u64;
    let free  = state.cache_free_bytes.load(Relaxed);
    let files = state.cache_file_count.load(Relaxed);

    let ratio = if max > 0 { (used as f64 / max as f64).clamp(0.0, 1.0) } else { 0.0 };
    let gauge_label = format!("{:.1} / {:.1} GB", gb(used), gb(max));
    let gauge_color = if ratio > 0.9 { Color::Red } else if ratio > 0.75 { Color::Yellow } else { Color::Green };

    let block = Block::default().borders(Borders::ALL).title(" Cache Usage ");
    f.render_widget(block, area);
    let inner = area.inner(ratatui::layout::Margin { vertical: 1, horizontal: 1 });
    let sub = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ]).split(inner);

    f.render_widget(
        Gauge::default()
            .gauge_style(Style::default().fg(gauge_color))
            .ratio(ratio)
            .label(gauge_label),
        sub[0],
    );
    f.render_widget(Paragraph::new(format!(" Free: {:.1} GB", gb(free))), sub[1]);
    f.render_widget(Paragraph::new(format!(" Files: {}", files)), sub[2]);
}

fn render_cacheio_pipeline(f: &mut Frame, area: Rect, state: &DashboardState) {
    let in_flight = state.in_flight_count.load(Relaxed);
    let deferred  = state.deferred_count.load(Relaxed);
    let done      = state.completed_copies.load(Relaxed);
    let failed    = state.failed_copies.load(Relaxed);
    let allowed   = state.caching_allowed.load(Relaxed);
    let ev_exp    = state.evictions_expired.load(Relaxed);
    let ev_size   = state.evictions_size.load(Relaxed);

    let window_str = if allowed { "OPEN" } else { "CLOSED" };
    let window_color = if allowed { Color::Green } else { Color::Red };

    let block = Block::default().borders(Borders::ALL).title(" Pipeline ");
    f.render_widget(block, area);
    let inner = area.inner(ratatui::layout::Margin { vertical: 1, horizontal: 1 });
    let sub = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(inner);

    f.render_widget(
        Paragraph::new(format!(
            " In-flight: {}   Deferred: {}   Completed: {}   Failed: {}",
            in_flight, deferred, done, failed,
        )),
        sub[0],
    );
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw(format!(" Window: ")),
            Span::styled(window_str, Style::default().fg(window_color)),
            Span::raw(format!("   Evictions — expired: {}  size-limit: {}", ev_exp, ev_size)),
        ])),
        sub[1],
    );
}

fn render_cacheio_inflight(f: &mut Frame, area: Rect, state: &DashboardState) {
    let block = Block::default().borders(Borders::ALL).title(" In-flight Transfers ");
    f.render_widget(block.clone(), area);
    let inner = area.inner(ratatui::layout::Margin { vertical: 1, horizontal: 1 });

    let copies = state.active_copies.lock().unwrap();
    let mut sorted: Vec<_> = copies.values().collect();
    sorted.sort_by_key(|c| c.started_at);

    if sorted.is_empty() {
        f.render_widget(
            Paragraph::new(" No active copies.").style(Style::default().fg(Color::DarkGray)),
            inner,
        );
        return;
    }

    // Each copy occupies 2 rows (title + gauge) + 1 spacer = 3 rows.
    let max_visible = (inner.height as usize / 3).max(1);
    let mut y = inner.y;

    for cp in sorted.iter().take(max_visible) {
        if y + 2 > inner.y + inner.height {
            break;
        }

        let title_area = Rect { x: inner.x, y, width: inner.width, height: 1 };
        let gauge_area = Rect { x: inner.x, y: y + 1, width: inner.width, height: 1 };

        let elapsed = cp.elapsed_secs();
        let path_str = truncate_path_tail(&cp.path, 3);
        // Right-align elapsed; pad path to fill the remaining space.
        let elapsed_label = format!("{}s", elapsed);
        let pad = (inner.width as usize).saturating_sub(path_str.len() + elapsed_label.len() + 1);
        let title_line = format!("{}{:pad$}{}", path_str, "", elapsed_label, pad = pad);
        f.render_widget(Paragraph::new(title_line), title_area);

        let ratio = if cp.size_bytes > 0 {
            (cp.bytes_copied as f64 / cp.size_bytes as f64).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let gauge_label = if cp.size_bytes > 0 {
            let pct = (ratio * 100.0) as u64;
            format!("{} / {}  ({}%)", fmt_bytes(cp.bytes_copied), fmt_bytes(cp.size_bytes), pct)
        } else {
            format!("{} / ?", fmt_bytes(cp.bytes_copied))
        };
        f.render_widget(
            Gauge::default()
                .gauge_style(Style::default().fg(Color::Cyan))
                .ratio(ratio)
                .label(gauge_label),
            gauge_area,
        );

        y += 3;
    }
}

// ---- Page 4: Logs ----

fn render_logs(f: &mut Frame, area: Rect, state: &DashboardState, scroll: usize) {
    let logs = state.recent_logs.lock().unwrap();
    let total = logs.len();
    let items: Vec<ListItem> = logs.iter().skip(scroll).map(|e| {
        let color = log_level_color(&e.level);
        ListItem::new(Line::from(vec![
            Span::styled(format!("{} ", e.timestamp), Style::default().fg(Color::DarkGray)),
            Span::styled(format!("{} ", e.level), Style::default().fg(color)),
            Span::raw(e.message.clone()),
        ]))
    }).collect();
    let title = format!(" Logs ({} entries  ↑↓: scroll  Home/End: jump) ", total);
    drop(logs);

    f.render_widget(
        List::new(items).block(Block::default().borders(Borders::ALL).title(title)),
        area,
    );
}

// ---- Formatting helpers ----

fn gb(bytes: u64) -> f64 {
    bytes as f64 / 1_073_741_824.0
}

fn fmt_bytes(bytes: u64) -> String {
    if bytes >= 1_073_741_824 {
        format!("{:.1} GB", gb(bytes))
    } else if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else {
        format!("{} KB", bytes / 1024)
    }
}

pub(super) fn fmt_time(t: SystemTime) -> String {
    crate::utils::fmt_time(t)
}

fn fmt_datetime(t: SystemTime) -> String {
    let secs = t.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_secs() as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&secs, &mut tm) };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday,
        tm.tm_hour, tm.tm_min, tm.tm_sec,
    )
}

fn fmt_relative(t: SystemTime) -> String {
    let elapsed = SystemTime::now().duration_since(t).unwrap_or(Duration::ZERO);
    let secs = elapsed.as_secs();
    if secs < 60 { format!("{}s ago", secs) }
    else if secs < 3600 { format!("{}m ago", secs / 60) }
    else if secs < 86400 { format!("{}h ago", secs / 3600) }
    else { format!("{}d ago", secs / 86400) }
}

fn fmt_duration_until(t: SystemTime) -> String {
    let remaining = t.duration_since(SystemTime::now()).unwrap_or(Duration::ZERO);
    let secs = remaining.as_secs();
    if secs == 0 { return "expired".to_string(); }
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    format!("{}h {}m", h, m)
}

/// Returns the last `n` path components joined by `/`, prefixed with `…/` if truncated.
/// Always preserves the filename: `…/Show/S01E01.mkv`, never `/mnt/Long/Path/…`.
fn truncate_path_tail(p: &Path, n: usize) -> String {
    let parts: Vec<_> = p.iter().collect();
    if parts.len() <= n {
        p.display().to_string()
    } else {
        let tail: Vec<String> = parts.iter().rev().take(n).rev()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        format!("…/{}", tail.join("/"))
    }
}

fn log_level_color(level: &str) -> Color {
    match level.trim() {
        "ERROR" => Color::Red,
        "WARN"  => Color::Yellow,
        "INFO"  => Color::Cyan,
        "DEBUG" => Color::DarkGray,
        "TRACE" => Color::DarkGray,
        _       => Color::White,
    }
}
