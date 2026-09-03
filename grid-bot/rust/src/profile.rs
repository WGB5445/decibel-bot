//! Persisted TUI profiles stored in `~/.config/decibel-grid/profiles.json`.
//!
//! Everything except the API key is stored as plain JSON so the file stays reviewable. The API
//! key is encrypted with Argon2id (password stretching) + XChaCha20-Poly1305 (AEAD), so reading
//! it back requires the password the user chose. A wrong password fails authentication rather
//! than returning garbage.

use std::{collections::BTreeMap, fs, io::Write, path::PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use argon2::Argon2;
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{Aead, KeyInit, OsRng, rand_core::RngCore},
};
use serde::{Deserialize, Serialize};

use crate::i18n::Language;

pub const DEFAULT_PROFILE: &str = "default";

/// A locally recorded POST_ONLY order created solely to acquire Spot base before a grid starts.
/// This is deliberately separate from a user profile: it lets a future process identify *only*
/// this bot's prior funding order without touching manually created orders.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FundingOrderRecord {
    pub network: String,
    pub subaccount: String,
    pub market: String,
    pub price: String,
    pub quantity: String,
    pub order_id: Option<String>,
    pub transaction_hash: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FundingOrderStore {
    pub orders: Vec<FundingOrderRecord>,
}

impl FundingOrderStore {
    pub fn path() -> Result<PathBuf> {
        let base = dirs::data_local_dir()
            .or_else(dirs::data_dir)
            .ok_or_else(|| anyhow!("could not determine the local data directory"))?;
        Ok(base.join("decibel-grid").join("spot-funding-orders.json"))
    }

    pub fn load() -> Result<Self> {
        let path = Self::path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("could not read {}", path.display()))?;
        Ok(serde_json::from_str(&raw).unwrap_or_default())
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("could not create {}", parent.display()))?;
        }
        let body = serde_json::to_string_pretty(self)?;
        write_private(&path, body.as_bytes())
            .with_context(|| format!("could not write {}", path.display()))
    }

    pub fn matching(
        &self,
        network: &str,
        subaccount: &str,
        market: &str,
    ) -> Option<&FundingOrderRecord> {
        self.orders.iter().find(|order| {
            order.network.eq_ignore_ascii_case(network)
                && order.subaccount.eq_ignore_ascii_case(subaccount)
                && order.market.eq_ignore_ascii_case(market)
        })
    }

    pub fn replace(&mut self, record: FundingOrderRecord) {
        self.orders.retain(|order| {
            !(order.network.eq_ignore_ascii_case(&record.network)
                && order.subaccount.eq_ignore_ascii_case(&record.subaccount)
                && order.market.eq_ignore_ascii_case(&record.market))
        });
        self.orders.push(record);
    }

    pub fn remove(&mut self, network: &str, subaccount: &str, market: &str) {
        self.orders.retain(|order| {
            !(order.network.eq_ignore_ascii_case(network)
                && order.subaccount.eq_ignore_ascii_case(subaccount)
                && order.market.eq_ignore_ascii_case(market))
        });
    }
}

/// Everything the TUI remembers between runs. Values are stored as strings so a profile written
/// by a newer build cannot panic an older one on a changed numeric type.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProfileData {
    pub language: Language,
    pub network: String,
    pub product: String,
    pub market: String,
    pub subaccount: String,
    pub perp_mode: String,
    pub range_kind: String,
    pub range_value: String,
    pub upper_bound: String,
    pub grid_count: String,
    pub allocation_kind: String,
    pub allocation_value: String,
    #[serde(default)]
    pub total_quote_budget: Option<String>,
    #[serde(default)]
    pub total_base_budget: Option<String>,
    pub maker_fee_rate: String,
    pub preview_leverage: String,
    pub refresh_seconds: String,
    pub price_source: String,
    /// How to handle assets on exit: "retain" or "sell".
    #[serde(default)]
    pub exit_asset_policy: String,
    #[serde(default)]
    pub min_net_margin_bps: String,
    #[serde(default)]
    pub reconciliation_interval_ms: String,
    #[serde(default)]
    pub ws_reconnect_backoff_ms: String,
    #[serde(default)]
    pub range_breakout_action: String,
    #[serde(default)]
    pub auto_convert_missing_base: String,
    #[serde(default)]
    pub entry_max_slippage_bps: String,
    #[serde(default)]
    pub exit_max_slippage_bps: String,
    #[serde(default)]
    pub entry_exit_max_attempts: String,
    #[serde(default)]
    pub entry_exit_retry_backoff_ms: String,
    #[serde(default)]
    pub entry_exit_timeout_ms: String,
    #[serde(default)]
    pub entry_min_fill_ratio: String,
    #[serde(default)]
    pub price_buffer_bps: String,
    #[serde(default)]
    pub max_consecutive_bulk_failures: String,
    #[serde(default)]
    pub max_position: Option<String>,
    /// Argon2id + XChaCha20-Poly1305 envelopes. `None` when a credential has not been saved.
    pub encrypted_api_key: Option<EncryptedSecret>,
    pub encrypted_aptos_private_key: Option<EncryptedSecret>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EncryptedSecret {
    salt: String,
    nonce: String,
    ciphertext: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProfileStore {
    pub profiles: BTreeMap<String, ProfileData>,
}

impl ProfileStore {
    pub fn path() -> Result<PathBuf> {
        let base = dirs::config_dir()
            .ok_or_else(|| anyhow!("could not determine the user config directory"))?;
        Ok(base.join("decibel-grid").join("profiles.json"))
    }

    pub fn load() -> Result<Self> {
        let path = Self::path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("could not read {}", path.display()))?;
        // A corrupt or hand-edited profile must not prevent the TUI from starting.
        Ok(serde_json::from_str(&raw).unwrap_or_default())
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("could not create {}", parent.display()))?;
        }
        let body = serde_json::to_string_pretty(self)?;
        write_private(&path, body.as_bytes())
            .with_context(|| format!("could not write {}", path.display()))
    }

    pub fn get(&self, name: &str) -> Option<&ProfileData> {
        self.profiles.get(name)
    }

    pub fn put(&mut self, name: &str, data: ProfileData) {
        self.profiles.insert(name.to_owned(), data);
    }

    pub fn remove(&mut self, name: &str) {
        self.profiles.remove(name);
    }

    pub fn names(&self) -> Vec<String> {
        self.profiles.keys().cloned().collect()
    }
}

/// Writes with owner-only permissions so a saved key is not world-readable.
fn write_private(path: &PathBuf, bytes: &[u8]) -> Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn derive_key(password: &str, salt: &[u8]) -> Result<[u8; 32]> {
    let mut key = [0u8; 32];
    Argon2::default()
        .hash_password_into(password.as_bytes(), salt, &mut key)
        .map_err(|error| anyhow!("could not derive an encryption key: {error}"))?;
    Ok(key)
}

pub fn encrypt_secret(password: &str, plaintext: &str) -> Result<EncryptedSecret> {
    if password.is_empty() {
        bail!("a non-empty password is required to encrypt the API key")
    }
    let mut salt = [0u8; 16];
    let mut nonce_bytes = [0u8; 24];
    OsRng.fill_bytes(&mut salt);
    OsRng.fill_bytes(&mut nonce_bytes);
    let key = derive_key(password, &salt)?;
    let cipher = XChaCha20Poly1305::new((&key).into());
    let ciphertext = cipher
        .encrypt(XNonce::from_slice(&nonce_bytes), plaintext.as_bytes())
        .map_err(|_| anyhow!("could not encrypt the API key"))?;
    Ok(EncryptedSecret {
        salt: BASE64.encode(salt),
        nonce: BASE64.encode(nonce_bytes),
        ciphertext: BASE64.encode(ciphertext),
    })
}

pub fn decrypt_secret(password: &str, secret: &EncryptedSecret) -> Result<String> {
    let salt = BASE64.decode(&secret.salt).context("invalid stored salt")?;
    let nonce = BASE64
        .decode(&secret.nonce)
        .context("invalid stored nonce")?;
    let ciphertext = BASE64
        .decode(&secret.ciphertext)
        .context("invalid stored ciphertext")?;
    if nonce.len() != 24 {
        bail!("stored nonce has the wrong length")
    }
    let key = derive_key(password, &salt)?;
    let cipher = XChaCha20Poly1305::new((&key).into());
    let plaintext = cipher
        .decrypt(XNonce::from_slice(&nonce), ciphertext.as_ref())
        .map_err(|_| anyhow!("wrong password or corrupted profile"))?;
    String::from_utf8(plaintext).context("decrypted API key is not valid UTF-8")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_a_secret() {
        let secret = encrypt_secret("correct horse", "decibel-api-key").unwrap();
        assert_eq!(
            decrypt_secret("correct horse", &secret).unwrap(),
            "decibel-api-key"
        );
    }

    #[test]
    fn wrong_password_is_rejected_rather_than_returning_garbage() {
        let secret = encrypt_secret("correct horse", "decibel-api-key").unwrap();
        assert!(decrypt_secret("wrong horse", &secret).is_err());
    }

    #[test]
    fn ciphertext_does_not_contain_the_plaintext() {
        let secret = encrypt_secret("pw", "super-secret-key").unwrap();
        assert!(!secret.ciphertext.contains("super-secret-key"));
    }

    #[test]
    fn empty_password_is_refused() {
        assert!(encrypt_secret("", "key").is_err());
    }

    #[test]
    fn each_encryption_uses_a_fresh_salt_and_nonce() {
        let first = encrypt_secret("pw", "key").unwrap();
        let second = encrypt_secret("pw", "key").unwrap();
        assert_ne!(first.salt, second.salt);
        assert_ne!(first.nonce, second.nonce);
    }
}
