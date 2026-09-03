pub mod neutral;
pub mod long;
pub mod short;
pub mod runtime;

use anyhow::bail;
use rust_decimal::Decimal;

use crate::{
    GridConfig, GridLevel, GridPlan, LevelState, Market, PerpMode, Product, Result, Side,
    derive_sizes, prices, resolve_range, side_counts,
};
use super::StrategyContext;

pub(crate) fn build_perp_plan(
    config: &GridConfig,
    market: &Market,
    ctx: &StrategyContext,
) -> Result<GridPlan> {
    debug_assert_eq!(config.product, Product::Perp);
    config.validate()?;
    let mid = ctx.mid;
    if mid <= Decimal::ZERO {
        bail!("market mid price must be positive")
    }
    let (lower, upper) = resolve_range(config, mid, config.total_count)?;
    if !(lower < mid && mid < upper) {
        bail!("mid price {mid} is outside grid range [{lower}, {upper}]")
    }
    let (bid_count, ask_count, bid_budget, ask_budget) =
        side_counts(config, lower, upper, mid);

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
    let (bid_size, ask_size) =
        derive_sizes(config, &bids, &asks, market, bid_budget, ask_budget)?;

    let mut bid_levels = bids
        .into_iter()
        .map(|price| GridLevel {
            side: Side::Bid,
            price,
            size: bid_size,
            notional: price * bid_size,
            state: LevelState::Planned,
        })
        .collect::<Vec<_>>();
    let mut ask_levels = asks
        .into_iter()
        .map(|price| GridLevel {
            side: Side::Ask,
            price,
            size: ask_size,
            notional: price * ask_size,
            state: LevelState::Planned,
        })
        .collect::<Vec<_>>();

    // Bulk orders do not have a reduce-only flag. Directional perp modes are deliberately
    // single-sided: long places only bids; short places only asks.
    match config.perp_mode {
        PerpMode::Long => ask_levels.clear(),
        PerpMode::Short => bid_levels.clear(),
        PerpMode::Neutral => {}
    }

    let quote_required = bid_levels
        .iter()
        .map(|l| l.notional * (Decimal::ONE + config.maker_fee_rate))
        .sum();
    let base_required = ask_levels.iter().map(|l| l.size).sum();
    let long_notional: Decimal = bid_levels.iter().map(|l| l.notional).sum();
    let short_notional: Decimal = ask_levels.iter().map(|l| l.notional).sum();
    let estimated_margin = Some(
        long_notional.max(short_notional) / config.preview_leverage
            + (long_notional + short_notional) * config.maker_fee_rate,
    );
    Ok(GridPlan {
        mid,
        lower,
        upper,
        per_grid_base_size: None,
        bids: bid_levels,
        asks: ask_levels,
        quote_required,
        base_required,
        estimated_margin,
    })
}
