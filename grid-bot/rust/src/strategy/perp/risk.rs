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

pub fn perp_bootstrap_target_is_safe(config: &GridConfig, target: Decimal) -> bool {
    config.max_position.is_none_or(|max| target.abs() <= max)
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
    let total = bid_levels + ask_levels;
    match config.perp_mode {
        // Long and short converge to a directional starting position, then their
        // same-direction orders can all fill. Neutral starts at its midpoint.
        // Every mode therefore has a maximum absolute endpoint of half the
        // total grid quantity on each side, except directional modes where the
        // opposite endpoint is zero.
        PerpMode::Long => (Decimal::from(total) * grid_size, Decimal::ZERO),
        PerpMode::Short => (Decimal::ZERO, Decimal::from(total) * grid_size),
        PerpMode::Neutral => {
            let max_side = Decimal::from(total) * grid_size / Decimal::TWO;
            (max_side, max_side)
        }
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
    position: Decimal,
    worst_long: Decimal,
    worst_short: Decimal,
    plan: &GridPlan,
) -> Decimal {
    // `cross_available_to_trade` is free collateral: the exchange has already
    // reserved margin for `position`. Only charge the ladder for the additional
    // margin needed to reach its worst-case endpoint.
    let worst_case_position = worst_long.abs().max(worst_short.abs());
    let additional_position = (worst_case_position - position.abs()).max(Decimal::ZERO);
    let pending_notional: Decimal = plan
        .bids
        .iter()
        .chain(plan.asks.iter())
        .map(|level| level.notional)
        .sum();
    additional_position * planning_price / config.preview_leverage
        + pending_notional * config.maker_fee_rate
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TrimSide {
    Bid,
    Ask,
}

fn trim_violation_side(
    config: &GridConfig,
    position: Decimal,
    plan: &GridPlan,
) -> Option<TrimSide> {
    let grid_size = plan.per_grid_base_size.unwrap_or_else(|| {
        plan.bids
            .first()
            .or_else(|| plan.asks.first())
            .map(|level| level.size)
            .unwrap_or(Decimal::ZERO)
    });
    let (max_long, max_short) =
        perp_theoretical_limits(config, plan.asks.len(), plan.bids.len(), grid_size);
    let (worst_long, worst_short) = perp_worst_case(position, plan);
    match config.perp_mode {
        PerpMode::Long => {
            if worst_long > max_long {
                Some(TrimSide::Bid)
            } else if worst_short < Decimal::ZERO && position > Decimal::ZERO {
                Some(TrimSide::Ask)
            } else {
                None
            }
        }
        PerpMode::Short => {
            if worst_long > Decimal::ZERO && position < Decimal::ZERO {
                Some(TrimSide::Bid)
            } else if worst_short < -max_short {
                Some(TrimSide::Ask)
            } else {
                None
            }
        }
        PerpMode::Neutral => {
            if worst_long > max_long {
                Some(TrimSide::Bid)
            } else if worst_short < -max_short {
                Some(TrimSide::Ask)
            } else {
                None
            }
        }
    }
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
        let Some(side) = trim_violation_side(config, position, plan) else {
            break;
        };
        let removed = match side {
            TrimSide::Bid => plan.bids.pop().is_some(),
            TrimSide::Ask => plan.asks.pop().is_some(),
        };
        if !removed {
            break;
        }
    }
    if position == Decimal::ZERO
        && matches!(config.perp_mode, PerpMode::Long | PerpMode::Short)
        && plan.target_position.is_some()
    {
        // Bootstrap convergence runs before bulk submission; flat directional grids are
        // intentionally bilateral even though worst-case exposure checks fail at zero.
        return Ok(());
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Allocation, GridLevel, GridPlan, LevelState, Product, RangeSpec, Side};
    use rust_decimal_macros::dec;
    use std::time::Duration;

    fn level(side: Side, price: Decimal, size: Decimal) -> GridLevel {
        GridLevel {
            side,
            price,
            size,
            notional: price * size,
            state: LevelState::Planned,
        }
    }

    fn short_config() -> GridConfig {
        GridConfig {
            product: Product::Perp,
            perp_mode: PerpMode::Short,
            market_name: "BTC/USD".to_owned(),
            range: RangeSpec::Bounds {
                lower: dec!(90),
                upper: dec!(110),
            },
            total_count: 4,
            allocation: Allocation::FixedSize(dec!(0.01)),
            maker_fee_rate: dec!(0.0001),
            preview_leverage: dec!(1),
            refresh: Duration::from_secs(3),
            price_source: crate::PriceSource::Prices,
            spot: crate::SpotExecutionConfig::default(),
            max_position: Some(dec!(0.03)),
            out_of_range_action: crate::OutOfRangeAction::Pause,
        }
    }

    #[test]
    fn long_bootstrap_at_flat_preserves_ask_ladder() {
        let mut plan = GridPlan {
            bids: vec![level(Side::Bid, dec!(99), dec!(0.01)); 2],
            asks: vec![level(Side::Ask, dec!(101), dec!(0.01)); 2],
            target_position: Some(dec!(0.02)),
            per_grid_base_size: Some(dec!(0.01)),
            ..GridPlan::default()
        };
        let config = GridConfig {
            perp_mode: PerpMode::Long,
            max_position: None,
            ..short_config()
        };
        apply_perp_risk_trim(&config, &mut plan, dec!(0)).unwrap();
        assert_eq!(plan.asks.len(), 2);
        assert_eq!(plan.bids.len(), 2);
    }

    #[test]
    fn bootstrap_target_respects_max_position() {
        let config = GridConfig {
            perp_mode: PerpMode::Long,
            max_position: Some(dec!(0.01)),
            ..short_config()
        };
        assert!(perp_bootstrap_target_is_safe(&config, dec!(0.01)));
        assert!(!perp_bootstrap_target_is_safe(&config, dec!(0.02)));
        assert!(!perp_bootstrap_target_is_safe(&config, dec!(-0.02)));
    }

    #[test]
    fn short_bootstrap_at_flat_preserves_bid_ladder() {
        let mut plan = GridPlan {
            bids: vec![level(Side::Bid, dec!(99), dec!(0.01)); 2],
            asks: vec![level(Side::Ask, dec!(101), dec!(0.01)); 2],
            target_position: Some(dec!(-0.02)),
            per_grid_base_size: Some(dec!(0.01)),
            ..GridPlan::default()
        };
        apply_perp_risk_trim(&short_config(), &mut plan, dec!(0)).unwrap();
        assert_eq!(plan.bids.len(), 2);
        assert_eq!(plan.asks.len(), 2);
    }

    #[test]
    fn short_mode_trim_prefers_bids_when_long_exposure_violates() {
        let mut plan = GridPlan {
            bids: vec![level(Side::Bid, dec!(99), dec!(0.01)); 4],
            asks: vec![level(Side::Ask, dec!(101), dec!(0.01)); 2],
            target_position: Some(dec!(-0.02)),
            per_grid_base_size: Some(dec!(0.01)),
            ..GridPlan::default()
        };
        apply_perp_risk_trim(&short_config(), &mut plan, dec!(-0.01)).unwrap();
        assert!(!plan.bids.is_empty());
        assert_eq!(plan.asks.len(), 2);
        assert!(perp_position_is_safe(dec!(-0.01), &plan, &short_config()));
    }

    #[test]
    fn margin_check_excludes_margin_already_locked_for_position() {
        let plan = GridPlan {
            bids: vec![level(Side::Bid, dec!(100), dec!(1)); 2],
            asks: vec![level(Side::Ask, dec!(100), dec!(1)); 1],
            ..GridPlan::default()
        };
        let config = short_config();

        // A one-unit short is already funded by the exchange. Filling both bids
        // moves it to a one-unit long, so only one additional unit needs margin.
        assert_eq!(
            perp_estimated_margin(&config, dec!(100), dec!(-1), dec!(1), dec!(-2), &plan),
            dec!(100.03)
        );
    }

    #[test]
    fn long_ladder_requires_reentry_after_position_returns_to_zero() {
        let plan = GridPlan {
            bids: vec![level(Side::Bid, dec!(100), dec!(1)); 2],
            asks: vec![level(Side::Ask, dec!(100), dec!(1)); 1],
            target_position: Some(dec!(1)),
            per_grid_base_size: Some(dec!(1)),
            ..GridPlan::default()
        };
        let config = GridConfig {
            perp_mode: PerpMode::Long,
            max_position: None,
            ..short_config()
        };

        // The initial one-unit long covers the ask. Both bids may subsequently
        // fill, taking the position to the three-unit grid maximum.
        assert!(perp_position_is_safe(dec!(1), &plan, &config));
        assert!(!perp_position_is_safe(Decimal::ZERO, &plan, &config));
    }
}
