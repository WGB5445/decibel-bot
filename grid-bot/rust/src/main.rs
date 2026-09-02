use std::{
    backtrace::Backtrace,
    fs,
    io::{self, Write},
    panic::{self, PanicHookInfo},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    str::FromStr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use tokio::sync::mpsc;

use anyhow::{Context, Result};
use chrono::Utc;
use clap::{Args as ClapArgs, Parser, Subcommand};
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
        MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use decibel_grid_tui::i18n::{self, Key as TKey, Language};
use decibel_grid_tui::profile::{self, DEFAULT_PROFILE, ProfileData, ProfileStore};
use decibel_grid_tui::*;
use dotenvy::dotenv;
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, Wrap},
};
use rust_decimal::{Decimal, prelude::ToPrimitive};

/// Cross USDC balance below this threshold is treated as zero for UI warnings and display.
const USDC_CROSS_DUST: Decimal = Decimal::from_parts(1, 0, 0, false, 6);

/// Every resting order occupies one grid cell. Render them in price order rather than pairing
/// the bid and ask arrays by index, because they live on opposite sides of the current mid.
fn grid_price_count(plan: &GridPlan) -> usize {
    plan.bids.len() + plan.asks.len()
}

#[derive(Clone, Copy)]
struct GridGeometry {
    cells: Rect,
    columns: usize,
    rows: usize,
    cell_width: u16,
    cell_height: u16,
    /// First ordered price represented by the visible top-left cell.
    first_index: usize,
}

impl GridGeometry {
    fn cell_rect(self, index: usize) -> Option<Rect> {
        if index < self.first_index || index >= self.first_index + self.columns * self.rows {
            return None;
        }
        let relative = index - self.first_index;
        let col = relative % self.columns;
        let row = relative / self.columns;
        let x = self.cells.x + col as u16 * self.cell_width;
        let y = self.cells.y + row as u16 * self.cell_height;
        if x >= self.cells.right() || y >= self.cells.bottom() {
            return None;
        }
        Some(Rect::new(
            x,
            y,
            self.cell_width.min(self.cells.right().saturating_sub(x)),
            self.cell_height.min(self.cells.bottom().saturating_sub(y)),
        ))
    }

    fn hit_test(self, column: u16, row: u16, count: usize) -> Option<usize> {
        (self.first_index..count).find(|&index| {
            self.cell_rect(index).is_some_and(|cell| {
                column >= cell.x && column < cell.right() && row >= cell.y && row < cell.bottom()
            })
        })
    }
}

/// Uses the exact same layout calculations as `render_grid`, so mouse hit testing and drawing
/// cannot drift apart when the terminal size changes.
fn price_grid_geometry(grid_area: Rect, price_count: usize, scroll: usize) -> GridGeometry {
    let board = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(8),
            Constraint::Min(6),
            Constraint::Length(8),
        ])
        .split(grid_area)[1];
    let cells = board.inner(Margin {
        vertical: 1,
        horizontal: 1,
    });
    let columns = price_count.clamp(1, 8);
    // A bordered tile needs interior lines for the side, price, and notional/P&L. Keep its
    // height fixed and page through rows rather than silently clipping lower prices.
    let cell_height = 5;
    let rows = usize::from((cells.height / cell_height).max(1));
    let first_index = (scroll / columns) * columns;
    GridGeometry {
        cells,
        columns,
        rows,
        cell_width: (cells.width / columns as u16).max(1),
        cell_height,
        first_index,
    }
}

fn keep_selected_price_visible(app: &mut App) {
    let Some(snapshot) = app.snapshot.as_ref() else {
        return;
    };
    let count = grid_price_count(&snapshot.plan);
    if count == 0 {
        app.selected_level = 0;
        app.grid_scroll = 0;
        return;
    }
    app.selected_level = app.selected_level.min(count - 1);
    let terminal = crossterm::terminal::size().unwrap_or((80, 24));
    let screen = Rect::new(0, 0, terminal.0, terminal.1);
    let content = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(3),
        ])
        .split(screen)[1];
    let geometry = price_grid_geometry(content, count, app.grid_scroll);
    let capacity = geometry.columns * geometry.rows;
    if app.selected_level < geometry.first_index {
        app.grid_scroll = (app.selected_level / geometry.columns) * geometry.columns;
    } else if app.selected_level >= geometry.first_index + capacity {
        let selected_row = app.selected_level / geometry.columns;
        app.grid_scroll =
            selected_row.saturating_add(1).saturating_sub(geometry.rows) * geometry.columns;
    }
    let max_scroll = count.saturating_sub(capacity);
    app.grid_scroll = app
        .grid_scroll
        .min(max_scroll / geometry.columns * geometry.columns);
}

const TAB_CONFIG: usize = 0;
const TAB_PREVIEW: usize = 1;
const TAB_MONITOR: usize = 2;
const TAB_COUNT: usize = 3;
/// Backoff used when the configuration is not yet valid (for example a missing API key),
/// so a failing setup does not retry on every render frame.
const RETRY_INTERVAL: Duration = Duration::from_secs(2);

fn ui(language: Language, english: &'static str, chinese: &'static str) -> &'static str {
    if language == Language::Chinese {
        chinese
    } else {
        english
    }
}

fn tab_titles(language: Language) -> [String; TAB_COUNT] {
    [
        i18n::t(language, TKey::TabConfigure).to_owned(),
        i18n::t(language, TKey::TabPreview).to_owned(),
        i18n::t(language, TKey::TabMonitor).to_owned(),
    ]
}

/// The exact rectangles of the tab buttons. Rendering and mouse hit-testing both use these
/// rectangles, rather than relying on undocumented padding in `ratatui::Tabs`.
fn tab_rects(area: Rect, language: Language) -> [Rect; TAB_COUNT] {
    let titles = tab_titles(language);
    let mut x = area.x.saturating_add(1);
    std::array::from_fn(|index| {
        let desired_width = ratatui::text::Line::from(titles[index].as_str()).width() as u16;
        let remaining = area.right().saturating_sub(1).saturating_sub(x);
        let width = desired_width.min(remaining);
        let rect = Rect::new(x, area.y.saturating_add(1), width, 1);
        x = x.saturating_add(width).saturating_add(1); // one column between buttons
        rect
    })
}

fn tab_at_position(area: Rect, language: Language, column: u16, row: u16) -> Option<usize> {
    tab_rects(area, language).iter().position(|rect| {
        column >= rect.x && column < rect.right() && row >= rect.y && row < rect.bottom()
    })
}

fn render_tabs(area: Rect, frame: &mut ratatui::Frame, app: &App) {
    let tabs = tab_rects(area, app.settings.language);
    frame.render_widget(
        Block::default().borders(Borders::ALL).title(format!(
            "{} — {}",
            app.settings.tr(TKey::AppTitle),
            ui(
                app.settings.language,
                "execute only from Preview",
                "仅可从预览页执行",
            )
        )),
        area,
    );
    let titles = tab_titles(app.settings.language);
    for (index, rect) in tabs.into_iter().enumerate() {
        let style = if index == app.tab {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        frame.render_widget(
            Paragraph::new(titles[index].clone())
                .style(style)
                .alignment(ratatui::layout::Alignment::Center),
            rect,
        );
    }
}

#[derive(Parser)]
#[command(
    name = "decibel-grid-tui",
    about = "Decibel grid planner and interactive monitor"
)]
struct Cli {
    /// `preview` opens the Preview tab; `run` writes continuous text snapshots; `tui` opens Configure.
    #[command(subcommand)]
    command: Option<Cmd>,
    #[command(flatten)]
    args: Args,
}

#[derive(Subcommand, Clone, Copy, Eq, PartialEq)]
enum Cmd {
    /// Launch a local engine child and return once its control socket is ready.
    Start,
    /// Internal foreground engine command for systemd/tmux. Do not use directly for ad-hoc trading.
    #[command(hide = true)]
    Engine,
    /// Stream the engine's local log file; pass --follow to wait for appended lines.
    Logs,
    /// Repeatedly render the engine's current socket status.
    Attach,
    /// Validate the key locally, then verify it against the selected network.
    CheckKey,
    Preview,
    /// Read the exchange and report desired-vs-actual grid drift. Never changes orders.
    Reconcile,
    /// Print a one-time account and grid snapshot. Never changes orders.
    Status,
    /// Verify API access, market rules, a generated plan, balances, and order drift. Never changes orders.
    Doctor,
    /// Monitor a grid; with -e it submits a fresh bulk ladder after every successful refresh.
    Run,
    /// Stop one grid now: cancel its ladder, then retain assets or liquidate according to the
    /// runtime --exit-asset-policy argument.
    Stop,
    /// Continuous shadow mode: reconcile every cycle, journal events, but never sign or submit.
    /// With --cycles N, exit after N successful reconciliation cycles (exit 0).
    Shadow,
    /// Move Cross USDC into PFS; set future settlement routing manually as subaccount owner first.
    SpotFundingSetup,
    Tui,
}

#[derive(ClapArgs, Clone)]
struct Args {
    #[arg(long, global = true, env = "NETWORK", default_value = "testnet")]
    network: String,
    #[arg(long, global = true, env = "DECIBEL_API_KEY", hide_env_values = true)]
    decibel_api_key: Option<String>,
    /// Aptos Ed25519 private key. Prefer entering it in the TUI so Ctrl+S encrypts it in the profile.
    #[arg(long, global = true, env = "APTOS_PRIVATE_KEY", hide_env_values = true)]
    aptos_private_key: Option<String>,
    /// Execute the configured grid. Only meaningful with the `run` command.
    #[arg(short = 'e', long = "execute", global = true, default_value_t = false)]
    execute: bool,
    /// Exit Shadow after this many successful reconciliation cycles; Shadow only.
    #[arg(long, global = true, env = "SHADOW_CYCLES")]
    shadow_cycles: Option<usize>,
    /// Required exact acknowledgement for any Mainnet execution: MAINNET.
    #[arg(long, global = true, env = "CONFIRM_MAINNET")]
    confirm_mainnet: Option<String>,
    #[arg(long, global = true, env = "GRID_PROFILE", default_value = "default")]
    profile: String,
    #[arg(
        long,
        global = true,
        env = "PRODUCT",
        value_enum,
        default_value = "perp"
    )]
    product: Product,
    #[arg(long, global = true, env = "MARKET", default_value = "BTC/USD")]
    market: String,
    #[arg(
        long,
        global = true,
        env = "SUBACCOUNT_ADDRESS",
        hide_env_values = true
    )]
    subaccount: Option<String>,
    /// Write stdout and stderr to this file, replacing it at startup.
    #[arg(long, global = true, env = "LOG_FILE")]
    log_file: Option<PathBuf>,
    /// Continue streaming new lines for the `logs` client.
    #[arg(short = 'f', long, global = true, default_value_t = false)]
    follow: bool,
    /// Exit mode for the `stop` client: hold or liquidate.
    #[arg(long, global = true, value_parser = ["hold", "liquidate"])]
    exit_mode: Option<String>,
    /// Human-readable USDC amount to move from Cross to PFS for `spot-funding-setup`.
    #[arg(long, global = true, env = "SPOT_FUNDING_AMOUNT", default_value = "0")]
    spot_funding_amount: String,
    /// USDC metadata object; defaults to the testnet USDC metadata.
    #[arg(long, global = true, env = "SPOT_FUNDING_METADATA")]
    spot_funding_metadata: Option<String>,
    #[arg(
        long,
        global = true,
        env = "PERP_GRID_MODE",
        value_enum,
        default_value = "neutral"
    )]
    perp_mode: PerpMode,
    #[arg(long, global = true, env = "GRID_TOTAL_COUNT", default_value_t = 40)]
    grid_count: usize,
    #[arg(long, global = true, env = "GRID_TOTAL_BUDGET")]
    total_budget: Option<String>,
    /// Spot-only quote inventory budget for all bid levels. Overrides the bid share of
    /// GRID_TOTAL_BUDGET when supplied.
    #[arg(long, global = true, env = "TOTAL_QUOTE_BUDGET")]
    total_quote_budget: Option<String>,
    /// Spot-only base inventory budget for all ask levels. It may be zero only when automatic
    /// entry conversion is enabled and sufficient additional PFS quote is available.
    #[arg(long, global = true, env = "TOTAL_BASE_BUDGET")]
    total_base_budget: Option<String>,
    #[arg(long, global = true, env = "GRID_ORDER_SIZE")]
    order_size: Option<String>,
    #[arg(long, global = true, env = "GRID_RANGE_PERCENT")]
    range_percent: Option<String>,
    #[arg(long, global = true, env = "GRID_STEP_PERCENT")]
    grid_step_percent: Option<String>,
    #[arg(long, global = true, env = "GRID_LOWER_PRICE")]
    lower_price: Option<String>,
    #[arg(long, global = true, env = "GRID_UPPER_PRICE")]
    upper_price: Option<String>,
    /// Optional Spot liquidation trigger after price breaks below the lower bound.
    #[arg(long, global = true, env = "SPOT_EXIT_PRICE")]
    spot_exit_price: Option<String>,
    #[arg(long, global = true, env = "GRID_MAKER_FEE_RATE", default_value = "0")]
    maker_fee_rate: String,
    #[arg(long, global = true, env = "PREVIEW_LEVERAGE", default_value = "1")]
    preview_leverage: String,
    #[arg(long, global = true, env = "GRID_REFRESH_SECONDS", default_value_t = 3)]
    refresh_seconds: u64,
    #[arg(
        long,
        global = true,
        env = "PRICE_SOURCE",
        value_enum,
        default_value = "prices"
    )]
    price_source: PriceSource,
    /// How to handle assets when the bot exits (retain or sell).
    #[arg(
        long,
        global = true,
        env = "EXIT_ASSET_POLICY",
        value_enum,
        default_value = "retain"
    )]
    exit_asset_policy: ExitAssetPolicy,
    #[arg(long, global = true, env = "MIN_NET_MARGIN_BPS", default_value = "15")]
    min_net_margin_bps: String,
    #[arg(
        long,
        global = true,
        env = "RECONCILIATION_INTERVAL_MS",
        default_value_t = 30_000
    )]
    reconciliation_interval_ms: u64,
    #[arg(
        long,
        global = true,
        env = "WS_RECONNECT_BACKOFF_MS",
        default_value = "1000,2000,5000,10000,30000"
    )]
    ws_reconnect_backoff_ms: String,
    #[arg(
        long,
        global = true,
        env = "RANGE_BREAKOUT_ACTION",
        value_enum,
        default_value = "pause-and-alert"
    )]
    range_breakout_action: RangeBreakoutAction,
    #[arg(
        long,
        global = true,
        env = "AUTO_CONVERT_MISSING_BASE",
        default_value_t = true
    )]
    auto_convert_missing_base: bool,
    #[arg(
        long,
        global = true,
        env = "ENTRY_MAX_SLIPPAGE_BPS",
        default_value = "50"
    )]
    entry_max_slippage_bps: String,
    #[arg(
        long,
        global = true,
        env = "EXIT_MAX_SLIPPAGE_BPS",
        default_value = "50"
    )]
    exit_max_slippage_bps: String,
    #[arg(
        long,
        global = true,
        env = "ENTRY_EXIT_MAX_ATTEMPTS",
        default_value_t = 5
    )]
    entry_exit_max_attempts: usize,
    #[arg(
        long,
        global = true,
        env = "ENTRY_EXIT_RETRY_BACKOFF_MS",
        default_value = "500,1000,2000,5000,10000"
    )]
    entry_exit_retry_backoff_ms: String,
    #[arg(
        long,
        global = true,
        env = "ENTRY_EXIT_TIMEOUT_MS",
        default_value_t = 60_000
    )]
    entry_exit_timeout_ms: u64,
    #[arg(
        long,
        global = true,
        env = "ENTRY_MIN_FILL_RATIO",
        default_value = "0.8"
    )]
    entry_min_fill_ratio: String,
    #[arg(long, global = true, env = "PRICE_BUFFER_BPS", default_value = "5")]
    price_buffer_bps: String,
    #[arg(
        long,
        global = true,
        env = "MAX_CONSECUTIVE_BULK_FAILURES",
        default_value_t = 5
    )]
    max_consecutive_bulk_failures: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RangeKind {
    Percent,
    Step,
    Bounds,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AllocationKind {
    Budget,
    FixedSize,
}

#[derive(Clone)]
struct Settings {
    api_key: String,
    /// Aptos Ed25519 key used only after explicit Preview execution confirmation.
    aptos_private_key: String,
    language: Language,
    network: String,
    product: Product,
    market: String,
    subaccount: String,
    perp_mode: PerpMode,
    range_kind: RangeKind,
    range_value: String,
    upper_bound: String,
    grid_count: String,
    allocation_kind: AllocationKind,
    allocation_value: String,
    total_quote_budget: Option<String>,
    total_base_budget: Option<String>,
    maker_fee_rate: String,
    preview_leverage: String,
    refresh_seconds: String,
    price_source: PriceSource,
    /// Optional USDC metadata override for Spot funding instructions on mainnet.
    spot_funding_metadata: Option<String>,
    /// How to handle Spot base + Perp position on exit.
    exit_asset_policy: ExitAssetPolicy,
    /// Optional Spot stop-loss price. When market price reaches this level, the ladder is
    /// cancelled and all available base is sold through the existing exit liquidation path.
    spot_exit_price: Option<String>,
    min_net_margin_bps: String,
    reconciliation_interval_ms: u64,
    ws_reconnect_backoff_ms: String,
    range_breakout_action: RangeBreakoutAction,
    auto_convert_missing_base: bool,
    entry_max_slippage_bps: String,
    exit_max_slippage_bps: String,
    entry_exit_max_attempts: usize,
    entry_exit_retry_backoff_ms: String,
    entry_exit_timeout_ms: u64,
    entry_min_fill_ratio: String,
    price_buffer_bps: String,
    max_consecutive_bulk_failures: usize,
}

impl From<&Args> for Settings {
    fn from(args: &Args) -> Self {
        let (range_kind, range_value, upper_bound) = if let Some(value) = &args.grid_step_percent {
            (RangeKind::Step, value.clone(), String::new())
        } else if let (Some(lower), Some(upper)) = (&args.lower_price, &args.upper_price) {
            (RangeKind::Bounds, lower.clone(), upper.clone())
        } else {
            (
                RangeKind::Percent,
                args.range_percent
                    .clone()
                    .unwrap_or_else(|| "10".to_owned()),
                String::new(),
            )
        };
        let (allocation_kind, allocation_value) = match (&args.total_budget, &args.order_size) {
            (Some(value), _) => (AllocationKind::Budget, value.clone()),
            (_, Some(value)) => (AllocationKind::FixedSize, value.clone()),
            _ => (AllocationKind::Budget, "1000".to_owned()),
        };
        Self {
            api_key: args.decibel_api_key.clone().unwrap_or_default(),
            aptos_private_key: args.aptos_private_key.clone().unwrap_or_default(),
            language: Language::default(),
            network: args.network.clone(),
            product: args.product,
            market: args.market.clone(),
            subaccount: args.subaccount.clone().unwrap_or_default(),
            perp_mode: args.perp_mode,
            range_kind,
            range_value,
            upper_bound,
            grid_count: args.grid_count.to_string(),
            allocation_kind,
            allocation_value,
            total_quote_budget: args.total_quote_budget.clone(),
            total_base_budget: args.total_base_budget.clone(),
            maker_fee_rate: args.maker_fee_rate.clone(),
            preview_leverage: args.preview_leverage.clone(),
            refresh_seconds: args.refresh_seconds.to_string(),
            price_source: args.price_source,
            spot_funding_metadata: args.spot_funding_metadata.clone(),
            exit_asset_policy: args.exit_asset_policy,
            spot_exit_price: args.spot_exit_price.clone(),
            min_net_margin_bps: args.min_net_margin_bps.clone(),
            reconciliation_interval_ms: args.reconciliation_interval_ms,
            ws_reconnect_backoff_ms: args.ws_reconnect_backoff_ms.clone(),
            range_breakout_action: args.range_breakout_action,
            auto_convert_missing_base: args.auto_convert_missing_base,
            entry_max_slippage_bps: args.entry_max_slippage_bps.clone(),
            exit_max_slippage_bps: args.exit_max_slippage_bps.clone(),
            entry_exit_max_attempts: args.entry_exit_max_attempts,
            entry_exit_retry_backoff_ms: args.entry_exit_retry_backoff_ms.clone(),
            entry_exit_timeout_ms: args.entry_exit_timeout_ms,
            entry_min_fill_ratio: args.entry_min_fill_ratio.clone(),
            price_buffer_bps: args.price_buffer_bps.clone(),
            max_consecutive_bulk_failures: args.max_consecutive_bulk_failures,
        }
    }
}

impl Settings {
    /// Built-in defaults used when a profile is reset. Deliberately conservative: testnet,
    /// a small grid, and a read-only-friendly price source.
    fn defaults() -> Self {
        Self {
            api_key: String::new(),
            aptos_private_key: String::new(),
            language: Language::default(),
            network: "testnet".to_owned(),
            product: Product::Perp,
            market: "BTC/USD".to_owned(),
            subaccount: String::new(),
            perp_mode: PerpMode::Neutral,
            range_kind: RangeKind::Percent,
            range_value: "10".to_owned(),
            upper_bound: String::new(),
            grid_count: "40".to_owned(),
            allocation_kind: AllocationKind::Budget,
            allocation_value: "1000".to_owned(),
            total_quote_budget: None,
            total_base_budget: None,
            maker_fee_rate: "0".to_owned(),
            preview_leverage: "1".to_owned(),
            refresh_seconds: "3".to_owned(),
            price_source: PriceSource::Prices,
            spot_funding_metadata: None,
            exit_asset_policy: ExitAssetPolicy::Retain,
            spot_exit_price: None,
            min_net_margin_bps: "15".to_owned(),
            reconciliation_interval_ms: 30_000,
            ws_reconnect_backoff_ms: "1000,2000,5000,10000,30000".to_owned(),
            range_breakout_action: RangeBreakoutAction::PauseAndAlert,
            auto_convert_missing_base: true,
            entry_max_slippage_bps: "50".to_owned(),
            exit_max_slippage_bps: "50".to_owned(),
            entry_exit_max_attempts: 5,
            entry_exit_retry_backoff_ms: "500,1000,2000,5000,10000".to_owned(),
            entry_exit_timeout_ms: 60_000,
            entry_min_fill_ratio: "0.8".to_owned(),
            price_buffer_bps: "5".to_owned(),
            max_consecutive_bulk_failures: 5,
        }
    }

    fn tr(&self, key: TKey) -> &'static str {
        i18n::t(self.language, key)
    }

    /// Serializable snapshot of everything except the API key, which is encrypted separately.
    fn to_profile(&self) -> ProfileData {
        ProfileData {
            language: self.language,
            network: self.network.clone(),
            product: format!("{:?}", self.product).to_lowercase(),
            market: self.market.clone(),
            subaccount: self.subaccount.clone(),
            perp_mode: format!("{:?}", self.perp_mode).to_lowercase(),
            range_kind: match self.range_kind {
                RangeKind::Percent => "percent",
                RangeKind::Step => "step",
                RangeKind::Bounds => "bounds",
            }
            .to_owned(),
            range_value: self.range_value.clone(),
            upper_bound: self.upper_bound.clone(),
            grid_count: self.grid_count.clone(),
            allocation_kind: match self.allocation_kind {
                AllocationKind::Budget => "budget",
                AllocationKind::FixedSize => "size",
            }
            .to_owned(),
            allocation_value: self.allocation_value.clone(),
            total_quote_budget: self.total_quote_budget.clone(),
            total_base_budget: self.total_base_budget.clone(),
            maker_fee_rate: self.maker_fee_rate.clone(),
            preview_leverage: self.preview_leverage.clone(),
            refresh_seconds: self.refresh_seconds.clone(),
            price_source: format!("{:?}", self.price_source).to_lowercase(),
            exit_asset_policy: format!("{:?}", self.exit_asset_policy).to_lowercase(),
            min_net_margin_bps: self.min_net_margin_bps.clone(),
            reconciliation_interval_ms: self.reconciliation_interval_ms.to_string(),
            ws_reconnect_backoff_ms: self.ws_reconnect_backoff_ms.clone(),
            range_breakout_action: format!("{:?}", self.range_breakout_action).to_lowercase(),
            auto_convert_missing_base: self.auto_convert_missing_base.to_string(),
            entry_max_slippage_bps: self.entry_max_slippage_bps.clone(),
            exit_max_slippage_bps: self.exit_max_slippage_bps.clone(),
            entry_exit_max_attempts: self.entry_exit_max_attempts.to_string(),
            entry_exit_retry_backoff_ms: self.entry_exit_retry_backoff_ms.clone(),
            entry_exit_timeout_ms: self.entry_exit_timeout_ms.to_string(),
            entry_min_fill_ratio: self.entry_min_fill_ratio.clone(),
            price_buffer_bps: self.price_buffer_bps.clone(),
            max_consecutive_bulk_failures: self.max_consecutive_bulk_failures.to_string(),
            encrypted_api_key: None,
            encrypted_aptos_private_key: None,
        }
    }

    /// Applies stored values over the current settings. Empty fields are left untouched so a
    /// partially written profile cannot blank out a working configuration.
    fn apply_profile(&mut self, data: &ProfileData) {
        fn set(target: &mut String, value: &str) {
            if !value.is_empty() {
                *target = value.to_owned();
            }
        }
        self.language = data.language;
        set(&mut self.network, &data.network);
        set(&mut self.market, &data.market);
        self.subaccount = data.subaccount.clone();
        set(&mut self.range_value, &data.range_value);
        set(&mut self.upper_bound, &data.upper_bound);
        set(&mut self.grid_count, &data.grid_count);
        set(&mut self.allocation_value, &data.allocation_value);
        self.total_quote_budget = data.total_quote_budget.clone();
        self.total_base_budget = data.total_base_budget.clone();
        set(&mut self.maker_fee_rate, &data.maker_fee_rate);
        set(&mut self.preview_leverage, &data.preview_leverage);
        set(&mut self.refresh_seconds, &data.refresh_seconds);
        set(&mut self.min_net_margin_bps, &data.min_net_margin_bps);
        set(
            &mut self.ws_reconnect_backoff_ms,
            &data.ws_reconnect_backoff_ms,
        );
        set(
            &mut self.entry_max_slippage_bps,
            &data.entry_max_slippage_bps,
        );
        set(&mut self.exit_max_slippage_bps, &data.exit_max_slippage_bps);
        set(
            &mut self.entry_exit_retry_backoff_ms,
            &data.entry_exit_retry_backoff_ms,
        );
        set(&mut self.entry_min_fill_ratio, &data.entry_min_fill_ratio);
        set(&mut self.price_buffer_bps, &data.price_buffer_bps);
        if let Ok(value) = data.reconciliation_interval_ms.parse() {
            self.reconciliation_interval_ms = value;
        }
        if let Ok(value) = data.entry_exit_max_attempts.parse() {
            self.entry_exit_max_attempts = value;
        }
        if let Ok(value) = data.entry_exit_timeout_ms.parse() {
            self.entry_exit_timeout_ms = value;
        }
        if let Ok(value) = data.max_consecutive_bulk_failures.parse() {
            self.max_consecutive_bulk_failures = value;
        }
        if let Ok(value) = data.auto_convert_missing_base.parse() {
            self.auto_convert_missing_base = value;
        }
        if data.product == "spot" {
            self.product = Product::Spot;
        } else if data.product == "perp" {
            self.product = Product::Perp;
        }
        match data.perp_mode.as_str() {
            "long" => self.perp_mode = PerpMode::Long,
            "short" => self.perp_mode = PerpMode::Short,
            "neutral" => self.perp_mode = PerpMode::Neutral,
            _ => {}
        }
        match data.range_kind.as_str() {
            "percent" => self.range_kind = RangeKind::Percent,
            "step" => self.range_kind = RangeKind::Step,
            "bounds" => self.range_kind = RangeKind::Bounds,
            _ => {}
        }
        match data.allocation_kind.as_str() {
            "budget" => self.allocation_kind = AllocationKind::Budget,
            "size" => self.allocation_kind = AllocationKind::FixedSize,
            _ => {}
        }
        match data.price_source.as_str() {
            "prices" => self.price_source = PriceSource::Prices,
            "depth" => self.price_source = PriceSource::Depth,
            _ => {}
        }
        match data.exit_asset_policy.as_str() {
            "retain" => self.exit_asset_policy = ExitAssetPolicy::Retain,
            "sell" => self.exit_asset_policy = ExitAssetPolicy::Sell,
            _ => {}
        }
        match data.range_breakout_action.as_str() {
            "pause-and-alert" | "pause_and_alert" => {
                self.range_breakout_action = RangeBreakoutAction::PauseAndAlert
            }
            "extend-grid" | "extend_grid" => {
                self.range_breakout_action = RangeBreakoutAction::ExtendGrid
            }
            _ => {}
        }
        if self.product == Product::Spot {
            self.perp_mode = PerpMode::Neutral;
        }
    }

    fn to_grid_config(&self) -> Result<GridConfig> {
        let range = match self.range_kind {
            RangeKind::Percent => RangeSpec::Percent {
                percent: decimal(&self.range_value)?,
            },
            RangeKind::Step => RangeSpec::StepPercent {
                percent: decimal(&self.range_value)?,
            },
            RangeKind::Bounds => RangeSpec::Bounds {
                lower: decimal(&self.range_value)?,
                upper: decimal(&self.upper_bound)?,
            },
        };
        let allocation = match self.allocation_kind {
            AllocationKind::Budget => Allocation::TotalBudget(decimal(&self.allocation_value)?),
            AllocationKind::FixedSize => Allocation::FixedSize(decimal(&self.allocation_value)?),
        };
        Ok(GridConfig {
            product: self.product,
            perp_mode: self.perp_mode,
            market_name: self.market.clone(),
            range,
            total_count: self
                .grid_count
                .parse()
                .context("Grid count must be an integer")?,
            allocation,
            maker_fee_rate: decimal(&self.maker_fee_rate)?,
            preview_leverage: decimal(&self.preview_leverage)?,
            refresh: Duration::from_secs(
                self.refresh_seconds
                    .parse()
                    .context("Refresh seconds must be an integer")?,
            ),
            price_source: self.price_source,
            spot: SpotExecutionConfig {
                total_quote_budget: self
                    .total_quote_budget
                    .as_deref()
                    .map(decimal)
                    .transpose()?,
                total_base_budget: self.total_base_budget.as_deref().map(decimal).transpose()?,
                min_net_margin_bps: decimal(&self.min_net_margin_bps)?,
                reconciliation_interval: Duration::from_millis(self.reconciliation_interval_ms),
                ws_reconnect_backoff: parse_duration_list_ms(&self.ws_reconnect_backoff_ms)?,
                range_breakout_action: self.range_breakout_action,
                auto_convert_missing_base: self.auto_convert_missing_base,
                entry_max_slippage_bps: decimal(&self.entry_max_slippage_bps)?,
                exit_max_slippage_bps: decimal(&self.exit_max_slippage_bps)?,
                entry_exit_max_attempts: self.entry_exit_max_attempts,
                entry_exit_retry_backoff: parse_duration_list_ms(
                    &self.entry_exit_retry_backoff_ms,
                )?,
                entry_exit_timeout: Duration::from_millis(self.entry_exit_timeout_ms),
                entry_min_fill_ratio: decimal(&self.entry_min_fill_ratio)?,
                price_buffer_bps: decimal(&self.price_buffer_bps)?,
                max_consecutive_bulk_failures: self.max_consecutive_bulk_failures,
            },
        })
    }
    fn api_client(&self) -> Result<DecibelClient> {
        if self.api_key.trim().is_empty() {
            anyhow::bail!("API key is required. Select API Key and press Enter to set it.")
        }
        DecibelClient::new(&self.network, &self.api_key)
    }
    fn masked_key(&self) -> String {
        masked_secret(&self.api_key, true)
    }

    fn masked_private_key(&self) -> String {
        masked_secret(&self.aptos_private_key, false)
    }
}

fn masked_secret(secret: &str, show_suffix: bool) -> String {
    if secret.is_empty() {
        return "not configured".to_owned();
    }
    if !show_suffix {
        return "••••••••".to_owned();
    }
    let chars: Vec<char> = secret.chars().collect();
    let suffix: String = chars.iter().skip(chars.len().saturating_sub(4)).collect();
    format!("••••••••{suffix}")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Field {
    ApiKey,
    AptosPrivateKey,
    Language,
    Network,
    Product,
    Market,
    Subaccount,
    PerpMode,
    RangeKind,
    RangeValue,
    UpperBound,
    GridCount,
    AllocationKind,
    AllocationValue,
    MakerFee,
    PreviewLeverage,
    RefreshSeconds,
    PriceSource,
    ExitAssetPolicy,
}
const FIELDS: [Field; 19] = [
    Field::ApiKey,
    Field::AptosPrivateKey,
    Field::Language,
    Field::Network,
    Field::Product,
    Field::Market,
    Field::Subaccount,
    Field::PerpMode,
    Field::RangeKind,
    Field::RangeValue,
    Field::UpperBound,
    Field::GridCount,
    Field::AllocationKind,
    Field::AllocationValue,
    Field::MakerFee,
    Field::PreviewLeverage,
    Field::RefreshSeconds,
    Field::PriceSource,
    Field::ExitAssetPolicy,
];

impl Field {
    fn label(self, language: Language) -> &'static str {
        i18n::t(
            language,
            match self {
                Self::ApiKey => TKey::FieldApiKey,
                Self::AptosPrivateKey => TKey::FieldAptosPrivateKey,
                Self::Language => TKey::FieldLanguage,
                Self::Network => TKey::FieldNetwork,
                Self::Product => TKey::FieldProduct,
                Self::Market => TKey::FieldMarket,
                Self::Subaccount => TKey::FieldSubaccount,
                Self::PerpMode => TKey::FieldPerpMode,
                Self::RangeKind => TKey::FieldRangeKind,
                Self::RangeValue => TKey::FieldRangeValue,
                Self::UpperBound => TKey::FieldUpperBound,
                Self::GridCount => TKey::FieldGridCount,
                Self::AllocationKind => TKey::FieldAllocationKind,
                Self::AllocationValue => TKey::FieldAllocationValue,
                Self::MakerFee => TKey::FieldMakerFee,
                Self::PreviewLeverage => TKey::FieldPreviewLeverage,
                Self::RefreshSeconds => TKey::FieldRefreshSeconds,
                Self::PriceSource => TKey::FieldPriceSource,
                Self::ExitAssetPolicy => TKey::FieldExitAssetPolicy,
            },
        )
    }
    fn editable(self) -> bool {
        matches!(
            self,
            Self::ApiKey
                | Self::AptosPrivateKey
                | Self::Market
                | Self::Subaccount
                | Self::RangeValue
                | Self::UpperBound
                | Self::GridCount
                | Self::AllocationValue
                | Self::MakerFee
                | Self::PreviewLeverage
                | Self::RefreshSeconds
        )
    }
    fn visible(self, settings: &Settings) -> bool {
        match self {
            // Direction is meaningful only for perpetual grids. Spot grids are always
            // two-sided inventory grids, so hiding this avoids a misleading setting.
            Self::PerpMode => settings.product == Product::Perp,
            Self::PreviewLeverage => settings.product == Product::Perp,
            Self::UpperBound => settings.range_kind == RangeKind::Bounds,
            _ => true,
        }
    }
}

/// What a password prompt is being collected for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PasswordPurpose {
    SaveProfile,
    LoadProfile,
}

enum MarketFetch {
    Markets(Result<Vec<Market>>),
    Detail {
        address: String,
        mid: Result<Decimal>,
        book: Result<OrderBook>,
    },
    Snapshot {
        settings_revision: u64,
        result: Box<Result<MonitorSnapshot>>,
    },
    Execution {
        settings_revision: u64,
        result: Box<Result<ExecutionResult>>,
    },
}

struct MarketPicker {
    query: String,
    selected: usize,
    detail_for: Option<String>,
    last_detail_at: Option<tokio::time::Instant>,
    mid: Option<Decimal>,
    book: Option<OrderBook>,
    detail_error: Option<String>,
    markets_pending: bool,
    detail_pending: bool,
}

impl MarketPicker {
    fn new() -> Self {
        Self {
            query: String::new(),
            selected: 0,
            detail_for: None,
            last_detail_at: None,
            mid: None,
            book: None,
            detail_error: None,
            markets_pending: false,
            detail_pending: false,
        }
    }
}

struct App {
    tab: usize,
    field_index: usize,
    /// Index of the selected price in the low-to-high grid.
    selected_level: usize,
    /// First price index of the visible grid page; always aligned to a row boundary.
    grid_scroll: usize,
    markets: Vec<Market>,
    markets_loaded_for: Option<(String, Product)>,
    market_picker: Option<MarketPicker>,
    editing: Option<Field>,
    edit_before: String,
    password: String,
    password_purpose: Option<PasswordPurpose>,
    /// Spot funding info modal content. When Some, the modal is open.
    funding_dialog: Option<String>,
    /// Text selection in the funding info modal: ((start_line, start_col), (end_line, end_col))
    /// in display coordinates relative to the modal content area.
    funding_selection: Option<((u16, u16), (u16, u16))>,
    /// Start cell of an in-progress mouse drag selection.
    funding_drag_start: Option<(u16, u16)>,
    /// Vertical scroll offset inside the funding info modal (wrapped lines).
    funding_scroll: u16,
    profile_name: String,
    settings: Settings,
    settings_revision: u64,
    refresh_now: bool,
    snapshot_pending: bool,
    execution_pending: bool,
    execute_requested: bool,
    /// Show a success check for two seconds after a successful snapshot refresh.
    refresh_success_until: Option<tokio::time::Instant>,
    /// Start time used to animate the in-progress refresh indicator.
    refresh_started_at: Option<tokio::time::Instant>,
    /// Compact status text for the header. This may be replaced by later refreshes.
    error: Option<String>,
    /// Immutable full diagnostic of the latest failure. It is never cleared by a successful
    /// refresh, so the F2 inspector remains useful while the monitor keeps updating.
    error_report: Option<String>,
    /// Full error inspector, opened with F2 so the compact header never blocks the UI.
    error_dialog: bool,
    snapshot: Option<MonitorSnapshot>,
    /// Prices whose observed execution state changed, retained briefly for visual feedback.
    price_highlights: Vec<(Decimal, tokio::time::Instant)>,
    /// Human-readable reason for the latest grid update, shown in the monitor summary.
    grid_change_notice: Option<String>,
    /// Width of the Configure form column, recorded at render time so mouse clicks on the
    /// right-hand explanation panel are not treated as form-field clicks.
    form_width: u16,
    /// Actual list rectangle of the market-picker overlay for correct mouse hit testing.
    market_list_area: Rect,
    /// Exact grid geometry produced during the last render, reused for mouse hit testing.
    grid_geometry: Option<GridGeometry>,
}
impl App {
    /// Preserve the complete anyhow error chain separately from the compact header status.
    fn set_error(&mut self, error: impl std::fmt::Display) {
        let message = format!("{error:#}");
        self.error = Some(message.clone());
        self.error_report = Some(message);
        // Open the diagnostic automatically for failures. Esc/F2 closes it; the persistent
        // error_report means the next F2 can reopen it even after background refreshes.
        self.error_dialog = true;
    }

    fn new(tab: usize, settings: Settings, profile_name: String) -> Self {
        Self {
            tab,
            field_index: 0,
            selected_level: 0,
            grid_scroll: 0,
            markets: Vec::new(),
            markets_loaded_for: None,
            market_picker: None,
            editing: None,
            edit_before: String::new(),
            password: String::new(),
            password_purpose: None,
            funding_dialog: None,
            funding_selection: None,
            funding_drag_start: None,
            funding_scroll: 0,
            profile_name,
            settings,
            settings_revision: 0,
            refresh_now: true,
            snapshot_pending: false,
            execution_pending: false,
            execute_requested: false,
            refresh_success_until: None,
            refresh_started_at: None,
            error: None,
            error_report: None,
            error_dialog: false,
            snapshot: None,
            price_highlights: Vec::new(),
            grid_change_notice: None,
            form_width: 0,
            market_list_area: Rect::default(),
            grid_geometry: None,
        }
    }

    /// Persists the current settings, encrypting the API key with the supplied password.
    fn save_profile(&mut self, password: &str) -> Result<()> {
        let mut store = ProfileStore::load()?;
        let mut data = self.settings.to_profile();
        if !self.settings.api_key.trim().is_empty() {
            data.encrypted_api_key =
                Some(profile::encrypt_secret(password, &self.settings.api_key)?);
        }
        if !self.settings.aptos_private_key.trim().is_empty() {
            data.encrypted_aptos_private_key = Some(profile::encrypt_secret(
                password,
                &self.settings.aptos_private_key,
            )?);
        }
        store.put(&self.profile_name, data);
        store.save()
    }

    /// Applies the collected password to whichever operation requested it.
    fn submit_password(&mut self) {
        let password = std::mem::take(&mut self.password);
        let purpose = self.password_purpose.take();
        if password.is_empty() {
            return;
        }
        let result = match purpose {
            Some(PasswordPurpose::SaveProfile) => self
                .save_profile(&password)
                .map(|()| self.settings.tr(TKey::ProfileSaved).to_owned()),
            Some(PasswordPurpose::LoadProfile) => self
                .load_encrypted_key(&password)
                .map(|()| self.settings.tr(TKey::ProfileLoaded).to_owned()),
            None => return,
        };
        match result {
            Ok(message) => self.error = Some(message),
            Err(_) => self.error = Some(self.settings.tr(TKey::PasswordWrong).to_owned()),
        }
    }

    /// Decrypts every credential stored in the profile with the supplied password.
    fn load_encrypted_key(&mut self, password: &str) -> Result<()> {
        let store = ProfileStore::load()?;
        let data = store.get(&self.profile_name).ok_or_else(|| {
            anyhow::anyhow!("no encrypted credentials are stored in this profile")
        })?;
        let mut loaded = false;
        if let Some(api_key) = data.encrypted_api_key.as_ref() {
            self.settings.api_key = profile::decrypt_secret(password, api_key)?;
            loaded = true;
        }
        if let Some(private_key) = data.encrypted_aptos_private_key.as_ref() {
            self.settings.aptos_private_key = profile::decrypt_secret(password, private_key)?;
            loaded = true;
        }
        if !loaded {
            anyhow::bail!("no encrypted credentials are stored in this profile")
        }
        self.settings_revision += 1;
        self.snapshot = None;
        self.markets.clear();
        self.markets_loaded_for = None;
        self.refresh_now = true;
        Ok(())
    }

    fn filtered_markets(&self) -> Vec<Market> {
        let query = self
            .market_picker
            .as_ref()
            .map(|picker| picker.query.to_ascii_lowercase())
            .unwrap_or_default();
        self.markets
            .iter()
            .filter(|market| query.is_empty() || market.name.to_ascii_lowercase().contains(&query))
            .cloned()
            .collect()
    }

    fn open_market_picker(&mut self) {
        self.market_picker = Some(MarketPicker::new());
        self.refresh_now = true;
        self.error = None;
    }

    fn close_market_picker(&mut self) {
        self.market_picker = None;
    }

    /// Copies the highlighted market row into the grid settings.
    fn use_selected_market(&mut self) {
        let selected = self
            .market_picker
            .as_ref()
            .and_then(|picker| self.filtered_markets().get(picker.selected).cloned());
        if let Some(market) = selected {
            self.settings.market = market.name.clone();
            self.settings_revision += 1;
            self.snapshot = None;
            self.refresh_now = true;
            self.error = Some(format!("Market selected: {}", market.name));
            self.close_market_picker();
        }
    }

    fn reset_profile(&mut self) -> Result<()> {
        let mut store = ProfileStore::load()?;
        store.remove(&self.profile_name);
        store.save()?;
        // Keep the API key so the user is not locked out of the API mid-session.
        let api_key = self.settings.api_key.clone();
        let language = self.settings.language;
        self.settings = Settings::defaults();
        self.settings.api_key = api_key;
        self.settings.language = language;
        self.settings_revision += 1;
        self.snapshot = None;
        self.markets.clear();
        self.markets_loaded_for = None;
        self.refresh_now = true;
        Ok(())
    }
    fn active_field(&self) -> Field {
        FIELDS[self.field_index]
    }
    fn current_visible_fields(&self) -> Vec<Field> {
        FIELDS
            .into_iter()
            .filter(|field| field.visible(&self.settings))
            .collect()
    }
    fn select_next_field(&mut self, delta: isize) {
        let visible = self.current_visible_fields();
        let current = visible
            .iter()
            .position(|field| *field == self.active_field())
            .unwrap_or(0) as isize;
        let next = (current + delta).clamp(0, visible.len().saturating_sub(1) as isize) as usize;
        self.field_index = FIELDS
            .iter()
            .position(|field| *field == visible[next])
            .unwrap_or(0);
    }
    fn field_value(&self, field: Field) -> String {
        match field {
            Field::ApiKey => self.settings.masked_key(),
            Field::AptosPrivateKey => self.settings.masked_private_key(),
            Field::Language => self.settings.language.name().to_owned(),
            Field::Network => self.settings.network.clone(),
            Field::Product => format!("{:?}", self.settings.product),
            Field::Market => self.settings.market.clone(),
            Field::Subaccount => {
                if self.settings.subaccount.is_empty() {
                    self.settings.tr(TKey::Optional).to_owned()
                } else {
                    self.settings.subaccount.clone()
                }
            }
            Field::PerpMode => format!("{:?}", self.settings.perp_mode),
            Field::RangeKind => self
                .settings
                .tr(match self.settings.range_kind {
                    RangeKind::Percent => TKey::RangePercentLabel,
                    RangeKind::Step => TKey::RangeStepLabel,
                    RangeKind::Bounds => TKey::RangeBoundsLabel,
                })
                .to_owned(),
            Field::RangeValue => self.settings.range_value.clone(),
            Field::UpperBound => self.settings.upper_bound.clone(),
            Field::GridCount => self.settings.grid_count.clone(),
            Field::AllocationKind => self
                .settings
                .tr(match self.settings.allocation_kind {
                    AllocationKind::Budget => TKey::AllocationBudgetLabel,
                    AllocationKind::FixedSize => TKey::AllocationSizeLabel,
                })
                .to_owned(),
            Field::AllocationValue => self.settings.allocation_value.clone(),
            Field::MakerFee => self.settings.maker_fee_rate.clone(),
            Field::PreviewLeverage => self.settings.preview_leverage.clone(),
            Field::RefreshSeconds => self.settings.refresh_seconds.clone(),
            Field::PriceSource => format!("{:?}", self.settings.price_source),
            Field::ExitAssetPolicy => format!("{:?}", self.settings.exit_asset_policy),
        }
    }
    fn editable_value_mut(&mut self, field: Field) -> Option<&mut String> {
        match field {
            Field::ApiKey => Some(&mut self.settings.api_key),
            Field::AptosPrivateKey => Some(&mut self.settings.aptos_private_key),
            Field::Market => Some(&mut self.settings.market),
            Field::Subaccount => Some(&mut self.settings.subaccount),
            Field::RangeValue => Some(&mut self.settings.range_value),
            Field::UpperBound => Some(&mut self.settings.upper_bound),
            Field::GridCount => Some(&mut self.settings.grid_count),
            Field::AllocationValue => Some(&mut self.settings.allocation_value),
            Field::MakerFee => Some(&mut self.settings.maker_fee_rate),
            Field::PreviewLeverage => Some(&mut self.settings.preview_leverage),
            Field::RefreshSeconds => Some(&mut self.settings.refresh_seconds),
            _ => None,
        }
    }
    fn start_edit(&mut self) {
        let field = self.active_field();
        if field.editable() {
            self.edit_before = self
                .editable_value_mut(field)
                .map(std::mem::take)
                .unwrap_or_default();
            self.editing = Some(field);
            self.error = None;
        }
    }
    fn finish_edit(&mut self, save: bool) {
        if let Some(field) = self.editing.take() {
            if !save {
                let previous = std::mem::take(&mut self.edit_before);
                if let Some(value) = self.editable_value_mut(field) {
                    *value = previous;
                }
            } else {
                self.edit_before.clear();
                self.settings_revision += 1;
                self.snapshot = None;
                self.refresh_now = true;
                self.error = Some(
                    "Setting saved for this session only. Press P for preview or R for monitor."
                        .to_owned(),
                );
            }
        }
    }
    fn cycle_field(&mut self, direction: i8) {
        let field = self.active_field();
        match field {
            Field::Language => self.settings.language = self.settings.language.toggled(),
            Field::Network => {
                self.settings.network = if self.settings.network == "mainnet" {
                    "testnet".to_owned()
                } else {
                    "mainnet".to_owned()
                }
            }
            Field::Product => {
                self.settings.product = if self.settings.product == Product::Perp {
                    Product::Spot
                } else {
                    Product::Perp
                };
                // Spot has no directional perp mode or leverage setting. Keep the values
                // internally valid and let visibility remove those controls from the form.
                if self.settings.product == Product::Spot {
                    self.settings.perp_mode = PerpMode::Neutral;
                    self.settings.preview_leverage = "1".to_owned();
                }
            }
            Field::PerpMode => {
                self.settings.perp_mode = match (self.settings.perp_mode, direction >= 0) {
                    (PerpMode::Neutral, true) | (PerpMode::Short, false) => PerpMode::Long,
                    (PerpMode::Long, true) | (PerpMode::Neutral, false) => PerpMode::Short,
                    _ => PerpMode::Neutral,
                }
            }
            Field::RangeKind => {
                self.settings.range_kind = match (self.settings.range_kind, direction >= 0) {
                    (RangeKind::Percent, true) | (RangeKind::Bounds, false) => RangeKind::Step,
                    (RangeKind::Step, true) | (RangeKind::Percent, false) => RangeKind::Bounds,
                    _ => RangeKind::Percent,
                }
            }
            Field::AllocationKind => {
                self.settings.allocation_kind =
                    if self.settings.allocation_kind == AllocationKind::Budget {
                        AllocationKind::FixedSize
                    } else {
                        AllocationKind::Budget
                    }
            }
            Field::PriceSource => {
                self.settings.price_source = if self.settings.price_source == PriceSource::Prices {
                    PriceSource::Depth
                } else {
                    PriceSource::Prices
                }
            }
            Field::ExitAssetPolicy => {
                self.settings.exit_asset_policy =
                    if self.settings.exit_asset_policy == ExitAssetPolicy::Retain {
                        ExitAssetPolicy::Sell
                    } else {
                        ExitAssetPolicy::Retain
                    }
            }
            _ => return,
        };
        self.settings_revision += 1;
        self.snapshot = None;
        self.refresh_now = true;
    }
}

fn decimal(value: &str) -> Result<Decimal> {
    Decimal::from_str(value).context("invalid decimal")
}

fn parse_duration_list_ms(value: &str) -> Result<Vec<Duration>> {
    let durations = value
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(|part| {
            part.parse::<u64>()
                .map(Duration::from_millis)
                .with_context(|| format!("invalid millisecond duration {part:?}"))
        })
        .collect::<Result<Vec<_>>>()?;
    if durations.is_empty() || durations.iter().any(Duration::is_zero) {
        anyhow::bail!("duration backoff lists must contain positive millisecond values")
    }
    Ok(durations)
}

fn has_complete_grid_config(args: &Args) -> bool {
    let range = matches!(
        (
            &args.lower_price,
            &args.upper_price,
            &args.range_percent,
            &args.grid_step_percent
        ),
        (Some(_), Some(_), None, None) | (None, None, Some(_), None) | (None, None, None, Some(_))
    );
    let allocation = matches!(
        (&args.total_budget, &args.order_size),
        (Some(_), None) | (None, Some(_))
    ) || (args.product == Product::Spot
        && (args.total_quote_budget.is_some() || args.total_base_budget.is_some()));
    range && allocation
}

fn print_snapshot(snapshot: &MonitorSnapshot, config: &GridConfig) {
    let profit = snapshot.plan.profit_preview(config.maker_fee_rate);
    println!(
        "{} {:?} mid={} net-scenario={}",
        snapshot.market.name, snapshot.market.product, snapshot.plan.mid, profit.net_capture
    );
    for level in snapshot.plan.all_levels() {
        println!(
            "{:3} {:>12} × {:>10} {:?}",
            level.side.as_str(),
            format_decimal(level.price, 8),
            format_decimal(level.size, 8),
            level.state
        );
    }
}

/// Send this process's stdout and stderr to `path`, replacing any previous contents.
///
/// This replaces file descriptors 1 and 2 (or the Windows standard handles) rather than merely
/// wrapping `println!`, so panic reports and anything a dependency writes directly to those
/// descriptors land in the same file. Rust's stdout is line-buffered even when it is not a
/// terminal, so the file stays readable while a long `run` is still going.
#[cfg(unix)]
fn redirect_output_to_log(path: &Path) -> Result<()> {
    use std::os::fd::AsRawFd;

    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("cannot create log directory {}", parent.display()))?;
    }
    // Truncating open: each run starts from a clean file instead of appending to stale output.
    let file = fs::File::create(path)
        .with_context(|| format!("cannot create log file {}", path.display()))?;
    let fd = file.as_raw_fd();
    for (target, name) in [
        (libc::STDOUT_FILENO, "stdout"),
        (libc::STDERR_FILENO, "stderr"),
    ] {
        // SAFETY: `fd` is a valid descriptor owned by `file` and still open here, and `target`
        // is one of the two standard descriptors, which are always valid dup2 targets.
        if unsafe { libc::dup2(fd, target) } < 0 {
            return Err(io::Error::last_os_error())
                .with_context(|| format!("cannot redirect {name} to {}", path.display()));
        }
    }
    // Descriptors 1 and 2 now reference the same open file, so the original handle is redundant.
    drop(file);
    Ok(())
}

#[cfg(windows)]
fn redirect_output_to_log(path: &Path) -> Result<()> {
    use std::os::windows::io::AsRawHandle;

    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("cannot create log directory {}", parent.display()))?;
    }
    let file = fs::File::create(path)
        .with_context(|| format!("cannot create log file {}", path.display()))?;
    let handle = file.as_raw_handle();

    // SAFETY: `handle` is a valid file handle owned by `file`. It is intentionally leaked so
    // the standard handles remain valid for the lifetime of the process.
    unsafe {
        let ok = windows_sys::Win32::System::Console::SetStdHandle(
            windows_sys::Win32::System::Console::STD_OUTPUT_HANDLE,
            handle as _,
        ) != 0
            && windows_sys::Win32::System::Console::SetStdHandle(
                windows_sys::Win32::System::Console::STD_ERROR_HANDLE,
                handle as _,
            ) != 0;
        if !ok {
            return Err(io::Error::last_os_error())
                .with_context(|| format!("cannot redirect stdout/stderr to {}", path.display()));
        }
    }
    std::mem::forget(file);
    Ok(())
}

struct SubaccountRunLock {
    // Held for the lifetime of the struct; the lock is released when this field is dropped.
    #[allow(dead_code)]
    lock: fslock::LockFile,
}

impl SubaccountRunLock {
    /// Acquire one non-blocking process lock per network/subaccount. The lock is intentionally
    /// independent of market so two processes cannot race bulk sequence numbers or funding for
    /// different markets on the same subaccount.
    fn acquire(network: &str, subaccount: &str) -> Result<Self> {
        use sha3::{Digest, Sha3_256};

        let base = std::env::var_os("DECIBEL_GRID_DATA_DIR")
            .map(PathBuf::from)
            .or_else(|| dirs::data_local_dir().or_else(dirs::data_dir))
            .ok_or_else(|| anyhow::anyhow!("could not determine the local data directory"))?;
        let lock_dir = base.join("decibel-grid").join("locks");
        fs::create_dir_all(&lock_dir)
            .with_context(|| format!("could not create lock directory {}", lock_dir.display()))?;
        let canonical_subaccount = subaccount
            .trim()
            .trim_start_matches("0x")
            .trim_start_matches('0')
            .to_ascii_lowercase();
        let key = format!(
            "{}:{}",
            network.trim().to_ascii_lowercase(),
            canonical_subaccount
        );
        let digest = hex::encode(Sha3_256::digest(key.as_bytes()));
        let path = lock_dir.join(format!("subaccount-{digest}.lock"));
        let mut lock = fslock::LockFile::open(&path)
            .with_context(|| format!("could not open subaccount lock {}", path.display()))?;
        if !lock
            .try_lock()
            .with_context(|| format!("could not acquire subaccount lock {}", path.display()))?
        {
            anyhow::bail!(
                "another grid process is already running for network {} and this subaccount; stop it before starting a second instance",
                network
            )
        }
        Ok(Self { lock })
    }
}

fn install_panic_reporter() {
    let previous = panic::take_hook();
    panic::set_hook(Box::new(move |info: &PanicHookInfo<'_>| {
        let backtrace = Backtrace::force_capture();
        let location = info
            .location()
            .map(|location| {
                format!(
                    "{}:{}:{}",
                    location.file(),
                    location.line(),
                    location.column()
                )
            })
            .unwrap_or_else(|| "unknown location".to_owned());
        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| info.payload().downcast_ref::<String>().map(String::as_str))
            .unwrap_or("non-string panic payload");
        let report = format!(
            "Rust panic\n\nmessage: {payload}\nlocation: {location}\n\nbacktrace:\n{backtrace}\n"
        );
        let path = save_error_report(&report).ok();

        // Panic can happen while crossterm is in raw/alternate-screen mode. Restore the
        // terminal before printing, otherwise the panic text remains trapped in the TUI.
        let mut stdout = io::stdout();
        let _ = disable_raw_mode();
        let _ = execute!(stdout, DisableMouseCapture, LeaveAlternateScreen);
        let _ = writeln!(
            stdout,
            "\nDecibel Grid TUI panicked: {payload}\nLocation: {location}"
        );
        if let Some(path) = path {
            let _ = writeln!(stdout, "Full panic report: {}", path.display());
        }
        let _ = stdout.flush();
        previous(info);
    }));
}

#[tokio::main]
async fn main() -> Result<()> {
    // reqwest 0.13's `rustls` feature hard-selects aws-lc-rs, and aptos-sdk pulls it in with
    // default features, so aws-lc-rs is the only provider in the tree. Install it explicitly
    // before any TLS handshake: rustls refuses to guess, and tokio-tungstenite builds its
    // ClientConfig lazily on first connect, which is where the panic surfaced.
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("failed to install rustls aws-lc-rs CryptoProvider"))?;
    install_panic_reporter();
    dotenv().ok();
    let cli = Cli::parse();
    let opens_tui = matches!(&cli.command, Some(Cmd::Preview | Cmd::Tui) | None);
    let engine_log = if matches!(&cli.command, Some(Cmd::Engine)) {
        Some(control_paths(&Settings::from(&cli.args))?.log)
    } else {
        None
    };
    let log_path = cli.args.log_file.clone().or(engine_log);
    if let Some(path) = log_path.as_deref() {
        if opens_tui {
            anyhow::bail!("--log-file is only supported by CLI commands, not TUI/preview")
        }
        redirect_output_to_log(path)?;
        println!(
            "CLI log started; output is being overwritten at {}",
            path.display()
        );
    }
    match cli.command {
        Some(Cmd::Start) => {
            start_cli(
                Settings::from(&cli.args),
                cli.args.confirm_mainnet.as_deref(),
            )
            .await
        }
        Some(Cmd::Engine) => {
            engine_cli(
                Settings::from(&cli.args),
                cli.args.confirm_mainnet.as_deref(),
            )
            .await
        }
        Some(Cmd::Logs) => logs_client(Settings::from(&cli.args), cli.args.follow).await,
        Some(Cmd::Attach) => attach_client(Settings::from(&cli.args)).await,
        Some(Cmd::CheckKey) => check_api_key(Settings::from(&cli.args)).await,
        Some(Cmd::Reconcile) => reconcile_cli(Settings::from(&cli.args)).await,
        Some(Cmd::Status) => status_client(Settings::from(&cli.args)).await,
        Some(Cmd::Doctor) => doctor_cli(Settings::from(&cli.args)).await,
        Some(Cmd::Shadow) => shadow_cli(Settings::from(&cli.args), cli.args.shadow_cycles).await,
        Some(Cmd::SpotFundingSetup) => {
            spot_funding_setup_cli(
                Settings::from(&cli.args),
                cli.args.spot_funding_amount.clone(),
                cli.args.spot_funding_metadata.clone(),
            )
            .await
        }
        Some(Cmd::Run) => anyhow::bail!(
            "`run` no longer owns a live trading loop; use `start` (or let systemd/tmux run the internal `engine` command) and control it with status/logs/stop/attach"
        ),
        Some(Cmd::Stop) => {
            stop_client(
                Settings::from(&cli.args),
                cli.args.confirm_mainnet.as_deref(),
                cli.args.exit_mode.as_deref(),
            )
            .await
        }
        Some(Cmd::Preview) => {
            run_tui(
                Settings::from(&cli.args),
                cli.args.profile.clone(),
                TAB_PREVIEW,
            )
            .await
        }
        Some(Cmd::Tui) => {
            run_tui(
                Settings::from(&cli.args),
                cli.args.profile.clone(),
                TAB_CONFIG,
            )
            .await
        }
        None if has_complete_grid_config(&cli.args) => {
            run_tui(
                Settings::from(&cli.args),
                cli.args.profile.clone(),
                TAB_MONITOR,
            )
            .await
        }
        None => {
            run_tui(
                Settings::from(&cli.args),
                cli.args.profile.clone(),
                TAB_CONFIG,
            )
            .await
        }
    }
}

async fn check_api_key(settings: Settings) -> Result<()> {
    validate_api_key_format(&settings.api_key).context("API key format check failed")?;
    let api = DecibelClient::new(&settings.network, &settings.api_key)?;
    api.verify_api_key().await?;
    println!(
        "API key format is valid and the key is accepted by the {} API.",
        settings.network
    );
    Ok(())
}

struct EngineRuntimeGuard {
    paths: control::ControlPaths,
}

impl Drop for EngineRuntimeGuard {
    fn drop(&mut self) {
        self.paths.remove_runtime_files();
    }
}

fn control_paths(settings: &Settings) -> Result<control::ControlPaths> {
    control::ControlPaths::for_subaccount(&settings.subaccount)
}

async fn control_request(
    settings: &Settings,
    request: control::Request,
) -> Result<control::Response> {
    control::request(&control_paths(settings)?, &request).await
}

async fn status_client(settings: Settings) -> Result<()> {
    match control_request(&settings, control::Request::Status).await? {
        control::Response::Status { status } => {
            println!("engine pid={} phase={}", status.pid, status.phase);
            println!(
                "{} {} {} {}",
                status.network, status.subaccount, status.product, status.market
            );
            println!(
                "last cycle: {:?}; mid: {:?}",
                status.last_cycle_at, status.mid
            );
            println!(
                "reconciliation: matched={:?} missing={:?} unmanaged={:?}",
                status.matched, status.missing, status.unmanaged
            );
            if let Some(error) = status.last_error {
                println!("last error: {error}");
            }
            Ok(())
        }
        control::Response::Error { message } => {
            anyhow::bail!("engine rejected status request: {message}")
        }
        response => anyhow::bail!("unexpected engine status response: {response:?}"),
    }
}

async fn stop_client(
    settings: Settings,
    confirm_mainnet: Option<&str>,
    exit_mode: Option<&str>,
) -> Result<()> {
    if settings.network.eq_ignore_ascii_case("mainnet") && confirm_mainnet != Some("MAINNET") {
        anyhow::bail!("mainnet stop requires --confirm-mainnet MAINNET")
    }
    let mode = match exit_mode.unwrap_or("hold") {
        "hold" => control::ExitMode::Hold,
        "liquidate" => control::ExitMode::Liquidate,
        _ => anyhow::bail!("--exit-mode must be hold or liquidate"),
    };
    match control_request(&settings, control::Request::Stop { exit_mode: mode }).await? {
        control::Response::Accepted { message } => {
            println!("{message}");
            Ok(())
        }
        control::Response::Error { message } => {
            anyhow::bail!("engine rejected stop request: {message}")
        }
        response => anyhow::bail!("unexpected engine stop response: {response:?}"),
    }
}

async fn logs_client(settings: Settings, follow: bool) -> Result<()> {
    let paths = control_paths(&settings)?;
    let shown = control::tail_lines(&paths.log, 200)?;
    if !shown.is_empty() {
        println!("{shown}");
    }
    if !follow {
        return Ok(());
    }
    let mut offset = fs::metadata(&paths.log)?.len();
    loop {
        tokio::time::sleep(Duration::from_millis(400)).await;
        let bytes = fs::read(&paths.log)?;
        if bytes.len() < offset as usize {
            offset = 0;
        }
        if bytes.len() > offset as usize {
            let appended = String::from_utf8_lossy(&bytes[offset as usize..]);
            print!("{appended}");
            io::stdout().flush()?;
            offset = bytes.len() as u64;
        }
    }
}

async fn attach_client(settings: Settings) -> Result<()> {
    let client = decibel_grid_tui::client::EngineClient::for_subaccount(&settings.subaccount)?;
    decibel_grid_tui::attach_tui::run(client).await
}

async fn start_cli(settings: Settings, confirm_mainnet: Option<&str>) -> Result<()> {
    if settings.network.eq_ignore_ascii_case("mainnet") && confirm_mainnet != Some("MAINNET") {
        anyhow::bail!("mainnet start requires --confirm-mainnet MAINNET")
    }
    let paths = control_paths(&settings)?;
    paths.ensure_directory()?;
    if let Some(pid) = paths.read_pid()? {
        if control::process_is_alive(pid) {
            anyhow::bail!("engine already running for this account (pid {pid})")
        }
        paths.remove_runtime_files();
    }
    // Check the same advisory file lock used by the engine before launching. The child acquires
    // and holds it for its entire lifetime, which closes the race after this preflight check.
    drop(SubaccountRunLock::acquire(
        &settings.network,
        &settings.subaccount,
    )?);
    let executable = std::env::current_exe().context("resolve grid-bot executable")?;
    let mut args = std::env::args_os().skip(1).collect::<Vec<_>>();
    let Some(index) = args.iter().position(|arg| arg == "start") else {
        anyhow::bail!("could not rewrite start command for engine child")
    };
    args[index] = "engine".into();
    let child = Command::new(executable)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("launch grid engine child")?;
    for _ in 0..40 {
        if matches!(
            control::request(&paths, &control::Request::Ping).await,
            Ok(control::Response::Pong)
        ) {
            println!(
                "grid engine started (pid {}); socket {}",
                child.id(),
                paths.socket.display()
            );
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    anyhow::bail!(
        "engine process {} did not open its control socket; inspect {}",
        child.id(),
        paths.log.display()
    )
}

async fn engine_cli(settings: Settings, confirm_mainnet: Option<&str>) -> Result<()> {
    let paths = control_paths(&settings)?;
    paths.ensure_directory()?;
    if let Some(pid) = paths.read_pid()? {
        if control::process_is_alive(pid) {
            anyhow::bail!("engine already running for this account (pid {pid})")
        }
        paths.remove_runtime_files();
    }
    let runtime = control::EngineHandle::new(control::EngineStatus {
        pid: std::process::id(),
        started_at: Some(Utc::now()),
        network: settings.network.clone(),
        subaccount: settings.subaccount.clone(),
        market: settings.market.clone(),
        product: format!("{:?}", settings.product).to_lowercase(),
        phase: "starting".to_owned(),
        ..Default::default()
    });
    paths.write_pid(std::process::id())?;
    let _guard = EngineRuntimeGuard {
        paths: paths.clone(),
    };
    let server = control::start_server(&paths, runtime.clone()).await?;
    let result = run_cli(settings, true, confirm_mainnet, Some(runtime.clone())).await;
    runtime
        .update_status(|status| status.phase = "stopped".to_owned())
        .await;
    server.abort();
    result
}

/// Legacy direct status implementation retained for compatibility tests; the public `status`
/// command now queries the running engine's local socket.
#[allow(dead_code)]
async fn status_cli(settings: Settings) -> Result<()> {
    validate_api_key_format(&settings.api_key).context("API key format check failed")?;
    let config = settings.to_grid_config()?;
    let api = settings.api_client()?;
    let snapshot = fetch_snapshot(&api, &config, optional_subaccount(&settings)).await?;
    print_snapshot(&snapshot, &config);
    Ok(())
}

/// Legacy direct lifecycle implementation retained for compatibility tests; the public `stop`
/// command now tells the running engine to execute this shutdown flow.
#[allow(dead_code)]
async fn stop_cli(settings: Settings, confirm_mainnet: Option<&str>) -> Result<()> {
    if settings.api_key.trim().is_empty()
        || settings.aptos_private_key.trim().is_empty()
        || settings.subaccount.trim().is_empty()
    {
        anyhow::bail!("stop requires DECIBEL_API_KEY, APTOS_PRIVATE_KEY, and SUBACCOUNT_ADDRESS")
    }
    if settings.network.eq_ignore_ascii_case("mainnet") && confirm_mainnet != Some("MAINNET") {
        anyhow::bail!("mainnet stop requires --confirm-mainnet MAINNET")
    }
    let _lock = SubaccountRunLock::acquire(&settings.network, &settings.subaccount)?;
    let mut config = settings.to_grid_config()?;
    let api = settings.api_client()?;
    let market = api.market(&config.market_name, config.product).await?;
    match settings.exit_asset_policy {
        ExitAssetPolicy::Retain => {
            let hash = spot_lifecycle::cancel_bulk_ladder(
                &settings.network,
                &settings.aptos_private_key,
                &settings.subaccount,
                &market,
            )
            .await?;
            println!("Grid stopped: ladder cancelled in tx {hash}; assets retained.");
        }
        ExitAssetPolicy::Sell => {
            let spot_guard = if market.product == Product::Spot {
                let rates = api.spot_fee_rates(&settings.subaccount).await?;
                config.maker_fee_rate = rates.maker_rate;
                Some((config.spot, rates))
            } else {
                None
            };
            let guard_refs = spot_guard.as_ref().map(|(policy, rates)| (policy, rates));
            let hashes = exit_sell_assets(
                &settings.network,
                &settings.api_key,
                &settings.aptos_private_key,
                &settings.subaccount,
                &market,
                guard_refs,
            )
            .await?;
            println!(
                "Grid stopped and liquidation attempted in {} transaction(s): {:?}",
                hashes.len(),
                hashes
            );
        }
    }
    Ok(())
}

/// Explicit, operator-confirmed Cross→PFS transfer. The bot does not attempt to set
/// HOLD_AS_NON_COLLATERAL: that entry function is owner-only, while the bot signer may only have
/// delegated trading/funds permissions. The operator must set the future-settlement flag manually
/// in the Decibel UI/wallet first.
async fn spot_funding_setup_cli(
    settings: Settings,
    amount: String,
    metadata: Option<String>,
) -> Result<()> {
    if settings.aptos_private_key.trim().is_empty() {
        anyhow::bail!("spot-funding-setup requires APTOS_PRIVATE_KEY")
    }
    if settings.subaccount.trim().is_empty() {
        anyhow::bail!("spot-funding-setup requires SUBACCOUNT_ADDRESS")
    }
    let metadata = metadata.unwrap_or_else(|| decibel_grid_tui::TESTNET_USDC_METADATA.to_owned());
    let amount_decimal = Decimal::from_str(amount.trim())
        .context("--spot-funding-amount/SPOT_FUNDING_AMOUNT must be a decimal USDC amount")?;
    if amount_decimal < Decimal::ZERO {
        anyhow::bail!("--spot-funding-amount/SPOT_FUNDING_AMOUNT cannot be negative")
    }
    println!(
        "NOTICE: HOLD_AS_NON_COLLATERAL is owner-only and is not submitted by this bot. Set it manually in the Decibel UI/wallet before relying on future Spot proceeds staying in PFS."
    );
    if amount_decimal.is_zero() {
        println!("No transfer amount given (0); skipping the Cross→PFS transfer.");
        return Ok(());
    }
    let raw = (amount_decimal * Decimal::from(1_000_000u64))
        .floor()
        .to_i64()
        .ok_or_else(|| anyhow::anyhow!("--spot-funding-amount is outside the supported range"))?;
    println!("Transferring {amount_decimal} USDC from Cross to PFS...");
    let transfer_tx = decibel_grid_tui::transfer_spot_cross_pfs(
        &settings.network,
        &settings.aptos_private_key,
        &settings.subaccount,
        &metadata,
        -raw,
    )
    .await?;
    println!("  Transfer submitted. tx {transfer_tx}");
    Ok(())
}

/// Verify the prerequisites for a safe Testnet/Mainnet run without modifying Decibel state.
async fn doctor_cli(settings: Settings) -> Result<()> {
    validate_api_key_format(&settings.api_key).context("API key format check failed")?;
    if settings.subaccount.trim().is_empty() {
        anyhow::bail!("doctor requires SUBACCOUNT_ADDRESS")
    }
    let config = settings.to_grid_config()?;
    let api = settings.api_client()?;
    api.verify_api_key()
        .await
        .context("API key verification failed")?;
    let (snapshot, result) = reconcile_snapshot(&api, &config, &settings.subaccount).await?;
    println!(
        "DOCTOR OK — {} {} on {}",
        snapshot.market.name,
        match config.product {
            Product::Spot => "Spot",
            Product::Perp => "Perp",
        },
        settings.network
    );
    println!(
        "  rules: tick={} lot={} min_size={}",
        snapshot.market.tick_size, snapshot.market.lot_size, snapshot.market.min_size
    );
    println!(
        "  plan: {} bid(s), {} ask(s), quote={}, base={}",
        snapshot.plan.bids.len(),
        snapshot.plan.asks.len(),
        snapshot.plan.quote_required,
        snapshot.plan.base_required
    );
    match config.product {
        Product::Spot => {
            let funds = snapshot.account.spot_funds.as_ref().ok_or_else(|| {
                anyhow::anyhow!("spot PFS balances unavailable in account overview")
            })?;
            println!(
                "  PFS: {} {} available, {} {} available",
                funds.available_base(),
                funds.base_symbol,
                funds.available_quote(),
                funds.quote_symbol
            );
            // A bulk replacement also gets credit for whatever is already escrowed in the
            // resting ladder, so report that separately rather than implying it is unusable.
            if funds.base_reserved > Decimal::ZERO || funds.quote_reserved > Decimal::ZERO {
                println!(
                    "  bulk escrow (credited on replacement): {} {}, {} {} → usable {} {} / {} {}",
                    funds.base_reserved,
                    funds.base_symbol,
                    funds.quote_reserved,
                    funds.quote_symbol,
                    funds.available_base_for_bulk(),
                    funds.base_symbol,
                    funds.available_quote_for_bulk(),
                    funds.quote_symbol
                );
            }
            if funds.quote_cross_balance() >= USDC_CROSS_DUST {
                println!(
                    "  note: {} {} sits in Cross and is NOT spendable by spot bulk orders; transfer it into PFS to fund bids.",
                    funds.quote_cross_balance(),
                    funds.quote_symbol
                );
            }
            if funds.available_base_for_bulk() < snapshot.plan.base_required
                || funds.available_quote_for_bulk() < snapshot.plan.quote_required
            {
                println!(
                    "  note: the pinned Spot grid is underfunded; it will not be placed until the missing asset is funded."
                );
            }
        }
        Product::Perp => {
            let margin = snapshot.account.available_margin.ok_or_else(|| {
                anyhow::anyhow!("available Perp margin unavailable in account overview")
            })?;
            let required = snapshot.plan.estimated_margin.unwrap_or(Decimal::ZERO);
            println!("  margin: available={} estimated={}", margin, required);
            if margin < required {
                anyhow::bail!(
                    "estimated Perp margin {} exceeds available {}",
                    required,
                    margin
                )
            }
        }
    }
    println!("  reconciliation: {}", result.summary());
    let blocking = decibel_grid_tui::reconcile::blocking_orders(&result.unmanaged);
    if !blocking.is_empty() {
        println!(
            "  warning: {} standalone order(s) of unprovable ownership will block live bulk replacement.",
            blocking.len()
        );
    } else if !result.unmanaged.is_empty() {
        println!(
            "  note: {} unmanaged level(s) belong to this account's bulk ladder; a new bulk submission replaces them atomically.",
            result.unmanaged.len()
        );
    }
    println!("  result: read-only checks passed; no exchange state changed.");
    Ok(())
}

/// Compare the current desired grid with open orders. This is intentionally read-only: any order
/// not exactly covered by the current plan remains unmanaged until a future client-ID-backed
/// execution ledger can establish ownership safely.
async fn reconcile_cli(settings: Settings) -> Result<()> {
    validate_api_key_format(&settings.api_key).context("API key format check failed")?;
    if settings.subaccount.trim().is_empty() {
        anyhow::bail!("reconcile requires SUBACCOUNT_ADDRESS")
    }
    let config = settings.to_grid_config()?;
    let api = settings.api_client()?;
    let (snapshot, result) = reconcile_snapshot(&api, &config, &settings.subaccount).await?;
    print_snapshot(&snapshot, &config);
    println!("RECONCILE-ONLY — {}", result.summary());
    for order in &result.missing {
        println!(
            "  MISSING {} {} @ {}",
            order.side.as_str(),
            format_decimal(order.size, 8),
            format_decimal(order.price, 8)
        );
    }
    for order in &result.unmanaged {
        println!(
            "  UNMANAGED {} {} @ {} (order {})",
            order.side.as_str(),
            format_decimal(order.remaining_size, 8),
            format_decimal(order.price, 8),
            order.order_id
        );
    }
    if result.is_converged() {
        println!("Grid and exchange snapshot converge; no changes were made.");
    } else {
        println!("No changes were made. Unmanaged orders are never cancelled automatically.");
    }
    Ok(())
}

/// Continuous shadow reconciliation: the same loop as `run -e` but never signs or submits.
/// Every cycle fetches a snapshot, reconciles, journals events, and reports drift — without
/// sending any Aptos transaction. Use this as a long-lived dry-run monitor that produces a
/// complete audit trail.
async fn shadow_cli(settings: Settings, max_cycles: Option<usize>) -> Result<()> {
    validate_api_key_format(&settings.api_key).context("API key format check failed")?;
    if settings.subaccount.trim().is_empty() {
        anyhow::bail!("shadow requires SUBACCOUNT_ADDRESS")
    }
    if max_cycles == Some(0) {
        anyhow::bail!("shadow --cycles must be at least 1")
    }
    let _subaccount_lock = SubaccountRunLock::acquire(&settings.network, &settings.subaccount)?;
    let config = settings.to_grid_config()?;
    let api = settings.api_client()?;
    let run_id = journal::generate_run_id();
    let journal = journal::Journal::new(&run_id)
        .context("shadow reconciliation requires a writable run journal")?;
    let mut metadata = journal::RunMetadata {
        run_id: run_id.clone(),
        started_at: Utc::now(),
        network: settings.network.clone(),
        subaccount: settings.subaccount.clone(),
        market: config.market_name.clone(),
        product: format!("{:?}", config.product).to_lowercase(),
        config_hash: {
            use sha3::{Digest, Sha3_256};
            hex::encode(Sha3_256::digest(format!("{config:?}")))
        },
        program_version: env!("CARGO_PKG_VERSION").to_owned(),
    };
    metadata.fingerprint_subaccount();
    let _ = journal.append(&journal::JournalEvent::RunStart(metadata));
    println!("Shadow reconciliation run {run_id}. No orders will be placed or cancelled.");
    if config.product == Product::Spot {
        println!("Spot: only PFS balances will be used. No automatic Cross→PFS transfer.");
    }
    let mut remaining_cycles = max_cycles.unwrap_or(usize::MAX);
    loop {
        let cycle_start = tokio::time::Instant::now();
        let snapshot = match fetch_snapshot(&api, &config, optional_subaccount(&settings)).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("shadow refresh failed: {e:#}");
                tokio::time::sleep(config.refresh).await;
                continue;
            }
        };
        let mut snapshot = snapshot;
        // Preserve the fixed Spot geometry across refreshes; only clear historical fill markers.
        snapshot.plan = snapshot.plan.executable();
        if let Some(adjustment) = fit_spot_snapshot_to_pfs(&mut snapshot)? {
            println!("Spot funding check: {adjustment}");
        }
        print_snapshot(&snapshot, &config);
        let event = journal::JournalEvent::PlanGenerated {
            at: Utc::now(),
            mid: snapshot.plan.mid.normalize().to_string(),
            bid_levels: snapshot.plan.bids.len(),
            ask_levels: snapshot.plan.asks.len(),
            quote_required: snapshot.plan.quote_required.normalize().to_string(),
            base_required: snapshot.plan.base_required.normalize().to_string(),
        };
        journal.append(&event)?;
        if let Ok(actual) = api
            .open_orders(&settings.subaccount, &snapshot.market)
            .await
        {
            let desired = decibel_grid_tui::reconcile::desired_orders(
                &snapshot.plan,
                snapshot.market.tick_size,
                snapshot.market.lot_size,
            );
            let result = decibel_grid_tui::reconcile::reconcile(
                &desired,
                &actual,
                snapshot.market.tick_size,
                snapshot.market.lot_size,
            );
            println!("SHADOW RECONCILE — {}", result.summary());
            let event = journal::JournalEvent::ReconciliationResult {
                at: Utc::now(),
                matched: result.matched.len(),
                missing: result.missing.len(),
                unmanaged: result.unmanaged.clone(),
                is_converged: result.is_converged(),
            };
            journal.append(&event)?;
            let blocking = decibel_grid_tui::reconcile::blocking_orders(&result.unmanaged);
            if !blocking.is_empty() {
                println!(
                    "  {} standalone order(s) of unprovable ownership detected. Bulk replacement would be blocked until operator review.",
                    blocking.len()
                );
            } else if !result.unmanaged.is_empty() {
                println!(
                    "  {} unmanaged level(s) belong to this account's bulk ladder; a new bulk submission would replace them atomically.",
                    result.unmanaged.len()
                );
            }
            remaining_cycles = remaining_cycles.saturating_sub(1);
            if remaining_cycles == 0 {
                journal.append(&journal::JournalEvent::Shutdown {
                    at: Utc::now(),
                    reason: "requested shadow cycle limit reached".to_owned(),
                })?;
                println!("Shadow cycle limit reached. No orders were placed or cancelled.");
                return Ok(());
            }
        }
        let elapsed = cycle_start.elapsed();
        let wait = config.refresh.saturating_sub(elapsed);
        tokio::time::sleep(wait).await;
    }
}

/// Run the grid from a non-interactive terminal.
///
/// Modes:
/// - default: dry-run monitor (fetch + print, no exchange mutations)
/// - `-e` / `--execute`: reconciliation-based live execution. Each cycle:
///   1. Fetch snapshot + open orders
///   2. Reconcile desired vs actual
///   3. If any open orders exist (no client-order ID), halt new submissions
///   4. If market is empty and desired levels exist, submit the **full** desired plan
///   5. Persist every step to an append-only event journal
async fn run_cli(
    settings: Settings,
    execute: bool,
    confirm_mainnet: Option<&str>,
    engine_runtime: Option<control::EngineHandle>,
) -> Result<()> {
    if execute
        && (settings.api_key.trim().is_empty()
            || settings.aptos_private_key.trim().is_empty()
            || settings.subaccount.trim().is_empty())
    {
        anyhow::bail!("-e requires DECIBEL_API_KEY, APTOS_PRIVATE_KEY, and SUBACCOUNT_ADDRESS")
    }
    if execute
        && settings.network.eq_ignore_ascii_case("mainnet")
        && confirm_mainnet != Some("MAINNET")
    {
        anyhow::bail!(
            "Mainnet execution requires --confirm-mainnet MAINNET (or CONFIRM_MAINNET=MAINNET)"
        )
    }
    // Keep the guard for the entire live run. This covers bulk sequence reads, replacements, and
    // Spot funding, preventing two processes for the same network/subaccount from racing each
    // other. Read-only `run` monitoring does not need the lock.
    let _subaccount_lock = if execute {
        Some(SubaccountRunLock::acquire(
            &settings.network,
            &settings.subaccount,
        )?)
    } else {
        None
    };
    let mut config = settings.to_grid_config()?;
    let api = settings.api_client()?;
    let spot_fee_rates = if execute && config.product == Product::Spot {
        let rates = api
            .spot_fee_rates(&settings.subaccount)
            .await
            .context("fetch required live Spot fee rates")?;
        config.maker_fee_rate = rates.maker_rate;
        println!(
            "Spot fee schedule: maker={} taker={}",
            rates.maker_rate, rates.taker_rate
        );
        Some(rates)
    } else {
        None
    };
    let run_id = if execute && config.product == Product::Spot {
        journal::persistent_run_id(&settings.network, &settings.subaccount, &config.market_name)
    } else {
        journal::generate_run_id()
    };
    let journal = if execute {
        Some(
            journal::Journal::new(&run_id)
                .context("live execution requires a writable run journal")?,
        )
    } else {
        journal::Journal::new(&run_id).ok()
    };
    let config_hash = {
        use sha3::{Digest, Sha3_256};
        hex::encode(Sha3_256::digest(format!("{config:?}")))
    };
    let mut metadata = journal::RunMetadata {
        run_id: run_id.clone(),
        started_at: Utc::now(),
        network: settings.network.clone(),
        subaccount: settings.subaccount.clone(),
        market: config.market_name.clone(),
        product: format!("{:?}", config.product).to_lowercase(),
        config_hash,
        program_version: env!("CARGO_PKG_VERSION").to_owned(),
    };
    metadata.fingerprint_subaccount();
    let mut resumed = false;
    let mut run_state = if let Some(journal) = &journal {
        match journal.load_state()? {
            Some(previous)
                if previous
                    .metadata
                    .network
                    .eq_ignore_ascii_case(&metadata.network)
                    && previous.metadata.subaccount == metadata.subaccount
                    && previous
                        .metadata
                        .market
                        .eq_ignore_ascii_case(&metadata.market)
                    && previous.metadata.product == metadata.product
                    && previous.metadata.config_hash == metadata.config_hash =>
            {
                resumed = true;
                previous
            }
            _ => journal::RunState::new(metadata.clone()),
        }
    } else {
        journal::RunState::new(metadata.clone())
    };
    if let Some(journal) = &journal {
        journal.append(&journal::JournalEvent::RunStart(metadata))?;
        journal.save_state(&run_state)?;
    }
    if resumed {
        println!(
            "Recovered durable Spot grid state for run {run_id}; reconciling exchange state before replacement."
        );
    }
    println!(
        "{}",
        if execute {
            format!(
                "Live grid execution, run {run_id}. Full ladder replacement is guarded by reconciliation."
            )
        } else {
            format!("Grid monitor, run {run_id}. Pass -e to submit bulk orders.")
        }
    );
    if config.product == Product::Spot {
        println!("Spot: only PFS balances will be used. No automatic Cross→PFS transfer.");
    }

    // Spot base inventory is acquired at most ONCE per process. Acquiring the inventory a
    // two-sided grid needs is a capital-allocation decision; topping it up again after a sell
    // fill is not. The grid's profit comes from selling high and letting the *bid* side buy back
    // lower, so re-buying at the ask after every fill would return the captured spread, plus
    // taker fees, to the market on every round trip.
    // Parse the optional Spot stop-loss once: a malformed value must fail at startup, not on the
    // cycle where the market happens to reach it.
    let spot_exit_price = match settings.spot_exit_price.as_deref().map(str::trim) {
        Some(raw) if !raw.is_empty() => {
            Some(decimal(raw).with_context(|| format!("invalid SPOT_EXIT_PRICE {raw:?}"))?)
        }
        _ => None,
    };
    if let Some(stop) = spot_exit_price {
        println!("Spot stop-loss armed: liquidate and stop when price <= {stop}.");
    }
    // Set once the stop-loss has liquidated, so the shutdown exit policy does not sell again.
    let mut stop_loss_liquidated = false;
    // A range breaker cancels the ladder but must never flow into the configured stop-time asset
    // disposition; an abnormal market state requires an explicit later operator decision.
    let mut paused_by_breakout = false;
    // A submission-failure circuit breaker also cancels and pauses without liquidating.
    let mut paused_by_failure_circuit = false;
    // Local PFS preflight rejections and submitted-chain failures are intentionally independent.
    let mut consecutive_local_preflight_rejections = 0usize;
    let mut consecutive_bulk_failures = 0usize;
    // Bootstrap eligibility is always derived from the newest PFS snapshot. Never cache a
    // historical "funded" success: fills, withdrawals, and a changed pinned plan can all make
    // an old success record unsafe.
    let mut consecutive_spot_funding_failures: usize = 0;
    let mut paused_by_spot_funding_circuit = false;
    // Spot grid geometry is initialized once per process and then pinned. A moving mid may
    // detect fills and trigger replacement, but it must never move the configured boundaries or
    // regenerate prices; otherwise the strategy becomes a moving target instead of a grid.
    let mut pinned_spot_plan: Option<GridPlan> = run_state
        .spot_runtime
        .as_ref()
        .map(|state| state.pinned_plan.clone());
    // A small mid-price move can change many quantized levels and otherwise cause a full bulk
    // replacement every refresh. Keep a short cooldown for same-sized ladders; initial placement
    // and structural changes (level count changes, fills, funding/affordability changes) bypass it.
    const BULK_REPLACEMENT_COOLDOWN: Duration = Duration::from_secs(30);
    let mut last_bulk_replacement_at: Option<tokio::time::Instant> = None;
    // Total resting levels (bid_count + ask_count) from the last submitted bulk ladder. A
    // changed level count means inventory/affordability moved (a fill, funding, or a PFS-driven
    // shrink), which must replace immediately regardless of the cooldown.
    let mut last_submitted_level_count: Option<usize> = None;
    // Trade history is the reliable fill signal. Bulk synthetic order IDs change on every
    // replacement because the sequence number changes, so comparing those IDs would falsely
    // classify every replacement as a fill.
    let mut last_seen_trade_ms: Option<i64> = run_state
        .spot_runtime
        .as_ref()
        .and_then(|state| state.last_seen_trade_ms);
    // Retained so the shutdown path can act on the market without re-fetching after Ctrl+C.
    let mut last_market: Option<Market> = None;
    // A shared cancellation token so every long `.await` in the loop body can bail promptly
    // on Ctrl+C rather than only checking between sleep-drain cycles.
    let cancel = engine_runtime
        .as_ref()
        .map(|runtime| runtime.cancel())
        .unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
    {
        let cancel = Arc::clone(&cancel);
        tokio::spawn(async move {
            #[cfg(unix)]
            {
                use tokio::signal::unix::{SignalKind, signal};
                let mut terminate =
                    signal(SignalKind::terminate()).expect("install SIGTERM handler");
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = terminate.recv() => {}
                }
            }
            #[cfg(not(unix))]
            tokio::signal::ctrl_c().await.ok();
            cancel.store(true, Ordering::Relaxed);
            println!("\nShutdown requested.");
        });
    }
    let (spot_event_tx, mut spot_event_rx) = mpsc::channel(256);
    let _spot_event_listener =
        if execute && config.product == Product::Spot && !settings.subaccount.trim().is_empty() {
            let market = api
                .market(&config.market_name, Product::Spot)
                .await
                .context("resolve Spot market before subscribing to lifecycle events")?;
            Some(events::spawn_spot_event_listener(
                api.clone(),
                market.address,
                settings.subaccount.clone(),
                config.spot.ws_reconnect_backoff.clone(),
                spot_event_tx,
                Arc::clone(&cancel),
            ))
        } else {
            None
        };
    macro_rules! check_cancel {
        () => {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
        };
    }

    loop {
        let cycle_start = tokio::time::Instant::now();
        let snapshot = match fetch_snapshot(&api, &config, optional_subaccount(&settings)).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("grid refresh failed: {e:#}");
                check_cancel!();
                tokio::time::sleep(config.refresh).await;
                continue;
            }
        };
        check_cancel!();
        if let Some(runtime) = &engine_runtime {
            let mid = snapshot.plan.mid.to_string();
            let ladder = snapshot
                .plan
                .all_levels()
                .map(|level| decibel_grid_tui::control::LadderLevel {
                    side: format!("{:?}", level.side),
                    price: level.price.to_string(),
                    size: level.size.to_string(),
                    state: format!("{:?}", level.state),
                })
                .collect();
            let funds = snapshot.account.spot_funds.as_ref().map(|funds| {
                (
                    funds.base_symbol.clone(),
                    funds.base_balance.to_string(),
                    funds.quote_symbol.clone(),
                    funds.quote_balance.to_string(),
                )
            });
            runtime
                .update_status(|status| {
                    status.phase = "running".to_owned();
                    status.last_cycle_at = Some(Utc::now());
                    status.mid = Some(mid);
                    status.last_error = None;
                    status.ladder = ladder;
                    if let Some((base_symbol, base_balance, quote_symbol, quote_balance)) = funds {
                        status.pfs_base_symbol = Some(base_symbol);
                        status.pfs_base_balance = Some(base_balance);
                        status.pfs_quote_symbol = Some(quote_symbol);
                        status.pfs_quote_balance = Some(quote_balance);
                    }
                })
                .await;
        }
        // Rebuild the plan without trade-history markers. Historical fills are a UI hint and
        // must not suppress a future desired order during reconciliation or execution.
        let mut snapshot = snapshot;
        last_market = Some(snapshot.market.clone());
        // Trade history is returned newest-first. A newly observed trade is a strong trigger for
        // replacement; a small price-only drift is not. Seed the cursor on the first cycle so
        // historical fills from before this process started do not cause an immediate refresh.
        let latest_trade_ms = snapshot.trades.iter().map(|trade| trade.timestamp_ms).max();
        let new_trade_observed = match (last_seen_trade_ms, latest_trade_ms) {
            (Some(previous), Some(latest)) => latest > previous,
            (None, Some(_)) => false,
            _ => false,
        };
        if let Some(latest) = latest_trade_ms {
            last_seen_trade_ms =
                Some(last_seen_trade_ms.map_or(latest, |previous| previous.max(latest)));
        }
        if snapshot.market.product == Product::Spot {
            // Optional Spot stop-loss: unlike a normal lower-bound breach, this is terminal.
            // Cancel the bulk ladder and liquidate all available base before stopping the run.
            if execute
                && let Some(stop) = spot_exit_price
                && snapshot.plan.mid <= stop
            {
                println!(
                    "Spot stop-loss reached at {} (trigger {}); cancelling ladder and liquidating base.",
                    snapshot.plan.mid, stop
                );
                match exit_sell_assets(
                    &settings.network,
                    &settings.api_key,
                    &settings.aptos_private_key,
                    &settings.subaccount,
                    &snapshot.market,
                    Some((
                        &config.spot,
                        spot_fee_rates
                            .as_ref()
                            .expect("live Spot execution fetched fee rates"),
                    )),
                )
                .await
                {
                    Ok(hashes) => {
                        println!("Spot stop-loss liquidation completed: {:?}", hashes);
                        stop_loss_liquidated = true;
                    }
                    Err(error) => eprintln!("Spot stop-loss liquidation failed: {error:#}"),
                }
                break;
            }
            // Upgrade pre-uniform persisted state once, retaining its price geometry while
            // introducing the fixed per-grid base size used by every later replacement.
            if pinned_spot_plan
                .as_ref()
                .is_some_and(|plan| plan.per_grid_base_size.is_none())
            {
                let upgraded = pinned_spot_plan
                    .as_ref()
                    .expect("checked above")
                    .pin_spot_per_grid_base_size(&config, &snapshot.market)?;
                println!(
                    "Migrated persisted Spot grid to fixed per-grid size {}.",
                    upgraded
                        .per_grid_base_size
                        .expect("Spot migration sets per_grid_base_size")
                );
                pinned_spot_plan = Some(upgraded);
            }
            // Initialize the Spot geometry once. From the second cycle onward the plan's mid,
            // lower/upper bounds, prices, and per-level sizes are all pinned for this run.
            if pinned_spot_plan.is_none() {
                pinned_spot_plan = Some(snapshot.plan.clone());
                println!(
                    "Spot grid pinned for this run: bounds [{}, {}], mid {}, {} bid(s), {} ask(s).",
                    snapshot.plan.lower,
                    snapshot.plan.upper,
                    snapshot.plan.mid,
                    snapshot.plan.bids.len(),
                    snapshot.plan.asks.len()
                );
            }
            let mid = snapshot.plan.mid;
            let (lower, upper) = pinned_spot_plan
                .as_ref()
                .map(|plan| (plan.lower, plan.upper))
                .expect("Spot plan was pinned above");
            if mid < lower || mid > upper {
                let direction = if mid < lower { "below" } else { "above" };
                let reason =
                    format!("Spot mid {mid} broke {direction} pinned range [{lower}, {upper}]");
                match config.spot.range_breakout_action {
                    RangeBreakoutAction::PauseAndAlert => {
                        eprintln!("RANGE BREAKOUT: {reason}; pausing the grid.");
                        if let Some(journal) = &journal {
                            let event = journal::JournalEvent::RiskRejected {
                                at: Utc::now(),
                                reason: reason.clone(),
                            };
                            journal.append(&event)?;
                            run_state.apply(&event);
                            journal.save_state(&run_state)?;
                        }
                        if execute {
                            match spot_lifecycle::cancel_bulk_ladder(
                                &settings.network,
                                &settings.aptos_private_key,
                                &settings.subaccount,
                                &snapshot.market,
                            )
                            .await
                            {
                                Ok(hash) => println!(
                                    "Range-breakout ladder cancellation submitted in tx {hash}"
                                ),
                                Err(error) => {
                                    eprintln!("Range-breakout cancellation failed: {error:#}")
                                }
                            }
                            paused_by_breakout = true;
                            break;
                        }
                    }
                    RangeBreakoutAction::ExtendGrid => {
                        let shifted_range = match config.range.clone() {
                            RangeSpec::Bounds { lower, upper } => {
                                let delta = if mid < lower {
                                    mid - lower
                                } else {
                                    mid - upper
                                };
                                RangeSpec::Bounds {
                                    lower: lower + delta,
                                    upper: upper + delta,
                                }
                            }
                            // Percent and geometric-step ranges are naturally rebuilt around the
                            // newest mid; retain their configured spacing parameters.
                            other => other,
                        };
                        config.range = shifted_range;
                        let fixed_size = pinned_spot_plan
                            .as_ref()
                            .and_then(|plan| plan.per_grid_base_size);
                        let shifted = build_plan_with_per_grid_base_size(
                            &config,
                            &snapshot.market,
                            mid,
                            fixed_size,
                        )?;
                        println!(
                            "RANGE BREAKOUT: {reason}; extended grid to [{}, {}]",
                            shifted.lower, shifted.upper
                        );
                        pinned_spot_plan = Some(shifted);
                    }
                }
            }
            // The pinned ladder supplies the prices and per-level sizes; only the bid/ask split
            // follows the latest price, which is what produces sell-high/buy-low rotation after
            // a fill without ever moving the grid itself.
            let pinned = pinned_spot_plan
                .as_ref()
                .expect("Spot plan was pinned above");
            snapshot.plan = pinned.project_spot(mid, snapshot.market.tick_size)?;
            if let Err(error) = snapshot.plan.enforce_spot_budget(&config) {
                let reason = format!(
                    "fixed Spot per-grid size no longer fits the configured budget after re-centering: {error:#}"
                );
                eprintln!("RISK REJECTED: {reason}");
                if let Some(journal) = &journal {
                    let event = journal::JournalEvent::RiskRejected {
                        at: Utc::now(),
                        reason,
                    };
                    journal.append(&event)?;
                    run_state.apply(&event);
                    journal.save_state(&run_state)?;
                }
                check_cancel!();
                tokio::time::sleep(config.refresh).await;
                continue;
            }
        } else {
            snapshot.plan = build_plan(&config, &snapshot.market, snapshot.plan.mid)?;
        }

        // Read the resting orders BEFORE funding or fitting a Spot plan. The executable Spot
        // plan is pinned; it must not be shrunk to today's free balance, because insufficient
        // inventory is a precondition to resolve before the ladder is submitted.
        // ask side to whatever base is already held, which makes the plan match the chain
        // exactly (`0 missing`) and hides the very shortfall that funding is supposed to close.
        let mut actual_for_execution = None;
        if execute {
            let actual = match api
                .open_orders(&settings.subaccount, &snapshot.market)
                .await
            {
                Ok(orders) => orders,
                Err(e) => {
                    eprintln!("reconciliation failed (open_orders): {e:#}; skipping cycle");
                    check_cancel!();
                    tokio::time::sleep(config.refresh).await;
                    continue;
                }
            };
            check_cancel!();
            // Every bootstrap decision comes from the current PFS snapshot. This same shortfall
            // helper is used at startup, after fills/re-centering, and before bulk submission.
            if config.spot.auto_convert_missing_base
                && !paused_by_spot_funding_circuit
                && snapshot.market.product == Product::Spot
                && let Some(funds) = &snapshot.account.spot_funds
                && let Some(base_gap) =
                    decibel_grid_tui::spot_base_shortfall(&snapshot.plan, funds, &snapshot.market)
                && decibel_grid_tui::reconcile::blocking_orders(&actual).is_empty()
            {
                let quote_spare = (funds.available_quote_for_bulk() - snapshot.plan.quote_required)
                    .max(Decimal::ZERO);
                let funding_result = spot_taker::execute_guarded_spot_ioc(
                    &settings.network,
                    &api,
                    &settings.aptos_private_key,
                    &settings.subaccount,
                    &snapshot.market,
                    spot_taker::TakerSide::Buy,
                    base_gap,
                    Some(quote_spare),
                    spot_fee_rates
                        .as_ref()
                        .expect("live Spot execution fetched fee rates"),
                    &config.spot,
                )
                .await;
                check_cancel!();
                match &funding_result {
                    Ok(funding) if funding.filled_total > Decimal::ZERO => {
                        println!(
                            "Spot base funding: filled {} of {} across {} IOC attempt(s).",
                            funding.filled_total, base_gap, funding.attempts
                        );
                    }
                    Ok(_) => {}
                    Err(error) => eprintln!("Spot base funding skipped: {error:#}"),
                }
                // Re-read balances after the IOC attempt — refinancing the plan with up-to-date
                // PFS balances is better than using stale pre-funding data.
                match api
                    .account(Some(&settings.subaccount), &snapshot.market)
                    .await
                {
                    Ok(account) => {
                        snapshot.account = account;
                        let accepted_partial = funding_result.as_ref().is_ok_and(|funding| {
                            base_gap > Decimal::ZERO
                                && funding.filled_total / base_gap
                                    >= config.spot.entry_min_fill_ratio
                        });
                        let available_base = snapshot
                            .account
                            .spot_funds
                            .as_ref()
                            .expect("Spot funds refreshed")
                            .available_base_for_bulk();
                        let needed_base = snapshot.plan.base_required;
                        if available_base >= needed_base {
                            consecutive_spot_funding_failures = 0;
                            println!(
                                "Spot base funding target reached; proceeding to the full pinned grid."
                            );
                        } else if accepted_partial {
                            let reduced = snapshot
                                .plan
                                .reduce_asks_to_available_base(available_base)?;
                            let dropped = snapshot.plan.asks.len() - reduced.asks.len();
                            let received_ratio = if needed_base > Decimal::ZERO {
                                available_base / needed_base
                            } else {
                                Decimal::ONE
                            };
                            if reduced.asks.is_empty() {
                                eprintln!(
                                    "Spot entry received {:.2}% of required base but cannot fund even one ask level; startup remains failed.",
                                    received_ratio * Decimal::from(100)
                                );
                            } else {
                                println!(
                                    "Spot entry received {:.2}% of required base ({} available vs {} needed); reduced asks from {} to {} and dropped {} unfundable level(s).",
                                    received_ratio * Decimal::from(100),
                                    available_base,
                                    needed_base,
                                    snapshot.plan.asks.len(),
                                    reduced.asks.len(),
                                    dropped
                                );
                                pinned_spot_plan = Some(reduced.clone());
                                snapshot.plan = reduced
                                    .project_spot(snapshot.plan.mid, snapshot.market.tick_size)?;
                                consecutive_spot_funding_failures = 0;
                            }
                        } else {
                            consecutive_spot_funding_failures += 1;
                            if consecutive_spot_funding_failures
                                >= config.spot.entry_exit_max_attempts
                            {
                                paused_by_spot_funding_circuit = true;
                                let reason = format!(
                                    "Spot entry funding failed to reach the {:.2}% minimum fill ratio {} consecutive time(s); paused pending manual intervention",
                                    config.spot.entry_min_fill_ratio * Decimal::from(100),
                                    consecutive_spot_funding_failures
                                );
                                eprintln!("RISK REJECTED: {reason}");
                                if let Some(journal) = &journal {
                                    let event = journal::JournalEvent::RiskRejected {
                                        at: Utc::now(),
                                        reason,
                                    };
                                    journal.append(&event)?;
                                    run_state.apply(&event);
                                    journal.save_state(&run_state)?;
                                }
                            }
                        }
                    }
                    Err(error) => eprintln!("  balance refresh after funding failed: {error:#}"),
                }
            }
            // Do not sell base to manufacture quote during startup. The initial rebalance is
            // one-way by design: preserve the bid reserve and buy only the missing ask inventory.
            check_cancel!();
            actual_for_execution = Some(actual);
        }

        // This runs even when the remaining gap is smaller than one legal IOC. Such a gap is not
        // sufficient to claim the full ladder is funded, but it can fund fewer whole ask levels.
        if execute
            && snapshot.market.product == Product::Spot
            && actual_for_execution.as_ref().is_some_and(|orders| {
                decibel_grid_tui::reconcile::blocking_orders(orders).is_empty()
            })
            && let Some(funds) = &snapshot.account.spot_funds
            && funds.available_base_for_bulk() < snapshot.plan.base_required
        {
            let reduced = snapshot
                .plan
                .reduce_asks_to_available_base(funds.available_base_for_bulk())?;
            if !reduced.asks.is_empty() && reduced.asks.len() < snapshot.plan.asks.len() {
                let dropped = snapshot.plan.asks.len() - reduced.asks.len();
                let ratio = funds.available_base_for_bulk() / snapshot.plan.base_required;
                println!(
                    "Spot base is {:.2}% funded ({} available vs {} needed); reduced asks from {} to {} and dropped {} unfundable level(s).",
                    ratio * Decimal::from(100),
                    funds.available_base_for_bulk(),
                    snapshot.plan.base_required,
                    snapshot.plan.asks.len(),
                    reduced.asks.len(),
                    dropped
                );
                pinned_spot_plan = Some(reduced.clone());
                snapshot.plan =
                    reduced.project_spot(snapshot.plan.mid, snapshot.market.tick_size)?;
            } else if reduced.asks.is_empty() {
                eprintln!("Spot base cannot fund even one ask level; full ladder remains blocked.");
            }
        }

        if let Some(adjustment) = fit_spot_snapshot_to_pfs(&mut snapshot)? {
            println!("Spot funding check: {adjustment}");
        }
        print_snapshot(&snapshot, &config);
        if config.product == Product::Spot
            && let Some(pinned_plan) = &pinned_spot_plan
        {
            run_state.spot_runtime = Some(journal::SpotRuntimeState {
                pinned_plan: pinned_plan.clone(),
                last_seen_trade_ms,
            });
        }
        if let Some(journal) = &journal {
            let event = journal::JournalEvent::PlanGenerated {
                at: Utc::now(),
                mid: snapshot.plan.mid.normalize().to_string(),
                bid_levels: snapshot.plan.bids.len(),
                ask_levels: snapshot.plan.asks.len(),
                quote_required: snapshot.plan.quote_required.normalize().to_string(),
                base_required: snapshot.plan.base_required.normalize().to_string(),
            };
            journal.append(&event)?;
            run_state.apply(&event);
            journal.save_state(&run_state)?;
        }

        if execute {
            // 1. Reconcile using the order snapshot fetched before Spot funding/fitting.
            let actual = actual_for_execution
                .ok_or_else(|| anyhow::anyhow!("execution order snapshot was not available"))?;
            let desired = decibel_grid_tui::reconcile::desired_orders(
                &snapshot.plan,
                snapshot.market.tick_size,
                snapshot.market.lot_size,
            );
            let reconcile_result = decibel_grid_tui::reconcile::reconcile(
                &desired,
                &actual,
                snapshot.market.tick_size,
                snapshot.market.lot_size,
            );

            println!("RECONCILE CYCLE — {}", reconcile_result.summary());
            if let Some(runtime) = &engine_runtime {
                let matched = reconcile_result.matched.len();
                let missing = reconcile_result.missing.len();
                let unmanaged = reconcile_result.unmanaged.len();
                runtime
                    .update_status(|status| {
                        status.matched = Some(matched);
                        status.missing = Some(missing);
                        status.unmanaged = Some(unmanaged);
                        status.events.push(decibel_grid_tui::control::EngineEvent {
                            at: Utc::now(),
                            message: format!("reconcile: {matched} matched, {missing} missing, {unmanaged} unmanaged"),
                        });
                        if status.events.len() > 200 { status.events.drain(..status.events.len() - 200); }
                    })
                    .await;
            }
            if let Some(journal) = &journal {
                let event = journal::JournalEvent::ReconciliationResult {
                    at: Utc::now(),
                    matched: reconcile_result.matched.len(),
                    missing: reconcile_result.missing.len(),
                    unmanaged: reconcile_result.unmanaged.clone(),
                    is_converged: reconcile_result.is_converged(),
                };
                journal.append(&event)?;
                run_state.apply(&event);
                journal.save_state(&run_state)?;
            }

            // 2. Standalone orders carry no client-order ID, so ownership cannot be proven and a
            // bulk submission could silently remove a manual order — those still halt execution.
            // Levels of this (subaccount, market)'s own bulk ladder are different: only one bulk
            // ladder can exist per pair and a new submission replaces it atomically by design, so
            // they must not block the very replacement that supersedes them.
            let blocking = decibel_grid_tui::reconcile::blocking_orders(&actual);
            if !blocking.is_empty() {
                let reason = format!(
                    "{} standalone open order(s) of unprovable ownership; live replacement halted until operator review",
                    blocking.len()
                );
                println!("  {reason}");
                if let Some(journal) = &journal {
                    let event = journal::JournalEvent::RiskRejected {
                        at: Utc::now(),
                        reason,
                    };
                    journal.append(&event)?;
                    run_state.apply(&event);
                    journal.save_state(&run_state)?;
                }
            } else if !reconcile_result.missing.is_empty() {
                // 3. Submit the FULL desired plan. The Decibel bulk ABI atomically replaces the
                // entire order ladder for this (subaccount, market) pair — it does not merge.

                // The Spot plan is pinned for this run. Rebalancing (IOC) already ran earlier in
                // the cycle when inventory was short, so this is the post-rebalance state.
                let exec_plan = snapshot.plan.clone();

                // Spot bulk orders source PFS only. On replacement, the existing bulk escrow is
                // credited by the Move entry function, so the currently reserved side counts
                // toward what the replacement can fund.
                let mut spot_underfunded = None;
                if snapshot.market.product == Product::Spot
                    && let Some(funds) = &snapshot.account.spot_funds
                    && (funds.available_quote_for_bulk() < exec_plan.quote_required
                        || decibel_grid_tui::spot_base_shortfall(
                            &exec_plan,
                            funds,
                            &snapshot.market,
                        )
                        .is_some())
                {
                    let account = api
                        .account(Some(&settings.subaccount), &snapshot.market)
                        .await
                        .ok();
                    let fresh_funds = account
                        .and_then(|a| a.spot_funds)
                        .unwrap_or_else(|| funds.clone());
                    if fresh_funds.available_quote_for_bulk() < exec_plan.quote_required
                        || decibel_grid_tui::spot_base_shortfall(
                            &exec_plan,
                            &fresh_funds,
                            &snapshot.market,
                        )
                        .is_some()
                    {
                        // Never shrink a pinned ladder to fit: that silently converts the
                        // configured grid into a different, narrower one. Skip the submission
                        // and report exactly which asset must be topped up.
                        spot_underfunded = Some(format!(
                            "quote needs {} (available {}), base needs {} (available {})",
                            exec_plan.quote_required,
                            fresh_funds.available_quote_for_bulk(),
                            exec_plan.base_required,
                            fresh_funds.available_base_for_bulk()
                        ));
                    }
                }

                if let Some(shortfall) = spot_underfunded {
                    consecutive_local_preflight_rejections =
                        consecutive_local_preflight_rejections.saturating_add(1);
                    eprintln!(
                        "  LOCAL PFS PRECHECK REJECTED ({}/{}): {shortfall}",
                        consecutive_local_preflight_rejections, config.spot.entry_exit_max_attempts
                    );
                    if consecutive_local_preflight_rejections >= config.spot.entry_exit_max_attempts
                    {
                        let reason = format!(
                            "{} consecutive local PFS precheck rejections; inspect funding and bootstrap logic",
                            consecutive_local_preflight_rejections
                        );
                        eprintln!(
                            "LOCAL PRECHECK CIRCUIT BREAKER: {reason}; pausing automatic replacement."
                        );
                        paused_by_failure_circuit = true;
                        if let Some(journal) = &journal {
                            let event = journal::JournalEvent::RiskRejected {
                                at: Utc::now(),
                                reason,
                            };
                            journal.append(&event)?;
                            run_state.apply(&event);
                            journal.save_state(&run_state)?;
                        }
                    }
                    if let Some(journal) = &journal {
                        let event = journal::JournalEvent::RiskRejected {
                            at: Utc::now(),
                            reason: format!("pinned Spot grid underfunded: {shortfall}"),
                        };
                        journal.append(&event)?;
                        run_state.apply(&event);
                        journal.save_state(&run_state)?;
                    }
                } else {
                    consecutive_local_preflight_rejections = 0;
                    if exec_plan.bids.is_empty() && exec_plan.asks.is_empty() {
                        println!("  No levels can be placed (budget exhausted).");
                    } else {
                        let desired_level_count = exec_plan.bids.len() + exec_plan.asks.len();
                        let structural_change = new_trade_observed
                            || last_submitted_level_count
                                .is_none_or(|previous| previous != desired_level_count);
                        let cooldown_active = last_bulk_replacement_at.is_some_and(|submitted| {
                            submitted.elapsed() < BULK_REPLACEMENT_COOLDOWN
                        });
                        if cooldown_active && !structural_change {
                            println!(
                                "  bulk replacement skipped: minor ladder drift during {}s cooldown ({} desired levels; no level-count change)",
                                BULK_REPLACEMENT_COOLDOWN.as_secs(),
                                desired_level_count
                            );
                        } else {
                            match execute_bulk_grid(
                                &settings.network,
                                &settings.api_key,
                                &settings.aptos_private_key,
                                &settings.subaccount,
                                &snapshot.market,
                                &exec_plan,
                            )
                            .await
                            {
                                Ok(execution) => {
                                    consecutive_bulk_failures = 0;
                                    last_bulk_replacement_at = Some(tokio::time::Instant::now());
                                    last_submitted_level_count =
                                        Some(execution.bid_count + execution.ask_count);
                                    println!(
                                        "  FULL ladder replaced: {} bid(s), {} ask(s) in tx {}",
                                        execution.bid_count,
                                        execution.ask_count,
                                        execution.transaction_hash
                                    );
                                    if let Some(journal) = &journal {
                                        let event = journal::JournalEvent::BulkOrderSubmitted {
                                            at: Utc::now(),
                                            transaction_hash: execution.transaction_hash,
                                            bid_count: execution.bid_count,
                                            ask_count: execution.ask_count,
                                        };
                                        journal.append(&event)?;
                                        run_state.apply(&event);
                                        journal.save_state(&run_state)?;
                                    }
                                }
                                Err(error) => {
                                    consecutive_bulk_failures =
                                        consecutive_bulk_failures.saturating_add(1);
                                    eprintln!(
                                        "  bulk order failed ({}/{}): {error:#}",
                                        consecutive_bulk_failures,
                                        config.spot.max_consecutive_bulk_failures
                                    );
                                    if let Some(journal) = &journal {
                                        let event = journal::JournalEvent::BulkOrderFailed {
                                            at: Utc::now(),
                                            error: format!("{error:#}"),
                                        };
                                        journal.append(&event)?;
                                        run_state.apply(&event);
                                        journal.save_state(&run_state)?;
                                    }
                                    if consecutive_bulk_failures
                                        >= config.spot.max_consecutive_bulk_failures
                                    {
                                        let reason = format!(
                                            "{} consecutive bulk replacement failures",
                                            consecutive_bulk_failures
                                        );
                                        eprintln!(
                                            "FAILURE CIRCUIT BREAKER: {reason}; cancelling ladder and pausing."
                                        );
                                        if let Some(journal) = &journal {
                                            let event = journal::JournalEvent::RiskRejected {
                                                at: Utc::now(),
                                                reason,
                                            };
                                            journal.append(&event)?;
                                            run_state.apply(&event);
                                            journal.save_state(&run_state)?;
                                        }
                                        match spot_lifecycle::cancel_bulk_ladder(
                                            &settings.network,
                                            &settings.aptos_private_key,
                                            &settings.subaccount,
                                            &snapshot.market,
                                        )
                                        .await
                                        {
                                            Ok(hash) => println!(
                                                "Failure-circuit cancellation submitted in tx {hash}"
                                            ),
                                            Err(cancel_error) => eprintln!(
                                                "Failure-circuit cancellation failed: {cancel_error:#}"
                                            ),
                                        }
                                        paused_by_failure_circuit = true;
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        let elapsed = cycle_start.elapsed();
        let interval = if config.product == Product::Spot {
            config.spot.reconciliation_interval
        } else {
            config.refresh
        };
        let wait = interval.saturating_sub(elapsed);
        if execute && config.product == Product::Spot {
            let sleep = tokio::time::sleep(wait);
            tokio::pin!(sleep);
            loop {
                tokio::select! {
                    _ = &mut sleep => break,
                    event = spot_event_rx.recv() => match event {
                        Some(events::SpotEvent::BulkFill(fill)) => {
                            println!("Spot bulk fill {} {} at {}; reconciling immediately.", fill.size, fill.market_addr, fill.price);
                            if let Some(runtime) = &engine_runtime {
                                let message = format!("fill: {} {} at {}", fill.size, fill.market_addr, fill.price);
                                runtime.update_status(|status| {
                                    status.events.push(decibel_grid_tui::control::EngineEvent { at: Utc::now(), message });
                                    if status.events.len() > 200 { status.events.drain(..status.events.len() - 200); }
                                }).await;
                            }
                            if let Some(journal) = &journal {
                                let event = journal::JournalEvent::SpotFill {
                                    at: Utc::now(),
                                    market: fill.market_addr,
                                    price: fill.price.normalize().to_string(),
                                    size: fill.size.normalize().to_string(),
                                    side: fill.side,
                                    event_uid: fill.event_uid,
                                };
                                journal.append(&event)?;
                                run_state.apply(&event);
                                journal.save_state(&run_state)?;
                            }
                            break;
                        }
                        Some(events::SpotEvent::BulkOrderRejected(rejected)) => {
                            eprintln!("Spot bulk order rejected for {}: {}; reconciling immediately.", rejected.market_addr, rejected.reason);
                            if let Some(runtime) = &engine_runtime {
                                let message = format!("bulk rejected: {}", rejected.reason);
                                runtime.update_status(|status| {
                                    status.events.push(decibel_grid_tui::control::EngineEvent { at: Utc::now(), message });
                                    if status.events.len() > 200 { status.events.drain(..status.events.len() - 200); }
                                }).await;
                            }
                            if let Some(journal) = &journal {
                                let event = journal::JournalEvent::RiskRejected {
                                    at: Utc::now(),
                                    reason: format!("bulk order rejected for {}: {}", rejected.market_addr, rejected.reason),
                                };
                                journal.append(&event)?;
                                run_state.apply(&event);
                                journal.save_state(&run_state)?;
                            }
                            break;
                        }
                        Some(events::SpotEvent::Reconnected(reconnected)) => {
                            println!("Spot event connection re-established ({}); reconciling REST state.", reconnected.reconnect_count);
                            break;
                        }
                        Some(events::SpotEvent::Mid(_) | events::SpotEvent::Depth(_)) => {
                            // Depth/mid updates refresh the local feed but only fills, rejects,
                            // reconnects, or the periodic timer may trigger a ladder replacement.
                        }
                        None => break,
                    }
                }
            }
        } else {
            tokio::time::sleep(wait).await;
        }
        check_cancel!();
    }

    let exit_policy = engine_runtime
        .as_ref()
        .and_then(|runtime| runtime.requested_exit_mode())
        .map(|mode| match mode {
            control::ExitMode::Hold => ExitAssetPolicy::Retain,
            control::ExitMode::Liquidate => ExitAssetPolicy::Sell,
        })
        .unwrap_or(settings.exit_asset_policy);
    if paused_by_breakout || paused_by_failure_circuit {
        println!(
            "Risk pause complete: ladder cancellation was attempted; assets were not liquidated."
        );
    } else if stop_loss_liquidated {
        println!("Stop-loss already liquidated this market; skipping the exit sell policy.");
    } else if execute {
        if let Some(market) = last_market {
            match exit_policy {
                ExitAssetPolicy::Sell => {
                    println!(
                        "Exit policy is SELL: cancelling the ladder and liquidating assets..."
                    );
                    match exit_sell_assets(
                        &settings.network,
                        &settings.api_key,
                        &settings.aptos_private_key,
                        &settings.subaccount,
                        &market,
                        (market.product == Product::Spot).then(|| {
                            (
                                &config.spot,
                                spot_fee_rates
                                    .as_ref()
                                    .expect("live Spot execution fetched fee rates"),
                            )
                        }),
                    )
                    .await
                    {
                        Ok(hashes) => println!(
                            "Exit cleanup completed: {} transaction(s): {:?}",
                            hashes.len(),
                            hashes
                        ),
                        Err(error) => eprintln!("Exit cleanup failed: {error:#}"),
                    }
                }
                ExitAssetPolicy::Retain => {
                    println!(
                        "Exit policy is RETAIN: cancelling the ladder and retaining released assets."
                    );
                    match spot_lifecycle::cancel_bulk_ladder(
                        &settings.network,
                        &settings.aptos_private_key,
                        &settings.subaccount,
                        &market,
                    )
                    .await
                    {
                        Ok(hash) => {
                            println!("Bulk ladder cancelled in tx {hash}; assets retained.")
                        }
                        Err(error) => eprintln!(
                            "Bulk cancellation failed; ladder may still be live: {error:#}"
                        ),
                    }
                }
            }
        } else {
            println!("No market snapshot was loaded; no ladder lifecycle action was sent.");
        }
    }
    Ok(())
}

fn optional_subaccount(settings: &Settings) -> Option<&str> {
    (!settings.subaccount.trim().is_empty()).then_some(settings.subaccount.as_str())
}

async fn run_tui(settings: Settings, profile_name: String, initial_tab: usize) -> Result<()> {
    // When a subaccount is configured, hold the same process lock as CLI execution for the
    // lifetime of the TUI. This prevents TUI `E`/funding actions from racing a CLI instance.
    let _subaccount_lock = if settings.subaccount.trim().is_empty() {
        None
    } else {
        Some(SubaccountRunLock::acquire(
            &settings.network,
            &settings.subaccount,
        )?)
    };
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    let result = tui_loop(&mut terminal, settings, profile_name, initial_tab).await;
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;
    result
}

async fn tui_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    settings: Settings,
    profile_name: String,
    initial_tab: usize,
) -> Result<()> {
    let profile_name = if profile_name.trim().is_empty() {
        DEFAULT_PROFILE.to_owned()
    } else {
        profile_name
    };
    let mut app = App::new(initial_tab, settings, profile_name);
    // Reuse the saved profile so the user does not re-enter settings on every run. CLI flags and
    // .env still win: they were already applied to `settings`, and only stored values that the
    // user actually saved are layered underneath.
    if let Ok(store) = ProfileStore::load()
        && let Some(data) = store.get(&app.profile_name).cloned()
    {
        app.settings.apply_profile(&data);
        app.settings_revision += 1;
        // The API key is encrypted, so it needs the password before it can be used.
        if (data.encrypted_api_key.is_some() && app.settings.api_key.trim().is_empty())
            || (data.encrypted_aptos_private_key.is_some()
                && app.settings.aptos_private_key.trim().is_empty())
        {
            app.password_purpose = Some(PasswordPurpose::LoadProfile);
            app.tab = TAB_CONFIG;
        }
    }
    let mut api: Option<DecibelClient> = None;
    let mut config: Option<GridConfig> = None;
    let (fetch_tx, mut fetch_rx) = mpsc::unbounded_channel::<MarketFetch>();
    let mut applied_revision = app.settings_revision;
    let mut next_refresh = tokio::time::Instant::now();
    loop {
        if applied_revision != app.settings_revision {
            api = None;
            config = None;
            applied_revision = app.settings_revision;
            app.snapshot = None;
            app.snapshot_pending = false;
            app.refresh_now = true;
        }
        // Apply completed background fetches without blocking input or rendering.
        while let Ok(result) = fetch_rx.try_recv() {
            match result {
                MarketFetch::Markets(result) => {
                    if let Some(picker) = app.market_picker.as_mut() {
                        picker.markets_pending = false;
                        match result {
                            Ok(markets) => {
                                app.markets = markets;
                                app.markets_loaded_for =
                                    Some((app.settings.network.clone(), app.settings.product));
                                picker.selected =
                                    picker.selected.min(app.markets.len().saturating_sub(1));
                                picker.detail_for = None;
                                picker.last_detail_at = None;
                                picker.detail_error = None;
                                app.error = None;
                            }
                            Err(error) => {
                                app.markets.clear();
                                app.set_error(error);
                            }
                        }
                    }
                }
                MarketFetch::Detail { address, mid, book } => {
                    if let Some(picker) = app.market_picker.as_mut()
                        && picker.detail_for.as_deref() == Some(address.as_str())
                    {
                        picker.detail_pending = false;
                        let mid_error = mid.as_ref().err().map(ToString::to_string);
                        picker.mid = mid.ok();
                        picker.book = book.ok();
                        picker.detail_error = mid_error;
                    }
                }
                MarketFetch::Snapshot {
                    settings_revision,
                    result,
                } if settings_revision == app.settings_revision => {
                    app.snapshot_pending = false;
                    app.refresh_started_at = None;
                    match *result {
                        Ok(snapshot) => {
                            let count = grid_price_count(&snapshot.plan);
                            if count > 0 {
                                app.selected_level = app.selected_level.min(count - 1);
                            }
                            let now = tokio::time::Instant::now();
                            app.price_highlights.retain(|(_, until)| *until > now);
                            let mut state_changes = 0;
                            if let Some(previous) = app.snapshot.as_ref() {
                                let changed = changed_level_prices(&previous.plan, &snapshot.plan);
                                state_changes = changed.len();
                                for price in changed {
                                    app.price_highlights
                                        .push((price, now + Duration::from_secs(3)));
                                }
                            }
                            app.grid_change_notice = Some(if state_changes > 0 {
                                format!(
                                    "{} order execution state change(s); green means filled",
                                    state_changes
                                )
                            } else {
                                "Grid refreshed: price moved/recalculated; no execution-state change".to_owned()
                            });
                            app.snapshot = Some(snapshot);
                            app.refresh_success_until =
                                Some(tokio::time::Instant::now() + Duration::from_secs(2));
                            keep_selected_price_visible(&mut app);
                            // A successful background refresh must not erase the full diagnostic
                            // from a preceding failed execution; F2 keeps that report available.
                            app.error = None;
                        }
                        Err(error) => app.set_error(error),
                    }
                }
                MarketFetch::Snapshot { .. } => {}
                MarketFetch::Execution {
                    settings_revision,
                    result,
                } if settings_revision == app.settings_revision => {
                    app.execution_pending = false;
                    match *result {
                        Ok(execution) => {
                            app.tab = TAB_MONITOR;
                            // A submitted transaction is status, not an error. Keep any previous
                            // failure report available until a new failure explicitly replaces it.
                            app.error = None;
                            app.grid_change_notice = Some(format!(
                                "Execution submitted: {} bid(s), {} ask(s), tx {}",
                                execution.bid_count,
                                execution.ask_count,
                                execution.transaction_hash
                            ));
                            app.refresh_now = true;
                        }
                        Err(error) => app.set_error(error),
                    }
                }
                MarketFetch::Execution { .. } => {}
            }
        }

        // Execute only the exact plan currently visible in Preview, after the explicit `e`
        // confirmation. The task runs off the UI thread; Monitor then observes the submitted
        // orders and never re-submits them during ordinary refreshes.
        if app.execute_requested && !app.execution_pending {
            app.execute_requested = false;
            let execution = app.snapshot.as_ref().map(|snapshot| {
                (
                    app.settings.network.clone(),
                    app.settings.api_key.clone(),
                    app.settings.aptos_private_key.clone(),
                    app.settings.subaccount.clone(),
                    snapshot.market.clone(),
                    snapshot.plan.clone(),
                    app.settings_revision,
                )
            });
            match execution {
                Some((network, api_key, private_key, subaccount, market, plan, revision))
                    if !api_key.trim().is_empty()
                        && !private_key.trim().is_empty()
                        && !subaccount.trim().is_empty() =>
                {
                    app.execution_pending = true;
                    let tx = fetch_tx.clone();
                    tokio::spawn(async move {
                        let result = execute_bulk_grid(
                            &network,
                            &api_key,
                            &private_key,
                            &subaccount,
                            &market,
                            &plan,
                        )
                        .await;
                        let _ = tx.send(MarketFetch::Execution {
                            settings_revision: revision,
                            result: Box::new(result),
                        });
                    });
                }
                Some(_) => app.set_error(
                    "Execution was not started: configure API key, Aptos private key, and subaccount address.",
                ),
                None => app.set_error("Execution was not started: load a valid Preview plan first."),
            }
        }

        // The market picker needs only an API client, not a valid grid. Network work runs in a
        // task so typing, mouse selection, and redraws remain responsive.
        if let Some(picker) = app.market_picker.as_mut()
            && app.markets_loaded_for != Some((app.settings.network.clone(), app.settings.product))
            && !picker.markets_pending
            && app.password_purpose.is_none()
        {
            match app.settings.api_client() {
                Ok(client) => {
                    picker.markets_pending = true;
                    let product = app.settings.product;
                    let tx = fetch_tx.clone();
                    tokio::spawn(async move {
                        let _ = tx.send(MarketFetch::Markets(client.markets(product).await));
                    });
                }
                Err(error) => app.set_error(error),
            }
        }
        if app.market_picker.is_some() && !app.markets.is_empty() {
            let selected = app
                .market_picker
                .as_ref()
                .and_then(|picker| app.filtered_markets().get(picker.selected).cloned());
            if let Some(market) = selected {
                let needs_detail = app.market_picker.as_ref().is_some_and(|picker| {
                    !picker.detail_pending
                        && (picker.detail_for.as_deref() != Some(&market.address)
                            || picker
                                .last_detail_at
                                .is_none_or(|last| last.elapsed() >= Duration::from_secs(2)))
                });
                if needs_detail {
                    match app.settings.api_client() {
                        Ok(client) => {
                            if let Some(picker) = app.market_picker.as_mut() {
                                picker.detail_for = Some(market.address.clone());
                                picker.last_detail_at = Some(tokio::time::Instant::now());
                                picker.detail_pending = true;
                                picker.mid = None;
                                picker.book = None;
                                picker.detail_error = None;
                            }
                            let tx = fetch_tx.clone();
                            let source = app.settings.price_source;
                            tokio::spawn(async move {
                                let (mid, book) = tokio::join!(
                                    client.mid_price(&market, source),
                                    client.order_book(&market, 8),
                                );
                                let _ = tx.send(MarketFetch::Detail {
                                    address: market.address.clone(),
                                    mid,
                                    book,
                                });
                            });
                        }
                        Err(error) => {
                            if let Some(picker) = app.market_picker.as_mut() {
                                picker.detail_pending = false;
                                picker.detail_error = Some(error.to_string());
                            }
                        }
                    }
                }
            }
        }
        // Keep a live snapshot available on Configure too, so the right-hand explanation
        // panel can show a real simulation while the user edits settings.
        if app.market_picker.is_none()
            && (app.refresh_now || tokio::time::Instant::now() >= next_refresh)
        {
            app.refresh_now = false;
            match (api.as_ref(), config.as_ref()) {
                (Some(api), Some(config)) if !app.snapshot_pending => {
                    app.snapshot_pending = true;
                    app.refresh_started_at = Some(tokio::time::Instant::now());
                    let api = api.clone();
                    let config = config.clone();
                    let subaccount = optional_subaccount(&app.settings).map(str::to_owned);
                    let revision = app.settings_revision;
                    let tx = fetch_tx.clone();
                    tokio::spawn(async move {
                        let mut result = fetch_snapshot(&api, &config, subaccount.as_deref()).await;
                        // Rebuild an executable plan and compute desired-vs-actual drift.
                        if let Ok(snapshot) = result.as_mut() {
                            let has_account = snapshot.account.spot_funds.is_some()
                                || snapshot.account.equity.is_some();
                            if has_account
                                && let Some(ref sub) = subaccount
                                && !sub.trim().is_empty()
                            {
                                // Keep the snapshot's already-generated geometry. Rebuilding from
                                // the latest mid here would move a Spot grid's bounds and could
                                // fail once the market leaves the original range. Clear the
                                // trade-history markers so reconciliation sees a placeable ladder.
                                snapshot.plan = snapshot.plan.executable();
                                let _ = fit_spot_snapshot_to_pfs(snapshot);
                                if let Ok(actual) = api.open_orders(sub, &snapshot.market).await {
                                    let desired = decibel_grid_tui::reconcile::desired_orders(
                                        &snapshot.plan,
                                        snapshot.market.tick_size,
                                        snapshot.market.lot_size,
                                    );
                                    let rec = decibel_grid_tui::reconcile::reconcile(
                                        &desired,
                                        &actual,
                                        snapshot.market.tick_size,
                                        snapshot.market.lot_size,
                                    );
                                    snapshot.reconciliation = Some(rec);
                                }
                            }
                        }
                        let _ = tx.send(MarketFetch::Snapshot {
                            settings_revision: revision,
                            result: Box::new(result),
                        });
                    });
                }
                (Some(_), Some(_)) => {}
                _ => match app.settings.to_grid_config().and_then(|new_config| {
                    app.settings
                        .api_client()
                        .map(|new_api| (new_config, new_api))
                }) {
                    Ok((new_config, new_api)) => {
                        next_refresh = tokio::time::Instant::now();
                        config = Some(new_config);
                        api = Some(new_api);
                        app.refresh_now = true;
                        continue;
                    }
                    Err(error) => {
                        app.set_error(error);
                    }
                },
            }
            // Always schedule the next attempt. Without this, a failing configuration (for
            // example a missing API key on the Configure tab) would retry every frame and
            // rebuild the HTTP client each time.
            next_refresh = tokio::time::Instant::now()
                + config
                    .as_ref()
                    .map_or(RETRY_INTERVAL, |current| current.refresh);
        }
        // The Configure form occupies the left 58% of the width (see render_config). Record it
        // so mouse clicks on the right-hand explanation panel are not read as field clicks.
        let terminal_size = terminal.size()?;
        app.form_width = terminal_size.width * 58 / 100;
        app.market_list_area =
            market_picker_list_area(Rect::new(0, 0, terminal_size.width, terminal_size.height));
        let screen = Rect::new(0, 0, terminal_size.width, terminal_size.height);
        let content = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(3),
                Constraint::Length(3),
            ])
            .split(screen)[1];
        app.grid_geometry = if app.tab != TAB_CONFIG {
            app.snapshot.as_ref().map(|snapshot| {
                price_grid_geometry(content, grid_price_count(&snapshot.plan), app.grid_scroll)
            })
        } else {
            None
        };
        terminal.draw(|frame| render(frame.area(), frame, &app, config.as_ref()))?;
        if event::poll(Duration::from_millis(120))? && handle_event(&mut app)? {
            // The TUI intentionally never mutates a live ladder during shutdown. Use the explicit
            // `stop --exit-asset-policy retain|sell` lifecycle command, which performs the
            // required reconciliation, fee lookup, cancellation, and guarded liquidation.
            if app.settings.exit_asset_policy == ExitAssetPolicy::Sell {
                println!(
                    "TUI did not liquidate assets. Run `stop --exit-asset-policy sell` explicitly."
                );
            }
            return Ok(());
        }
    }
}

/// Handles keyboard controls while the market-picker terminal is open.
/// Search is local and case-insensitive; network/API requests happen only when the picker
/// opens or the selected row changes.
fn handle_market_picker_key(app: &mut App, code: KeyCode) {
    let visible_count = app.filtered_markets().len();
    let Some(picker) = app.market_picker.as_mut() else {
        return;
    };
    match code {
        KeyCode::Esc => app.close_market_picker(),
        KeyCode::Enter => app.use_selected_market(),
        KeyCode::Up => {
            picker.selected = picker.selected.saturating_sub(1);
            picker.detail_for = None;
            picker.last_detail_at = None;
        }
        KeyCode::Down => {
            picker.selected = (picker.selected + 1).min(visible_count.saturating_sub(1));
            picker.detail_for = None;
            picker.last_detail_at = None;
        }
        KeyCode::Char('f') => {
            picker.last_detail_at = None;
        }
        KeyCode::Backspace => {
            picker.query.pop();
            picker.selected = 0;
            picker.detail_for = None;
            picker.last_detail_at = None;
        }
        KeyCode::Char(character) => {
            picker.query.push(character);
            picker.selected = 0;
            picker.detail_for = None;
            picker.last_detail_at = None;
        }
        _ => {}
    }
}

/// Computes the list rectangle inside the market-picker overlay. Used both at render time and
/// event time so mouse hit testing follows terminal resizing instead of hard-coded coordinates.
fn market_picker_list_area(area: Rect) -> Rect {
    let popup = centered_rect(94, area.height.saturating_sub(4), area);
    let inner = popup.inner(Margin {
        vertical: 1,
        horizontal: 1,
    });
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(3),
        ])
        .split(inner);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(rows[1])[0]
}

/// Mouse support for the market terminal. Clicking a row only highlights it; applying a market
/// is an explicit Enter-key action, so selection can never change the active grid by accident.
fn handle_market_picker_mouse(app: &mut App, column: u16, row: u16) {
    let list = app.market_list_area;
    if column < list.x
        || column >= list.x + list.width
        || row < list.y + 2
        || row >= list.y + list.height
    {
        return;
    }
    // First two list rows are border + table header.
    let index = usize::from(row - list.y - 2);
    let visible_count = app.filtered_markets().len();
    if index >= visible_count {
        return;
    }
    if let Some(picker) = app.market_picker.as_mut() {
        picker.selected = index;
        picker.detail_for = None;
        picker.last_detail_at = None;
    }
}

/// Returns true when the user requested exit.
fn handle_event(app: &mut App) -> Result<bool> {
    match event::read()? {
        // Windows terminals can emit both Press and Release events. State changes must happen
        // only once, on the press event; otherwise one key/button appears to activate twice.
        Event::Key(key) if key.kind != KeyEventKind::Press => {}
        // Handle this before modal/editing guards so Ctrl+C always exits immediately.
        Event::Key(key)
            if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            return Ok(true);
        }
        Event::Key(key) if app.error_dialog => match key.code {
            KeyCode::Esc | KeyCode::F(2) => app.error_dialog = false,
            // F2 already copies and saves the error automatically. Keep these as explicit
            // retries for terminals whose clipboard provider was temporarily unavailable.
            KeyCode::Char('c') | KeyCode::Char('y') => {
                if let Some(error) = app.error_report.as_deref() {
                    let _ = copy_error_to_clipboard(error);
                }
            }
            KeyCode::Char('s') => {
                if let Some(error) = app.error_report.as_deref() {
                    let _ = save_error_report(error);
                }
            }
            _ => {}
        },
        Event::Key(key) if app.market_picker.is_some() => handle_market_picker_key(app, key.code),
        Event::Key(key) if app.editing.is_some() => match key.code {
            KeyCode::Enter => {
                app.finish_edit(true);
            }
            KeyCode::Esc => app.finish_edit(false),
            KeyCode::Backspace => {
                if let Some(field) = app.editing
                    && let Some(value) = app.editable_value_mut(field)
                {
                    value.pop();
                }
            }
            KeyCode::Char(character) => {
                if let Some(field) = app.editing
                    && let Some(value) = app.editable_value_mut(field)
                {
                    value.push(character);
                }
            }
            _ => {}
        },
        Event::Key(key) if app.funding_dialog.is_some() => match key.code {
            KeyCode::Enter | KeyCode::Esc => {
                app.funding_dialog = None;
                app.funding_selection = None;
                app.funding_drag_start = None;
                app.funding_scroll = 0;
            }
            KeyCode::Char('c') | KeyCode::Char('C') => {
                if let Some(text) = app.funding_dialog.as_deref() {
                    if let Err(error) = copy_text_to_clipboard(text) {
                        app.set_error(error);
                    } else {
                        app.grid_change_notice =
                            Some("Funding instructions copied to clipboard.".to_owned());
                    }
                }
            }
            KeyCode::PageUp => {
                let (width, height) = crossterm::terminal::size()?;
                let area = Rect::new(0, 0, width, height);
                if let Some((_popup, _wrapped, visible, _max_scroll)) =
                    funding_modal_layout(app, area)
                {
                    app.funding_scroll = app.funding_scroll.saturating_sub(visible);
                }
            }
            KeyCode::PageDown => {
                let (width, height) = crossterm::terminal::size()?;
                let area = Rect::new(0, 0, width, height);
                if let Some((_popup, _wrapped, visible, max_scroll)) =
                    funding_modal_layout(app, area)
                {
                    app.funding_scroll = (app.funding_scroll + visible).min(max_scroll);
                }
            }
            _ => {}
        },
        Event::Key(key) if app.password_purpose.is_some() => match key.code {
            KeyCode::Enter => app.submit_password(),
            KeyCode::Esc => {
                app.password.clear();
                app.password_purpose = None;
            }
            KeyCode::Backspace => {
                app.password.pop();
            }
            KeyCode::Char(character) => app.password.push(character),
            _ => {}
        },
        Event::Key(key) => match key.code {
            // Ctrl+S / Ctrl+R must be checked before the plain-character arms below.
            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                app.password_purpose = Some(PasswordPurpose::SaveProfile);
                app.password.clear();
                app.error = None;
            }
            KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                match app.reset_profile() {
                    Ok(()) => app.error = Some(app.settings.tr(TKey::ProfileReset).to_owned()),
                    Err(error) => app.set_error(error),
                }
            }
            KeyCode::F(2) if app.error_report.is_some() => {
                app.error_dialog = true;
            }
            KeyCode::Char('q') => return Ok(true),
            KeyCode::Esc => {
                app.tab = TAB_CONFIG;
            }
            KeyCode::Char('1') => app.tab = TAB_CONFIG,
            KeyCode::Char('2') | KeyCode::Char('p') => {
                app.tab = TAB_PREVIEW;
                app.refresh_now = true;
            }
            KeyCode::Char('3') | KeyCode::Char('r') => {
                app.tab = TAB_MONITOR;
                app.refresh_now = true;
            }
            // `m` opens the market terminal modal rather than changing the page.
            KeyCode::Char('m') => app.open_market_picker(),
            // Spot funding setup is explicit: U opens an informational modal; it never submits
            // transactions. HOLD_AS_NON_COLLATERAL is owner-only and must be done manually.
            KeyCode::Char('u') if app.tab == TAB_PREVIEW || app.tab == TAB_MONITOR => {
                if app.settings.product == Product::Spot {
                    app.funding_dialog = build_funding_instructions(app);
                    app.funding_selection = None;
                    app.funding_drag_start = None;
                    app.funding_scroll = 0;
                    app.error = None;
                }
            }
            KeyCode::Tab | KeyCode::Right => {
                app.tab = (app.tab + 1) % TAB_COUNT;
                app.refresh_now = true;
            }
            KeyCode::BackTab | KeyCode::Left => {
                app.tab = (app.tab + TAB_COUNT - 1) % TAB_COUNT;
                app.refresh_now = true;
            }
            KeyCode::Up if app.tab == TAB_CONFIG => app.select_next_field(-1),
            KeyCode::Down if app.tab == TAB_CONFIG => app.select_next_field(1),
            KeyCode::Enter if app.tab == TAB_CONFIG && app.active_field() == Field::Market => {
                app.open_market_picker();
            }
            KeyCode::Enter if app.tab == TAB_CONFIG => {
                if app.active_field().editable() {
                    app.start_edit();
                } else {
                    app.cycle_field(1);
                }
            }
            KeyCode::Char(' ') if app.tab == TAB_CONFIG => app.cycle_field(1),
            KeyCode::Char('[') if app.tab == TAB_CONFIG => app.cycle_field(-1),
            KeyCode::Char('f') => {
                app.refresh_now = true;
                app.refresh_success_until = None;
            }
            KeyCode::Char('e') if app.tab == TAB_PREVIEW => {
                // The TUI is deliberately preview/monitor only. The resilient lifecycle (PFS
                // preflight, fee fetch, reconciliation, durable state and event listener) lives
                // in `run -e`; bypassing it here could replace an unmanaged ladder.
                app.error = Some(
                    "Live execution is available only through `run -e`; TUI remains preview/monitor only."
                        .to_owned(),
                );
            }
            KeyCode::Up => {
                app.selected_level = app.selected_level.saturating_sub(1);
                keep_selected_price_visible(app);
            }
            KeyCode::Down => {
                app.selected_level = app.selected_level.saturating_add(1);
                keep_selected_price_visible(app);
            }
            KeyCode::PageUp => {
                let count = app
                    .snapshot
                    .as_ref()
                    .map(|snapshot| grid_price_count(&snapshot.plan))
                    .unwrap_or(0);
                let page = app
                    .grid_geometry
                    .map_or(1, |geometry| geometry.columns * geometry.rows);
                app.selected_level = app.selected_level.saturating_sub(page.min(count));
                keep_selected_price_visible(app);
            }
            KeyCode::PageDown => {
                let count = app
                    .snapshot
                    .as_ref()
                    .map(|snapshot| grid_price_count(&snapshot.plan))
                    .unwrap_or(0);
                let page = app
                    .grid_geometry
                    .map_or(1, |geometry| geometry.columns * geometry.rows);
                app.selected_level = (app.selected_level + page).min(count.saturating_sub(1));
                keep_selected_price_visible(app);
            }
            _ => {}
        },
        Event::Mouse(mouse)
            if app.market_picker.is_some() && matches!(mouse.kind, MouseEventKind::Down(_)) =>
        {
            handle_market_picker_mouse(app, mouse.column, mouse.row);
        }
        Event::Mouse(mouse)
            if app.tab != TAB_CONFIG
                && matches!(
                    mouse.kind,
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                ) =>
        {
            if let Some(geometry) = app.grid_geometry {
                let step = geometry.columns;
                match mouse.kind {
                    MouseEventKind::ScrollUp => {
                        app.grid_scroll = app.grid_scroll.saturating_sub(step);
                    }
                    MouseEventKind::ScrollDown => {
                        let count = app
                            .snapshot
                            .as_ref()
                            .map(|snapshot| grid_price_count(&snapshot.plan))
                            .unwrap_or(0);
                        let capacity = geometry.columns * geometry.rows;
                        let max_scroll = count.saturating_sub(capacity);
                        app.grid_scroll = (app.grid_scroll + step)
                            .min(max_scroll / geometry.columns * geometry.columns);
                    }
                    _ => {}
                }
            }
        }
        Event::Mouse(mouse) if app.funding_dialog.is_some() => {
            let (width, height) = crossterm::terminal::size()?;
            let area = Rect::new(0, 0, width, height);
            let Some((popup, wrapped, _visible, max_scroll)) = funding_modal_layout(app, area)
            else {
                return Ok(false);
            };
            match mouse.kind {
                MouseEventKind::Down(_) => {
                    if let Some((row, col)) =
                        funding_modal_mouse_position(mouse.column, mouse.row, popup)
                    {
                        let content_row = row + app.funding_scroll;
                        if let Some(orig) = funding_original_position(&wrapped, content_row, col) {
                            app.funding_drag_start = Some(orig);
                            app.funding_selection = Some((orig, orig));
                        }
                    }
                }
                MouseEventKind::Drag(_) => {
                    if let (Some(start), Some((row, col))) = (
                        app.funding_drag_start,
                        funding_modal_mouse_position(mouse.column, mouse.row, popup),
                    ) {
                        let content_row = row + app.funding_scroll;
                        if let Some(orig) = funding_original_position(&wrapped, content_row, col) {
                            app.funding_selection = Some((start, orig));
                        }
                    }
                }
                MouseEventKind::Up(_) => {
                    if let Some(text) = selected_funding_text(app) {
                        if let Err(error) = copy_text_to_clipboard(&text) {
                            app.set_error(error);
                        } else {
                            app.grid_change_notice = Some(
                                "Selected funding instructions copied to clipboard.".to_owned(),
                            );
                        }
                    }
                    app.funding_drag_start = None;
                }
                MouseEventKind::ScrollUp => {
                    app.funding_scroll = app.funding_scroll.saturating_sub(1);
                }
                MouseEventKind::ScrollDown => {
                    app.funding_scroll = (app.funding_scroll + 1).min(max_scroll);
                }
                _ => {}
            }
        }
        Event::Mouse(mouse) if matches!(mouse.kind, MouseEventKind::Down(_)) => {
            if mouse.row == 1 {
                let tab_area = Rect::new(0, 0, crossterm::terminal::size()?.0, 3);
                if let Some(index) =
                    tab_at_position(tab_area, app.settings.language, mouse.column, mouse.row)
                {
                    app.tab = index;
                    app.refresh_now = true;
                }
            } else if app.tab == TAB_CONFIG && mouse.column < app.form_width && mouse.row >= 5 {
                let visible = app.current_visible_fields();
                let index = usize::from(mouse.row - 5);
                if index < visible.len() {
                    app.field_index = FIELDS
                        .iter()
                        .position(|field| *field == visible[index])
                        .unwrap_or(0);
                    // Mouse clicks only select a configuration row. Editing or cycling is
                    // deliberately keyboard-confirmed with Enter/Space to prevent accidental
                    // changes when a user is merely inspecting the form.
                }
            } else if app.tab == TAB_PREVIEW || app.tab == TAB_MONITOR {
                if app.settings.product == Product::Spot
                    && let Some(area) = spot_funding_line_area()
                    && mouse.column >= area.x
                    && mouse.column < area.right()
                    && mouse.row >= area.y
                    && mouse.row < area.bottom()
                {
                    // Clicking the Spot funding line opens the same info modal as the U key.
                    app.funding_dialog = build_funding_instructions(app);
                    app.funding_selection = None;
                    app.funding_drag_start = None;
                    app.funding_scroll = 0;
                    app.error = None;
                    return Ok(false);
                }
                if app.tab == TAB_PREVIEW {
                    let (width, height) = crossterm::terminal::size()?;
                    let screen = Rect::new(0, 0, width, height);
                    let content = Layout::default()
                        .direction(Direction::Vertical)
                        .constraints([
                            Constraint::Length(3),
                            Constraint::Min(3),
                            Constraint::Length(3),
                        ])
                        .split(screen)[1];
                    let button = preview_execute_button(content);
                    if mouse.column >= button.x
                        && mouse.column < button.right()
                        && mouse.row >= button.y
                        && mouse.row < button.bottom()
                    {
                        // The TUI is deliberately preview/monitor only. Live execution is routed
                        // through `run -e` so it cannot bypass reconciliation and risk guards.
                        app.error = Some(
                            "Live execution is available only through `run -e`; TUI remains preview/monitor only."
                                .to_owned(),
                        );
                    } else {
                        select_grid_cell_from_mouse(app, mouse.column, mouse.row);
                    }
                } else {
                    select_grid_cell_from_mouse(app, mouse.column, mouse.row);
                }
            }
        }
        _ => {}
    }
    Ok(false)
}

fn preview_execute_button(content: Rect) -> Rect {
    let summary = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(8),
            Constraint::Min(6),
            Constraint::Length(8),
        ])
        .split(content)[0];
    Rect::new(
        summary.right().saturating_sub(22),
        summary.bottom().saturating_sub(2),
        20.min(summary.width),
        1,
    )
}

/// Returns the screen rectangle of the Spot funding warning/config line inside the summary block.
/// This is `None` only when the terminal size cannot be read, which should not happen in the TUI.
fn spot_funding_line_area() -> Option<Rect> {
    let (width, height) = crossterm::terminal::size().ok()?;
    let screen = Rect::new(0, 0, width, height);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(3),
        ])
        .split(screen);
    let grid_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(8),
            Constraint::Min(6),
            Constraint::Length(8),
        ])
        .split(chunks[1]);
    let summary = grid_chunks[0];
    let inner = Rect::new(
        summary.x + 1,
        summary.y + 1,
        summary.width - 2,
        summary.height - 2,
    );
    // The warning/config line is the 6th line (index 5) of the summary text.
    Some(Rect::new(inner.x, inner.y + 5, inner.width, 1))
}

fn select_grid_cell_from_mouse(app: &mut App, column: u16, row: u16) {
    let price_count = app
        .snapshot
        .as_ref()
        .map(|snapshot| grid_price_count(&snapshot.plan))
        .unwrap_or(0);
    if let Some(index) = app
        .grid_geometry
        .and_then(|geometry| geometry.hit_test(column, row, price_count))
    {
        app.selected_level = index;
    }
}

/// Render transient refresh feedback or the current error as a compact header status. Keeping
/// it in the title bar avoids a modal-style status panel obscuring the grid or configuration.
fn render_refresh_indicator(area: Rect, frame: &mut ratatui::Frame, app: &App) {
    let now = tokio::time::Instant::now();
    let (label, style) = if let Some(error) = app.error.as_deref() {
        let max_message_width = usize::from(area.width.saturating_sub(20));
        let message: String = error.chars().take(max_message_width).collect();
        let suffix = if error.chars().count() > max_message_width {
            "…"
        } else {
            ""
        };
        (
            format!(
                " {}: {message}{suffix} ",
                app.settings.tr(TKey::StatusTitle)
            ),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )
    } else if app.snapshot_pending {
        let elapsed = app
            .refresh_started_at
            .map(|started| started.elapsed().as_millis() / 120)
            .unwrap_or(0);
        let spinner = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
        (
            format!(
                " {} {}",
                spinner[(elapsed as usize) % spinner.len()],
                ui(app.settings.language, "Refreshing", "正在刷新")
            ),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
    } else if app.refresh_success_until.is_some_and(|until| until > now) {
        (
            format!(" ✓ {}", ui(app.settings.language, "Refreshed", "刷新成功")),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        return;
    };
    let width = (label.chars().count() as u16)
        .saturating_add(2)
        .min(area.width.saturating_sub(2));
    let x = area.right().saturating_sub(width).saturating_sub(1);
    let indicator = Rect::new(x, area.y.saturating_add(1), width, 1);
    frame.render_widget(Paragraph::new(label).style(style), indicator);
}

fn render(area: Rect, frame: &mut ratatui::Frame, app: &App, config: Option<&GridConfig>) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(3),
        ])
        .split(area);
    render_tabs(chunks[0], frame, app);
    render_refresh_indicator(area, frame, app);
    match app.tab {
        TAB_CONFIG => render_config(chunks[1], frame, app),
        TAB_PREVIEW | TAB_MONITOR => render_grid(chunks[1], frame, app, config),
        _ => unreachable!(),
    }
    let help = app.settings.tr(if app.editing.is_some() {
        TKey::HelpEditing
    } else if app.tab == TAB_CONFIG {
        TKey::HelpConfigure
    } else {
        TKey::HelpGrid
    });
    frame.render_widget(
        Paragraph::new(help).block(
            Block::default()
                .borders(Borders::ALL)
                .title(app.settings.tr(TKey::ControlsTitle)),
        ),
        chunks[2],
    );
    if app.editing.is_some() {
        render_edit_modal(area, frame, app);
    }
    if app.password_purpose.is_some() {
        render_password_modal(area, frame, app);
    }
    if app.market_picker.is_some() {
        render_market_picker(area, frame, app);
    }
    if app.funding_dialog.is_some() {
        render_funding_modal(area, frame, app);
    }
    // Keep the compact header status visible by default. The full inspector is explicitly opened
    // with F2, and Esc/F2 can then reliably close it without immediately being rendered again.
    if app.error_dialog && app.error_report.is_some() {
        render_error_dialog(area, frame, app);
    }
}

/// Searchable market terminal. Unlike the previous Markets Tab it is an overlay: the user can
/// inspect a live market and apply it without losing their place in Configure.
fn render_market_picker(area: Rect, frame: &mut ratatui::Frame, app: &App) {
    let Some(picker) = app.market_picker.as_ref() else {
        return;
    };
    let popup = centered_rect(94, area.height.saturating_sub(4), area);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .title("Market Picker — pending/live"),
        popup,
    );
    let inner = popup.inner(Margin {
        vertical: 1,
        horizontal: 1,
    });
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(3),
        ])
        .split(inner);
    let search = format!(
        "Search [{}]: {}   | Product: {:?}   | Network: {}",
        if picker.query.is_empty() {
            "type to filter"
        } else {
            "filter"
        },
        picker.query,
        app.settings.product,
        app.settings.network
    );
    frame.render_widget(
        Paragraph::new(search).block(
            Block::default()
                .borders(Borders::ALL)
                .title("Market Terminal — live data"),
        ),
        rows[0],
    );

    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(rows[1]);
    let filtered = app.filtered_markets();
    if picker.markets_pending {
        frame.render_widget(
            Paragraph::new("pending…\nLoading markets asynchronously; input remains available.")
                .wrap(Wrap { trim: true })
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title("Markets · pending"),
                ),
            columns[0],
        );
    } else if filtered.is_empty() {
        frame.render_widget(
            Paragraph::new(
                picker
                    .detail_error
                    .as_deref()
                    .unwrap_or("No market matches this search."),
            )
            .wrap(Wrap { trim: true })
            .block(Block::default().borders(Borders::ALL).title("Markets")),
            columns[0],
        );
    } else {
        let market_rows = filtered.iter().enumerate().map(|(index, market)| {
            let active = market.name == app.settings.market;
            let style = if index == picker.selected {
                Style::default()
                    .bg(Color::Blue)
                    .add_modifier(Modifier::BOLD)
            } else if active {
                Style::default().fg(Color::Green)
            } else {
                Style::default()
            };
            Row::new(vec![
                Cell::from(if active { "*" } else { " " }),
                Cell::from(market.name.clone()),
                Cell::from(format_decimal(market.tick_size, 6)),
            ])
            .style(style)
        });
        frame.render_widget(
            Table::new(
                market_rows,
                [
                    Constraint::Length(2),
                    Constraint::Min(14),
                    Constraint::Length(14),
                ],
            )
            .header(
                Row::new([" ", "Market", "Tick"]).style(
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
            )
            .block(Block::default().borders(Borders::ALL).title("Markets")),
            columns[0],
        );
    }

    let selected = filtered.get(picker.selected);
    let market_name = selected.map_or("—", |market| market.name.as_str());
    let mid = if picker.detail_pending {
        "pending…".to_owned()
    } else {
        picker
            .mid
            .map(|value| format_decimal(value, 8))
            .unwrap_or_else(|| "unavailable".to_owned())
    };
    let mut detail_lines = vec![Line::from(Span::styled(
        format!("{market_name}  |  Mid: {mid}"),
        Style::default().add_modifier(Modifier::BOLD),
    ))];
    if let Some(market) = selected {
        detail_lines.push(Line::from(format!(
            "Tick: {}   Lot: {}   Min size: {}",
            format_decimal(market.tick_size, 8),
            format_decimal(market.lot_size, 8),
            format_decimal(market.min_size, 8),
        )));
    }
    if let Some(book) = &picker.book {
        detail_lines.push(Line::from(""));
        detail_lines.push(Line::from(Span::styled(
            "ASKS (best first)",
            Style::default().fg(Color::Red),
        )));
        for level in &book.asks {
            detail_lines.push(Line::from(format!(
                "  {:>16}   {:>14}",
                format_decimal(level.price, 8),
                format_decimal(level.size, 8),
            )));
        }
        detail_lines.push(Line::from(Span::styled(
            "BIDS (best first)",
            Style::default().fg(Color::Green),
        )));
        for level in &book.bids {
            detail_lines.push(Line::from(format!(
                "  {:>16}   {:>14}",
                format_decimal(level.price, 8),
                format_decimal(level.size, 8),
            )));
        }
    } else if let Some(error) = &picker.detail_error {
        detail_lines.push(Line::from(""));
        detail_lines.push(Line::from(Span::styled(
            error.clone(),
            Style::default().fg(Color::Red),
        )));
    } else {
        detail_lines.push(Line::from(""));
        detail_lines.push(Line::from("Loading live price and order book…"));
    }
    frame.render_widget(
        Paragraph::new(detail_lines)
            .wrap(Wrap { trim: true })
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Live market detail"),
            ),
        columns[1],
    );
    frame.render_widget(
        Paragraph::new(
            "Type to search · ↑↓ choose market · Enter apply to grid · Esc close · f refresh",
        )
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Market picker controls"),
        ),
        rows[2],
    );
}

/// Build the informational text shown in the Spot funding modal.
/// Returns `None` when the required snapshot/subaccount data is not available.
fn build_funding_instructions(app: &App) -> Option<String> {
    let snapshot = app.snapshot.as_ref()?;
    let funds = snapshot.account.spot_funds.as_ref()?;
    let network = app.settings.network.trim();
    let subaccount = app.settings.subaccount.trim();
    if subaccount.is_empty() {
        return None;
    }
    let package = decibel_grid_tui::package_for_network(network).ok()?;
    let metadata = if network.eq_ignore_ascii_case("mainnet") {
        app.settings
            .spot_funding_metadata
            .as_deref()
            .unwrap_or("<set SPOT_FUNDING_METADATA>")
    } else {
        decibel_grid_tui::TESTNET_USDC_METADATA
    };
    let required = snapshot.plan.quote_required;
    let pfs = funds.available_quote_for_bulk();
    let cross = funds.quote_cross_balance();
    let gap = (required - pfs).max(Decimal::ZERO);
    let transfer = cross.min(gap);
    let raw_transfer = (transfer * Decimal::from(1_000_000u64))
        .floor()
        .to_i64()
        .unwrap_or(0);
    let display_cross = if cross < USDC_CROSS_DUST {
        Decimal::ZERO
    } else {
        cross
    };
    let mut lines = vec![
        "Spot funding setup instructions".to_owned(),
        String::new(),
        format!("Grid quote required: {} USDC", format_decimal(required, 6)),
        format!("PFS available for bulk: {} USDC", format_decimal(pfs, 6)),
        format!("Cross USDC balance: {} USDC", format_decimal(display_cross, 6)),
        format!("Funding gap: {} USDC", format_decimal(gap, 6)),
        String::new(),
        "Spot bulk orders use PFS funds. By default, Spot proceeds settle to Cross (perp collateral). The subaccount owner must call the function below so future Spot proceeds stay in PFS instead of moving to Cross.".to_owned(),
        String::new(),
        "Owner-only entry:".to_owned(),
        format!(
            "  {package}::dex_accounts_spot_entry::set_hold_as_non_collateral_for_subaccount({subaccount}, {metadata}, hold=true)"
        ),
        String::new(),
        "Bot/delegate entry (after owner-only setup):".to_owned(),
        format!(
            "  {package}::dex_accounts_entry::transfer_assets_between_non_collateral_and_collateral({subaccount}, {metadata}, amount=-{raw_transfer})"
        ),
    ];
    if cross < gap {
        let deposit = gap - cross;
        lines.push(String::new());
        lines.push(format!(
            "NOTE: Cross balance is {} USDC short of the gap; deposit {} USDC to PFS after the owner-only setup.",
            format_decimal(deposit, 6),
            format_decimal(deposit, 6)
        ));
    }
    lines.push(String::new());
    lines.push("C copy full instructions · drag to select/copy · Esc or Enter to close".to_owned());
    Some(lines.join("\n"))
}

fn copy_text_to_clipboard(text: &str) -> Result<()> {
    let mut clipboard = arboard::Clipboard::new().context("initialize system clipboard")?;
    clipboard
        .set_text(text.to_owned())
        .context("copy text to system clipboard")?;
    Ok(())
}

/// One visible row after char-wrapping the funding modal text.
struct WrappedLine {
    text: String,
    /// Original logical line index this wrapped row belongs to.
    original_line: u16,
    /// Column offset within the original logical line.
    original_start_col: u16,
}

/// Wrap funding modal text to a fixed character width so every visible row maps to a known
/// substring. This makes mouse selection and vertical scrolling deterministic.
fn wrap_funding_lines(text: &str, width: usize) -> Vec<WrappedLine> {
    let mut result = Vec::new();
    for (line_index, line) in text.lines().enumerate() {
        if line.is_empty() {
            result.push(WrappedLine {
                text: String::new(),
                original_line: line_index as u16,
                original_start_col: 0,
            });
            continue;
        }
        let chars: Vec<char> = line.chars().collect();
        for (chunk_index, chunk) in chars.chunks(width.max(1)).enumerate() {
            result.push(WrappedLine {
                text: chunk.iter().collect(),
                original_line: line_index as u16,
                original_start_col: (chunk_index * width) as u16,
            });
        }
    }
    result
}

/// Compute the funding modal popup, wrapped content, visible rows, and max scroll.
fn funding_modal_layout(app: &App, area: Rect) -> Option<(Rect, Vec<WrappedLine>, u16, u16)> {
    let text = app.funding_dialog.as_deref()?;
    let popup_width = 90u16;
    let content_width = popup_width.saturating_sub(2);
    let wrapped = wrap_funding_lines(text, content_width as usize);
    let content_height = wrapped.len() as u16;
    let popup_height = (content_height + 2)
        .min(area.height.saturating_sub(4))
        .max(12);
    let popup = centered_rect(popup_width, popup_height, area);
    let visible = popup_height.saturating_sub(2);
    let max_scroll = content_height.saturating_sub(visible);
    Some((popup, wrapped, visible, max_scroll))
}

/// Map a mouse position to (wrapped_row, column) inside the funding modal content area.
fn funding_modal_mouse_position(column: u16, row: u16, popup: Rect) -> Option<(u16, u16)> {
    let content = Rect::new(popup.x + 1, popup.y + 1, popup.width - 2, popup.height - 2);
    if column < content.x || column >= content.right() || row < content.y || row >= content.bottom()
    {
        return None;
    }
    Some((row - content.y, column - content.x))
}

/// Map a wrapped row to the original (line, column) it represents.
fn funding_original_position(
    wrapped_lines: &[WrappedLine],
    wrapped_row: u16,
    column: u16,
) -> Option<(u16, u16)> {
    let line = wrapped_lines.get(wrapped_row as usize)?;
    Some((
        line.original_line,
        line.original_start_col.saturating_add(column),
    ))
}

fn normalize_funding_selection(selection: ((u16, u16), (u16, u16))) -> ((u16, u16), (u16, u16)) {
    let ((l1, c1), (l2, c2)) = selection;
    if l1 < l2 || (l1 == l2 && c1 <= c2) {
        ((l1, c1), (l2, c2))
    } else {
        ((l2, c2), (l1, c1))
    }
}

fn split_string_at_indices(s: &str, start: usize, end: usize) -> (String, String, String) {
    let mut chars = s.chars();
    let before: String = chars.by_ref().take(start).collect();
    let selected: String = chars.by_ref().take(end.saturating_sub(start)).collect();
    let after: String = chars.collect();
    (before, selected, after)
}

/// Extract the currently selected text from the funding modal.
fn selected_funding_text(app: &App) -> Option<String> {
    let text = app.funding_dialog.as_deref()?;
    let selection = app.funding_selection?;
    let lines: Vec<&str> = text.lines().collect();
    let ((start_line, start_col), (end_line, end_col)) = normalize_funding_selection(selection);
    let mut result = String::new();
    for line_idx in start_line..=end_line {
        let line_idx = line_idx as usize;
        if line_idx >= lines.len() {
            break;
        }
        let line = lines[line_idx];
        let line_len = line.chars().count() as u16;
        let s = if line_idx as u16 == start_line {
            start_col
        } else {
            0
        };
        let e = if line_idx as u16 == end_line {
            end_col
        } else {
            line_len
        };
        let s = s.min(line_len);
        let e = e.max(s).min(line_len);
        let selected: String = line
            .chars()
            .skip(s as usize)
            .take((e - s) as usize)
            .collect();
        result.push_str(&selected);
        if line_idx as u16 != end_line {
            result.push('\n');
        }
    }
    if result.is_empty() {
        None
    } else {
        Some(result)
    }
}

/// Render one wrapped row of the funding modal, highlighting the selected region if any.
fn render_funding_wrapped_line(
    wrapped: &WrappedLine,
    width: u16,
    selection: Option<((u16, u16), (u16, u16))>,
) -> Line<'static> {
    let selection = selection.map(normalize_funding_selection);
    let Some(((start_line, start_col), (end_line, end_col))) = selection else {
        return Line::from(wrapped.text.clone());
    };
    let line_index = wrapped.original_line;
    if line_index < start_line || line_index > end_line {
        return Line::from(wrapped.text.clone());
    }
    let segment_start = wrapped.original_start_col;
    let segment_end = segment_start.saturating_add(width);
    let sel_start = if line_index == start_line {
        start_col
    } else {
        0
    };
    let sel_end = if line_index == end_line {
        end_col
    } else {
        u16::MAX
    };
    let line_len = wrapped.text.chars().count() as u16;
    let s = sel_start
        .max(segment_start)
        .saturating_sub(segment_start)
        .min(line_len);
    let e = sel_end
        .min(segment_end)
        .saturating_sub(segment_start)
        .min(line_len);
    if s >= e {
        return Line::from(wrapped.text.clone());
    }
    let selected_style = Style::default().bg(Color::Blue).fg(Color::White);
    let (before, selected, after) = split_string_at_indices(&wrapped.text, s as usize, e as usize);
    Line::from(vec![
        Span::raw(before),
        Span::styled(selected, selected_style),
        Span::raw(after),
    ])
}

/// Informational Spot funding modal. It shows the two entry functions and their arguments.
/// The bot does not submit HOLD_AS_NON_COLLATERAL (owner-only); it also does not submit the
/// Cross→PFS transfer from this modal. Users can copy the instructions with C or by selecting
/// text with the mouse.
fn render_funding_modal(area: Rect, frame: &mut ratatui::Frame, app: &App) {
    let Some((popup, wrapped, visible, max_scroll)) = funding_modal_layout(app, area) else {
        return;
    };
    frame.render_widget(Clear, popup);
    let scroll = app.funding_scroll.min(max_scroll);
    let content_width = popup.width.saturating_sub(2);
    let lines: Vec<Line> = wrapped
        .iter()
        .skip(scroll as usize)
        .take(visible as usize)
        .map(|line| render_funding_wrapped_line(line, content_width, app.funding_selection))
        .collect();
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title("Spot funding setup instructions"),
        ),
        popup,
    );
}

fn render_password_modal(area: Rect, frame: &mut ratatui::Frame, app: &App) {
    let prompt = app.settings.tr(match app.password_purpose {
        Some(PasswordPurpose::LoadProfile) => TKey::PasswordPromptExisting,
        _ => TKey::PasswordPromptNew,
    });
    let popup = centered_rect(75, 8, area);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                prompt,
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from(app.settings.tr(TKey::PasswordNote)),
            // Never echo the password itself.
            Line::from(format!("> {}", "•".repeat(app.password.chars().count()))),
            Line::from(app.settings.tr(TKey::EditSaveCancel)),
        ])
        .wrap(Wrap { trim: true })
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(app.settings.tr(TKey::PasswordPromptTitle)),
        ),
        popup,
    );
}

fn copy_error_to_clipboard(error: &str) -> Result<()> {
    let mut clipboard = arboard::Clipboard::new().context("initialize system clipboard")?;
    clipboard
        .set_text(error.to_owned())
        .context("copy error to system clipboard")?;
    Ok(())
}

fn save_error_report(error: &str) -> Result<PathBuf> {
    let base = dirs::data_local_dir()
        .or_else(dirs::cache_dir)
        .or_else(dirs::home_dir)
        .ok_or_else(|| anyhow::anyhow!("could not determine a writable data directory"))?;
    let dir = base.join("decibel-grid");
    fs::create_dir_all(&dir).context("create error report directory")?;
    let path = dir.join("last-error.txt");
    fs::write(&path, error).context("write error report")?;
    Ok(path)
}

fn render_error_dialog(area: Rect, frame: &mut ratatui::Frame, app: &App) {
    let Some(error) = app.error_report.as_deref() else {
        return;
    };
    let popup = centered_rect(90, 70, area);
    frame.render_widget(Clear, popup);
    let inner = popup.inner(Margin {
        vertical: 1,
        horizontal: 1,
    });
    let text = format!(
        "{}\n\n{}\n\n{}",
        ui(app.settings.language, "Error details", "错误详情"),
        error,
        ui(
            app.settings.language,
            "C/Y copy to clipboard · S save file · Esc/F2 close",
            "C/Y 复制到剪贴板 · S 保存文件 · Esc/F2 关闭"
        )
    );
    frame.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .scroll((0, 0))
            .block(Block::default().borders(Borders::ALL).title(ui(
                app.settings.language,
                "Error report",
                "错误报告",
            ))),
        inner,
    );
    frame.render_widget(
        Block::default().borders(Borders::ALL).title(ui(
            app.settings.language,
            "Error report",
            "错误报告",
        )),
        popup,
    );
}

fn render_config(area: Rect, frame: &mut ratatui::Frame, app: &App) {
    // Web-like two-column layout: editable form on the left, contextual help/simulation on
    // the right. The selected field drives the explanation so the unit is never ambiguous.
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(58), Constraint::Percentage(42)])
        .split(area);
    let visible = app.current_visible_fields();
    let rows = visible.iter().map(|field| {
        let selected = *field == app.active_field();
        let mutable = app.settings.tr(if field.editable() {
            TKey::ActionEdit
        } else {
            TKey::ActionCycle
        });
        let style = if selected {
            Style::default()
                .bg(Color::Blue)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        Row::new(vec![
            Cell::from(field_display_label(app, *field)),
            Cell::from(app.field_value(*field)),
            Cell::from(mutable),
        ])
        .style(style)
    });
    let table = Table::new(
        rows,
        [
            Constraint::Length(22),
            Constraint::Min(20),
            Constraint::Length(8),
        ],
    )
    .header(
        Row::new([
            app.settings.tr(TKey::ColumnField),
            app.settings.tr(TKey::ColumnValue),
            app.settings.tr(TKey::ColumnAction),
        ])
        .style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
    )
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(app.settings.tr(TKey::ConfigTitle)),
    );
    frame.render_widget(table, columns[0]);

    let selected = app.active_field();
    let explanation = field_explanation(app, selected);
    frame.render_widget(
        Paragraph::new(explanation).wrap(Wrap { trim: true }).block(
            Block::default()
                .borders(Borders::ALL)
                .title("What this setting means"),
        ),
        columns[1],
    );
}

fn field_display_label(app: &App, field: Field) -> String {
    match field {
        Field::RangeValue => match app.settings.range_kind {
            RangeKind::Percent => "Range ± (%)".to_owned(),
            RangeKind::Step => "Step (%)".to_owned(),
            RangeKind::Bounds => "Lower Price".to_owned(),
        },
        Field::UpperBound => "Upper Price".to_owned(),
        Field::AllocationValue => match app.settings.allocation_kind {
            AllocationKind::Budget => "Total Budget".to_owned(),
            AllocationKind::FixedSize => "Order Size".to_owned(),
        },
        _ => field.label(app.settings.language).to_owned(),
    }
}

fn field_explanation(app: &App, field: Field) -> String {
    let language = app.settings.language;
    let value = app.field_value(field);
    let english = match field {
        Field::Language => {
            "Default is English. Toggle to Chinese; this choice is saved in the profile."
        }
        Field::Network => {
            "Select testnet for experiments or mainnet for real market data. Markets are reloaded after changing it."
        }
        Field::Product => {
            "Spot uses base/quote inventory and always shows both sides. Perp enables neutral, long, and short modes."
        }
        Field::Market => {
            "Choose a market from the Markets tab. Its tick size, lot size, and minimum order size determine the final grid."
        }
        Field::PerpMode => {
            "Neutral: bids and asks. Long: bids only. Short: asks only. This setting is hidden for Spot."
        }
        Field::RangeKind => {
            "Choose how prices are generated: midpoint ± percent, percent per step, or fixed lower/upper prices."
        }
        Field::RangeValue => match app.settings.range_kind {
            RangeKind::Percent => {
                "Example: 10 means current mid × [0.90, 1.10]. It is not a dollar price."
            }
            RangeKind::Step => {
                "Example: 0.5 means each next level is about 0.5% farther from mid. It is not the total range."
            }
            RangeKind::Bounds => {
                "This is the fixed lower price in quote currency, e.g. 65000 for BTC/USD."
            }
        },
        Field::UpperBound => {
            "Fixed upper price in quote currency. The live midpoint must stay between lower and upper."
        }
        Field::GridCount => {
            "Total Bid + Ask orders. The bot policy caps this at 40; Decibel also caps either side at 30."
        }
        Field::AllocationKind => {
            "Total Budget derives a uniform size from your capital. Fixed Order Size uses the exact base quantity per level."
        }
        Field::AllocationValue => match app.settings.allocation_kind {
            AllocationKind::Budget => {
                "Total quote/USDC budget for this grid. Spot splits it between bid funds and ask inventory; Perp estimates margin."
            }
            AllocationKind::FixedSize => {
                "Base asset quantity submitted at every level. It is rounded down to the market lot size."
            }
        },
        Field::MakerFee => {
            "Decimal rate used only for fee-adjusted simulation. Example: 0.0001 = 0.01%."
        }
        Field::PreviewLeverage => {
            "Perp simulation input only. It does not change on-chain leverage. Hidden for Spot."
        }
        Field::RefreshSeconds => "How often the preview/monitor rereads market and account data.",
        Field::PriceSource => {
            "Prices uses mid_px and works when depth is unavailable. Depth uses best bid/ask and is better for execution."
        }
        Field::ExitAssetPolicy => {
            "Retain leaves Spot base and Perp positions untouched. Sell cancels the bot ladder, sells available Spot base with bounded IOC orders, and submits a reduce-only Perp close."
        }
        Field::ApiKey => {
            "Decibel API key for market/account reads. It is masked and can be encrypted in the profile with Ctrl+S."
        }
        Field::AptosPrivateKey => {
            "Aptos Ed25519 private key used only after explicit execution confirmation; it is encrypted in the profile."
        }
        Field::Subaccount => {
            "Optional address used to read positions, open orders, balances, and trade history."
        }
    };
    let chinese = match field {
        Field::Language => "默认英文。切换为中文后会随配置档案保存。",
        Field::Network => "实验使用 testnet，真实行情使用 mainnet。切换后会重新加载市场。",
        Field::Product => "Spot 使用现货库存并始终显示双边；Perp 才有中性、做多、做空模式。",
        Field::Market => "请在 Markets 页选择市场。Tick、Lot 和最小下单量决定最终网格。",
        Field::PerpMode => "Neutral 双边；Long 仅买单；Short 仅卖单。Spot 不显示此项。",
        Field::RangeKind => "选择价格生成方式：中间价上下百分比、每格百分比、固定上下界。",
        Field::RangeValue => match app.settings.range_kind {
            RangeKind::Percent => "例：10 表示当前中间价的 [90%, 110%]，不是美元价格。",
            RangeKind::Step => "例：0.5 表示每向外一格约增加 0.5%，不是总区间。",
            RangeKind::Bounds => "这里填写固定下界报价，例如 BTC/USD 填 65000。",
        },
        Field::UpperBound => "固定上界报价。实时中间价必须位于上下界之间。",
        Field::GridCount => {
            "Bid + Ask 总数。机器人策略上限为 40，Decibel 对单边另有最多 30 档的限制。"
        }
        Field::AllocationKind => {
            "总预算会根据资金自动推导每格数量；固定数量则每格使用指定 base 数量。"
        }
        Field::AllocationValue => match app.settings.allocation_kind {
            AllocationKind::Budget => {
                "本网格的 quote/USDC 总预算。Spot 分配买单资金和卖单库存；Perp 估算保证金。"
            }
            AllocationKind::FixedSize => "每个价格档的 base 数量，最终会向下对齐市场 Lot。",
        },
        Field::MakerFee => "仅用于扣除手续费的模拟。例：0.0001 = 0.01%。",
        Field::PreviewLeverage => "仅用于 Perp 模拟，不会修改链上杠杆。Spot 隐藏此项。",
        Field::RefreshSeconds => "预览/监控重新读取市场和账户数据的间隔。",
        Field::PriceSource => "Prices 使用 mid_px；Depth 使用最优买卖价，更适合执行。",
        Field::ApiKey => "用于读取 Decibel 市场和账户数据。会被遮蔽,并可在 Ctrl+S 时加密保存。",
        Field::AptosPrivateKey => {
            "用于真实下单的 Aptos Ed25519 私钥。只有确认执行计划后才会使用,并会加密保存。"
        }
        Field::Subaccount => "可选地址,用于读取仓位、挂单、余额和成交历史。",
        Field::ExitAssetPolicy => {
            "Retain：退出时不处理资产。Sell：退出时取消 bot 挂单，用 IOC 卖出可用 Spot base，并提交 reduce-only Perp 平仓。"
        }
    };
    let mut text = if language == Language::Chinese {
        chinese
    } else {
        english
    }
    .to_owned();
    text.push_str(&format!("\n\nCurrent: {value}"));
    if let Some(snapshot) = &app.snapshot {
        let plan = &snapshot.plan;
        let profit = plan
            .profit_preview(Decimal::from_str(&app.settings.maker_fee_rate).unwrap_or_default());
        text.push_str(&format!(
            "\n\nSimulation with current mid {}:\n{} levels ({} bids + {} asks)\nEstimated net capture: {} quote\nQuote reserve: {}\nBase reserve: {}",
            format_decimal(plan.mid, 4),
            plan.bids.len() + plan.asks.len(),
            plan.bids.len(),
            plan.asks.len(),
            format_decimal(profit.net_capture, 6),
            format_decimal(plan.quote_required, 6),
            format_decimal(plan.base_required, 6),
        ));
    } else {
        text.push_str("\n\nSimulation: switch to Preview or Markets to load live data.");
    }
    text
}

fn render_grid(area: Rect, frame: &mut ratatui::Frame, app: &App, config: Option<&GridConfig>) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(8),
            Constraint::Min(6),
            Constraint::Length(8),
        ])
        .split(area);
    let title = if app.tab == TAB_PREVIEW {
        ui(
            app.settings.language,
            "Profit Preview — review then explicitly execute this plan",
            "利润预览 — 审核后明确执行当前计划",
        )
    } else {
        ui(
            app.settings.language,
            "Live Monitor — tracks submitted plan; does not auto-replace orders",
            "实时监控 — 跟踪已提交计划；不会自动撤单重挂",
        )
    };
    let Some(snapshot) = app.snapshot.as_ref() else {
        frame.render_widget(
            Paragraph::new(app.error.as_deref().unwrap_or("Loading market data..."))
                .style(Style::default().fg(Color::Red))
                .wrap(Wrap { trim: true })
                .block(Block::default().borders(Borders::ALL).title(title)),
            area,
        );
        return;
    };
    let Some(config) = config else {
        frame.render_widget(
            Paragraph::new("Waiting for valid configuration...")
                .block(Block::default().borders(Borders::ALL).title(title)),
            area,
        );
        return;
    };
    let execute_button = preview_execute_button(area);
    let execute_label = if app.execution_pending {
        "  EXECUTING...  "
    } else {
        "  [E] USE run -e  "
    };
    frame.render_widget(
        Paragraph::new(execute_label)
            .style(if app.execution_pending {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::Black).bg(Color::Green)
            })
            .alignment(ratatui::layout::Alignment::Center),
        execute_button,
    );
    let profit = snapshot.plan.profit_preview(config.maker_fee_rate);
    let order_count = grid_price_count(&snapshot.plan);
    let total_notional: Decimal = snapshot.plan.all_levels().map(|level| level.notional).sum();
    let average_order_notional = if order_count == 0 {
        Decimal::ZERO
    } else {
        total_notional / Decimal::from(order_count)
    };
    let average_pair_profit = if profit.matched_pairs == 0 {
        None
    } else {
        Some(profit.net_capture / Decimal::from(profit.matched_pairs))
    };
    // Perp collateral and Spot inventory are materially different. Do not present a Perp
    // position as though it were a spot wallet balance: that would make the funding panel look
    // polished but be economically wrong. The REST overview currently supplies Perp margin,
    // while Spot wallet balances need a separate account-assets endpoint.
    let capital_line = match snapshot.market.product {
        Product::Perp => {
            let margin_required = snapshot.plan.estimated_margin.unwrap_or(Decimal::ZERO);
            let available_margin = snapshot.account.available_margin;
            let margin_shortfall =
                available_margin.map(|balance| (margin_required - balance).max(Decimal::ZERO));
            format!(
                "{}  {} {}  ·  {} {}  ·  {} {}",
                ui(app.settings.language, "COLLATERAL", "保证金"),
                ui(app.settings.language, "Required", "需求"),
                format_decimal(margin_required, 6),
                ui(app.settings.language, "Available", "可用"),
                available_margin
                    .map(|value| format_decimal(value, 6))
                    .unwrap_or_else(|| "—".to_owned()),
                ui(app.settings.language, "Shortfall", "缺口"),
                margin_shortfall
                    .map(|value| format_decimal(value, 6))
                    .unwrap_or_else(|| "—".to_owned()),
            )
        }
        Product::Spot => format!(
            "{}  {} {}  ·  {} {}",
            ui(app.settings.language, "INVENTORY", "库存需求"),
            ui(app.settings.language, "Quote required", "报价资产需求"),
            format_decimal(snapshot.plan.quote_required, 6),
            ui(app.settings.language, "Base required", "基础资产需求"),
            format_decimal(snapshot.plan.base_required, 6),
        ),
    };
    let exposure_line = match snapshot.market.product {
        Product::Perp => format!(
            "{}  {} {}  ·  {} {}",
            ui(app.settings.language, "GRID EXPOSURE", "网格敞口"),
            ui(app.settings.language, "Buy-side", "买入侧"),
            format_decimal(snapshot.plan.quote_required, 6),
            ui(app.settings.language, "Sell-side", "卖出侧"),
            format_decimal(
                snapshot.plan.asks.iter().map(|level| level.notional).sum(),
                6,
            ),
        ),
        Product::Spot => format!(
            "{}  {} {}  ·  {} {}",
            ui(app.settings.language, "ORDER INVENTORY", "挂单库存"),
            ui(app.settings.language, "Buy orders", "买单资金"),
            format_decimal(snapshot.plan.quote_required, 6),
            ui(app.settings.language, "Sell orders", "卖单数量"),
            format_decimal(snapshot.plan.base_required, 6),
        ),
    };
    let average_pair_profit_display = average_pair_profit
        .map(|value| format_decimal(value, 6))
        .unwrap_or_else(|| ui(app.settings.language, "n/a", "不适用").to_owned());
    let change_notice = app.grid_change_notice.as_deref().unwrap_or_else(|| {
        ui(
            app.settings.language,
            "No grid refresh received yet",
            "尚未收到网格刷新",
        )
    });
    let spot_funding_warning = if snapshot.market.product == Product::Spot {
        snapshot.account.spot_funds.as_ref().and_then(|funds| {
            let required = snapshot.plan.quote_required;
            let pfs = funds.available_quote_for_bulk();
            let cross = funds.quote_cross_balance();
            if pfs >= required {
                return None;
            }
            let total = pfs + cross;
            if total >= required {
                let display_cross = if cross < USDC_CROSS_DUST {
                    Decimal::ZERO
                } else {
                    cross
                };
                Some(format!(
                    "WARNING: grid needs {} USDC in PFS but only {} available; {} USDC in Cross can cover the gap. Press U or click here for funding instructions.",
                    format_decimal(required, 6),
                    format_decimal(pfs, 6),
                    format_decimal(display_cross, 6)
                ))
            } else {
                Some(format!(
                    "WARNING: grid needs {} USDC but PFS+Cross only has {} ({} PFS + {} Cross). Deposit more USDC to PFS.",
                    format_decimal(required, 6),
                    format_decimal(total, 6),
                    format_decimal(pfs, 6),
                    format_decimal(cross, 6)
                ))
            }
        })
    } else {
        None
    };
    let reconciliation_line = match snapshot.reconciliation.as_ref() {
        Some(result) if result.unmanaged.is_empty() => format!(
            "{}  {} {}  {} {}  {} {}  ·  {}",
            ui(app.settings.language, "RECONCILE", "对账"),
            ui(app.settings.language, "Matched", "匹配"),
            result.matched.len(),
            ui(app.settings.language, "Missing", "缺失"),
            result.missing.len(),
            ui(app.settings.language, "Unmanaged", "未知"),
            result.unmanaged.len(),
            ui(
                app.settings.language,
                "no existing-order block",
                "无现存订单阻断",
            ),
        ),
        Some(result) => format!(
            "{}  {} {}  {} {}  {} {}  ·  {}",
            ui(app.settings.language, "RECONCILE", "对账"),
            ui(app.settings.language, "Matched", "匹配"),
            result.matched.len(),
            ui(app.settings.language, "Missing", "缺失"),
            result.missing.len(),
            ui(app.settings.language, "Unmanaged", "未知"),
            result.unmanaged.len(),
            ui(
                app.settings.language,
                "BULK BLOCKED: existing orders",
                "批量下单阻断：存在订单",
            ),
        ),
        None => ui(
            app.settings.language,
            "RECONCILE  unavailable (set subaccount)",
            "对账不可用（请设置子账户）",
        )
        .to_owned(),
    };
    let summary = [
        format!(
            "{} {:?}  {} {}  {} {} – {}",
            snapshot.market.name,
            snapshot.market.product,
            ui(app.settings.language, "Mid", "中间价"),
            format_decimal(snapshot.plan.mid, 6),
            ui(app.settings.language, "Range", "区间"),
            format_decimal(snapshot.plan.lower, 6),
            format_decimal(snapshot.plan.upper, 6),
        ),
        format!(
            "{} {}  {} {}  {} {}",
            ui(app.settings.language, "Orders", "订单数"),
            order_count,
            ui(app.settings.language, "Avg order amount", "每格下单金额"),
            format_decimal(average_order_notional, 6),
            ui(app.settings.language, "Total order amount", "总下单金额"),
            format_decimal(total_notional, 6),
        ),
        format!(
            "{} {}  {} {}",
            ui(app.settings.language, "Net profit", "净利润"),
            format_decimal(profit.net_capture, 6),
            ui(app.settings.language, "Avg profit/trade", "单次平均利润"),
            average_pair_profit_display,
        ),
        capital_line,
        exposure_line,
        spot_funding_warning.unwrap_or_else(|| {
            if snapshot.market.product == Product::Spot {
                "Spot funding: PFS quote is sufficient for the current grid; press U or click here to view funding instructions.".to_owned()
            } else {
                String::new()
            }
        }),
        change_notice.to_owned(),
        reconciliation_line,
    ]
    .join("\n");
    frame.render_widget(
        Paragraph::new(summary).block(Block::default().borders(Borders::ALL).title(title)),
        chunks[0],
    );

    // One square is one order price. The board uses the current plan's actual number of
    // orders, sorted from the lowest price to the highest price.
    let board = chunks[1];
    frame.render_widget(
        Block::default().borders(Borders::ALL).title(
            "Order price grid — low → high  ·  yellow current interval  ·  blue selected  ·  green filled  ·  cyan repriced",
        ),
        board,
    );
    let levels = ordered_grid_levels(&snapshot.plan);
    let (current_bid, current_ask) = current_interval_prices(&snapshot.plan);
    let geometry = price_grid_geometry(area, levels.len(), app.grid_scroll);
    let inner = geometry.cells;
    for (index, level) in levels
        .iter()
        .enumerate()
        .skip(geometry.first_index)
        .take(geometry.columns * geometry.rows)
    {
        let relative = index - geometry.first_index;
        let col = relative % geometry.columns;
        let row = relative / geometry.columns;
        let x = inner.x + col as u16 * geometry.cell_width;
        let y = inner.y + row as u16 * geometry.cell_height;
        if x >= inner.right() || y >= inner.bottom() {
            continue;
        }
        let cell = Rect::new(
            x,
            y,
            geometry.cell_width.min(inner.right().saturating_sub(x)),
            geometry.cell_height.min(inner.bottom().saturating_sub(y)),
        );
        let in_current_price_range =
            Some(level.price) == current_bid || Some(level.price) == current_ask;
        let changed = app
            .price_highlights
            .iter()
            .any(|(price, until)| *until > tokio::time::Instant::now() && level.price == *price);
        let level_profit = level_pair_profit(&snapshot.plan, level, config.maker_fee_rate)
            .map(|profit| format_decimal(profit, 4))
            .unwrap_or_else(|| ui(app.settings.language, "n/a", "不适用").to_owned());
        // Green is reserved for an observed trade at this level. A newly repriced level is cyan,
        // so routine plan refreshes cannot be mistaken for a fill. Explicit selection wins.
        let style = if index == app.selected_level {
            Style::default().fg(Color::White).bg(Color::Blue)
        } else if level.state == LevelState::Filled {
            Style::default().fg(Color::Black).bg(Color::Green)
        } else if changed {
            Style::default().fg(Color::Black).bg(Color::Cyan)
        } else if in_current_price_range {
            Style::default().fg(Color::Black).bg(Color::Yellow)
        } else {
            Style::default().fg(Color::Gray).bg(Color::Rgb(24, 24, 24))
        };
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(level.side.as_str()),
                Line::from(format_decimal(level.price, 5)),
                Line::from(format!(
                    "{} {} · {} {}",
                    ui(app.settings.language, "Order", "下单金额"),
                    format_decimal(level.notional, 4),
                    ui(app.settings.language, "P&L", "预估利润"),
                    level_profit,
                )),
            ])
            .style(style)
            .alignment(ratatui::layout::Alignment::Center)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!("#{:02}", index + 1))
                    .style(style),
            ),
            cell,
        );
    }
    render_trade_history(chunks[2], frame, snapshot);
}

fn level_pair_profit(
    plan: &GridPlan,
    level: &GridLevel,
    maker_fee_rate: Decimal,
) -> Option<Decimal> {
    let pair = match level.side {
        Side::Bid => plan.asks.get(
            plan.bids
                .iter()
                .position(|candidate| std::ptr::eq(candidate, level))?,
        ),
        Side::Ask => plan.bids.get(
            plan.asks
                .iter()
                .position(|candidate| std::ptr::eq(candidate, level))?,
        ),
    }?;
    let size = level.size.min(pair.size);
    let gross = match level.side {
        Side::Bid => (pair.price - level.price) * size,
        Side::Ask => (level.price - pair.price) * size,
    };
    Some(gross - (level.price + pair.price) * size * maker_fee_rate)
}

fn ordered_grid_levels(plan: &GridPlan) -> Vec<&GridLevel> {
    let mut levels: Vec<_> = plan.bids.iter().chain(&plan.asks).collect();
    levels.sort_by_key(|level| level.price);
    levels
}

/// The current mid lies between the highest bid and lowest ask. Highlight these two boundary
/// cells to show the current price interval without colouring every bid-side cell.
fn current_interval_prices(plan: &GridPlan) -> (Option<Decimal>, Option<Decimal>) {
    let bid = plan
        .bids
        .iter()
        .filter(|level| level.price <= plan.mid)
        .map(|level| level.price)
        .max();
    let ask = plan
        .asks
        .iter()
        .filter(|level| level.price >= plan.mid)
        .map(|level| level.price)
        .min();
    (bid, ask)
}

fn changed_level_prices(previous: &GridPlan, current: &GridPlan) -> Vec<Decimal> {
    let old = ordered_grid_levels(previous);
    // A price disappearing/reappearing is a normal consequence of rebuilding the theoretical
    // grid around a new mid. Do not call that a fill or flash the whole board cyan. Only retain
    // a highlight when the same side/price was observed and its execution state changed.
    ordered_grid_levels(current)
        .into_iter()
        .filter_map(|level| {
            old.iter()
                .find(|previous| previous.price == level.price && previous.side == level.side)
                .filter(|previous| previous.state != level.state)
                .map(|_| level.price)
        })
        .collect()
}

fn render_trade_history(area: Rect, frame: &mut ratatui::Frame, snapshot: &MonitorSnapshot) {
    let lines: Vec<Line> = if snapshot.trades.is_empty() {
        vec![Line::from("No account trades for this market yet.")]
    } else {
        snapshot
            .trades
            .iter()
            .take(5)
            .map(|trade| {
                Line::from(format!(
                    "{}  price {}  size {}",
                    trade.timestamp_ms,
                    format_decimal(trade.price, 6),
                    format_decimal(trade.size, 6),
                ))
            })
            .collect()
    };
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: true }).block(
            Block::default()
                .borders(Borders::ALL)
                .title("Recent fills / trade history"),
        ),
        area,
    );
}

fn render_edit_modal(area: Rect, frame: &mut ratatui::Frame, app: &App) {
    let field = app.editing.expect("editing field must exist");
    let display = if field == Field::ApiKey {
        "•".repeat(app.settings.api_key.chars().count())
    } else {
        app.field_value(field)
    };
    let popup = centered_rect(75, 7, area);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                format!("Editing {}", field.label(app.settings.language)),
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from(app.settings.tr(if field == Field::ApiKey {
                TKey::EditApiKeyNote
            } else {
                TKey::EditValueNote
            })),
            Line::from(format!("Value: {display}")),
            Line::from(app.settings.tr(TKey::EditSaveCancel)),
        ])
        .wrap(Wrap { trim: true })
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(app.settings.tr(TKey::EditTitle)),
        ),
        popup,
    );
}
fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - height) / 2),
            Constraint::Length(height),
            Constraint::Percentage((100 - height) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - width) / 2),
            Constraint::Percentage(width),
            Constraint::Percentage((100 - width) / 2),
        ])
        .split(vertical[1])[1]
}
