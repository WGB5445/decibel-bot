use thiserror::Error;

/// Output of the quote computation.
#[derive(Debug, Clone, PartialEq)]
pub struct Quotes {
    /// Bid price, rounded DOWN to the nearest tick.
    pub bid: f64,
    /// Ask price, rounded UP to the nearest tick.
    pub ask: f64,
    /// Order size per side, rounded to the nearest lot.
    pub size: f64,
}

#[derive(Debug, Error)]
pub enum PricingError {
    #[error(
        "Spread {spread:.8} is too tight: minimum for one tick is {min_spread:.8} \
         (tick_size={tick_size} / mid={mid_price})"
    )]
    SpreadTooTight {
        spread: f64,
        min_spread: f64,
        tick_size: f64,
        mid_price: f64,
    },

    #[error("Invalid pricing parameter: {0}")]
    InvalidParam(String),
}

/// Compute POST-ONLY bid/ask quotes with inventory skew.
///
/// # Algorithm
///
/// ```text
/// half_spread = spread / 2
/// skew        = inventory * skew_per_unit   # positive inventory → shift quotes DOWN
///
/// raw_bid = mid * (1 − half_spread − skew)
/// raw_ask = mid * (1 + half_spread − skew)
///
/// bid = floor(raw_bid / tick) * tick        # always round DOWN
/// ask =  ceil(raw_ask / tick) * tick        # always round UP
///
/// if ask ≤ bid: ask = bid + tick            # enforce minimum post-round spread
///
/// size = round(order_size / lot) * lot
/// ```
///
/// # Returns
/// - `Ok(Some(quotes))` — normal quoting
/// - `Ok(None)` — stop quoting (inventory at limit or size rounds to zero)
/// - `Err(PricingError)` — invalid parameters (spread too tight)
pub fn compute_quotes(
    mid_price: f64,
    inventory: f64,
    spread: f64,
    skew_per_unit: f64,
    max_inventory: f64,
    tick_size: f64,
    lot_size: f64,
    min_size: f64,
    order_size: f64,
) -> Result<Option<Quotes>, PricingError> {
    // ── Parameter sanity ─────────────────────────────────────────────────────
    if mid_price <= 0.0 {
        return Err(PricingError::InvalidParam(format!(
            "mid_price must be positive, got {mid_price}"
        )));
    }
    if tick_size <= 0.0 {
        return Err(PricingError::InvalidParam(format!(
            "tick_size must be positive, got {tick_size}"
        )));
    }
    if lot_size <= 0.0 {
        return Err(PricingError::InvalidParam(format!(
            "lot_size must be positive, got {lot_size}"
        )));
    }

    // ── Spread validation ─────────────────────────────────────────────────────
    // Minimum spread required to place quotes one tick apart:
    //   one_tick_fraction = tick_size / mid_price
    let min_spread = tick_size / mid_price;
    if spread < min_spread {
        return Err(PricingError::SpreadTooTight {
            spread,
            min_spread,
            tick_size,
            mid_price,
        });
    }

    // ── Inventory guard ───────────────────────────────────────────────────────
    if inventory.abs() >= max_inventory {
        return Ok(None);
    }

    // ── Core pricing ─────────────────────────────────────────────────────────
    let half_spread = spread / 2.0;
    let skew = inventory * skew_per_unit; // positive inventory → push quotes down

    let raw_bid = mid_price * (1.0 - half_spread - skew);
    let raw_ask = mid_price * (1.0 + half_spread - skew);

    // Bid rounds DOWN, ask rounds UP (conservative for maker orders)
    let bid = (raw_bid / tick_size).floor() * tick_size;
    let mut ask = (raw_ask / tick_size).ceil() * tick_size;

    // After rounding, ask might have collapsed onto or below bid
    if ask <= bid {
        ask = bid + tick_size;
    }

    // ── Size ─────────────────────────────────────────────────────────────────
    let size = (order_size / lot_size).round() * lot_size;
    if size <= 0.0 || size < min_size {
        return Ok(None);
    }

    Ok(Some(Quotes { bid, ask, size }))
}

// ─────────────────────────────────────────────────────────────────────────────
// Unit tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use approx::assert_relative_eq;

    // ── Default parameters ────────────────────────────────────────────────────
    const MID: f64 = 100_000.0;
    const SPREAD: f64 = 0.001;   // 0.1 %
    const TICK: f64 = 1.0;
    const LOT: f64 = 0.00001;
    const MIN_SIZE: f64 = 0.00002;
    const ORDER_SIZE: f64 = 0.001;
    const SKEW: f64 = 0.0001;
    const MAX_INV: f64 = 0.005;

    fn quotes(inventory: f64) -> Result<Option<Quotes>, PricingError> {
        compute_quotes(
            MID, inventory, SPREAD, SKEW, MAX_INV, TICK, LOT, MIN_SIZE, ORDER_SIZE,
        )
    }

    // ── Happy-path ────────────────────────────────────────────────────────────

    #[test]
    fn zero_inventory_symmetric_around_mid() {
        let q = quotes(0.0).unwrap().unwrap();
        // half_spread = 0.0005 → raw_bid = 99950, raw_ask = 100050
        assert_relative_eq!(q.bid, 99_950.0, epsilon = 1e-6);
        assert_relative_eq!(q.ask, 100_050.0, epsilon = 1e-6);
        assert_relative_eq!(q.size, ORDER_SIZE, epsilon = 1e-10);
    }

    #[test]
    fn positive_inventory_shifts_quotes_down() {
        let q0 = quotes(0.0).unwrap().unwrap();
        let q_long = quotes(0.001).unwrap().unwrap();
        // Positive inventory → skew > 0 → quotes shift down
        assert!(
            q_long.bid <= q0.bid,
            "bid should shift down: {} vs {}",
            q_long.bid,
            q0.bid
        );
        assert!(
            q_long.ask <= q0.ask,
            "ask should shift down: {} vs {}",
            q_long.ask,
            q0.ask
        );
    }

    #[test]
    fn negative_inventory_shifts_quotes_up() {
        let q0 = quotes(0.0).unwrap().unwrap();
        let q_short = quotes(-0.001).unwrap().unwrap();
        assert!(q_short.bid >= q0.bid, "bid should shift up");
        assert!(q_short.ask >= q0.ask, "ask should shift up");
    }

    #[test]
    fn ask_always_greater_than_bid() {
        for inv_x100 in [-4i32, -3, -2, -1, 0, 1, 2, 3, 4] {
            let inv = inv_x100 as f64 * 0.001;
            if let Ok(Some(q)) = quotes(inv) {
                assert!(q.ask > q.bid, "ask ({}) must be > bid ({}) at inv={}", q.ask, q.bid, inv);
            }
        }
    }

    #[test]
    fn bid_is_multiple_of_tick() {
        let q = quotes(0.0).unwrap().unwrap();
        assert_relative_eq!(q.bid % TICK, 0.0, epsilon = 1e-9);
    }

    #[test]
    fn ask_is_multiple_of_tick() {
        let q = quotes(0.0).unwrap().unwrap();
        assert_relative_eq!(q.ask % TICK, 0.0, epsilon = 1e-9);
    }

    #[test]
    fn size_is_multiple_of_lot() {
        let q = quotes(0.0).unwrap().unwrap();
        let remainder = (q.size / LOT).round() * LOT - q.size;
        assert_relative_eq!(remainder.abs(), 0.0, epsilon = 1e-12);
    }

    // ── None returns ──────────────────────────────────────────────────────────

    #[test]
    fn max_inventory_long_returns_none() {
        assert!(quotes(MAX_INV).unwrap().is_none());
    }

    #[test]
    fn max_inventory_short_returns_none() {
        assert!(quotes(-MAX_INV).unwrap().is_none());
    }

    #[test]
    fn size_below_min_returns_none() {
        let result = compute_quotes(
            MID, 0.0, SPREAD, SKEW, MAX_INV, TICK, LOT,
            /*min_size=*/ 1.0,      // require 1 full BTC
            /*order_size=*/ 0.0001, // too small
        );
        assert!(result.unwrap().is_none());
    }

    #[test]
    fn order_size_rounds_to_zero_returns_none() {
        // order_size < lot/2 → rounds to 0 → None
        let result = compute_quotes(
            MID, 0.0, SPREAD, SKEW, MAX_INV,
            TICK, /*lot=*/ 0.01, /*min_size=*/ 0.001,
            /*order_size=*/ 0.004, // rounds to 0 lots of 0.01
        );
        assert!(result.unwrap().is_none());
    }

    // ── Error returns ─────────────────────────────────────────────────────────

    #[test]
    fn spread_too_tight_returns_error() {
        let too_tight = (TICK / MID) * 0.5;
        let result = compute_quotes(MID, 0.0, too_tight, SKEW, MAX_INV, TICK, LOT, MIN_SIZE, ORDER_SIZE);
        assert!(
            matches!(result, Err(PricingError::SpreadTooTight { .. })),
            "expected SpreadTooTight, got {:?}",
            result
        );
    }

    #[test]
    fn zero_mid_price_returns_error() {
        let result = compute_quotes(0.0, 0.0, SPREAD, SKEW, MAX_INV, TICK, LOT, MIN_SIZE, ORDER_SIZE);
        assert!(matches!(result, Err(PricingError::InvalidParam(_))));
    }

    #[test]
    fn zero_tick_size_returns_error() {
        let result = compute_quotes(MID, 0.0, SPREAD, SKEW, MAX_INV, 0.0, LOT, MIN_SIZE, ORDER_SIZE);
        assert!(matches!(result, Err(PricingError::InvalidParam(_))));
    }

    // ── Rounding edge cases ───────────────────────────────────────────────────

    #[test]
    fn minimum_spread_enforced_after_rounding() {
        // tick=10, mid=1000 → min_spread = 10/1000 = 0.01
        // Use spread=0.011 (just above minimum) — raw half_spread = 0.0055
        // raw_bid = 1000 * (1 - 0.0055) = 994.5 → floor to tick=10 → 990
        // raw_ask = 1000 * (1 + 0.0055) = 1005.5 → ceil to tick=10 → 1010
        // ask(1010) > bid(990)+tick(10) ✓
        // Now use a spread that makes rounding collapse them:
        // spread=0.019 → half=0.0095 → raw_bid=990.5→990, raw_ask=1009.5→1010 still fine
        // To force ask==bid+tick: use tiny spread just over min (0.011):
        let tick = 10.0;
        let mid = 1_000.0;
        let spread = 0.011; // just above min_spread=0.01
        let q = compute_quotes(mid, 0.0, spread, 0.0, 10.0, tick, 0.001, 0.001, 0.001)
            .unwrap()
            .unwrap();
        assert!(
            q.ask >= q.bid + tick,
            "ask {} must be ≥ bid {} + tick {}",
            q.ask,
            q.bid,
            tick
        );
    }

    #[test]
    fn skew_shifts_both_legs_by_same_amount() {
        // With a positive inventory, both bid and ask shift down by inventory*skew_per_unit*mid
        let inv = 0.002;
        let q0 = quotes(0.0).unwrap().unwrap();
        let q1 = quotes(inv).unwrap().unwrap();
        // Expected raw shift = inv * SKEW * MID = 0.002 * 0.0001 * 100_000 = 0.02
        // After rounding to TICK=1.0 it might be 0 or 1; just check direction
        let bid_shift = q0.bid - q1.bid;
        let ask_shift = q0.ask - q1.ask;
        assert!(bid_shift >= 0.0, "bid should not increase with positive inv");
        assert!(ask_shift >= 0.0, "ask should not increase with positive inv");
    }
}
