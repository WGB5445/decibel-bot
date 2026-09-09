use anyhow::Result;
use decibel_grid_tui::*;
use rust_decimal::Decimal;

use crate::engine::cancel::journaled_bulk_cancel;

/// Re-read account position before signing a Perp bulk transaction, and re-check risk.
/// Returns false if the submission should be skipped.
pub async fn run_pre_submit_guard(
    api: &DecibelClient,
    config: &GridConfig,
    subaccount: &str,
    market: &Market,
    exec_plan: &mut GridPlan,
    snapshot_position: Decimal,
    snapshot_margin: Option<Decimal>,
    journal: Option<&journal::Journal>,
    run_state: &mut journal::RunState,
    perp_bootstrap_pending: bool,
    last_perp_submission_block_reason: &mut Option<String>,
    refresh_duration: std::time::Duration,
) -> Result<bool> {
    match api.account(Some(subaccount), market).await {
        Ok(refreshed) => {
            let latest_position = refreshed.position.size;
            let latest_margin = refreshed.available_margin;
            if (latest_position - snapshot_position).abs() > market.lot_size {
                tracing::info!(
                    target: "engine",
                    "Pre-submit position changed: {} → {}. Rebuilding plan.",
                    snapshot_position, latest_position
                );
            }
            *exec_plan = strategy::perp::runtime::finalize_perp_executable_plan(
                config,
                exec_plan.clone(),
                latest_position,
                latest_margin,
            )?;
            if let Some(reason) = strategy::perp::runtime::perp_submission_blocked(
                config,
                exec_plan,
                latest_position,
                latest_margin,
                market.lot_size,
                perp_bootstrap_pending,
            ) {
                tracing::warn!(target: "engine", "PRE-SUBMIT BLOCKED: {reason}");
                strategy::perp::runtime::record_perp_risk_rejection(reason.clone(), journal, run_state)?;
                *last_perp_submission_block_reason = Some(reason);
                tokio::time::sleep(refresh_duration).await;
                return Ok(false);
            }
        }
        Err(error) => {
            tracing::warn!(
                target: "engine",
                "PRE-SUBMIT account refresh failed: {error:#}; submitting with stale position {}",
                snapshot_position
            );
        }
    }
    Ok(true)
}

/// Cancel the active bulk ladder with full journal lifecycle.
pub async fn cancel_active_ladder(
    network: &str,
    private_key: &str,
    subaccount: &str,
    market: &Market,
    gas_station: Option<&GasStationConfig>,
    journal: Option<&journal::Journal>,
    run_state: &mut journal::RunState,
) -> Result<Option<String>> {
    journaled_bulk_cancel(journal, run_state, network, private_key, subaccount, market, gas_station).await
}