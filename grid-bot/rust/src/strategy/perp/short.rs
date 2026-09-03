use crate::{GridConfig, GridPlan, Market, PerpMode, Product, Result};
use crate::strategy::{GridStrategy, StrategyContext};
use super::build_perp_plan;

pub struct PerpShortStrategy;

impl GridStrategy for PerpShortStrategy {
    fn id(&self) -> &'static str {
        "perp-short"
    }

    fn product(&self) -> Product {
        Product::Perp
    }

    fn validate_config(&self, config: &GridConfig) -> Result<()> {
        debug_assert_eq!(config.perp_mode, PerpMode::Short);
        config.validate()
    }

    fn build_plan(
        &self,
        config: &GridConfig,
        market: &Market,
        ctx: &StrategyContext,
    ) -> Result<GridPlan> {
        build_perp_plan(config, market, ctx)
    }
}

pub static STRATEGY: PerpShortStrategy = PerpShortStrategy;
