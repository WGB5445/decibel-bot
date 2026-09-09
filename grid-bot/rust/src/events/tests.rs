use super::*;
use rust_decimal_macros::dec;

const MARKET: &str = "0x0000000abc";

#[test]
fn parses_mid_fixture_and_filters_product_and_market() {
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
    assert_eq!(events[0], SpotEvent::Mid(SpotMid {
        event_uid: "9007199254740993".to_owned(),
        market_addr: "0xabc".to_owned(),
        mid_price: dec!(12.345),
    }));
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
    assert_eq!(events, vec![SpotEvent::Depth(SpotDepth {
        event_uid: "depth-7".to_owned(),
        market_addr: "0xabc".to_owned(),
        bids: vec![SpotDepthLevel { price: dec!(10.0), size: dec!(2.5) }],
        asks: vec![SpotDepthLevel { price: dec!(10.5), size: dec!(1.25) }],
    })]);
}

#[test]
fn parses_fill_and_rejection_fixtures_from_nested_collections() {
    let fills = r#"
    {
      "topic": "bulk_order_fills:0xaccount",
      "data": { "fills": [
        {"event_uid": "fill-9", "asset_type": "spot", "market": "0xabc", "order_id": "order-1", "sequence_number": 42, "side": "buy", "fill_px": "10.25", "fill_sz": "3", "fee": "0.01", "timestamp_ms": 1710000000000},
        {"event_uid": "perp-fill", "asset_type": "perp", "market": "0xabc", "fill_px": "1", "fill_sz": "1"}
      ]}
    }"#;
    let orders = r#"
    {
      "topic": "bulk_orders:0xaccount",
      "orders": [
        {"event_uid": "reject-2", "asset_type": "spot", "market": "0xabc", "status": "rejected", "sequence_number": "43", "reason": "insufficient PFS funds"},
        {"event_uid": "accepted", "asset_type": "spot", "market": "0xabc", "status": "open"}
      ]
    }"#;
    let fill_events = parse_spot_events(fills, MARKET).unwrap();
    assert_eq!(fill_events, vec![SpotEvent::BulkFill(SpotBulkFill {
        event_uid: "fill-9".to_owned(), market_addr: "0xabc".to_owned(),
        order_id: Some("order-1".to_owned()), bulk_sequence_number: Some("42".to_owned()),
        side: Some("buy".to_owned()), price: dec!(10.25), size: dec!(3), fee: Some(dec!(0.01)),
        timestamp: Some("1710000000000".to_owned()),
    })]);
    let order_events = parse_spot_events(orders, MARKET).unwrap();
    assert_eq!(order_events, vec![SpotEvent::BulkOrderRejected(SpotBulkOrderRejected {
        event_uid: "reject-2".to_owned(), market_addr: "0xabc".to_owned(),
        order_id: None, bulk_sequence_number: Some("43".to_owned()),
        reason: "insufficient PFS funds".to_owned(), timestamp: None,
    })]);
}

#[test]
fn ignores_acknowledgements_and_unrelated_topics() {
    assert!(parse_spot_events(r#"{"success": true}"#, MARKET).unwrap().is_empty());
    assert!(parse_spot_events(r#"{"topic":"all_market_prices","data":[]}"#, MARKET).unwrap().is_empty());
}