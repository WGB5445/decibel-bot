//! Explicit Spot ladder lifecycle helpers.
//!
//! Stopping a grid always cancels the bulk ladder first.  Asset disposition is a separate
//! decision: retain leaves released PFS funds untouched, while liquidation is handled by the
//! guarded IOC executor.

use anyhow::{Context, Result, anyhow, bail};
use aptos_sdk::{
    account::Ed25519Account,
    transaction::{InputEntryFunctionData, TransactionBuilder},
    types::AccountAddress,
};
use serde_json::Value;

use crate::aptos_tx;
use crate::geomi::GasStationConfig;
use crate::network::{self, default_registry};
use crate::{Market, Product, normalize_private_key, package_for_network};

/// Cancel the complete Spot or Perp bulk ladder for one `(subaccount, market)`.
///
/// A successful response is required before a caller may treat the ladder as stopped.  It does
/// not sell or transfer any asset.
pub async fn cancel_bulk_ladder(
    network: &str,
    private_key: &str,
    subaccount: &str,
    market: &Market,
    gas_station: Option<&GasStationConfig>,
) -> Result<String> {
    let package = package_for_network(network)?;
    let key = normalize_private_key(private_key)?;
    let signer =
        Ed25519Account::from_private_key_hex(&key).context("invalid Aptos Ed25519 private key")?;
    let subaccount_addr: AccountAddress =
        subaccount.parse().context("invalid subaccount address")?;
    let market_addr: AccountAddress = market.address.parse().context("invalid market address")?;
    let aptos = default_registry().aptos(network::default_registry().resolve(network)?)?;
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
        bail!("gas price {gas_price} exceeds the 0.5 APT lifecycle cap")
    }
    let entry = match market.product {
        Product::Spot => {
            format!("{package}::dex_accounts_spot_entry::cancel_spot_bulk_order_to_subaccount")
        }
        Product::Perp => format!("{package}::dex_accounts_entry::cancel_bulk_order_to_subaccount"),
    };
    let payload = InputEntryFunctionData::new(&entry)
        .arg(subaccount_addr)
        .arg(market_addr)
        .build()
        .context("build bulk cancellation transaction")?;
    let raw = TransactionBuilder::new()
        .sender(signer.address())
        .sequence_number(aptos.get_sequence_number(signer.address()).await?)
        .payload(payload)
        .max_gas_amount(max_gas_amount)
        .gas_unit_price(gas_price)
        .chain_id(aptos.ensure_chain_id().await?)
        .expiration_from_now(aptos_tx::expiration_seconds(gas_station))
        .build()
        .context("build bulk cancellation transaction")?;
    let response = aptos_tx::submit_raw_and_wait(
        &aptos,
        raw,
        &signer,
        gas_station,
        "submit bulk cancellation transaction",
    )
    .await
    .context("submit bulk cancellation transaction")?;
    if !response
        .get("success")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        bail!(
            "bulk cancellation failed: {}",
            response
                .get("vm_status")
                .and_then(Value::as_str)
                .unwrap_or("unknown VM status")
        )
    }
    let hash = response
        .get("hash")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("bulk cancellation response has no transaction hash"))?;
    Ok(hash.to_owned())
}
