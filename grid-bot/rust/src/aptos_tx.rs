//! Unified Aptos submission: self-paid or Geomi fee-payer.

use std::time::Duration;

use anyhow::{Context, Result};
use aptos_sdk::{
    Aptos,
    account::Ed25519Account,
    transaction::{RawTransaction, builder::sign_transaction},
    types::HashValue,
};
use serde_json::Value;

use crate::geomi::{GasStationConfig, sign_and_submit};

const WAIT_TIMEOUT: Duration = Duration::from_secs(60);

/// Geomi Gas Station rejects transactions with expiration beyond 120 seconds.
const GEOMI_MAX_EXPIRATION: u64 = 120;
/// Self-paid transactions may use the longer default expiration.
const SELF_PAID_EXPIRATION: u64 = 600;

/// Return the appropriate `expiration_from_now` value based on the gas station mode.
pub fn expiration_seconds(gas_station: Option<&GasStationConfig>) -> u64 {
    if gas_station.is_some() {
        GEOMI_MAX_EXPIRATION
    } else {
        SELF_PAID_EXPIRATION
    }
}

/// Sign and submit a built raw transaction, waiting until the fullnode reports commitment.
pub async fn submit_raw_and_wait(
    aptos: &Aptos,
    raw: RawTransaction,
    signer: &Ed25519Account,
    gas_station: Option<&GasStationConfig>,
    context: &str,
) -> Result<Value> {
    submit_raw_and_wait_with_broadcast(aptos, raw, signer, gas_station, context, |_| Ok(())).await
}

/// Submit a transaction and invoke `on_broadcast` as soon as the venue returns its hash, before
/// waiting for commitment. Callers that manage irreversible state must fsync the hash here so a
/// crash during the wait can be recovered without resubmitting.
pub async fn submit_raw_and_wait_with_broadcast<F>(
    aptos: &Aptos,
    raw: RawTransaction,
    signer: &Ed25519Account,
    gas_station: Option<&GasStationConfig>,
    context: &str,
    on_broadcast: F,
) -> Result<Value>
where
    F: FnOnce(&str) -> Result<()>,
{
    if let Some(config) = gas_station {
        let hash = sign_and_submit(config, &raw, signer, context).await?;
        on_broadcast(&hash)?;
        let hash_value = HashValue::from_hex(&hash)
            .with_context(|| format!("{context}: invalid transaction hash from Geomi: {hash}"))?;
        let response = aptos
            .fullnode()
            .wait_for_transaction(&hash_value, Some(WAIT_TIMEOUT))
            .await
            .with_context(|| format!("{context}: wait for Geomi-submitted transaction {hash}"))?;
        return Ok(response.data);
    }

    let signed = sign_transaction(&raw, signer)
        .with_context(|| format!("{context}: sign self-paid transaction"))?;
    let pending = aptos
        .submit_transaction(&signed)
        .await
        .with_context(|| format!("{context}: submit self-paid transaction"))?;
    let hash = pending.data.hash.to_string();
    on_broadcast(&hash)?;
    let hash_value = HashValue::from_hex(&hash)
        .with_context(|| format!("{context}: invalid transaction hash from fullnode: {hash}"))?;
    let response = aptos
        .fullnode()
        .wait_for_transaction(&hash_value, Some(WAIT_TIMEOUT))
        .await
        .with_context(|| format!("{context}: wait for self-paid transaction {hash}"))?;
    Ok(response.data)
}
