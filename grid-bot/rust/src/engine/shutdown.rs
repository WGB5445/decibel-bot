use anyhow::{Context, Result};
use decibel_grid_tui::*;

use crate::cli::settings::Settings;
use crate::engine::cancel::journaled_bulk_cancel;

/// Execute the Perp exit policy: retain (cancel ladder only) or sell (cancel + flat).
pub async fn handle_perp_exit(
    exit_policy: ExitAssetPolicy,
    execute: bool,
    settings: &Settings,
    run_state: &mut journal::RunState,
    last_market: &mut Option<Market>,
    api: &DecibelClient,
    config: &GridConfig,
    gas_station: Option<&GasStationConfig>,
    journal: Option<&journal::Journal>,
) -> Result<()> {
    let market = last_market.clone().context("no market captured for exit")?;
    if !execute {
        return Ok(());
    }
    if exit_policy == ExitAssetPolicy::Sell {
        let result = strategy::perp::runtime::cancel_and_flatten_perp(
            &settings.network, api, &settings.aptos_private_key,
            &settings.subaccount, &market, &config.spot, gas_station,
        ).await?;
        tracing::info!(
            target: "engine",
            "Perp exit: cancelled in tx {}, position {} → {}",
            result.cancel_transaction_hash, result.position_before, result.position_after
        );
    } else {
        let hash = journaled_bulk_cancel(
            journal, run_state, &settings.network, &settings.aptos_private_key,
            &settings.subaccount, &market, gas_station,
        ).await?;
        if let Some(h) = hash {
            tracing::info!(target: "engine", "Perp exit: cancelled ladder in tx {h}; position retained.");
        }
    }
    Ok(())
}