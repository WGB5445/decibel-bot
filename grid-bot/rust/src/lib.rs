//! Read-only Decibel grid planner and monitor.
//!
//! This first Rust version intentionally does not sign or submit Aptos transactions. It plans
//! the exact bulk grid, monitors market/account data, and marks levels observed in trade history.
//! The executor boundary is isolated so a native Aptos bulk-order submitter can be added without
//! changing the CLI, TUI, or pricing model.

use std::{str::FromStr, time::Duration};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use clap::ValueEnum;
use futures_util::{SinkExt, StreamExt};
use reqwest::{Client as HttpClient, header};
use rust_decimal::{Decimal, RoundingStrategy};
use serde_json::Value;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message, client::IntoClientRequest, http::HeaderValue},
};

pub mod i18n;
pub mod profile;

pub const MAX_LEVELS_PER_SIDE: usize = 40;

/// Validate only properties that are true for every Decibel bearer API key.
///
/// The server is the authority for whether a key exists, is active, and has access; use
/// [`DecibelClient::verify_api_key`] for that remote check.
pub fn validate_api_key_format(api_key: &str) -> Result<()> {
    if api_key.is_empty() {
        bail!("API key is empty")
    }
    if api_key.len() > 512 {
        bail!("API key is too long")
    }
    if api_key.chars().any(char::is_whitespace) {
        bail!("API key must not contain whitespace")
    }
    if api_key.chars().any(char::is_control) {
        bail!("API key contains a control character")
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum Product {
    Spot,
    Perp,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum PerpMode {
    Neutral,
    Long,
    Short,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum PriceSource {
    Prices,
    Depth,
}

#[derive(Clone, Debug)]
pub enum RangeSpec {
    Bounds { lower: Decimal, upper: Decimal },
    Percent { percent: Decimal },
    StepPercent { percent: Decimal },
}

#[derive(Clone, Debug)]
pub enum Allocation {
    TotalBudget(Decimal),
    FixedSize(Decimal),
}

#[derive(Clone, Debug)]
pub struct GridConfig {
    pub product: Product,
    pub perp_mode: PerpMode,
    pub market_name: String,
    pub range: RangeSpec,
    /// Combined bid and ask count. The maximum is 80 = 40 per side.
    pub total_count: usize,
    pub allocation: Allocation,
    pub maker_fee_rate: Decimal,
    pub preview_leverage: Decimal,
    pub refresh: Duration,
    pub price_source: PriceSource,
}

impl GridConfig {
    pub fn validate(&self) -> Result<()> {
        if !(2..=MAX_LEVELS_PER_SIDE * 2).contains(&self.total_count) {
            bail!("grid count must be between 2 and 80")
        }
        if self.maker_fee_rate.is_sign_negative() || self.maker_fee_rate >= Decimal::ONE {
            bail!("maker fee rate must be >= 0 and < 1")
        }
        if self.preview_leverage <= Decimal::ZERO {
            bail!("preview leverage must be positive")
        }
        match self.range {
            RangeSpec::Bounds { lower, upper } if lower >= upper || lower <= Decimal::ZERO => {
                bail!("lower price must be positive and below upper price")
            }
            RangeSpec::Percent { percent } | RangeSpec::StepPercent { percent }
                if percent <= Decimal::ZERO || percent >= Decimal::from(100) =>
            {
                bail!("range/step percent must be > 0 and < 100")
            }
            _ => {}
        }
        match self.allocation {
            Allocation::TotalBudget(v) | Allocation::FixedSize(v) if v <= Decimal::ZERO => {
                bail!("budget or order size must be positive")
            }
            _ => {}
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct Market {
    pub address: String,
    pub name: String,
    pub tick_size: Decimal,
    pub lot_size: Decimal,
    pub min_size: Decimal,
    pub px_decimals: u32,
    pub sz_decimals: u32,
    pub product: Product,
}

#[derive(Clone, Debug)]
pub struct BookLevel {
    pub price: Decimal,
    pub size: Decimal,
}

#[derive(Clone, Debug, Default)]
pub struct OrderBook {
    pub bids: Vec<BookLevel>,
    pub asks: Vec<BookLevel>,
}

#[derive(Clone, Debug)]
pub struct Position {
    pub size: Decimal,
    pub entry_price: Decimal,
}

#[derive(Clone, Debug)]
pub struct AccountOverview {
    pub available_margin: Option<Decimal>,
    pub equity: Option<Decimal>,
    pub position: Position,
    pub open_order_count: usize,
}

#[derive(Clone, Debug)]
pub struct Trade {
    pub price: Decimal,
    pub size: Decimal,
    pub timestamp_ms: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LevelState {
    Planned,
    Resting,
    Filled,
    Selected,
}

#[derive(Clone, Debug)]
pub struct GridLevel {
    pub side: Side,
    pub price: Decimal,
    pub size: Decimal,
    pub notional: Decimal,
    pub state: LevelState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Side {
    Bid,
    Ask,
}

impl Side {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bid => "BID",
            Self::Ask => "ASK",
        }
    }
}

#[derive(Clone, Debug)]
pub struct GridPlan {
    pub mid: Decimal,
    pub lower: Decimal,
    pub upper: Decimal,
    pub bids: Vec<GridLevel>,
    pub asks: Vec<GridLevel>,
    pub quote_required: Decimal,
    pub base_required: Decimal,
    pub estimated_margin: Option<Decimal>,
}

#[derive(Clone, Debug, Default)]
pub struct ProfitPreview {
    pub matched_pairs: usize,
    pub gross_capture: Decimal,
    pub maker_fees: Decimal,
    pub net_capture: Decimal,
    pub min_pair_net: Option<Decimal>,
    pub max_pair_net: Option<Decimal>,
}

impl GridPlan {
    pub fn all_levels(&self) -> impl Iterator<Item = &GridLevel> {
        self.bids.iter().chain(self.asks.iter())
    }

    pub fn apply_trade_history(&mut self, trades: &[Trade], tick: Decimal) {
        for level in self.bids.iter_mut().chain(self.asks.iter_mut()) {
            if trades
                .iter()
                .any(|trade| close_to_tick(trade.price, level.price, tick))
            {
                level.state = LevelState::Filled;
            }
        }
    }

    pub fn select(&mut self, selected: usize) {
        for (index, level) in self.bids.iter_mut().chain(self.asks.iter_mut()).enumerate() {
            if level.state != LevelState::Filled {
                level.state = if index == selected {
                    LevelState::Selected
                } else {
                    LevelState::Planned
                };
            }
        }
    }

    /// Scenario-only maker-to-maker capture. Every pair assumes its bid fills, then the paired
    /// ask fills later. It excludes funding, gas, partial fills, drift, liquidation and slippage.
    pub fn profit_preview(&self, maker_fee_rate: Decimal) -> ProfitPreview {
        let pairs = self.bids.len().min(self.asks.len());
        let mut result = ProfitPreview {
            matched_pairs: pairs,
            ..ProfitPreview::default()
        };
        let mut nets = Vec::with_capacity(pairs);
        for index in 0..pairs {
            let bid = &self.bids[index];
            let ask = &self.asks[index];
            let size = bid.size.min(ask.size);
            let gross = (ask.price - bid.price) * size;
            let fees = (bid.price * size + ask.price * size) * maker_fee_rate;
            let net = gross - fees;
            result.gross_capture += gross;
            result.maker_fees += fees;
            nets.push(net);
        }
        result.net_capture = result.gross_capture - result.maker_fees;
        result.min_pair_net = nets.iter().copied().min();
        result.max_pair_net = nets.iter().copied().max();
        result
    }
}

pub fn build_plan(config: &GridConfig, market: &Market, mid: Decimal) -> Result<GridPlan> {
    config.validate()?;
    if mid <= Decimal::ZERO {
        bail!("market mid price must be positive")
    }
    let (bid_count, ask_count) = side_counts(config);
    let (lower, upper) = resolve_range(config, mid, bid_count.max(ask_count))?;
    if !(lower < mid && mid < upper) {
        bail!("mid price {mid} is outside grid range [{lower}, {upper}]")
    }

    let bids = prices(
        config,
        Side::Bid,
        mid,
        lower,
        upper,
        bid_count,
        market.tick_size,
    )?;
    let asks = prices(
        config,
        Side::Ask,
        mid,
        lower,
        upper,
        ask_count,
        market.tick_size,
    )?;
    let (bid_size, ask_size) = derive_sizes(config, mid, &bids, &asks, market)?;

    let bid_levels = bids
        .into_iter()
        .map(|price| GridLevel {
            side: Side::Bid,
            price,
            size: bid_size,
            notional: price * bid_size,
            state: LevelState::Planned,
        })
        .collect::<Vec<_>>();
    let ask_levels = asks
        .into_iter()
        .map(|price| GridLevel {
            side: Side::Ask,
            price,
            size: ask_size,
            notional: price * ask_size,
            state: LevelState::Planned,
        })
        .collect::<Vec<_>>();

    let quote_required = bid_levels
        .iter()
        .map(|l| l.notional * (Decimal::ONE + config.maker_fee_rate))
        .sum();
    let base_required = ask_levels.iter().map(|l| l.size).sum();
    let long_notional: Decimal = bid_levels.iter().map(|l| l.notional).sum();
    let short_notional: Decimal = ask_levels.iter().map(|l| l.notional).sum();
    let estimated_margin = match config.product {
        Product::Spot => None,
        Product::Perp => Some(
            long_notional.max(short_notional) / config.preview_leverage
                + (long_notional + short_notional) * config.maker_fee_rate,
        ),
    };
    Ok(GridPlan {
        mid,
        lower,
        upper,
        bids: bid_levels,
        asks: ask_levels,
        quote_required,
        base_required,
        estimated_margin,
    })
}

fn side_counts(config: &GridConfig) -> (usize, usize) {
    match (config.product, config.perp_mode) {
        (Product::Perp, PerpMode::Long) => (config.total_count, 1),
        (Product::Perp, PerpMode::Short) => (1, config.total_count),
        _ => (
            config.total_count / 2,
            config.total_count - config.total_count / 2,
        ),
    }
}

fn resolve_range(config: &GridConfig, mid: Decimal, levels: usize) -> Result<(Decimal, Decimal)> {
    let hundred = Decimal::from(100);
    match config.range {
        RangeSpec::Bounds { lower, upper } => Ok((lower, upper)),
        RangeSpec::Percent { percent } => {
            let fraction = percent / hundred;
            Ok((
                mid * (Decimal::ONE - fraction),
                mid * (Decimal::ONE + fraction),
            ))
        }
        RangeSpec::StepPercent { percent } => {
            let fraction = percent / hundred;
            Ok((
                mid * pow_decimal(Decimal::ONE - fraction, levels),
                mid * pow_decimal(Decimal::ONE + fraction, levels),
            ))
        }
    }
}

fn prices(
    config: &GridConfig,
    side: Side,
    mid: Decimal,
    lower: Decimal,
    upper: Decimal,
    count: usize,
    tick: Decimal,
) -> Result<Vec<Decimal>> {
    let mut values = Vec::with_capacity(count);
    for i in 1..=count {
        let raw = match (&config.range, side) {
            (RangeSpec::StepPercent { percent }, Side::Bid) => {
                mid * pow_decimal(Decimal::ONE - *percent / Decimal::from(100), i)
            }
            (RangeSpec::StepPercent { percent }, Side::Ask) => {
                mid * pow_decimal(Decimal::ONE + *percent / Decimal::from(100), i)
            }
            (_, Side::Bid) => mid - (mid - lower) * Decimal::from(i) / Decimal::from(count),
            (_, Side::Ask) => mid + (upper - mid) * Decimal::from(i) / Decimal::from(count),
        };
        values.push(round_down(raw, tick));
    }
    values.sort();
    values.dedup();
    if side == Side::Bid {
        values.reverse();
    }
    if values.is_empty() {
        bail!("grid range is too narrow for market tick size")
    }
    Ok(values)
}

fn derive_sizes(
    config: &GridConfig,
    _mid: Decimal,
    bids: &[Decimal],
    asks: &[Decimal],
    market: &Market,
) -> Result<(Decimal, Decimal)> {
    let (mut bid, mut ask) = match config.allocation {
        Allocation::FixedSize(size) => (size, size),
        Allocation::TotalBudget(budget) => match config.product {
            Product::Spot => {
                let half = budget / Decimal::TWO;
                let bid_denominator: Decimal =
                    bids.iter().sum::<Decimal>() * (Decimal::ONE + config.maker_fee_rate);
                let ask_denominator: Decimal = asks.iter().sum();
                (
                    if bids.is_empty() {
                        Decimal::ZERO
                    } else {
                        half / bid_denominator
                    },
                    if asks.is_empty() {
                        Decimal::ZERO
                    } else {
                        half / ask_denominator
                    },
                )
            }
            Product::Perp => {
                let bid_notional: Decimal = bids.iter().sum();
                let ask_notional: Decimal = asks.iter().sum();
                let per_base = bid_notional.max(ask_notional) / config.preview_leverage
                    + (bid_notional + ask_notional) * config.maker_fee_rate;
                if per_base <= Decimal::ZERO {
                    bail!("cannot derive size from an empty grid")
                }
                let size = budget / per_base;
                (size, size)
            }
        },
    };
    if config.product == Product::Perp && config.perp_mode == PerpMode::Long {
        ask = Decimal::ZERO;
    }
    if config.product == Product::Perp && config.perp_mode == PerpMode::Short {
        bid = Decimal::ZERO;
    }
    bid = if bid > Decimal::ZERO {
        round_down(bid, market.lot_size)
    } else {
        bid
    };
    ask = if ask > Decimal::ZERO {
        round_down(ask, market.lot_size)
    } else {
        ask
    };
    if bid > Decimal::ZERO && bid < market.min_size {
        bail!(
            "derived bid size {bid} is below min size {}",
            market.min_size
        )
    }
    if ask > Decimal::ZERO && ask < market.min_size {
        bail!(
            "derived ask size {ask} is below min size {}",
            market.min_size
        )
    }
    Ok((bid, ask))
}

pub fn round_down(value: Decimal, increment: Decimal) -> Decimal {
    (value / increment).round_dp_with_strategy(0, RoundingStrategy::ToZero) * increment
}

fn pow_decimal(value: Decimal, exponent: usize) -> Decimal {
    (0..exponent).fold(Decimal::ONE, |acc, _| acc * value)
}

fn close_to_tick(left: Decimal, right: Decimal, tick: Decimal) -> bool {
    (left - right).abs() <= tick / Decimal::TWO
}

#[derive(Clone)]
pub struct DecibelClient {
    http: HttpClient,
    base_url: String,
    ws_url: String,
    api_key: String,
}

impl DecibelClient {
    pub fn new(network: &str, api_key: &str) -> Result<Self> {
        validate_api_key_format(api_key)?;
        let (api_root, ws_url) = match network {
            "mainnet" => (
                "https://api.mainnet.aptoslabs.com/decibel/api/v1",
                "wss://api.mainnet.aptoslabs.com/decibel/ws",
            ),
            "testnet" => (
                "https://api.testnet.aptoslabs.com/decibel/api/v1",
                "wss://api.testnet.aptoslabs.com/decibel/ws",
            ),
            other => bail!("unsupported network {other}; expected mainnet or testnet"),
        };
        let mut headers = header::HeaderMap::new();
        let bearer = format!("Bearer {api_key}")
            .parse()
            .context("invalid DECIBEL_API_KEY header")?;
        headers.insert(header::AUTHORIZATION, bearer);
        let http = HttpClient::builder()
            .default_headers(headers)
            .timeout(Duration::from_secs(15))
            .build()?;
        Ok(Self {
            http,
            base_url: api_root.to_owned(),
            ws_url: ws_url.to_owned(),
            api_key: api_key.to_owned(),
        })
    }

    async fn get(&self, path: &str, params: &[(&str, String)]) -> Result<Value> {
        let url = format!("{}/{}", self.base_url, path);
        let response = self.http.get(url).query(params).send().await?;
        let status = response.status();
        let body = response.text().await?;
        if !status.is_success() {
            bail!("Decibel {path} returned {status}: {body}")
        }
        serde_json::from_str(&body).with_context(|| format!("invalid JSON from Decibel {path}"))
    }

    /// Check that the bearer key is accepted by both documented API transports without exposing
    /// the key or response body: REST (`/markets`) and WebSocket (`all_market_prices`).
    pub async fn verify_api_key(&self) -> Result<()> {
        let url = format!("{}/markets", self.base_url);
        let response = self.http.get(url).send().await?;
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            bail!("API key is invalid or not authorized for REST (HTTP {status})")
        }
        if !status.is_success() {
            bail!("REST API key check could not be completed (HTTP {status})")
        }
        self.ws_snapshot("all_market_prices")
            .await
            .context("API key is not accepted by the Decibel WebSocket gateway")?;
        Ok(())
    }

    pub async fn markets(&self, product: Product) -> Result<Vec<Market>> {
        let data = self.get("markets", &[]).await?;
        let rows = data
            .as_array()
            .ok_or_else(|| anyhow!("/markets did not return an array"))?;
        rows.iter()
            .map(|row| parse_market(row, product))
            .filter_map(Result::transpose)
            .collect()
    }

    pub async fn market(&self, market_name: &str, product: Product) -> Result<Market> {
        let data = self.get("markets", &[]).await?;
        let rows = data
            .as_array()
            .ok_or_else(|| anyhow!("/markets did not return an array"))?;
        for row in rows {
            if let Some(market) = parse_market(row, product)?
                && market.name.eq_ignore_ascii_case(market_name)
            {
                return Ok(market);
            }
        }
        Err(anyhow!(
            "{} {:?} market was not found",
            market_name,
            product
        ))
    }

    pub async fn mid_price(&self, market: &Market, source: PriceSource) -> Result<Decimal> {
        match source {
            PriceSource::Prices => self.mid_from_prices_or_depth(market).await,
            PriceSource::Depth => self.mid_from_depth(market).await,
        }
    }

    async fn ws_snapshot(&self, topic: &str) -> Result<Value> {
        let mut request = self.ws_url.clone().into_client_request()?;
        request.headers_mut().insert(
            "Sec-WebSocket-Protocol",
            HeaderValue::from_str(&format!("decibel, {}", self.api_key))?,
        );
        let (mut socket, _) = connect_async(request).await?;
        socket
            .send(Message::Text(
                serde_json::json!({"method": "subscribe", "topic": topic})
                    .to_string()
                    .into(),
            ))
            .await?;
        let result = tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(message) = socket.next().await {
                let message = message?;
                let Message::Text(text) = message else {
                    continue;
                };
                let value: Value = serde_json::from_str(&text)?;
                if value.get("topic").and_then(Value::as_str) == Some(topic)
                    && value.get("success").is_none()
                {
                    return Ok::<Value, anyhow::Error>(value);
                }
                if value.get("success") == Some(&Value::Bool(false)) {
                    bail!("WebSocket subscription failed for {topic}: {}", value);
                }
            }
            bail!("WebSocket closed before receiving {topic}")
        })
        .await
        .context("timed out waiting for Decibel WebSocket market data")??;
        Ok(result)
    }

    /// Perp prices come from the documented `all_market_prices` WebSocket topic.
    async fn mid_from_prices(&self, market: &Market) -> Result<Decimal> {
        let data = self.ws_snapshot("all_market_prices").await?;
        let rows = data
            .get("prices")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("all_market_prices did not return a prices array"))?;
        let row = rows
            .iter()
            .find(|row| price_row_matches_market(row, market))
            .ok_or_else(|| {
                anyhow!(
                    "all_market_prices has no Perp price row for {}",
                    market.name
                )
            })?;
        decimal_field(row, "mid_px")
            .or_else(|| decimal_field(row, "mark_px"))
            .filter(|price| *price > Decimal::ZERO)
            .ok_or_else(|| {
                anyhow!(
                    "all_market_prices has no positive price for {}",
                    market.name
                )
            })
    }

    async fn mid_from_prices_or_depth(&self, market: &Market) -> Result<Decimal> {
        match self.mid_from_prices(market).await {
            Ok(price) => Ok(price),
            Err(price_error) => self.mid_from_depth(market).await.map_err(|depth_error| {
                anyhow!(
                    "market {} is registered but has no usable price or order book: all_market_prices: {}; depth: {}",
                    market.name,
                    price_error,
                    depth_error
                )
            }),
        }
    }

    pub async fn order_book(&self, market: &Market, _limit: usize) -> Result<OrderBook> {
        let data = self
            .ws_snapshot(&format!("depth:{}:1", market.address))
            .await?;
        let parse_levels = |side: &str| -> Result<Vec<BookLevel>> {
            data.get(side)
                .and_then(Value::as_array)
                .ok_or_else(|| anyhow!("depth has no {side}"))?
                .iter()
                .map(|row| {
                    Ok(BookLevel {
                        price: decimal_field(row, "price")
                            .ok_or_else(|| anyhow!("depth {side} has no price"))?,
                        size: decimal_field(row, "size")
                            .ok_or_else(|| anyhow!("depth {side} has no size"))?,
                    })
                })
                .collect()
        };
        Ok(OrderBook {
            bids: parse_levels("bids")?,
            asks: parse_levels("asks")?,
        })
    }

    async fn mid_from_depth(&self, market: &Market) -> Result<Decimal> {
        let book = self.order_book(market, 1).await?;
        let bid = book
            .bids
            .first()
            .map(|level| level.price)
            .ok_or_else(|| anyhow!("/depth has no bid price"))?;
        let ask = book
            .asks
            .first()
            .map(|level| level.price)
            .ok_or_else(|| anyhow!("/depth has no ask price"))?;
        Ok((bid + ask) / Decimal::TWO)
    }

    pub async fn account(
        &self,
        subaccount: Option<&str>,
        market: &Market,
    ) -> Result<AccountOverview> {
        let Some(account) = subaccount else {
            return Ok(AccountOverview {
                available_margin: None,
                equity: None,
                position: Position {
                    size: Decimal::ZERO,
                    entry_price: Decimal::ZERO,
                },
                open_order_count: 0,
            });
        };
        let overview = self
            .get("account_overviews", &[("account", account.to_owned())])
            .await?;
        let overview = overview
            .as_array()
            .and_then(|rows| rows.first())
            .unwrap_or(&overview);
        let positions = self
            .get("account_positions", &[("account", account.to_owned())])
            .await?;
        let position = positions
            .as_array()
            .and_then(|rows| {
                rows.iter().find(|row| {
                    normalized_address(value_str(row, "market").unwrap_or_default())
                        == normalized_address(&market.address)
                })
            })
            .map(|row| Position {
                size: decimal_field(row, "size").unwrap_or(Decimal::ZERO),
                entry_price: decimal_field(row, "entry_price").unwrap_or(Decimal::ZERO),
            })
            .unwrap_or(Position {
                size: Decimal::ZERO,
                entry_price: Decimal::ZERO,
            });
        let open = self
            .get("open_orders", &[("account", account.to_owned())])
            .await?;
        let open_order_count = open
            .get("items")
            .and_then(Value::as_array)
            .map(|rows| {
                rows.iter()
                    .filter(|row| {
                        normalized_address(value_str(row, "market").unwrap_or_default())
                            == normalized_address(&market.address)
                    })
                    .count()
            })
            .unwrap_or(0);
        Ok(AccountOverview {
            available_margin: decimal_field(overview, "cross_available_to_trade")
                .or_else(|| decimal_field(overview, "perp_equity_balance")),
            equity: decimal_field(overview, "perp_equity_balance"),
            position,
            open_order_count,
        })
    }

    pub async fn trade_history(
        &self,
        subaccount: Option<&str>,
        market: &Market,
    ) -> Result<Vec<Trade>> {
        let Some(account) = subaccount else {
            return Ok(Vec::new());
        };
        let data = self
            .get(
                "trade_history",
                &[
                    ("account", account.to_owned()),
                    ("market", market.address.clone()),
                    ("limit", "100".to_owned()),
                ],
            )
            .await?;
        let rows = data
            .get("items")
            .and_then(Value::as_array)
            .or_else(|| data.as_array())
            .ok_or_else(|| anyhow!("/trade_history returned no items array"))?;
        Ok(rows
            .iter()
            .filter_map(|row| {
                Some(Trade {
                    price: decimal_field(row, "price")?,
                    size: decimal_field(row, "size")?,
                    timestamp_ms: integer_field(row, "transaction_unix_ms").unwrap_or_default(),
                })
            })
            .collect())
    }
}

#[derive(Clone, Debug)]
pub struct MonitorSnapshot {
    pub observed_at: DateTime<Utc>,
    pub market: Market,
    pub plan: GridPlan,
    pub account: AccountOverview,
    /// Most recent account trades for the active market, newest first when supplied by the API.
    pub trades: Vec<Trade>,
    pub status: String,
}

pub async fn fetch_snapshot(
    client: &DecibelClient,
    config: &GridConfig,
    subaccount: Option<&str>,
) -> Result<MonitorSnapshot> {
    let market = client.market(&config.market_name, config.product).await?;
    let mid = client.mid_price(&market, config.price_source).await?;
    let mut plan = build_plan(config, &market, mid)?;
    let (account, trades) = tokio::try_join!(
        client.account(subaccount, &market),
        client.trade_history(subaccount, &market)
    )?;
    plan.apply_trade_history(&trades, market.tick_size);
    Ok(MonitorSnapshot {
        observed_at: Utc::now(),
        market,
        plan,
        account,
        trades,
        status: "LIVE DATA — READ-ONLY EXECUTOR".to_owned(),
    })
}

fn parse_market(value: &Value, wanted_product: Product) -> Result<Option<Market>> {
    let product = match value_str(value, "asset_type").or_else(|| value_str(value, "product")) {
        Some("spot") | Some("Spot") => Product::Spot,
        Some(_) | None => Product::Perp,
    };
    if product != wanted_product {
        return Ok(None);
    }
    let px_decimals = integer_field(value, "px_decimals").unwrap_or(0) as u32;
    let sz_decimals = integer_field(value, "sz_decimals").unwrap_or(0) as u32;
    let tick = scale_raw(
        decimal_field(value, "tick_size").ok_or_else(|| anyhow!("market tick_size missing"))?,
        px_decimals,
    );
    let lot = scale_raw(
        decimal_field(value, "lot_size").ok_or_else(|| anyhow!("market lot_size missing"))?,
        sz_decimals,
    );
    let min = scale_raw(
        decimal_field(value, "min_size").ok_or_else(|| anyhow!("market min_size missing"))?,
        sz_decimals,
    );
    Ok(Some(Market {
        address: value_str(value, "market_addr")
            .ok_or_else(|| anyhow!("market_addr missing"))?
            .to_owned(),
        name: value_str(value, "market_name")
            .ok_or_else(|| anyhow!("market_name missing"))?
            .to_owned(),
        tick_size: tick,
        lot_size: lot,
        min_size: min,
        px_decimals,
        sz_decimals,
        product,
    }))
}

fn scale_raw(raw: Decimal, decimals: u32) -> Decimal {
    raw / Decimal::from(10u64.pow(decimals))
}
fn value_str<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key)?.as_str()
}
fn decimal_field(value: &Value, key: &str) -> Option<Decimal> {
    value.get(key).and_then(decimal_value)
}
fn decimal_value(value: &Value) -> Option<Decimal> {
    match value {
        Value::String(v) => Decimal::from_str(v).ok(),
        Value::Number(v) => Decimal::from_str(&v.to_string()).ok(),
        _ => None,
    }
}
fn integer_field(value: &Value, key: &str) -> Option<i64> {
    value
        .get(key)?
        .as_i64()
        .or_else(|| value.get(key)?.as_str()?.parse().ok())
}
fn normalized_address(value: &str) -> String {
    value
        .trim_start_matches("0x")
        .trim_start_matches('0')
        .to_ascii_lowercase()
}

fn price_row_matches_market(row: &Value, market: &Market) -> bool {
    normalized_address(value_str(row, "market").unwrap_or_default())
        == normalized_address(&market.address)
}

pub fn format_decimal(value: Decimal, scale: u32) -> String {
    value
        .round_dp_with_strategy(scale, RoundingStrategy::ToZero)
        .normalize()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn market() -> Market {
        Market {
            address: "0x1".to_owned(),
            name: "BTC/USD".to_owned(),
            tick_size: dec!(1),
            lot_size: dec!(0.01),
            min_size: dec!(0.01),
            px_decimals: 0,
            sz_decimals: 2,
            product: Product::Perp,
        }
    }
    fn config() -> GridConfig {
        GridConfig {
            product: Product::Perp,
            perp_mode: PerpMode::Neutral,
            market_name: "BTC/USD".to_owned(),
            range: RangeSpec::Percent { percent: dec!(10) },
            total_count: 40,
            allocation: Allocation::TotalBudget(dec!(1000)),
            maker_fee_rate: dec!(0.0001),
            preview_leverage: dec!(1),
            refresh: Duration::from_secs(3),
            price_source: PriceSource::Prices,
        }
    }

    #[test]
    fn total_count_splits_between_sides() {
        assert_eq!(side_counts(&config()), (20, 20));
        assert_eq!(
            side_counts(&GridConfig {
                total_count: 41,
                ..config()
            }),
            (20, 21)
        );
    }
    #[test]
    fn plan_obeys_budget_and_level_limit() {
        let mut market = market();
        market.tick_size = dec!(0.1);
        let plan = build_plan(&config(), &market, dec!(100)).unwrap();
        assert_eq!(plan.bids.len(), 20);
        assert_eq!(plan.asks.len(), 20);
        assert!(plan.estimated_margin.unwrap() <= dec!(1000));
    }
    #[test]
    fn step_grid_is_compounded() {
        let plan = build_plan(
            &GridConfig {
                range: RangeSpec::StepPercent { percent: dec!(1) },
                total_count: 4,
                allocation: Allocation::FixedSize(dec!(1)),
                ..config()
            },
            &market(),
            dec!(100),
        )
        .unwrap();
        assert_eq!(plan.bids[0].price, dec!(99));
        assert_eq!(plan.asks[0].price, dec!(101));
    }
    #[test]
    fn trade_marks_matching_level_filled() {
        let mut plan = build_plan(
            &GridConfig {
                total_count: 4,
                allocation: Allocation::FixedSize(dec!(1)),
                ..config()
            },
            &market(),
            dec!(100),
        )
        .unwrap();
        let price = plan.bids[0].price;
        plan.apply_trade_history(
            &[Trade {
                price,
                size: dec!(1),
                timestamp_ms: 0,
            }],
            dec!(1),
        );
        assert_eq!(plan.bids[0].state, LevelState::Filled);
    }

    #[test]
    fn api_key_format_rejects_empty_whitespace_and_control_values() {
        assert!(validate_api_key_format("").is_err());
        assert!(validate_api_key_format(" key").is_err());
        assert!(validate_api_key_format("key\nvalue").is_err());
        assert!(validate_api_key_format("valid-key").is_ok());
    }

    #[test]
    fn api_key_format_rejects_unreasonably_long_values() {
        let key = "k".repeat(513);
        assert!(validate_api_key_format(&key).is_err());
        assert!(validate_api_key_format(&"k".repeat(512)).is_ok());
    }
}
