//! Perp grid execution, convergence, and pre-submission risk gates.

use anyhow::Result;
use chrono::Utc;
use rust_decimal::Decimal;

use super::{
    convergence::convergence_blocked_reason,
    planning::refresh_perp_plan_metrics,
    risk::{apply_perp_risk_trim, perp_margin_is_safe, perp_position_is_safe},
};
use crate::{
    DecibelClient, GasStationConfig, GridConfig, GridPlan, Market, MonitorSnapshot,
    SpotExecutionConfig, build_plan, journal, spot_lifecycle,
};

const PERP_CANCEL_CONFIRM_ATTEMPTS: usize = 6;
const PERP_CANCEL_CONFIRM_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PerpCloseResult {
    pub cancel_transaction_hash: String,
    pub close_transaction_hash: Option<String>,
    pub position_before: Decimal,
    pub position_after: Decimal,
}

pub fn rebuild_perp_plan(config: &GridConfig, snapshot: &mut MonitorSnapshot) -> Result<()> {
    snapshot.plan = build_plan(config, &snapshot.market, snapshot.plan.mid)?;
    Ok(())
}

pub fn prepare_perp_executable_plan(
    config: &GridConfig,
    mut plan: GridPlan,
    position: Decimal,
    available_margin: Option<Decimal>,
) -> Result<GridPlan> {
    if plan.paused_by_out_of_range {
        return Ok(plan);
    }
    plan.convergence_delta = plan.target_position.map(|target| target - position);
    refresh_perp_plan_metrics(&mut plan, config, position);
    if let Some(margin) = available_margin
        && !perp_margin_is_safe(&plan, margin)
    {
        let required = plan.estimated_margin.unwrap_or(Decimal::ZERO);
        plan.perp_blocked_reason = Some(format!(
            "estimated Perp margin {required} exceeds available {margin}"
        ));
    }
    Ok(plan)
}

pub fn finalize_perp_executable_plan(
    config: &GridConfig,
    mut plan: GridPlan,
    position: Decimal,
    available_margin: Option<Decimal>,
) -> Result<GridPlan> {
    if plan.paused_by_out_of_range {
        return Ok(plan);
    }
    apply_perp_risk_trim(config, &mut plan, position)?;
    prepare_perp_executable_plan(config, plan, position, available_margin)
}

/// Returns a rejection reason when the plan must not be submitted. Target convergence is only a
/// bootstrap prerequisite; a running grid is intentionally allowed to drift with passive fills.
pub fn perp_submission_blocked(
    config: &GridConfig,
    plan: &GridPlan,
    position: Decimal,
    available_margin: Option<Decimal>,
    lot_size: Decimal,
    require_convergence: bool,
) -> Option<String> {
    if plan.paused_by_out_of_range {
        return None;
    }
    if plan.bids.is_empty() && plan.asks.is_empty() {
        return None;
    }
    if let Some(reason) = plan.perp_blocked_reason.clone() {
        return Some(reason);
    }
    if require_convergence
        && let Some(reason) = convergence_blocked_reason(plan, position, lot_size)
    {
        return Some(reason);
    }
    if !perp_position_is_safe(position, plan, config) {
        let (worst_long, worst_short) = (
            plan.worst_long.unwrap_or(position),
            plan.worst_short.unwrap_or(position),
        );
        return Some(format!(
            "Perp worst-case exposure violates mode constraints (worst_long={worst_long}, worst_short={worst_short})"
        ));
    }
    let required = plan.estimated_margin.unwrap_or(Decimal::ZERO);
    if let Some(margin) = available_margin
        && required > margin
    {
        return Some(format!(
            "estimated Perp margin {required} exceeds available {margin}"
        ));
    }
    None
}

pub async fn run_perp_convergence(
    network: &str,
    client: &DecibelClient,
    private_key: &str,
    subaccount: &str,
    market: &Market,
    plan: &GridPlan,
    guard: &SpotExecutionConfig,
    gas_station: Option<&GasStationConfig>,
) -> Result<super::convergence::ConvergencePlan> {
    super::convergence::execute_perp_convergence_market(
        network,
        client,
        private_key,
        subaccount,
        market,
        plan,
        guard,
        gas_station,
    )
    .await
}

/// Safely flatten the market after a strategy stop. Cancellation is treated as a fill race: no
/// reduce-only market close is sent until the current-order endpoint confirms the bulk ladder is
/// gone, and the final exchange position is re-read after the close.
pub async fn cancel_and_flatten_perp(
    network: &str,
    client: &DecibelClient,
    private_key: &str,
    subaccount: &str,
    market: &Market,
    guard: &SpotExecutionConfig,
    gas_station: Option<&GasStationConfig>,
) -> Result<PerpCloseResult> {
    if market.product != crate::Product::Perp {
        anyhow::bail!("Perp close lifecycle cannot run for a Spot market")
    }
    let cancel_transaction_hash =
        spot_lifecycle::cancel_bulk_ladder(network, private_key, subaccount, market, gas_station)
            .await?;
    for attempt in 1..=PERP_CANCEL_CONFIRM_ATTEMPTS {
        let active = client.open_orders(subaccount, market).await?;
        if active.is_empty() {
            break;
        }
        if attempt == PERP_CANCEL_CONFIRM_ATTEMPTS {
            anyhow::bail!(
                "Perp bulk cancellation {} was committed but {} order(s) remain active after {} confirmation attempts; refusing market close",
                cancel_transaction_hash,
                active.len(),
                PERP_CANCEL_CONFIRM_ATTEMPTS,
            )
        }
        tokio::time::sleep(PERP_CANCEL_CONFIRM_INTERVAL).await;
    }

    let before = client
        .account(Some(subaccount), market)
        .await?
        .position
        .size;
    if before.abs() < market.lot_size {
        return Ok(PerpCloseResult {
            cancel_transaction_hash,
            close_transaction_hash: None,
            position_before: before,
            position_after: before,
        });
    }
    let close = super::convergence::execute_perp_convergence_market(
        network,
        client,
        private_key,
        subaccount,
        market,
        &GridPlan {
            target_position: Some(Decimal::ZERO),
            ..GridPlan::default()
        },
        guard,
        gas_station,
    )
    .await?;
    let after = client
        .account(Some(subaccount), market)
        .await?
        .position
        .size;
    if after.abs() >= market.lot_size {
        anyhow::bail!(
            "Perp close did not reach flat position: started {before}, convergence target {}, now {after}",
            close.target
        )
    }
    Ok(PerpCloseResult {
        cancel_transaction_hash,
        // The convergence helper confirms the transaction, but does not currently return its hash.
        close_transaction_hash: None,
        position_before: before,
        position_after: after,
    })
}

pub async fn handle_perp_out_of_range(
    config: &GridConfig,
    plan: &GridPlan,
    network: &str,
    aptos_private_key: &str,
    subaccount: &str,
    market: &Market,
    client: &DecibelClient,
    execute: bool,
    gas_station: Option<&GasStationConfig>,
    out_of_range_handled: &mut bool,
) -> Result<()> {
    if plan.out_of_range_action_applied.is_none() {
        *out_of_range_handled = false;
        return Ok(());
    }
    if *out_of_range_handled {
        return Ok(());
    }
    let action = config.out_of_range_action;
    if !execute {
        return Ok(());
    }
    match action {
        crate::OutOfRangeAction::Pause => {}
        crate::OutOfRangeAction::CancelOrders => {
            let hash = spot_lifecycle::cancel_bulk_ladder(
                network,
                aptos_private_key,
                subaccount,
                market,
                gas_station,
            )
            .await?;
            println!("Perp out-of-range cancelled ladder in tx {hash}");
            *out_of_range_handled = true;
        }
        crate::OutOfRangeAction::ClosePosition => {
            let result = cancel_and_flatten_perp(
                network,
                client,
                aptos_private_key,
                subaccount,
                market,
                &guard_from_config(config),
                gas_station,
            )
            .await?;
            println!(
                "Perp out-of-range close: cancelled ladder in tx {}; position {} -> {}",
                result.cancel_transaction_hash, result.position_before, result.position_after
            );
            *out_of_range_handled = true;
        }
        crate::OutOfRangeAction::ClampContinue => {}
    }
    Ok(())
}

fn guard_from_config(config: &GridConfig) -> SpotExecutionConfig {
    config.spot.clone()
}

pub fn record_perp_risk_rejection(
    reason: String,
    journal: Option<&journal::Journal>,
    run_state: &mut journal::RunState,
) -> Result<()> {
    eprintln!("RISK REJECTED: {reason}");
    if let Some(journal) = journal {
        let event = journal::JournalEvent::RiskRejected {
            at: Utc::now(),
            reason: reason.clone(),
        };
        journal.append(&event)?;
        run_state.apply(&event);
        journal.save_state(run_state)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use rust_decimal_macros::dec;

    use super::perp_submission_blocked;
    use crate::{
        Allocation, GridConfig, GridLevel, GridPlan, LevelState, PerpMode, Product, RangeSpec,
        Side, SpotExecutionConfig,
    };

    fn config() -> GridConfig {
        GridConfig {
            product: Product::Perp,
            perp_mode: PerpMode::Neutral,
            market_name: "BTC/USD".to_owned(),
            range: RangeSpec::Bounds {
                lower: dec!(90),
                upper: dec!(110),
            },
            total_count: 2,
            allocation: Allocation::FixedSize(dec!(1)),
            maker_fee_rate: dec!(0),
            preview_leverage: dec!(1),
            refresh: std::time::Duration::from_secs(1),
            price_source: crate::PriceSource::Prices,
            spot: SpotExecutionConfig::default(),
            max_position: None,
            out_of_range_action: crate::OutOfRangeAction::Pause,
        }
    }

    fn level(side: Side, price: rust_decimal::Decimal) -> GridLevel {
        GridLevel {
            side,
            price,
            size: dec!(1),
            notional: price,
            state: LevelState::Planned,
        }
    }

    fn plan() -> GridPlan {
        GridPlan {
            mid: dec!(100),
            lower: dec!(90),
            upper: dec!(110),
            bids: vec![level(Side::Bid, dec!(99))],
            asks: vec![level(Side::Ask, dec!(101))],
            target_position: Some(dec!(1)),
            estimated_margin: Some(dec!(1)),
            ..GridPlan::default()
        }
    }

    #[test]
    fn completed_bootstrap_does_not_block_replacement_for_target_drift() {
        let plan = plan();
        assert!(
            perp_submission_blocked(&config(), &plan, dec!(0), Some(dec!(10)), dec!(0.1), true)
                .is_some()
        );
        assert!(
            perp_submission_blocked(&config(), &plan, dec!(0), Some(dec!(10)), dec!(0.1), false)
                .is_none()
        );
    }

    #[test]
    fn completed_bootstrap_keeps_margin_gate() {
        assert!(
            perp_submission_blocked(&config(), &plan(), dec!(0), Some(dec!(0)), dec!(0.1), false)
                .is_some()
        );
    }
}
