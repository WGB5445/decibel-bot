use std::{
    fs,
    io::{self, Write},
    path::PathBuf,
    str::FromStr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result};
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
        MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use decibel_grid_tui::i18n::{self, Key as TKey, Language};
use decibel_grid_tui::monitor_log::{
    LogPanelState, MIN_LOG_WIDTH, MIN_MAIN_WIDTH, render_log_panel, split_layout,
};
use decibel_grid_tui::process_lock::SubaccountRunLock;
use decibel_grid_tui::profile::{self, DEFAULT_PROFILE, ProfileData, ProfileStore};
use decibel_grid_tui::*;
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, Wrap},
};
use rust_decimal::{Decimal, prelude::ToPrimitive};
use tokio::sync::mpsc;

use crate::cli::settings::{AllocationKind, Args, RangeKind, Settings};
use crate::engine::optional_subaccount;

/// Cross USDC balance below this threshold is treated as zero for UI warnings and display.
pub(crate) const USDC_CROSS_DUST: Decimal = Decimal::from_parts(1, 0, 0, false, 6);

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

pub const TAB_CONFIG: usize = 0;
pub const TAB_PREVIEW: usize = 1;
pub const TAB_MONITOR: usize = 2;
pub const TAB_COUNT: usize = 3;
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Field {
    ApiKey,
    AptosPrivateKey,
    Language,
    Network,
    Product,
    Market,
    Subaccount,
    PerpMode,
    OutOfRangeAction,
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
    MaxPosition,
}
const FIELDS: [Field; 21] = [
    Field::ApiKey,
    Field::AptosPrivateKey,
    Field::Language,
    Field::Network,
    Field::Product,
    Field::Market,
    Field::Subaccount,
    Field::PerpMode,
    Field::OutOfRangeAction,
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
    Field::MaxPosition,
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
                Self::OutOfRangeAction => TKey::FieldOutOfRangeAction,
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
                Self::MaxPosition => TKey::FieldMaxPosition,
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
                | Self::MaxPosition
        )
    }
    fn visible(self, settings: &Settings) -> bool {
        match self {
            // Direction is meaningful only for perpetual grids. Spot grids are always
            // two-sided inventory grids, so hiding this avoids a misleading setting.
            Self::PerpMode => settings.product == Product::Perp,
            Self::OutOfRangeAction => settings.product == Product::Perp,
            Self::PreviewLeverage => settings.product == Product::Perp,
            Self::MaxPosition => settings.product == Product::Perp,
            Self::UpperBound => settings.range_kind == RangeKind::Bounds,
            _ => true,
        }
    }
}

/// What a password prompt is being collected for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PasswordPurpose {
    SaveProfile,
    LoadProfile,
}

pub enum MarketFetch {
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

pub struct MarketPicker {
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

pub struct App {
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
    /// Engine log sidebar state for the Monitor tab.
    log_panel: LogPanelState,
    /// Whether the Monitor tab currently shows the log sidebar.
    log_split_visible: bool,
    engine_log_polled_at: Option<tokio::time::Instant>,
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
            log_panel: LogPanelState::default(),
            log_split_visible: false,
            engine_log_polled_at: None,
        }
    }

    fn engine_log_poll_due(&self) -> bool {
        if self.tab != TAB_MONITOR || self.settings.subaccount.trim().is_empty() {
            return false;
        }
        self.engine_log_polled_at
            .map(|at| at.elapsed() >= Duration::from_secs(2))
            .unwrap_or(true)
    }

    async fn poll_engine_log_path(&mut self) {
        self.engine_log_polled_at = Some(tokio::time::Instant::now());
        let subaccount = self.settings.subaccount.trim();
        if subaccount.is_empty() {
            self.log_panel.sync_engine_log_path(None);
            return;
        }
        let path = match decibel_grid_tui::client::EngineClient::for_subaccount(subaccount) {
            Ok(client) => client
                .get_status()
                .await
                .ok()
                .and_then(|status| status.log_path),
            Err(_) => None,
        };
        self.log_panel.sync_engine_log_path(path.as_deref());
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
            Field::OutOfRangeAction => match self.settings.out_of_range_action {
                OutOfRangeAction::Pause => "pause",
                OutOfRangeAction::CancelOrders => "cancel_orders",
                OutOfRangeAction::ClosePosition => "close_position",
                OutOfRangeAction::ClampContinue => "clamp_continue",
            }
            .to_owned(),
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
            Field::MaxPosition => self
                .settings
                .max_position
                .clone()
                .unwrap_or_else(|| self.settings.tr(TKey::Optional).to_owned()),
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
            Field::MaxPosition => {
                if self.settings.max_position.is_none() {
                    self.settings.max_position = Some(String::new());
                }
                self.settings.max_position.as_mut()
            }
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
            Field::OutOfRangeAction => {
                self.settings.out_of_range_action =
                    match (self.settings.out_of_range_action, direction >= 0) {
                        (OutOfRangeAction::Pause, true)
                        | (OutOfRangeAction::ClosePosition, false) => {
                            OutOfRangeAction::CancelOrders
                        }
                        (OutOfRangeAction::CancelOrders, true)
                        | (OutOfRangeAction::ClampContinue, false) => {
                            OutOfRangeAction::ClosePosition
                        }
                        (OutOfRangeAction::ClosePosition, true)
                        | (OutOfRangeAction::Pause, false) => OutOfRangeAction::ClampContinue,
                        _ => OutOfRangeAction::Pause,
                    };
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
pub async fn run_tui(settings: Settings, profile_name: String, initial_tab: usize) -> Result<()> {
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
            app.engine_log_polled_at = None;
        }
        if app.tab == TAB_MONITOR {
            if app.engine_log_poll_due() {
                app.poll_engine_log_path().await;
            }
            let _ = app.log_panel.tailer.as_mut().map(|tailer| tailer.poll());
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
                    app.settings.geomi_gas_station_api_key.clone(),
                    app.settings.geomi_gas_station_url.clone(),
                    app.settings.subaccount.clone(),
                    snapshot.market.clone(),
                    snapshot.plan.clone(),
                    app.settings_revision,
                )
            });
            match execution {
                Some((
                    network,
                    api_key,
                    private_key,
                    geomi_api_key,
                    geomi_url,
                    subaccount,
                    market,
                    plan,
                    revision,
                )) if !api_key.trim().is_empty()
                    && !private_key.trim().is_empty()
                    && !subaccount.trim().is_empty() =>
                {
                    app.execution_pending = true;
                    let tx = fetch_tx.clone();
                    tokio::spawn(async move {
                        let gas_station = GasStationConfig::resolve(
                            &network,
                            geomi_api_key.as_deref(),
                            geomi_url.as_deref(),
                        )
                        .ok()
                        .flatten();
                        let gas_station_ref = gas_station.as_ref();
                        let result = execute_bulk_grid(
                            &network,
                            &api_key,
                            &private_key,
                            &subaccount,
                            &market,
                            &plan,
                            gas_station_ref,
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
        terminal.draw(|frame| render(frame.area(), frame, &mut app, config.as_ref()))?;
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
            KeyCode::Char('[') if app.tab == TAB_MONITOR && app.log_split_visible => {
                app.log_panel.scroll_up(1);
            }
            KeyCode::Char(']') if app.tab == TAB_MONITOR && app.log_split_visible => {
                app.log_panel.scroll_down(1);
            }
            KeyCode::Char('f') if app.tab == TAB_MONITOR && app.log_split_visible => {
                app.log_panel.toggle_follow();
            }
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

fn render(area: Rect, frame: &mut ratatui::Frame, app: &mut App, config: Option<&GridConfig>) {
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

pub(crate) fn save_error_report(error: &str) -> Result<PathBuf> {
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
            "All Perp modes place bilateral grids. The mode changes the target-position formula."
        }
        Field::OutOfRangeAction => {
            "Unified Perp action outside the range. Pause is the safe default; clamp_continue must be selected explicitly."
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
        Field::MaxPosition => {
            "Perp-only absolute position cap. Live execution rejects plans that would breach current or worst-case exposure after resting bids/asks fill."
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
        Field::PerpMode => "所有 Perp 模式均挂双边网格；模式仅改变目标仓位公式。",
        Field::OutOfRangeAction => "Perp 越界统一动作。默认 pause；clamp_continue 必须显式选择。",
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
        Field::MaxPosition => {
            "仅 Perp：绝对仓位上限。若当前或最坏成交后敞口会超限，则拒绝提交并取消挂单。"
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

fn render_grid(area: Rect, frame: &mut ratatui::Frame, app: &mut App, config: Option<&GridConfig>) {
    let (content_area, log_area) = if app.tab == TAB_MONITOR && app.log_panel.has_engine_log() {
        let (main, log) = split_layout(area, MIN_MAIN_WIDTH, MIN_LOG_WIDTH);
        app.log_split_visible = log.is_some();
        if let Some(log_rect) = log {
            let viewport = log_rect.height.saturating_sub(2) as usize;
            let content_width = log_rect.width.saturating_sub(2) as usize;
            app.log_panel.update_scroll_bounds(viewport, content_width);
            render_log_panel(frame, log_rect, &app.log_panel);
        }
        (main, log)
    } else {
        app.log_split_visible = false;
        (area, None)
    };
    let _ = log_area;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(8),
            Constraint::Min(6),
            Constraint::Length(8),
        ])
        .split(content_area);
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
            content_area,
        );
        return;
    };
    let Some(config) = config else {
        frame.render_widget(
            Paragraph::new("Waiting for valid configuration...")
                .block(Block::default().borders(Borders::ALL).title(title)),
            content_area,
        );
        return;
    };
    let execute_button = preview_execute_button(content_area);
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
