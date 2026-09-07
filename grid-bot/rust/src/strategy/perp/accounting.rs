//! Deterministic Perp fill accounting.
//!
//! The exchange position remains the risk authority. This ledger attributes fills that belong to
//! the current durable bot run so realized PnL, fees, and funding are never inferred from a
//! changing exchange average entry price.

use std::collections::BTreeSet;

use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FillSide {
    Buy,
    Sell,
}

impl FillSide {
    fn signed_quantity(self, quantity: Decimal) -> Decimal {
        match self {
            Self::Buy => quantity,
            Self::Sell => -quantity,
        }
    }
}

/// One immutable Perp fill from Decibel's trade history. All monetary fields are USDC according
/// to the documented Perp DTO, so no price-time conversion is required.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PerpFill {
    pub id: String,
    pub side: FillSide,
    pub price: Decimal,
    pub quantity: Decimal,
    pub fee_quote: Decimal,
    pub realized_pnl_quote: Decimal,
    pub realized_funding_quote: Decimal,
    pub timestamp: DateTime<Utc>,
}

/// One authoritative funding settlement, with a positive amount representing funds received.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FundingPayment {
    pub id: String,
    pub amount_quote: Decimal,
    pub timestamp: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PerpAccounting {
    /// Stable venue IDs make ingestion idempotent across overlapping history pages and restarts.
    #[serde(default)]
    processed_fill_ids: BTreeSet<String>,
    #[serde(default)]
    processed_funding_ids: BTreeSet<String>,
    #[serde(default)]
    pub position_base: Decimal,
    #[serde(default)]
    pub average_entry_price: Decimal,
    #[serde(default)]
    pub realized_gross_quote: Decimal,
    #[serde(default)]
    pub trade_fees_quote: Decimal,
    #[serde(default)]
    pub funding_received_quote: Decimal,
    #[serde(default)]
    pub funding_paid_quote: Decimal,
    /// False means history was incomplete; documented Perp fills always carry a USDC fee.
    #[serde(default = "default_true")]
    pub fees_complete: bool,
    /// True after at least one documented Perp trade supplied realized_funding_amount.
    #[serde(default)]
    pub funding_complete: bool,
    #[serde(default)]
    pub last_fill_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_funding_at: Option<DateTime<Utc>>,
    /// Beginning of this strategy's accounting window. A flat restart must not replay earlier
    /// account-wide fills into this run's realized PnL.
    #[serde(default)]
    pub baseline_at: Option<DateTime<Utc>>,
}

fn default_true() -> bool {
    true
}

impl Default for PerpAccounting {
    fn default() -> Self {
        Self {
            processed_fill_ids: BTreeSet::new(),
            processed_funding_ids: BTreeSet::new(),
            position_base: Decimal::ZERO,
            average_entry_price: Decimal::ZERO,
            realized_gross_quote: Decimal::ZERO,
            trade_fees_quote: Decimal::ZERO,
            funding_received_quote: Decimal::ZERO,
            funding_paid_quote: Decimal::ZERO,
            fees_complete: true,
            funding_complete: false,
            last_fill_at: None,
            last_funding_at: None,
            baseline_at: None,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PerpPnlSnapshot {
    pub exchange_position_base: Decimal,
    pub ledger_position_base: Decimal,
    pub reconciliation_delta_base: Decimal,
    pub average_entry_price: Option<Decimal>,
    pub mark_price: Option<Decimal>,
    pub unrealized_gross_quote: Option<Decimal>,
    pub realized_gross_quote: Decimal,
    pub trade_fees_quote: Option<Decimal>,
    pub funding_pnl_quote: Option<Decimal>,
    pub net_pnl_quote: Option<Decimal>,
}

impl PerpAccounting {
    /// Mark fills visible before a brand-new strategy starts as historical baseline data. They do
    /// not belong to this run and must never be replayed into its realized PnL.
    pub fn seed_historical_fills<'a>(
        &mut self,
        fills: impl IntoIterator<Item = &'a PerpFill>,
        baseline_at: DateTime<Utc>,
    ) {
        self.processed_fill_ids
            .extend(fills.into_iter().map(|fill| fill.id.clone()));
        self.baseline_at = Some(baseline_at);
    }

    pub fn history_cursor(&self) -> Option<DateTime<Utc>> {
        self.last_fill_at.or(self.baseline_at)
    }

    pub fn apply_fill(&mut self, fill: &PerpFill) -> Result<bool> {
        if fill.id.trim().is_empty() {
            bail!("Perp fill has no stable venue ID")
        }
        if fill.quantity <= Decimal::ZERO || fill.price <= Decimal::ZERO {
            bail!("Perp fill {} has non-positive price or quantity", fill.id)
        }
        if !self.processed_fill_ids.insert(fill.id.clone()) {
            return Ok(false);
        }

        let delta = fill.side.signed_quantity(fill.quantity);
        let previous = self.position_base;
        if previous.is_zero() || previous.is_sign_positive() == delta.is_sign_positive() {
            let total = previous.abs() + delta.abs();
            self.average_entry_price = if previous.is_zero() {
                fill.price
            } else {
                (self.average_entry_price * previous.abs() + fill.price * delta.abs()) / total
            };
            self.position_base += delta;
        } else {
            let remaining = previous + delta;
            if remaining.is_zero() {
                self.position_base = Decimal::ZERO;
                self.average_entry_price = Decimal::ZERO;
            } else if remaining.is_sign_positive() == previous.is_sign_positive() {
                self.position_base = remaining;
            } else {
                self.position_base = remaining;
                self.average_entry_price = fill.price;
            }
        }

        // Decibel returns fee_amount independently from realized PnL. A negative value is a
        // rebate and is valid: subtracting a negative fee correctly increases net PnL.
        self.trade_fees_quote += fill.fee_quote;
        self.realized_gross_quote += fill.realized_pnl_quote;
        if fill.realized_funding_quote.is_sign_positive() {
            self.funding_received_quote += fill.realized_funding_quote;
        } else {
            self.funding_paid_quote += -fill.realized_funding_quote;
        }
        self.funding_complete = true;
        if self
            .last_fill_at
            .is_none_or(|previous| fill.timestamp > previous)
        {
            self.last_fill_at = Some(fill.timestamp);
        }
        Ok(true)
    }

    pub fn apply_funding_payment(&mut self, payment: &FundingPayment) -> Result<bool> {
        if payment.id.trim().is_empty() {
            bail!("funding payment has no stable settlement ID")
        }
        if !self.processed_funding_ids.insert(payment.id.clone()) {
            return Ok(false);
        }
        if payment.amount_quote.is_sign_positive() {
            self.funding_received_quote += payment.amount_quote;
        } else {
            self.funding_paid_quote += -payment.amount_quote;
        }
        self.funding_complete = true;
        if self
            .last_funding_at
            .is_none_or(|previous| payment.timestamp > previous)
        {
            self.last_funding_at = Some(payment.timestamp);
        }
        Ok(true)
    }

    pub fn pnl_snapshot(
        &self,
        exchange_position_base: Decimal,
        mark_price: Option<Decimal>,
    ) -> PerpPnlSnapshot {
        let average_entry_price = (!self.position_base.is_zero()
            && self.average_entry_price > Decimal::ZERO)
            .then_some(self.average_entry_price);
        let unrealized_gross_quote = average_entry_price
            .zip(mark_price.filter(|price| *price > Decimal::ZERO))
            .map(|(entry, mark)| self.position_base * (mark - entry));
        let trade_fees_quote = self.fees_complete.then_some(self.trade_fees_quote);
        let funding_pnl_quote = self
            .funding_complete
            .then_some(self.funding_received_quote - self.funding_paid_quote);
        let net_pnl_quote = unrealized_gross_quote
            .zip(trade_fees_quote)
            .zip(funding_pnl_quote)
            .map(|((unrealized, fees), funding)| {
                self.realized_gross_quote + unrealized - fees + funding
            });
        PerpPnlSnapshot {
            exchange_position_base,
            ledger_position_base: self.position_base,
            reconciliation_delta_base: exchange_position_base - self.position_base,
            average_entry_price,
            mark_price,
            unrealized_gross_quote,
            realized_gross_quote: self.realized_gross_quote,
            trade_fees_quote,
            funding_pnl_quote,
            net_pnl_quote,
        }
    }

    pub fn position_matches_exchange(
        &self,
        exchange_position_base: Decimal,
        lot_size: Decimal,
    ) -> bool {
        (exchange_position_base - self.position_base).abs() < lot_size
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;

    use super::{FillSide, FundingPayment, PerpAccounting, PerpFill};

    fn fill(
        id: &str,
        side: FillSide,
        price: rust_decimal::Decimal,
        quantity: rust_decimal::Decimal,
    ) -> PerpFill {
        PerpFill {
            id: id.to_owned(),
            side,
            price,
            quantity,
            fee_quote: dec!(1),
            realized_pnl_quote: Decimal::ZERO,
            realized_funding_quote: Decimal::ZERO,
            timestamp: Utc::now(),
        }
    }

    #[test]
    fn realizes_long_and_short_reductions_without_changing_residual_basis() {
        let mut accounting = PerpAccounting::default();
        accounting
            .apply_fill(&fill("b1", FillSide::Buy, dec!(100), dec!(2)))
            .unwrap();
        accounting
            .apply_fill(&fill("b2", FillSide::Buy, dec!(110), dec!(2)))
            .unwrap();
        let mut first_close = fill("s1", FillSide::Sell, dec!(120), dec!(3));
        first_close.realized_pnl_quote = dec!(45);
        accounting.apply_fill(&first_close).unwrap();
        assert_eq!(accounting.position_base, dec!(1));
        assert_eq!(accounting.average_entry_price, dec!(105));
        assert_eq!(accounting.realized_gross_quote, dec!(45));

        let mut second_close = fill("s2", FillSide::Sell, dec!(90), dec!(3));
        second_close.realized_pnl_quote = dec!(-15);
        accounting.apply_fill(&second_close).unwrap();
        assert_eq!(accounting.position_base, dec!(-2));
        assert_eq!(accounting.average_entry_price, dec!(90));
        assert_eq!(accounting.realized_gross_quote, dec!(30));
    }

    #[test]
    fn only_counts_each_fill_and_funding_settlement_once() {
        let mut accounting = PerpAccounting::default();
        let trade = fill("fill-1", FillSide::Buy, dec!(100), dec!(1));
        assert!(accounting.apply_fill(&trade).unwrap());
        assert!(!accounting.apply_fill(&trade).unwrap());
        let payment = FundingPayment {
            id: "funding-1".to_owned(),
            amount_quote: dec!(-2.5),
            timestamp: Utc::now(),
        };
        assert!(accounting.apply_funding_payment(&payment).unwrap());
        assert!(!accounting.apply_funding_payment(&payment).unwrap());
        let snapshot = accounting.pnl_snapshot(dec!(1), Some(dec!(105)));
        assert_eq!(snapshot.unrealized_gross_quote, Some(dec!(5)));
        assert_eq!(snapshot.funding_pnl_quote, Some(dec!(-2.5)));
        assert_eq!(snapshot.net_pnl_quote, Some(dec!(1.5)));
    }

    #[test]
    fn signed_rebates_and_funding_are_booked_once_in_net_pnl() {
        let mut accounting = PerpAccounting::default();
        let mut trade = fill("fill-1", FillSide::Buy, dec!(100), dec!(1));
        trade.fee_quote = dec!(-0.25);
        accounting.apply_fill(&trade).unwrap();
        let payment = FundingPayment {
            id: "funding-1".to_owned(),
            amount_quote: dec!(2),
            timestamp: Utc::now(),
        };
        accounting.apply_funding_payment(&payment).unwrap();
        let snapshot = accounting.pnl_snapshot(dec!(1), Some(dec!(105)));
        assert_eq!(snapshot.trade_fees_quote, Some(dec!(-0.25)));
        assert_eq!(snapshot.net_pnl_quote, Some(dec!(7.25)));
    }
}
