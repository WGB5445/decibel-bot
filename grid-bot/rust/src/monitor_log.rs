//! Shared engine-log tailing and colored rendering for Grid monitor UIs.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::Line,
    widgets::{Block, Borders, Paragraph, Wrap},
};
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub const MIN_MAIN_WIDTH: u16 = 72;
pub const MIN_LOG_WIDTH: u16 = 40;
pub const LOG_BUFFER_LINES: usize = 500;
pub const LOG_POLL_INTERVAL: Duration = Duration::from_millis(400);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogSeverity {
    Info,
    Success,
    Warn,
    Error,
}

pub fn classify_log_line(line: &str) -> LogSeverity {
    let lower = line.to_ascii_lowercase();
    if lower.contains("error")
        || lower.contains("failed")
        || lower.contains("rejected")
        || lower.contains("risk rejected")
        || lower.contains("blocked")
        || lower.contains("panic")
    {
        LogSeverity::Error
    } else if lower.contains("warning")
        || lower.contains("skipped")
        || lower.contains("paused")
        || lower.contains("retry")
        || lower.contains("partial")
        || lower.contains("disconnect")
    {
        LogSeverity::Warn
    } else if lower.contains("submitted")
        || lower.contains("filled")
        || lower.contains("replaced")
        || lower.contains("started")
        || lower.contains("converged")
    {
        LogSeverity::Success
    } else {
        LogSeverity::Info
    }
}

/// Fold a single log line into display rows that fit `max_cols` terminal columns.
pub fn fold_log_line(line: &str, max_cols: usize) -> Vec<String> {
    if max_cols == 0 {
        return vec![line.to_string()];
    }
    let mut rows = Vec::new();
    let mut rest = line.to_string();
    while !rest.is_empty() {
        if rest.chars().count() <= max_cols {
            rows.push(rest);
            break;
        }
        let chunk: String = rest.chars().take(max_cols).collect();
        let break_byte = chunk
            .char_indices()
            .rev()
            .find_map(|(index, ch)| (ch == ' ' && index > max_cols / 4).then_some(index));
        if let Some(index) = break_byte {
            rows.push(chunk[..index].to_string());
            rest = rest[index + 1..].trim_start().to_string();
        } else {
            rows.push(chunk);
            rest = rest.chars().skip(max_cols).collect();
        }
    }
    rows
}

/// Expand buffered log lines into display rows for a fixed-width panel.
pub fn expand_log_lines(lines: &[String], content_width: usize) -> Vec<String> {
    let width = content_width.max(1);
    lines
        .iter()
        .flat_map(|line| fold_log_line(line, width))
        .collect()
}

fn severity_style(severity: LogSeverity) -> Style {
    match severity {
        LogSeverity::Info => Style::default().fg(Color::Gray),
        LogSeverity::Success => Style::default().fg(Color::Green),
        LogSeverity::Warn => Style::default().fg(Color::Yellow),
        LogSeverity::Error => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
    }
}

/// Incrementally tails an engine log file with a fixed-size ring buffer.
pub struct LogTailer {
    path: PathBuf,
    file_offset: u64,
    lines: VecDeque<String>,
    /// Bytes after the last `\n` in the file; held until the line completes.
    pending: String,
    last_poll: Instant,
    file_exists: bool,
}

impl LogTailer {
    pub fn new(path: PathBuf) -> Self {
        let mut tailer = Self {
            path,
            file_offset: 0,
            lines: VecDeque::with_capacity(LOG_BUFFER_LINES),
            pending: String::new(),
            last_poll: Instant::now() - LOG_POLL_INTERVAL,
            file_exists: false,
        };
        tailer.bootstrap();
        tailer
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn lines(&self) -> &VecDeque<String> {
        &self.lines
    }

    pub fn file_exists(&self) -> bool {
        self.file_exists
    }

    /// Polls the log file when the poll interval elapses. Returns true when new lines were read.
    pub fn poll(&mut self) -> bool {
        if self.last_poll.elapsed() < LOG_POLL_INTERVAL {
            return false;
        }
        self.last_poll = Instant::now();
        self.read_incremental()
    }

    fn bootstrap(&mut self) {
        self.file_exists = self.path.exists();
        if !self.file_exists {
            return;
        }
        match std::fs::read_to_string(&self.path) {
            Ok(content) => {
                let ends_with_newline = content.is_empty() || content.ends_with('\n');
                if !content.is_empty() {
                    self.ingest_appended(&content, ends_with_newline);
                }
                self.file_offset = std::fs::metadata(&self.path)
                    .map(|meta| meta.len())
                    .unwrap_or(0);
            }
            Err(_) => {
                self.file_exists = false;
            }
        }
    }

    fn read_incremental(&mut self) -> bool {
        self.file_exists = self.path.exists();
        if !self.file_exists {
            return false;
        }
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(_) => {
                self.file_exists = false;
                return false;
            }
        };
        if bytes.len() < self.file_offset as usize {
            self.lines.clear();
            self.pending.clear();
            self.file_offset = 0;
        }
        if bytes.len() <= self.file_offset as usize {
            return false;
        }
        let appended_bytes = &bytes[self.file_offset as usize..];
        let ends_with_newline = appended_bytes.is_empty() || appended_bytes.ends_with(b"\n");
        let appended = String::from_utf8_lossy(appended_bytes);
        self.file_offset = bytes.len() as u64;
        self.ingest_appended(&appended, ends_with_newline)
    }

    fn ingest_appended(&mut self, appended: &str, ends_with_newline: bool) -> bool {
        let mut combined = self.pending.clone();
        combined.push_str(appended);
        if !ends_with_newline {
            match combined.rfind('\n') {
                Some(index) => {
                    let (complete, rest) = combined.split_at(index);
                    let changed = self.push_complete_text(complete);
                    self.pending = rest.strip_prefix('\n').unwrap_or(rest).to_string();
                    changed
                }
                None => {
                    self.pending = combined;
                    false
                }
            }
        } else {
            self.pending.clear();
            self.push_complete_text(&combined)
        }
    }

    fn push_complete_text(&mut self, text: &str) -> bool {
        if text.is_empty() {
            return false;
        }
        let mut changed = false;
        for line in text.lines() {
            self.push_line(line.to_owned());
            changed = true;
        }
        self.trim_to_buffer_capacity();
        changed
    }

    fn push_line(&mut self, line: String) {
        if self.lines.len() >= LOG_BUFFER_LINES {
            self.lines.pop_front();
        }
        self.lines.push_back(line);
    }

    fn trim_to_buffer_capacity(&mut self) {
        while self.lines.len() > LOG_BUFFER_LINES {
            self.lines.pop_front();
        }
    }
}

#[derive(Default)]
pub struct LogPanelState {
    pub tailer: Option<LogTailer>,
    pub scroll: usize,
    pub max_scroll: usize,
    pub follow_latest: bool,
}

impl LogPanelState {
    pub fn with_path(path: PathBuf) -> Self {
        Self {
            tailer: Some(LogTailer::new(path)),
            scroll: 0,
            max_scroll: 0,
            follow_latest: true,
        }
    }

    /// Point the panel at the engine-reported log path. Clears the tailer when the engine has
    /// no log file (for example TUI/preview without `--log-file`).
    pub fn sync_engine_log_path(&mut self, path: Option<&str>) {
        let path = path.map(str::trim).filter(|value| !value.is_empty());
        match path {
            None => {
                self.tailer = None;
                self.scroll = 0;
                self.max_scroll = 0;
            }
            Some(path) => {
                let path_buf = PathBuf::from(path);
                let same = self
                    .tailer
                    .as_ref()
                    .is_some_and(|tailer| tailer.path() == path_buf.as_path());
                if !same {
                    self.tailer = Some(LogTailer::new(path_buf));
                    self.scroll = 0;
                    self.follow_latest = true;
                }
            }
        }
    }

    pub fn has_engine_log(&self) -> bool {
        self.tailer.is_some()
    }

    pub fn update_scroll_bounds(&mut self, viewport_height: usize, content_width: usize) {
        let line_count = self
            .tailer
            .as_ref()
            .map(|tailer| {
                let source: Vec<String> = tailer.lines().iter().cloned().collect();
                expand_log_lines(&source, content_width).len()
            })
            .unwrap_or(0);
        self.max_scroll = line_count.saturating_sub(viewport_height.max(1));
        self.scroll = if self.follow_latest {
            self.max_scroll
        } else {
            self.scroll.min(self.max_scroll)
        };
        self.follow_latest = self.scroll == self.max_scroll;
    }

    pub fn scroll_up(&mut self, amount: usize) {
        self.scroll = self.scroll.saturating_sub(amount);
        self.follow_latest = self.scroll == self.max_scroll;
    }

    pub fn scroll_down(&mut self, amount: usize) {
        self.scroll = self.scroll.saturating_add(amount).min(self.max_scroll);
        self.follow_latest = self.scroll == self.max_scroll;
    }

    pub fn toggle_follow(&mut self) {
        self.follow_latest = !self.follow_latest;
        if self.follow_latest {
            self.scroll = self.max_scroll;
        }
    }

    pub fn recent_lines(&self, limit: usize) -> Vec<&str> {
        self.tailer
            .as_ref()
            .map(|tailer| {
                let lines: Vec<&str> = tailer.lines().iter().map(String::as_str).collect();
                let start = lines.len().saturating_sub(limit);
                lines[start..].to_vec()
            })
            .unwrap_or_default()
    }

    pub fn recent_error_lines(&self, limit: usize) -> Vec<String> {
        self.tailer
            .as_ref()
            .map(|tailer| {
                tailer
                    .lines()
                    .iter()
                    .rev()
                    .filter(|line| {
                        matches!(
                            classify_log_line(line),
                            LogSeverity::Error | LogSeverity::Warn
                        )
                    })
                    .take(limit)
                    .cloned()
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Splits `area` horizontally when wide enough for a log sidebar.
pub fn split_layout(area: Rect, min_main: u16, min_log: u16) -> (Rect, Option<Rect>) {
    let divider = 1u16;
    let required = min_main.saturating_add(min_log).saturating_add(divider);
    if area.width < required {
        return (area, None);
    }
    let log_width = min_log.max(area.width.saturating_sub(min_main).saturating_sub(divider));
    let main_width = area.width.saturating_sub(log_width).saturating_sub(divider);
    let main = Rect {
        x: area.x,
        y: area.y,
        width: main_width,
        height: area.height,
    };
    let log = Rect {
        x: main.x.saturating_add(main.width).saturating_add(divider),
        y: area.y,
        width: log_width,
        height: area.height,
    };
    (main, Some(log))
}

pub fn render_log_panel(frame: &mut Frame, area: Rect, state: &LogPanelState) {
    let Some(tailer) = state.tailer.as_ref() else {
        return;
    };
    if !tailer.file_exists() {
        frame.render_widget(
            Paragraph::new(Line::styled(
                format!(
                    "Waiting for engine log:\n{}\n(file not created yet)",
                    tailer.path().display()
                ),
                Style::default().fg(Color::DarkGray),
            ))
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL).title("Logs")),
            area,
        );
        return;
    }

    let content_width = area.width.saturating_sub(2) as usize;
    let mut lines = Vec::new();
    for line in tailer.lines() {
        let severity = classify_log_line(line);
        let style = severity_style(severity);
        for display in fold_log_line(line, content_width.max(1)) {
            lines.push(Line::styled(display, style));
        }
    }
    if lines.is_empty() {
        lines.push(Line::styled(
            "(no log lines yet)",
            Style::default().fg(Color::DarkGray),
        ));
    }

    let follow = if state.follow_latest {
        "following"
    } else {
        "manual"
    };
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!("Logs ({follow})")),
            )
            .scroll((state.scroll.min(u16::MAX as usize) as u16, 0))
            .wrap(Wrap { trim: false }),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_log_line_detects_severity_keywords() {
        assert_eq!(classify_log_line("order submitted"), LogSeverity::Success);
        assert_eq!(classify_log_line("order filled"), LogSeverity::Success);
        assert_eq!(classify_log_line("engine started"), LogSeverity::Success);
        assert_eq!(classify_log_line("grid converged"), LogSeverity::Success);
        assert_eq!(classify_log_line("warning: paused"), LogSeverity::Warn);
        assert_eq!(classify_log_line("retry in 2s"), LogSeverity::Warn);
        assert_eq!(classify_log_line("partial fill"), LogSeverity::Warn);
        assert_eq!(
            classify_log_line("disconnect from socket"),
            LogSeverity::Warn
        );
        assert_eq!(classify_log_line("order failed"), LogSeverity::Error);
        assert_eq!(classify_log_line("RISK REJECTED"), LogSeverity::Error);
        assert_eq!(classify_log_line("blocked by policy"), LogSeverity::Error);
        assert_eq!(classify_log_line("cycle complete"), LogSeverity::Info);
    }

    #[test]
    fn split_layout_respects_width_threshold() {
        let area = Rect::new(0, 0, 112, 20);
        let (main, log) = split_layout(area, MIN_MAIN_WIDTH, MIN_LOG_WIDTH);
        assert_eq!(main, area);
        assert!(log.is_none());

        let area = Rect::new(0, 0, 113, 20);
        let (main, log) = split_layout(area, MIN_MAIN_WIDTH, MIN_LOG_WIDTH);
        assert!(log.is_some());
        assert_eq!(main.width + log.unwrap().width + 1, area.width);
    }

    #[test]
    fn fold_log_line_preserves_full_error_text_across_rows() {
        let line = "grid refresh failed: Decibel account_overviews returned 404 Not Found: account missing";
        let rows = fold_log_line(line, 38);
        assert!(rows.len() > 1);
        assert!(rows.iter().all(|row| row.chars().count() <= 38));
        assert!(line.contains(rows.first().unwrap()));
        assert!(rows.last().unwrap().contains("account missing"));
    }

    #[test]
    fn log_tailer_keeps_incomplete_line_until_newline() {
        let dir =
            std::env::temp_dir().join(format!("grid-bot-monitor-partial-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("partial.log");
        std::fs::write(&path, "grid refresh failed: 404 Not Fo").unwrap();
        let mut tailer = LogTailer::new(path.clone());
        assert_eq!(tailer.lines().len(), 0);

        std::fs::write(&path, "grid refresh failed: 404 Not Found: body\n").unwrap();
        tailer.last_poll = Instant::now() - LOG_POLL_INTERVAL;
        assert!(tailer.poll());
        assert_eq!(tailer.lines().len(), 1);
        assert_eq!(
            tailer.lines().back().map(String::as_str),
            Some("grid refresh failed: 404 Not Found: body")
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn log_tailer_keeps_ring_buffer_size() {
        let dir =
            std::env::temp_dir().join(format!("grid-bot-monitor-log-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.log");
        let mut content = String::new();
        for index in 0..600 {
            content.push_str(&format!("line-{index}\n"));
        }
        std::fs::write(&path, content).unwrap();
        let tailer = LogTailer::new(path);
        assert_eq!(tailer.lines().len(), LOG_BUFFER_LINES);
        assert_eq!(tailer.lines().front().map(String::as_str), Some("line-100"));
        assert_eq!(tailer.lines().back().map(String::as_str), Some("line-599"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
