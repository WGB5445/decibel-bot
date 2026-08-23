use std::{io, str::FromStr, time::Duration};

use tokio::sync::mpsc;

use anyhow::{Context, Result};
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
use rust_decimal::Decimal;

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
            Constraint::Length(6),
            Constraint::Min(6),
            Constraint::Length(8),
        ])
        .split(grid_area)[1];
    let cells = board.inner(Margin {
        vertical: 1,
        horizontal: 1,
    });
    let columns = price_count.clamp(1, 8);
    // A bordered tile needs an interior line for both the side and price. Keep its height fixed
    // and page through rows rather than silently clipping the lower prices.
    let cell_height = 4;
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
        Block::default()
            .borders(Borders::ALL)
            .title("Decibel Grid Agent — READ ONLY"),
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
    /// Validate the key locally, then verify it against the selected network.
    CheckKey,
    Preview,
    Run,
    Tui,
}

#[derive(ClapArgs, Clone)]
struct Args {
    #[arg(long, global = true, env = "NETWORK", default_value = "testnet")]
    network: String,
    #[arg(long, global = true, env = "DECIBEL_API_KEY")]
    decibel_api_key: Option<String>,
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
    #[arg(long, global = true, env = "SUBACCOUNT_ADDRESS")]
    subaccount: Option<String>,
    #[arg(
        long,
        global = true,
        env = "PERP_GRID_MODE",
        value_enum,
        default_value = "neutral"
    )]
    perp_mode: PerpMode,
    #[arg(long, global = true, env = "GRID_TOTAL_COUNT", default_value_t = 80)]
    grid_count: usize,
    #[arg(long, global = true, env = "GRID_TOTAL_BUDGET")]
    total_budget: Option<String>,
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
    maker_fee_rate: String,
    preview_leverage: String,
    refresh_seconds: String,
    price_source: PriceSource,
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
            maker_fee_rate: args.maker_fee_rate.clone(),
            preview_leverage: args.preview_leverage.clone(),
            refresh_seconds: args.refresh_seconds.to_string(),
            price_source: args.price_source,
        }
    }
}

impl Settings {
    /// Built-in defaults used when a profile is reset. Deliberately conservative: testnet,
    /// a small grid, and a read-only-friendly price source.
    fn defaults() -> Self {
        Self {
            api_key: String::new(),
            language: Language::default(),
            network: "testnet".to_owned(),
            product: Product::Perp,
            market: "BTC/USD".to_owned(),
            subaccount: String::new(),
            perp_mode: PerpMode::Neutral,
            range_kind: RangeKind::Percent,
            range_value: "10".to_owned(),
            upper_bound: String::new(),
            grid_count: "80".to_owned(),
            allocation_kind: AllocationKind::Budget,
            allocation_value: "1000".to_owned(),
            maker_fee_rate: "0".to_owned(),
            preview_leverage: "1".to_owned(),
            refresh_seconds: "3".to_owned(),
            price_source: PriceSource::Prices,
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
            maker_fee_rate: self.maker_fee_rate.clone(),
            preview_leverage: self.preview_leverage.clone(),
            refresh_seconds: self.refresh_seconds.clone(),
            price_source: format!("{:?}", self.price_source).to_lowercase(),
            encrypted_api_key: None,
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
        set(&mut self.maker_fee_rate, &data.maker_fee_rate);
        set(&mut self.preview_leverage, &data.preview_leverage);
        set(&mut self.refresh_seconds, &data.refresh_seconds);
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
        })
    }
    fn api_client(&self) -> Result<DecibelClient> {
        if self.api_key.trim().is_empty() {
            anyhow::bail!("API key is required. Select API Key and press Enter to set it.")
        }
        DecibelClient::new(&self.network, &self.api_key)
    }
    fn masked_key(&self) -> String {
        if self.api_key.is_empty() {
            return "not configured".to_owned();
        }
        let chars: Vec<char> = self.api_key.chars().collect();
        let suffix: String = chars.iter().skip(chars.len().saturating_sub(4)).collect();
        format!("••••••••{suffix}")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Field {
    ApiKey,
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
}
const FIELDS: [Field; 17] = [
    Field::ApiKey,
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
];

impl Field {
    fn label(self, language: Language) -> &'static str {
        i18n::t(
            language,
            match self {
                Self::ApiKey => TKey::FieldApiKey,
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
            },
        )
    }
    fn editable(self) -> bool {
        matches!(
            self,
            Self::ApiKey
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
    profile_name: String,
    settings: Settings,
    settings_revision: u64,
    refresh_now: bool,
    snapshot_pending: bool,
    /// Show a success check for two seconds after a successful snapshot refresh.
    refresh_success_until: Option<tokio::time::Instant>,
    /// Start time used to animate the in-progress refresh indicator.
    refresh_started_at: Option<tokio::time::Instant>,
    error: Option<String>,
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
            profile_name,
            settings,
            settings_revision: 0,
            refresh_now: true,
            snapshot_pending: false,
            refresh_success_until: None,
            refresh_started_at: None,
            error: None,
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

    /// Decrypts the stored API key. Only called when a saved profile has one.
    fn load_encrypted_key(&mut self, password: &str) -> Result<()> {
        let store = ProfileStore::load()?;
        let secret = store
            .get(&self.profile_name)
            .and_then(|data| data.encrypted_api_key.clone())
            .ok_or_else(|| anyhow::anyhow!("no encrypted API key is stored in this profile"))?;
        self.settings.api_key = profile::decrypt_secret(password, &secret)?;
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
        }
    }
    fn editable_value_mut(&mut self, field: Field) -> Option<&mut String> {
        match field {
            Field::ApiKey => Some(&mut self.settings.api_key),
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
    );
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

#[tokio::main]
async fn main() -> Result<()> {
    dotenv().ok();
    let cli = Cli::parse();
    match cli.command {
        Some(Cmd::CheckKey) => check_api_key(Settings::from(&cli.args)).await,
        Some(Cmd::Run) => run_cli(Settings::from(&cli.args)).await,
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

async fn run_cli(settings: Settings) -> Result<()> {
    let config = settings.to_grid_config()?;
    let api = settings.api_client()?;
    loop {
        let snapshot = fetch_snapshot(&api, &config, optional_subaccount(&settings)).await?;
        print_snapshot(&snapshot, &config);
        tokio::time::sleep(config.refresh).await;
    }
}

fn optional_subaccount(settings: &Settings) -> Option<&str> {
    (!settings.subaccount.trim().is_empty()).then_some(settings.subaccount.as_str())
}

async fn run_tui(settings: Settings, profile_name: String, initial_tab: usize) -> Result<()> {
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
        if data.encrypted_api_key.is_some() && app.settings.api_key.trim().is_empty() {
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
                                app.error = Some(error.to_string());
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
                            app.error = None;
                        }
                        Err(error) => app.error = Some(error.to_string()),
                    }
                }
                MarketFetch::Snapshot { .. } => {}
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
                Err(error) => app.error = Some(error.to_string()),
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
                        let result = fetch_snapshot(&api, &config, subaccount.as_deref()).await;
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
                        app.error = Some(error.to_string());
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
        Event::Key(key) if app.market_picker.is_some() => handle_market_picker_key(app, key.code),
        Event::Key(key) if app.editing.is_some() => match key.code {
            KeyCode::Enter => {
                app.finish_edit(true);
            }
            KeyCode::Esc => app.finish_edit(false),
            KeyCode::Backspace => {
                if let Some(field) = app.editing {
                    if let Some(value) = app.editable_value_mut(field) {
                        value.pop();
                    }
                }
            }
            KeyCode::Char(character) => {
                if let Some(field) = app.editing {
                    if let Some(value) = app.editable_value_mut(field) {
                        value.push(character);
                    }
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
                    Err(error) => app.error = Some(error.to_string()),
                }
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
                    if app.active_field().editable() {
                        app.start_edit();
                    } else {
                        app.cycle_field(1);
                    }
                }
            } else if app.tab != TAB_CONFIG {
                // Reuse geometry captured during the last draw. This is the only reliable
                // coordinate space: terminal dimensions alone do not capture all Ratatui splits.
                let price_count = app
                    .snapshot
                    .as_ref()
                    .map(|snapshot| grid_price_count(&snapshot.plan))
                    .unwrap_or(0);
                if let Some(index) = app
                    .grid_geometry
                    .and_then(|geometry| geometry.hit_test(mouse.column, mouse.row, price_count))
                {
                    app.selected_level = index;
                }
            }
        }
        _ => {}
    }
    Ok(false)
}

fn render_refresh_indicator(area: Rect, frame: &mut ratatui::Frame, app: &App) {
    let now = tokio::time::Instant::now();
    let (label, style) = if app.snapshot_pending {
        let elapsed = app
            .refresh_started_at
            .map(|started| started.elapsed().as_millis() / 120)
            .unwrap_or(0);
        let spinner = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
        (
            format!(
                " {} Refreshing",
                spinner[(elapsed as usize) % spinner.len()]
            ),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
    } else if app.refresh_success_until.is_some_and(|until| until > now) {
        (
            " ✓ Refreshed".to_owned(),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        return;
    };
    let width = (label.chars().count() as u16).saturating_add(2);
    let x = area.right().saturating_sub(width);
    let indicator = Rect::new(x, area.y, width, 1);
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
    if let Some(error) = &app.error {
        let popup = centered_rect(85, 5, area);
        frame.render_widget(Clear, popup);
        frame.render_widget(
            Paragraph::new(error.clone())
                .style(Style::default().fg(Color::Red))
                .wrap(Wrap { trim: true })
                .block(Block::default().borders(Borders::ALL).title("Status")),
            popup,
        );
    }
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
            "Total Bid + Ask orders. 40 means roughly 20 bids and 20 asks; the protocol maximum is 80 total."
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
        Field::ApiKey => {
            "Decibel API key for market/account reads. It is masked and can be encrypted in the profile with Ctrl+S."
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
        Field::GridCount => "Bid + Ask 总数。40 大约是 20 买 + 20 卖，协议最多 80 个。",
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
        Field::ApiKey => "用于读取 Decibel 市场和账户数据。会被遮蔽，并可在 Ctrl+S 时加密保存。",
        Field::Subaccount => "可选地址，用于读取仓位、挂单、余额和成交历史。",
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
            Constraint::Length(6),
            Constraint::Min(6),
            Constraint::Length(8),
        ])
        .split(area);
    let title = if app.tab == TAB_PREVIEW {
        "Profit Preview — theoretical maker scenario; no execution"
    } else {
        "Live Monitor — updates data; no execution in this Rust version"
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
    let profit = snapshot.plan.profit_preview(config.maker_fee_rate);
    let margin = snapshot
        .plan
        .estimated_margin
        .map(|value| format_decimal(value, 4))
        .unwrap_or_else(|| "n/a".to_owned());
    let available = snapshot
        .account
        .available_margin
        .map(|value| format_decimal(value, 4))
        .unwrap_or_else(|| "n/a".to_owned());
    let change_notice = app
        .grid_change_notice
        .as_deref()
        .unwrap_or("No grid refresh received yet");
    let summary = format!(
        "{} {:?}  mid {}  range {} – {}\nPairs {}  NET {}  Quote {}  Base {}  Margin {}  Available {}\nPosition {} @ {}\n{}",
        snapshot.market.name,
        snapshot.market.product,
        format_decimal(snapshot.plan.mid, 6),
        format_decimal(snapshot.plan.lower, 6),
        format_decimal(snapshot.plan.upper, 6),
        profit.matched_pairs,
        format_decimal(profit.net_capture, 6),
        format_decimal(snapshot.plan.quote_required, 6),
        format_decimal(snapshot.plan.base_required, 6),
        margin,
        available,
        format_decimal(snapshot.account.position.size, 6),
        format_decimal(snapshot.account.position.entry_price, 6),
        change_notice
    );
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
