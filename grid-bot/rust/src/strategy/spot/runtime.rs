//! Spot grid cycle logic extracted from `run_cli` without algorithm changes.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use anyhow::Result;
use chrono::Utc;
use rust_decimal::Decimal;

use crate::{
    GridConfig, GridPlan, MonitorSnapshot, RangeBreakoutAction, SpotFeeRates, exit_sell_assets,
    journal, simulation, spot_lifecycle,
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
    let had_pinned = ctx.pinned_spot_plan.is_some();
    let migrated_from_legacy = ctx
        .pinned_spot_plan
        .as_ref()
        .is_some_and(|plan| plan.per_grid_base_size.is_none());

    let offline = simulation::run_offline_cycle(simulation::OfflineCycleInput {
        config: ctx.config.clone(),
        market: ctx.snapshot.market.clone(),
        mid: ctx.snapshot.plan.mid,
        account: ctx.snapshot.account.clone(),
        pinned_spot_plan: ctx.pinned_spot_plan.clone(),
        spot_exit_price: ctx.spot_exit_price,
    })?;

    *ctx.config = offline.config;
    *ctx.pinned_spot_plan = offline.pinned_spot_plan.clone();
    ctx.snapshot.plan = offline.plan;

    if migrated_from_legacy && let Some(upgraded) = ctx.pinned_spot_plan.as_ref() {
        println!(
            "Migrated persisted Spot grid to fixed per-grid size {}.",
            upgraded
                .per_grid_base_size
                .expect("Spot migration sets per_grid_base_size")
        );
    } else if !had_pinned && ctx.pinned_spot_plan.is_some() {
        let pinned = ctx.pinned_spot_plan.as_ref().expect("pinned plan");
        println!(
            "Spot grid pinned for this run: bounds [{}, {}], mid {}, {} bid(s), {} ask(s).",
            pinned.lower,
            pinned.upper,
            ctx.snapshot.plan.mid,
            ctx.snapshot.plan.bids.len(),
            ctx.snapshot.plan.asks.len()
        );
    }

    if offline.spot_stop_loss_triggered {
        if ctx.execute {
            println!(
                "Spot stop-loss reached at {} (trigger {}); cancelling ladder and liquidating base.",
                ctx.snapshot.plan.mid,
                ctx.spot_exit_price.expect("stop-loss armed")
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
        }
        return Ok(SpotCycleOutcome::BreakLoop);
    }

    if let Some(reason) = offline.spot_breakout {
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
                println!(
                    "RANGE BREAKOUT: {reason}; extended grid to [{}, {}]",
                    ctx.snapshot.plan.lower, ctx.snapshot.plan.upper
                );
            }
        }
    }

    if let Some(reason) = offline.budget_rejected {
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

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn offline_cycle_matches_legacy_pin_and_project() {
        let config = GridConfig {
            product: crate::Product::Spot,
            perp_mode: crate::PerpMode::Neutral,
            market_name: "APT/USDC".to_owned(),
            range: crate::RangeSpec::Percent { percent: dec!(10) },
            total_count: 8,
            allocation: crate::Allocation::FixedSize(dec!(1)),
            maker_fee_rate: dec!(0.001),
            preview_leverage: dec!(1),
            refresh: std::time::Duration::from_secs(3),
            price_source: crate::PriceSource::Prices,
            spot: crate::SpotExecutionConfig::default(),
            max_position: None,
            out_of_range_action: crate::OutOfRangeAction::default(),
        };
        let market = crate::Market {
            address: "0xspot".to_owned(),
            name: "APT/USDC".to_owned(),
            tick_size: dec!(0.01),
            lot_size: dec!(0.01),
            min_size: dec!(0.01),
            px_decimals: 2,
            sz_decimals: 2,
            product: crate::Product::Spot,
            base_asset_addr: None,
            quote_asset_addr: None,
            base_symbol: Some("APT".to_owned()),
            quote_symbol: Some("USDC".to_owned()),
        };
        let account = crate::AccountOverview {
            available_margin: None,
            equity: None,
            position: crate::Position {
                size: Decimal::ZERO,
                entry_price: Decimal::ZERO,
            },
            open_order_count: 0,
            spot_funds: None,
        };
        let first = simulation::run_offline_cycle(simulation::OfflineCycleInput {
            config: config.clone(),
            market: market.clone(),
            mid: dec!(10),
            account: account.clone(),
            pinned_spot_plan: None,
            spot_exit_price: None,
        })
        .expect("first cycle");
        assert_eq!(first.plan.bids.len(), 4);
        assert_eq!(first.plan.asks.len(), 4);
        assert_eq!(first.plan.bids[0].price, dec!(9.75));
        assert_eq!(first.plan.asks[0].price, dec!(10.25));

        let pinned_lower = first.pinned_spot_plan.as_ref().map(|plan| plan.lower);
        let second = simulation::run_offline_cycle(simulation::OfflineCycleInput {
            config: config.clone(),
            market: market.clone(),
            mid: dec!(9.8),
            account,
            pinned_spot_plan: first.pinned_spot_plan,
            spot_exit_price: None,
        })
        .expect("second cycle");
        assert_eq!(
            second.pinned_spot_plan.as_ref().map(|plan| plan.lower),
            pinned_lower
        );
        assert!(second.plan.all_levels().all(|level| level.size == dec!(1)));
    }
}
