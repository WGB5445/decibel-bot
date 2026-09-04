use std::{path::PathBuf, str::FromStr, time::Duration};

use anyhow::{Context, Result};
use clap::{Args as ClapArgs, Parser, Subcommand};
use decibel_grid_tui::i18n::{Key as TKey, Language};
use decibel_grid_tui::profile::ProfileData;
use decibel_grid_tui::*;
use rust_decimal::Decimal;

#[derive(Parser)]
#[command(
    name = "decibel-grid-tui",
    about = "Decibel grid planner and interactive monitor"
)]
pub struct Cli {
    /// `preview` opens the Preview tab; `run` writes continuous text snapshots; `tui` opens Configure.
    #[command(subcommand)]
    pub(crate) command: Option<Cmd>,
    #[command(flatten)]
    pub(crate) args: Args,
}

#[derive(Subcommand, Clone, Copy, Eq, PartialEq)]
pub enum Cmd {
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
    /// Offline multi-step scenario simulation (zero network); writes JSONL to stdout.
    Simulate,
    Tui,
}

#[derive(ClapArgs, Clone)]
pub struct Args {
    #[arg(long, global = true, env = "NETWORK", default_value = "testnet")]
    pub(crate) network: String,
    #[arg(long, global = true, env = "DECIBEL_API_KEY", hide_env_values = true)]
    pub(crate) decibel_api_key: Option<String>,
    /// Aptos Ed25519 private key. Prefer entering it in the TUI so Ctrl+S encrypts it in the profile.
    #[arg(long, global = true, env = "APTOS_PRIVATE_KEY", hide_env_values = true)]
    pub(crate) aptos_private_key: Option<String>,
    /// Execute the configured grid. Only meaningful with the `run` command.
    #[arg(short = 'e', long = "execute", global = true, default_value_t = false)]
    pub(crate) execute: bool,
    /// Exit Shadow after this many successful reconciliation cycles; Shadow only.
    #[arg(long, global = true, env = "SHADOW_CYCLES")]
    pub(crate) shadow_cycles: Option<usize>,
    /// Required exact acknowledgement for any Mainnet execution: MAINNET.
    #[arg(long, global = true, env = "CONFIRM_MAINNET")]
    pub(crate) confirm_mainnet: Option<String>,
    #[arg(long, global = true, env = "GRID_PROFILE", default_value = "default")]
    pub(crate) profile: String,
    #[arg(
        long,
        global = true,
        env = "PRODUCT",
        value_enum,
        default_value = "perp"
    )]
    product: Product,
    #[arg(long, global = true, env = "MARKET", default_value = "BTC/USD")]
    pub(crate) market: String,
    #[arg(
        long,
        global = true,
        env = "SUBACCOUNT_ADDRESS",
        hide_env_values = true
    )]
    subaccount: Option<String>,
    /// Write stdout and stderr to this file, replacing it at startup.
    #[arg(long, global = true, env = "LOG_FILE")]
    pub(crate) log_file: Option<PathBuf>,
    /// Continue streaming new lines for the `logs` client.
    #[arg(short = 'f', long, global = true, default_value_t = false)]
    pub(crate) follow: bool,
    /// Exit mode for the `stop` client: hold or liquidate.
    #[arg(long, global = true, value_parser = ["hold", "liquidate"])]
    pub(crate) exit_mode: Option<String>,
    /// Human-readable USDC amount to move from Cross to PFS for `spot-funding-setup`.
    #[arg(long, global = true, env = "SPOT_FUNDING_AMOUNT", default_value = "0")]
    pub(crate) spot_funding_amount: String,
    /// Scenario YAML/JSON file for the offline `simulate` command.
    #[arg(long, global = true, env = "GRID_SCENARIO")]
    pub(crate) scenario: Option<PathBuf>,
    /// USDC metadata object; defaults to the testnet USDC metadata.
    #[arg(long, global = true, env = "SPOT_FUNDING_METADATA")]
    pub(crate) spot_funding_metadata: Option<String>,
    #[arg(
        long,
        global = true,
        env = "PERP_GRID_MODE",
        value_enum,
        default_value = "neutral"
    )]
    perp_mode: PerpMode,
    #[arg(long, global = true, env = "GRID_TOTAL_COUNT", default_value_t = 40)]
    pub(crate) grid_count: usize,
    #[arg(long, global = true, env = "GRID_TOTAL_BUDGET")]
    pub(crate) total_budget: Option<String>,
    /// Spot-only quote inventory budget for all bid levels. Overrides the bid share of
    /// GRID_TOTAL_BUDGET when supplied.
    #[arg(long, global = true, env = "TOTAL_QUOTE_BUDGET")]
    pub(crate) total_quote_budget: Option<String>,
    /// Spot-only base inventory budget for all ask levels. It may be zero only when automatic
    /// entry conversion is enabled and sufficient additional PFS quote is available.
    #[arg(long, global = true, env = "TOTAL_BASE_BUDGET")]
    pub(crate) total_base_budget: Option<String>,
    #[arg(long, global = true, env = "GRID_ORDER_SIZE")]
    pub(crate) order_size: Option<String>,
    #[arg(long, global = true, env = "GRID_RANGE_PERCENT")]
    pub(crate) range_percent: Option<String>,
    #[arg(long, global = true, env = "GRID_STEP_PERCENT")]
    pub(crate) grid_step_percent: Option<String>,
    #[arg(long, global = true, env = "GRID_LOWER_PRICE")]
    pub(crate) lower_price: Option<String>,
    #[arg(long, global = true, env = "GRID_UPPER_PRICE")]
    pub(crate) upper_price: Option<String>,
    /// Optional Spot liquidation trigger after price breaks below the lower bound.
    #[arg(long, global = true, env = "SPOT_EXIT_PRICE")]
    pub(crate) spot_exit_price: Option<String>,
    #[arg(long, global = true, env = "GRID_MAKER_FEE_RATE", default_value = "0")]
    pub(crate) maker_fee_rate: String,
    #[arg(long, global = true, env = "PREVIEW_LEVERAGE", default_value = "1")]
    pub(crate) preview_leverage: String,
    #[arg(long, global = true, env = "GRID_REFRESH_SECONDS", default_value_t = 3)]
    pub(crate) refresh_seconds: u64,
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
    pub(crate) min_net_margin_bps: String,
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
    pub(crate) price_buffer_bps: String,
    #[arg(
        long,
        global = true,
        env = "MAX_CONSECUTIVE_BULK_FAILURES",
        default_value_t = 5
    )]
    max_consecutive_bulk_failures: usize,
    #[arg(
        long,
        global = true,
        env = "GRID_OUT_OF_RANGE_ACTION",
        value_enum,
        default_value = "pause"
    )]
    out_of_range_action: OutOfRangeAction,
    /// Perp-only absolute position cap.
    #[arg(long, global = true, env = "GRID_MAX_POSITION")]
    pub(crate) max_position: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RangeKind {
    Percent,
    Step,
    Bounds,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AllocationKind {
    Budget,
    FixedSize,
}

#[derive(Clone)]
pub struct Settings {
    pub(crate) api_key: String,
    /// Aptos Ed25519 key used only after explicit Preview execution confirmation.
    pub(crate) aptos_private_key: String,
    pub(crate) language: Language,
    pub(crate) network: String,
    pub(crate) product: Product,
    pub(crate) market: String,
    pub(crate) subaccount: String,
    pub(crate) perp_mode: PerpMode,
    pub(crate) range_kind: RangeKind,
    pub(crate) range_value: String,
    pub(crate) upper_bound: String,
    pub(crate) grid_count: String,
    pub(crate) allocation_kind: AllocationKind,
    pub(crate) allocation_value: String,
    pub(crate) total_quote_budget: Option<String>,
    pub(crate) total_base_budget: Option<String>,
    pub(crate) maker_fee_rate: String,
    pub(crate) preview_leverage: String,
    pub(crate) refresh_seconds: String,
    pub(crate) price_source: PriceSource,
    /// Optional USDC metadata override for Spot funding instructions on mainnet.
    pub(crate) spot_funding_metadata: Option<String>,
    /// How to handle Spot base + Perp position on exit.
    pub(crate) exit_asset_policy: ExitAssetPolicy,
    /// Optional Spot stop-loss price. When market price reaches this level, the ladder is
    /// cancelled and all available base is sold through the existing exit liquidation path.
    pub(crate) spot_exit_price: Option<String>,
    pub(crate) min_net_margin_bps: String,
    pub(crate) reconciliation_interval_ms: u64,
    pub(crate) ws_reconnect_backoff_ms: String,
    pub(crate) range_breakout_action: RangeBreakoutAction,
    pub(crate) auto_convert_missing_base: bool,
    pub(crate) entry_max_slippage_bps: String,
    pub(crate) exit_max_slippage_bps: String,
    pub(crate) entry_exit_max_attempts: usize,
    pub(crate) entry_exit_retry_backoff_ms: String,
    pub(crate) entry_exit_timeout_ms: u64,
    pub(crate) entry_min_fill_ratio: String,
    pub(crate) price_buffer_bps: String,
    pub(crate) max_consecutive_bulk_failures: usize,
    pub(crate) max_position: Option<String>,
    pub(crate) out_of_range_action: OutOfRangeAction,
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
            max_position: args.max_position.clone(),
            out_of_range_action: args.out_of_range_action,
        }
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

impl Settings {
    /// Built-in defaults used when a profile is reset. Deliberately conservative: testnet,
    /// a small grid, and a read-only-friendly price source.
    pub(crate) fn defaults() -> Self {
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
            max_position: None,
            out_of_range_action: OutOfRangeAction::default(),
        }
    }

    pub(crate) fn network_profile(
        &self,
    ) -> Result<&'static decibel_grid_tui::network::NetworkProfile> {
        decibel_grid_tui::network::default_registry().resolve(&self.network)
    }

    pub(crate) fn tr(&self, key: TKey) -> &'static str {
        i18n::t(self.language, key)
    }

    /// Serializable snapshot of everything except the API key, which is encrypted separately.
    pub(crate) fn to_profile(&self) -> ProfileData {
        ProfileData {
            language: self.language,
            network: self.network.clone(),
            product: format!("{:?}", self.product).to_lowercase(),
            market: self.market.clone(),
            subaccount: self.subaccount.clone(),
            perp_mode: format!("{:?}", self.perp_mode).to_lowercase(),
            out_of_range_action: match self.out_of_range_action {
                OutOfRangeAction::Pause => "pause",
                OutOfRangeAction::CancelOrders => "cancel_orders",
                OutOfRangeAction::ClosePosition => "close_position",
                OutOfRangeAction::ClampContinue => "clamp_continue",
            }
            .to_owned(),
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
            max_position: self.max_position.clone(),
            encrypted_api_key: None,
            encrypted_aptos_private_key: None,
        }
    }

    /// Applies stored values over the current settings. Empty fields are left untouched so a
    /// partially written profile cannot blank out a working configuration.
    pub(crate) fn apply_profile(&mut self, data: &ProfileData) {
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
        self.max_position = data.max_position.clone();
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
        match data.out_of_range_action.as_str() {
            "pause" => self.out_of_range_action = OutOfRangeAction::Pause,
            "cancel_orders" => self.out_of_range_action = OutOfRangeAction::CancelOrders,
            "close_position" => self.out_of_range_action = OutOfRangeAction::ClosePosition,
            "clamp_continue" => self.out_of_range_action = OutOfRangeAction::ClampContinue,
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

    pub(crate) fn to_grid_config(&self) -> Result<GridConfig> {
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
            max_position: self.max_position.as_deref().map(decimal).transpose()?,
            out_of_range_action: self.out_of_range_action,
        })
    }
    pub(crate) fn api_client(&self) -> Result<DecibelClient> {
        if self.api_key.trim().is_empty() {
            anyhow::bail!("API key is required. Select API Key and press Enter to set it.")
        }
        let _ = self.network_profile()?;
        DecibelClient::new(&self.network, &self.api_key)
    }
    pub(crate) fn masked_key(&self) -> String {
        masked_secret(&self.api_key, true)
    }

    pub(crate) fn masked_private_key(&self) -> String {
        masked_secret(&self.aptos_private_key, false)
    }
}

pub(crate) fn decimal(value: &str) -> Result<Decimal> {
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

pub(crate) fn has_complete_grid_config(args: &Args) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_round_trips_perp_out_of_range_action() {
        let mut settings = Settings::defaults();
        settings.out_of_range_action = OutOfRangeAction::ClosePosition;
        let profile = settings.to_profile();
        assert_eq!(profile.out_of_range_action, "close_position");

        let mut restored = Settings::defaults();
        restored.apply_profile(&profile);
        assert_eq!(
            restored.out_of_range_action,
            OutOfRangeAction::ClosePosition
        );
    }

    #[test]
    fn legacy_profile_defaults_out_of_range_action_to_pause() {
        let profile: ProfileData = serde_json::from_str("{}").unwrap();
        assert_eq!(profile.out_of_range_action, "pause");
    }
}
