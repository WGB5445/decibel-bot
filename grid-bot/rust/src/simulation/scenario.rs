//! JSON/YAML scenario fixtures for offline multi-step simulation.

use std::io::Write;

use anyhow::{Context, Result, bail};
use rust_decimal::Decimal;
use serde::Deserialize;

use crate::simulation::cycle::{OfflineCycleInput, OfflineCycleOutput, run_offline_cycle};
use crate::{
    AccountOverview, Allocation, GridConfig, Market, OutOfRangeAction, PerpMode, Position,
    PriceSource, Product, RangeBreakoutAction, RangeSpec, SpotExecutionConfig, SpotFunds,
};

#[derive(Clone, Debug, Deserialize)]
pub struct Scenario {
    pub name: String,
    #[serde(default)]
    pub product: ScenarioProduct,
    pub config: ScenarioConfig,
    pub market: ScenarioMarket,
    #[serde(default)]
    pub spot_exit_price: Option<Decimal>,
    pub steps: Vec<ScenarioStep>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScenarioProduct {
    #[default]
    Spot,
    Perp,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ScenarioConfig {
    #[serde(default = "default_total_count")]
    pub total_count: usize,
    #[serde(default)]
    pub perp_mode: ScenarioPerpMode,
    #[serde(default = "default_maker_fee")]
    pub maker_fee_rate: Decimal,
    #[serde(default = "default_preview_leverage")]
    pub preview_leverage: Decimal,
    #[serde(default)]
    pub range: ScenarioRange,
    #[serde(default)]
    pub allocation: ScenarioAllocation,
    #[serde(default)]
    pub range_breakout_action: ScenarioBreakoutAction,
    #[serde(default)]
    pub max_position: Option<Decimal>,
}

fn default_total_count() -> usize {
    8
}
fn default_maker_fee() -> Decimal {
    Decimal::new(1, 3)
}
fn default_preview_leverage() -> Decimal {
    Decimal::ONE
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScenarioPerpMode {
    #[default]
    Neutral,
    Long,
    Short,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ScenarioRange {
    Percent { percent: Decimal },
    StepPercent { percent: Decimal },
    Bounds { lower: Decimal, upper: Decimal },
}

impl Default for ScenarioRange {
    fn default() -> Self {
        Self::Percent {
            percent: Decimal::from(10),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ScenarioAllocation {
    TotalBudget { value: Decimal },
    FixedSize { value: Decimal },
}

impl Default for ScenarioAllocation {
    fn default() -> Self {
        Self::FixedSize {
            value: Decimal::ONE,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScenarioBreakoutAction {
    #[default]
    PauseAndAlert,
    ExtendGrid,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ScenarioMarket {
    pub name: String,
    pub tick_size: Decimal,
    pub lot_size: Decimal,
    #[serde(default = "default_min_size")]
    pub min_size: Decimal,
    #[serde(default = "default_px_decimals")]
    pub px_decimals: u32,
    #[serde(default = "default_sz_decimals")]
    pub sz_decimals: u32,
}

fn default_min_size() -> Decimal {
    Decimal::new(1, 2)
}
fn default_px_decimals() -> u32 {
    2
}
fn default_sz_decimals() -> u32 {
    2
}

#[derive(Clone, Debug, Deserialize)]
pub struct ScenarioStep {
    pub mid: Decimal,
    #[serde(default)]
    pub position: Option<Decimal>,
    #[serde(default)]
    pub available_margin: Option<Decimal>,
    #[serde(default)]
    pub spot_funds: Option<ScenarioSpotFunds>,
    #[serde(default)]
    pub expect: ScenarioStepExpect,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct ScenarioSpotFunds {
    pub quote: Decimal,
    pub base: Decimal,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct ScenarioStepExpect {
    #[serde(default)]
    pub bid_count: Option<usize>,
    #[serde(default)]
    pub ask_count: Option<usize>,
    #[serde(default)]
    pub first_bid_price: Option<Decimal>,
    #[serde(default)]
    pub first_ask_price: Option<Decimal>,
    #[serde(default)]
    pub pinned_lower: Option<Decimal>,
    #[serde(default)]
    pub pinned_upper: Option<Decimal>,
    #[serde(default)]
    pub per_grid_base_size: Option<Decimal>,
    #[serde(default)]
    pub all_level_sizes: Option<Decimal>,
    #[serde(default)]
    pub perp_blocked: Option<bool>,
    #[serde(default)]
    pub spot_breakout: Option<bool>,
    #[serde(default)]
    pub paused: Option<bool>,
    #[serde(default)]
    pub matched_count: Option<usize>,
    #[serde(default)]
    pub missing_count: Option<usize>,
    #[serde(default)]
    pub unmanaged_count: Option<usize>,
    #[serde(default)]
    pub reconcile_converged: Option<bool>,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct ScenarioStepResult {
    pub step: usize,
    pub mid: Decimal,
    pub output: OfflineCycleOutput,
}

impl Scenario {
    pub fn to_grid_config(&self) -> Result<GridConfig> {
        let product = match self.product {
            ScenarioProduct::Spot => Product::Spot,
            ScenarioProduct::Perp => Product::Perp,
        };
        let perp_mode = match self.config.perp_mode {
            ScenarioPerpMode::Neutral => PerpMode::Neutral,
            ScenarioPerpMode::Long => PerpMode::Long,
            ScenarioPerpMode::Short => PerpMode::Short,
        };
        let range = match self.config.range.clone() {
            ScenarioRange::Percent { percent } => RangeSpec::Percent { percent },
            ScenarioRange::StepPercent { percent } => RangeSpec::StepPercent { percent },
            ScenarioRange::Bounds { lower, upper } => RangeSpec::Bounds { lower, upper },
        };
        let allocation = match self.config.allocation.clone() {
            ScenarioAllocation::TotalBudget { value } => Allocation::TotalBudget(value),
            ScenarioAllocation::FixedSize { value } => Allocation::FixedSize(value),
        };
        let range_breakout_action = match self.config.range_breakout_action {
            ScenarioBreakoutAction::PauseAndAlert => RangeBreakoutAction::PauseAndAlert,
            ScenarioBreakoutAction::ExtendGrid => RangeBreakoutAction::ExtendGrid,
        };
        Ok(GridConfig {
            product,
            perp_mode,
            market_name: self.market.name.clone(),
            range,
            total_count: self.config.total_count,
            allocation,
            maker_fee_rate: self.config.maker_fee_rate,
            preview_leverage: self.config.preview_leverage,
            refresh: std::time::Duration::from_secs(3),
            price_source: PriceSource::Prices,
            spot: SpotExecutionConfig {
                range_breakout_action,
                ..SpotExecutionConfig::default()
            },
            max_position: self.config.max_position,
            out_of_range_action: OutOfRangeAction::default(),
        })
    }

    pub fn to_market(&self) -> Market {
        let product = match self.product {
            ScenarioProduct::Spot => Product::Spot,
            ScenarioProduct::Perp => Product::Perp,
        };
        Market {
            address: "0xscenario".to_owned(),
            name: self.market.name.clone(),
            tick_size: self.market.tick_size,
            lot_size: self.market.lot_size,
            min_size: self.market.min_size,
            px_decimals: self.market.px_decimals,
            sz_decimals: self.market.sz_decimals,
            product,
            base_asset_addr: None,
            quote_asset_addr: None,
            base_symbol: Some("BASE".to_owned()),
            quote_symbol: Some("QUOTE".to_owned()),
        }
    }

    fn account_for_step(&self, step: &ScenarioStep) -> AccountOverview {
        let spot_funds = step.spot_funds.as_ref().map(|funds| SpotFunds {
            base_symbol: "BASE".to_owned(),
            quote_symbol: "QUOTE".to_owned(),
            base_balance: funds.base,
            quote_balance: funds.quote,
            base_reserved: Decimal::ZERO,
            quote_reserved: Decimal::ZERO,
            quote_cross_balance: Decimal::ZERO,
        });
        AccountOverview {
            available_margin: step.available_margin,
            equity: None,
            position: Position {
                size: step.position.unwrap_or(Decimal::ZERO),
                entry_price: Decimal::ZERO,
            },
            open_order_count: 0,
            spot_funds,
        }
    }

    pub fn run(&self) -> Result<Vec<ScenarioStepResult>> {
        let mut config = self.to_grid_config()?;
        let market = self.to_market();
        let mut pinned_spot_plan = None;
        let mut results = Vec::with_capacity(self.steps.len());
        for (step_index, step) in self.steps.iter().enumerate() {
            let output = run_offline_cycle(OfflineCycleInput {
                config: config.clone(),
                market: market.clone(),
                mid: step.mid,
                account: self.account_for_step(step),
                pinned_spot_plan: pinned_spot_plan.clone(),
                spot_exit_price: self.spot_exit_price,
            })?;
            assert_step_expectations(step_index, step, &output)?;
            config = output.config.clone();
            pinned_spot_plan = output.pinned_spot_plan.clone();
            results.push(ScenarioStepResult {
                step: step_index,
                mid: step.mid,
                output,
            });
        }
        Ok(results)
    }
}

pub fn parse_scenario(raw: &str) -> Result<Scenario> {
    if raw.trim_start().starts_with('{') {
        serde_json::from_str(raw).context("parse scenario JSON")
    } else {
        serde_yaml::from_str(raw).context("parse scenario YAML")
    }
}

pub fn simulate_scenario_from_str(raw: &str) -> Result<Vec<ScenarioStepResult>> {
    parse_scenario(raw)?.run()
}

pub fn simulate_scenario<W: Write>(scenario: &Scenario, mut writer: W) -> Result<()> {
    for result in scenario.run()? {
        serde_json::to_writer(&mut writer, &result)?;
        writer.write_all(b"\n")?;
    }
    Ok(())
}

fn assert_step_expectations(
    step_index: usize,
    step: &ScenarioStep,
    output: &OfflineCycleOutput,
) -> Result<()> {
    let expect = &step.expect;
    let plan = &output.plan;
    if let Some(bid_count) = expect.bid_count {
        anyhow::ensure!(
            plan.bids.len() == bid_count,
            "step {step_index}: expected {bid_count} bids, got {}",
            plan.bids.len()
        );
    }
    if let Some(ask_count) = expect.ask_count {
        anyhow::ensure!(
            plan.asks.len() == ask_count,
            "step {step_index}: expected {ask_count} asks, got {}",
            plan.asks.len()
        );
    }
    if let Some(price) = expect.first_bid_price {
        let actual = plan.bids.first().map(|level| level.price);
        anyhow::ensure!(
            actual == Some(price),
            "step {step_index}: expected first bid {price}, got {actual:?}"
        );
    }
    if let Some(price) = expect.first_ask_price {
        let actual = plan.asks.first().map(|level| level.price);
        anyhow::ensure!(
            actual == Some(price),
            "step {step_index}: expected first ask {price}, got {actual:?}"
        );
    }
    if let Some(lower) = expect.pinned_lower {
        let actual = output.pinned_spot_plan.as_ref().map(|pinned| pinned.lower);
        anyhow::ensure!(
            actual == Some(lower),
            "step {step_index}: expected pinned lower {lower}, got {actual:?}"
        );
    }
    if let Some(upper) = expect.pinned_upper {
        let actual = output.pinned_spot_plan.as_ref().map(|pinned| pinned.upper);
        anyhow::ensure!(
            actual == Some(upper),
            "step {step_index}: expected pinned upper {upper}, got {actual:?}"
        );
    }
    if let Some(size) = expect.per_grid_base_size {
        anyhow::ensure!(
            plan.per_grid_base_size == Some(size),
            "step {step_index}: expected per_grid_base_size {size}, got {:?}",
            plan.per_grid_base_size
        );
    }
    if let Some(size) = expect.all_level_sizes {
        let uniform = plan.all_levels().all(|level| level.size == size);
        anyhow::ensure!(
            uniform,
            "step {step_index}: not all levels have size {size}"
        );
    }
    if let Some(blocked) = expect.perp_blocked {
        let actual = output.perp_blocked.is_some();
        anyhow::ensure!(
            actual == blocked,
            "step {step_index}: expected perp_blocked={blocked}, got {actual}"
        );
    }
    if let Some(breakout) = expect.spot_breakout {
        let actual = output.spot_breakout.is_some();
        anyhow::ensure!(
            actual == breakout,
            "step {step_index}: expected spot_breakout={breakout}, got {actual}"
        );
    }
    if let Some(paused) = expect.paused {
        anyhow::ensure!(
            output.paused == paused,
            "step {step_index}: expected paused={paused}, got {}",
            output.paused
        );
    }
    if expect.matched_count.is_some()
        || expect.missing_count.is_some()
        || expect.unmanaged_count.is_some()
        || expect.reconcile_converged.is_some()
    {
        bail!("reconcile expectations belong in reconcile integration tests, not run_offline_cycle")
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yaml_and_json_share_schema() {
        let json = r#"{
            "name": "smoke",
            "product": "spot",
            "config": {
                "total_count": 8,
                "range": { "kind": "percent", "percent": "10" },
                "allocation": { "kind": "fixed_size", "value": "1" }
            },
            "market": {
                "name": "APT/USDC",
                "tick_size": "0.01",
                "lot_size": "0.01"
            },
            "steps": [{ "mid": "10", "expect": { "bid_count": 4, "ask_count": 4 } }]
        }"#;
        let yaml = r#"
name: smoke
product: spot
config:
  total_count: 8
  range:
    kind: percent
    percent: "10"
  allocation:
    kind: fixed_size
    value: "1"
market:
  name: APT/USDC
  tick_size: "0.01"
  lot_size: "0.01"
steps:
  - mid: "10"
    expect:
      bid_count: 4
      ask_count: 4
"#;
        let from_json = parse_scenario(json).expect("json");
        let from_yaml = parse_scenario(yaml).expect("yaml");
        assert_eq!(from_json.name, from_yaml.name);
        assert_eq!(from_json.steps.len(), from_yaml.steps.len());
    }
}
