//! Perp grid cycle: rebuild plan each refresh, convergence, and pre-submission risk gates.

use anyhow::Result;
use chrono::Utc;
use rust_decimal::Decimal;

use super::{
    convergence::{convergence_blocked_reason, perp_convergence_plan},
    planning::refresh_perp_plan_metrics,
    risk::{apply_perp_risk_trim, perp_margin_is_safe, perp_position_is_safe},
};
use crate::{
    DecibelClient, GridConfig, GridPlan, Market, MonitorSnapshot, SpotExecutionConfig, build_plan,
    journal, spot_lifecycle,
};

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

/// Returns a rejection reason when the plan must not be submitted.
pub fn perp_submission_blocked(
    config: &GridConfig,
    plan: &GridPlan,
    position: Decimal,
    available_margin: Option<Decimal>,
    lot_size: Decimal,
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
    if let Some(reason) = convergence_blocked_reason(plan, position, lot_size) {
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
) -> Result<super::convergence::ConvergencePlan> {
    super::convergence::execute_perp_convergence_ioc(
        network,
        client,
        private_key,
        subaccount,
        market,
        plan,
        guard,
    )
    .await
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
) -> Result<()> {
    if plan.out_of_range_action_applied.is_none() {
        return Ok(());
    }
    let action = config.out_of_range_action;
    if !execute {
        return Ok(());
    }
    match action {
        crate::OutOfRangeAction::Pause => {}
        crate::OutOfRangeAction::CancelOrders => {
            let hash =
                spot_lifecycle::cancel_bulk_ladder(network, aptos_private_key, subaccount, market)
                    .await?;
            println!("Perp out-of-range cancelled ladder in tx {hash}");
        }
        crate::OutOfRangeAction::ClosePosition => {
            let hash =
                spot_lifecycle::cancel_bulk_ladder(network, aptos_private_key, subaccount, market)
                    .await?;
            println!("Perp out-of-range cancelled ladder in tx {hash}");
            let overview = client.account(Some(subaccount), market).await?;
            if overview.position.size != Decimal::ZERO {
                super::convergence::execute_perp_convergence_ioc(
                    network,
                    client,
                    aptos_private_key,
                    subaccount,
                    market,
                    &GridPlan {
                        target_position: Some(Decimal::ZERO),
                        ..plan.clone()
                    },
                    &guard_from_config(config),
                )
                .await?;
            }
        }
        crate::OutOfRangeAction::ClampContinue => {}
    }
    Ok(())
}

fn guard_from_config(config: &GridConfig) -> SpotExecutionConfig {
    config.spot.clone()
}

pub async fn record_perp_risk_rejection(
    reason: String,
    network: &str,
    aptos_private_key: &str,
    subaccount: &str,
    market: &Market,
    journal: Option<&journal::Journal>,
    run_state: &mut journal::RunState,
    execute: bool,
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
    if execute {
        match spot_lifecycle::cancel_bulk_ladder(network, aptos_private_key, subaccount, market)
            .await
        {
            Ok(hash) => println!("Perp risk gate cancelled ladder in tx {hash}"),
            Err(error) => eprintln!("Perp risk gate ladder cancellation failed: {error:#}"),
        }
    }
    Ok(())
}
