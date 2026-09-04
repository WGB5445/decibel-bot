//! Perp worst-case exposure, margin preview, and pending-order trimming.

use anyhow::{Result, bail};
use rust_decimal::Decimal;

use crate::{GridConfig, GridPlan, PerpMode};

pub(crate) fn compute_perp_target(
    mode: PerpMode,
    ask_levels: usize,
    bid_levels: usize,
    grid_size: Decimal,
) -> Decimal {
    match mode {
        PerpMode::Long => Decimal::from(ask_levels) * grid_size,
        PerpMode::Short => -Decimal::from(bid_levels) * grid_size,
        PerpMode::Neutral => {
            Decimal::from(ask_levels.saturating_sub(bid_levels)) * grid_size / Decimal::TWO
        }
    }
}

pub(crate) fn perp_worst_case(position: Decimal, plan: &GridPlan) -> (Decimal, Decimal) {
    let bid_sum: Decimal = plan.bids.iter().map(|level| level.size).sum();
    let ask_sum: Decimal = plan.asks.iter().map(|level| level.size).sum();
    (position + bid_sum, position - ask_sum)
}

pub(crate) fn perp_theoretical_limits(
    config: &GridConfig,
    ask_levels: usize,
    bid_levels: usize,
    grid_size: Decimal,
) -> (Decimal, Decimal) {
    if let Some(max) = config.max_position {
        return (max, max);
    }
    match config.perp_mode {
        PerpMode::Long => (Decimal::from(ask_levels) * grid_size, Decimal::ZERO),
        PerpMode::Short => (Decimal::ZERO, Decimal::from(bid_levels) * grid_size),
        PerpMode::Neutral => (
            Decimal::from(ask_levels) * grid_size,
            Decimal::from(bid_levels) * grid_size,
        ),
    }
}

pub fn perp_position_is_safe(position: Decimal, plan: &GridPlan, config: &GridConfig) -> bool {
    let grid_size = plan.per_grid_base_size.unwrap_or_else(|| {
        plan.bids
            .first()
            .or_else(|| plan.asks.first())
            .map(|level| level.size)
            .unwrap_or(Decimal::ZERO)
    });
    let ask_levels = plan.asks.len();
    let bid_levels = plan.bids.len();
    let (max_long, max_short) = perp_theoretical_limits(config, ask_levels, bid_levels, grid_size);
    let (worst_long, worst_short) = perp_worst_case(position, plan);
    mode_constraints_hold(
        config.perp_mode,
        worst_long,
        worst_short,
        max_long,
        max_short,
    )
}

fn mode_constraints_hold(
    mode: PerpMode,
    worst_long: Decimal,
    worst_short: Decimal,
    max_long: Decimal,
    max_short: Decimal,
) -> bool {
    match mode {
        PerpMode::Long => worst_short >= Decimal::ZERO && worst_long <= max_long,
        PerpMode::Short => worst_long <= Decimal::ZERO && worst_short >= -max_short,
        PerpMode::Neutral => worst_long <= max_long && worst_short >= -max_short,
    }
}

pub(crate) fn perp_estimated_margin(
    config: &GridConfig,
    planning_price: Decimal,
    worst_long: Decimal,
    worst_short: Decimal,
    plan: &GridPlan,
) -> Decimal {
    let margin_position = worst_long.abs().max(worst_short.abs());
    let pending_notional: Decimal = plan
        .bids
        .iter()
        .chain(plan.asks.iter())
        .map(|level| level.notional)
        .sum();
    margin_position * planning_price / config.preview_leverage
        + pending_notional * config.maker_fee_rate
}

pub(crate) fn apply_perp_risk_trim(
    config: &GridConfig,
    plan: &mut GridPlan,
    position: Decimal,
) -> Result<()> {
    if plan.bids.is_empty() && plan.asks.is_empty() {
        return Ok(());
    }
    loop {
        if perp_position_is_safe(position, plan, config) {
            return Ok(());
        }
        let removed_bid = if !plan.bids.is_empty() {
            plan.bids.pop();
            true
        } else {
            false
        };
        let removed_ask = if !removed_bid && !plan.asks.is_empty() {
            plan.asks.pop();
            true
        } else {
            false
        };
        if !removed_bid && !removed_ask {
            break;
        }
    }
    if !perp_position_is_safe(position, plan, config) || side_constraint_broken(config, plan) {
        bail!("perp pending ladder cannot be trimmed to a safe bilateral shape")
    }
    Ok(())
}

fn side_constraint_broken(config: &GridConfig, plan: &GridPlan) -> bool {
    match config.perp_mode {
        PerpMode::Long => {
            plan.asks.is_empty() && plan.target_position.is_some_and(|t| t > Decimal::ZERO)
        }
        PerpMode::Short => {
            plan.bids.is_empty() && plan.target_position.is_some_and(|t| t < Decimal::ZERO)
        }
        PerpMode::Neutral => false,
    }
}

pub(crate) fn perp_margin_is_safe(plan: &GridPlan, available_margin: Decimal) -> bool {
    plan.estimated_margin
        .is_some_and(|required| required <= available_margin)
}
