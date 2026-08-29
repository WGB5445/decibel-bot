//! Decibel grid planner, monitor, and explicitly confirmed Aptos executor.

use std::{str::FromStr, time::Duration};

use anyhow::{Context, Result, anyhow, bail};
use aptos_sdk::{
    Aptos, AptosConfig,
    account::Ed25519Account,
    transaction::{InputEntryFunctionData, TransactionBuilder, move_none, sign_transaction},
    types::AccountAddress,
};
use chrono::{DateTime, Utc};
use clap::ValueEnum;
use futures_util::{SinkExt, StreamExt};
use profile::FundingOrderStore;
use reqwest::{Client as HttpClient, header};
use rust_decimal::{Decimal, RoundingStrategy, prelude::ToPrimitive};
use serde_json::Value;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message, client::IntoClientRequest, http::HeaderValue},
};

pub mod i18n;
pub mod journal;
pub mod profile;
pub mod reconcile;

pub const MAX_LEVELS_PER_SIDE: usize = 40;

/// Validate only properties that are true for every Decibel bearer API key.
///
/// The server is the authority for whether a key exists, is active, and has access; use
/// [`DecibelClient::verify_api_key`] for that remote check.
pub fn validate_api_key_format(api_key: &str) -> Result<()> {
    if api_key.is_empty() {
        bail!("API key is empty")
    }
    if api_key.len() > 512 {
        bail!("API key is too long")
    }
    if api_key.chars().any(char::is_whitespace) {
        bail!("API key must not contain whitespace")
    }
    if api_key.chars().any(char::is_control) {
        bail!("API key contains a control character")
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum Product {
    Spot,
    Perp,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum PerpMode {
    Neutral,
    Long,
    Short,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum PriceSource {
    Prices,
    Depth,
}

#[derive(Clone, Debug)]
pub enum RangeSpec {
    Bounds { lower: Decimal, upper: Decimal },
    Percent { percent: Decimal },
    StepPercent { percent: Decimal },
}

#[derive(Clone, Debug)]
pub enum Allocation {
    TotalBudget(Decimal),
    FixedSize(Decimal),
}

#[derive(Clone, Debug)]
pub struct GridConfig {
    pub product: Product,
    pub perp_mode: PerpMode,
    pub market_name: String,
    pub range: RangeSpec,
    /// Combined bid and ask count. The maximum is 80 = 40 per side.
    pub total_count: usize,
    pub allocation: Allocation,
    pub maker_fee_rate: Decimal,
    pub preview_leverage: Decimal,
    pub refresh: Duration,
    pub price_source: PriceSource,
}

impl GridConfig {
    pub fn validate(&self) -> Result<()> {
        if !(2..=MAX_LEVELS_PER_SIDE * 2).contains(&self.total_count) {
            bail!("grid count must be between 2 and 80")
        }
        if self.maker_fee_rate.is_sign_negative() || self.maker_fee_rate >= Decimal::ONE {
            bail!("maker fee rate must be >= 0 and < 1")
        }
        if self.preview_leverage <= Decimal::ZERO {
            bail!("preview leverage must be positive")
        }
        match self.range {
            RangeSpec::Bounds { lower, upper } if lower >= upper || lower <= Decimal::ZERO => {
                bail!("lower price must be positive and below upper price")
            }
            RangeSpec::Percent { percent } | RangeSpec::StepPercent { percent }
                if percent <= Decimal::ZERO || percent >= Decimal::from(100) =>
            {
                bail!("range/step percent must be > 0 and < 100")
            }
            _ => {}
        }
        match self.allocation {
            Allocation::TotalBudget(v) | Allocation::FixedSize(v) if v <= Decimal::ZERO => {
                bail!("budget or order size must be positive")
            }
            _ => {}
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct Market {
    pub address: String,
    pub name: String,
    pub tick_size: Decimal,
    pub lot_size: Decimal,
    pub min_size: Decimal,
    pub px_decimals: u32,
    pub sz_decimals: u32,
    pub product: Product,
    pub base_asset_addr: Option<String>,
    pub quote_asset_addr: Option<String>,
    pub base_symbol: Option<String>,
    pub quote_symbol: Option<String>,
}

#[derive(Clone, Debug)]
pub struct BookLevel {
    pub price: Decimal,
    pub size: Decimal,
}

#[derive(Clone, Debug, Default)]
pub struct OrderBook {
    pub bids: Vec<BookLevel>,
    pub asks: Vec<BookLevel>,
}

#[derive(Clone, Debug)]
pub struct Position {
    pub size: Decimal,
    pub entry_price: Decimal,
}

#[derive(Clone, Debug)]
pub struct AccountOverview {
    pub available_margin: Option<Decimal>,
    pub equity: Option<Decimal>,
    pub position: Position,
    pub open_order_count: usize,
    pub spot_funds: Option<SpotFunds>,
}

#[derive(Clone, Debug)]
pub struct SpotFunds {
    pub base_symbol: String,
    pub quote_symbol: String,
    pub base_balance: Decimal,
    pub quote_balance: Decimal,
    pub base_reserved: Decimal,
    pub quote_reserved: Decimal,
    /// Withdrawable USDC held in the Cross/collateral account rather than in the spot PFS.
    ///
    /// Observed behaviour on testnet: spot sell proceeds settle here, not into `spot.positions`,
    /// so a spot grid funded only from PFS quote can sell its base inventory down and never
    /// recycle the proceeds into bids. Whether the spot bulk entry function may *spend* this
    /// balance directly is NOT established by any read-only endpoint — it depends on the Move
    /// module's funding source. Treat it as diagnostic until proven by an on-chain trial.
    pub quote_cross_balance: Decimal,
}

impl SpotFunds {
    /// `base_balance`/`quote_balance` come from `spot.positions`, and the account overview's own
    /// arithmetic proves that figure is already net of `in_flight_orders` reservations:
    /// `spot.total_usd` equals the sum of `positions[].usd_value` PLUS `reserved_usd_value`, so a
    /// reserved amount is not double-present in `positions.amount`. Subtracting `base_reserved`/
    /// `quote_reserved` again here would double-count it and can zero out a real balance (see
    /// `in_flight_reservation_is_classified_by_asset_address_from_positions`, verified against a
    /// live account: 8.078783 APT free + 70 APT reserved were both real, but the old formula
    /// reported 0 available). `base_reserved`/`quote_reserved` are kept on the struct for display
    /// and future verification, not for this calculation.
    pub fn available_base(&self) -> Decimal {
        self.base_balance.max(Decimal::ZERO)
    }

    /// Quote available to the Spot grid, from the subaccount's PFS only.
    ///
    /// The Cross/collateral balance is deliberately NOT included. `spot_order_public_api::
    /// source_bulk_funds_from_pfs` asserts against `primary_fungible_store::balance` alone, and
    /// `dex_accounts_spot_extension::place_spot_bulk_order_to_subaccount` documents that bulk
    /// orders source funds "from the subaccount's PFS only — CBS sourcing is intentionally not
    /// supported". Counting Cross here makes the bot plan bids it cannot fund and the chain
    /// rejects the whole submission with `EINSUFFICIENT_PFS_FUNDS(0x1)`.
    pub fn available_quote(&self) -> Decimal {
        self.quote_balance.max(Decimal::ZERO)
    }

    /// Base usable when *replacing* this market's bulk ladder.
    ///
    /// `source_bulk_funds_from_pfs` credits whatever already sits in the existing bulk escrow
    /// against the new requirement and only withdraws the delta from PFS:
    /// `base_delta = if (base_needed > existing_base) { base_needed - existing_base } else { 0 }`.
    /// The reserved amount reported by `in_flight_orders` is exactly that escrow, so for a
    /// replacement it is spendable, not locked away.
    pub fn available_base_for_bulk(&self) -> Decimal {
        (self.base_balance + self.base_reserved).max(Decimal::ZERO)
    }

    /// Quote counterpart of [`Self::available_base_for_bulk`]. Still PFS-sourced: the escrow
    /// credit does not make Cross funds reachable.
    pub fn available_quote_for_bulk(&self) -> Decimal {
        (self.quote_balance + self.quote_reserved).max(Decimal::ZERO)
    }

    /// Withdrawable Cross USDC that could be moved into PFS to fund bids. Diagnostic only — it
    /// is not spendable by the bulk entry function until the operator transfers it.
    pub fn quote_cross_balance(&self) -> Decimal {
        self.quote_cross_balance.max(Decimal::ZERO)
    }
}

#[derive(Clone, Debug)]
pub struct Trade {
    pub price: Decimal,
    pub size: Decimal,
    pub timestamp_ms: i64,
}

/// Result of a confirmed on-chain bulk grid submission.
#[derive(Clone, Debug)]
pub struct ExecutionResult {
    pub transaction_hash: String,
    pub product: Product,
    pub bid_count: usize,
    pub ask_count: usize,
}

// Result of the optional automatic Spot base-inventory funding step. The live execution path
// invokes this before sizing/shrinking a Spot bulk ladder when the planned asks exceed PFS base.
#[derive(Clone, Debug)]
pub struct SpotFundingResult {
    pub base_gap_before: Decimal,
    pub bought_base: Decimal,
    pub transaction_hash: Option<String>,
    pub borrowed_from_grid_quote: Decimal,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpotFundingPlan {
    pub base_gap: Decimal,
    pub quote_gap: Decimal,
    pub required_quote_for_grid: Decimal,
    pub spare_quote: Decimal,
    pub buy_price: Option<Decimal>,
    pub buy_quantity: Decimal,
    pub borrowed_from_grid_quote: Decimal,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BulkOrderParameters {
    pub sequence_number: u64,
    pub bid_prices: Vec<u64>,
    pub bid_sizes: Vec<u64>,
    pub ask_prices: Vec<u64>,
    pub ask_sizes: Vec<u64>,
}

const MAINNET_PACKAGE: &str = "0x50ead22afd6ffd9769e3b3d6e0e64a2a350d68e8b102c4e72e33d0b8cfdfdb06";
const TESTNET_PACKAGE: &str = "0xe7da2794b1d8af76532ed95f38bfdf1136abfd8ea3a240189971988a83101b7f";
/// Decibel's testnet USDC metadata object. Mainnet assets must be supplied explicitly by the
/// caller because metadata addresses are network-specific.
pub const TESTNET_USDC_METADATA: &str =
    "0x5428acf5c112826d0c74ae1cd2de9030f53d1d01235e6c2621d967bf914ee1c8";

fn package_for_network(network: &str) -> Result<&'static str> {
    match network.trim().to_ascii_lowercase().as_str() {
        "mainnet" => Ok(MAINNET_PACKAGE),
        "testnet" => Ok(TESTNET_PACKAGE),
        other => bail!("unsupported execution network {other}; expected mainnet or testnet"),
    }
}

fn aptos_for_network(network: &str) -> Result<Aptos> {
    Ok(Aptos::new(
        match network.trim().to_ascii_lowercase().as_str() {
            "mainnet" => AptosConfig::mainnet(),
            "testnet" => AptosConfig::testnet(),
            other => bail!("unsupported execution network {other}; expected mainnet or testnet"),
        },
    )?)
}

/// Build, sign, submit, and wait for an official Spot or Perp bulk order transaction.
/// Spot: reads real PFS balances, automatically shrinks the grid if needed, and submits.
/// Perp: submitted as-configured (no automatic adjustment).
pub async fn execute_bulk_grid(
    network: &str,
    api_key: &str,
    private_key: &str,
    subaccount: &str,
    market: &Market,
    plan: &GridPlan,
) -> Result<ExecutionResult> {
    let subaccount_str = subaccount.trim();
    if subaccount_str.is_empty() {
        bail!("subaccount address is required for live execution")
    }
    let client = DecibelClient::new(network, api_key)?;
    let mut execution_plan = plan.clone();
    if market.product == Product::Spot {
        let account = client.account(Some(subaccount_str), market).await?;
        let funds = account.spot_funds.clone().ok_or_else(|| {
            anyhow!(
                "spot funds unavailable for {}: account_overviews did not include spot_overview; refusing to submit bulk order without a local PFS balance check",
                market.name
            )
        })?;

        // Spot bulk orders consume PFS (primary fungible store) inventory only. If PFS USDC
        // is insufficient, the program does NOT automatically transfer Cross/CBS collateral into
        // PFS — that requires ChangingCollateralFundsMovement on the signer address, which is
        // separate from TradeSpotAllMarkets. Instead the grid is scaled to the available PFS
        // balances and submitted as-is.
        let bulk_quote_available = funds.available_quote_for_bulk();
        let bulk_base_available = funds.available_base_for_bulk();
        if bulk_quote_available < execution_plan.quote_required
            || bulk_base_available < execution_plan.base_required
        {
            let adjustment = shrink_spot_to_available(
                &mut execution_plan,
                bulk_quote_available,
                bulk_base_available,
                market,
            )?;
            println!("Spot PFS balances limited the grid: {adjustment}");
        }
    }
    let sequence = client
        .next_bulk_sequence(subaccount_str, &market.address, market.product)
        .await?;
    let key = normalize_private_key(private_key)?;
    let signer =
        Ed25519Account::from_private_key_hex(&key).context("invalid Aptos Ed25519 private key")?;
    let subaccount_addr: AccountAddress = subaccount_str
        .parse()
        .context("invalid subaccount address")?;
    let market_addr: AccountAddress = market.address.parse().context("invalid market address")?;
    let network_name = network.trim().to_ascii_lowercase();
    let package = match network_name.as_str() {
        "mainnet" => MAINNET_PACKAGE,
        "testnet" => TESTNET_PACKAGE,
        other => bail!("unsupported execution network {other}; expected mainnet or testnet"),
    };
    let aptos = Aptos::new(match network_name.as_str() {
        "mainnet" => AptosConfig::mainnet(),
        _ => AptosConfig::testnet(),
    })?;
    let bids: Vec<&GridLevel> = execution_plan
        .bids
        .iter()
        .filter(|level| level.state != LevelState::Filled)
        .collect();
    let asks: Vec<&GridLevel> = execution_plan
        .asks
        .iter()
        .filter(|level| level.state != LevelState::Filled)
        .collect();
    if bids.is_empty() && asks.is_empty() {
        bail!("refusing to submit empty bulk order")
    }
    let bulk = prepare_bulk_order_parameters(sequence, &bids, &asks, market)?;
    let (entry_function, product_label, required_permission) = match market.product {
        Product::Perp => (
            format!("{package}::dex_accounts_entry::place_bulk_orders_to_subaccount"),
            "Perp",
            "Subaccount owner, TradePerpsAllMarkets, or TradePerpsOnMarket for this market",
        ),
        Product::Spot => (
            format!("{package}::dex_accounts_spot_entry::place_spot_bulk_order_to_subaccount"),
            "Spot",
            "Subaccount owner or delegate with TradeSpotAllMarkets",
        ),
    };
    let payload = InputEntryFunctionData::new(&entry_function)
        .arg(subaccount_addr)
        .arg(market_addr)
        .arg(bulk.sequence_number)
        .arg(bulk.bid_prices)
        .arg(bulk.bid_sizes)
        .arg(bulk.ask_prices)
        .arg(bulk.ask_sizes)
        // Option<T> is already BCS-encoded by move_none(); use arg_raw rather than arg,
        // otherwise the SDK encodes the bytes as vector<u8>, causing
        // FAILED_TO_DESERIALIZE_ARGUMENT at the Move entry function.
        .arg_raw(move_none()) // builder_address: Option<address>
        .arg_raw(move_none()) // builder_fees: Option<u64>
        .build()
        .context("build Perp bulk-order transaction")?;
    // 0.5 APT is the hard cap for this transaction's gas budget (1 APT = 100_000_000 octas).
    // The SDK default is 2_000_000 gas units, which can reserve more than a small funded wallet
    // can afford at the current gas-unit price.
    const MAX_GAS_OCTAS: u64 = 50_000_000;
    let sequence_number = aptos.get_sequence_number(signer.address()).await?;
    let gas_price = aptos
        .fullnode()
        .estimate_gas_price()
        .await?
        .data
        .recommended();
    if gas_price == 0 {
        bail!("Aptos returned a zero gas unit price")
    }
    let max_gas_amount = MAX_GAS_OCTAS / gas_price;
    if max_gas_amount == 0 {
        bail!("gas price {gas_price} octas exceeds the 0.5 APT transaction cap")
    }
    let chain_id = aptos.ensure_chain_id().await?;
    let raw = TransactionBuilder::new()
        .sender(signer.address())
        .sequence_number(sequence_number)
        .payload(payload)
        .max_gas_amount(max_gas_amount)
        .gas_unit_price(gas_price)
        .chain_id(chain_id)
        .expiration_from_now(600)
        .build()
        .context("build Perp bulk-order transaction with 0.5 APT gas cap")?;
    let signed = sign_transaction(&raw, &signer).with_context(|| {
        format!("sign {product_label} bulk-order transaction ({entry_function})")
    })?;
    let response = aptos
        .submit_and_wait(&signed, Some(Duration::from_secs(60)))
        .await
        .with_context(|| {
            format!(
                "submit {product_label} bulk-order transaction ({entry_function}); signer={} subaccount={} market={} required_permission={required_permission}",
                signer.address(), subaccount_str, market.address
            )
        })?;
    if !response
        .data
        .get("success")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        bail!(
            "Perp bulk-order transaction failed: {}",
            response
                .data
                .get("vm_status")
                .and_then(Value::as_str)
                .unwrap_or("unknown VM status")
        )
    }
    Ok(ExecutionResult {
        transaction_hash: response
            .data
            .get("hash")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        product: market.product,
        bid_count: bids.len(),
        ask_count: asks.len(),
    })
}

/// Bounded taker-buy sizing for the Spot base inventory a grid's ask side needs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpotTakerFunding {
    /// Base still missing for the plan's ask side.
    pub base_gap: Decimal,
    /// Quote left over after fully reserving the plan's bid requirement.
    pub quote_surplus: Decimal,
    /// IOC limit price: the best ask plus a bounded sweep allowance.
    pub limit_price: Decimal,
    /// Lot-rounded quantity affordable without touching the bid reserve.
    pub quantity: Decimal,
}

/// How many IOC sweeps may be attempted before giving up and letting the ask side shrink.
const MAX_TAKER_FUNDING_ATTEMPTS: usize = 6;

/// Size a single aggressive (IOC) base purchase that cannot consume quote the grid's own bids
/// need. Pure arithmetic so the cost bound is unit-testable without touching the network.
pub fn compute_spot_taker_funding(
    funds: &SpotFunds,
    grid: &GridPlan,
    best_ask: Decimal,
    market: &Market,
) -> Result<SpotTakerFunding> {
    if best_ask <= Decimal::ZERO {
        bail!("best ask must be positive to size a Spot taker funding order")
    }
    // Sweep allowance over the best ask so a thin top level does not stall funding, while still
    // bounding how far up the book a single IOC may walk.
    let slippage = Decimal::new(3, 3);
    // Taker-fee headroom, so a complete fill still cannot encroach on the grid's bid reserve.
    let fee_buffer = Decimal::new(1, 3);
    // Existing bulk escrow is already credited by the replacement ABI, so it counts toward the
    // target inventory. Excluding it would overbuy base when repairing an undersized ladder.
    let base_gap = (grid.base_required - funds.available_base_for_bulk()).max(Decimal::ZERO);
    // Quote spare for funding, measured consistently with `base_gap` above.
    //
    // The replacement credits the resting ladder's escrow against the new bids' requirement, so
    // what the new bids still need from PFS is `quote_required - escrow`, and everything beyond
    // that is spare. Measuring the surplus against free PFS alone would report zero spare
    // whenever a ladder is resting (free < quote_required) and refuse to fund at all.
    // The IOC itself can only draw on free PFS, so cap the surplus by that.
    let quote_surplus = (funds.available_quote_for_bulk() - grid.quote_required)
        .max(Decimal::ZERO)
        .min(funds.available_quote());
    let limit_price = round_up(best_ask * (Decimal::ONE + slippage), market.tick_size);
    if limit_price <= Decimal::ZERO {
        bail!(
            "funding limit price rounds to zero at tick size {}",
            market.tick_size
        )
    }
    let affordable = quote_surplus / (limit_price * (Decimal::ONE + fee_buffer));
    let quantity = round_down(base_gap.min(affordable), market.lot_size);
    Ok(SpotTakerFunding {
        base_gap,
        quote_surplus,
        limit_price,
        quantity,
    })
}

/// Aggressively buy the Spot base inventory the grid's ask side needs, using IOC orders.
///
/// Deliberately scoped to *initial* grid placement. Re-buying inventory after every sell fill
/// would hand back the captured spread plus taker fees, so callers must invoke this only when no
/// bulk ladder is resting for the (subaccount, market) pair.
pub async fn fund_spot_base_for_grid(
    network: &str,
    api_key: &str,
    private_key: &str,
    subaccount: &str,
    market: &Market,
    plan: &GridPlan,
) -> Result<SpotFundingResult> {
    if market.product != Product::Spot {
        bail!("automatic base funding is only available for Spot markets")
    }
    let subaccount = subaccount.trim();
    if subaccount.is_empty() {
        bail!("subaccount address is required for automatic Spot funding")
    }
    let client = DecibelClient::new(network, api_key)?;
    // An older build could have left a resting POST_ONLY funding bid. That order is standalone,
    // so it would block bulk replacement; clear any locally recorded one before funding.
    cancel_recorded_spot_funding_order(network, private_key, subaccount, market, &client).await?;
    let initial = client.account(Some(subaccount), market).await?;
    let mut funds = initial.spot_funds.ok_or_else(|| {
        anyhow!(
            "spot funds unavailable for {}: account_overviews did not include a usable spot balance",
            market.name
        )
    })?;
    let initial_base = funds.available_base_for_bulk();
    let base_gap_before = (plan.base_required - initial_base).max(Decimal::ZERO);
    if base_gap_before <= Decimal::ZERO {
        return Ok(SpotFundingResult {
            base_gap_before: Decimal::ZERO,
            bought_base: Decimal::ZERO,
            transaction_hash: None,
            borrowed_from_grid_quote: Decimal::ZERO,
        });
    }
    println!(
        "Spot base funding: plan needs {} {}, PFS holds {}, buying up to {} with IOC orders.",
        plan.base_required, funds.base_symbol, initial_base, base_gap_before
    );
    let mut last_hash = None;
    for attempt in 1..=MAX_TAKER_FUNDING_ATTEMPTS {
        let book = client.order_book(market, 1).await?;
        let best_ask =
            book.asks.first().map(|level| level.price).ok_or_else(|| {
                anyhow!("Spot order book for {} has no ask to buy from", market.name)
            })?;
        let funding = compute_spot_taker_funding(&funds, plan, best_ask, market)?;
        if funding.base_gap <= Decimal::ZERO {
            break;
        }
        if funding.quantity < market.min_size {
            // A residual smaller than one exchange order is not a funding failure: the previous
            // IOC(s) may already have filled almost the entire target. Return the partial result
            // so the caller refreshes balances and fits the ladder to the actual post-fill PFS
            // balance instead of falling back to a stale pre-funding snapshot.
            println!(
                "Spot IOC funding stopped with {} {} remaining; below minimum order size {} at {} {}.",
                funding.base_gap,
                funds.base_symbol,
                market.min_size,
                funding.limit_price,
                funds.quote_symbol
            );
            break;
        }
        let hash = submit_spot_ioc_bid(
            network,
            private_key,
            subaccount,
            market,
            funding.limit_price,
            funding.quantity,
        )
        .await?;
        println!(
            "  IOC {}/{}: buy up to {} {} at limit {} {}, tx {}",
            attempt,
            MAX_TAKER_FUNDING_ATTEMPTS,
            funding.quantity,
            funds.base_symbol,
            funding.limit_price,
            funds.quote_symbol,
            hash
        );
        last_hash = Some(hash);
        // An IOC leaves nothing resting, so the committed transaction is already reflected in the
        // balance. Re-read it to decide whether another sweep is needed.
        let current = client.account(Some(subaccount), market).await?;
        funds = current.spot_funds.ok_or_else(|| {
            anyhow!(
                "spot funds became unavailable while funding {}",
                market.name
            )
        })?;
        let remaining = (plan.base_required - funds.available_base_for_bulk()).max(Decimal::ZERO);
        println!(
            "  filled to {} {}; {} still needed",
            funds.available_base_for_bulk(),
            funds.base_symbol,
            remaining
        );
        if remaining <= Decimal::ZERO {
            break;
        }
    }
    let bought_base = (funds.available_base_for_bulk() - initial_base).max(Decimal::ZERO);
    let remaining = (plan.base_required - funds.available_base_for_bulk()).max(Decimal::ZERO);
    if remaining > Decimal::ZERO {
        println!(
            "Spot funding stopped {} {} short of the planned asks; the ask side will be shrunk to fit.",
            remaining, funds.base_symbol
        );
    }
    Ok(SpotFundingResult {
        base_gap_before,
        bought_base,
        transaction_hash: last_hash,
        borrowed_from_grid_quote: Decimal::ZERO,
    })
}

/// Compute a passive base-buy that cannot consume quote required by the grid's own bids.
pub fn compute_spot_funding_plan(
    funds: &SpotFunds,
    grid: &GridPlan,
    best_bid: Decimal,
    market_mid: Decimal,
    market: &Market,
) -> Result<SpotFundingPlan> {
    if best_bid <= Decimal::ZERO || market_mid <= Decimal::ZERO {
        bail!("best bid and market mid must be positive for Spot funding")
    }
    let available_base = funds.available_base();
    let available_quote = funds.available_quote();
    let base_gap = (grid.base_required - available_base).max(Decimal::ZERO);
    let required_quote_for_grid = grid.quote_required;
    let quote_gap = (required_quote_for_grid - available_quote).max(Decimal::ZERO);
    // The grid already accounts for its configured maker fee. Keep another 10 bps of the quote
    // surplus for the funding order's fee, so a full fill cannot encroach on grid bid collateral.
    let funding_fee_rate = Decimal::new(1, 3);
    let spare_quote = ((available_quote - required_quote_for_grid).max(Decimal::ZERO)
        / (Decimal::ONE + funding_fee_rate))
        .floor();
    if base_gap <= Decimal::ZERO || quote_gap > Decimal::ZERO {
        return Ok(SpotFundingPlan {
            base_gap,
            quote_gap,
            required_quote_for_grid,
            spare_quote,
            buy_price: None,
            buy_quantity: Decimal::ZERO,
            borrowed_from_grid_quote: Decimal::ZERO,
        });
    }
    let buy_price = round_down(
        best_bid.min(market_mid) * Decimal::from(9_995u32) / Decimal::from(10_000u32),
        market.tick_size,
    );
    if buy_price <= Decimal::ZERO {
        bail!(
            "funding price rounds to zero at market tick size {}",
            market.tick_size
        )
    }
    let funding_fee_rate = Decimal::new(1, 3);
    let raw_quantity = base_gap.min(spare_quote / buy_price);
    let mut buy_quantity = round_down(raw_quantity, market.lot_size);
    let mut borrowed_from_grid_quote = Decimal::ZERO;
    // A rounding-down shortfall of less than one lot should not force a manual top-up. Round the
    // required base amount up one lot, then permit it only when the additional inclusive cost is
    // at most 1% of the grid bid budget. The caller shrinks bid levels by that amount before bulk
    // submission, so the final transaction remains fully funded.
    if buy_quantity < base_gap {
        let required_rounded_up = round_up(base_gap, market.lot_size);
        let grid_surplus = (available_quote - required_quote_for_grid).max(Decimal::ZERO);
        let inclusive_cost = required_rounded_up * buy_price * (Decimal::ONE + funding_fee_rate);
        let borrowed = (inclusive_cost - grid_surplus).max(Decimal::ZERO);
        let borrow_limit = required_quote_for_grid * Decimal::new(1, 2);
        if required_rounded_up - base_gap <= market.lot_size && borrowed <= borrow_limit {
            buy_quantity = required_rounded_up;
            borrowed_from_grid_quote = borrowed;
        }
    }
    Ok(SpotFundingPlan {
        base_gap,
        quote_gap,
        required_quote_for_grid,
        spare_quote,
        buy_price: Some(buy_price),
        buy_quantity,
        borrowed_from_grid_quote,
    })
}

/// Submit the official eight-argument Spot ABI with the requested time-in-force.
/// `2` is IOC: it immediately takes available asks and leaves no resting order.
async fn submit_spot_ioc_bid(
    network: &str,
    private_key: &str,
    subaccount: &str,
    market: &Market,
    price: Decimal,
    quantity: Decimal,
) -> Result<String> {
    const IOC: u8 = 2;
    const MAX_GAS_OCTAS: u64 = 50_000_000;
    let key = normalize_private_key(private_key)?;
    let signer =
        Ed25519Account::from_private_key_hex(&key).context("invalid Aptos Ed25519 private key")?;
    let subaccount_addr: AccountAddress =
        subaccount.parse().context("invalid subaccount address")?;
    let market_addr: AccountAddress = market.address.parse().context("invalid market address")?;
    let network = network.trim().to_ascii_lowercase();
    let package = match network.as_str() {
        "mainnet" => MAINNET_PACKAGE,
        "testnet" => TESTNET_PACKAGE,
        other => bail!("unsupported execution network {other}; expected mainnet or testnet"),
    };
    let aptos = Aptos::new(match network.as_str() {
        "mainnet" => AptosConfig::mainnet(),
        _ => AptosConfig::testnet(),
    })?;
    let entry_function =
        format!("{package}::dex_accounts_spot_entry::place_spot_order_to_subaccount");
    let payload = InputEntryFunctionData::new(&entry_function)
        .arg(subaccount_addr)
        .arg(market_addr)
        .arg(scale_chain_amount(price, market.px_decimals)?)
        .arg(scale_chain_amount(quantity, market.sz_decimals)?)
        .arg(true)
        .arg(IOC)
        .arg_raw(move_none())
        .arg_raw(move_none())
        .build()
        .context("build Spot IOC funding transaction")?;
    let sequence_number = aptos.get_sequence_number(signer.address()).await?;
    let gas_price = aptos
        .fullnode()
        .estimate_gas_price()
        .await?
        .data
        .recommended();
    if gas_price == 0 {
        bail!("Aptos returned a zero gas unit price")
    }
    let max_gas_amount = MAX_GAS_OCTAS / gas_price;
    if max_gas_amount == 0 {
        bail!("gas price {gas_price} octas exceeds the 0.5 APT transaction cap")
    }
    let raw = TransactionBuilder::new()
        .sender(signer.address())
        .sequence_number(sequence_number)
        .payload(payload)
        .max_gas_amount(max_gas_amount)
        .gas_unit_price(gas_price)
        .chain_id(aptos.ensure_chain_id().await?)
        .expiration_from_now(600)
        .build()
        .context("build Spot IOC funding transaction with 0.5 APT gas cap")?;
    let signed = sign_transaction(&raw, &signer)
        .with_context(|| format!("sign Spot funding transaction ({entry_function})"))?;
    let response = aptos
        .submit_and_wait(&signed, Some(Duration::from_secs(60)))
        .await
        .with_context(|| format!("submit Spot funding transaction ({entry_function})"))?;
    if !response
        .data
        .get("success")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        bail!(
            "Spot funding transaction failed: {}",
            response
                .data
                .get("vm_status")
                .and_then(Value::as_str)
                .unwrap_or("unknown VM status")
        )
    }
    Ok(response
        .data
        .get("hash")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned())
}

/// Cancel a prior funding bid left by this bot for this exact network/subaccount/Spot market.
/// The store records its submitted price and quantity, then `/open_orders` supplies its on-chain
/// u128 order ID. A missing row means the order filled or was already cancelled, which is safe.
async fn cancel_recorded_spot_funding_order(
    network: &str,
    private_key: &str,
    subaccount: &str,
    market: &Market,
    client: &DecibelClient,
) -> Result<()> {
    let mut store = FundingOrderStore::load()?;
    let Some(record) = store
        .matching(network, subaccount, &market.address)
        .cloned()
    else {
        return Ok(());
    };
    let expected_price = Decimal::from_str(&record.price)
        .context("saved Spot funding order has an invalid price")?;
    let expected_quantity = Decimal::from_str(&record.quantity)
        .context("saved Spot funding order has an invalid quantity")?;
    let order_id = if let Some(id) = record.order_id {
        Some(id)
    } else {
        client
            .spot_open_orders(subaccount, market)
            .await?
            .iter()
            .find(|order| is_recorded_funding_order(order, expected_price, expected_quantity))
            .and_then(|order| value_str(order, "order_id").map(str::to_owned))
    };
    if let Some(order_id) = order_id {
        println!(
            "Cancelling prior automatic Spot funding order {} for {} before recalculating the grid.",
            order_id, market.name
        );
        cancel_spot_order(network, private_key, subaccount, market, &order_id).await?;
    }
    store.remove(network, subaccount, &market.address);
    store.save()?;
    Ok(())
}

fn is_recorded_funding_order(order: &Value, price: Decimal, quantity: Decimal) -> bool {
    let is_buy = order
        .get("is_buy")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || order
            .get("order_direction")
            .and_then(Value::as_str)
            .is_some_and(|side| side.eq_ignore_ascii_case("buy"));
    let post_only = order
        .get("time_in_force")
        .and_then(Value::as_str)
        .is_some_and(|tif| tif.eq_ignore_ascii_case("post_only"));
    let matches_price = decimal_field(order, "price").is_some_and(|actual| actual == price);
    // `orig_size` stays constant after a partial fill. Requiring it to match exactly prevents a
    // coincidental manual POST_ONLY order at the same price from being cancelled.
    let matches_size = decimal_field(order, "orig_size").is_some_and(|actual| actual == quantity);
    is_buy && post_only && matches_price && matches_size
}

/// Submit the three-argument Spot cancellation ABI for a recorded u128 order id.
async fn cancel_spot_order(
    network: &str,
    private_key: &str,
    subaccount: &str,
    market: &Market,
    order_id: &str,
) -> Result<()> {
    const MAX_GAS_OCTAS: u64 = 50_000_000;
    let order_id: u128 = order_id.parse().context("Spot order_id is not a u128")?;
    let key = normalize_private_key(private_key)?;
    let signer =
        Ed25519Account::from_private_key_hex(&key).context("invalid Aptos Ed25519 private key")?;
    let subaccount_addr: AccountAddress =
        subaccount.parse().context("invalid subaccount address")?;
    let market_addr: AccountAddress = market.address.parse().context("invalid market address")?;
    let network = network.trim().to_ascii_lowercase();
    let package = match network.as_str() {
        "mainnet" => MAINNET_PACKAGE,
        "testnet" => TESTNET_PACKAGE,
        other => bail!("unsupported execution network {other}; expected mainnet or testnet"),
    };
    let aptos = Aptos::new(match network.as_str() {
        "mainnet" => AptosConfig::mainnet(),
        _ => AptosConfig::testnet(),
    })?;
    let entry_function =
        format!("{package}::dex_accounts_spot_entry::cancel_spot_order_to_subaccount");
    let payload = InputEntryFunctionData::new(&entry_function)
        .arg(subaccount_addr)
        .arg(market_addr)
        .arg(order_id)
        .build()
        .context("build Spot funding-order cancellation transaction")?;
    let gas_price = aptos
        .fullnode()
        .estimate_gas_price()
        .await?
        .data
        .recommended();
    if gas_price == 0 {
        bail!("Aptos returned a zero gas unit price")
    }
    let max_gas_amount = MAX_GAS_OCTAS / gas_price;
    if max_gas_amount == 0 {
        bail!("gas price {gas_price} octas exceeds the 0.5 APT transaction cap")
    }
    let raw = TransactionBuilder::new()
        .sender(signer.address())
        .sequence_number(aptos.get_sequence_number(signer.address()).await?)
        .payload(payload)
        .max_gas_amount(max_gas_amount)
        .gas_unit_price(gas_price)
        .chain_id(aptos.ensure_chain_id().await?)
        .expiration_from_now(600)
        .build()
        .context("build Spot funding-order cancellation transaction with 0.5 APT gas cap")?;
    let signed = sign_transaction(&raw, &signer)
        .with_context(|| format!("sign Spot funding cancellation ({entry_function})"))?;
    let response = aptos
        .submit_and_wait(&signed, Some(Duration::from_secs(60)))
        .await
        .with_context(|| format!("submit Spot funding cancellation ({entry_function})"))?;
    if !response
        .data
        .get("success")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        let status = response
            .data
            .get("vm_status")
            .and_then(Value::as_str)
            .unwrap_or("unknown VM status");
        if !status.contains("ERESOURCE_DOES_NOT_EXIST") && !status.contains("EORDER_NOT_FOUND") {
            bail!("Spot funding-order cancellation failed: {status}")
        }
    }
    Ok(())
}

/// Move funds between a subaccount's Cross/collateral balance and PFS.
/// Positive `amount` is PFS -> Cross; negative `amount` is Cross -> PFS.
pub async fn transfer_spot_cross_pfs(
    network: &str,
    private_key: &str,
    subaccount: &str,
    metadata: &str,
    amount: i64,
) -> Result<String> {
    submit_spot_account_management_entry(
        network,
        private_key,
        "dex_accounts_entry::transfer_assets_between_non_collateral_and_collateral",
        subaccount,
        metadata,
        Some(amount),
    )
    .await
}

async fn submit_spot_account_management_entry(
    network: &str,
    private_key: &str,
    function_suffix: &str,
    subaccount: &str,
    metadata: &str,
    amount: Option<i64>,
) -> Result<String> {
    let key = normalize_private_key(private_key)?;
    let signer =
        Ed25519Account::from_private_key_hex(&key).context("invalid Aptos Ed25519 private key")?;
    let subaccount_addr: AccountAddress =
        subaccount.parse().context("invalid subaccount address")?;
    let metadata_addr: AccountAddress =
        metadata.parse().context("invalid asset metadata address")?;
    let package = package_for_network(network)?;
    let aptos = aptos_for_network(network)?;
    let entry_function = format!("{package}::{function_suffix}");
    let mut payload = InputEntryFunctionData::new(&entry_function)
        .arg(subaccount_addr)
        .arg(metadata_addr);
    if let Some(value) = amount {
        payload = payload.arg(value);
    }
    let payload = payload
        .build()
        .context("build Spot account-management transaction")?;
    let gas_price = aptos
        .fullnode()
        .estimate_gas_price()
        .await?
        .data
        .recommended();
    if gas_price == 0 {
        bail!("Aptos returned a zero gas unit price")
    }
    let max_gas_amount = 50_000_000u64 / gas_price;
    if max_gas_amount == 0 {
        bail!("gas price exceeds the 0.5 APT transaction cap")
    }
    let raw = TransactionBuilder::new()
        .sender(signer.address())
        .sequence_number(aptos.get_sequence_number(signer.address()).await?)
        .payload(payload)
        .max_gas_amount(max_gas_amount)
        .gas_unit_price(gas_price)
        .chain_id(aptos.ensure_chain_id().await?)
        .expiration_from_now(600)
        .build()
        .context("build Spot account-management transaction with 0.5 APT gas cap")?;
    let signed = sign_transaction(&raw, &signer)
        .with_context(|| format!("sign Spot account-management transaction ({entry_function})"))?;
    let response = aptos
        .submit_and_wait(&signed, Some(Duration::from_secs(60)))
        .await
        .with_context(|| {
            format!("submit Spot account-management transaction ({entry_function})")
        })?;
    if !response
        .data
        .get("success")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        bail!(
            "Spot account-management transaction failed: {}",
            response
                .data
                .get("vm_status")
                .and_then(Value::as_str)
                .unwrap_or("unknown VM status")
        )
    }
    Ok(response
        .data
        .get("hash")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned())
}

/// Validate every Move-side bulk invariant before signing anything.
fn prepare_bulk_order_parameters(
    sequence_number: u64,
    bids: &[&GridLevel],
    asks: &[&GridLevel],
    market: &Market,
) -> Result<BulkOrderParameters> {
    if sequence_number == 0 {
        bail!("bulk sequence number must be greater than zero")
    }
    if bids.len() > MAX_LEVELS_PER_SIDE || asks.len() > MAX_LEVELS_PER_SIDE {
        bail!("bulk order limit exceeded: at most {MAX_LEVELS_PER_SIDE} orders per side")
    }
    validate_bulk_side(bids, Side::Bid, market)?;
    validate_bulk_side(asks, Side::Ask, market)?;
    let bid_prices = scale_levels(bids, market.px_decimals, |level| level.price)?;
    let bid_sizes = scale_levels(bids, market.sz_decimals, |level| level.size)?;
    let ask_prices = scale_levels(asks, market.px_decimals, |level| level.price)?;
    let ask_sizes = scale_levels(asks, market.sz_decimals, |level| level.size)?;
    if let (Some(best_bid), Some(best_ask)) = (bid_prices.first(), ask_prices.first())
        && best_bid >= best_ask
    {
        bail!("bulk grid crosses: best bid {best_bid} must be below best ask {best_ask}")
    }
    Ok(BulkOrderParameters {
        sequence_number,
        bid_prices,
        bid_sizes,
        ask_prices,
        ask_sizes,
    })
}

fn validate_bulk_side(levels: &[&GridLevel], side: Side, market: &Market) -> Result<()> {
    for (index, level) in levels.iter().enumerate() {
        if level.side != side {
            bail!("bulk level {index} is not on the expected {:?} side", side)
        }
        if level.price <= Decimal::ZERO || level.size <= Decimal::ZERO {
            bail!(
                "bulk {:?} level {index} has non-positive price or size",
                side
            )
        }
        if round_down(level.price, market.tick_size) != level.price {
            bail!(
                "bulk {:?} level {index} price {} is not aligned to tick {}",
                side,
                level.price,
                market.tick_size
            )
        }
        if round_down(level.size, market.lot_size) != level.size || level.size < market.min_size {
            bail!(
                "bulk {:?} level {index} size {} is below lot/minimum requirements",
                side,
                level.size
            )
        }
        if index > 0 {
            let previous = levels[index - 1].price;
            let ordered = match side {
                Side::Bid => previous > level.price,
                Side::Ask => previous < level.price,
            };
            if !ordered {
                bail!(
                    "bulk {:?} prices are not strictly ordered at level {index}",
                    side
                )
            }
        }
    }
    Ok(())
}

fn normalize_private_key(private_key: &str) -> Result<String> {
    let key = private_key.trim();
    if key.is_empty() {
        bail!("Aptos private key is required")
    }
    let body = key
        .split_once("-priv-")
        .map(|(_, value)| value)
        .unwrap_or(key)
        .trim_start_matches("0x");
    let bytes = hex::decode(body).context("private key is not hexadecimal")?;
    if bytes.len() != 32 {
        bail!("Aptos Ed25519 private key must be exactly 32 bytes")
    }
    Ok(format!("0x{}", hex::encode(bytes)))
}

fn scale_levels<F>(levels: &[&GridLevel], decimals: u32, value: F) -> Result<Vec<u64>>
where
    F: Fn(&GridLevel) -> Decimal,
{
    levels
        .iter()
        .map(|level| scale_chain_amount(value(level), decimals))
        .collect()
}

fn scale_chain_amount(value: Decimal, decimals: u32) -> Result<u64> {
    if value <= Decimal::ZERO {
        bail!("chain amount must be positive, got {value}")
    }
    let factor = Decimal::from(
        10u64
            .checked_pow(decimals)
            .ok_or_else(|| anyhow!("decimal scale overflow"))?,
    );
    let raw = (value * factor).floor();
    if raw <= Decimal::ZERO {
        bail!("amount {value} rounds to zero in chain units")
    }
    raw.to_u64()
        .ok_or_else(|| anyhow!("amount {value} cannot be represented as u64 chain units"))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LevelState {
    Planned,
    Resting,
    Filled,
    Selected,
}

#[derive(Clone, Debug)]
pub struct GridLevel {
    pub side: Side,
    pub price: Decimal,
    pub size: Decimal,
    pub notional: Decimal,
    pub state: LevelState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Side {
    Bid,
    Ask,
}

impl Side {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bid => "BID",
            Self::Ask => "ASK",
        }
    }
}

#[derive(Clone, Debug)]
pub struct GridPlan {
    pub mid: Decimal,
    pub lower: Decimal,
    pub upper: Decimal,
    pub bids: Vec<GridLevel>,
    pub asks: Vec<GridLevel>,
    pub quote_required: Decimal,
    pub base_required: Decimal,
    pub estimated_margin: Option<Decimal>,
}

#[derive(Clone, Debug, Default)]
pub struct ProfitPreview {
    pub matched_pairs: usize,
    pub gross_capture: Decimal,
    pub maker_fees: Decimal,
    pub net_capture: Decimal,
    pub min_pair_net: Option<Decimal>,
    pub max_pair_net: Option<Decimal>,
}

/// Shrink a Spot grid to fit available PFS balances by retaining only the closest-to-mid levels
/// that can be fully funded at the market's minimum order size. Unlike an old proportional-shrink
/// approach this never produces sub-min-size levels that are later dropped entirely.
///
/// This operates on the supplied plan as-is. Live callers pass the executable plan, while the
/// monitor may retain historical fill markers for display.
pub fn shrink_spot_to_available(
    plan: &mut GridPlan,
    quote_budget: Decimal,
    base_budget: Decimal,
    market: &Market,
) -> Result<String> {
    let old_quote = plan.quote_required;
    let old_base = plan.base_required;
    let old_bid_notional: Decimal = plan.bids.iter().map(|level| level.notional).sum();
    let fee_multiplier = if old_bid_notional > Decimal::ZERO {
        old_quote / old_bid_notional
    } else {
        Decimal::ONE
    };
    // Calculate a minimum feasible level notional (incl. maker fee markup) and the maximum number
    // of affordable levels, then keep that many closest-to-mid levels at min_size each.
    let min_cost_per_level = market.min_size * plan.mid * fee_multiplier;
    let max_quote_levels = if min_cost_per_level > Decimal::ZERO {
        (quote_budget / min_cost_per_level)
            .floor()
            .to_usize()
            .unwrap_or(0)
    } else {
        plan.bids.len()
    };
    let max_base_units = if market.min_size > Decimal::ZERO {
        (base_budget / market.min_size)
            .floor()
            .to_usize()
            .unwrap_or(0)
    } else {
        plan.asks.len()
    };
    // Sort bids ascending (cheapest first, i.e. farthest from mid) then keep the highest-price
    // levels; asks are naturally ascending (cheapest first). Keep levels closest to mid.
    plan.bids.sort_by_key(|level| level.price);
    let keep_bids = max_quote_levels.min(plan.bids.len());
    if keep_bids < plan.bids.len() {
        plan.bids = plan
            .bids
            .split_off(plan.bids.len().saturating_sub(keep_bids));
    }
    plan.asks.sort_by_key(|level| level.price);
    let keep_asks = max_base_units.min(plan.asks.len());
    if keep_asks < plan.asks.len() {
        plan.asks.truncate(keep_asks);
    }
    // Set every retained level to the largest feasible size that stays within budget.
    // For bids: equal size = min(quote_budget / kept_count / price / fee_multiplier, original).
    // For asks: equal size = min(base_budget / kept_count, original).
    for level in &mut plan.bids {
        let max_affordable = if keep_bids > 0 {
            (quote_budget / Decimal::from(keep_bids) / level.price / fee_multiplier).floor()
        } else {
            Decimal::ZERO
        };
        let original = level.size;
        level.size = round_down(max_affordable.min(original), market.lot_size);
        level.notional = level.price * level.size;
    }
    for level in &mut plan.asks {
        let max_affordable = if keep_asks > 0 {
            (base_budget / Decimal::from(keep_asks)).floor()
        } else {
            Decimal::ZERO
        };
        let original = level.size;
        level.size = round_down(max_affordable.min(original), market.lot_size);
        level.notional = level.price * level.size;
    }
    plan.bids.retain(|level| level.size >= market.min_size);
    plan.asks.retain(|level| level.size >= market.min_size);
    // `prices()` and the Move bulk ABI require bids highest-to-lowest (nearest mid first),
    // while the affordability pass above sorts bids low-to-high to keep the closest levels.
    // Restore the ABI order after shrinking; otherwise level 1 fails on-chain with
    // "bulk Bid prices are not strictly ordered".
    plan.bids
        .sort_by_key(|level| std::cmp::Reverse(level.price));
    let bid_notional: Decimal = plan.bids.iter().map(|l| l.notional).sum();
    let ask_quantity: Decimal = plan.asks.iter().map(|l| l.size).sum();
    plan.quote_required = bid_notional * fee_multiplier;
    plan.base_required = ask_quantity;
    Ok(format!(
        "grid shrunk: quote {} → {} (available {})  base {} → {} (available {})",
        old_quote, plan.quote_required, quote_budget, old_base, plan.base_required, base_budget
    ))
}

impl GridPlan {
    pub fn all_levels(&self) -> impl Iterator<Item = &GridLevel> {
        self.bids.iter().chain(self.asks.iter())
    }

    pub fn apply_trade_history(&mut self, trades: &[Trade], tick: Decimal) {
        for level in self.bids.iter_mut().chain(self.asks.iter_mut()) {
            if trades
                .iter()
                .any(|trade| close_to_tick(trade.price, level.price, tick))
            {
                level.state = LevelState::Filled;
            }
        }
    }

    pub fn select(&mut self, selected: usize) {
        for (index, level) in self.bids.iter_mut().chain(self.asks.iter_mut()).enumerate() {
            if level.state != LevelState::Filled {
                level.state = if index == selected {
                    LevelState::Selected
                } else {
                    LevelState::Planned
                };
            }
        }
    }

    /// Scenario-only maker-to-maker capture. Every pair assumes its bid fills, then the paired
    /// ask fills later. It excludes funding, gas, partial fills, drift, liquidation and slippage.
    pub fn profit_preview(&self, maker_fee_rate: Decimal) -> ProfitPreview {
        let pairs = self.bids.len().min(self.asks.len());
        let mut result = ProfitPreview {
            matched_pairs: pairs,
            ..ProfitPreview::default()
        };
        let mut nets = Vec::with_capacity(pairs);
        for index in 0..pairs {
            let bid = &self.bids[index];
            let ask = &self.asks[index];
            let size = bid.size.min(ask.size);
            let gross = (ask.price - bid.price) * size;
            let fees = (bid.price * size + ask.price * size) * maker_fee_rate;
            let net = gross - fees;
            result.gross_capture += gross;
            result.maker_fees += fees;
            nets.push(net);
        }
        result.net_capture = result.gross_capture - result.maker_fees;
        result.min_pair_net = nets.iter().copied().min();
        result.max_pair_net = nets.iter().copied().max();
        result
    }
}

pub fn build_plan(config: &GridConfig, market: &Market, mid: Decimal) -> Result<GridPlan> {
    config.validate()?;
    if mid <= Decimal::ZERO {
        bail!("market mid price must be positive")
    }
    let (bid_count, ask_count) = side_counts(config);
    let (lower, upper) = resolve_range(config, mid, bid_count.max(ask_count))?;
    if !(lower < mid && mid < upper) {
        bail!("mid price {mid} is outside grid range [{lower}, {upper}]")
    }

    let bids = prices(
        config,
        Side::Bid,
        mid,
        lower,
        upper,
        bid_count,
        market.tick_size,
    )?;
    let asks = prices(
        config,
        Side::Ask,
        mid,
        lower,
        upper,
        ask_count,
        market.tick_size,
    )?;
    let (bid_size, ask_size) = derive_sizes(config, mid, &bids, &asks, market)?;

    let bid_levels = bids
        .into_iter()
        .map(|price| GridLevel {
            side: Side::Bid,
            price,
            size: bid_size,
            notional: price * bid_size,
            state: LevelState::Planned,
        })
        .collect::<Vec<_>>();
    let ask_levels = asks
        .into_iter()
        .map(|price| GridLevel {
            side: Side::Ask,
            price,
            size: ask_size,
            notional: price * ask_size,
            state: LevelState::Planned,
        })
        .collect::<Vec<_>>();

    let quote_required = bid_levels
        .iter()
        .map(|l| l.notional * (Decimal::ONE + config.maker_fee_rate))
        .sum();
    let base_required = ask_levels.iter().map(|l| l.size).sum();
    let long_notional: Decimal = bid_levels.iter().map(|l| l.notional).sum();
    let short_notional: Decimal = ask_levels.iter().map(|l| l.notional).sum();
    let estimated_margin = match config.product {
        Product::Spot => None,
        Product::Perp => Some(
            long_notional.max(short_notional) / config.preview_leverage
                + (long_notional + short_notional) * config.maker_fee_rate,
        ),
    };
    Ok(GridPlan {
        mid,
        lower,
        upper,
        bids: bid_levels,
        asks: ask_levels,
        quote_required,
        base_required,
        estimated_margin,
    })
}

fn side_counts(config: &GridConfig) -> (usize, usize) {
    match (config.product, config.perp_mode) {
        (Product::Perp, PerpMode::Long) => (config.total_count, 1),
        (Product::Perp, PerpMode::Short) => (1, config.total_count),
        _ => (
            config.total_count / 2,
            config.total_count - config.total_count / 2,
        ),
    }
}

fn resolve_range(config: &GridConfig, mid: Decimal, levels: usize) -> Result<(Decimal, Decimal)> {
    let hundred = Decimal::from(100);
    match config.range {
        RangeSpec::Bounds { lower, upper } => Ok((lower, upper)),
        RangeSpec::Percent { percent } => {
            let fraction = percent / hundred;
            Ok((
                mid * (Decimal::ONE - fraction),
                mid * (Decimal::ONE + fraction),
            ))
        }
        RangeSpec::StepPercent { percent } => {
            let fraction = percent / hundred;
            Ok((
                mid * pow_decimal(Decimal::ONE - fraction, levels),
                mid * pow_decimal(Decimal::ONE + fraction, levels),
            ))
        }
    }
}

fn prices(
    config: &GridConfig,
    side: Side,
    mid: Decimal,
    lower: Decimal,
    upper: Decimal,
    count: usize,
    tick: Decimal,
) -> Result<Vec<Decimal>> {
    let mut values = Vec::with_capacity(count);
    for i in 1..=count {
        let raw = match (&config.range, side) {
            (RangeSpec::StepPercent { percent }, Side::Bid) => {
                mid * pow_decimal(Decimal::ONE - *percent / Decimal::from(100), i)
            }
            (RangeSpec::StepPercent { percent }, Side::Ask) => {
                mid * pow_decimal(Decimal::ONE + *percent / Decimal::from(100), i)
            }
            (_, Side::Bid) => mid - (mid - lower) * Decimal::from(i) / Decimal::from(count),
            (_, Side::Ask) => mid + (upper - mid) * Decimal::from(i) / Decimal::from(count),
        };
        values.push(round_down(raw, tick));
    }
    values.sort();
    values.dedup();
    if side == Side::Bid {
        values.reverse();
    }
    if values.is_empty() {
        bail!("grid range is too narrow for market tick size")
    }
    Ok(values)
}

fn derive_sizes(
    config: &GridConfig,
    _mid: Decimal,
    bids: &[Decimal],
    asks: &[Decimal],
    market: &Market,
) -> Result<(Decimal, Decimal)> {
    let (mut bid, mut ask) = match config.allocation {
        Allocation::FixedSize(size) => (size, size),
        Allocation::TotalBudget(budget) => match config.product {
            Product::Spot => {
                let half = budget / Decimal::TWO;
                let bid_denominator: Decimal =
                    bids.iter().sum::<Decimal>() * (Decimal::ONE + config.maker_fee_rate);
                let ask_denominator: Decimal = asks.iter().sum();
                (
                    if bids.is_empty() {
                        Decimal::ZERO
                    } else {
                        half / bid_denominator
                    },
                    if asks.is_empty() {
                        Decimal::ZERO
                    } else {
                        half / ask_denominator
                    },
                )
            }
            Product::Perp => {
                let bid_notional: Decimal = bids.iter().sum();
                let ask_notional: Decimal = asks.iter().sum();
                let per_base = bid_notional.max(ask_notional) / config.preview_leverage
                    + (bid_notional + ask_notional) * config.maker_fee_rate;
                if per_base <= Decimal::ZERO {
                    bail!("cannot derive size from an empty grid")
                }
                let size = budget / per_base;
                (size, size)
            }
        },
    };
    if config.product == Product::Perp && config.perp_mode == PerpMode::Long {
        ask = Decimal::ZERO;
    }
    if config.product == Product::Perp && config.perp_mode == PerpMode::Short {
        bid = Decimal::ZERO;
    }
    bid = if bid > Decimal::ZERO {
        round_down(bid, market.lot_size)
    } else {
        bid
    };
    ask = if ask > Decimal::ZERO {
        round_down(ask, market.lot_size)
    } else {
        ask
    };
    if bid > Decimal::ZERO && bid < market.min_size {
        bail!(
            "derived bid size {bid} is below min size {}",
            market.min_size
        )
    }
    if ask > Decimal::ZERO && ask < market.min_size {
        bail!(
            "derived ask size {ask} is below min size {}",
            market.min_size
        )
    }
    Ok((bid, ask))
}

pub fn round_down(value: Decimal, increment: Decimal) -> Decimal {
    (value / increment).round_dp_with_strategy(0, RoundingStrategy::ToZero) * increment
}

pub fn round_up(value: Decimal, increment: Decimal) -> Decimal {
    (value / increment).ceil() * increment
}

fn pow_decimal(value: Decimal, exponent: usize) -> Decimal {
    (0..exponent).fold(Decimal::ONE, |acc, _| acc * value)
}

fn close_to_tick(left: Decimal, right: Decimal, tick: Decimal) -> bool {
    (left - right).abs() <= tick / Decimal::TWO
}

#[derive(Clone)]
pub struct DecibelClient {
    http: HttpClient,
    base_url: String,
    ws_url: String,
    api_key: String,
}

impl DecibelClient {
    pub fn new(network: &str, api_key: &str) -> Result<Self> {
        validate_api_key_format(api_key)?;
        let (api_root, ws_url) = match network {
            "mainnet" => (
                "https://api.mainnet.aptoslabs.com/decibel/api/v1",
                "wss://api.mainnet.aptoslabs.com/decibel/ws",
            ),
            "testnet" => (
                "https://api.testnet.aptoslabs.com/decibel/api/v1",
                "wss://api.testnet.aptoslabs.com/decibel/ws",
            ),
            other => bail!("unsupported network {other}; expected mainnet or testnet"),
        };
        let mut headers = header::HeaderMap::new();
        let bearer = format!("Bearer {api_key}")
            .parse()
            .context("invalid DECIBEL_API_KEY header")?;
        headers.insert(header::AUTHORIZATION, bearer);
        let http = HttpClient::builder()
            .default_headers(headers)
            .timeout(Duration::from_secs(15))
            .build()?;
        Ok(Self {
            http,
            base_url: api_root.to_owned(),
            ws_url: ws_url.to_owned(),
            api_key: api_key.to_owned(),
        })
    }

    async fn get(&self, path: &str, params: &[(&str, String)]) -> Result<Value> {
        let url = format!("{}/{}", self.base_url, path);
        let response = self.http.get(url).query(params).send().await?;
        let status = response.status();
        let body = response.text().await?;
        if !status.is_success() {
            bail!("Decibel {path} returned {status}: {body}")
        }
        serde_json::from_str(&body).with_context(|| format!("invalid JSON from Decibel {path}"))
    }

    /// Bulk sequence is a venue-side monotonically increasing value. The API's bulk-orders reader
    /// accepts the active account and market filters; the latest row's `sequence_number` is the
    /// predecessor for the next transaction.
    async fn next_bulk_sequence(
        &self,
        subaccount: &str,
        market: &str,
        product: Product,
    ) -> Result<u64> {
        let asset_type = match product {
            Product::Perp => "perp",
            Product::Spot => "spot",
        };
        let data = self
            .get(
                "bulk_orders",
                &[
                    ("account", subaccount.to_owned()),
                    ("market", market.to_owned()),
                    ("asset_type", asset_type.to_owned()),
                ],
            )
            .await?;
        let rows = data
            .as_array()
            .ok_or_else(|| anyhow!("/bulk_orders did not return an array"))?;
        rows.iter()
            .filter_map(|row| {
                row.get("sequence_number")
                    .and_then(Value::as_u64)
                    .or_else(|| {
                        row.get("sequence_number")
                            .and_then(Value::as_i64)
                            .map(|value| value as u64)
                    })
            })
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| anyhow!("bulk sequence number overflow"))
    }

    /// Check that the bearer key is accepted by both documented API transports without exposing
    /// the key or response body: REST (`/markets`) and WebSocket (`all_market_prices`).
    pub async fn verify_api_key(&self) -> Result<()> {
        let url = format!("{}/markets", self.base_url);
        let response = self.http.get(url).send().await?;
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            bail!("API key is invalid or not authorized for REST (HTTP {status})")
        }
        if !status.is_success() {
            bail!("REST API key check could not be completed (HTTP {status})")
        }
        self.ws_snapshot("all_market_prices")
            .await
            .context("API key is not accepted by the Decibel WebSocket gateway")?;
        Ok(())
    }

    pub async fn markets(&self, product: Product) -> Result<Vec<Market>> {
        let data = self.get("markets", &[]).await?;
        let rows = data
            .as_array()
            .ok_or_else(|| anyhow!("/markets did not return an array"))?;
        rows.iter()
            .map(|row| parse_market(row, product))
            .filter_map(Result::transpose)
            .collect()
    }

    pub async fn market(&self, market_name: &str, product: Product) -> Result<Market> {
        let data = self.get("markets", &[]).await?;
        let rows = data
            .as_array()
            .ok_or_else(|| anyhow!("/markets did not return an array"))?;
        for row in rows {
            if let Some(market) = parse_market(row, product)?
                && market.name.eq_ignore_ascii_case(market_name)
            {
                return Ok(market);
            }
        }
        Err(anyhow!(
            "{} {:?} market was not found",
            market_name,
            product
        ))
    }

    pub async fn mid_price(&self, market: &Market, source: PriceSource) -> Result<Decimal> {
        // `all_market_prices` is a Perp feed. Spot markets use their order book regardless
        // of the generic price-source setting, otherwise a Spot refresh can first query a
        // feed that can never contain the selected market and produce a misleading fallback
        // error before reading the Spot book.
        if market.product == Product::Spot {
            return self.mid_from_depth(market).await;
        }
        match source {
            PriceSource::Prices => self.mid_from_prices_or_depth(market).await,
            PriceSource::Depth => self.mid_from_depth(market).await,
        }
    }

    async fn ws_snapshot(&self, topic: &str) -> Result<Value> {
        let mut request = self.ws_url.clone().into_client_request()?;
        request.headers_mut().insert(
            "Sec-WebSocket-Protocol",
            HeaderValue::from_str(&format!("decibel, {}", self.api_key))?,
        );
        let (mut socket, _) = connect_async(request).await?;
        socket
            .send(Message::Text(
                serde_json::json!({"method": "subscribe", "topic": topic})
                    .to_string()
                    .into(),
            ))
            .await?;
        let result = tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(message) = socket.next().await {
                let message = message?;
                let Message::Text(text) = message else {
                    continue;
                };
                let value: Value = serde_json::from_str(&text)?;
                if value.get("topic").and_then(Value::as_str) == Some(topic)
                    && value.get("success").is_none()
                {
                    return Ok::<Value, anyhow::Error>(value);
                }
                if value.get("success") == Some(&Value::Bool(false)) {
                    bail!("WebSocket subscription failed for {topic}: {}", value);
                }
            }
            bail!("WebSocket closed before receiving {topic}")
        })
        .await
        .context("timed out waiting for Decibel WebSocket market data")??;
        Ok(result)
    }

    /// Perp prices come from the documented `all_market_prices` WebSocket topic.
    async fn mid_from_prices(&self, market: &Market) -> Result<Decimal> {
        let data = self.ws_snapshot("all_market_prices").await?;
        let rows = data
            .get("prices")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("all_market_prices did not return a prices array"))?;
        let row = rows
            .iter()
            .find(|row| price_row_matches_market(row, market))
            .ok_or_else(|| {
                anyhow!(
                    "all_market_prices has no Perp price row for {}",
                    market.name
                )
            })?;
        decimal_field(row, "mid_px")
            .or_else(|| decimal_field(row, "mark_px"))
            .filter(|price| *price > Decimal::ZERO)
            .ok_or_else(|| {
                anyhow!(
                    "all_market_prices has no positive price for {}",
                    market.name
                )
            })
    }

    async fn mid_from_prices_or_depth(&self, market: &Market) -> Result<Decimal> {
        match self.mid_from_prices(market).await {
            Ok(price) => Ok(price),
            Err(price_error) => self.mid_from_depth(market).await.map_err(|depth_error| {
                anyhow!(
                    "market {} is registered but has no usable price or order book: all_market_prices: {}; depth: {}",
                    market.name,
                    price_error,
                    depth_error
                )
            }),
        }
    }

    pub async fn order_book(&self, market: &Market, _limit: usize) -> Result<OrderBook> {
        let data = self
            .ws_snapshot(&format!("depth:{}:1", market.address))
            .await?;
        let parse_levels = |side: &str| -> Result<Vec<BookLevel>> {
            data.get(side)
                .and_then(Value::as_array)
                .ok_or_else(|| anyhow!("depth has no {side}"))?
                .iter()
                .map(|row| {
                    Ok(BookLevel {
                        price: decimal_field(row, "price")
                            .ok_or_else(|| anyhow!("depth {side} has no price"))?,
                        size: decimal_field(row, "size")
                            .ok_or_else(|| anyhow!("depth {side} has no size"))?,
                    })
                })
                .collect()
        };
        Ok(OrderBook {
            bids: parse_levels("bids")?,
            asks: parse_levels("asks")?,
        })
    }

    async fn mid_from_depth(&self, market: &Market) -> Result<Decimal> {
        // A newly subscribed book can occasionally arrive empty while the market is idle.
        // Retry briefly so a transient WS snapshot is not reported as a permanent refresh
        // failure. Do not invent a price from a one-sided book: both sides are required for
        // safe grid placement.
        let mut last_error = None;
        for attempt in 0..3 {
            match self.try_mid_from_depth(market).await {
                Ok(mid) => return Ok(mid),
                Err(error) => {
                    last_error = Some(error);
                    if attempt < 2 {
                        tokio::time::sleep(Duration::from_millis(150)).await;
                    }
                }
            }
        }
        Err(last_error.expect("depth retry loop always records an error"))
    }

    async fn try_mid_from_depth(&self, market: &Market) -> Result<Decimal> {
        let book = self.order_book(market, 1).await?;
        let bid = book
            .bids
            .first()
            .map(|level| level.price)
            .ok_or_else(|| anyhow!("/depth has no bid price"))?;
        let ask = book
            .asks
            .first()
            .map(|level| level.price)
            .ok_or_else(|| anyhow!("/depth has no ask price"))?;
        Ok((bid + ask) / Decimal::TWO)
    }

    /// Return currently open Spot orders for one market. This is used only to resolve the order ID
    /// of a locally recorded automatic funding order; callers must not treat it as bot ownership.
    pub async fn spot_open_orders(&self, subaccount: &str, market: &Market) -> Result<Vec<Value>> {
        let data = self
            .get(
                "open_orders",
                &[
                    ("account", subaccount.to_owned()),
                    ("asset_type", "spot".to_owned()),
                    ("limit", "100".to_owned()),
                ],
            )
            .await?;
        let rows = data
            .get("items")
            .and_then(Value::as_array)
            .or_else(|| data.as_array())
            .ok_or_else(|| anyhow!("/open_orders returned no items array"))?;
        Ok(rows
            .iter()
            .filter(|order| {
                normalized_address(value_str(order, "market").unwrap_or_default())
                    == normalized_address(&market.address)
            })
            .cloned()
            .collect())
    }

    /// Return parsed open orders for one market. This is read-only and intentionally does not
    /// infer bot ownership: without a Decibel client-order-id, an unmatched order may be manual
    /// or from a previous process and must remain unmanaged.
    pub async fn open_orders(
        &self,
        subaccount: &str,
        market: &Market,
    ) -> Result<Vec<reconcile::ActualOrder>> {
        // A partial snapshot cannot safely drive a bulk replacement. Ask for a deliberately high
        // bound and refuse a full response, because it may be truncated by the API.
        const OPEN_ORDER_LIMIT: usize = 1_000;
        let asset_type = match market.product {
            Product::Spot => "spot",
            Product::Perp => "perp",
        };
        let data = self
            .get(
                "open_orders",
                &[
                    ("account", subaccount.to_owned()),
                    ("asset_type", asset_type.to_owned()),
                    ("limit", OPEN_ORDER_LIMIT.to_string()),
                ],
            )
            .await?;
        let rows = data
            .get("items")
            .and_then(Value::as_array)
            .or_else(|| data.as_array())
            .ok_or_else(|| anyhow!("/open_orders returned no items array"))?;
        if rows.len() >= OPEN_ORDER_LIMIT {
            bail!(
                "/open_orders returned {} rows at limit {OPEN_ORDER_LIMIT}; refusing to reconcile a potentially truncated snapshot",
                rows.len()
            )
        }
        let mut orders: Vec<reconcile::ActualOrder> = rows
            .iter()
            .filter(|order| {
                normalized_address(value_str(order, "market").unwrap_or_default())
                    == normalized_address(&market.address)
            })
            .map(parse_open_order)
            .collect::<Result<_>>()?;

        // The REST open_orders endpoint does not expose orders created by the bulk ABI. The
        // bulk-orders endpoint does expose the currently active ladder, so include its levels as
        // synthetic ActualOrder values. This is deliberately conservative: merely seeing a bulk
        // ladder makes the caller treat the market as occupied and refuse an automatic replacement
        // unless ownership/replacement has been explicitly reviewed.
        let bulk_data = self
            .get(
                "bulk_orders",
                &[
                    ("account", subaccount.to_owned()),
                    ("market", market.address.clone()),
                    ("asset_type", asset_type.to_owned()),
                ],
            )
            .await?;
        let bulk_rows = bulk_data
            .as_array()
            .ok_or_else(|| anyhow!("/bulk_orders did not return an array"))?;
        if let Some(latest) = bulk_rows
            .iter()
            .max_by_key(|row| integer_field(row, "sequence_number").unwrap_or_default())
        {
            let sequence = integer_field(latest, "sequence_number").unwrap_or_default();
            append_bulk_levels(
                &mut orders,
                latest,
                "bid_prices",
                "bid_sizes",
                Side::Bid,
                sequence,
            )?;
            append_bulk_levels(
                &mut orders,
                latest,
                "ask_prices",
                "ask_sizes",
                Side::Ask,
                sequence,
            )?;
        }
        Ok(orders)
    }

    /// The amount of actual USDC collateral that Decibel permits moving from Cross/CBS into PFS.
    pub async fn cross_withdrawable_usdc(&self, subaccount: &str) -> Result<Decimal> {
        let overview = self
            .get("account_overviews", &[("account", subaccount.to_owned())])
            .await?;
        let overview = overview
            .as_array()
            .and_then(|rows| rows.first())
            .unwrap_or(&overview);
        Ok(decimal_field(overview, "usdc_cross_withdrawable_balance").unwrap_or(Decimal::ZERO))
    }

    pub async fn account(
        &self,
        subaccount: Option<&str>,
        market: &Market,
    ) -> Result<AccountOverview> {
        let Some(account) = subaccount else {
            return Ok(AccountOverview {
                available_margin: None,
                equity: None,
                position: Position {
                    size: Decimal::ZERO,
                    entry_price: Decimal::ZERO,
                },
                open_order_count: 0,
                spot_funds: None,
            });
        };
        let overview = self
            .get("account_overviews", &[("account", account.to_owned())])
            .await?;
        let overview = overview
            .as_array()
            .and_then(|rows| rows.first())
            .unwrap_or(&overview);
        let positions = self
            .get("account_positions", &[("account", account.to_owned())])
            .await?;
        let position = positions
            .as_array()
            .and_then(|rows| {
                rows.iter().find(|row| {
                    normalized_address(value_str(row, "market").unwrap_or_default())
                        == normalized_address(&market.address)
                })
            })
            .map(|row| Position {
                size: decimal_field(row, "size").unwrap_or(Decimal::ZERO),
                entry_price: decimal_field(row, "entry_price").unwrap_or(Decimal::ZERO),
            })
            .unwrap_or(Position {
                size: Decimal::ZERO,
                entry_price: Decimal::ZERO,
            });
        let open = self
            .get("open_orders", &[("account", account.to_owned())])
            .await?;
        let spot_funds = if market.product == Product::Spot {
            parse_spot_funds(overview, market)
        } else {
            None
        };
        let open_order_count = open
            .get("items")
            .and_then(Value::as_array)
            .map(|rows| {
                rows.iter()
                    .filter(|row| {
                        normalized_address(value_str(row, "market").unwrap_or_default())
                            == normalized_address(&market.address)
                    })
                    .count()
            })
            .unwrap_or(0);
        Ok(AccountOverview {
            available_margin: decimal_field(overview, "cross_available_to_trade")
                .or_else(|| decimal_field(overview, "perp_equity_balance")),
            equity: decimal_field(overview, "perp_equity_balance"),
            position,
            open_order_count,
            spot_funds,
        })
    }

    pub async fn trade_history(
        &self,
        subaccount: Option<&str>,
        market: &Market,
    ) -> Result<Vec<Trade>> {
        let Some(account) = subaccount else {
            return Ok(Vec::new());
        };
        let data = self
            .get(
                "trade_history",
                &[
                    ("account", account.to_owned()),
                    ("market", market.address.clone()),
                    ("limit", "100".to_owned()),
                ],
            )
            .await?;
        let rows = data
            .get("items")
            .and_then(Value::as_array)
            .or_else(|| data.as_array())
            .ok_or_else(|| anyhow!("/trade_history returned no items array"))?;
        Ok(rows
            .iter()
            .filter_map(|row| {
                Some(Trade {
                    price: decimal_field(row, "price")?,
                    size: decimal_field(row, "size")?,
                    timestamp_ms: integer_field(row, "transaction_unix_ms").unwrap_or_default(),
                })
            })
            .collect())
    }
}

#[derive(Clone, Debug)]
pub struct MonitorSnapshot {
    pub observed_at: DateTime<Utc>,
    pub market: Market,
    /// Display plan. It may contain historical-fill markers for the TUI only.
    pub plan: GridPlan,
    pub account: AccountOverview,
    /// Desired-vs-actual order drift calculated from a clean executable plan. `None` when no
    /// subaccount was supplied, because open orders cannot then be read safely.
    pub reconciliation: Option<reconcile::Reconciliation>,
    /// Most recent account trades for the active market, newest first when supplied by the API.
    pub trades: Vec<Trade>,
    pub status: String,
}

pub async fn fetch_snapshot(
    client: &DecibelClient,
    config: &GridConfig,
    subaccount: Option<&str>,
) -> Result<MonitorSnapshot> {
    let market = client.market(&config.market_name, config.product).await?;
    let mid = client.mid_price(&market, config.price_source).await?;
    let mut plan = build_plan(config, &market, mid)?;
    let (account, trades) = tokio::try_join!(
        client.account(subaccount, &market),
        client.trade_history(subaccount, &market)
    )?;
    plan.apply_trade_history(&trades, market.tick_size);
    Ok(MonitorSnapshot {
        observed_at: Utc::now(),
        market,
        plan,
        account,
        reconciliation: None,
        trades,
        status: "LIVE DATA — EXECUTION PLAN MONITOR".to_owned(),
    })
}

/// Convert active levels from a bulk-orders row into synthetic open orders for reconciliation.
/// Bulk orders do not have individual order IDs in the REST open_orders response, so the synthetic
/// ID is only an observation key; it must never be used to cancel a single order.
fn append_bulk_levels(
    orders: &mut Vec<reconcile::ActualOrder>,
    row: &Value,
    prices_key: &str,
    sizes_key: &str,
    side: Side,
    sequence: i64,
) -> Result<()> {
    let prices = row
        .get(prices_key)
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("/bulk_orders row has no {prices_key} array"))?;
    let sizes = row
        .get(sizes_key)
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("/bulk_orders row has no {sizes_key} array"))?;
    if prices.len() != sizes.len() {
        bail!(
            "/bulk_orders {prices_key}/{sizes_key} length mismatch: {} prices, {} sizes",
            prices.len(),
            sizes.len()
        )
    }
    for (index, (price, size)) in prices.iter().zip(sizes).enumerate() {
        orders.push(reconcile::ActualOrder {
            order_id: format!("bulk:{sequence}:{side:?}:{index}"),
            side,
            price: decimal_value(price)
                .ok_or_else(|| anyhow!("/bulk_orders {prices_key}[{index}] has no price"))?,
            remaining_size: decimal_value(size)
                .ok_or_else(|| anyhow!("/bulk_orders {sizes_key}[{index}] has no size"))?,
            origin: reconcile::OrderOrigin::Bulk,
        });
    }
    Ok(())
}

/// Convert the documented open-order shape into the venue-neutral reconciliation shape.
fn parse_open_order(value: &Value) -> Result<reconcile::ActualOrder> {
    let side = match value.get("is_buy").and_then(Value::as_bool) {
        Some(true) => Side::Bid,
        Some(false) => Side::Ask,
        None => match value_str(value, "order_direction") {
            Some(side) if side.eq_ignore_ascii_case("buy") => Side::Bid,
            Some(side) if side.eq_ignore_ascii_case("sell") => Side::Ask,
            _ => bail!("open order is missing a usable side"),
        },
    };
    Ok(reconcile::ActualOrder {
        order_id: value_str(value, "order_id")
            .ok_or_else(|| anyhow!("open order is missing order_id"))?
            .to_owned(),
        side,
        price: decimal_field(value, "price")
            .ok_or_else(|| anyhow!("open order is missing price"))?,
        remaining_size: decimal_field(value, "remaining_size")
            .or_else(|| decimal_field(value, "orig_size"))
            .ok_or_else(|| anyhow!("open order is missing remaining_size"))?,
        // Individually placed orders carry no client-order ID, so ownership cannot be proven.
        origin: reconcile::OrderOrigin::Standalone,
    })
}

/// Read the current plan and current orders, then compare them without submitting, replacing, or
/// cancelling anything. This is the safe first step for startup and operational reconciliation.
/// Fit a Spot snapshot's plan to its PFS balances. Perp plans are unchanged.
///
/// This is deliberately shared by read-only reconciliation and the live executor so both report
/// the same desired ladder. It never moves funds or submits orders.
pub fn fit_spot_snapshot_to_pfs(snapshot: &mut MonitorSnapshot) -> Result<Option<String>> {
    if snapshot.market.product != Product::Spot {
        return Ok(None);
    }
    let funds = snapshot.account.spot_funds.as_ref().ok_or_else(|| {
        anyhow!(
            "spot funds unavailable for {}: account overview did not include PFS balances",
            snapshot.market.name
        )
    })?;
    // Use the bulk-replacement figures: a new bulk submission credits the existing escrow
    // against its requirement, so the resting ladder's reserved inventory is spendable here.
    let quote_available = funds.available_quote_for_bulk();
    let base_available = funds.available_base_for_bulk();
    if quote_available >= snapshot.plan.quote_required
        && base_available >= snapshot.plan.base_required
    {
        return Ok(None);
    }
    shrink_spot_to_available(
        &mut snapshot.plan,
        quote_available,
        base_available,
        &snapshot.market,
    )
    .map(Some)
}

pub async fn reconcile_snapshot(
    client: &DecibelClient,
    config: &GridConfig,
    subaccount: &str,
) -> Result<(MonitorSnapshot, reconcile::Reconciliation)> {
    if subaccount.trim().is_empty() {
        bail!("subaccount address is required for reconciliation")
    }
    let mut snapshot = fetch_snapshot(client, config, Some(subaccount)).await?;
    // Trade history only provides a UI hint. Rebuild an executable plan so a historical fill at
    // the same price cannot suppress a future desired order during reconciliation.
    snapshot.plan = build_plan(config, &snapshot.market, snapshot.plan.mid)?;
    fit_spot_snapshot_to_pfs(&mut snapshot)?;
    let actual = client.open_orders(subaccount, &snapshot.market).await?;
    let desired = reconcile::desired_orders(
        &snapshot.plan,
        snapshot.market.tick_size,
        snapshot.market.lot_size,
    );
    let result = reconcile::reconcile(
        &desired,
        &actual,
        snapshot.market.tick_size,
        snapshot.market.lot_size,
    );
    snapshot.reconciliation = Some(result.clone());
    Ok((snapshot, result))
}

fn parse_spot_funds(overview: &Value, market: &Market) -> Option<SpotFunds> {
    let spot = overview.get("spot")?;
    let positions = spot.get("positions")?.as_array()?;
    let base_symbol = market
        .base_symbol
        .clone()
        .unwrap_or_else(|| market.name.split('/').next().unwrap_or("BASE").to_owned());
    let quote_symbol = market
        .quote_symbol
        .clone()
        .unwrap_or_else(|| market.name.split('/').nth(1).unwrap_or("USDC").to_owned());
    let matches_asset = |row: &Value, symbol: &str, address: Option<&String>| {
        address.is_some_and(|expected| {
            normalized_address(
                row.get("asset_addr")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            ) == normalized_address(expected)
        }) || row
            .get("asset_symbol")
            .and_then(Value::as_str)
            .is_some_and(|actual| actual.eq_ignore_ascii_case(symbol))
    };
    let position_amount = |symbol: &str, address: Option<&String>| {
        positions
            .iter()
            .filter(|row| matches_asset(row, symbol, address))
            .filter_map(|row| decimal_field(row, "amount"))
            .sum()
    };
    let base_balance: Decimal = position_amount(&base_symbol, market.base_asset_addr.as_ref());
    // The market metadata may omit asset addresses, while in-flight reservations identify assets
    // by address. Recover the addresses from the corresponding Spot positions before classifying
    // reservations; otherwise an APT reservation is incorrectly counted as USDC.
    let position_asset_address = |symbol: &str| {
        positions.iter().find_map(|row| {
            row.get("asset_symbol")
                .and_then(Value::as_str)
                .filter(|actual| actual.eq_ignore_ascii_case(symbol))
                .and_then(|_| row.get("asset_addr"))
                .and_then(Value::as_str)
        })
    };
    let base_asset_address = market
        .base_asset_addr
        .as_deref()
        .or_else(|| position_asset_address(&base_symbol));
    let quote_asset_address = market
        .quote_asset_addr
        .as_deref()
        .or_else(|| position_asset_address(&quote_symbol));
    // PFS quote position. Kept separate from the Cross balance below: which of the two the spot
    // bulk entry function can actually spend is not observable from any read-only endpoint.
    let quote_balance: Decimal = position_amount(&quote_symbol, market.quote_asset_addr.as_ref());
    // Observed on testnet: spot sell proceeds settle into the Cross/collateral USDC balance
    // rather than into `spot.positions`. Recorded for diagnostics and for the opt-in funding
    // policy; never silently folded into the PFS figure.
    let quote_cross_balance =
        decimal_field(overview, "usdc_cross_withdrawable_balance").unwrap_or(Decimal::ZERO);
    let (base_reserved, quote_reserved) = spot
        .get("in_flight_orders")
        .and_then(Value::as_array)
        .map(|orders| {
            orders
                .iter()
                .fold((Decimal::ZERO, Decimal::ZERO), |(base, quote), row| {
                    let amount = decimal_field(row, "reserved_amount").unwrap_or(Decimal::ZERO);
                    let asset = row
                        .get("reserved_asset")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let is_base = base_asset_address.is_some_and(|expected| {
                        normalized_address(asset) == normalized_address(expected)
                    }) || asset.eq_ignore_ascii_case(&base_symbol);
                    let is_quote = quote_asset_address.is_some_and(|expected| {
                        normalized_address(asset) == normalized_address(expected)
                    }) || asset.eq_ignore_ascii_case(&quote_symbol);
                    if is_base {
                        (base + amount, quote)
                    } else if is_quote {
                        (base, quote + amount)
                    } else {
                        (base, quote)
                    }
                })
        })
        .unwrap_or((Decimal::ZERO, Decimal::ZERO));
    Some(SpotFunds {
        base_symbol,
        quote_symbol,
        base_balance,
        quote_balance,
        base_reserved,
        quote_reserved,
        quote_cross_balance,
    })
}

fn parse_market(value: &Value, wanted_product: Product) -> Result<Option<Market>> {
    let product = match value_str(value, "asset_type").or_else(|| value_str(value, "product")) {
        Some("spot") | Some("Spot") => Product::Spot,
        Some(_) | None => Product::Perp,
    };
    if product != wanted_product {
        return Ok(None);
    }
    let px_decimals = integer_field(value, "px_decimals").unwrap_or(0) as u32;
    let sz_decimals = integer_field(value, "sz_decimals").unwrap_or(0) as u32;
    let tick = scale_raw(
        decimal_field(value, "tick_size").ok_or_else(|| anyhow!("market tick_size missing"))?,
        px_decimals,
    );
    let lot = scale_raw(
        decimal_field(value, "lot_size").ok_or_else(|| anyhow!("market lot_size missing"))?,
        sz_decimals,
    );
    let min = scale_raw(
        decimal_field(value, "min_size").ok_or_else(|| anyhow!("market min_size missing"))?,
        sz_decimals,
    );
    Ok(Some(Market {
        address: value_str(value, "market_addr")
            .ok_or_else(|| anyhow!("market_addr missing"))?
            .to_owned(),
        name: value_str(value, "market_name")
            .ok_or_else(|| anyhow!("market_name missing"))?
            .to_owned(),
        tick_size: tick,
        lot_size: lot,
        min_size: min,
        px_decimals,
        sz_decimals,
        product,
        base_asset_addr: value_str(value, "base_asset_addr").map(str::to_owned),
        quote_asset_addr: value_str(value, "quote_asset_addr").map(str::to_owned),
        base_symbol: value_str(value, "base_symbol").map(str::to_owned),
        quote_symbol: value_str(value, "quote_symbol").map(str::to_owned),
    }))
}

fn scale_raw(raw: Decimal, decimals: u32) -> Decimal {
    raw / Decimal::from(10u64.pow(decimals))
}
fn value_str<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key)?.as_str()
}
fn decimal_field(value: &Value, key: &str) -> Option<Decimal> {
    value.get(key).and_then(decimal_value)
}
fn decimal_value(value: &Value) -> Option<Decimal> {
    match value {
        Value::String(v) => Decimal::from_str(v).ok(),
        Value::Number(v) => Decimal::from_str(&v.to_string()).ok(),
        _ => None,
    }
}
fn integer_field(value: &Value, key: &str) -> Option<i64> {
    value
        .get(key)?
        .as_i64()
        .or_else(|| value.get(key)?.as_str()?.parse().ok())
}
fn normalized_address(value: &str) -> String {
    value
        .trim_start_matches("0x")
        .trim_start_matches('0')
        .to_ascii_lowercase()
}

fn price_row_matches_market(row: &Value, market: &Market) -> bool {
    normalized_address(value_str(row, "market").unwrap_or_default())
        == normalized_address(&market.address)
}

pub fn format_decimal(value: Decimal, scale: u32) -> String {
    value
        .round_dp_with_strategy(scale, RoundingStrategy::ToZero)
        .normalize()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn bulk_levels_are_visible_to_reconciliation() {
        let row = serde_json::json!({
            "sequence_number": 427,
            "bid_prices": [0.55],
            "bid_sizes": [10.0],
            "ask_prices": [0.56, 0.57],
            "ask_sizes": [10.0, 5.0]
        });
        let mut orders = Vec::new();
        append_bulk_levels(&mut orders, &row, "bid_prices", "bid_sizes", Side::Bid, 427)
            .expect("bulk bids");
        append_bulk_levels(&mut orders, &row, "ask_prices", "ask_sizes", Side::Ask, 427)
            .expect("bulk asks");
        assert_eq!(orders.len(), 3);
        assert_eq!(orders[0].order_id, "bulk:427:Bid:0");
        assert_eq!(orders[1].side, Side::Ask);
        assert_eq!(orders[2].remaining_size, dec!(5));
    }

    #[test]
    fn bulk_levels_reject_mismatched_price_and_size_arrays() {
        let row = serde_json::json!({
            "bid_prices": [0.55],
            "bid_sizes": []
        });
        let error = append_bulk_levels(
            &mut Vec::new(),
            &row,
            "bid_prices",
            "bid_sizes",
            Side::Bid,
            1,
        )
        .expect_err("mismatched bulk arrays must fail");
        assert!(error.to_string().contains("length mismatch"));
    }

    fn market() -> Market {
        Market {
            address: "0x1".to_owned(),
            name: "BTC/USD".to_owned(),
            tick_size: dec!(1),
            lot_size: dec!(0.01),
            min_size: dec!(0.01),
            px_decimals: 0,
            sz_decimals: 2,
            product: Product::Perp,
            base_asset_addr: None,
            quote_asset_addr: None,
            base_symbol: None,
            quote_symbol: None,
        }
    }
    fn config() -> GridConfig {
        GridConfig {
            product: Product::Perp,
            perp_mode: PerpMode::Neutral,
            market_name: "BTC/USD".to_owned(),
            range: RangeSpec::Percent { percent: dec!(10) },
            total_count: 40,
            allocation: Allocation::TotalBudget(dec!(1000)),
            maker_fee_rate: dec!(0.0001),
            preview_leverage: dec!(1),
            refresh: Duration::from_secs(3),
            price_source: PriceSource::Prices,
        }
    }

    #[test]
    fn spot_funds_keep_positions_net_of_in_flight_reservations() {
        // `base_balance`/`quote_balance` come from `spot.positions`, which the account overview's
        // own arithmetic (`total_usd` = positions + reserved) proves is already net of
        // `in_flight_orders`. Available must equal the position balance as-is, not balance minus
        // reserved again — see `available_base`/`available_quote` doc comments for the proof.
        let funds = SpotFunds {
            base_symbol: "BTC".to_owned(),
            quote_symbol: "USDC".to_owned(),
            base_balance: dec!(0.5),
            quote_balance: dec!(1000),
            base_reserved: dec!(0.2),
            quote_reserved: dec!(400),
            quote_cross_balance: Decimal::ZERO,
        };
        assert_eq!(funds.available_base(), dec!(0.5));
        assert_eq!(funds.available_quote(), dec!(1000));
    }

    #[test]
    fn parse_spot_funds_reads_generic_assets_and_reservations() {
        let overview = serde_json::json!({
            "spot": {
                "positions": [
                    {"asset_symbol": "BTC", "amount": 0.5},
                    {"asset_symbol": "USDC", "amount": 1000.0}
                ],
                "in_flight_orders": [
                    {"reserved_asset": "BTC", "reserved_amount": 0.2},
                    {"reserved_asset": "USDC", "reserved_amount": 400.0}
                ]
            }
        });
        let market = Market {
            address: "0x1".to_owned(),
            name: "BTC/USDC".to_owned(),
            tick_size: dec!(0.01),
            lot_size: dec!(0.00001),
            min_size: dec!(0.00001),
            px_decimals: 2,
            sz_decimals: 5,
            product: Product::Spot,
            base_asset_addr: None,
            quote_asset_addr: None,
            base_symbol: Some("BTC".to_owned()),
            quote_symbol: Some("USDC".to_owned()),
        };
        let funds = parse_spot_funds(&overview, &market).expect("spot funds");
        assert_eq!(funds.available_base(), dec!(0.5));
        assert_eq!(funds.available_quote(), dec!(1000));
    }

    #[test]
    fn spot_funds_never_report_negative_available_balances() {
        // Even though reservations are not subtracted from `positions` again, a negative
        // position balance (which should never happen, but must not panic or underflow) is
        // still floored at zero rather than propagated.
        let funds = SpotFunds {
            base_symbol: "APT".to_owned(),
            quote_symbol: "USDC".to_owned(),
            base_balance: dec!(-1),
            quote_balance: dec!(-10),
            base_reserved: dec!(2),
            quote_reserved: dec!(20),
            quote_cross_balance: Decimal::ZERO,
        };
        assert_eq!(funds.available_base(), Decimal::ZERO);
        assert_eq!(funds.available_quote(), Decimal::ZERO);
    }

    #[test]
    fn in_flight_reservation_is_classified_by_asset_address_from_positions() {
        // The /markets row for APT/USDC returns null asset addresses, while in_flight_orders
        // identifies the reserved asset by address. The base reservation must not be counted
        // against the quote balance.
        let overview = serde_json::json!({
            "usdc_cross_withdrawable_balance": 958.884555,
            "spot": {
                "positions": [
                    {"asset_addr": "0xa", "asset_symbol": "APT", "amount": 8.078783},
                    {"asset_addr": "0x5428", "asset_symbol": "USDC", "amount": 0.0005}
                ],
                "in_flight_orders": [
                    {"reserved_asset": "0xa", "reserved_amount": 70.0}
                ]
            }
        });
        let market = Market {
            address: "0x26f1".to_owned(),
            name: "APT/USDC".to_owned(),
            tick_size: dec!(0.0001),
            lot_size: dec!(0.01),
            min_size: dec!(10),
            px_decimals: 4,
            sz_decimals: 2,
            product: Product::Spot,
            base_asset_addr: None,
            quote_asset_addr: None,
            base_symbol: None,
            quote_symbol: None,
        };
        // Verified against a live testnet account: 8.078783 APT free + 70 APT reserved in a
        // resting sell ladder were both real (78.078783 total, matching the wallet display).
        // `base_reserved` is recorded for display/future verification; `available_base` must
        // still report the free 8.078783, not double-subtract the 70 already excluded from
        // `positions.amount`.
        let funds = parse_spot_funds(&overview, &market).expect("spot funds");
        assert_eq!(funds.base_reserved, dec!(70));
        assert_eq!(funds.quote_reserved, Decimal::ZERO);
        assert_eq!(funds.available_base(), dec!(8.078783));
        // On replacement the resting ladder's escrow is credited by the Move entry function.
        assert_eq!(funds.available_base_for_bulk(), dec!(78.078783));
        // Cross USDC is NOT spendable by `place_bulk_order_from_pfs`; only PFS quote counts.
        assert_eq!(funds.available_quote(), dec!(0.0005));
        assert_eq!(funds.available_quote_for_bulk(), dec!(0.0005));
        assert_eq!(funds.quote_cross_balance(), dec!(958.884555));
    }

    #[test]
    fn spot_funds_do_not_treat_cross_quote_as_bulk_funding() {
        // Verified on-chain: `source_bulk_funds_from_pfs` asserts against
        // `primary_fungible_store::balance` alone, so counting Cross here produced a plan the
        // chain rejected with EINSUFFICIENT_PFS_FUNDS(0x1).
        let funds = SpotFunds {
            base_symbol: "APT".to_owned(),
            quote_symbol: "USDC".to_owned(),
            base_balance: dec!(8),
            quote_balance: dec!(0.0005),
            base_reserved: Decimal::ZERO,
            quote_reserved: Decimal::ZERO,
            quote_cross_balance: dec!(958.884555),
        };
        assert_eq!(funds.available_quote(), dec!(0.0005));
        assert_eq!(funds.quote_cross_balance(), dec!(958.884555));

        // Existing escrow is credited for replacement sizing; Cross funds still are not.
        let with_escrow = SpotFunds {
            quote_reserved: dec!(100),
            ..funds
        };
        assert_eq!(with_escrow.available_quote_for_bulk(), dec!(100.0005));
    }

    #[test]
    fn shrink_spot_keeps_minimum_size_asks_when_proportional_scaling_would_drop_them() {
        let market = Market {
            address: "0x1".to_owned(),
            name: "APT/USDC".to_owned(),
            tick_size: dec!(0.0001),
            lot_size: dec!(0.01),
            min_size: dec!(10),
            px_decimals: 4,
            sz_decimals: 2,
            product: Product::Spot,
            base_asset_addr: None,
            quote_asset_addr: None,
            base_symbol: Some("APT".to_owned()),
            quote_symbol: Some("USDC".to_owned()),
        };
        let level = |side, price| GridLevel {
            side,
            price,
            size: dec!(20.56),
            notional: price * dec!(20.56),
            state: LevelState::Planned,
        };
        let mut plan = GridPlan {
            mid: dec!(0.578),
            lower: dec!(0.55),
            upper: dec!(0.61),
            bids: vec![level(Side::Bid, dec!(0.57)); 4],
            asks: vec![
                level(Side::Ask, dec!(0.58)),
                level(Side::Ask, dec!(0.59)),
                level(Side::Ask, dec!(0.60)),
                level(Side::Ask, dec!(0.61)),
            ],
            quote_required: dec!(50),
            base_required: dec!(82.24),
            estimated_margin: None,
        };
        shrink_spot_to_available(&mut plan, Decimal::ZERO, dec!(21.048783), &market).unwrap();
        assert!(plan.bids.is_empty());
        assert_eq!(plan.asks.len(), 2);
        assert!(plan.asks.iter().all(|level| level.size >= market.min_size));
        assert!(plan.base_required <= dec!(21.048783));
    }

    #[test]
    fn shrink_spot_restores_descending_bid_order_for_the_bulk_abi() {
        // Regression: the affordability pass sorts bids ascending to keep the levels nearest mid.
        // If that order leaks out, the Move bulk ABI rejects the transaction at level 1 with
        // "bulk Bid prices are not strictly ordered". Bids must come back highest-price-first,
        // and asks must stay lowest-price-first.
        let market = Market {
            address: "0x1".to_owned(),
            name: "APT/USDC".to_owned(),
            tick_size: dec!(0.0001),
            lot_size: dec!(0.01),
            min_size: dec!(10),
            px_decimals: 4,
            sz_decimals: 2,
            product: Product::Spot,
            base_asset_addr: None,
            quote_asset_addr: None,
            base_symbol: Some("APT".to_owned()),
            quote_symbol: Some("USDC".to_owned()),
        };
        let level = |side, price: Decimal| GridLevel {
            side,
            price,
            size: dec!(24.5),
            notional: price * dec!(24.5),
            state: LevelState::Planned,
        };
        let mut plan = GridPlan {
            mid: dec!(0.5377),
            lower: dec!(0.4839),
            upper: dec!(0.5915),
            // Built by `prices()` in ABI order: descending for bids.
            bids: vec![
                level(Side::Bid, dec!(0.5363)),
                level(Side::Bid, dec!(0.5350)),
                level(Side::Bid, dec!(0.5336)),
                level(Side::Bid, dec!(0.5323)),
            ],
            asks: vec![
                level(Side::Ask, dec!(0.5390)),
                level(Side::Ask, dec!(0.5404)),
            ],
            quote_required: dec!(52.5),
            base_required: dec!(49),
            estimated_margin: None,
        };

        shrink_spot_to_available(&mut plan, dec!(60), dec!(25), &market).unwrap();

        assert!(plan.bids.len() >= 2, "expected surviving bid levels");
        assert!(
            plan.bids
                .windows(2)
                .all(|pair| pair[0].price > pair[1].price),
            "bids must be strictly descending for the bulk ABI, got {:?}",
            plan.bids.iter().map(|l| l.price).collect::<Vec<_>>()
        );
        assert!(
            plan.asks
                .windows(2)
                .all(|pair| pair[0].price < pair[1].price),
            "asks must be strictly ascending for the bulk ABI"
        );

        // The real failure was only caught on-chain, so assert the pre-signing validator agrees.
        let bids: Vec<&GridLevel> = plan.bids.iter().collect();
        let asks: Vec<&GridLevel> = plan.asks.iter().collect();
        prepare_bulk_order_parameters(1, &bids, &asks, &market)
            .expect("shrunk plan must satisfy the bulk ABI ordering invariants");
    }

    #[test]
    fn total_count_splits_between_sides() {
        assert_eq!(side_counts(&config()), (20, 20));
        assert_eq!(
            side_counts(&GridConfig {
                total_count: 41,
                ..config()
            }),
            (20, 21)
        );
    }
    #[test]
    fn plan_obeys_budget_and_level_limit() {
        let mut market = market();
        market.tick_size = dec!(0.1);
        let plan = build_plan(&config(), &market, dec!(100)).unwrap();
        assert_eq!(plan.bids.len(), 20);
        assert_eq!(plan.asks.len(), 20);
        assert!(plan.estimated_margin.unwrap() <= dec!(1000));
    }
    #[test]
    fn step_grid_is_compounded() {
        let plan = build_plan(
            &GridConfig {
                range: RangeSpec::StepPercent { percent: dec!(1) },
                total_count: 4,
                allocation: Allocation::FixedSize(dec!(1)),
                ..config()
            },
            &market(),
            dec!(100),
        )
        .unwrap();
        assert_eq!(plan.bids[0].price, dec!(99));
        assert_eq!(plan.asks[0].price, dec!(101));
    }
    #[test]
    fn trade_marks_matching_level_filled() {
        let mut plan = build_plan(
            &GridConfig {
                total_count: 4,
                allocation: Allocation::FixedSize(dec!(1)),
                ..config()
            },
            &market(),
            dec!(100),
        )
        .unwrap();
        let price = plan.bids[0].price;
        plan.apply_trade_history(
            &[Trade {
                price,
                size: dec!(1),
                timestamp_ms: 0,
            }],
            dec!(1),
        );
        assert_eq!(plan.bids[0].state, LevelState::Filled);
    }

    #[test]
    fn rebuilt_execution_plan_ignores_historical_fill_markers() {
        let config = GridConfig {
            total_count: 4,
            allocation: Allocation::FixedSize(dec!(1)),
            ..config()
        };
        let market = market();
        let mut display_plan = build_plan(&config, &market, dec!(100)).unwrap();
        let filled_price = display_plan.bids[0].price;
        display_plan.apply_trade_history(
            &[Trade {
                price: filled_price,
                size: dec!(1),
                timestamp_ms: 0,
            }],
            market.tick_size,
        );
        assert_eq!(display_plan.bids[0].state, LevelState::Filled);

        // Live reconciliation rebuilds the plan from market/config rather than trusting an old
        // trade-history marker, so the level remains eligible for a future order.
        let executable_plan = build_plan(&config, &market, dec!(100)).unwrap();
        assert!(
            executable_plan
                .all_levels()
                .all(|level| level.state == LevelState::Planned)
        );
    }

    #[test]
    fn api_key_format_rejects_empty_whitespace_and_control_values() {
        assert!(validate_api_key_format("").is_err());
        assert!(validate_api_key_format(" key").is_err());
        assert!(validate_api_key_format("key\nvalue").is_err());
        assert!(validate_api_key_format("valid-key").is_ok());
    }

    #[test]
    fn api_key_format_rejects_unreasonably_long_values() {
        let key = "k".repeat(513);
        assert!(validate_api_key_format(&key).is_err());
        assert!(validate_api_key_format(&"k".repeat(512)).is_ok());
    }

    #[test]
    fn recorded_funding_order_match_requires_post_only_buy_price_and_size() {
        let matching = serde_json::json!({
            "is_buy": true,
            "time_in_force": "POST_ONLY",
            "price": 5.995,
            "orig_size": 100.0,
            "remaining_size": 40.0
        });
        assert!(is_recorded_funding_order(&matching, dec!(5.995), dec!(100)));

        let manual_buy = serde_json::json!({
            "is_buy": true,
            "time_in_force": "GTC",
            "price": 5.995,
            "orig_size": 100.0
        });
        assert!(!is_recorded_funding_order(
            &manual_buy,
            dec!(5.995),
            dec!(100)
        ));

        let different_size = serde_json::json!({
            "is_buy": true,
            "time_in_force": "POST_ONLY",
            "price": 5.995,
            "orig_size": 101.0
        });
        assert!(!is_recorded_funding_order(
            &different_size,
            dec!(5.995),
            dec!(100)
        ));
    }

    fn spot_funding_market() -> Market {
        Market {
            address: "0x1".to_owned(),
            name: "APT/USDC".to_owned(),
            tick_size: dec!(0.0001),
            lot_size: dec!(0.01),
            min_size: dec!(0.01),
            px_decimals: 4,
            sz_decimals: 2,
            product: Product::Spot,
            base_asset_addr: None,
            quote_asset_addr: None,
            base_symbol: Some("APT".to_owned()),
            quote_symbol: Some("USDC".to_owned()),
        }
    }

    fn spot_funding_funds(base: Decimal, quote: Decimal) -> SpotFunds {
        SpotFunds {
            base_symbol: "APT".to_owned(),
            quote_symbol: "USDC".to_owned(),
            base_balance: base,
            quote_balance: quote,
            base_reserved: Decimal::ZERO,
            quote_reserved: Decimal::ZERO,
            quote_cross_balance: Decimal::ZERO,
        }
    }

    fn spot_funding_grid(base_required: Decimal, quote_required: Decimal) -> GridPlan {
        GridPlan {
            mid: dec!(0.5372),
            lower: dec!(0.4834),
            upper: dec!(0.591),
            bids: vec![],
            asks: vec![],
            quote_required,
            base_required,
            estimated_margin: None,
        }
    }

    #[test]
    fn taker_funding_buys_the_full_gap_when_quote_surplus_allows() {
        // The reported live case: 40 bids reserve ~500 USDC of a ~969 USDC PFS balance, leaving
        // enough surplus to buy the entire missing ask inventory rather than shrinking to 6 asks.
        let funds = spot_funding_funds(dec!(64.367828), dec!(969.539875));
        let grid = spot_funding_grid(dec!(120), dec!(499.830392));
        let funding =
            compute_spot_taker_funding(&funds, &grid, dec!(0.5375), &spot_funding_market())
                .unwrap();
        assert_eq!(funding.base_gap, dec!(55.632172));
        // Whole gap is affordable, so the plan buys it all and the ask side is never shrunk.
        assert_eq!(funding.quantity, dec!(55.63));
    }

    #[test]
    fn taker_funding_never_spends_quote_reserved_for_the_bids() {
        // Only 20 USDC is spare above the bid reserve, so the buy must be bounded by that surplus
        // (including taker-fee headroom), not by the much larger base gap.
        let funds = spot_funding_funds(Decimal::ZERO, dec!(520));
        let grid = spot_funding_grid(dec!(800), dec!(500));
        let market = spot_funding_market();
        let funding = compute_spot_taker_funding(&funds, &grid, dec!(0.5), &market).unwrap();
        assert_eq!(funding.quote_surplus, dec!(20));
        let inclusive_cost =
            funding.quantity * funding.limit_price * (Decimal::ONE + Decimal::new(1, 3));
        assert!(
            inclusive_cost <= funding.quote_surplus,
            "cost {inclusive_cost} must stay within surplus {}",
            funding.quote_surplus
        );
        // Buying is still bounded well below the 800 APT gap.
        assert!(funding.quantity < funding.base_gap);
    }

    #[test]
    fn taker_funding_limit_price_crosses_the_spread_but_is_bounded() {
        let funds = spot_funding_funds(Decimal::ZERO, dec!(1000));
        let grid = spot_funding_grid(dec!(100), dec!(500));
        let market = spot_funding_market();
        let best_ask = dec!(0.5375);
        let funding = compute_spot_taker_funding(&funds, &grid, best_ask, &market).unwrap();
        // Aggressive enough to take the resting ask, but never an unbounded market order.
        assert!(funding.limit_price >= best_ask);
        assert!(funding.limit_price <= best_ask * dec!(1.004));
    }

    #[test]
    fn taker_funding_reports_no_gap_when_inventory_is_already_sufficient() {
        let funds = spot_funding_funds(dec!(150), dec!(1000));
        let grid = spot_funding_grid(dec!(120), dec!(500));
        let funding =
            compute_spot_taker_funding(&funds, &grid, dec!(0.5375), &spot_funding_market())
                .unwrap();
        assert_eq!(funding.base_gap, Decimal::ZERO);
        assert_eq!(funding.quantity, Decimal::ZERO);
    }

    #[test]
    fn taker_funding_counts_existing_bulk_escrow_as_held_inventory() {
        // Reproduces the live symptom: an active ladder already escrows 36 APT while only 1.63
        // sits free in PFS. The replacement ABI credits that escrow, so the funding gap is
        // measured against base + escrow. Using the free balance alone would re-buy inventory
        // the account already owns.
        let funds = SpotFunds {
            base_symbol: "APT".to_owned(),
            quote_symbol: "USDC".to_owned(),
            base_balance: dec!(1.63264),
            quote_balance: dec!(488.418878),
            base_reserved: dec!(36),
            quote_reserved: dec!(499.876920),
            quote_cross_balance: Decimal::ZERO,
        };
        let grid = spot_funding_grid(dec!(40), dec!(499.876920));
        let funding =
            compute_spot_taker_funding(&funds, &grid, dec!(0.5368), &spot_funding_market())
                .unwrap();
        // 40 needed - (1.63264 free + 36 escrowed) = 2.36736, NOT 40 - 1.63264 = 38.36736.
        assert_eq!(funding.base_gap, dec!(2.36736));
        assert!(
            funding.quantity <= dec!(2.37),
            "must not overbuy past the true gap, got {}",
            funding.quantity
        );
    }

    #[test]
    fn taker_funding_ignores_escrow_it_does_not_have() {
        // Same plan, but with no resting ladder: the whole ask side must be bought.
        let funds = SpotFunds {
            base_symbol: "APT".to_owned(),
            quote_symbol: "USDC".to_owned(),
            base_balance: dec!(1.63264),
            quote_balance: dec!(988.295798),
            base_reserved: Decimal::ZERO,
            quote_reserved: Decimal::ZERO,
            quote_cross_balance: Decimal::ZERO,
        };
        let grid = spot_funding_grid(dec!(40), dec!(499.876920));
        let funding =
            compute_spot_taker_funding(&funds, &grid, dec!(0.5368), &spot_funding_market())
                .unwrap();
        assert_eq!(funding.base_gap, dec!(38.36736));
    }

    #[test]
    fn taker_funding_sees_spare_quote_while_a_ladder_is_resting() {
        // Live shape: the resting ladder escrows ~499.88 USDC of bids and 36 APT of asks, so free
        // PFS quote (488.42) is BELOW quote_required. Measuring surplus against free PFS alone
        // reports zero spare and refuses to fund, even though the replacement credits the escrow
        // and the free balance is genuinely available to buy base with.
        let funds = SpotFunds {
            base_symbol: "APT".to_owned(),
            quote_symbol: "USDC".to_owned(),
            base_balance: dec!(1.63264),
            quote_balance: dec!(488.418878),
            base_reserved: dec!(36),
            quote_reserved: dec!(499.876920),
            quote_cross_balance: Decimal::ZERO,
        };
        let grid = spot_funding_grid(dec!(40), dec!(499.876920));
        let funding =
            compute_spot_taker_funding(&funds, &grid, dec!(0.5368), &spot_funding_market())
                .unwrap();
        assert!(
            funding.quote_surplus > Decimal::ZERO,
            "escrowed bids must not mask the spare free balance"
        );
        assert_eq!(funding.base_gap, dec!(2.36736));
        assert!(
            funding.quantity >= market_min(),
            "must actually fund the gap"
        );
    }

    #[test]
    fn taker_funding_never_promises_more_quote_than_pfs_holds() {
        // Escrow makes `available_quote_for_bulk` large, but an IOC can only spend free PFS.
        // The surplus must stay within the free balance or the IOC aborts on-chain.
        let funds = SpotFunds {
            base_symbol: "APT".to_owned(),
            quote_symbol: "USDC".to_owned(),
            base_balance: Decimal::ZERO,
            quote_balance: dec!(10),
            base_reserved: Decimal::ZERO,
            quote_reserved: dec!(900),
            quote_cross_balance: Decimal::ZERO,
        };
        let grid = spot_funding_grid(dec!(100), dec!(500));
        let funding =
            compute_spot_taker_funding(&funds, &grid, dec!(0.5), &spot_funding_market()).unwrap();
        assert!(
            funding.quote_surplus <= funds.available_quote(),
            "surplus {} exceeded free PFS {}",
            funding.quote_surplus,
            funds.available_quote()
        );
    }

    fn market_min() -> Decimal {
        spot_funding_market().min_size
    }

    #[test]
    fn taker_funding_rejects_a_non_positive_ask() {
        let funds = spot_funding_funds(Decimal::ZERO, dec!(1000));
        let grid = spot_funding_grid(dec!(100), dec!(500));
        assert!(
            compute_spot_taker_funding(&funds, &grid, Decimal::ZERO, &spot_funding_market())
                .is_err()
        );
    }

    #[test]
    fn compute_spot_funding_plan_when_base_is_sufficient_does_not_buy() {
        let funds = SpotFunds {
            base_symbol: "BTC".to_owned(),
            quote_symbol: "USDC".to_owned(),
            base_balance: dec!(1.5),
            quote_balance: dec!(2000),
            base_reserved: dec!(0.2),
            quote_reserved: dec!(500),
            quote_cross_balance: Decimal::ZERO,
        };
        let grid = GridPlan {
            mid: dec!(60000),
            lower: dec!(57000),
            upper: dec!(63000),
            bids: vec![],
            asks: vec![],
            quote_required: dec!(1000),
            base_required: dec!(1),
            estimated_margin: None,
        };
        let market = Market {
            address: "0x1".to_owned(),
            name: "BTC/USDC".to_owned(),
            tick_size: dec!(1),
            lot_size: dec!(0.001),
            min_size: dec!(0.001),
            px_decimals: 0,
            sz_decimals: 3,
            product: Product::Spot,
            base_asset_addr: None,
            quote_asset_addr: None,
            base_symbol: Some("BTC".to_owned()),
            quote_symbol: Some("USDC".to_owned()),
        };
        let plan =
            compute_spot_funding_plan(&funds, &grid, dec!(59900), dec!(60000), &market).unwrap();
        assert_eq!(plan.base_gap, Decimal::ZERO);
        assert_eq!(plan.buy_quantity, Decimal::ZERO);
        assert!(plan.buy_price.is_none());
    }

    #[test]
    fn compute_spot_funding_plan_can_use_mid_when_order_book_has_no_bid() {
        let funds = SpotFunds {
            base_symbol: "APT".to_owned(),
            quote_symbol: "USDC".to_owned(),
            base_balance: Decimal::ZERO,
            quote_balance: dec!(1000),
            base_reserved: Decimal::ZERO,
            quote_reserved: Decimal::ZERO,
            quote_cross_balance: Decimal::ZERO,
        };
        let grid = GridPlan {
            mid: dec!(0.5782),
            lower: dec!(0.55),
            upper: dec!(0.61),
            bids: vec![],
            asks: vec![],
            quote_required: dec!(400),
            base_required: dec!(100),
            estimated_margin: None,
        };
        let market = Market {
            address: "0x1".to_owned(),
            name: "APT/USDC".to_owned(),
            tick_size: dec!(0.0001),
            lot_size: dec!(0.01),
            min_size: dec!(0.01),
            px_decimals: 4,
            sz_decimals: 2,
            product: Product::Spot,
            base_asset_addr: None,
            quote_asset_addr: None,
            base_symbol: Some("APT".to_owned()),
            quote_symbol: Some("USDC".to_owned()),
        };
        // The caller passes mid as the best-bid fallback when depth is temporarily empty.
        let plan = compute_spot_funding_plan(&funds, &grid, grid.mid, grid.mid, &market).unwrap();
        assert_eq!(plan.buy_price, Some(dec!(0.5779)));
        assert_eq!(plan.buy_quantity, dec!(100));
    }

    #[test]
    fn compute_spot_funding_plan_when_quote_gap_exists_does_not_buy() {
        let funds = SpotFunds {
            base_symbol: "BTC".to_owned(),
            quote_symbol: "USDC".to_owned(),
            base_balance: dec!(0.5),
            quote_balance: dec!(200),
            base_reserved: dec!(0),
            quote_reserved: dec!(0),
            quote_cross_balance: Decimal::ZERO,
        };
        let grid = GridPlan {
            mid: dec!(60000),
            lower: dec!(57000),
            upper: dec!(63000),
            bids: vec![],
            asks: vec![],
            quote_required: dec!(500),
            base_required: dec!(1),
            estimated_margin: None,
        };
        let market = Market {
            address: "0x1".to_owned(),
            name: "BTC/USDC".to_owned(),
            tick_size: dec!(1),
            lot_size: dec!(0.001),
            min_size: dec!(0.001),
            px_decimals: 0,
            sz_decimals: 3,
            product: Product::Spot,
            base_asset_addr: None,
            quote_asset_addr: None,
            base_symbol: Some("BTC".to_owned()),
            quote_symbol: Some("USDC".to_owned()),
        };
        let plan =
            compute_spot_funding_plan(&funds, &grid, dec!(59900), dec!(60000), &market).unwrap();
        assert_eq!(plan.quote_gap, dec!(300));
        assert_eq!(plan.buy_quantity, Decimal::ZERO);
        assert!(plan.buy_price.is_none());
    }

    #[test]
    fn funding_plan_rounds_up_one_lot_within_one_percent_grid_tolerance() {
        let funds = SpotFunds {
            base_symbol: "APT".to_owned(),
            quote_symbol: "USDC".to_owned(),
            base_balance: Decimal::ZERO,
            quote_balance: dec!(100),
            base_reserved: Decimal::ZERO,
            quote_reserved: Decimal::ZERO,
            quote_cross_balance: Decimal::ZERO,
        };
        let grid = GridPlan {
            mid: dec!(1),
            lower: dec!(0.9),
            upper: dec!(1.1),
            bids: vec![],
            asks: vec![],
            quote_required: dec!(99.995),
            base_required: dec!(0.011),
            estimated_margin: None,
        };
        let market = Market {
            address: "0x1".to_owned(),
            name: "APT/USDC".to_owned(),
            tick_size: dec!(0.0001),
            lot_size: dec!(0.01),
            min_size: dec!(0.01),
            px_decimals: 4,
            sz_decimals: 2,
            product: Product::Spot,
            base_asset_addr: None,
            quote_asset_addr: None,
            base_symbol: Some("APT".to_owned()),
            quote_symbol: Some("USDC".to_owned()),
        };
        let plan = compute_spot_funding_plan(&funds, &grid, dec!(1), dec!(1), &market).unwrap();
        assert_eq!(plan.buy_quantity, dec!(0.02));
        assert!(plan.buy_quantity >= grid.base_required);
        assert!(plan.borrowed_from_grid_quote > Decimal::ZERO);
        assert!(plan.borrowed_from_grid_quote <= grid.quote_required * dec!(0.01));
    }

    #[test]
    fn funding_plan_refuses_round_up_beyond_one_percent_grid_tolerance() {
        let funds = SpotFunds {
            base_symbol: "APT".to_owned(),
            quote_symbol: "USDC".to_owned(),
            base_balance: Decimal::ZERO,
            quote_balance: dec!(100),
            base_reserved: Decimal::ZERO,
            quote_reserved: Decimal::ZERO,
            quote_cross_balance: Decimal::ZERO,
        };
        let grid = GridPlan {
            mid: dec!(1),
            lower: dec!(0.9),
            upper: dec!(1.1),
            bids: vec![],
            asks: vec![],
            quote_required: dec!(99.995),
            base_required: dec!(2),
            estimated_margin: None,
        };
        let market = Market {
            address: "0x1".to_owned(),
            name: "APT/USDC".to_owned(),
            tick_size: dec!(0.0001),
            lot_size: dec!(0.01),
            min_size: dec!(0.01),
            px_decimals: 4,
            sz_decimals: 2,
            product: Product::Spot,
            base_asset_addr: None,
            quote_asset_addr: None,
            base_symbol: Some("APT".to_owned()),
            quote_symbol: Some("USDC".to_owned()),
        };
        let plan = compute_spot_funding_plan(&funds, &grid, dec!(1), dec!(1), &market).unwrap();
        assert!(plan.buy_quantity < grid.base_required);
        assert_eq!(plan.borrowed_from_grid_quote, Decimal::ZERO);
    }

    #[test]
    fn compute_spot_funding_plan_when_quote_spare_after_grid_is_used_to_buy_base() {
        let funds = SpotFunds {
            base_symbol: "BTC".to_owned(),
            quote_symbol: "USDC".to_owned(),
            base_balance: dec!(0.5),
            quote_balance: dec!(2000),
            base_reserved: dec!(0),
            quote_reserved: dec!(0),
            quote_cross_balance: Decimal::ZERO,
        };
        let grid = GridPlan {
            mid: dec!(60000),
            lower: dec!(57000),
            upper: dec!(63000),
            bids: vec![],
            asks: vec![],
            quote_required: dec!(500),
            base_required: dec!(1),
            estimated_margin: None,
        };
        let market = Market {
            address: "0x1".to_owned(),
            name: "BTC/USDC".to_owned(),
            tick_size: dec!(1),
            lot_size: dec!(0.001),
            min_size: dec!(0.001),
            px_decimals: 0,
            sz_decimals: 3,
            product: Product::Spot,
            base_asset_addr: None,
            quote_asset_addr: None,
            base_symbol: Some("BTC".to_owned()),
            quote_symbol: Some("USDC".to_owned()),
        };
        let plan =
            compute_spot_funding_plan(&funds, &grid, dec!(59900), dec!(60000), &market).unwrap();
        assert!(plan.base_gap > Decimal::ZERO);
        assert!(plan.buy_price.is_some());
        assert!(plan.buy_quantity > Decimal::ZERO);
        let buy_price = plan.buy_price.unwrap();
        assert!(buy_price < dec!(59900), "buy price must be below best bid");
        assert!(
            buy_price < dec!(60000),
            "buy price must be below market mid"
        );
        let capped = dec!(59900).min(dec!(60000)) * dec!(9995) / dec!(10000);
        let expected_price = (capped / market.tick_size).floor() * market.tick_size;
        assert_eq!(buy_price, expected_price);
        let available_base = funds.available_base();
        let base_gap = (grid.base_required - available_base).max(Decimal::ZERO);
        let spare_q = ((funds.available_quote() - grid.quote_required).max(Decimal::ZERO)
            / (Decimal::ONE + Decimal::new(1, 3)))
        .floor();
        let raw = base_gap.min(spare_q / buy_price);
        let expected_qty = (raw / market.lot_size).floor() * market.lot_size;
        assert_eq!(plan.buy_quantity, expected_qty);
    }
}
