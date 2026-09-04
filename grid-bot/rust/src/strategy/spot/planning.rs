//! Spot grid plan construction — adapted from `build_plan_with_per_grid_base_size` without
//! algorithm changes.

use anyhow::bail;
use rust_decimal::Decimal;

use crate::strategy::StrategyContext;
use crate::{
    GridConfig, GridLevel, GridPlan, LevelState, Market, Product, Result, Side, derive_sizes,
    prices, resolve_range, round_down, side_counts,
};

pub fn build(config: &GridConfig, market: &Market, ctx: &StrategyContext) -> Result<GridPlan> {
    debug_assert_eq!(config.product, Product::Spot);
    config.validate()?;
    let mid = ctx.mid;
    if mid <= Decimal::ZERO {
        bail!("market mid price must be positive")
    }
    // Resolve the configured range first. For Spot, an already-pinned market may later trade
    // outside its bounds; clamp only the side-allocation reference used to build a snapshot.
    // The bounds and generated prices remain those of the configured range, while the live
    // execution loop projects the pinned plan against the out-of-range price.
    let (lower, upper) = resolve_range(config, mid, config.total_count)?;
    let allocation_mid = mid.clamp(lower, upper);
    let (bid_count, ask_count, bid_budget, ask_budget) =
        side_counts(config, lower, upper, allocation_mid);

    let bids = prices(
        config,
        Side::Bid,
        allocation_mid,
        lower,
        upper,
        bid_count,
        market.tick_size,
    )?;
    let asks = prices(
        config,
        Side::Ask,
        allocation_mid,
        lower,
        upper,
        ask_count,
        market.tick_size,
    )?;
    let (bid_size, ask_size) = if let Some(size) = ctx.pinned_per_grid_base_size {
        if round_down(size, market.lot_size) != size || size < market.min_size {
            bail!(
                "pinned per_grid_base_size {size} is not aligned to lot {} or below min size {}",
                market.lot_size,
                market.min_size
            )
        }
        (size, size)
    } else {
        derive_sizes(config, &bids, &asks, market, bid_budget, ask_budget)?
    };

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
    let plan = GridPlan {
        mid,
        lower,
        upper,
        per_grid_base_size: Some(bid_size),
        bids: bid_levels,
        asks: ask_levels,
        quote_required,
        base_required,
        estimated_margin: None,
        ..Default::default()
    };
    plan.enforce_min_net_margin(config.maker_fee_rate, config.spot.min_net_margin_bps)?;
    plan.enforce_spot_budget(config)?;
    Ok(plan)
}
