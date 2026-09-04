//! Perp grid planning: uniform range levels, planning_price split, target derivation.

use anyhow::bail;
use rust_decimal::Decimal;

use super::risk::{compute_perp_target, perp_estimated_margin, perp_worst_case};
use crate::{
    Allocation, GridConfig, GridLevel, GridPlan, LevelState, MAX_LEVELS_PER_SIDE, Market,
    OutOfRangeAction, PerpMode, Product, RangeSpec, Result, Side, resolve_range, round_down,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutOfRangeDirection {
    BelowLower,
    AboveUpper,
}

#[derive(Clone, Debug)]
pub struct PerpRangeState {
    pub raw_planning_price: Decimal,
    pub effective_planning_price: Decimal,
    pub out_of_range: Option<OutOfRangeDirection>,
    pub action: OutOfRangeAction,
    pub skip_bulk: bool,
    pub paused_by_out_of_range: bool,
}

pub(crate) fn build_perp_plan(
    config: &GridConfig,
    market: &Market,
    planning_price: Decimal,
) -> Result<GridPlan> {
    debug_assert_eq!(config.product, Product::Perp);
    config.validate()?;
    if planning_price <= Decimal::ZERO {
        bail!("planning price must be positive")
    }

    let (lower, upper) = resolve_range(config, planning_price, config.total_count)?;
    let range_state = resolve_out_of_range(config, planning_price, lower, upper);
    let effective = range_state.effective_planning_price;

    if range_state.skip_bulk {
        return Ok(empty_perp_plan(planning_price, lower, upper, &range_state));
    }

    let (bid_prices, ask_prices) =
        split_uniform_levels(config, lower, upper, effective, market.tick_size)?;

    if bid_prices.len() > MAX_LEVELS_PER_SIDE || ask_prices.len() > MAX_LEVELS_PER_SIDE {
        bail!(
            "grid split exceeds per-side limit of {MAX_LEVELS_PER_SIDE}: {} bid(s), {} ask(s)",
            bid_prices.len(),
            ask_prices.len()
        )
    }

    let grid_size = derive_perp_grid_size(config, &bid_prices, &ask_prices, market)?;
    if grid_size < market.min_size {
        bail!(
            "derived grid size {grid_size} is below market minimum {}",
            market.min_size
        )
    }

    let ask_count = ask_prices.len();
    let bid_count = bid_prices.len();
    let target_position = compute_perp_target(config.perp_mode, ask_count, bid_count, grid_size);

    let bid_levels = bid_prices
        .into_iter()
        .map(|price| level(Side::Bid, price, grid_size))
        .collect::<Vec<_>>();
    let ask_levels = ask_prices
        .into_iter()
        .map(|price| level(Side::Ask, price, grid_size))
        .collect::<Vec<_>>();

    let mut plan = GridPlan {
        mid: effective,
        lower,
        upper,
        per_grid_base_size: Some(grid_size),
        bids: bid_levels,
        asks: ask_levels,
        quote_required: Decimal::ZERO,
        base_required: Decimal::ZERO,
        estimated_margin: None,
        planning_price: Some(effective),
        raw_planning_price: Some(planning_price),
        target_position: Some(target_position),
        worst_long: None,
        worst_short: None,
        paused_by_out_of_range: range_state.paused_by_out_of_range,
        out_of_range_action_applied: range_state
            .out_of_range
            .map(|_| format!("{:?}", range_state.action).to_lowercase()),
        convergence_delta: None,
        perp_blocked_reason: None,
    };

    refresh_perp_plan_metrics(&mut plan, config, Decimal::ZERO);
    Ok(plan)
}

pub(crate) fn refresh_perp_plan_metrics(
    plan: &mut GridPlan,
    config: &GridConfig,
    position: Decimal,
) {
    let (worst_long, worst_short) = perp_worst_case(position, plan);
    plan.worst_long = Some(worst_long);
    plan.worst_short = Some(worst_short);
    let planning = plan.planning_price.unwrap_or(plan.mid);
    plan.estimated_margin = Some(perp_estimated_margin(
        config,
        planning,
        worst_long,
        worst_short,
        plan,
    ));
    plan.quote_required = plan
        .bids
        .iter()
        .map(|level| level.notional * (Decimal::ONE + config.maker_fee_rate))
        .sum();
    plan.base_required = plan.asks.iter().map(|level| level.size).sum();
}

fn empty_perp_plan(
    raw_planning_price: Decimal,
    lower: Decimal,
    upper: Decimal,
    range_state: &PerpRangeState,
) -> GridPlan {
    GridPlan {
        mid: raw_planning_price,
        lower,
        upper,
        per_grid_base_size: None,
        bids: Vec::new(),
        asks: Vec::new(),
        quote_required: Decimal::ZERO,
        base_required: Decimal::ZERO,
        estimated_margin: None,
        planning_price: Some(raw_planning_price),
        raw_planning_price: Some(raw_planning_price),
        target_position: None,
        worst_long: Some(Decimal::ZERO),
        worst_short: Some(Decimal::ZERO),
        paused_by_out_of_range: range_state.paused_by_out_of_range,
        out_of_range_action_applied: range_state
            .out_of_range
            .map(|_| format!("{:?}", range_state.action).to_lowercase()),
        convergence_delta: None,
        perp_blocked_reason: None,
    }
}

pub(crate) fn resolve_out_of_range(
    config: &GridConfig,
    planning_price: Decimal,
    lower: Decimal,
    upper: Decimal,
) -> PerpRangeState {
    let out_of_range = if planning_price < lower {
        Some(OutOfRangeDirection::BelowLower)
    } else if planning_price > upper {
        Some(OutOfRangeDirection::AboveUpper)
    } else {
        None
    };
    let action = config.out_of_range_action;
    let (effective_planning_price, skip_bulk, paused_by_out_of_range) = match out_of_range {
        None => (planning_price, false, false),
        Some(_) => match action {
            OutOfRangeAction::ClampContinue => (planning_price.clamp(lower, upper), false, false),
            OutOfRangeAction::Pause => (planning_price, true, true),
            OutOfRangeAction::CancelOrders | OutOfRangeAction::ClosePosition => {
                (planning_price, true, false)
            }
        },
    };
    PerpRangeState {
        raw_planning_price: planning_price,
        effective_planning_price,
        out_of_range,
        action,
        skip_bulk,
        paused_by_out_of_range,
    }
}

pub(crate) fn uniform_range_prices(
    lower: Decimal,
    upper: Decimal,
    count: usize,
    tick: Decimal,
) -> Result<Vec<Decimal>> {
    if count == 0 {
        return Ok(Vec::new());
    }
    let span = upper - lower;
    if span <= Decimal::ZERO {
        bail!("grid lower bound must be below upper bound")
    }
    let mut prices = Vec::with_capacity(count);
    append_uniform_prices(&mut prices, lower, upper, count, tick);
    prices.sort();
    prices.dedup();
    if prices.is_empty() {
        bail!("grid range is too narrow for market tick size")
    }
    Ok(prices)
}

pub(crate) fn split_uniform_levels(
    config: &GridConfig,
    lower: Decimal,
    upper: Decimal,
    planning_price: Decimal,
    tick: Decimal,
) -> Result<(Vec<Decimal>, Vec<Decimal>)> {
    let prices = uniform_range_prices(lower, upper, config.total_count, tick)?;
    let mut bids = Vec::new();
    let mut asks = Vec::new();
    for price in prices {
        if close_to_tick(price, planning_price, tick) {
            continue;
        }
        if price < planning_price {
            if !asks.contains(&price) {
                bids.push(price);
            }
        } else if price > planning_price {
            if !bids.contains(&price) {
                asks.push(price);
            }
        }
    }
    bids.sort_by(|left, right| right.cmp(left));
    asks.sort();
    Ok((bids, asks))
}

fn derive_perp_grid_size(
    config: &GridConfig,
    bid_prices: &[Decimal],
    ask_prices: &[Decimal],
    market: &Market,
) -> Result<Decimal> {
    let size = match config.allocation {
        Allocation::FixedSize(value) => value,
        Allocation::TotalBudget(budget) => {
            let bid_notional: Decimal = bid_prices.iter().copied().sum();
            let ask_notional: Decimal = ask_prices.iter().copied().sum();
            let per_base = bid_notional.max(ask_notional) / config.preview_leverage
                + (bid_notional + ask_notional) * config.maker_fee_rate;
            if per_base <= Decimal::ZERO {
                bail!("cannot derive perp grid size from an empty level set")
            }
            budget / per_base
        }
    };
    let rounded = round_down(size, market.lot_size);
    if rounded <= Decimal::ZERO {
        bail!("grid size rounds to zero at this market lot size")
    }
    Ok(rounded)
}

fn level(side: Side, price: Decimal, size: Decimal) -> GridLevel {
    GridLevel {
        side,
        price,
        size,
        notional: price * size,
        state: LevelState::Planned,
    }
}

fn close_to_tick(left: Decimal, right: Decimal, tick: Decimal) -> bool {
    (left - right).abs() <= tick / Decimal::TWO
}

fn append_uniform_prices(
    prices: &mut Vec<Decimal>,
    lower: Decimal,
    upper: Decimal,
    count: usize,
    tick: Decimal,
) {
    let span = upper - lower;
    let denom = Decimal::from(count + 1);
    for i in 1..=count {
        let raw = lower + span * Decimal::from(i) / denom;
        prices.push(round_down(raw, tick));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GridConfig, SpotExecutionConfig};
    use rust_decimal_macros::dec;
    use std::time::Duration;

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
            base_asset_addr: None,
            quote_asset_addr: None,
            base_symbol: None,
            quote_symbol: None,
        }
    }

    fn config(perp_mode: PerpMode, total_count: usize) -> GridConfig {
        GridConfig {
            product: Product::Perp,
            perp_mode,
            market_name: "BTC/USD".to_owned(),
            range: RangeSpec::Bounds {
                lower: dec!(90),
                upper: dec!(110),
            },
            total_count,
            allocation: Allocation::FixedSize(dec!(0.01)),
            maker_fee_rate: dec!(0.0001),
            preview_leverage: dec!(1),
            refresh: Duration::from_secs(3),
            price_source: crate::PriceSource::Prices,
            spot: SpotExecutionConfig::default(),
            max_position: None,
            out_of_range_action: OutOfRangeAction::Pause,
        }
    }

    #[test]
    fn bilateral_long_places_bids_and_asks() {
        let plan = build_perp_plan(&config(PerpMode::Long, 4), &market(), dec!(100)).unwrap();
        assert!(!plan.bids.is_empty());
        assert!(!plan.asks.is_empty());
        assert_eq!(plan.target_position, Some(dec!(0.02)));
    }

    #[test]
    fn out_of_range_defaults_to_pause_without_levels() {
        let plan = build_perp_plan(&config(PerpMode::Neutral, 4), &market(), dec!(120)).unwrap();
        assert!(plan.bids.is_empty());
        assert!(plan.asks.is_empty());
        assert!(plan.paused_by_out_of_range);
    }
}
