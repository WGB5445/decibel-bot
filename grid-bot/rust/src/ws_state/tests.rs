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
    assert!(actual.iter().all(|order| order.origin == reconcile::OrderOrigin::Bulk));
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