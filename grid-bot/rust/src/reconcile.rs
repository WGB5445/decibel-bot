//! Pure desired-state reconciliation for Decibel grid orders.
//!
//! This module deliberately does not submit or cancel orders. It compares a deterministic grid
//! plan with an exchange snapshot and reports the safe next action. Until Decibel exposes a
//! client-order identifier in the relevant order APIs, unmatched exchange orders are *unmanaged*
//! and must be reviewed by an operator rather than cancelled automatically.

use std::collections::BTreeSet;

use rust_decimal::Decimal;

use crate::{GridPlan, Side, round_down};

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DesiredOrder {
    pub side: Side,
    pub price: Decimal,
    pub size: Decimal,
}

/// Where a resting order came from, which determines whether it may be replaced automatically.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum OrderOrigin {
    /// An individually placed order. Decibel exposes no client-order ID for these, so ownership
    /// cannot be proven and an automatic bulk replacement must not silently remove it.
    #[default]
    Standalone,
    /// A level of this (subaccount, market)'s active bulk ladder. The bulk ABI atomically
    /// replaces the entire ladder, and only one ladder can exist per subaccount and market, so
    /// submitting a new bulk order necessarily supersedes these levels. They therefore never
    /// block replacement.
    Bulk,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ActualOrder {
    pub order_id: String,
    pub side: Side,
    pub price: Decimal,
    /// The currently resting quantity. An absent/zero remaining quantity must not match a new
    /// desired level because a partial or terminal order cannot safely be treated as full cover.
    pub remaining_size: Decimal,
    /// Defaulted so journals written before this field existed still deserialize, conservatively,
    /// as `Standalone`.
    #[serde(default)]
    pub origin: OrderOrigin,
}

/// Orders that must block an automatic bulk replacement: everything whose ownership cannot be
/// established. Levels of the account's own bulk ladder are excluded because a new bulk
/// submission replaces them atomically by design.
pub fn blocking_orders(orders: &[ActualOrder]) -> Vec<&ActualOrder> {
    orders
        .iter()
        .filter(|order| order.origin == OrderOrigin::Standalone)
        .collect()
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MatchedOrder {
    pub desired: DesiredOrder,
    pub actual: ActualOrder,
}

#[derive(Clone, Debug, Eq, PartialEq, Default)]
pub struct Reconciliation {
    /// Desired levels already covered by a resting order of exactly the requested price and size.
    pub matched: Vec<MatchedOrder>,
    /// Desired levels that have no matching resting order and could be placed after risk checks.
    pub missing: Vec<DesiredOrder>,
    /// Open orders which do not exactly correspond to this desired grid. They are never treated as
    /// bot-owned solely based on price/size; automatic cancellation would risk touching a manual
    /// order or an order from a previous process.
    pub unmanaged: Vec<ActualOrder>,
}

impl Reconciliation {
    pub fn is_converged(&self) -> bool {
        self.missing.is_empty() && self.unmanaged.is_empty()
    }

    pub fn summary(&self) -> String {
        format!(
            "{} matched, {} missing, {} unmanaged",
            self.matched.len(),
            self.missing.len(),
            self.unmanaged.len()
        )
    }
}

/// Converts all non-filled plan levels to quantized desired orders.
pub fn desired_orders(
    plan: &GridPlan,
    price_tick: Decimal,
    size_lot: Decimal,
) -> Vec<DesiredOrder> {
    plan.all_levels()
        .filter(|level| level.state != crate::LevelState::Filled)
        .map(|level| DesiredOrder {
            side: level.side,
            price: round_down(level.price, price_tick),
            size: round_down(level.size, size_lot),
        })
        .collect()
}

/// Compare desired grid levels to a snapshot of open orders.
///
/// Matching is one-to-one. Duplicate exchange orders only cover one desired level; the additional
/// order remains unmanaged. Quantizing both sides prevents representation noise from changing the
/// answer while keeping an order with a materially different price or remaining quantity visible.
pub fn reconcile(
    desired: &[DesiredOrder],
    actual: &[ActualOrder],
    price_tick: Decimal,
    size_lot: Decimal,
) -> Reconciliation {
    let mut consumed = BTreeSet::new();
    let mut result = Reconciliation::default();

    for wanted in desired {
        let wanted_price = round_down(wanted.price, price_tick);
        let wanted_size = round_down(wanted.size, size_lot);
        let found = actual.iter().enumerate().find(|(index, order)| {
            !consumed.contains(index)
                && order.side == wanted.side
                && round_down(order.price, price_tick) == wanted_price
                && round_down(order.remaining_size, size_lot) == wanted_size
        });
        if let Some((index, order)) = found {
            consumed.insert(index);
            result.matched.push(MatchedOrder {
                desired: wanted.clone(),
                actual: order.clone(),
            });
        } else {
            result.missing.push(wanted.clone());
        }
    }

    result.unmanaged = actual
        .iter()
        .enumerate()
        .filter(|(index, _)| !consumed.contains(index))
        .map(|(_, order)| order.clone())
        .collect();
    result
}

#[cfg(test)]
mod tests {
    use rust_decimal_macros::dec;

    use super::{ActualOrder, DesiredOrder, OrderOrigin, reconcile};
    use crate::Side;

    fn desired(
        side: Side,
        price: rust_decimal::Decimal,
        size: rust_decimal::Decimal,
    ) -> DesiredOrder {
        DesiredOrder { side, price, size }
    }

    fn actual(
        id: &str,
        side: Side,
        price: rust_decimal::Decimal,
        size: rust_decimal::Decimal,
    ) -> ActualOrder {
        ActualOrder {
            order_id: id.to_owned(),
            side,
            price,
            remaining_size: size,
            origin: OrderOrigin::Standalone,
        }
    }

    fn bulk_level(
        id: &str,
        side: Side,
        price: rust_decimal::Decimal,
        size: rust_decimal::Decimal,
    ) -> ActualOrder {
        ActualOrder {
            origin: OrderOrigin::Bulk,
            ..actual(id, side, price, size)
        }
    }

    #[test]
    fn reconciliation_matches_quantized_levels_one_to_one() {
        let desired = vec![
            desired(Side::Bid, dec!(99.01), dec!(1.23)),
            desired(Side::Ask, dec!(101.02), dec!(1.23)),
        ];
        let actual = vec![
            actual("bid-1", Side::Bid, dec!(99.019), dec!(1.239)),
            actual("ask-1", Side::Ask, dec!(101.029), dec!(1.239)),
        ];

        let result = reconcile(&desired, &actual, dec!(0.01), dec!(0.01));

        assert!(result.is_converged());
        assert_eq!(result.matched.len(), 2);
    }

    #[test]
    fn bulk_levels_do_not_block_replacement() {
        let orders = vec![
            bulk_level("bulk:1:Ask:0", Side::Ask, dec!(101), dec!(10)),
            actual("manual", Side::Bid, dec!(99), dec!(10)),
        ];
        let blocking = super::blocking_orders(&orders);
        assert_eq!(blocking.len(), 1);
        assert_eq!(blocking[0].order_id, "manual");
    }

    #[test]
    fn partial_or_different_size_order_is_missing_and_unmanaged() {
        let desired = vec![desired(Side::Bid, dec!(99), dec!(10))];
        let actual = vec![actual("partial", Side::Bid, dec!(99), dec!(4))];

        let result = reconcile(&desired, &actual, dec!(0.01), dec!(0.01));

        assert_eq!(result.missing.len(), 1);
        assert_eq!(result.unmanaged.len(), 1);
        assert!(!result.is_converged());
    }

    #[test]
    fn duplicate_order_does_not_hide_an_unmanaged_order() {
        let desired = vec![desired(Side::Ask, dec!(101), dec!(10))];
        let actual = vec![
            actual("expected", Side::Ask, dec!(101), dec!(10)),
            actual("duplicate", Side::Ask, dec!(101), dec!(10)),
        ];

        let result = reconcile(&desired, &actual, dec!(0.01), dec!(0.01));

        assert_eq!(result.matched.len(), 1);
        assert_eq!(result.unmanaged[0].order_id, "duplicate");
    }
}
