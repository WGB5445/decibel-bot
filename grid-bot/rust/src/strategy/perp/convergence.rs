//! Perp position convergence via preflighted market orders before bulk replacement.

use anyhow::{Result, anyhow, bail};
use rust_decimal::Decimal;

use crate::{
    DecibelClient, GasStationConfig, GridPlan, Market, OrderBook, SpotExecutionConfig, round_down,
    submit_perp_market_order,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConvergencePlan {
    pub current: Decimal,
    pub target: Decimal,
    pub delta: Decimal,
    pub converged: bool,
}

pub fn perp_convergence_plan(
    current: Decimal,
    target: Decimal,
    lot_size: Decimal,
) -> ConvergencePlan {
    let delta = target - current;
    let converged = delta.abs() < lot_size;
    ConvergencePlan {
        current,
        target,
        delta,
        converged,
    }
}

/// Estimate a market order from the current visible book. This is a preflight guard only: a
/// market order has no on-chain price cap, so the book may move before execution.
pub fn preflight_perp_market_order(
    book: &OrderBook,
    buy: bool,
    quantity: Decimal,
    max_slippage_bps: Decimal,
) -> Result<Decimal> {
    if quantity <= Decimal::ZERO {
        bail!("Perp market-order quantity must be positive")
    }
    let levels = if buy { &book.asks } else { &book.bids };
    let reference = levels
        .first()
        .ok_or_else(|| anyhow!("Perp market-order preflight has no executable book side"))?
        .price;
    let mut remaining = quantity;
    let mut quote = Decimal::ZERO;
    for level in levels {
        let taken = remaining.min(level.size);
        quote += taken * level.price;
        remaining -= taken;
        if remaining <= Decimal::ZERO {
            break;
        }
    }
    if remaining > Decimal::ZERO {
        bail!(
            "Perp market-order preflight has only {} base within visible depth; needs {quantity}",
            quantity - remaining
        )
    }
    let average = quote / quantity;
    let slippage_bps = if buy {
        (average / reference - Decimal::ONE) * Decimal::from(10_000)
    } else {
        (Decimal::ONE - average / reference) * Decimal::from(10_000)
    };
    if slippage_bps > max_slippage_bps {
        bail!(
            "Perp market-order preflight estimates {slippage_bps:.4} bps slippage above configured {max_slippage_bps} bps; no price-capped Perp IOC ABI is configured"
        )
    }
    Ok(average)
}

pub async fn execute_perp_convergence_market(
    network: &str,
    client: &DecibelClient,
    private_key: &str,
    subaccount: &str,
    market: &Market,
    plan: &GridPlan,
    guard: &SpotExecutionConfig,
    gas_station: Option<&GasStationConfig>,
) -> Result<ConvergencePlan> {
    let target = plan
        .target_position
        .ok_or_else(|| anyhow!("perp plan is missing target_position"))?;
    let overview = client.account(Some(subaccount), market).await?;
    let current = overview.position.size;
    let mut state = perp_convergence_plan(current, target, market.lot_size);
    if state.converged {
        return Ok(state);
    }

    let mut attempts = 0usize;
    while attempts < guard.entry_exit_max_attempts {
        let position = client
            .account(Some(subaccount), market)
            .await?
            .position
            .size;
        state = perp_convergence_plan(position, target, market.lot_size);
        if state.converged {
            return Ok(state);
        }
        let remaining = state.delta.abs();
        let side_buy = state.delta.is_sign_positive();
        let reduce_only =
            (position > Decimal::ZERO && !side_buy) || (position < Decimal::ZERO && side_buy);
        let size = round_down(remaining, market.lot_size);
        if size < market.min_size {
            bail!(
                "Perp market convergence has no executable size (remaining={remaining} rounds below min_size={})",
                market.min_size
            )
        }
        let max_slippage_bps = if reduce_only {
            guard.exit_max_slippage_bps
        } else {
            guard.entry_max_slippage_bps
        };
        let average = preflight_perp_market_order(
            &client.order_book(market, 50).await?,
            side_buy,
            size,
            max_slippage_bps,
        )?;
        println!(
            "Perp market-order preflight: {} {} at estimated average {} ({} bps guard; no hard price cap)",
            if side_buy { "buy" } else { "sell" },
            size,
            average,
            max_slippage_bps,
        );
        submit_perp_market_order(
            network,
            private_key,
            subaccount,
            market,
            size,
            side_buy,
            reduce_only,
            gas_station,
        )
        .await?;
        attempts += 1;
        let after = client
            .account(Some(subaccount), market)
            .await?
            .position
            .size;
        let filled = (after - position).abs();
        let min_fill = remaining * guard.entry_min_fill_ratio;
        if filled < min_fill.max(market.lot_size) {
            eprintln!(
                "Perp market convergence partial fill {filled} below minimum {min_fill} for remaining {remaining}; retrying ({attempts}/{})",
                guard.entry_exit_max_attempts
            );
            continue;
        }
        state = perp_convergence_plan(after, target, market.lot_size);
        if state.converged {
            return Ok(state);
        }
    }
    let final_position = client
        .account(Some(subaccount), market)
        .await?
        .position
        .size;
    state = perp_convergence_plan(final_position, target, market.lot_size);
    if state.converged {
        Ok(state)
    } else {
        bail!("perp position {final_position} did not converge to target {target} within tolerance")
    }
}

pub fn convergence_blocked_reason(
    plan: &GridPlan,
    current: Decimal,
    lot_size: Decimal,
) -> Option<String> {
    let target = plan.target_position?;
    let state = perp_convergence_plan(current, target, lot_size);
    if state.converged {
        None
    } else {
        Some(format!(
            "perp position {current} must converge to {target} (delta {}) before bulk submission",
            state.delta
        ))
    }
}

#[cfg(test)]
mod tests {
    use rust_decimal_macros::dec;

    use crate::{BookLevel, OrderBook};

    use super::preflight_perp_market_order;

    #[test]
    fn preflight_rejects_a_market_buy_beyond_the_slippage_guard() {
        let book = OrderBook {
            bids: vec![],
            asks: vec![
                BookLevel {
                    price: dec!(100),
                    size: dec!(1),
                },
                BookLevel {
                    price: dec!(102),
                    size: dec!(1),
                },
            ],
        };
        assert!(preflight_perp_market_order(&book, true, dec!(2), dec!(50)).is_err());
        assert_eq!(
            preflight_perp_market_order(&book, true, dec!(2), dec!(100)).unwrap(),
            dec!(101)
        );
    }
}
