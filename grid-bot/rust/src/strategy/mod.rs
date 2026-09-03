//! Pluggable grid strategies for Spot and Perp products.

pub mod perp;
pub mod spot;

use rust_decimal::Decimal;

use crate::{GridConfig, GridPlan, Market, PerpMode, Product};
use anyhow::Result;

/// Inputs shared by every strategy when building a grid plan.
#[derive(Clone, Debug)]
pub struct StrategyContext {
    pub mid: Decimal,
    pub position: Option<Decimal>,
    pub pinned_per_grid_base_size: Option<Decimal>,
}

pub trait GridStrategy: Send + Sync {
    fn id(&self) -> &'static str;
    fn product(&self) -> Product;
    fn validate_config(&self, config: &GridConfig) -> Result<()>;
    fn build_plan(
        &self,
        config: &GridConfig,
        market: &Market,
        ctx: &StrategyContext,
    ) -> Result<GridPlan>;
}

struct SpotGridStrategy;

impl GridStrategy for SpotGridStrategy {
    fn id(&self) -> &'static str {
        "spot"
    }

    fn product(&self) -> Product {
        Product::Spot
    }

    fn validate_config(&self, config: &GridConfig) -> Result<()> {
        config.validate()
    }

    fn build_plan(
        &self,
        config: &GridConfig,
        market: &Market,
        ctx: &StrategyContext,
    ) -> Result<GridPlan> {
        spot::planning::build(config, market, ctx)
    }
}

static SPOT_STRATEGY: SpotGridStrategy = SpotGridStrategy;

pub fn resolve(config: &GridConfig) -> &'static dyn GridStrategy {
    match (config.product, config.perp_mode) {
        (Product::Spot, _) => &SPOT_STRATEGY,
        (Product::Perp, PerpMode::Neutral) => &perp::neutral::STRATEGY,
        (Product::Perp, PerpMode::Long) => &perp::long::STRATEGY,
        (Product::Perp, PerpMode::Short) => &perp::short::STRATEGY,
    }
}
