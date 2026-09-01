//! Guarded IOC execution for the two exceptional Spot operations: initial base acquisition and
//! optional exit liquidation. Grid maintenance itself never calls this module.

use anyhow::{Result, anyhow, bail};
use rust_decimal::Decimal;
use tokio::time::Instant;

use crate::{
    AccountOverview, BookLevel, DecibelClient, Market, OrderBook, SpotExecutionConfig,
    SpotFeeRates, round_down, round_up, submit_spot_ioc_order,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TakerSide {
    Buy,
    Sell,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TakerAttempt {
    pub size: Decimal,
    pub limit_price: Decimal,
    pub worst_book_price: Decimal,
}

#[derive(Clone, Debug, Default)]
pub struct GuardedTakerOutcome {
    pub filled_total: Decimal,
    pub remaining: Decimal,
    pub attempts: usize,
    pub transaction_hashes: Vec<String>,
}

impl GuardedTakerOutcome {
    pub fn fully_filled(&self, market: &Market) -> bool {
        self.remaining < market.min_size
    }
}

/// Calculate the immutable cap anchored at the first executable top-of-book quote.
pub fn price_cap(reference: Decimal, side: TakerSide, slippage_bps: Decimal) -> Result<Decimal> {
    if reference <= Decimal::ZERO || slippage_bps.is_sign_negative() {
        bail!("reference price must be positive and slippage bps must not be negative")
    }
    let bps = Decimal::from(10_000);
    Ok(match side {
        TakerSide::Buy => reference * (Decimal::ONE + slippage_bps / bps),
        TakerSide::Sell => reference * (Decimal::ONE - slippage_bps / bps),
    })
}

/// Walk the available order book only as far as the immutable price cap permits. The returned
/// price has an additional two-tick-or-bps buffer, but never exceeds the cap (buy) or drops below
/// it (sell).
#[allow(clippy::too_many_arguments)]
pub fn plan_ioc_attempt(
    book: &OrderBook,
    side: TakerSide,
    remaining_target: Decimal,
    cap: Decimal,
    market: &Market,
    price_buffer_bps: Decimal,
    max_quote_spend: Option<Decimal>,
    taker_fee_rate: Decimal,
) -> Result<Option<TakerAttempt>> {
    if remaining_target <= Decimal::ZERO || cap <= Decimal::ZERO {
        return Ok(None);
    }
    if price_buffer_bps.is_sign_negative() || taker_fee_rate.is_sign_negative() {
        bail!("price buffer and taker fee must not be negative")
    }
    let levels: &[BookLevel] = match side {
        TakerSide::Buy => &book.asks,
        TakerSide::Sell => &book.bids,
    };
    let mut available = Decimal::ZERO;
    let mut worst = None;
    for level in levels {
        let allowed = match side {
            TakerSide::Buy => level.price <= cap,
            TakerSide::Sell => level.price >= cap,
        };
        if !allowed {
            break;
        }
        available += level.size;
        worst = Some(level.price);
        if available >= remaining_target {
            break;
        }
    }
    let Some(worst_book_price) = worst else {
        return Ok(None);
    };
    let bps = Decimal::from(10_000);
    let buffer = (market.tick_size * Decimal::TWO).max(worst_book_price * price_buffer_bps / bps);
    let limit_price = match side {
        TakerSide::Buy => round_up((worst_book_price + buffer).min(cap), market.tick_size),
        TakerSide::Sell => round_down((worst_book_price - buffer).max(cap), market.tick_size),
    };
    if limit_price <= Decimal::ZERO {
        return Ok(None);
    }
    let mut size = round_down(available.min(remaining_target), market.lot_size);
    if let (TakerSide::Buy, Some(quote_budget)) = (side, max_quote_spend) {
        let affordable = quote_budget / (limit_price * (Decimal::ONE + taker_fee_rate));
        size = size.min(round_down(affordable, market.lot_size));
    }
    if size < market.min_size {
        return Ok(None);
    }
    Ok(Some(TakerAttempt {
        size,
        limit_price,
        worst_book_price,
    }))
}

fn base_balance(account: &AccountOverview) -> Result<Decimal> {
    account
        .spot_funds
        .as_ref()
        .map(|funds| funds.available_base())
        .ok_or_else(|| anyhow!("account overview did not return Spot PFS balances"))
}

/// Execute bounded IOC slices. A reference price and cap are fixed for the entire call, while the
/// REST depth snapshot is refreshed before each slice. Direct Move submissions do not expose an
/// order ID or `pendingCbs`; therefore this executor uses the only documented PFS observation
/// source (account overview) after every committed transaction and refuses to count unobserved
/// proceeds as fills.
#[allow(clippy::too_many_arguments)]
pub async fn execute_guarded_spot_ioc(
    network: &str,
    api: &DecibelClient,
    private_key: &str,
    subaccount: &str,
    market: &Market,
    side: TakerSide,
    target_size: Decimal,
    max_quote_spend: Option<Decimal>,
    fees: &SpotFeeRates,
    config: &SpotExecutionConfig,
) -> Result<GuardedTakerOutcome> {
    if target_size < market.min_size {
        return Ok(GuardedTakerOutcome {
            remaining: target_size.max(Decimal::ZERO),
            ..GuardedTakerOutcome::default()
        });
    }
    let initial_book = api.order_book(market, 50).await?;
    let reference = match side {
        TakerSide::Buy => initial_book.asks.first(),
        TakerSide::Sell => initial_book.bids.first(),
    }
    .ok_or_else(|| anyhow!("cannot start guarded IOC without executable book liquidity"))?
    .price;
    let cap = price_cap(
        reference,
        side,
        match side {
            TakerSide::Buy => config.entry_max_slippage_bps,
            TakerSide::Sell => config.exit_max_slippage_bps,
        },
    )?;
    let started = Instant::now();
    let mut outcome = GuardedTakerOutcome {
        remaining: target_size,
        ..GuardedTakerOutcome::default()
    };
    let mut quote_budget = max_quote_spend;
    while outcome.remaining >= market.min_size
        && outcome.attempts < config.entry_exit_max_attempts
        && started.elapsed() < config.entry_exit_timeout
    {
        let book = api.order_book(market, 50).await?;
        let Some(attempt) = plan_ioc_attempt(
            &book,
            side,
            outcome.remaining,
            cap,
            market,
            config.price_buffer_bps,
            quote_budget,
            fees.taker_rate,
        )?
        else {
            break;
        };
        let before = base_balance(&api.account(Some(subaccount), market).await?)?;
        let hash = submit_spot_ioc_order(
            network,
            private_key,
            subaccount,
            market,
            attempt.limit_price,
            attempt.size,
            side == TakerSide::Buy,
        )
        .await?;
        outcome.attempts += 1;
        outcome.transaction_hashes.push(hash);

        // The transaction itself is committed; wait for the PFS view to reflect its final state.
        // A no-change observation is deliberately not treated as a successful fill.
        let mut observed = Decimal::ZERO;
        for delay in &config.entry_exit_retry_backoff {
            if started.elapsed() >= config.entry_exit_timeout {
                break;
            }
            tokio::time::sleep(*delay).await;
            let after = base_balance(&api.account(Some(subaccount), market).await?)?;
            observed = match side {
                TakerSide::Buy => (after - before).max(Decimal::ZERO),
                TakerSide::Sell => (before - after).max(Decimal::ZERO),
            };
            if observed > Decimal::ZERO {
                break;
            }
        }
        if observed <= Decimal::ZERO {
            // No demonstrated PFS settlement may represent an unfilled IOC or an undocumented
            // CBS-pending request. Stop rather than submit a duplicate taker order.
            break;
        }
        let filled = observed.min(outcome.remaining);
        outcome.filled_total += filled;
        outcome.remaining -= filled;
        if let (TakerSide::Buy, Some(budget)) = (side, quote_budget.as_mut()) {
            *budget = (*budget - attempt.limit_price * filled * (Decimal::ONE + fees.taker_rate))
                .max(Decimal::ZERO);
        }
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use rust_decimal_macros::dec;

    use super::{TakerSide, plan_ioc_attempt, price_cap};
    use crate::{BookLevel, Market, OrderBook};

    fn market() -> Market {
        Market {
            address: "0x1".to_owned(),
            name: "BTC/USDC".to_owned(),
            tick_size: dec!(1),
            lot_size: dec!(0.001),
            min_size: dec!(0.001),
            px_decimals: 0,
            sz_decimals: 3,
            product: crate::Product::Spot,
            base_asset_addr: None,
            quote_asset_addr: None,
            base_symbol: Some("BTC".to_owned()),
            quote_symbol: Some("USDC".to_owned()),
        }
    }

    #[test]
    fn buy_walk_is_capped_and_buffered() {
        let book = OrderBook {
            asks: vec![
                BookLevel {
                    price: dec!(100),
                    size: dec!(1),
                },
                BookLevel {
                    price: dec!(101),
                    size: dec!(2),
                },
                BookLevel {
                    price: dec!(103),
                    size: dec!(9),
                },
            ],
            bids: vec![],
        };
        let cap = price_cap(dec!(100), TakerSide::Buy, dec!(200)).unwrap();
        let attempt = plan_ioc_attempt(
            &book,
            TakerSide::Buy,
            dec!(2.5),
            cap,
            &market(),
            dec!(5),
            None,
            dec!(0.001),
        )
        .unwrap()
        .unwrap();
        assert_eq!(attempt.size, dec!(2.5));
        assert_eq!(attempt.worst_book_price, dec!(101));
        assert!(attempt.limit_price <= cap);
        assert!(attempt.limit_price >= attempt.worst_book_price);
    }

    #[test]
    fn buy_walk_refuses_depth_outside_fixed_cap() {
        let book = OrderBook {
            asks: vec![BookLevel {
                price: dec!(102),
                size: dec!(1),
            }],
            bids: vec![],
        };
        let attempt = plan_ioc_attempt(
            &book,
            TakerSide::Buy,
            dec!(1),
            dec!(101),
            &market(),
            dec!(5),
            None,
            dec!(0),
        )
        .unwrap();
        assert!(attempt.is_none());
    }
}
