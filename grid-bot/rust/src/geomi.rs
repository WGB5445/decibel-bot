//! Geomi / Aptos Gas Station HTTP client (`/gs/v1`).

use std::time::Duration;

use anyhow::{Context, Result, bail};
use aptos_sdk::{
    account::{Account, Ed25519Account},
    aptos_bcs,
    transaction::{FeePayerRawTransaction, RawTransaction, authenticator::AccountAuthenticator},
    types::AccountAddress,
};
use reqwest::{Client, StatusCode, header};
use serde::Deserialize;

const SUBMIT_PATH: &str = "/api/transaction/signAndSubmit";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// Resolved Gas Station credentials for sponsored on-chain submission.
#[derive(Clone, Debug)]
pub struct GasStationConfig {
    pub api_key: String,
    pub base_url: String,
}

impl GasStationConfig {
    /// Enable Geomi only when a non-empty API key is provided.
    pub fn resolve(
        network: &str,
        api_key: Option<&str>,
        url_override: Option<&str>,
    ) -> Result<Option<Self>> {
        match api_key {
            None => Ok(None),
            Some(key) if key.trim().is_empty() => {
                bail!("GEOMI_GAS_STATION_API_KEY cannot be empty when set")
            }
            Some(key) => {
                let profile = crate::network::default_registry().resolve(network)?;
                let base_url = match url_override
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                {
                    Some(url) => {
                        validate_url_for_network(network, url)?;
                        url.to_owned()
                    }
                    None => profile.gas_station_url.to_owned(),
                };
                Ok(Some(Self {
                    api_key: key.trim().to_owned(),
                    base_url,
                }))
            }
        }
    }
}

fn validate_url_for_network(network: &str, url: &str) -> Result<()> {
    let lower = url.to_ascii_lowercase();
    if network.eq_ignore_ascii_case("mainnet") && lower.contains("testnet") {
        bail!("GEOMI_GAS_STATION_URL points at testnet but NETWORK=mainnet")
    }
    if network.eq_ignore_ascii_case("testnet")
        && lower.contains("mainnet")
        && !lower.contains("testnet")
    {
        bail!("GEOMI_GAS_STATION_URL points at mainnet but NETWORK=testnet")
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SignAndSubmitBody {
    pub transaction_bytes: Vec<u8>,
    pub sender_auth: Vec<u8>,
}

pub(crate) fn build_sign_and_submit_body(
    raw: &RawTransaction,
    signer: &Ed25519Account,
) -> Result<SignAndSubmitBody> {
    let fee_payer_txn = FeePayerRawTransaction::new_simple(raw.clone(), AccountAddress::ZERO);
    let signing_message = fee_payer_txn
        .signing_message()
        .context("build fee-payer signing message")?;
    let signature = signer
        .sign(&signing_message)
        .context("sign fee-payer transaction as sender")?;
    let sender_auth = AccountAuthenticator::ed25519(signer.public_key_bytes(), signature);
    let transaction_bytes =
        aptos_bcs::to_bytes(&fee_payer_txn).context("BCS-encode fee-payer transaction")?;
    let sender_auth_bytes =
        aptos_bcs::to_bytes(&sender_auth).context("BCS-encode sender authenticator")?;
    Ok(SignAndSubmitBody {
        transaction_bytes,
        sender_auth: sender_auth_bytes,
    })
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SignAndSubmitResponse {
    transaction_hash: String,
}

/// POST the sender-signed fee-payer transaction to Geomi and return the committed hash.
pub async fn sign_and_submit(
    config: &GasStationConfig,
    raw: &RawTransaction,
    signer: &Ed25519Account,
    context: &str,
) -> Result<String> {
    let body = build_sign_and_submit_body(raw, signer)?;
    let url = format!("{}{}", config.base_url.trim_end_matches('/'), SUBMIT_PATH);
    let client = Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .context("build Geomi HTTP client")?;
    let payload = serde_json::json!({
        "transactionBytes": body.transaction_bytes,
        "senderAuth": body.sender_auth,
    });
    let response = client
        .post(url)
        .header(header::AUTHORIZATION, format!("Bearer {}", config.api_key))
        .header(header::CONTENT_TYPE, "application/json")
        .json(&payload)
        .send()
        .await
        .with_context(|| format!("{context}: Geomi gas station request failed"))?;

    let status = response.status();
    let response_body = response
        .text()
        .await
        .with_context(|| format!("{context}: read Geomi gas station response"))?;

    if status == StatusCode::UNAUTHORIZED {
        bail!(
            "{context}: Geomi gas station rejected the API key (401 Unauthorized); verify GEOMI_GAS_STATION_API_KEY"
        )
    }
    if status == StatusCode::TOO_MANY_REQUESTS {
        bail!(
            "{context}: Geomi gas station rate limit exceeded (429); reduce grid refresh frequency or raise the station limit"
        )
    }
    if !status.is_success() {
        let detail = classify_gas_station_error(&response_body);
        bail!("{context}: Geomi gas station returned HTTP {status}: {detail}")
    }

    let parsed: SignAndSubmitResponse = serde_json::from_str(&response_body).with_context(|| {
        format!(
            "{context}: parse Geomi gas station response (expected transactionHash): {response_body}"
        )
    })?;
    if parsed.transaction_hash.trim().is_empty() {
        bail!("{context}: Geomi gas station response missing transactionHash")
    }
    Ok(parsed.transaction_hash)
}

fn classify_gas_station_error(body: &str) -> String {
    let lower = body.to_ascii_lowercase();
    if lower.contains("allowlist") || lower.contains("not allowed") {
        return format!(
            "transaction function is not on the Gas Station allowlist; see grid-bot/rust/README.md — {body}"
        );
    }
    if lower.contains("insufficient") && lower.contains("balance") {
        return format!(
            "Gas Station funding account has insufficient APT; top up the station — {body}"
        );
    }
    if lower.contains("recaptcha") {
        return format!(
            "Gas Station requires reCAPTCHA but this bot does not send recaptchaToken; disable reCAPTCHA in the Geomi dashboard — {body}"
        );
    }
    body.to_owned()
}

#[cfg(test)]
mod tests {
    use aptos_sdk::{
        account::Ed25519Account,
        transaction::{
            EntryFunction, RawTransaction, TransactionBuilder, TransactionPayload,
            builder::sign_transaction,
        },
        types::{AccountAddress, ChainId, MoveModuleId},
    };

    use super::*;

    fn sample_raw() -> RawTransaction {
        let payload = TransactionPayload::EntryFunction(EntryFunction {
            module: MoveModuleId::from_str_strict("0x1::coin").unwrap(),
            function: "transfer".to_string(),
            type_args: vec![],
            args: vec![],
        });
        TransactionBuilder::new()
            .sender(AccountAddress::ONE)
            .sequence_number(7)
            .payload(payload)
            .max_gas_amount(100_000)
            .gas_unit_price(100)
            .chain_id(ChainId::testnet())
            .expiration_from_now(600)
            .build()
            .unwrap()
    }

    #[test]
    fn resolve_returns_none_without_key() {
        assert!(
            GasStationConfig::resolve("testnet", None, None)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn resolve_rejects_empty_key() {
        let error = GasStationConfig::resolve("testnet", Some(""), None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("cannot be empty"));
    }

    #[test]
    fn resolve_uses_network_default_url() {
        let config = GasStationConfig::resolve("testnet", Some("aptoslabs_test"), None)
            .unwrap()
            .expect("config");
        assert_eq!(config.base_url, "https://api.testnet.aptoslabs.com/gs/v1");
    }

    #[test]
    fn resolve_rejects_cross_network_url() {
        let error = GasStationConfig::resolve(
            "mainnet",
            Some("aptoslabs_test"),
            Some("https://api.testnet.aptoslabs.com/gs/v1"),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("NETWORK=mainnet"));
    }

    #[test]
    fn fee_payer_sender_auth_differs_from_self_pay_signature() {
        let signer = Ed25519Account::generate();
        let raw = sample_raw();
        let self_pay = sign_transaction(&raw, &signer).unwrap();
        let sponsored = build_sign_and_submit_body(&raw, &signer).unwrap();
        let sponsored_sender =
            aptos_bcs::from_bytes::<AccountAuthenticator>(&sponsored.sender_auth)
                .expect("sender auth BCS");
        assert_ne!(
            aptos_bcs::to_bytes(&self_pay.authenticator).unwrap(),
            sponsored.sender_auth
        );
        assert_eq!(
            aptos_bcs::to_bytes(&sponsored_sender).unwrap(),
            sponsored.sender_auth
        );
    }
}
