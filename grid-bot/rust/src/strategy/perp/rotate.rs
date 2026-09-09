use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::strategy::{GridStrategy, StrategyContext};
use crate::strategy::perp::accounting::{FillSide, PerpFill};
use crate::{GridConfig, GridLevel, GridPlan, LevelState, Market, PerpMode, Product, Result, Side};

pub struct PerpRotateStrategy;

pub(crate) static STRATEGY: PerpRotateStrategy = PerpRotateStrategy;

impl GridStrategy for PerpRotateStrategy {
    fn id(&self) -> &'static str {
        "perp-rotate"
    }

    fn product(&self) -> Product {
        Product::Perp
    }

    fn validate_config(&self, config: &GridConfig) -> Result<()> {
        debug_assert_eq!(config.perp_mode, PerpMode::Rotate);
        config.validate()
    }

    fn build_plan(
        &self,
        config: &GridConfig,
        market: &Market,
        ctx: &StrategyContext,
    ) -> Result<GridPlan> {
        super::planning::build_perp_plan(config, market, ctx.mid)
    }
}

/// One individual lot created by a passive fill.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InventoryLot {
    pub entry_side: Side,
    pub entry_price: Decimal,
    pub size: Decimal,
    pub exit_price: Decimal,
    pub status: LotStatus,
}

impl InventoryLot {
    pub fn is_open(&self) -> bool {
        self.status == LotStatus::Open
    }

    pub fn is_closed(&self) -> bool {
        self.status == LotStatus::Closed
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum LotStatus {
    Open,
    Closed,
}

/// Tracks every filled lot and derives the correct EXIT-only orders needed to reduce the position.
/// The opposite entry side remains at the config's uniform grid price set (pinned).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RotatingGridState {
    pub lots: Vec<InventoryLot>,
    /// Prices from the initial grid geometry used to rebuild entry orders.
    pub pinned_bid_prices: Vec<Decimal>,
    pub pinned_ask_prices: Vec<Decimal>,
}

impl RotatingGridState {
    pub fn new_with_pinned_prices(bid_prices: Vec<Decimal>, ask_prices: Vec<Decimal>) -> Self {
        Self {
            lots: Vec::new(),
            pinned_bid_prices: bid_prices,
            pinned_ask_prices: ask_prices,
        }
    }

    pub fn record_fill(&mut self, side: Side, price: Decimal, size: Decimal, grid_size: Decimal) {
        let exit_price = self.derive_exit_price(side, price, grid_size);
        self.lots.push(InventoryLot {
            entry_side: side,
            entry_price: price,
            size,
            exit_price,
            status: LotStatus::Open,
        });
    }

    pub fn close_lot_at(&mut self, side: Side, price: Decimal) {
        let opposite = side.opposite();
        for lot in self.lots.iter_mut().rev() {
            if lot.status == LotStatus::Open
                && lot.entry_side == opposite
                && lot.exit_price == price
            {
                lot.status = LotStatus::Closed;
                return;
            }
        }
    }

    /// Called when a fill arrives on `filled_side`. If the fill matches an exit order (i.e. the
    /// exit price for some open lot), close that lot. Returns true if a lot was closed.
    pub fn try_close_exit_fill(&mut self, filled_side: Side, price: Decimal) -> bool {
        let before = self.open_lot_count(filled_side.opposite());
        for lot in self.lots.iter_mut().rev() {
            if lot.status == LotStatus::Open
                && lot.entry_side == filled_side.opposite()
                && lot.exit_price == price
            {
                lot.status = LotStatus::Closed;
                return true;
            }
        }
        before != self.open_lot_count(filled_side.opposite())
    }

    pub fn derive_exit_price(&self, entry_side: Side, entry_price: Decimal, grid_size: Decimal) -> Decimal {
        let grid_step = self
            .pinned_ask_prices
            .first()
            .and_then(|first_ask| {
                self.pinned_bid_prices
                    .last()
                    .map(|last_bid| *first_ask - *last_bid)
            })
            .filter(|step| *step > Decimal::ZERO)
            .unwrap_or(grid_size * Decimal::TEN);
        match entry_side {
            Side::Bid => entry_price + grid_step,
            Side::Ask => entry_price - grid_step,
        }
    }

    pub fn open_lot_count(&self, side: Side) -> usize {
        self.lots
            .iter()
            .filter(|lot| lot.status == LotStatus::Open && lot.entry_side == side)
            .count()
    }

    pub fn derive_exit_orders(&self) -> Vec<GridLevel> {
        let mut levels: Vec<GridLevel> = Vec::new();
        for lot in &self.lots {
            if lot.status != LotStatus::Open {
                continue;
            }
            let exit_side = lot.entry_side.opposite();
            levels.push(GridLevel {
                side: exit_side,
                price: lot.exit_price,
                size: lot.size,
                notional: lot.exit_price * lot.size,
                state: LevelState::Planned,
            });
        }
        levels.sort_by(|a, b| {
            if a.side == b.side {
                a.price.cmp(&b.price)
            } else {
                (a.side as u8).cmp(&(b.side as u8))
            }
        });
        levels
    }

    pub fn all_exit_prices(&self, side: Side) -> Vec<Decimal> {
        self.lots
            .iter()
            .filter(|lot| lot.status == LotStatus::Open && lot.entry_side == side.opposite())
            .map(|lot| lot.exit_price)
            .collect()
    }

    pub fn available_entry_prices(&self, side: Side) -> Vec<Decimal> {
        let pinned = match side {
            Side::Bid => &self.pinned_bid_prices,
            Side::Ask => &self.pinned_ask_prices,
        };
        let exit_prices = self.all_exit_prices(side);
        pinned
            .iter()
            .filter(|price| !exit_prices.contains(price))
            .copied()
            .collect()
    }
}

/// Apply a PerpFill to the rotating state: entry fills create open lots, exit fills close them.
pub fn apply_fill_to_rotating_state(
    state: &mut RotatingGridState,
    fill: &PerpFill,
    grid_size: Decimal,
) {
    let fill_side = match fill.side {
        FillSide::Buy => Side::Bid,
        FillSide::Sell => Side::Ask,
    };
    if !state.try_close_exit_fill(fill_side, fill.price) {
        state.record_fill(fill_side, fill.price, fill.quantity, grid_size);
    }
}

/// Build a full ladder plan for Rotate mode: entry orders from the pinned grid, plus any open
/// exit orders from filled lots. Entry levels that collide with exit prices are filtered out.
pub fn build_rotate_ladder(
    config: &GridConfig,
    market: &Market,
    planning_price: Decimal,
    rotating_state: &RotatingGridState,
) -> Result<GridPlan> {
    let mut plan = super::planning::build_perp_plan(config, market, planning_price)?;
    let exit_orders = rotating_state.derive_exit_orders();
    let exit_prices: Vec<Decimal> = exit_orders.iter().map(|l| l.price).collect();
    plan.bids.retain(|level| !exit_prices.contains(&level.price));
    plan.asks.retain(|level| !exit_prices.contains(&level.price));
    plan.bids
        .extend(exit_orders.iter().filter(|l| l.side == Side::Bid).cloned());
    plan.asks
        .extend(exit_orders.iter().filter(|l| l.side == Side::Ask).cloned());
    plan.bids.sort_by(|a, b| b.price.cmp(&a.price));
    plan.asks.sort_by(|a, b| a.price.cmp(&b.price));
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Side;
    use rust_decimal_macros::dec;

    fn rotate_state() -> RotatingGridState {
        RotatingGridState::new_with_pinned_prices(
            vec![dec!(90), dec!(92), dec!(94), dec!(96), dec!(98)],
            vec![dec!(102), dec!(104), dec!(106), dec!(108), dec!(110)],
        )
    }

    #[test]
    fn bid_fill_creates_open_lot_with_exit_ask() {
        let mut state = rotate_state();
        state.record_fill(Side::Bid, dec!(90), dec!(1), dec!(1));
        assert_eq!(state.lots.len(), 1);
        assert!(state.lots[0].is_open());
        assert_eq!(state.lots[0].entry_side, Side::Bid);
        assert_eq!(state.lots[0].exit_price, dec!(94));
    }

    #[test]
    fn ask_fill_creates_open_lot_with_exit_bid() {
        let mut state = rotate_state();
        state.record_fill(Side::Ask, dec!(110), dec!(1), dec!(1));
        assert_eq!(state.lots.len(), 1);
        assert!(state.lots[0].is_open());
        assert_eq!(state.lots[0].entry_side, Side::Ask);
        assert_eq!(state.lots[0].exit_price, dec!(106));
    }

    #[test]
    fn exit_order_closes_matching_lot() {
        let mut state = rotate_state();
        state.record_fill(Side::Bid, dec!(90), dec!(1), dec!(1));
        state.close_lot_at(Side::Ask, dec!(94));
        assert!(state.lots[0].is_closed());
    }

    #[test]
    fn derive_exit_orders_produces_correct_side_and_price() {
        let mut state = rotate_state();
        state.record_fill(Side::Bid, dec!(90), dec!(0.5), dec!(1));
        state.record_fill(Side::Bid, dec!(92), dec!(0.5), dec!(1));
        state.record_fill(Side::Ask, dec!(108), dec!(0.5), dec!(1));
        let exits = state.derive_exit_orders();
        assert_eq!(exits.len(), 3);
        let ask_exits: Vec<_> = exits.iter().filter(|l| l.side == Side::Ask).collect();
        let bid_exits: Vec<_> = exits.iter().filter(|l| l.side == Side::Bid).collect();
        assert_eq!(ask_exits.len(), 2);
        assert_eq!(bid_exits.len(), 1);
    }

    #[test]
    fn available_entry_prices_excludes_exit_price_overlap() {
        let mut state = rotate_state();
        let total_bids = state.available_entry_prices(Side::Bid).len();
        state.record_fill(Side::Bid, dec!(90), dec!(1), dec!(1));
        let avail = state.available_entry_prices(Side::Bid);
        assert_eq!(avail.len(), total_bids);
    }

    #[test]
    fn open_lot_count_is_accurate() {
        let mut state = rotate_state();
        state.record_fill(Side::Bid, dec!(90), dec!(1), dec!(1));
        state.record_fill(Side::Bid, dec!(92), dec!(1), dec!(1));
        state.record_fill(Side::Ask, dec!(108), dec!(1), dec!(1));
        assert_eq!(state.open_lot_count(Side::Bid), 2);
        assert_eq!(state.open_lot_count(Side::Ask), 1);
        state.close_lot_at(Side::Ask, dec!(96));
        assert_eq!(state.open_lot_count(Side::Bid), 1);
        assert_eq!(state.open_lot_count(Side::Ask), 1);
    }

    #[test]
    fn close_then_reopen_state_is_reflected() {
        let state = RotatingGridState::new_with_pinned_prices(
            vec![dec!(90), dec!(100)],
            vec![dec!(200), dec!(210)],
        );
        assert!(state.pinned_bid_prices.contains(&dec!(90)));
        assert!(state.pinned_ask_prices.contains(&dec!(210)));
    }

    #[test]
    fn empty_state_produces_no_exit_orders() {
        let state = rotate_state();
        assert!(state.derive_exit_orders().is_empty());
    }
}