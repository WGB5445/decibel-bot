//! Interactive attach panel.
//!
//! The screen is a single, full-page scrollable status view. It uses the alternate screen so a
//! live redraw cannot corrupt the normal terminal buffer. Press `c` to copy the complete
//! (unscrolled) snapshot to the macOS clipboard.

use crate::{
    client::{ClientCommand, EngineClient},
    control::{EngineStatus, ExitMode, LadderLevel, PerpPnlStatus},
    monitor_log::{
        LogPanelState, MIN_LOG_WIDTH, MIN_MAIN_WIDTH, fold_log_line, render_log_panel, split_layout,
    },
};
use anyhow::{Context, Result};
use crossterm::{
    cursor::{Hide, Show},
    event::{DisableMouseCapture, Event, EventStream, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures_util::StreamExt;
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};
use rust_decimal::Decimal;
use std::{
    io::{self, Stdout, Write},
    panic,
    process::{Command, Stdio},
    str::FromStr,
    sync::Once,
    time::{Duration, Instant},
};
use tokio::time::MissedTickBehavior;

static PANIC_HOOK: Once = Once::new();

/// Bound rendering so a trackpad gesture cannot produce one terminal redraw per input event.
const FRAME_INTERVAL: Duration = Duration::from_millis(33);
const INPUT_RETRY_DELAY: Duration = Duration::from_secs(1);
const MAX_RENDERED_EVENTS: usize = 10;
fn restore_terminal() {
    let _ = disable_raw_mode();
    let mut stdout = io::stdout();
    // Do not clear or purge the main screen here: those operations destroy the user's
    // scrollback. Returning from the alternate screen should leave the original buffer intact.
    let _ = execute!(stdout, Show, DisableMouseCapture, LeaveAlternateScreen);
}
/// Owns the terminal's raw-mode and alternate-screen lifecycle.
///
/// The alternate screen is intentional. A continuously redrawn TUI cannot safely coexist with
/// native terminal text selection in the main buffer; `c` copies the full current snapshot
/// without requiring the user to select a transient frame.
pub struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalGuard {
    pub fn enter() -> Result<Self> {
        PANIC_HOOK.call_once(|| {
            let previous = panic::take_hook();
            panic::set_hook(Box::new(move |info| {
                restore_terminal();
                previous(info);
            }));
        });

        enable_raw_mode()?;
        let mut stdout = io::stdout();
        // Attach intentionally never captures the mouse: wheel input remains owned by the host
        // terminal. Do not clear or purge the main buffer here; it belongs to the user.
        if let Err(error) = execute!(stdout, DisableMouseCapture, EnterAlternateScreen, Hide) {
            restore_terminal();
            return Err(error.into());
        }
        match Terminal::new(CrosstermBackend::new(stdout)) {
            Ok(mut terminal) => {
                // This clears only the alternate screen so the first draw starts clean.
                if let Err(error) = terminal.clear() {
                    restore_terminal();
                    return Err(error.into());
                }
                Ok(Self { terminal })
            }
            Err(error) => {
                restore_terminal();
                Err(error.into())
            }
        }
    }

    fn draw(&mut self, app: &mut App) -> Result<()> {
        let terminal_size = self.terminal.size()?;
        let term_width = terminal_size.width;
        let term_height = terminal_size.height;
        let (_, log_rect) = split_layout(
            Rect::new(0, 0, term_width, 1),
            MIN_MAIN_WIDTH,
            MIN_LOG_WIDTH,
        );
        let width_ok = log_rect.is_some();
        let engine_has_log = app.log_panel.has_engine_log();
        let split_visible = width_ok && engine_has_log;
        let main_width = log_rect
            .map(|rect| term_width.saturating_sub(rect.width).saturating_sub(1))
            .unwrap_or(term_width);
        if split_visible {
            let log_width = log_rect
                .map(|rect| rect.width.saturating_sub(2) as usize)
                .unwrap_or(0);
            app.log_panel
                .update_scroll_bounds(term_height.saturating_sub(10) as usize, log_width);
        }
        // The main paragraph is inside a bordered panel. Its usable height also
        // depends on whether the Perp header contributes a fourth header line.
        let viewport_height = main_viewport_height(term_height, app.status.perp_mode.is_some());
        let recent_log_errors = if split_visible {
            app.log_panel.recent_error_lines(12)
        } else {
            Vec::new()
        };
        let content = snapshot_lines(
            &app.status,
            !split_visible,
            main_width.saturating_sub(2),
            &recent_log_errors,
        );
        let max_scroll = content.len().saturating_sub(viewport_height.max(1));
        app.update_scroll_bounds(max_scroll);
        self.terminal
            .draw(|frame| render(frame, app, &content, max_scroll, split_visible))?;
        Ok(())
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

#[derive(Default)]
struct App {
    status: EngineStatus,
    scroll: usize,
    max_scroll: usize,
    follow_latest: bool,
    log_panel: LogPanelState,
    confirm_liquidate: bool,
    connected: bool,
    subscribed: bool,
    received_snapshot: bool,
    live_sequence: u64,
    notice: String,
}

impl App {
    fn update_scroll_bounds(&mut self, max_scroll: usize) {
        self.max_scroll = max_scroll;
        self.scroll = if self.follow_latest {
            max_scroll
        } else {
            self.scroll.min(max_scroll)
        };
        self.follow_latest = self.scroll == max_scroll;
    }

    fn scroll_up(&mut self, amount: usize) {
        self.scroll = self.scroll.saturating_sub(amount);
        self.follow_latest = self.scroll == self.max_scroll;
    }

    fn scroll_down(&mut self, amount: usize) {
        self.scroll = self.scroll.saturating_add(amount).min(self.max_scroll);
        self.follow_latest = self.scroll == self.max_scroll;
    }

    fn scroll_to(&mut self, offset: usize) {
        self.scroll = offset.min(self.max_scroll);
        self.follow_latest = self.scroll == self.max_scroll;
    }
}

#[derive(Debug, PartialEq, Eq)]
enum EventOutcome {
    Continue,
    Redraw,
    Quit,
}

pub async fn run(client: EngineClient) -> Result<()> {
    let mut guard = TerminalGuard::enter()?;

    // Manual acceptance hook: validates terminal restoration before the panic report is printed.
    if std::env::var("GRID_ATTACH_PANIC_TEST").as_deref() == Ok("1") {
        panic!("intentional attach TUI panic test");
    }

    let mut updates: Option<tokio::sync::mpsc::Receiver<Result<EngineStatus>>> = None;
    let mut reconnect_at = Instant::now();
    let mut reconnect_backoff = Duration::from_secs(1);
    let mut input = Some(EventStream::new());
    let mut input_retry_at = None;
    let mut tick = tokio::time::interval(FRAME_INTERVAL);
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut app = App {
        notice: "Live updates disconnected; reconnecting...".to_owned(),
        ..Default::default()
    };
    let mut needs_redraw = true;

    loop {
        tokio::select! {
            biased;
            _ = tick.tick() => {
                let now = Instant::now();
                if app.log_panel.tailer.as_mut().is_some_and(|tailer| tailer.poll()) {
                    needs_redraw = true;
                }
                if updates.is_none() && now >= reconnect_at {
                    match client.subscribe_updates().await {
                        Ok(receiver) => {
                            updates = Some(receiver);
                            reconnect_backoff = Duration::from_secs(1);
                            app.connected = true;
                            app.subscribed = false;
                            app.notice = "Connected; waiting for the initial snapshot...".to_owned();
                            needs_redraw = true;
                        }
                        Err(error) => {
                            app.connected = false;
                            app.subscribed = false;
                            app.notice = format!(
                                "Live updates disconnected; retrying in {}s: {error:#}",
                                reconnect_backoff.as_secs(),
                            );
                            reconnect_at = now + reconnect_backoff;
                            reconnect_backoff = (reconnect_backoff * 2).min(Duration::from_secs(30));
                            needs_redraw = true;
                        }
                    }
                }
                if input.is_none() && input_retry_at.is_some_and(|retry_at| now >= retry_at) {
                    input = Some(EventStream::new());
                    input_retry_at = None;
                    app.notice = "Terminal input restored.".to_owned();
                    needs_redraw = true;
                }
                if needs_redraw {
                    guard.draw(&mut app)?;
                    needs_redraw = false;
                }
            }
            update = async {
                match &mut updates {
                    Some(receiver) => receiver.recv().await,
                    None => std::future::pending().await,
                }
            } => match update {
                Some(Ok(status)) => {
                    // Keep the last complete snapshot during later reconnects. This update is
                    // the authoritative initial snapshot or a subsequent full state broadcast.
                    app.status = status;
                    app.log_panel
                        .sync_engine_log_path(app.status.log_path.as_deref());
                    app.connected = true;
                    app.subscribed = true;
                    app.received_snapshot = true;
                    app.live_sequence = app.live_sequence.saturating_add(1);
                    app.notice.clear();
                    needs_redraw = true;
                }
                Some(Err(error)) => {
                    updates = None;
                    app.connected = false;
                    app.subscribed = false;
                    app.notice = format!("Live updates disconnected; reconnecting: {error:#}");
                    reconnect_at = Instant::now() + reconnect_backoff;
                    reconnect_backoff = (reconnect_backoff * 2).min(Duration::from_secs(30));
                    needs_redraw = true;
                }
                None => {
                    updates = None;
                    app.connected = false;
                    app.subscribed = false;
                    app.notice = "Live updates disconnected; reconnecting...".to_owned();
                    reconnect_at = Instant::now() + reconnect_backoff;
                    reconnect_backoff = (reconnect_backoff * 2).min(Duration::from_secs(30));
                    needs_redraw = true;
                }
            },
            event = async {
                match &mut input {
                    Some(stream) => stream.next().await,
                    None => std::future::pending().await,
                }
            } => match event {
                // Mouse capture is disabled, but some terminal multiplexers can still forward
                // wheel reports. Ignore them without querying terminal size or requesting a draw.
                Some(Ok(Event::Mouse(_))) => {}
                Some(Ok(event)) => {
                    let term_size = guard.terminal.size()?;
                    let (_, log_rect) = split_layout(
                        Rect::new(0, 0, term_size.width, 1),
                        MIN_MAIN_WIDTH,
                        MIN_LOG_WIDTH,
                    );
                    match handle_event(
                        event,
                        &mut app,
                        &client,
                        term_size.height,
                        log_rect.is_some(),
                    )
                    .await? {
                        EventOutcome::Quit => break,
                        EventOutcome::Redraw => needs_redraw = true,
                        EventOutcome::Continue => {}
                    }
                }
                Some(Err(error)) => {
                    input = None;
                    input_retry_at = Some(Instant::now() + INPUT_RETRY_DELAY);
                    app.notice = format!("Terminal input failed; retrying in 1s: {error}");
                    needs_redraw = true;
                }
                None => {
                    input = None;
                    input_retry_at = Some(Instant::now() + INPUT_RETRY_DELAY);
                    app.notice = "Terminal input stream closed; retrying in 1s.".to_owned();
                    needs_redraw = true;
                }
            }
        }
    }
    Ok(())
}

async fn handle_event(
    event: Event,
    app: &mut App,
    client: &EngineClient,
    term_height: u16,
    log_split_visible: bool,
) -> Result<EventOutcome> {
    let outcome = match event {
        Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                EventOutcome::Quit
            }
            KeyCode::Char('q') | KeyCode::Esc if !app.confirm_liquidate => EventOutcome::Quit,
            KeyCode::Char('s') if !app.confirm_liquidate => {
                app.confirm_liquidate = true;
                EventOutcome::Redraw
            }
            KeyCode::Char('y') if app.confirm_liquidate => {
                app.notice = client
                    .send_command(ClientCommand::Stop {
                        exit_mode: ExitMode::Liquidate,
                    })
                    .await?;
                app.confirm_liquidate = false;
                EventOutcome::Redraw
            }
            KeyCode::Char('n') | KeyCode::Esc if app.confirm_liquidate => {
                app.confirm_liquidate = false;
                EventOutcome::Redraw
            }
            KeyCode::Char('c') if !app.confirm_liquidate => {
                app.notice = match copy_snapshot_to_clipboard(&app.status, &app.log_panel) {
                    Ok(()) => "Copied the complete snapshot to the clipboard.".to_owned(),
                    Err(error) => format!("Could not copy snapshot: {error:#}"),
                };
                EventOutcome::Redraw
            }
            KeyCode::Char('m') if !app.confirm_liquidate => {
                app.notice = match copy_monitor_to_clipboard(&app.status) {
                    Ok(()) => "Copied the monitor pane to the clipboard.".to_owned(),
                    Err(error) => format!("Could not copy monitor pane: {error:#}"),
                };
                EventOutcome::Redraw
            }
            KeyCode::Char('l') if !app.confirm_liquidate => {
                app.notice = match copy_log_to_clipboard(&app.log_panel) {
                    Ok(()) => "Copied the log pane to the clipboard.".to_owned(),
                    Err(error) => format!("Could not copy log pane: {error:#}"),
                };
                EventOutcome::Redraw
            }
            KeyCode::Char('[') if log_split_visible && !app.confirm_liquidate => {
                app.log_panel.scroll_up(1);
                EventOutcome::Redraw
            }
            KeyCode::Char(']') if log_split_visible && !app.confirm_liquidate => {
                app.log_panel.scroll_down(1);
                EventOutcome::Redraw
            }
            KeyCode::Char('f') if log_split_visible && !app.confirm_liquidate => {
                app.log_panel.toggle_follow();
                EventOutcome::Redraw
            }
            // Content starts at the top when scroll=0. Going down means a larger offset.
            KeyCode::Up => {
                app.scroll_up(1);
                EventOutcome::Redraw
            }
            KeyCode::Down => {
                app.scroll_down(1);
                EventOutcome::Redraw
            }
            KeyCode::PageUp => {
                app.scroll_up(page_step(term_height));
                EventOutcome::Redraw
            }
            KeyCode::PageDown => {
                app.scroll_down(page_step(term_height));
                EventOutcome::Redraw
            }
            KeyCode::Home | KeyCode::Char('g') => {
                app.scroll_to(0);
                EventOutcome::Redraw
            }
            KeyCode::End | KeyCode::Char('G') => {
                app.scroll_to(app.max_scroll);
                EventOutcome::Redraw
            }
            _ => EventOutcome::Continue,
        },
        // Do not capture mouse-wheel input. It remains available to the host terminal, as users
        // expect for terminal scrollback and selection behavior.
        Event::Mouse(_) => EventOutcome::Continue,
        Event::Resize(_, _) => EventOutcome::Redraw,
        _ => EventOutcome::Continue,
    };
    Ok(outcome)
}

fn page_step(term_height: u16) -> usize {
    (term_height.saturating_sub(10) as usize).max(1)
}

fn main_viewport_height(term_height: u16, is_perp: bool) -> usize {
    let header_height = if is_perp { 6 } else { 5 };
    // Header + footer + main-panel borders.
    term_height.saturating_sub(header_height + 3 + 2).max(1) as usize
}

fn snapshot_lines(
    status: &EngineStatus,
    include_events: bool,
    main_width: u16,
    recent_log_errors: &[String],
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let base_symbol = status.pfs_base_symbol.as_deref().unwrap_or("BASE");
    let base_balance = status.pfs_base_balance.as_deref().unwrap_or("-");
    let quote_symbol = status.pfs_quote_symbol.as_deref().unwrap_or("QUOTE");
    let quote_balance = status.pfs_quote_balance.as_deref().unwrap_or("-");

    if let Some(summary) = perp_summary_text(status) {
        for row in fold_log_line(&summary, main_width as usize) {
            lines.push(Line::styled(
                row,
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ));
        }
    }
    if let Some(pnl) = &status.perp_pnl {
        lines.push(Line::styled(
            "PERP ACCOUNTING",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
        for row in perp_accounting_text(pnl) {
            for wrapped in fold_log_line(&row, main_width as usize) {
                lines.push(Line::from(wrapped));
            }
        }
    }

    lines.push(Line::from(vec![
        Span::styled("PFS balances  ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("{base_symbol}: {base_balance}"),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw("   "),
        Span::styled(
            format!("{quote_symbol}: {quote_balance}"),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw("   "),
        Span::styled("Realized PnL: ", Style::default().fg(Color::DarkGray)),
        Span::raw(
            status
                .realized_pnl
                .as_deref()
                .unwrap_or("unavailable")
                .to_owned(),
        ),
    ]));
    lines.push(Line::from(vec![
        Span::styled("Reconcile     ", Style::default().fg(Color::DarkGray)),
        Span::raw(format!(
            "matched={}  missing={}  unmanaged={}",
            status
                .matched
                .map_or_else(|| "-".to_owned(), |count| count.to_string()),
            status
                .missing
                .map_or_else(|| "-".to_owned(), |count| count.to_string()),
            status
                .unmanaged
                .map_or_else(|| "-".to_owned(), |count| count.to_string()),
        )),
    ]));
    if let Some(error) = &status.last_error {
        lines.push(Line::styled(
            "Last engine error:",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ));
        append_wrapped_error(&mut lines, error, main_width as usize);
    } else if !recent_log_errors.is_empty() {
        lines.push(Line::styled(
            "ENGINE LOG ERRORS (latest)",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ));
        for error in recent_log_errors {
            append_wrapped_error(&mut lines, error, main_width as usize);
        }
    }
    if let Some(reason) = &status.perp_blocked_reason {
        lines.push(Line::styled(
            "Perp blocked:",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ));
        append_wrapped_error(&mut lines, reason, main_width as usize);
    }

    lines.push(Line::raw(""));
    lines.push(Line::styled(
        format!("GRID LEVELS ({})", status.ladder.len()),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ));
    let columns = ladder_columns_for_width(main_width);
    lines.push(Line::styled(
        format!(
            "{:<width_side$} {:>width_price$} {:>width_size$} {:>width_status$}",
            "SIDE",
            "PRICE (QUOTE)",
            "QTY (BASE)",
            "STATUS",
            width_side = columns.side,
            width_price = columns.price,
            width_size = columns.size,
            width_status = columns.status,
        ),
        Style::default().fg(Color::DarkGray),
    ));
    lines.push(Line::styled(
        "-".repeat(columns.total().min(main_width as usize)),
        Style::default().fg(Color::DarkGray),
    ));

    let levels = ordered_levels(&status.ladder);
    if levels.is_empty() {
        lines.push(Line::styled(
            "(no grid levels in the latest snapshot)",
            Style::default().fg(Color::DarkGray),
        ));
    } else {
        for level in levels {
            lines.push(ladder_line(level, columns));
        }
    }

    if include_events {
        let rendered_event_count = status.events.len().min(MAX_RENDERED_EVENTS);
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            format!(
                "EVENTS (latest {rendered_event_count} / {})",
                status.events.len()
            ),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
        lines.push(Line::styled(
            "-".repeat(columns.total().min(main_width as usize)),
            Style::default().fg(Color::DarkGray),
        ));
        if status.events.is_empty() {
            lines.push(Line::styled(
                "(no events in the latest snapshot)",
                Style::default().fg(Color::DarkGray),
            ));
        } else {
            for event in status.events.iter().rev().take(MAX_RENDERED_EVENTS) {
                let event_rows =
                    fold_log_line(&event.message, (main_width as usize).saturating_sub(10));
                for (index, row) in event_rows.into_iter().enumerate() {
                    let prefix = if index == 0 {
                        event.at.format("%H:%M:%S").to_string()
                    } else {
                        "        ".to_owned()
                    };
                    lines.push(Line::from(vec![
                        Span::styled(prefix, Style::default().fg(Color::DarkGray)),
                        Span::raw("  "),
                        Span::raw(row),
                    ]));
                }
            }
        }
    }
    lines
}

fn append_wrapped_error(lines: &mut Vec<Line<'static>>, error: &str, width: usize) {
    for source_line in error.lines() {
        for row in fold_log_line(source_line, width.max(1)) {
            lines.push(Line::styled(row, Style::default().fg(Color::Red)));
        }
    }
}

fn ordered_levels(levels: &[LadderLevel]) -> Vec<&LadderLevel> {
    let mut bids = levels
        .iter()
        .filter(|l| l.side.eq_ignore_ascii_case("BID"))
        .collect::<Vec<_>>();
    let mut asks = levels
        .iter()
        .filter(|l| l.side.eq_ignore_ascii_case("ASK"))
        .collect::<Vec<_>>();
    let mut other: Vec<&LadderLevel> = levels
        .iter()
        .filter(|l| !l.side.eq_ignore_ascii_case("BID") && !l.side.eq_ignore_ascii_case("ASK"))
        .collect();

    // BID: 按价格升序排列(最低在最上面 = 从最便宜的 bid 开始展示)
    bids.sort_by(|a, b| parse_decimal(&a.price).cmp(&parse_decimal(&b.price)));
    // ASK: 按价格升序排列(最低在最上面 = best ask 先可见)
    asks.sort_by(|a, b| parse_decimal(&a.price).cmp(&parse_decimal(&b.price)));
    other.sort_by(|a, b| parse_decimal(&a.price).cmp(&parse_decimal(&b.price)));

    // 输出顺序: 全部 BID(低→高)在上, 全部 ASK(低→高)在下
    bids.extend(asks);
    bids.extend(other);
    bids
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LadderColumns {
    side: usize,
    price: usize,
    size: usize,
    status: usize,
}

impl LadderColumns {
    fn total(&self) -> usize {
        self.side + self.price + self.size + self.status + 3
    }
}

fn ladder_columns_for_width(available: u16) -> LadderColumns {
    const MIN_SIDE: usize = 4;
    const MIN_STATUS: usize = 8;
    const MIN_PRICE: usize = 6;
    const MIN_SIZE: usize = 6;
    const GAPS: usize = 3;

    let available = available.max(1) as usize;
    if available <= MIN_SIDE + MIN_STATUS + GAPS {
        return LadderColumns {
            side: MIN_SIDE.min(available.saturating_sub(MIN_STATUS + GAPS)),
            price: 1,
            size: 1,
            status: MIN_STATUS.min(available.saturating_sub(MIN_SIDE + GAPS)),
        };
    }

    let fixed = MIN_SIDE + MIN_STATUS + GAPS;
    let flex = available.saturating_sub(fixed);
    let mut price = ((flex as f64) * 0.55).round() as usize;
    let mut size = flex.saturating_sub(price);
    price = price.clamp(MIN_PRICE, flex);
    size = size.clamp(MIN_SIZE, flex);
    let mut columns = LadderColumns {
        side: MIN_SIDE,
        price,
        size,
        status: MIN_STATUS,
    };
    while columns.total() > available && columns.price > 1 {
        columns.price -= 1;
    }
    while columns.total() > available && columns.size > 1 {
        columns.size -= 1;
    }
    columns
}

fn ladder_line(level: &LadderLevel, columns: LadderColumns) -> Line<'static> {
    let side_is_bid = level.side.eq_ignore_ascii_case("BID");
    let side_is_ask = level.side.eq_ignore_ascii_case("ASK");
    let side_style = if side_is_bid {
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD)
    } else if side_is_ask {
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    };
    let price_style = Style::default()
        .fg(Color::White)
        .add_modifier(Modifier::BOLD);
    let state = display_state(&level.state);
    let state_style = if state == "Active" {
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD)
    } else if state == "Planned" {
        Style::default().fg(Color::Yellow)
    } else if state == "Unmanaged" || state == "Cancelled" {
        Style::default().fg(Color::Yellow)
    } else if state == "Failed" || state == "Rejected" {
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    Line::from(vec![
        Span::styled(
            format!(
                "{:<width$}",
                level.side.to_ascii_uppercase(),
                width = columns.side
            ),
            side_style,
        ),
        Span::styled(
            format!(
                "{:>width$}",
                format_decimal(&level.price, 8),
                width = columns.price
            ),
            price_style,
        ),
        Span::styled(
            format!(
                "{:>width$}",
                format_decimal(&level.size, 6),
                width = columns.size
            ),
            side_style,
        ),
        Span::styled(
            format!("{:>width$}", state, width = columns.status),
            state_style,
        ),
    ])
}

fn display_state(state: &str) -> &str {
    if state.eq_ignore_ascii_case("placed") || state.eq_ignore_ascii_case("resting") {
        "Active"
    } else if state.eq_ignore_ascii_case("planned") {
        "Planned"
    } else if state.eq_ignore_ascii_case("cancelled") || state.eq_ignore_ascii_case("canceled") {
        "Cancelled"
    } else if state.eq_ignore_ascii_case("failed") {
        "Failed"
    } else if state.eq_ignore_ascii_case("rejected") {
        "Rejected"
    } else {
        state
    }
}

fn parse_decimal(value: &str) -> Option<Decimal> {
    Decimal::from_str(value).ok()
}

fn format_decimal(value: &str, scale: usize) -> String {
    parse_decimal(value)
        .map(|decimal| format!("{decimal:.scale$}"))
        .unwrap_or_else(|| value.to_owned())
}

fn short_account(account: &str) -> String {
    if account.len() <= 14 {
        account.to_owned()
    } else {
        format!("{}...{}", &account[..8], &account[account.len() - 4..])
    }
}

fn perp_summary_text(status: &EngineStatus) -> Option<String> {
    let mode = status.perp_mode.as_deref()?;
    let position = status.position.as_deref().unwrap_or("-");
    let target = status.target_position.as_deref().unwrap_or("-");
    let bootstrap = status.perp_bootstrap_status.as_deref().unwrap_or("-");
    let delta = status.convergence_delta.as_deref().unwrap_or("-");
    let max_position = status.max_position.as_deref().unwrap_or("-");
    let available = status.available_margin.as_deref().unwrap_or("-");
    let estimated = status.estimated_margin.as_deref().unwrap_or("-");
    Some(format!(
        "Perp: {}  pos={}  bootstrap-target={}  bootstrap={}  Δ={}  max={}  margin={}/{} USDC",
        mode.to_ascii_uppercase(),
        position,
        target,
        bootstrap,
        delta,
        max_position,
        available,
        estimated,
    ))
}

fn perp_accounting_text(pnl: &PerpPnlStatus) -> [String; 3] {
    let unavailable = "unavailable";
    let net = pnl.net_pnl_quote.as_deref().unwrap_or(unavailable);
    let unrealized = pnl.unrealized_gross_quote.as_deref().unwrap_or(unavailable);
    let fees = pnl.trade_fees_quote.as_deref().unwrap_or(unavailable);
    let funding = pnl.funding_pnl_quote.as_deref().unwrap_or(unavailable);
    let last_fill = pnl
        .last_fill_at
        .map(|time| time.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|| "-".to_owned());
    let last_funding = pnl
        .last_funding_at
        .map(|time| time.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|| "-".to_owned());
    [
        format!(
            "Position exchange/ledger/delta: {}/{}/{}  avg entry: {}  mark: {}",
            pnl.exchange_position_base,
            pnl.ledger_position_base,
            pnl.reconciliation_delta_base,
            pnl.average_entry_price.as_deref().unwrap_or(unavailable),
            pnl.mark_price.as_deref().unwrap_or(unavailable),
        ),
        format!(
            "PnL gross realized/unrealized: {}/{}  trade fees: {}  funding: {}  net: {}",
            pnl.realized_gross_quote, unrealized, fees, funding, net,
        ),
        format!(
            "Accounting completeness: fees={} funding={}  last fill: {}  last funding: {}",
            if pnl.fees_complete {
                "complete"
            } else {
                "missing"
            },
            if pnl.funding_complete {
                "complete"
            } else {
                "unavailable"
            },
            last_fill,
            last_funding,
        ),
    ]
}

fn perp_summary_line(status: &EngineStatus) -> Option<Line<'static>> {
    let mode = status.perp_mode.as_deref()?;
    let position = status.position.as_deref().unwrap_or("-").to_owned();
    let target = status.target_position.as_deref().unwrap_or("-").to_owned();
    let bootstrap = status
        .perp_bootstrap_status
        .as_deref()
        .unwrap_or("-")
        .to_owned();
    let delta = status
        .convergence_delta
        .as_deref()
        .unwrap_or("-")
        .to_owned();
    let action = status
        .out_of_range_action
        .as_deref()
        .unwrap_or("-")
        .to_owned();
    let available = status.available_margin.as_deref().unwrap_or("-").to_owned();
    let estimated = status.estimated_margin.as_deref().unwrap_or("-").to_owned();
    Some(Line::from(vec![
        Span::styled("Perp: ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!(
                "{} pos/bootstrap-target={}/{} bootstrap={} Δ={} margin={}/{} oor={}{}{}",
                mode.to_ascii_uppercase(),
                position,
                target,
                bootstrap,
                delta,
                available,
                estimated,
                action,
                if status.paused_by_out_of_range {
                    " PAUSED"
                } else {
                    ""
                },
                status
                    .perp_blocked_reason
                    .as_deref()
                    .map(|_| " blocked".to_owned())
                    .unwrap_or_default(),
            ),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
    ]))
}

fn product_tag(product: &str) -> (&'static str, Style) {
    if product.eq_ignore_ascii_case("spot") {
        (
            "[S]",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        (
            "[P]",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
    }
}

fn snapshot_plain_text(status: &EngineStatus, include_events: bool, main_width: u16) -> String {
    snapshot_lines(status, include_events, main_width, &[])
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

fn copy_snapshot_to_clipboard(status: &EngineStatus, log_panel: &LogPanelState) -> Result<()> {
    let mut text = snapshot_plain_text(status, true, u16::MAX);
    let recent_logs = log_panel.recent_lines(200);
    if !recent_logs.is_empty() {
        text.push_str("\n\nENGINE LOG (recent)\n");
        text.push_str(&"-".repeat(40));
        text.push('\n');
        text.push_str(&recent_logs.join("\n"));
    }
    copy_text_to_clipboard(&text)
}

fn copy_monitor_to_clipboard(status: &EngineStatus) -> Result<()> {
    copy_text_to_clipboard(&snapshot_plain_text(status, true, u16::MAX))
}

fn copy_log_to_clipboard(log_panel: &LogPanelState) -> Result<()> {
    let lines = log_panel.recent_lines(200);
    if lines.is_empty() {
        anyhow::bail!("the engine log pane has no lines to copy")
    }
    copy_text_to_clipboard(&lines.join("\n"))
}

fn copy_text_to_clipboard(text: &str) -> Result<()> {
    let mut child = Command::new("pbcopy")
        .stdin(Stdio::piped())
        .spawn()
        .context("start macOS clipboard command (pbcopy)")?;
    child
        .stdin
        .as_mut()
        .context("open clipboard command input")?
        .write_all(text.as_bytes())
        .context("write snapshot to clipboard")?;
    let result = child.wait().context("wait for clipboard command")?;
    if result.success() {
        Ok(())
    } else {
        anyhow::bail!("pbcopy exited with {result}")
    }
}

fn snapshot_indicator(app: &App) -> Span<'static> {
    if let Some(error) = &app.status.last_error {
        Span::styled(
            format!("REST snapshot FAILED: {error}"),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )
    } else if app.received_snapshot {
        Span::styled(
            format!("REST snapshot OK: {} grid levels", app.status.ladder.len()),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled(
            "REST snapshot pending",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
    }
}

fn render(
    frame: &mut ratatui::Frame,
    app: &App,
    content: &[Line<'static>],
    max_scroll: usize,
    split_visible: bool,
) {
    let area = frame.area();
    let header_height = if app.status.perp_mode.is_some() { 6 } else { 5 };
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(header_height),
            Constraint::Min(3),
            Constraint::Length(3),
        ])
        .split(area);

    let connection = if app.connected {
        Span::styled(
            "● Connected",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled(
            "● Disconnected",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )
    };
    let subscription = if app.subscribed {
        Span::styled(
            "● Subscribed",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        )
    } else if app.connected {
        Span::styled(
            "● Waiting for snapshot",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled(
            "● Live updates retrying",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )
    };
    let snapshot = snapshot_indicator(app);
    let last_update = app
        .status
        .last_cycle_at
        .map(|at| at.format("%H:%M:%S").to_string())
        .unwrap_or_else(|| "-".to_owned());
    let (tag, tag_style) = product_tag(&app.status.product);
    let mut header_lines = vec![
        Line::from(vec![connection, Span::raw("   "), subscription]),
        Line::from(vec![
            Span::styled("Account: ", Style::default().fg(Color::DarkGray)),
            Span::raw(short_account(&app.status.subaccount)),
            Span::styled("   Network: ", Style::default().fg(Color::DarkGray)),
            Span::raw(&app.status.network),
            Span::styled("   Engine: ", Style::default().fg(Color::DarkGray)),
            Span::raw(&app.status.phase),
            Span::raw("   "),
            snapshot,
        ]),
        Line::from(vec![
            Span::styled(tag, tag_style),
            Span::raw(" "),
            Span::styled(
                &app.status.market,
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    "  · live seq={}  · updated {}",
                    app.live_sequence, last_update
                ),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled("  · mid ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                app.status.mid.as_deref().unwrap_or("-"),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
    ];
    if let Some(perp_line) = perp_summary_line(&app.status) {
        header_lines.push(perp_line);
    }

    frame.render_widget(
        Paragraph::new(header_lines)
            .block(Block::default().borders(Borders::ALL).title("Grid monitor")),
        rows[0],
    );

    let (main_rect, log_rect) = if split_visible {
        split_layout(rows[1], MIN_MAIN_WIDTH, MIN_LOG_WIDTH)
    } else {
        (rows[1], None)
    };
    frame.render_widget(
        Paragraph::new(content.to_vec())
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Price order: BID low→high, ASK low→high"),
            )
            .scroll((app.scroll.min(u16::MAX as usize) as u16, 0))
            .wrap(Wrap { trim: false }),
        main_rect,
    );
    if let Some(log_area) = log_rect {
        render_log_panel(frame, log_area, &app.log_panel);
    }

    let viewport_height = main_rect.height.saturating_sub(2) as usize;
    let start = app.scroll.min(max_scroll).saturating_add(1);
    let end = start
        .saturating_add(viewport_height)
        .saturating_sub(1)
        .min(content.len());
    let scroll_mode = if app.follow_latest {
        "following latest"
    } else {
        "manual scroll"
    };
    let log_controls = if split_visible {
        "  |  [ / ] log scroll  f follow"
    } else {
        ""
    };
    let controls = format!(
        "Lines {start}-{end} / {} ({scroll_mode})  |  Up/Down scroll  PgUp/PgDn page  Home/End bounds  c all  m monitor  l log  s liquidate  q/Esc/Ctrl+C quit{log_controls}",
        content.len(),
    );
    let notice_style = if app.connected {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::styled(controls, Style::default().fg(Color::DarkGray)),
            Line::styled(app.notice.clone(), notice_style),
        ])
        .block(Block::default().borders(Borders::ALL)),
        rows[2],
    );

    if app.confirm_liquidate {
        let popup = Rect {
            x: area.width.saturating_sub(62) / 2,
            y: area.height.saturating_sub(7) / 2,
            width: 62.min(area.width),
            height: 7.min(area.height),
        };
        frame.render_widget(Clear, popup);
        frame.render_widget(
            Paragraph::new(
                "Stop the engine, cancel the ladder, and liquidate Spot base?\n\n[y] confirm liquidation    [n/Esc] cancel",
            )
            .style(Style::default().fg(Color::Yellow))
            .block(Block::default().borders(Borders::ALL).title("Confirm liquidation")),
            popup,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{EngineEvent, PerpPnlStatus};
    use chrono::Utc;

    fn level(side: &str, price: &str) -> LadderLevel {
        LadderLevel {
            side: side.into(),
            price: price.into(),
            size: "2".into(),
            state: "Placed".into(),
        }
    }

    #[test]
    fn snapshot_includes_perp_fields_when_present() {
        let status = EngineStatus {
            perp_mode: Some("long".to_owned()),
            max_position: Some("0.01".to_owned()),
            position: Some("0.002".to_owned()),
            available_margin: Some("120".to_owned()),
            estimated_margin: Some("500".to_owned()),
            ..EngineStatus::default()
        };

        let snapshot = snapshot_plain_text(&status, true, 120);
        assert!(snapshot.contains("Perp: LONG"));
        assert!(snapshot.contains("pos=0.002"));
        assert!(snapshot.contains("max=0.01"));
        assert!(snapshot.contains("margin=120/500 USDC"));
    }

    #[test]
    fn snapshot_labels_incomplete_perp_pnl_without_fabricating_a_net_value() {
        let status = EngineStatus {
            perp_mode: Some("neutral".to_owned()),
            perp_pnl: Some(PerpPnlStatus {
                exchange_position_base: "1".to_owned(),
                ledger_position_base: "1".to_owned(),
                reconciliation_delta_base: "0".to_owned(),
                average_entry_price: Some("100".to_owned()),
                mark_price: Some("105".to_owned()),
                unrealized_gross_quote: Some("5".to_owned()),
                realized_gross_quote: "2".to_owned(),
                trade_fees_quote: Some("1".to_owned()),
                funding_pnl_quote: None,
                net_pnl_quote: None,
                fees_complete: true,
                funding_complete: false,
                last_fill_at: None,
                last_funding_at: None,
            }),
            ..EngineStatus::default()
        };

        let snapshot = snapshot_plain_text(&status, true, 120);
        assert!(snapshot.contains("PERP ACCOUNTING"));
        assert!(snapshot.contains("gross realized/unrealized: 2/5"));
        assert!(snapshot.contains("net: unavailable"));
        assert!(snapshot.contains("funding=unavailable"));
    }

    #[test]
    fn snapshot_contains_all_ladder_and_event_rows() {
        let status = EngineStatus {
            ladder: vec![level("BID", "1.0"), level("ASK", "1.1")],
            events: vec![EngineEvent {
                at: Utc::now(),
                message: "reconciled".into(),
            }],
            ..EngineStatus::default()
        };

        let snapshot = snapshot_plain_text(&status, true, 120);
        assert!(snapshot.contains("BID"));
        assert!(snapshot.contains("ASK"));
        assert!(snapshot.contains("reconciled"));
    }

    #[test]
    fn snapshot_shows_only_the_ten_latest_events() {
        let status = EngineStatus {
            events: (0..=10)
                .map(|index| EngineEvent {
                    at: Utc::now(),
                    message: format!("event-{index:02}"),
                })
                .collect(),
            ..EngineStatus::default()
        };

        let snapshot = snapshot_plain_text(&status, true, 120);
        assert!(snapshot.contains("EVENTS (latest 10 / 11)"));
        assert!(snapshot.contains("event-10"));
        assert!(snapshot.contains("event-01"));
        assert!(!snapshot.contains("event-00"));
    }

    #[test]
    fn grid_levels_are_bid_then_ask_with_each_side_ordered() {
        let levels = vec![
            level("ASK", "1.20"),
            level("BID", "1.10"),
            level("ASK", "1.15"),
            level("BID", "1.05"),
        ];
        let ordered = ordered_levels(&levels);
        let prices = ordered
            .iter()
            .map(|level| format!("{}:{}", level.side, level.price))
            .collect::<Vec<_>>();
        // BID: 从小到大(最便宜的 bid 在最上面), ASK: 从小到大(最便宜的 ask 在最上面)
        assert_eq!(prices, ["BID:1.05", "BID:1.10", "ASK:1.15", "ASK:1.20"]);
    }

    #[test]
    fn formats_quantities_and_prices_at_fixed_precision() {
        assert_eq!(format_decimal("50.68", 6), "50.680000");
        assert_eq!(format_decimal("0.5559", 8), "0.55590000");
    }

    #[test]
    fn page_step_is_a_full_viewport() {
        assert_eq!(page_step(1), 1);
        assert_eq!(page_step(24), 14);
    }

    #[test]
    fn split_snapshot_omits_events_when_requested() {
        let status = EngineStatus {
            events: vec![EngineEvent {
                at: Utc::now(),
                message: "reconciled".into(),
            }],
            ..EngineStatus::default()
        };

        let with_events = snapshot_plain_text(&status, true, 120);
        let without_events = snapshot_plain_text(&status, false, 120);
        assert!(with_events.contains("EVENTS"));
        assert!(with_events.contains("reconciled"));
        assert!(!without_events.contains("EVENTS"));
        assert!(!without_events.contains("reconciled"));
    }

    #[test]
    fn ladder_columns_for_width_allocates_flex_space() {
        let narrow = ladder_columns_for_width(30);
        assert!(narrow.total() <= 30);

        let wide = ladder_columns_for_width(120);
        assert!(wide.price >= 6);
        assert!(wide.size >= 6);
        assert_eq!(wide.side, 4);
        assert_eq!(wide.status, 8);
    }

    #[test]
    fn resting_ladder_levels_display_as_active() {
        assert_eq!(display_state("Resting"), "Active");
        assert_eq!(display_state("planned"), "Planned");
    }

    #[test]
    fn long_error_is_expanded_into_scrollable_snapshot_rows() {
        let error = "simulation failed because the market-order transaction was rejected by the gas station";
        let status = EngineStatus {
            last_error: Some(error.to_owned()),
            ..EngineStatus::default()
        };

        let lines = snapshot_lines(&status, false, 72, &[]);
        let header = lines
            .iter()
            .position(|line| line.to_string() == "Last engine error:")
            .expect("error header");
        assert!(lines.len() > header + 2);
        assert!(
            lines[header + 1]
                .to_string()
                .starts_with("simulation failed")
        );
    }

    #[test]
    fn perp_main_viewport_accounts_for_header_and_borders() {
        assert_eq!(main_viewport_height(24, false), 14);
        assert_eq!(main_viewport_height(24, true), 13);
    }

    #[test]
    fn manual_scroll_stays_clamped_and_following_resumes_at_bottom() {
        let mut app = App::default();
        app.update_scroll_bounds(5);
        assert_eq!(app.scroll, 0);
        assert!(!app.follow_latest);

        app.scroll_down(2);
        assert_eq!(app.scroll, 2);
        assert!(!app.follow_latest);

        app.update_scroll_bounds(8);
        assert_eq!(app.scroll, 2);
        assert!(!app.follow_latest);

        app.scroll_down(usize::MAX);
        assert_eq!(app.scroll, 8);
        assert!(app.follow_latest);

        app.update_scroll_bounds(11);
        assert_eq!(app.scroll, 11);
        assert!(app.follow_latest);

        app.scroll_up(1);
        assert_eq!(app.scroll, 10);
        assert!(!app.follow_latest);
    }
}
