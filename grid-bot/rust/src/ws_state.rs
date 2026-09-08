//! WS-first state cache for a single running grid engine.
//!
//! Market data is deliberately coalesced into this cache instead of sharing the loss-sensitive
//! fill/rejection channel. The engine only trades when the relevant entries are fresh.

use std::{
    collections::{HashMap, HashSet},
    str::FromStr,
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use futures_util::{SinkExt, StreamExt};
use rust_decimal::Decimal;
use serde_json::Value;
use tokio::{sync::broadcast, task::JoinHandle};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message, client::IntoClientRequest, http::HeaderValue},
};

use crate::{
    ActiveBulkLadder, BookLevel, DecibelClient, OrderBook, Product, Side, Trade, journal,
    reconcile,
    strategy::perp::accounting::{FillSide, PerpFill},
};

pub const DEFAULT_DEPTH_AGGREGATION: u16 = 10;

#[derive(Clone, Debug)]
pub struct Versioned<T> {
    pub value: T,
    pub venue_ms: Option<i64>,
    pub received_at: Instant,
    pub generation: u64,
}

impl<T> Versioned<T> {
    pub fn fresh(&self, max_age: Duration) -> bool {
        self.received_at.elapsed() <= max_age
    }
}

#[derive(Clone, Debug)]
pub struct PerpPrice {
    pub mid: Decimal,
    pub mark: Decimal,
    pub oracle: Decimal,
}

/// A validated bulk-ladder snapshot from `bulk_orders`. Unlike the REST reader, this preserves
/// the predecessor and transaction version required to detect a dropped WebSocket update.
#[derive(Clone, Debug)]
pub struct WsBulkLadder {
    pub active: ActiveBulkLadder,
    pub previous_sequence: Option<u64>,
    pub transaction_version: u64,
    pub transaction_unix_ms: Option<i64>,
    pub event_uid: Option<String>,
}

#[derive(Clone, Debug)]
pub struct WsBulkFill {
    pub event_uid: String,
    pub trade_id: String,
    pub sequence: u64,
    pub side: Side,
    pub price: Decimal,
    pub size: Decimal,
    pub transaction_version: u64,
}

#[derive(Clone, Debug)]
pub struct WsOrder {
    pub order_id: String,
    pub side: Side,
    pub price: Decimal,
    pub remaining_size: Decimal,
    pub transaction_version: u64,
}

#[derive(Clone, Debug)]
pub struct WsUserTrade {
    pub trade_id: String,
    pub action: String,
    pub price: Decimal,
    pub size: Decimal,
    pub fee_amount: Decimal,
    pub realized_pnl_amount: Decimal,
    pub realized_funding_amount: Decimal,
    pub transaction_unix_ms: i64,
    pub transaction_version: Option<u64>,
}

impl WsUserTrade {
    pub fn perp_fill(&self) -> Result<PerpFill> {
        let side = match self.action.to_ascii_lowercase().as_str() {
            "openlong" | "closeshort" | "buy" => FillSide::Buy,
            "closelong" | "openshort" | "sell" => FillSide::Sell,
            action => bail!(
                "Perp WS trade {} has unresolved action {action}",
                self.trade_id
            ),
        };
        let timestamp =
            DateTime::from_timestamp_millis(self.transaction_unix_ms).ok_or_else(|| {
                anyhow::anyhow!("Perp WS trade {} has invalid timestamp", self.trade_id)
            })?;
        Ok(PerpFill {
            id: self.trade_id.clone(),
            side,
            price: self.price,
            quantity: self.size,
            fee_quote: self.fee_amount,
            realized_pnl_quote: self.realized_pnl_amount,
            realized_funding_quote: self.realized_funding_amount,
            timestamp: timestamp.with_timezone(&Utc),
        })
    }
}

#[derive(Clone, Debug)]
pub enum WsLifecycleEvent {
    Ready,
    Reconnected,
    Disconnected,
    Desynced(String),
    TradeApplied,
    BulkFillApplied(WsBulkFill),
    BulkOrderChanged,
    OrderChanged,
}

#[derive(Clone, Debug, Default)]
pub struct WsState {
    pub generation: u64,
    pub connected: bool,
    pub subscriptions_ready: bool,
    pub last_error: Option<String>,
    pub price: Option<Versioned<PerpPrice>>,
    pub depth: Option<Versioned<OrderBook>>,
    pub account_overview: Option<Versioned<Value>>,
    pub positions: Option<Versioned<Value>>,
    pub open_orders: Option<Versioned<Value>>,
    pub typed_orders: HashMap<String, Versioned<WsOrder>>,
    pub orders_hydrated: bool,
    pub orders_desynced: bool,
    pub bulk_ladder: Option<Versioned<WsBulkLadder>>,
    pub bulk_ladder_hydrated: bool,
    pub bulk_ladder_desynced: bool,
    pub trades: Vec<Versioned<Trade>>,
    pub user_trades: Vec<Versioned<WsUserTrade>>,
    seen_trade_ids: HashSet<String>,
    seen_bulk_fill_ids: HashSet<String>,
}

pub type WsStateHandle = Arc<RwLock<WsState>>;

#[derive(Clone, Debug)]
pub struct WsSessionConfig {
    pub product: Product,
    pub market_address: String,
    pub subaccount: String,
    pub depth_aggregation: u16,
    pub reconnect_backoff: Vec<Duration>,
}

impl WsSessionConfig {
    fn topics(&self) -> Vec<String> {
        let mut topics = vec![
            format!("depth:{}:{}", self.market_address, self.depth_aggregation),
            format!("account_overview:{}", self.subaccount),
            format!("account_open_orders:{}", self.subaccount),
            format!("order_updates:{}", self.subaccount),
            format!("user_trades:{}", self.subaccount),
            format!("bulk_orders:{}", self.subaccount),
            format!("bulk_order_fills:{}", self.subaccount),
        ];
        match self.product {
            Product::Perp => {
                topics.push(format!("market_price:{}", self.market_address));
                topics.push(format!("account_positions:{}", self.subaccount));
            }
            Product::Spot => topics.push("all_spot_mids".to_owned()),
        }
        topics
    }
}

pub fn new_handle() -> WsStateHandle {
    Arc::new(RwLock::new(WsState::default()))
}

pub fn lifecycle_channel() -> (
    broadcast::Sender<WsLifecycleEvent>,
    broadcast::Receiver<WsLifecycleEvent>,
) {
    broadcast::channel(256)
}

/// Return a fresh, validated aggregated book. Execution callers deliberately receive an error
/// instead of a REST fallback when the live depth stream is unavailable.
pub fn fresh_depth(state: &WsStateHandle, max_age: Duration) -> Result<OrderBook> {
    let state = state.read().expect("WS state lock poisoned");
    if !state.subscriptions_ready {
        bail!("WebSocket subscriptions are not ready")
    }
    state
        .depth
        .as_ref()
        .filter(|book| book.fresh(max_age))
        .map(|book| book.value.clone())
        .ok_or_else(|| anyhow::anyhow!("WebSocket depth is stale or unavailable"))
}

pub fn trades(state: &WsStateHandle) -> Vec<Trade> {
    state
        .read()
        .expect("WS state lock poisoned")
        .trades
        .iter()
        .map(|trade| trade.value.clone())
        .collect()
}

pub fn perp_fills(state: &WsStateHandle) -> Result<Vec<PerpFill>> {
    state
        .read()
        .expect("WS state lock poisoned")
        .user_trades
        .iter()
        .map(|trade| trade.value.perp_fill())
        .collect()
}

pub fn generation(state: &WsStateHandle) -> u64 {
    state.read().expect("WS state lock poisoned").generation
}

/// Whether the engine may compute execution plans and derive bulk sequences. A subscription ACK
/// without a first `bulk_orders` snapshot is not enough: an empty WS cache cannot prove there is
/// no active ladder, and deriving sequence 1 from Nothing would cause a repeated full replace.
pub fn can_execute(state: &WsStateHandle) -> bool {
    let state = state.read().expect("WS state lock poisoned");
    state.subscriptions_ready
        && state.orders_hydrated
        && state.bulk_ladder_hydrated
        && !state.orders_desynced
        && !state.bulk_ladder_desynced
}

/// Return the last valid venue ladder. A detected sequence gap is a hard execution block until
/// targeted recovery replaces this state; callers must not derive a new sequence from a gap.
pub fn active_bulk_ladder(state: &WsStateHandle) -> Result<Option<ActiveBulkLadder>> {
    let state = state.read().expect("WS state lock poisoned");
    if !state.subscriptions_ready {
        bail!("WebSocket subscriptions are not ready")
    }
    if state.bulk_ladder_desynced {
        bail!("WebSocket bulk ladder sequence is desynced")
    }
    Ok(state
        .bulk_ladder
        .as_ref()
        .map(|ladder| ladder.value.active.clone()))
}

pub fn next_bulk_sequence(state: &WsStateHandle) -> Result<u64> {
    active_bulk_ladder(state)?
        .map(|ladder| {
            ladder
                .sequence
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("bulk sequence number overflow"))
        })
        .transpose()
        .map(|sequence| sequence.unwrap_or(1))
}

pub fn bulk_ladder_desynced(state: &WsStateHandle) -> bool {
    state
        .read()
        .expect("WS state lock poisoned")
        .bulk_ladder_desynced
}

/// Install a targeted REST recovery result after a detected WS sequence gap. This is deliberately
/// explicit so normal execution cannot erase a desync merely by receiving another partial event.
pub fn recover_bulk_ladder(state: &WsStateHandle, ladder: Option<ActiveBulkLadder>) {
    let mut state = state.write().expect("WS state lock poisoned");
    let generation = state.generation;
    state.bulk_ladder = ladder.map(|active| {
        versioned(
            WsBulkLadder {
                active,
                previous_sequence: None,
                transaction_version: state
                    .bulk_ladder
                    .as_ref()
                    .map(|known| known.value.transaction_version.saturating_add(1))
                    .unwrap_or(0),
                transaction_unix_ms: None,
                event_uid: None,
            },
            None,
            Instant::now(),
            generation,
        )
    });
    state.bulk_ladder_desynced = false;
    state.bulk_ladder_hydrated = true;
}

/// Convert the complete typed account-order snapshot plus the active bulk ladder into the
/// existing reconciliation view. A missing base snapshot is not equivalent to an empty account.
pub fn actual_orders(state: &WsStateHandle) -> Result<Vec<reconcile::ActualOrder>> {
    let state = state.read().expect("WS state lock poisoned");
    if !state.subscriptions_ready || !state.orders_hydrated {
        bail!("WebSocket open-order snapshot is not ready")
    }
    if state.orders_desynced || state.bulk_ladder_desynced {
        bail!("WebSocket order state is desynced")
    }
    let mut orders = state
        .typed_orders
        .values()
        .map(|order| reconcile::ActualOrder {
            order_id: order.value.order_id.clone(),
            side: order.value.side,
            price: order.value.price,
            remaining_size: order.value.remaining_size,
            origin: reconcile::OrderOrigin::Standalone,
        })
        .collect::<Vec<_>>();
    if let Some(ladder) = &state.bulk_ladder {
        orders.extend(
            ladder
                .value
                .active
                .levels
                .iter()
                .map(|level| reconcile::ActualOrder {
                    order_id: format!(
                        "bulk:{}:{:?}:{}",
                        ladder.value.active.sequence, level.side, level.index
                    ),
                    side: level.side,
                    price: level.price,
                    remaining_size: level.original_size - level.filled_size,
                    origin: reconcile::OrderOrigin::Bulk,
                })
                .filter(|order| order.remaining_size > Decimal::ZERO),
        );
    }
    Ok(orders)
}

pub fn spawn_ws_session(
    client: DecibelClient,
    config: WsSessionConfig,
    state: WsStateHandle,
    shutdown: Arc<AtomicBool>,
    lifecycle: broadcast::Sender<WsLifecycleEvent>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut failures = 0_usize;
        while !shutdown.load(Ordering::Acquire) {
            let generation = {
                let mut current = state.write().expect("WS state lock poisoned");
                current.generation = current.generation.saturating_add(1);
                current.connected = false;
                current.subscriptions_ready = false;
                // A new connection has no replay cursor. Do not authorize trading from state
                // observed before this generation until the account streams refresh.
                current.price = None;
                current.depth = None;
                current.account_overview = None;
                current.positions = None;
                current.open_orders = None;
                current.typed_orders.clear();
                current.orders_hydrated = false;
                current.orders_desynced = false;
                current.bulk_ladder = None;
                current.bulk_ladder_hydrated = false;
                current.bulk_ladder_desynced = false;
                current.trades.clear();
                current.user_trades.clear();
                current.seen_trade_ids.clear();
                current.seen_bulk_fill_ids.clear();
                current.generation
            };
            if generation > 1 {
                let _ = lifecycle.send(WsLifecycleEvent::Reconnected);
            }
            match run_session(&client, &config, &state, &shutdown, generation, &lifecycle).await {
                Ok(()) => failures = 0,
                Err(error) => {
                    failures = failures.saturating_add(1);
                    let mut current = state.write().expect("WS state lock poisoned");
                    current.connected = false;
                    current.subscriptions_ready = false;
                    current.last_error = Some(format!("{error:#}"));
                    let _ = lifecycle.send(WsLifecycleEvent::Disconnected);
                }
            }
            let delay = config
                .reconnect_backoff
                .get(failures.saturating_sub(1))
                .copied()
                .or_else(|| config.reconnect_backoff.last().copied())
                .unwrap_or(Duration::from_secs(3));
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                _ = wait_for_shutdown(&shutdown) => return,
            }
        }
    })
}

async fn run_session(
    client: &DecibelClient,
    config: &WsSessionConfig,
    state: &WsStateHandle,
    shutdown: &Arc<AtomicBool>,
    generation: u64,
    lifecycle: &broadcast::Sender<WsLifecycleEvent>,
) -> Result<()> {
    let mut request = client.ws_url.clone().into_client_request()?;
    request.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        HeaderValue::from_str(&format!("decibel, {}", client.api_key))?,
    );
    let (mut socket, _) = connect_async(request).await?;
    let topics = config.topics();
    for topic in &topics {
        socket
            .send(Message::Text(
                serde_json::json!({"method":"subscribe", "topic":topic})
                    .to_string()
                    .into(),
            ))
            .await?;
    }
    {
        let mut current = state.write().expect("WS state lock poisoned");
        current.connected = true;
        current.last_error = None;
    }

    let mut acknowledgements = 0_usize;
    while !shutdown.load(Ordering::Acquire) {
        let message = tokio::select! {
            next = socket.next() => next.ok_or_else(|| anyhow::anyhow!("WebSocket closed"))??,
            _ = wait_for_shutdown(shutdown) => return Ok(()),
        };
        match message {
            Message::Text(text) => {
                let payload: Value = serde_json::from_str(&text)?;
                if payload.get("success") == Some(&Value::Bool(false)) {
                    bail!("Decibel WebSocket rejected subscription: {payload}")
                }
                if payload.get("success") == Some(&Value::Bool(true)) {
                    acknowledgements = acknowledgements.saturating_add(1);
                    if acknowledgements >= topics.len() {
                        state
                            .write()
                            .expect("WS state lock poisoned")
                            .subscriptions_ready = true;
                        let _ = lifecycle.send(WsLifecycleEvent::Ready);
                    }
                    continue;
                }
                if let Err(error) = apply_payload(state, config, generation, &payload, lifecycle) {
                    let _ = lifecycle.send(WsLifecycleEvent::Desynced(format!("{error:#}")));
                    return Err(error);
                }
            }
            Message::Ping(payload) => socket.send(Message::Pong(payload)).await?,
            Message::Close(_) => return Ok(()),
            Message::Binary(_) | Message::Pong(_) | Message::Frame(_) => {}
        }
    }
    Ok(())
}

fn apply_payload(
    state: &WsStateHandle,
    config: &WsSessionConfig,
    generation: u64,
    payload: &Value,
    lifecycle: &broadcast::Sender<WsLifecycleEvent>,
) -> Result<()> {
    let Some(topic) = payload.get("topic").and_then(Value::as_str) else {
        return Ok(());
    };
    let now = Instant::now();
    let venue_ms = integer(payload, "unix_ms").or_else(|| integer(payload, "transaction_unix_ms"));
    let mut lifecycle_event = None;
    let mut current = state.write().expect("WS state lock poisoned");
    if current.generation != generation {
        return Ok(());
    }
    if topic.starts_with("market_price:") {
        let price = payload
            .get("price")
            .ok_or_else(|| anyhow::anyhow!("market_price has no price"))?;
        current.price = Some(versioned(
            PerpPrice {
                mid: decimal(price, "mid_px")?,
                mark: decimal(price, "mark_px")?,
                oracle: decimal(price, "oracle_px")?,
            },
            venue_ms,
            now,
            generation,
        ));
    } else if topic.starts_with("depth:") {
        let book = parse_book(payload)?;
        current.depth = Some(versioned(book, venue_ms, now, generation));
    } else if topic.starts_with("account_overview:") {
        current.account_overview = Some(versioned(payload.clone(), venue_ms, now, generation));
    } else if topic.starts_with("account_positions:") {
        current.positions = Some(versioned(payload.clone(), venue_ms, now, generation));
    } else if topic.starts_with("account_open_orders:") {
        current.open_orders = Some(versioned(payload.clone(), venue_ms, now, generation));
        current.typed_orders = parse_open_orders(payload, config, venue_ms, now, generation)?;
        current.orders_hydrated = true;
        current.orders_desynced = false;
        lifecycle_event = Some(WsLifecycleEvent::OrderChanged);
    } else if topic.starts_with("bulk_orders:") {
        apply_bulk_ladder(&mut current, payload, config, venue_ms, now, generation)?;
        lifecycle_event = Some(WsLifecycleEvent::BulkOrderChanged);
    } else if topic.starts_with("user_trades:") {
        append_user_trades(
            &mut current,
            payload,
            &config.market_address,
            venue_ms,
            now,
            generation,
        )?;
        lifecycle_event = Some(WsLifecycleEvent::TradeApplied);
    } else if topic.starts_with("bulk_order_fills:") {
        match apply_bulk_fill(&mut current, payload, config)? {
            Some(fill) => lifecycle_event = Some(WsLifecycleEvent::BulkFillApplied(fill)),
            None => {}
        }
    } else if topic.starts_with("order_updates:") {
        apply_order_updates(&mut current, payload, config, venue_ms, now, generation)?;
        lifecycle_event = Some(WsLifecycleEvent::OrderChanged);
    } else if topic == "all_spot_mids" && config.product == Product::Spot {
        // The per-market depth stream remains the executable Spot price source.
    }
    drop(current);
    if let Some(event) = lifecycle_event {
        let _ = lifecycle.send(event);
    }
    Ok(())
}

fn apply_bulk_ladder(
    state: &mut WsState,
    payload: &Value,
    config: &WsSessionConfig,
    venue_ms: Option<i64>,
    received_at: Instant,
    generation: u64,
) -> Result<()> {
    let Some(ladder) = parse_bulk_ladder(payload, config)? else {
        return Ok(());
    };
    state.bulk_ladder_hydrated = true;
    let Some(current) = state.bulk_ladder.as_ref() else {
        state.bulk_ladder = Some(versioned(ladder, venue_ms, received_at, generation));
        return Ok(());
    };
    if ladder.transaction_version <= current.value.transaction_version {
        return Ok(());
    }
    if ladder.previous_sequence != Some(current.value.active.sequence) {
        state.bulk_ladder_desynced = true;
        bail!(
            "bulk ladder sequence gap: received sequence {} after {}, with predecessor {:?}",
            ladder.active.sequence,
            current.value.active.sequence,
            ladder.previous_sequence
        )
    }
    state.bulk_ladder = Some(versioned(ladder, venue_ms, received_at, generation));
    Ok(())
}

fn parse_bulk_ladder(payload: &Value, config: &WsSessionConfig) -> Result<Option<WsBulkLadder>> {
    let records = payload
        .get("orders")
        .and_then(Value::as_array)
        .map(|rows| rows.iter().collect::<Vec<_>>())
        .or_else(|| {
            payload
                .get("data")
                .and_then(Value::as_array)
                .map(|rows| rows.iter().collect())
        })
        .unwrap_or_else(|| vec![payload]);
    let mut latest = None;
    for record in records {
        let record = record.get("bulk_order").unwrap_or(record);
        if !same_address(
            string(record, "market").unwrap_or_default(),
            &config.market_address,
        ) || product(record.get("asset_type")) != Some(config.product)
        {
            continue;
        }
        let status = string(record, "status").unwrap_or("placed");
        if status.eq_ignore_ascii_case("rejected") {
            continue;
        }
        let candidate = WsBulkLadder {
            active: ActiveBulkLadder {
                product: config.product,
                market_address: config.market_address.clone(),
                sequence: unsigned(record, "sequence_number")?,
                levels: bulk_levels(record, "bid_prices", "bid_sizes", Side::Bid)?
                    .into_iter()
                    .chain(bulk_levels(record, "ask_prices", "ask_sizes", Side::Ask)?)
                    .collect(),
            },
            previous_sequence: optional_unsigned(record, "previous_seq_num")?,
            transaction_version: unsigned(record, "transaction_version")?,
            transaction_unix_ms: integer(record, "transaction_unix_ms"),
            event_uid: string(record, "event_uid").map(ToOwned::to_owned),
        };
        if latest.as_ref().is_none_or(|known: &WsBulkLadder| {
            candidate.transaction_version > known.transaction_version
        }) {
            latest = Some(candidate);
        }
    }
    Ok(latest)
}

fn bulk_levels(
    record: &Value,
    prices_key: &str,
    sizes_key: &str,
    side: Side,
) -> Result<Vec<journal::BulkLevelState>> {
    let prices = record
        .get(prices_key)
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("bulk_orders has no {prices_key} array"))?;
    let sizes = record
        .get(sizes_key)
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("bulk_orders has no {sizes_key} array"))?;
    if prices.len() != sizes.len() {
        bail!(
            "bulk_orders {prices_key}/{sizes_key} length mismatch: {} prices, {} sizes",
            prices.len(),
            sizes.len()
        )
    }
    prices
        .iter()
        .zip(sizes)
        .enumerate()
        .map(|(index, (price, size))| {
            let price = decimal_value(price)?;
            let size = decimal_value(size)?;
            if price <= Decimal::ZERO || size <= Decimal::ZERO {
                bail!("bulk_orders contains a non-positive level")
            }
            Ok(journal::BulkLevelState {
                side,
                index,
                price,
                original_size: size,
                filled_size: Decimal::ZERO,
            })
        })
        .collect()
}

fn parse_open_orders(
    payload: &Value,
    config: &WsSessionConfig,
    venue_ms: Option<i64>,
    received_at: Instant,
    generation: u64,
) -> Result<HashMap<String, Versioned<WsOrder>>> {
    let rows = payload
        .get("orders")
        .or_else(|| payload.get("data").and_then(|data| data.get("orders")))
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("account_open_orders has no orders array"))?;
    let mut orders = HashMap::new();
    for row in rows {
        let order = row.get("order").unwrap_or(row);
        let Some(parsed) = parse_order(order, config)? else {
            continue;
        };
        orders.insert(
            parsed.order_id.clone(),
            versioned(parsed, venue_ms, received_at, generation),
        );
    }
    Ok(orders)
}

fn apply_order_updates(
    state: &mut WsState,
    payload: &Value,
    config: &WsSessionConfig,
    venue_ms: Option<i64>,
    received_at: Instant,
    generation: u64,
) -> Result<()> {
    if !state.orders_hydrated {
        state.orders_desynced = true;
        bail!("order update arrived before the open-order base snapshot")
    }
    let rows = payload
        .get("orders")
        .and_then(Value::as_array)
        .map(|rows| rows.iter().collect::<Vec<_>>())
        .or_else(|| {
            payload
                .get("data")
                .and_then(Value::as_array)
                .map(|rows| rows.iter().collect())
        })
        .unwrap_or_else(|| vec![payload]);
    for row in rows {
        let order = row
            .get("order")
            .and_then(|value| value.get("order"))
            .or_else(|| row.get("order"))
            .unwrap_or(row);
        let Some(parsed) = parse_order(order, config)? else {
            continue;
        };
        let status = string(order, "status").unwrap_or("open");
        if state
            .typed_orders
            .get(&parsed.order_id)
            .is_some_and(|known| known.value.transaction_version >= parsed.transaction_version)
        {
            continue;
        }
        if terminal_order_status(status) {
            state.typed_orders.remove(&parsed.order_id);
        } else {
            state.typed_orders.insert(
                parsed.order_id.clone(),
                versioned(parsed, venue_ms, received_at, generation),
            );
        }
    }
    Ok(())
}

fn parse_order(value: &Value, config: &WsSessionConfig) -> Result<Option<WsOrder>> {
    if !same_address(
        string(value, "market").unwrap_or_default(),
        &config.market_address,
    ) || product(value.get("asset_type")) != Some(config.product)
    {
        return Ok(None);
    }
    let order_id =
        identifier(value, "order_id").ok_or_else(|| anyhow::anyhow!("order has no order_id"))?;
    let side = match value.get("is_buy").and_then(Value::as_bool) {
        Some(true) => Side::Bid,
        Some(false) => Side::Ask,
        None => match string(value, "order_direction")
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            "buy" | "bid" => Side::Bid,
            "sell" | "ask" => Side::Ask,
            _ => bail!("order {order_id} has no usable side"),
        },
    };
    let price = decimal(value, "price")?;
    let remaining_size = value
        .get("remaining_size")
        .or_else(|| value.get("size"))
        .ok_or_else(|| anyhow::anyhow!("order {order_id} has no remaining_size"))
        .and_then(decimal_value)?;
    if price <= Decimal::ZERO || remaining_size <= Decimal::ZERO {
        bail!("order {order_id} has non-positive price or remaining size")
    }
    Ok(Some(WsOrder {
        order_id,
        side,
        price,
        remaining_size,
        transaction_version: optional_unsigned(value, "transaction_version")?.unwrap_or(0),
    }))
}

fn terminal_order_status(status: &str) -> bool {
    matches!(
        status.to_ascii_lowercase().as_str(),
        "filled" | "cancelled" | "canceled" | "expired" | "rejected"
    )
}

fn apply_bulk_fill(
    state: &mut WsState,
    payload: &Value,
    config: &WsSessionConfig,
) -> Result<Option<WsBulkFill>> {
    let rows = payload
        .get("fills")
        .and_then(Value::as_array)
        .map(|rows| rows.iter().collect::<Vec<_>>())
        .or_else(|| {
            payload
                .get("data")
                .and_then(Value::as_array)
                .map(|rows| rows.iter().collect())
        })
        .unwrap_or_else(|| vec![payload]);
    let mut applied = None;
    for row in rows {
        if !same_address(
            string(row, "market").unwrap_or_default(),
            &config.market_address,
        ) || product(row.get("asset_type")) != Some(config.product)
        {
            continue;
        }
        let event_uid = identifier(row, "event_uid")
            .ok_or_else(|| anyhow::anyhow!("bulk fill has no event_uid"))?;
        let trade_id = identifier(row, "trade_id")
            .ok_or_else(|| anyhow::anyhow!("bulk fill has no trade_id"))?;
        let dedupe = format!("{event_uid}:{trade_id}");
        if !state.seen_bulk_fill_ids.insert(dedupe) {
            continue;
        }
        let sequence = unsigned(row, "sequence_number")?;
        let side = match row.get("is_bid").and_then(Value::as_bool) {
            Some(true) => Side::Bid,
            Some(false) => Side::Ask,
            None => match string(row, "side")
                .unwrap_or_default()
                .to_ascii_lowercase()
                .as_str()
            {
                "buy" | "bid" => Side::Bid,
                "sell" | "ask" => Side::Ask,
                _ => bail!("bulk fill has no usable side"),
            },
        };
        let fill = WsBulkFill {
            event_uid,
            trade_id,
            sequence,
            side,
            price: row
                .get("price")
                .or_else(|| row.get("fill_px"))
                .ok_or_else(|| anyhow::anyhow!("bulk fill has no price"))
                .and_then(decimal_value)?,
            size: row
                .get("filled_size")
                .or_else(|| row.get("fill_sz"))
                .or_else(|| row.get("size"))
                .ok_or_else(|| anyhow::anyhow!("bulk fill has no size"))
                .and_then(decimal_value)?,
            transaction_version: unsigned(row, "transaction_version")?,
        };
        let ladder = state
            .bulk_ladder
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("bulk fill arrived without active bulk ladder"))?;
        if fill.sequence != ladder.value.active.sequence
            || fill.transaction_version < ladder.value.transaction_version
        {
            state.bulk_ladder_desynced = true;
            bail!("bulk fill is from an unknown or stale ladder sequence")
        }
        let matches = ladder
            .value
            .active
            .levels
            .iter()
            .enumerate()
            .filter(|(_, level)| level.side == fill.side && level.price == fill.price)
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        let [index] = matches.as_slice() else {
            state.bulk_ladder_desynced = true;
            bail!("bulk fill cannot be uniquely attributed to a ladder level")
        };
        let level = &mut ladder.value.active.levels[*index];
        if fill.size <= Decimal::ZERO || level.filled_size + fill.size > level.original_size {
            state.bulk_ladder_desynced = true;
            bail!("bulk fill has invalid or excessive size")
        }
        level.filled_size += fill.size;
        applied = Some(fill);
    }
    Ok(applied)
}

fn versioned<T>(
    value: T,
    venue_ms: Option<i64>,
    received_at: Instant,
    generation: u64,
) -> Versioned<T> {
    Versioned {
        value,
        venue_ms,
        received_at,
        generation,
    }
}

fn append_user_trades(
    state: &mut WsState,
    payload: &Value,
    market_address: &str,
    venue_ms: Option<i64>,
    received_at: Instant,
    generation: u64,
) -> Result<()> {
    let rows = payload
        .get("trades")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("user_trades has no trades array"))?;
    for row in rows {
        if !same_address(
            row.get("market")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            market_address,
        ) {
            continue;
        }
        let id = row
            .get("trade_id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("user_trades row has no trade_id"))?;
        if !state.seen_trade_ids.insert(id.to_owned()) {
            continue;
        }
        let timestamp_ms = integer(row, "transaction_unix_ms")
            .or(venue_ms)
            .ok_or_else(|| anyhow::anyhow!("user_trades row has no timestamp"))?;
        let user_trade = WsUserTrade {
            trade_id: id.to_owned(),
            action: string(row, "action").unwrap_or("net").to_owned(),
            price: decimal(row, "price")?,
            size: decimal(row, "size")?,
            fee_amount: optional_decimal(row, "fee_amount")?.unwrap_or(Decimal::ZERO),
            realized_pnl_amount: optional_decimal(row, "realized_pnl_amount")?
                .unwrap_or(Decimal::ZERO),
            realized_funding_amount: optional_decimal(row, "realized_funding_amount")?
                .unwrap_or(Decimal::ZERO),
            transaction_unix_ms: timestamp_ms,
            transaction_version: optional_unsigned(row, "transaction_version")?,
        };
        state.trades.push(versioned(
            Trade {
                price: user_trade.price,
                size: user_trade.size,
                timestamp_ms,
            },
            Some(timestamp_ms),
            received_at,
            generation,
        ));
        state.user_trades.push(versioned(
            user_trade,
            Some(timestamp_ms),
            received_at,
            generation,
        ));
    }
    const MAX_TRADES: usize = 256;
    if state.trades.len() > MAX_TRADES {
        state.trades.drain(..state.trades.len() - MAX_TRADES);
        // A retained id set larger than the display window is intentional: it prevents a replayed
        // fill from becoming a second accounting event during this connection generation.
    }
    if state.user_trades.len() > MAX_TRADES {
        state
            .user_trades
            .drain(..state.user_trades.len() - MAX_TRADES);
    }
    Ok(())
}

fn same_address(left: &str, right: &str) -> bool {
    let normalize = |value: &str| {
        value
            .trim()
            .trim_start_matches("0x")
            .trim_start_matches('0')
            .to_ascii_lowercase()
    };
    normalize(left) == normalize(right)
}

fn parse_book(payload: &Value) -> Result<OrderBook> {
    let levels = |side: &str| -> Result<Vec<BookLevel>> {
        payload
            .get(side)
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow::anyhow!("depth has no {side}"))?
            .iter()
            .map(|level| {
                let price = level
                    .get("price")
                    .or_else(|| level.get("px"))
                    .or_else(|| level.get(0))
                    .ok_or_else(|| anyhow::anyhow!("depth level has no price"));
                let size = level
                    .get("size")
                    .or_else(|| level.get("sz"))
                    .or_else(|| level.get(1))
                    .ok_or_else(|| anyhow::anyhow!("depth level has no size"));
                let price = decimal_value(price?)?;
                let size = decimal_value(size?)?;
                if price <= Decimal::ZERO || size <= Decimal::ZERO {
                    bail!("depth contains non-positive level")
                }
                Ok(BookLevel { price, size })
            })
            .collect()
    };
    let mut bids = levels("bids")?;
    let mut asks = levels("asks")?;
    bids.sort_by_key(|level| std::cmp::Reverse(level.price));
    asks.sort_by_key(|level| level.price);
    if bids
        .first()
        .is_some_and(|bid| asks.first().is_some_and(|ask| bid.price >= ask.price))
    {
        bail!("depth is crossed")
    }
    Ok(OrderBook { bids, asks })
}

fn decimal(object: &Value, field: &str) -> Result<Decimal> {
    decimal_value(
        object
            .get(field)
            .ok_or_else(|| anyhow::anyhow!("payload has no {field}"))?,
    )
}
fn optional_decimal(object: &Value, field: &str) -> Result<Option<Decimal>> {
    object
        .get(field)
        .filter(|value| !value.is_null())
        .map(decimal_value)
        .transpose()
}
fn string<'a>(value: &'a Value, field: &str) -> Option<&'a str> {
    value.get(field).and_then(Value::as_str)
}
fn identifier(value: &Value, field: &str) -> Option<String> {
    match value.get(field) {
        Some(Value::String(value)) => Some(value.clone()),
        Some(Value::Number(value)) => Some(value.to_string()),
        _ => None,
    }
}
fn product(value: Option<&Value>) -> Option<Product> {
    match value.and_then(Value::as_str) {
        Some(value) if value.eq_ignore_ascii_case("spot") => Some(Product::Spot),
        Some(value) if value.eq_ignore_ascii_case("perp") => Some(Product::Perp),
        _ => None,
    }
}
fn unsigned(value: &Value, field: &str) -> Result<u64> {
    optional_unsigned(value, field)?.ok_or_else(|| anyhow::anyhow!("payload has no {field}"))
}
fn optional_unsigned(value: &Value, field: &str) -> Result<Option<u64>> {
    let Some(value) = value.get(field).filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    match value {
        Value::Number(value) => value
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("{field} is not an unsigned integer"))
            .map(Some),
        Value::String(value) => value
            .parse::<u64>()
            .map(Some)
            .map_err(|_| anyhow::anyhow!("{field} is not an unsigned integer")),
        _ => bail!("{field} is not an unsigned integer"),
    }
}
fn decimal_value(value: &Value) -> Result<Decimal> {
    match value {
        Value::String(value) => Decimal::from_str(value).map_err(Into::into),
        Value::Number(value) => Decimal::from_str(&value.to_string()).map_err(Into::into),
        _ => bail!("value is not decimal"),
    }
}
fn integer(value: &Value, field: &str) -> Option<i64> {
    value.get(field).and_then(Value::as_i64)
}
async fn wait_for_shutdown(shutdown: &AtomicBool) {
    while !shutdown.load(Ordering::Acquire) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn parses_and_sorts_documented_depth() {
        let payload: Value = serde_json::json!({"bids":[{"price":79080,"size":0.023}],"asks":[{"price":79090,"size":0.06897}]});
        let book = parse_book(&payload).unwrap();
        assert_eq!(book.bids[0].price, dec!(79080));
        assert_eq!(book.asks[0].size, dec!(0.06897));
    }

    fn bulk_config() -> WsSessionConfig {
        WsSessionConfig {
            product: Product::Spot,
            market_address: "0xmarket".to_owned(),
            subaccount: "0xaccount".to_owned(),
            depth_aggregation: DEFAULT_DEPTH_AGGREGATION,
            reconnect_backoff: vec![],
        }
    }

    fn bulk_payload(sequence: u64, previous: Option<u64>, version: u64) -> Value {
        serde_json::json!({
            "topic":"bulk_orders:0xaccount",
            "orders":[{
                "asset_type":"spot",
                "market":"0xmarket",
                "sequence_number":sequence,
                "previous_seq_num":previous,
                "transaction_version":version,
                "transaction_unix_ms":1234,
                "event_uid":format!("event-{version}"),
                "status":"placed",
                "bid_prices":["100"],
                "bid_sizes":["2"],
                "ask_prices":["101"],
                "ask_sizes":["3"]
            }]
        })
    }

    fn apply_test(state: &WsStateHandle, config: &WsSessionConfig, payload: &Value) -> Result<()> {
        let (lifecycle, _) = lifecycle_channel();
        apply_payload(state, config, 0, payload, &lifecycle)
    }

    #[test]
    fn bulk_ladder_reducer_uses_versioned_contiguous_updates() {
        let config = bulk_config();
        let state = new_handle();
        state.write().unwrap().subscriptions_ready = true;
        apply_test(&state, &config, &bulk_payload(7, None, 10)).unwrap();
        let first = active_bulk_ladder(&state).unwrap().unwrap();
        assert_eq!(first.sequence, 7);
        assert_eq!(first.levels.len(), 2);
        assert_eq!(next_bulk_sequence(&state).unwrap(), 8);

        apply_test(&state, &config, &bulk_payload(8, Some(7), 11)).unwrap();
        assert_eq!(active_bulk_ladder(&state).unwrap().unwrap().sequence, 8);

        // A replay cannot replace a newer cached venue generation.
        apply_test(&state, &config, &bulk_payload(9, Some(8), 10)).unwrap();
        assert_eq!(active_bulk_ladder(&state).unwrap().unwrap().sequence, 8);
    }

    #[test]
    fn bulk_ladder_sequence_gap_blocks_new_execution() {
        let config = bulk_config();
        let state = new_handle();
        state.write().unwrap().subscriptions_ready = true;
        apply_test(&state, &config, &bulk_payload(7, None, 10)).unwrap();
        let error = apply_test(&state, &config, &bulk_payload(9, Some(5), 11))
            .unwrap_err()
            .to_string();
        assert!(error.contains("sequence gap"));
        assert!(active_bulk_ladder(&state).is_err());
    }

    #[test]
    fn rejected_bulk_ladder_does_not_replace_active_ladder() {
        let config = bulk_config();
        let state = new_handle();
        state.write().unwrap().subscriptions_ready = true;
        apply_test(&state, &config, &bulk_payload(7, None, 10)).unwrap();
        let mut rejected = bulk_payload(8, Some(7), 11);
        rejected["orders"][0]["status"] = Value::String("rejected".to_owned());
        apply_test(&state, &config, &rejected).unwrap();
        assert_eq!(active_bulk_ladder(&state).unwrap().unwrap().sequence, 7);
    }

    #[test]
    fn typed_orders_apply_terminal_updates_and_include_bulk_levels() {
        let config = bulk_config();
        let state = new_handle();
        state.write().unwrap().subscriptions_ready = true;
        apply_test(&state, &config, &bulk_payload(7, None, 10)).unwrap();
        let snapshot = serde_json::json!({
            "topic":"account_open_orders:0xaccount",
            "orders":[{
                "asset_type":"spot", "market":"0xmarket", "order_id":"standalone-1",
                "is_buy":true, "price":"99", "remaining_size":"1", "transaction_version":1
            }]
        });
        apply_test(&state, &config, &snapshot).unwrap();
        assert_eq!(actual_orders(&state).unwrap().len(), 3);

        let terminal = serde_json::json!({
            "topic":"order_updates:0xaccount",
            "order":{
                "asset_type":"spot", "market":"0xmarket", "order_id":"standalone-1",
                "is_buy":true, "price":"99", "remaining_size":"1", "transaction_version":2,
                "status":"cancelled"
            }
        });
        apply_test(&state, &config, &terminal).unwrap();
        let actual = actual_orders(&state).unwrap();
        assert_eq!(actual.len(), 2);
        assert!(
            actual
                .iter()
                .all(|order| order.origin == reconcile::OrderOrigin::Bulk)
        );
    }

    #[test]
    fn recovered_bulk_ladder_is_available_to_reconciliation() {
        let config = bulk_config();
        let source = new_handle();
        source.write().unwrap().subscriptions_ready = true;
        apply_test(&source, &config, &bulk_payload(7, None, 10)).unwrap();
        let active = active_bulk_ladder(&source).unwrap();

        let state = new_handle();
        state.write().unwrap().subscriptions_ready = true;
        apply_test(
            &state,
            &config,
            &serde_json::json!({"topic":"account_open_orders:0xaccount","orders":[]}),
        )
        .unwrap();
        recover_bulk_ladder(&state, active);

        assert_eq!(next_bulk_sequence(&state).unwrap(), 8);
        assert_eq!(actual_orders(&state).unwrap().len(), 2);
    }

    #[test]
    fn bulk_fill_reducer_attributes_once_and_reduces_executable_size() {
        let config = bulk_config();
        let state = new_handle();
        state.write().unwrap().subscriptions_ready = true;
        apply_test(&state, &config, &bulk_payload(7, None, 10)).unwrap();
        apply_test(
            &state,
            &config,
            &serde_json::json!({"topic":"account_open_orders:0xaccount","orders":[]}),
        )
        .unwrap();
        let fill = serde_json::json!({
            "topic":"bulk_order_fills:0xaccount",
            "fills":[{
                "asset_type":"spot", "market":"0xmarket", "event_uid":"event-fill",
                "trade_id":"trade-fill", "sequence_number":7, "is_bid":true,
                "price":"100", "filled_size":"1", "transaction_version":10
            }]
        });
        apply_test(&state, &config, &fill).unwrap();
        apply_test(&state, &config, &fill).unwrap();
        let bid = actual_orders(&state)
            .unwrap()
            .into_iter()
            .find(|order| order.side == Side::Bid)
            .unwrap();
        assert_eq!(bid.remaining_size, dec!(1));
    }

    #[test]
    fn perp_user_trade_preserves_accounting_fields() {
        let mut config = bulk_config();
        config.product = Product::Perp;
        let state = new_handle();
        state.write().unwrap().subscriptions_ready = true;
        let payload = serde_json::json!({
            "topic":"user_trades:0xaccount",
            "trades":[{
                "market":"0xmarket", "trade_id":"perp-trade-1", "action":"openlong",
                "price":"100", "size":"2", "fee_amount":"0.1",
                "realized_pnl_amount":"3", "realized_funding_amount":"-0.2",
                "transaction_unix_ms":1700000000000_i64, "transaction_version":42
            }]
        });
        apply_test(&state, &config, &payload).unwrap();
        let fill = perp_fills(&state).unwrap().pop().unwrap();
        assert_eq!(fill.id, "perp-trade-1");
        assert_eq!(fill.price, dec!(100));
        assert_eq!(fill.fee_quote, dec!(0.1));
        assert_eq!(fill.realized_pnl_quote, dec!(3));
        assert_eq!(fill.realized_funding_quote, dec!(-0.2));
    }
}
