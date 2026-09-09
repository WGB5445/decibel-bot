//! L0 offline scenario tests — zero network, embedded JSON fixtures.

use decibel_grid_tui::{
    Allocation, GridConfig, OutOfRangeAction, PerpMode, PriceSource, Product, RangeSpec,
    SpotExecutionConfig, build_plan, reconcile, simulation, strategy,
};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::Deserialize;
use std::time::Duration;

fn load_scenario(name: &str) -> simulation::Scenario {
    let raw = match name {
        "spot_pin_sweep" => include_str!("scenarios/spot_pin_sweep.json"),
        "spot_breakout_pause" => include_str!("scenarios/spot_breakout_pause.json"),
        "perp_neutral_mid50" => include_str!("scenarios/perp_neutral_mid50.json"),
        "perp_bilateral_long" => include_str!("scenarios/perp_bilateral_long.json"),
        "perp_max_position_block" => include_str!("scenarios/perp_max_position_block.json"),
        "reconcile_bulk_match" => include_str!("scenarios/reconcile_bulk_match.json"),
        other => panic!("unknown scenario fixture: {other}"),
    };
    simulation::parse_scenario(raw).expect("parse scenario fixture")
}

#[test]
fn spot_pin_mid_sweep() {
    load_scenario("spot_pin_sweep").run().expect("scenario");
}

#[test]
fn spot_breakout_pause() {
    load_scenario("spot_breakout_pause")
        .run()
        .expect("scenario");
}

#[test]
fn perp_modes_level_counts() {
    load_scenario("perp_neutral_mid50").run().expect("neutral");
    load_scenario("perp_bilateral_long").run().expect("long");
}

#[test]
fn perp_max_position_gate() {
    load_scenario("perp_max_position_block")
        .run()
        .expect("scenario");
}

#[derive(Deserialize)]
struct ReconcileFixture {
    #[serde(default)]
    reconcile: Option<ReconcileSection>,
}

#[derive(Deserialize)]
struct ReconcileSection {
    desired: Vec<ReconcileDesired>,
    actual: Vec<ReconcileActual>,
    expect: ReconcileExpect,
}

#[derive(Deserialize)]
struct ReconcileDesired {
    side: String,
    price: Decimal,
    size: Decimal,
}

#[derive(Deserialize)]
struct ReconcileActual {
    order_id: String,
    side: String,
    price: Decimal,
    remaining_size: Decimal,
    #[serde(default)]
    origin: String,
}

#[derive(Deserialize)]
struct ReconcileExpect {
    matched_count: usize,
    missing_count: usize,
    unmanaged_count: usize,
    reconcile_converged: bool,
}

#[test]
fn reconcile_converged() {
    let raw = include_str!("scenarios/reconcile_bulk_match.json");
    let fixture: ReconcileFixture =
        serde_json::from_str(raw).expect("parse reconcile fixture section");
    let section = fixture.reconcile.expect("reconcile section");
    let desired = section
        .desired
        .into_iter()
        .map(|row| reconcile::DesiredOrder {
            side: parse_side(&row.side),
            price: row.price,
            size: row.size,
        })
        .collect::<Vec<_>>();
    let actual = section
        .actual
        .into_iter()
        .map(|row| reconcile::ActualOrder {
            order_id: row.order_id,
            side: parse_side(&row.side),
            price: row.price,
            remaining_size: row.remaining_size,
            origin: if row.origin == "bulk" {
                reconcile::OrderOrigin::Bulk
            } else {
                reconcile::OrderOrigin::Standalone
            },
        })
        .collect::<Vec<_>>();
    let result = reconcile::reconcile(&desired, &actual, dec!(0.01), dec!(0.01));
    assert_eq!(result.matched.len(), section.expect.matched_count);
    assert_eq!(result.missing.len(), section.expect.missing_count);
    assert_eq!(result.unmanaged.len(), section.expect.unmanaged_count);
    assert_eq!(result.is_converged(), section.expect.reconcile_converged);

    load_scenario("reconcile_bulk_match")
        .run()
        .expect("planning portion of reconcile fixture");
}

fn parse_side(raw: &str) -> decibel_grid_tui::Side {
    match raw.to_ascii_lowercase().as_str() {
        "bid" => decibel_grid_tui::Side::Bid,
        "ask" => decibel_grid_tui::Side::Ask,
        other => panic!("unknown side: {other}"),
    }
}

#[test]
fn strategy_registry_spot_unchanged() {
    let config = GridConfig {
        product: Product::Spot,
        perp_mode: PerpMode::Neutral,
        market_name: "APT/USDC".to_owned(),
        range: RangeSpec::Percent { percent: dec!(10) },
        total_count: 8,
        allocation: Allocation::FixedSize(dec!(1)),
        maker_fee_rate: dec!(0.001),
        preview_leverage: dec!(1),
        refresh: Duration::from_secs(3),
        price_source: PriceSource::Prices,
        spot: SpotExecutionConfig::default(),
        max_position: None,
        out_of_range_action: OutOfRangeAction::default(),
    };
    let market = decibel_grid_tui::Market {
        address: "0xspot".to_owned(),
        name: "APT/USDC".to_owned(),
        tick_size: dec!(0.01),
        lot_size: dec!(0.01),
        min_size: dec!(0.01),
        px_decimals: 2,
        sz_decimals: 2,
        product: Product::Spot,
        base_asset_addr: None,
        quote_asset_addr: None,
        base_symbol: Some("APT".to_owned()),
        quote_symbol: Some("USDC".to_owned()),
    };
    let strategy = strategy::resolve(&config);
    assert_eq!(strategy.id(), "spot");
    let ctx = strategy::StrategyContext {
        mid: dec!(10),
        position: None,
        pinned_per_grid_base_size: None,
    };
    let plan = strategy.build_plan(&config, &market, &ctx).expect("plan");
    assert_eq!(plan.bids.len(), 4);
    assert_eq!(plan.asks.len(), 4);
    assert_eq!(plan.bids[0].price, dec!(9.75));
    assert_eq!(plan.asks[0].price, dec!(10.25));
    assert!(plan.bids.iter().all(|level| level.size == dec!(1)));
    assert!(plan.asks.iter().all(|level| level.size == dec!(1)));
    assert_eq!(plan.per_grid_base_size, Some(dec!(1)));

    let via_build_plan = build_plan(&config, &market, dec!(10)).expect("build_plan");
    assert_eq!(via_build_plan.bids.len(), plan.bids.len());
    assert_eq!(via_build_plan.asks[0].price, plan.asks[0].price);
}

#[test]
fn simulate_scenario_from_embedded_json() {
    let results =
        simulation::simulate_scenario_from_str(include_str!("scenarios/spot_pin_sweep.json"))
            .expect("simulate");
    assert_eq!(results.len(), 3);
}
