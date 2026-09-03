//! Synchronous planning cycle shared by live runtime, integration tests, and `simulate` CLI.

use anyhow::Result;
use rust_decimal::Decimal;

use crate::strategy::perp::runtime::perp_submission_blocked;
use crate::{
    AccountOverview, GridConfig, GridPlan, Market, Product, RangeBreakoutAction, RangeSpec,
    build_plan, build_plan_with_per_grid_base_size,
};

/// Inputs for one offline planning cycle (no async I/O).
#[derive(Clone, Debug)]
pub struct OfflineCycleInput {
    pub config: GridConfig,
    pub market: Market,
    pub mid: Decimal,
    pub account: AccountOverview,
    pub pinned_spot_plan: Option<GridPlan>,
    pub spot_exit_price: Option<Decimal>,
}

/// Planning outcome for one offline cycle.
#[derive(Clone, Debug, serde::Serialize)]
pub struct OfflineCycleOutput {
    pub plan: GridPlan,
    pub pinned_spot_plan: Option<GridPlan>,
    #[serde(skip)]
    pub config: GridConfig,
    pub perp_blocked: Option<String>,
    pub spot_breakout: Option<String>,
    pub spot_stop_loss_triggered: bool,
    pub budget_rejected: Option<String>,
    pub paused: bool,
}

pub fn run_offline_cycle(input: OfflineCycleInput) -> Result<OfflineCycleOutput> {
    match input.config.product {
        Product::Spot => run_spot_offline_cycle(input),
        Product::Perp => run_perp_offline_cycle(input),
    }
}

fn run_spot_offline_cycle(input: OfflineCycleInput) -> Result<OfflineCycleOutput> {
    let mut config = input.config;
    let market = input.market;
    let mid = input.mid;
    let mut pinned_spot_plan = input.pinned_spot_plan;
    let mut spot_stop_loss_triggered = false;
    let mut spot_breakout = None;
    let mut budget_rejected = None;
    let mut paused = false;

    if let Some(stop) = input.spot_exit_price
        && mid <= stop
    {
        spot_stop_loss_triggered = true;
        paused = true;
        let plan = pinned_spot_plan
            .clone()
            .map(|pinned| pinned.project_spot(mid, market.tick_size))
            .transpose()?
            .unwrap_or_else(|| build_plan(&config, &market, mid).expect("initial plan"));
        return Ok(OfflineCycleOutput {
            plan,
            pinned_spot_plan,
            config,
            perp_blocked: None,
            spot_breakout,
            spot_stop_loss_triggered,
            budget_rejected,
            paused,
        });
    }

    if pinned_spot_plan
        .as_ref()
        .is_some_and(|plan| plan.per_grid_base_size.is_none())
    {
        let upgraded = pinned_spot_plan
            .as_ref()
            .expect("checked above")
            .pin_spot_per_grid_base_size(&config, &market)?;
        pinned_spot_plan = Some(upgraded);
    }

    if pinned_spot_plan.is_none() {
        pinned_spot_plan = Some(build_plan(&config, &market, mid)?);
    }

    let (lower, upper) = pinned_spot_plan
        .as_ref()
        .map(|plan| (plan.lower, plan.upper))
        .expect("Spot plan was pinned above");

    if mid < lower || mid > upper {
        let direction = if mid < lower { "below" } else { "above" };
        let reason = format!("Spot mid {mid} broke {direction} pinned range [{lower}, {upper}]");
        match config.spot.range_breakout_action {
            RangeBreakoutAction::PauseAndAlert => {
                spot_breakout = Some(reason);
                paused = true;
            }
            RangeBreakoutAction::ExtendGrid => {
                let shifted_range = match config.range.clone() {
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
                    other => other,
                };
                config.range = shifted_range;
                let fixed_size = pinned_spot_plan
                    .as_ref()
                    .and_then(|plan| plan.per_grid_base_size);
                let shifted =
                    build_plan_with_per_grid_base_size(&config, &market, mid, fixed_size)?;
                pinned_spot_plan = Some(shifted);
            }
        }
    }

    let pinned = pinned_spot_plan
        .as_ref()
        .expect("Spot plan was pinned above");
    let plan = pinned.project_spot(mid, market.tick_size)?;
    if let Err(error) = plan.enforce_spot_budget(&config) {
        budget_rejected = Some(format!(
            "fixed Spot per-grid size no longer fits the configured budget after re-centering: {error:#}"
        ));
    }

    Ok(OfflineCycleOutput {
        plan,
        pinned_spot_plan,
        config,
        perp_blocked: None,
        spot_breakout,
        spot_stop_loss_triggered,
        budget_rejected,
        paused,
    })
}

fn run_perp_offline_cycle(input: OfflineCycleInput) -> Result<OfflineCycleOutput> {
    let config = input.config;
    let market = input.market;
    let mid = input.mid;
    let plan = build_plan(&config, &market, mid)?;
    let perp_blocked = perp_submission_blocked(
        &config,
        &plan,
        input.account.position.size,
        input.account.available_margin,
    );
    Ok(OfflineCycleOutput {
        plan,
        pinned_spot_plan: None,
        config,
        perp_blocked,
        spot_breakout: None,
        spot_stop_loss_triggered: false,
        budget_rejected: None,
        paused: false,
    })
}
