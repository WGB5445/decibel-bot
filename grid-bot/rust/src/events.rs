//! Persistent Decibel Spot WebSocket event listener.
//!
//! This module deliberately parses WebSocket payloads through `serde_json::Value` rather than a
//! `f64`-based intermediate representation.  In particular, Decibel's `event_uid` is retained as
//! a [`String`], so a UID larger than JavaScript's safe-integer range cannot be rounded while it
//! is being routed to the grid loop.

use std::{
    str::FromStr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Result, bail};
use futures_util::{SinkExt, StreamExt};
use rust_decimal::Decimal;
use serde_json::Value;
use tokio::{
    sync::mpsc::Sender,
    task::JoinHandle,
    time::{Instant, sleep},
};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message, client::IntoClientRequest, http::HeaderValue},
};

use crate::{DecibelClient, Product};

/// Reconnect shortly before the venue's one-hour WebSocket connection limit.
const MAX_CONNECTION_AGE: Duration = Duration::from_secs(59 * 60 + 55);
/// An atomic flag has no notification primitive, so use a small bounded wait whenever a task must
/// remain interruptible by shutdown (including while a channel receiver is temporarily slow).
const SHUTDOWN_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// A Spot mid-price update from `all_spot_mids`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpotMid {
    /// Decibel event identity, preserved exactly rather than parsed as a floating point number.
    pub event_uid: String,
    pub market_addr: String,
    pub mid_price: Decimal,
}

/// A single bid or ask in a Spot depth update.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpotDepthLevel {
    pub price: Decimal,
    pub size: Decimal,
}

/// A Spot order-book update from `depth:{market}:1`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpotDepth {
    /// Decibel event identity, preserved exactly rather than parsed as a floating point number.
    pub event_uid: String,
    pub market_addr: String,
    pub bids: Vec<SpotDepthLevel>,
    pub asks: Vec<SpotDepthLevel>,
}

/// A Spot fill notification from `bulk_order_fills:{subaccount}`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpotBulkFill {
    /// Decibel event identity, preserved exactly rather than parsed as a floating point number.
    pub event_uid: String,
    pub market_addr: String,
    pub order_id: Option<String>,
    /// The venue's bulk-order sequence number, when included in the event.
    pub bulk_sequence_number: Option<String>,
    pub side: Option<String>,
    pub price: Decimal,
    pub size: Decimal,
    pub fee: Option<Decimal>,
    pub timestamp: Option<String>,
}

/// A rejected Spot bulk-order notification from `bulk_orders:{subaccount}`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpotBulkOrderRejected {
    /// Decibel event identity, preserved exactly rather than parsed as a floating point number.
    pub event_uid: String,
    pub market_addr: String,
    pub order_id: Option<String>,
    /// The venue's bulk-order sequence number, when included in the event.
    pub bulk_sequence_number: Option<String>,
    pub reason: String,
    pub timestamp: Option<String>,
}

/// Metadata emitted after a successfully subscribed replacement connection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpotReconnected {
    /// Number of reconnects since this listener was spawned. The first such event has value `1`.
    pub reconnect_count: u64,
}

/// Typed notifications emitted by [`spawn_spot_event_listener`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SpotEvent {
    Mid(SpotMid),
    Depth(SpotDepth),
    BulkFill(SpotBulkFill),
    BulkOrderRejected(SpotBulkOrderRejected),
    Reconnected(SpotReconnected),
}

/// Spawn a persistent authenticated Decibel Spot event listener.
///
/// Each connection subscribes to the four Spot topics needed by a grid loop. A connection is
/// intentionally recycled before it is one hour old. Failed connections and normal disconnects
/// use the caller-provided `backoff` schedule: values are consumed in order and the final value is
/// reused for later attempts. An empty schedule means immediate retry (with a Tokio yield).
///
/// The task stops when `shutdown` is set or the receiver side of `sender` is dropped. Connection
/// errors are deliberately retried rather than surfaced through the `JoinHandle`; consumers should
/// use [`SpotEvent::Reconnected`] to resynchronise any state after a replacement connection.
pub fn spawn_spot_event_listener(
    client: DecibelClient,
    market_addr: String,
    subaccount: String,
    backoff: Vec<Duration>,
    sender: Sender<SpotEvent>,
    shutdown: Arc<AtomicBool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        run_spot_event_listener(client, market_addr, subaccount, backoff, sender, shutdown).await;
    })
}

async fn run_spot_event_listener(
    client: DecibelClient,
    market_addr: String,
    subaccount: String,
    backoff: Vec<Duration>,
    sender: Sender<SpotEvent>,
    shutdown: Arc<AtomicBool>,
) {
    let topics = [
        "all_spot_mids".to_owned(),
        format!("depth:{market_addr}:1"),
        format!("bulk_order_fills:{subaccount}"),
        format!("bulk_orders:{subaccount}"),
    ];
    let mut reconnect_count = 0_u64;
    let mut connected_before = false;
    let mut failed_attempts = 0_usize;

    while !shutdown.load(Ordering::Acquire) && !sender.is_closed() {
        let session_result = run_connection(
            &client,
            &topics,
            &market_addr,
            &sender,
            &shutdown,
            connected_before.then_some(reconnect_count.saturating_add(1)),
        )
        .await;

        if shutdown.load(Ordering::Acquire) || sender.is_closed() {
            return;
        }

        // A successful subscription is enough to establish a connection. It is possible for that
        // connection to close before its first data message; it still counts as a reconnect and
        // has already delivered the Reconnected marker to the consumer.
        if session_result.subscribed {
            if connected_before {
                reconnect_count = reconnect_count.saturating_add(1);
            } else {
                connected_before = true;
            }
            failed_attempts = 0;
        } else {
            failed_attempts = failed_attempts.saturating_add(1);
        }

        let delay = reconnect_backoff(&backoff, failed_attempts);
        if !sleep_or_shutdown(delay, &shutdown).await {
            return;
        }
    }
}

/// Result details intentionally kept local: all connection failures retry, while `subscribed`
/// distinguishes a failed handshake/subscribe from an established connection that later closed.
struct ConnectionResult {
    subscribed: bool,
}

async fn run_connection(
    client: &DecibelClient,
    topics: &[String; 4],
    market_addr: &str,
    sender: &Sender<SpotEvent>,
    shutdown: &Arc<AtomicBool>,
    reconnect_marker: Option<u64>,
) -> ConnectionResult {
    let mut subscribed = false;
    let result: Result<()> = async {
        let mut request = client.ws_url.clone().into_client_request()?;
        request.headers_mut().insert(
            "Sec-WebSocket-Protocol",
            HeaderValue::from_str(&format!("decibel, {}", client.api_key))?,
        );

        let (mut socket, _) = connect_async(request).await?;
        for topic in topics {
            socket
                .send(Message::Text(
                    serde_json::json!({ "method": "subscribe", "topic": topic })
                        .to_string()
                        .into(),
                ))
                .await?;
        }

        subscribed = true;

        if let Some(reconnect_count) = reconnect_marker
            && !send_event(
                sender,
                SpotEvent::Reconnected(SpotReconnected { reconnect_count }),
                shutdown,
            )
            .await
        {
            bail!("Spot event receiver was dropped or listener was shut down")
        }

        listen_to_connection(&mut socket, market_addr, sender, shutdown).await
    }
    .await;

    let _ = result;
    ConnectionResult {
        // `run_connection` reports a connection as subscribed only if setup reached the receive
        // loop. Sending a reconnect marker happens after all four subscriptions were written.
        subscribed,
    }
}

async fn listen_to_connection<S>(
    socket: &mut S,
    market_addr: &str,
    sender: &Sender<SpotEvent>,
    shutdown: &Arc<AtomicBool>,
) -> Result<()>
where
    S: futures_util::Sink<Message, Error = tokio_tungstenite::tungstenite::Error>
        + futures_util::Stream<
            Item = std::result::Result<Message, tokio_tungstenite::tungstenite::Error>,
        > + Unpin,
{
    let reconnect_at = Instant::now() + MAX_CONNECTION_AGE;

    loop {
        if shutdown.load(Ordering::Acquire) || sender.is_closed() {
            return Ok(());
        }
        if Instant::now() >= reconnect_at {
            // Dropping this socket in the caller forces a fresh authenticated connection before
            // the server-side one-hour limit.
            return Ok(());
        }

        let until_poll = reconnect_at
            .saturating_duration_since(Instant::now())
            .min(SHUTDOWN_POLL_INTERVAL);
        tokio::select! {
            _ = sleep(until_poll) => continue,
            next = socket.next() => {
                let message = match next {
                    Some(message) => message?,
                    None => return Ok(()),
                };
                match message {
                    Message::Text(text) => {
                        let payload: Value = serde_json::from_str(&text)?;
                        if payload.get("success") == Some(&Value::Bool(false)) {
                            bail!("Decibel WebSocket subscription failed: {payload}")
                        }
                        for event in parse_spot_event_value(&payload, market_addr)? {
                            if !send_event(sender, event, shutdown).await {
                                return Ok(());
                            }
                        }
                    }
                    Message::Ping(payload) => socket.send(Message::Pong(payload)).await?,
                    Message::Close(_) => return Ok(()),
                    Message::Binary(_) | Message::Pong(_) | Message::Frame(_) => {}
                }
            }
        }
    }
}

async fn send_event(
    sender: &Sender<SpotEvent>,
    event: SpotEvent,
    shutdown: &Arc<AtomicBool>,
) -> bool {
    tokio::select! {
        result = sender.send(event) => result.is_ok(),
        _ = wait_for_shutdown(shutdown) => false,
    }
}

async fn sleep_or_shutdown(delay: Duration, shutdown: &Arc<AtomicBool>) -> bool {
    if delay.is_zero() {
        tokio::task::yield_now().await;
        return !shutdown.load(Ordering::Acquire);
    }
    tokio::select! {
        _ = sleep(delay) => !shutdown.load(Ordering::Acquire),
        _ = wait_for_shutdown(shutdown) => false,
    }
}

async fn wait_for_shutdown(shutdown: &Arc<AtomicBool>) {
    while !shutdown.load(Ordering::Acquire) {
        sleep(SHUTDOWN_POLL_INTERVAL).await;
    }
}

fn reconnect_backoff(backoff: &[Duration], failed_attempts: usize) -> Duration {
    backoff
        .get(failed_attempts.saturating_sub(1))
        .copied()
        .or_else(|| backoff.last().copied())
        .unwrap_or(Duration::ZERO)
}

/// Parse a Decibel WebSocket JSON text frame into Spot events for `market_addr`.
///
/// Unknown topics, subscription acknowledgements, and records for another product or market are
/// intentionally returned as an empty vector. Malformed JSON is returned as an error; malformed
/// individual records are ignored so one bad record does not interrupt the persistent listener.
pub fn parse_spot_events(text: &str, market_addr: &str) -> Result<Vec<SpotEvent>> {
    let payload: Value = serde_json::from_str(text)?;
    parse_spot_event_value(&payload, market_addr)
}

/// Value-based form of [`parse_spot_events`], useful when a caller already decoded a WebSocket
/// text frame.
pub fn parse_spot_event_value(payload: &Value, market_addr: &str) -> Result<Vec<SpotEvent>> {
    let Some(topic) = payload.get("topic").and_then(Value::as_str) else {
        return Ok(Vec::new());
    };

    if topic == "all_spot_mids" {
        return Ok(event_records(payload, &["mids", "prices"])
            .into_iter()
            .filter_map(|record| parse_mid(record, payload, market_addr))
            .map(SpotEvent::Mid)
            .collect());
    }
    if topic.starts_with("depth:") {
        return Ok(event_records(payload, &["depth"])
            .into_iter()
            .filter_map(|record| parse_depth(record, payload, market_addr))
            .map(SpotEvent::Depth)
            .collect());
    }
    if topic.starts_with("bulk_order_fills:") {
        return Ok(event_records(payload, &["fills"])
            .into_iter()
            .filter_map(|record| parse_bulk_fill(record, payload, market_addr))
            .map(SpotEvent::BulkFill)
            .collect());
    }
    if topic.starts_with("bulk_orders:") {
        return Ok(event_records(payload, &["orders"])
            .into_iter()
            .filter_map(|record| parse_bulk_order_rejected(record, payload, market_addr))
            .map(SpotEvent::BulkOrderRejected)
            .collect());
    }

    Ok(Vec::new())
}

fn event_records<'a>(payload: &'a Value, collection_names: &[&str]) -> Vec<&'a Value> {
    if let Some(records) = collection_records(payload, collection_names) {
        return records;
    }

    match payload.get("data") {
        Some(Value::Array(records)) => records.iter().collect(),
        Some(Value::Object(_)) => {
            let data = &payload["data"];
            collection_records(data, collection_names).unwrap_or_else(|| vec![data])
        }
        _ => vec![payload],
    }
}

fn collection_records<'a>(
    container: &'a Value,
    collection_names: &[&str],
) -> Option<Vec<&'a Value>> {
    for name in collection_names {
        let Some(value) = container.get(*name) else {
            continue;
        };
        return Some(match value {
            Value::Array(records) => records.iter().collect(),
            Value::Object(_) => vec![value],
            _ => Vec::new(),
        });
    }
    None
}

fn parse_mid(record: &Value, envelope: &Value, market_addr: &str) -> Option<SpotMid> {
    require_spot_market(record, envelope, market_addr)?;
    Some(SpotMid {
        event_uid: event_uid(record, envelope)?,
        market_addr: market(record, envelope)?.to_owned(),
        mid_price: decimal_field(record, envelope, &["mid_px", "mid_price", "mid"])?,
    })
}

fn parse_depth(record: &Value, envelope: &Value, market_addr: &str) -> Option<SpotDepth> {
    require_spot_market(record, envelope, market_addr)?;
    let bids = depth_levels(value_field(record, envelope, &["bids"])?)?;
    let asks = depth_levels(value_field(record, envelope, &["asks"])?)?;
    Some(SpotDepth {
        event_uid: event_uid(record, envelope)?,
        market_addr: market(record, envelope)?.to_owned(),
        bids,
        asks,
    })
}

fn parse_bulk_fill(record: &Value, envelope: &Value, market_addr: &str) -> Option<SpotBulkFill> {
    require_spot_market(record, envelope, market_addr)?;
    Some(SpotBulkFill {
        event_uid: event_uid(record, envelope)?,
        market_addr: market(record, envelope)?.to_owned(),
        order_id: string_field(record, envelope, &["order_id", "client_order_id"]),
        bulk_sequence_number: string_field(
            record,
            envelope,
            &["bulk_sequence_number", "sequence_number"],
        ),
        side: string_field(record, envelope, &["side"]),
        price: decimal_field(record, envelope, &["fill_px", "filled_px", "price", "px"])?,
        size: decimal_field(
            record,
            envelope,
            &["fill_sz", "filled_sz", "filled_size", "size", "sz"],
        )?,
        fee: decimal_field(record, envelope, &["fee", "fee_paid"]),
        timestamp: string_field(record, envelope, &["timestamp", "timestamp_ms", "time"]),
    })
}

fn parse_bulk_order_rejected(
    record: &Value,
    envelope: &Value,
    market_addr: &str,
) -> Option<SpotBulkOrderRejected> {
    require_spot_market(record, envelope, market_addr)?;
    if !is_rejection(record, envelope) {
        return None;
    }
    Some(SpotBulkOrderRejected {
        event_uid: event_uid(record, envelope)?,
        market_addr: market(record, envelope)?.to_owned(),
        order_id: string_field(record, envelope, &["order_id", "client_order_id"]),
        bulk_sequence_number: string_field(
            record,
            envelope,
            &["bulk_sequence_number", "sequence_number"],
        ),
        reason: string_field(
            record,
            envelope,
            &["rejection_reason", "reason", "error", "message"],
        )
        .unwrap_or_else(|| "rejected".to_owned()),
        timestamp: string_field(record, envelope, &["timestamp", "timestamp_ms", "time"]),
    })
}

/// Enforce the two routing constraints on every individual record, including records nested in a
/// `data`/`fills`/`orders` envelope. `Product` is used rather than a boolean so this remains
/// explicit if the crate later grows additional product types.
fn require_spot_market(record: &Value, envelope: &Value, target_market: &str) -> Option<()> {
    let product = string_field(record, envelope, &["asset_type"])?;
    let product = if product.eq_ignore_ascii_case("spot") {
        Product::Spot
    } else {
        Product::Perp
    };
    if product != Product::Spot {
        return None;
    }

    let event_market = market(record, envelope)?;
    same_market(event_market, target_market).then_some(())
}

fn is_rejection(record: &Value, envelope: &Value) -> bool {
    let Some(value) = string_field(record, envelope, &["status", "event_type", "type"]) else {
        return false;
    };
    matches!(
        value.to_ascii_lowercase().as_str(),
        "rejected" | "bulk_order_rejected" | "bulk_order_rejection"
    )
}

fn event_uid<'a>(record: &'a Value, envelope: &'a Value) -> Option<String> {
    // `Value::Number::to_string` preserves serde_json's integer representation. No f64 is used
    // here, so a large numeric UID remains exact before becoming its public String form.
    string_field(record, envelope, &["event_uid"])
}

fn market<'a>(record: &'a Value, envelope: &'a Value) -> Option<&'a str> {
    value_field(record, envelope, &["market", "market_addr"]).and_then(Value::as_str)
}

fn string_field(record: &Value, envelope: &Value, names: &[&str]) -> Option<String> {
    value_field(record, envelope, names).and_then(string_value)
}

fn value_field<'a>(record: &'a Value, envelope: &'a Value, names: &[&str]) -> Option<&'a Value> {
    // Prefer every field on an individual record before consulting its envelope. This prevents an
    // envelope's `market` from shadowing a record's `market_addr` (or equivalent aliases).
    for name in names {
        if let Some(value) = record.get(*name) {
            return Some(value);
        }
    }
    for name in names {
        if let Some(value) = envelope.get(*name) {
            return Some(value);
        }
    }
    None
}

fn string_value(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.to_owned()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

fn decimal_field(record: &Value, envelope: &Value, names: &[&str]) -> Option<Decimal> {
    value_field(record, envelope, names).and_then(decimal_value)
}

fn decimal_value(value: &Value) -> Option<Decimal> {
    match value {
        Value::String(value) => Decimal::from_str(value).ok(),
        Value::Number(value) => Decimal::from_str(&value.to_string()).ok(),
        _ => None,
    }
}

fn depth_levels(value: &Value) -> Option<Vec<SpotDepthLevel>> {
    value
        .as_array()?
        .iter()
        .map(|level| match level {
            Value::Object(_) => Some(SpotDepthLevel {
                price: value_field(level, level, &["price", "px"]).and_then(decimal_value)?,
                size: value_field(level, level, &["size", "sz"]).and_then(decimal_value)?,
            }),
            Value::Array(pair) if pair.len() >= 2 => Some(SpotDepthLevel {
                price: decimal_value(&pair[0])?,
                size: decimal_value(&pair[1])?,
            }),
            _ => None,
        })
        .collect()
}

fn same_market(left: &str, right: &str) -> bool {
    normalize_market(left) == normalize_market(right)
}

fn normalize_market(value: &str) -> String {
    value
        .trim()
        .strip_prefix("0x")
        .or_else(|| value.trim().strip_prefix("0X"))
        .unwrap_or(value.trim())
        .trim_start_matches('0')
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    const MARKET: &str = "0x0000000abc";

    #[test]
    fn parses_mid_fixture_and_filters_product_and_market() {
        // The numeric UID intentionally exceeds 2^53. It must not make a round trip through f64.
        let fixture = r#"
        {
          "topic": "all_spot_mids",
          "mids": [
            {"event_uid": 9007199254740993, "asset_type": "spot", "market": "0xabc", "mid_px": "12.345"},
            {"event_uid": "perp", "asset_type": "perp", "market": "0xabc", "mid_px": "99"},
            {"event_uid": "other", "asset_type": "spot", "market": "0xdef", "mid_px": "88"}
          ]
        }"#;

        let events = parse_spot_events(fixture, MARKET).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0],
            SpotEvent::Mid(SpotMid {
                event_uid: "9007199254740993".to_owned(),
                market_addr: "0xabc".to_owned(),
                mid_price: dec!(12.345),
            })
        );
    }

    #[test]
    fn parses_depth_fixture_with_object_and_tuple_levels() {
        let fixture = r#"
        {
          "topic": "depth:0xabc:1",
          "data": {
            "event_uid": "depth-7",
            "asset_type": "spot",
            "market_addr": "0xabc",
            "bids": [{"price": "10.0", "size": "2.5"}],
            "asks": [["10.5", "1.25"]]
          }
        }"#;

        let events = parse_spot_events(fixture, MARKET).unwrap();
        assert_eq!(
            events,
            vec![SpotEvent::Depth(SpotDepth {
                event_uid: "depth-7".to_owned(),
                market_addr: "0xabc".to_owned(),
                bids: vec![SpotDepthLevel {
                    price: dec!(10.0),
                    size: dec!(2.5),
                }],
                asks: vec![SpotDepthLevel {
                    price: dec!(10.5),
                    size: dec!(1.25),
                }],
            })]
        );
    }

    #[test]
    fn parses_fill_and_rejection_fixtures_from_nested_collections() {
        let fills = r#"
        {
          "topic": "bulk_order_fills:0xaccount",
          "data": {
            "fills": [
              {
                "event_uid": "fill-9", "asset_type": "spot", "market": "0xabc",
                "order_id": "order-1", "sequence_number": 42, "side": "buy",
                "fill_px": "10.25", "fill_sz": "3", "fee": "0.01", "timestamp_ms": 1710000000000
              },
              {
                "event_uid": "perp-fill", "asset_type": "perp", "market": "0xabc",
                "fill_px": "1", "fill_sz": "1"
              }
            ]
          }
        }"#;
        let orders = r#"
        {
          "topic": "bulk_orders:0xaccount",
          "orders": [
            {
              "event_uid": "reject-2", "asset_type": "spot", "market": "0xabc",
              "status": "rejected", "sequence_number": "43", "reason": "insufficient PFS funds"
            },
            {
              "event_uid": "accepted", "asset_type": "spot", "market": "0xabc", "status": "open"
            }
          ]
        }"#;

        let fill_events = parse_spot_events(fills, MARKET).unwrap();
        assert_eq!(
            fill_events,
            vec![SpotEvent::BulkFill(SpotBulkFill {
                event_uid: "fill-9".to_owned(),
                market_addr: "0xabc".to_owned(),
                order_id: Some("order-1".to_owned()),
                bulk_sequence_number: Some("42".to_owned()),
                side: Some("buy".to_owned()),
                price: dec!(10.25),
                size: dec!(3),
                fee: Some(dec!(0.01)),
                timestamp: Some("1710000000000".to_owned()),
            })]
        );

        let order_events = parse_spot_events(orders, MARKET).unwrap();
        assert_eq!(
            order_events,
            vec![SpotEvent::BulkOrderRejected(SpotBulkOrderRejected {
                event_uid: "reject-2".to_owned(),
                market_addr: "0xabc".to_owned(),
                order_id: None,
                bulk_sequence_number: Some("43".to_owned()),
                reason: "insufficient PFS funds".to_owned(),
                timestamp: None,
            })]
        );
    }

    #[test]
    fn ignores_acknowledgements_and_unrelated_topics() {
        assert!(
            parse_spot_events(r#"{"success": true}"#, MARKET)
                .unwrap()
                .is_empty()
        );
        assert!(
            parse_spot_events(r#"{"topic":"all_market_prices","data":[]}"#, MARKET)
                .unwrap()
                .is_empty()
        );
    }
}
