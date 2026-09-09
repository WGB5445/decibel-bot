use super::*;
use crate::control::{EngineEvent, PerpPnlStatus};
use chrono::Utc;

fn level(side: &str, price: &str) -> LadderLevel {
    LadderLevel { side: side.into(), price: price.into(), size: "2".into(), state: "Placed".into() }
}

#[test]
fn snapshot_includes_perp_fields_when_present() {
    let status = EngineStatus {
        perp_mode: Some("long".to_owned()),
        max_position: Some("0.01".to_owned()),
        position: Some("0.002".to_owned()),
        available_margin: Some("120".to_owned()),
        estimated_margin: Some("500".to_owned()),
        ..EngineStatus::default()
    };
    let snapshot = snapshot_plain_text(&status, true, 120);
    assert!(snapshot.contains("Perp: LONG"));
    assert!(snapshot.contains("pos=0.002"));
    assert!(snapshot.contains("max=0.01"));
    assert!(snapshot.contains("margin=120/500 USDC"));
}

#[test]
fn snapshot_labels_incomplete_perp_pnl_without_fabricating_a_net_value() {
    let status = EngineStatus {
        perp_mode: Some("neutral".to_owned()),
        perp_pnl: Some(PerpPnlStatus {
            exchange_position_base: "1".to_owned(),
            ledger_position_base: "1".to_owned(),
            reconciliation_delta_base: "0".to_owned(),
            average_entry_price: Some("100".to_owned()),
            mark_price: Some("105".to_owned()),
            unrealized_gross_quote: Some("5".to_owned()),
            realized_gross_quote: "2".to_owned(),
            trade_fees_quote: Some("1".to_owned()),
            funding_pnl_quote: None,
            net_pnl_quote: None,
            fees_complete: true,
            funding_complete: false,
            last_fill_at: None,
            last_funding_at: None,
        }),
        ..EngineStatus::default()
    };
    let snapshot = snapshot_plain_text(&status, true, 120);
    assert!(snapshot.contains("PERP ACCOUNTING"));
    assert!(snapshot.contains("gross realized/unrealized: 2/5"));
    assert!(snapshot.contains("net: unavailable"));
    assert!(snapshot.contains("funding=unavailable"));
}

#[test]
fn snapshot_contains_all_ladder_and_event_rows() {
    let status = EngineStatus {
        ladder: vec![level("BID", "1.0"), level("ASK", "1.1")],
        events: vec![EngineEvent { at: Utc::now(), message: "reconciled".into() }],
        ..EngineStatus::default()
    };
    let snapshot = snapshot_plain_text(&status, true, 120);
    assert!(snapshot.contains("BID"));
    assert!(snapshot.contains("ASK"));
    assert!(snapshot.contains("reconciled"));
}

#[test]
fn snapshot_shows_only_the_ten_latest_events() {
    let status = EngineStatus {
        events: (0..=10).map(|i| EngineEvent { at: Utc::now(), message: format!("event-{i:02}") }).collect(),
        ..EngineStatus::default()
    };
    let snapshot = snapshot_plain_text(&status, true, 120);
    assert!(snapshot.contains("EVENTS (latest 10 / 11)"));
    assert!(snapshot.contains("event-10"));
    assert!(snapshot.contains("event-01"));
    assert!(!snapshot.contains("event-00"));
}

#[test]
fn grid_levels_are_bid_then_ask_with_each_side_ordered() {
    let levels = vec![level("ASK", "1.20"), level("BID", "1.10"), level("ASK", "1.15"), level("BID", "1.05")];
    let ordered = ordered_levels(&levels);
    let prices: Vec<_> = ordered.iter().map(|l| format!("{}:{}", l.side, l.price)).collect();
    assert_eq!(prices, ["BID:1.05", "BID:1.10", "ASK:1.15", "ASK:1.20"]);
}

#[test]
fn formats_quantities_and_prices_at_fixed_precision() {
    assert_eq!(format_decimal("50.68", 6), "50.680000");
    assert_eq!(format_decimal("0.5559", 8), "0.55590000");
}

#[test]
fn page_step_is_a_full_viewport() {
    assert_eq!(page_step(1), 1);
    assert_eq!(page_step(24), 14);
}

#[test]
fn split_snapshot_omits_events_when_requested() {
    let status = EngineStatus {
        events: vec![EngineEvent { at: Utc::now(), message: "reconciled".into() }],
        ..EngineStatus::default()
    };
    let with_events = snapshot_plain_text(&status, true, 120);
    let without_events = snapshot_plain_text(&status, false, 120);
    assert!(with_events.contains("EVENTS"));
    assert!(with_events.contains("reconciled"));
    assert!(!without_events.contains("EVENTS"));
    assert!(!without_events.contains("reconciled"));
}

#[test]
fn ladder_columns_for_width_allocates_flex_space() {
    let narrow = ladder_columns_for_width(30);
    assert!(narrow.total() <= 30);
    let wide = ladder_columns_for_width(120);
    assert!(wide.price >= 6);
    assert!(wide.size >= 6);
    assert_eq!(wide.side, 4);
    assert_eq!(wide.status, 8);
}

#[test]
fn resting_ladder_levels_display_as_active() {
    assert_eq!(display_state("Resting"), "Active");
    assert_eq!(display_state("planned"), "Planned");
}

#[test]
fn long_error_is_expanded_into_scrollable_snapshot_rows() {
    let error = "simulation failed because the market-order transaction was rejected by the gas station";
    let status = EngineStatus { last_error: Some(error.to_owned()), ..EngineStatus::default() };
    let lines = snapshot_lines(&status, false, 72, &[]);
    let header = lines.iter().position(|line| line.to_string() == "Last engine error:").expect("error header");
    assert!(lines.len() > header + 2);
    assert!(lines[header + 1].to_string().starts_with("simulation failed"));
}

#[test]
fn perp_main_viewport_accounts_for_header_and_borders() {
    assert_eq!(main_viewport_height(24, false), 14);
    assert_eq!(main_viewport_height(24, true), 13);
}

#[test]
fn manual_scroll_stays_clamped_and_following_resumes_at_bottom() {
    let mut app = App::default();
    app.update_scroll_bounds(5);
    assert_eq!(app.scroll, 0);
    assert!(!app.follow_latest);
    app.scroll_down(2);
    assert_eq!(app.scroll, 2);
    assert!(!app.follow_latest);
    app.update_scroll_bounds(8);
    assert_eq!(app.scroll, 2);
    assert!(!app.follow_latest);
    app.scroll_down(usize::MAX);
    assert_eq!(app.scroll, 8);
    assert!(app.follow_latest);
    app.update_scroll_bounds(11);
    assert_eq!(app.scroll, 11);
    assert!(app.follow_latest);
    app.scroll_up(1);
    assert_eq!(app.scroll, 10);
    assert!(!app.follow_latest);
}