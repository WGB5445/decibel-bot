//! Spot grid cycle logic extracted from `run_cli` without algorithm changes.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use anyhow::Result;
use chrono::Utc;
use rust_decimal::Decimal;

use crate::{
    GridConfig, GridPlan, MonitorSnapshot, RangeBreakoutAction, RangeSpec, SpotFeeRates,
    build_plan_with_per_grid_base_size, exit_sell_assets, journal, spot_lifecycle,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SpotCycleOutcome {
    Completed,
    BreakLoop,
    ContinueOuterLoop,
}

pub struct SpotCycleContext<'a> {
    pub execute: bool,
    pub spot_exit_price: Option<Decimal>,
    pub spot_fee_rates: Option<&'a SpotFeeRates>,
    pub network: &'a str,
    pub api_key: &'a str,
    pub aptos_private_key: &'a str,
    pub subaccount: &'a str,
    pub config: &'a mut GridConfig,
    pub journal: Option<&'a journal::Journal>,
    pub run_state: &'a mut journal::RunState,
    pub pinned_spot_plan: &'a mut Option<GridPlan>,
    pub snapshot: &'a mut MonitorSnapshot,
    pub stop_loss_liquidated: &'a mut bool,
    pub paused_by_breakout: &'a mut bool,
    pub cancelled: Arc<AtomicBool>,
}

impl SpotCycleContext<'_> {
    fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }
}

pub async fn run_spot_cycle(ctx: &mut SpotCycleContext<'_>) -> Result<SpotCycleOutcome> {
    // Optional Spot stop-loss: unlike a normal lower-bound breach, this is terminal.
    // Cancel the bulk ladder and liquidate all available base before stopping the run.
    if ctx.execute
        && let Some(stop) = ctx.spot_exit_price
        && ctx.snapshot.plan.mid <= stop
    {
        println!(
            "Spot stop-loss reached at {} (trigger {}); cancelling ladder and liquidating base.",
            ctx.snapshot.plan.mid, stop
        );
        match exit_sell_assets(
            ctx.network,
            ctx.api_key,
            ctx.aptos_private_key,
            ctx.subaccount,
            &ctx.snapshot.market,
            Some((
                &ctx.config.spot,
                ctx.spot_fee_rates
                    .as_ref()
                    .expect("live Spot execution fetched fee rates"),
            )),
        )
        .await
        {
            Ok(hashes) => {
                println!("Spot stop-loss liquidation completed: {:?}", hashes);
                *ctx.stop_loss_liquidated = true;
            }
            Err(error) => eprintln!("Spot stop-loss liquidation failed: {error:#}"),
        }
        return Ok(SpotCycleOutcome::BreakLoop);
    }
    // Upgrade pre-uniform persisted state once, retaining its price geometry while
    // introducing the fixed per-grid base size used by every later replacement.
    if ctx
        .pinned_spot_plan
        .as_ref()
        .is_some_and(|plan| plan.per_grid_base_size.is_none())
    {
        let upgraded = ctx
            .pinned_spot_plan
            .as_ref()
            .expect("checked above")
            .pin_spot_per_grid_base_size(ctx.config, &ctx.snapshot.market)?;
        println!(
            "Migrated persisted Spot grid to fixed per-grid size {}.",
            upgraded
                .per_grid_base_size
                .expect("Spot migration sets per_grid_base_size")
        );
        *ctx.pinned_spot_plan = Some(upgraded);
    }
    // Initialize the Spot geometry once. From the second cycle onward the plan's mid,
    // lower/upper bounds, prices, and per-level sizes are all pinned for this run.
    if ctx.pinned_spot_plan.is_none() {
        *ctx.pinned_spot_plan = Some(ctx.snapshot.plan.clone());
        println!(
            "Spot grid pinned for this run: bounds [{}, {}], mid {}, {} bid(s), {} ask(s).",
            ctx.snapshot.plan.lower,
            ctx.snapshot.plan.upper,
            ctx.snapshot.plan.mid,
            ctx.snapshot.plan.bids.len(),
            ctx.snapshot.plan.asks.len()
        );
    }
    let mid = ctx.snapshot.plan.mid;
    let (lower, upper) = ctx
        .pinned_spot_plan
        .as_ref()
        .map(|plan| (plan.lower, plan.upper))
        .expect("Spot plan was pinned above");
    if mid < lower || mid > upper {
        let direction = if mid < lower { "below" } else { "above" };
        let reason = format!("Spot mid {mid} broke {direction} pinned range [{lower}, {upper}]");
        match ctx.config.spot.range_breakout_action {
            RangeBreakoutAction::PauseAndAlert => {
                eprintln!("RANGE BREAKOUT: {reason}; pausing the grid.");
                if let Some(journal) = ctx.journal {
                    let event = journal::JournalEvent::RiskRejected {
                        at: Utc::now(),
                        reason: reason.clone(),
                    };
                    journal.append(&event)?;
                    ctx.run_state.apply(&event);
                    journal.save_state(ctx.run_state)?;
                }
                if ctx.execute {
                    match spot_lifecycle::cancel_bulk_ladder(
                        ctx.network,
                        ctx.aptos_private_key,
                        ctx.subaccount,
                        &ctx.snapshot.market,
                    )
                    .await
                    {
                        Ok(hash) => {
                            println!("Range-breakout ladder cancellation submitted in tx {hash}")
                        }
                        Err(error) => eprintln!("Range-breakout cancellation failed: {error:#}"),
                    }
                    *ctx.paused_by_breakout = true;
                    return Ok(SpotCycleOutcome::BreakLoop);
                }
            }
            RangeBreakoutAction::ExtendGrid => {
                let shifted_range = match ctx.config.range.clone() {
                    RangeSpec::Bounds { lower, upper } => {
                        let delta = if mid < lower {
                            mid - lower
                        } else {
                            mid - upper
                        };
                        RangeSpec::Bounds {
                            lower: lower + delta,
                            upper: upper + delta,
                        }
                    }
                    // Percent and geometric-step ranges are naturally rebuilt around the
                    // newest mid; retain their configured spacing parameters.
                    other => other,
                };
                ctx.config.range = shifted_range;
                let fixed_size = ctx
                    .pinned_spot_plan
                    .as_ref()
                    .and_then(|plan| plan.per_grid_base_size);
                let shifted = build_plan_with_per_grid_base_size(
                    ctx.config,
                    &ctx.snapshot.market,
                    mid,
                    fixed_size,
                )?;
                println!(
                    "RANGE BREAKOUT: {reason}; extended grid to [{}, {}]",
                    shifted.lower, shifted.upper
                );
                *ctx.pinned_spot_plan = Some(shifted);
            }
        }
    }
    // The pinned ladder supplies the prices and per-level sizes; only the bid/ask split
    // follows the latest price, which is what produces sell-high/buy-low rotation after
    // a fill without ever moving the grid itself.
    let pinned = ctx
        .pinned_spot_plan
        .as_ref()
        .expect("Spot plan was pinned above");
    ctx.snapshot.plan = pinned.project_spot(mid, ctx.snapshot.market.tick_size)?;
    if let Err(error) = ctx.snapshot.plan.enforce_spot_budget(ctx.config) {
        let reason = format!(
            "fixed Spot per-grid size no longer fits the configured budget after re-centering: {error:#}"
        );
        eprintln!("RISK REJECTED: {reason}");
        if let Some(journal) = ctx.journal {
            let event = journal::JournalEvent::RiskRejected {
                at: Utc::now(),
                reason,
            };
            journal.append(&event)?;
            ctx.run_state.apply(&event);
            journal.save_state(ctx.run_state)?;
        }
        if ctx.cancelled() {
            return Ok(SpotCycleOutcome::BreakLoop);
        }
        tokio::time::sleep(ctx.config.refresh).await;
        return Ok(SpotCycleOutcome::ContinueOuterLoop);
    }
    Ok(SpotCycleOutcome::Completed)
}
