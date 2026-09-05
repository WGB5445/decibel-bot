//! Perp position convergence via bounded IOC before bulk replacement.

use anyhow::{Result, anyhow, bail};
use rust_decimal::Decimal;

use crate::{
    DecibelClient, GasStationConfig, GridPlan, Market, OrderBook, SpotExecutionConfig, round_down,
    round_up, submit_perp_ioc_order,
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

pub async fn execute_perp_convergence_ioc(
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

    let book = client.order_book(market, 50).await?;
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
        let attempt = plan_perp_ioc_attempt(&book, side_buy, remaining, market, guard)?;
        let Some(attempt) = attempt else {
            bail!("perp convergence IOC has no executable liquidity within slippage cap")
        };
        submit_perp_ioc_order(
            network,
            private_key,
            subaccount,
            market,
            attempt.limit_price,
            attempt.size,
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
            bail!(
                "perp convergence IOC partial fill {filled} below minimum {min_fill} for remaining {remaining}"
            );
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

#[derive(Clone, Debug)]
struct PerpIocAttempt {
    size: Decimal,
    limit_price: Decimal,
}

fn plan_perp_ioc_attempt(
    book: &OrderBook,
    buy: bool,
    remaining: Decimal,
    market: &Market,
    guard: &SpotExecutionConfig,
) -> Result<Option<PerpIocAttempt>> {
    let reference = if buy {
        book.asks.first()
    } else {
        book.bids.first()
    };
    let Some(reference) = reference else {
        return Ok(None);
    };
    let bps = Decimal::from(10_000);
    let cap = if buy {
        reference.price * (Decimal::ONE + guard.entry_max_slippage_bps / bps)
    } else {
        reference.price * (Decimal::ONE - guard.entry_max_slippage_bps / bps)
    };
    let limit_price = if buy {
        round_up(
            cap.min(reference.price * Decimal::new(1003, 3)),
            market.tick_size,
        )
    } else {
        round_down(
            cap.max(reference.price * Decimal::new(997, 3)),
            market.tick_size,
        )
    };
    let size = round_down(remaining, market.lot_size);
    if size < market.min_size {
        return Ok(None);
    }
    Ok(Some(PerpIocAttempt { size, limit_price }))
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
