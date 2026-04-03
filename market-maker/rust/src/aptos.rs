use anyhow::{Context, Result};
use ed25519_dalek::{Signer, SigningKey};
use reqwest::Client;
use serde_json::{json, Value};
use sha3::{Digest, Sha3_256};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::{debug, warn};

// ─────────────────────────────────────────────────────────────────────────────
// Types
// ─────────────────────────────────────────────────────────────────────────────

/// Result of a committed on-chain transaction.
#[derive(Debug)]
pub struct TxResult {
    pub hash: String,
    pub success: bool,
    pub vm_status: String,
}

impl TxResult {
    /// `true` when the error is "order not found" — treated as a successful cancel.
    pub fn is_acceptable_cancel_failure(&self) -> bool {
        self.vm_status.contains("ERESOURCE_DOES_NOT_EXIST")
            || self.vm_status.contains("EORDER_NOT_FOUND")
    }

    /// `true` when the cancel should be counted as succeeded.
    pub fn cancel_succeeded(&self) -> bool {
        self.success || self.is_acceptable_cancel_failure()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Client
// ─────────────────────────────────────────────────────────────────────────────

pub struct AptosClient {
    client: Client,
    fullnode_url: String,
    node_api_key: String,
    signing_key: SigningKey,
    sender_address: String,
}

impl AptosClient {
    /// Build the client, deriving the sender address from the private key.
    pub fn new(fullnode_url: &str, node_api_key: &str, private_key_bytes: [u8; 32]) -> Self {
        let signing_key = SigningKey::from_bytes(&private_key_bytes);
        let sender_address = derive_address(&signing_key);
        Self {
            client: Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("Failed to build reqwest client"),
            fullnode_url: fullnode_url.trim_end_matches('/').to_string(),
            node_api_key: node_api_key.to_string(),
            signing_key,
            sender_address,
        }
    }

    pub fn sender_address(&self) -> &str {
        &self.sender_address
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    fn get(&self, path: &str) -> reqwest::RequestBuilder {
        let url = format!("{}{}", self.fullnode_url, path);
        debug!(url = %url, "Fullnode GET");
        self.client.get(url).bearer_auth(&self.node_api_key)
    }

    fn post_json(&self, path: &str) -> reqwest::RequestBuilder {
        let url = format!("{}{}", self.fullnode_url, path);
        debug!(url = %url, "Fullnode POST");
        self.client
            .post(url)
            .bearer_auth(&self.node_api_key)
            .header("Content-Type", "application/json")
    }

    // ── Chain queries ─────────────────────────────────────────────────────────

    async fn fetch_sequence_number(&self) -> Result<u64> {
        let resp = self
            .get(&format!("/accounts/{}", self.sender_address))
            .send()
            .await
            .context("fetch_sequence_number: request failed")?;
        resp.error_for_status_ref()
            .context("fetch_sequence_number: HTTP error")?;
        let data: Value = resp.json().await?;
        // sequence_number may come back as a JSON string or integer
        let seq = data["sequence_number"]
            .as_str()
            .and_then(|s| s.parse::<u64>().ok())
            .or_else(|| data["sequence_number"].as_u64())
            .context("Could not parse sequence_number from account response")?;
        Ok(seq)
    }

    async fn fetch_gas_unit_price(&self) -> Result<u64> {
        let resp = self
            .get("/estimate_gas_price")
            .send()
            .await
            .context("fetch_gas_unit_price: request failed")?;
        resp.error_for_status_ref()
            .context("fetch_gas_unit_price: HTTP error")?;
        let data: Value = resp.json().await?;
        let price = data["gas_estimate"]
            .as_u64()
            .context("Could not parse gas_estimate")?;
        Ok(price)
    }

    // ── Transaction lifecycle ─────────────────────────────────────────────────

    /// Build, sign, and submit an entry function transaction.
    ///
    /// Flow:
    ///   1. Parallel-fetch sequence_number and gas_unit_price.
    ///   2. Build unsigned JSON tx body.
    ///   3. POST to /transactions/encode_submission → BCS bytes.
    ///   4. Ed25519-sign the bytes.
    ///   5. Attach signature and POST to /transactions.
    ///   6. Poll /transactions/by_hash until committed.
    pub async fn submit_entry_function(
        &self,
        function: &str,
        type_arguments: Vec<String>,
        arguments: Vec<Value>,
    ) -> Result<TxResult> {
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // Fetch chain state in parallel
        let (seq_num, gas_price) =
            tokio::try_join!(self.fetch_sequence_number(), self.fetch_gas_unit_price())?;

        debug!(
            function,
            seq_num, gas_price, "Building transaction"
        );

        let unsigned_tx = json!({
            "sender":                   self.sender_address,
            "sequence_number":          seq_num.to_string(),
            "max_gas_amount":           "200000",
            "gas_unit_price":           gas_price.to_string(),
            "expiration_timestamp_secs": (now_secs + 60).to_string(),
            "payload": {
                "type":           "entry_function_payload",
                "function":       function,
                "type_arguments": type_arguments,
                "arguments":      arguments
            }
        });

        // Encode to BCS for signing
        let encode_resp = self
            .post_json("/transactions/encode_submission")
            .json(&unsigned_tx)
            .send()
            .await
            .context("encode_submission: request failed")?;
        encode_resp
            .error_for_status_ref()
            .context("encode_submission: HTTP error")?;
        // Response is a JSON-encoded hex string: "\"0xaabb...\""
        let encoded_hex: String = encode_resp
            .json()
            .await
            .context("encode_submission: failed to decode hex string")?;

        let bytes_to_sign = hex::decode(encoded_hex.trim_start_matches("0x"))
            .context("encode_submission: returned invalid hex")?;

        // Sign
        let signature = self.signing_key.sign(&bytes_to_sign);
        let pub_key_bytes = self.signing_key.verifying_key().to_bytes();

        let mut signed_tx = unsigned_tx;
        signed_tx["signature"] = json!({
            "type":       "ed25519_signature",
            "public_key": format!("0x{}", hex::encode(pub_key_bytes)),
            "signature":  format!("0x{}", hex::encode(signature.to_bytes()))
        });

        // Submit
        let submit_resp = self
            .post_json("/transactions")
            .json(&signed_tx)
            .send()
            .await
            .context("submit transaction: request failed")?;
        submit_resp
            .error_for_status_ref()
            .context("submit transaction: HTTP error")?;
        let submit_data: Value = submit_resp
            .json()
            .await
            .context("submit transaction: JSON decode failed")?;

        let hash = submit_data["hash"]
            .as_str()
            .context("No 'hash' field in transaction submit response")?
            .to_string();

        debug!(hash = %hash, "Transaction submitted, polling for result");
        self.wait_for_transaction(&hash).await
    }

    /// Poll until the transaction is committed (or timeout after ~12 s).
    async fn wait_for_transaction(&self, hash: &str) -> Result<TxResult> {
        // Give the network a head-start before the first poll
        tokio::time::sleep(Duration::from_millis(500)).await;

        for attempt in 1..=12 {
            let resp = self
                .get(&format!("/transactions/by_hash/{hash}"))
                .send()
                .await
                .context("wait_for_transaction: request failed")?;

            // 404 means not yet indexed
            if resp.status() == reqwest::StatusCode::NOT_FOUND {
                debug!(attempt, "Transaction not yet indexed");
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }

            resp.error_for_status_ref()
                .context("wait_for_transaction: HTTP error")?;

            let data: Value = resp.json().await?;

            // "pending_transaction" means still in mempool
            if data.get("type").and_then(Value::as_str) == Some("pending_transaction") {
                debug!(attempt, "Transaction pending");
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }

            let success = data["success"].as_bool().unwrap_or(false);
            let vm_status = data["vm_status"]
                .as_str()
                .unwrap_or("unknown")
                .to_string();

            if !success {
                warn!(hash, vm_status = %vm_status, "Transaction failed on-chain");
            } else {
                debug!(hash, "Transaction succeeded");
            }

            return Ok(TxResult {
                hash: hash.to_string(),
                success,
                vm_status,
            });
        }

        anyhow::bail!(
            "Transaction {hash} did not commit after 12 polling attempts (~12 s)"
        )
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Address derivation
// ─────────────────────────────────────────────────────────────────────────────

/// Derive an Aptos account address from an Ed25519 signing key.
///
/// `address = sha3_256(public_key_bytes || 0x00)`  (0x00 = Ed25519 scheme tag)
fn derive_address(signing_key: &SigningKey) -> String {
    let pub_key = signing_key.verifying_key();
    let mut hasher = Sha3_256::new();
    hasher.update(pub_key.as_bytes());
    hasher.update([0x00u8]);
    let digest = hasher.finalize();
    format!("0x{}", hex::encode(digest))
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_address_produces_correct_format() {
        let key_bytes = [1u8; 32];
        let signing_key = SigningKey::from_bytes(&key_bytes);
        let addr = derive_address(&signing_key);
        assert!(addr.starts_with("0x"), "address must start with 0x");
        assert_eq!(addr.len(), 66, "0x + 32 hex bytes = 66 chars");
    }

    #[test]
    fn derive_address_is_deterministic() {
        let key_bytes = [42u8; 32];
        let sk = SigningKey::from_bytes(&key_bytes);
        assert_eq!(derive_address(&sk), derive_address(&sk));
    }

    #[test]
    fn different_keys_give_different_addresses() {
        let sk1 = SigningKey::from_bytes(&[1u8; 32]);
        let sk2 = SigningKey::from_bytes(&[2u8; 32]);
        assert_ne!(derive_address(&sk1), derive_address(&sk2));
    }

    #[test]
    fn tx_result_cancel_logic() {
        let ok = TxResult { hash: "0x1".into(), success: true, vm_status: "Executed successfully".into() };
        assert!(ok.cancel_succeeded());

        let not_found = TxResult {
            hash: "0x2".into(),
            success: false,
            vm_status: "Move abort: EORDER_NOT_FOUND".into(),
        };
        assert!(not_found.cancel_succeeded());

        let no_resource = TxResult {
            hash: "0x3".into(),
            success: false,
            vm_status: "Move abort: ERESOURCE_DOES_NOT_EXIST".into(),
        };
        assert!(no_resource.cancel_succeeded());

        let real_failure = TxResult {
            hash: "0x4".into(),
            success: false,
            vm_status: "Out of gas".into(),
        };
        assert!(!real_failure.cancel_succeeded());
    }
}
