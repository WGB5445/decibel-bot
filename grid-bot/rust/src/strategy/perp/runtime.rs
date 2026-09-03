//! Perp grid cycle: rebuild plan each refresh and pre-submission risk gates.

use anyhow::Result;
use chrono::Utc;
use rust_decimal::Decimal;

use crate::{
    GridConfig, GridPlan, Market, MonitorSnapshot, build_plan, journal, perp_position_is_safe,
    spot_lifecycle,
};

pub fn rebuild_perp_plan(config: &GridConfig, snapshot: &mut MonitorSnapshot) -> Result<()> {
    snapshot.plan = build_plan(config, &snapshot.market, snapshot.plan.mid)?;
    Ok(())
}

/// Returns a rejection reason when the plan must not be submitted.
pub fn perp_submission_blocked(
    config: &GridConfig,
    plan: &GridPlan,
    position: Decimal,
    available_margin: Option<Decimal>,
) -> Option<String> {
    if !perp_position_is_safe(position, plan, config.max_position) {
        let max = config
            .max_position
            .map(|value| value.to_string())
            .unwrap_or_else(|| "unset".to_owned());
        return Some(format!(
            "Perp position {position} or worst-case exposure exceeds max_position {max}"
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
        match spot_lifecycle::cancel_bulk_ladder(
            network,
            aptos_private_key,
            subaccount,
            market,
        )
        .await
        {
            Ok(hash) => println!("Perp risk gate cancelled ladder in tx {hash}"),
            Err(error) => eprintln!("Perp risk gate ladder cancellation failed: {error:#}"),
        }
    }
    Ok(())
}
