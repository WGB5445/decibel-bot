pub mod convergence;
pub mod long;
pub mod neutral;
pub mod planning;
pub mod risk;
pub mod runtime;
pub mod short;

use super::StrategyContext;
use crate::{GridConfig, GridPlan, Market, Result};

pub(crate) fn build_perp_plan(
    config: &GridConfig,
    market: &Market,
    ctx: &StrategyContext,
) -> Result<GridPlan> {
    planning::build_perp_plan(config, market, ctx.mid)
}
