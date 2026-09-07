use super::build_perp_plan;
use crate::strategy::{GridStrategy, StrategyContext};
use crate::{GridConfig, GridPlan, Market, PerpMode, Product, Result};

pub struct PerpNeutralStrategy;

impl GridStrategy for PerpNeutralStrategy {
    fn id(&self) -> &'static str {
        "perp-neutral"
    }

    fn product(&self) -> Product {
        Product::Perp
    }

    fn validate_config(&self, config: &GridConfig) -> Result<()> {
        debug_assert_eq!(config.perp_mode, PerpMode::Neutral);
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

pub static STRATEGY: PerpNeutralStrategy = PerpNeutralStrategy;
