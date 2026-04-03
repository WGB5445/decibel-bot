use anyhow::{Context, Result};
use clap::Parser;

// ── Network profiles ──────────────────────────────────────────────────────────

/// Per-network URL and address presets.
pub struct NetworkProfile {
    pub rest_api_base:   &'static str,
    pub fullnode_url:    &'static str,
    pub package_address: &'static str,
}

/// Return the profile for a given network name.
///
/// Valid names: `"testnet"`, `"mainnet"`.
pub fn profile_for(name: &str) -> Result<NetworkProfile> {
    match name {
        "testnet" => Ok(NetworkProfile {
            rest_api_base:   "https://api.testnet.aptoslabs.com/decibel/api/v1",
            fullnode_url:    "https://api.testnet.aptoslabs.com/v1",
            package_address: "0xe7da2794b1d8af76532ed95f38bfdf1136abfd8ea3a240189971988a83101b7f",
        }),
        "mainnet" => Ok(NetworkProfile {
            rest_api_base:   "https://api.mainnet.aptoslabs.com/decibel/api/v1",
            fullnode_url:    "https://api.mainnet.aptoslabs.com/v1",
            package_address: "0x2a4e9bee4b09f5b8e9c996a489c6993abe1e9e45e61e81bb493e38e53a3e7e3d",
        }),
        other => anyhow::bail!(
            "Unknown network {:?}; valid values: testnet, mainnet",
            other
        ),
    }
}

// ── CLI / env config ──────────────────────────────────────────────────────────

#[derive(Parser, Debug, Clone)]
#[command(
    name = "decibel-mm",
    about = "Decibel DEX Perpetual Futures Market Maker Bot",
    long_about = None
)]
pub struct Args {
    // ── Network ──────────────────────────────────────────────────────────────

    /// Network preset.  Sets default URLs and package address.
    /// Valid values: testnet | mainnet
    #[arg(long, env = "NETWORK", default_value = "testnet")]
    pub network: String,

    // ── Trading parameters ────────────────────────────────────────────────────

    /// Market symbol to trade (e.g. "BTC/USD" or "BTC-USD")
    #[arg(long, env = "MARKET_NAME", default_value = "BTC/USD")]
    pub market_name: String,

    /// Total spread fraction (0.001 = 0.1 %)
    #[arg(long, env = "SPREAD", default_value_t = 0.001)]
    pub spread: f64,

    /// Base-asset units per side per quote
    #[arg(long, env = "ORDER_SIZE", default_value_t = 0.001)]
    pub order_size: f64,

    /// Stop quoting when abs(position) >= this
    #[arg(long, env = "MAX_INVENTORY", default_value_t = 0.005)]
    pub max_inventory: f64,

    /// Extra half-spread per 1.0 unit of inventory (inventory skew coefficient)
    #[arg(long, env = "SKEW_PER_UNIT", default_value_t = 0.0001)]
    pub skew_per_unit: f64,

    /// Pause quoting when cross_margin_ratio exceeds this threshold (0–1)
    #[arg(long, env = "MAX_MARGIN_USAGE", default_value_t = 0.5)]
    pub max_margin_usage: f64,

    /// Seconds to sleep between cycles
    #[arg(long, env = "REFRESH_INTERVAL", default_value_t = 20.0)]
    pub refresh_interval: f64,

    /// Seconds to sleep between placing the bid and the ask
    #[arg(long, env = "COOLDOWN_S", default_value_t = 1.5)]
    pub cooldown_s: f64,

    /// Seconds to wait before re-checking open orders after a failed cancel
    #[arg(long, env = "CANCEL_RESYNC_S", default_value_t = 8.0)]
    pub cancel_resync_s: f64,

    /// Automatically place a reduce-only GTC order when inventory hits the limit
    #[arg(long, env = "AUTO_FLATTEN", default_value_t = false)]
    pub auto_flatten: bool,

    /// Price offset from mid for the flatten order (fraction)
    #[arg(long, env = "FLATTEN_AGGRESSION", default_value_t = 0.001)]
    pub flatten_aggression: f64,

    /// Log all actions without sending any on-chain transactions
    #[arg(long, env = "DRY_RUN", default_value_t = false)]
    pub dry_run: bool,

    // ── Credentials / addresses ───────────────────────────────────────────────

    /// REST API bearer token
    #[arg(long, env = "BEARER_TOKEN")]
    pub bearer_token: String,

    /// Subaccount object address to trade on (0x…)
    #[arg(long, env = "SUBACCOUNT_ADDRESS")]
    pub subaccount_address: String,

    /// 32-byte Ed25519 private key, hex-encoded (with or without 0x prefix)
    #[arg(long, env = "PRIVATE_KEY")]
    pub private_key: String,

    /// Perp engine global object address (stored for reference / future use)
    #[arg(long, env = "PERP_ENGINE_GLOBAL_ADDRESS")]
    pub perp_engine_global_address: String,

    /// Move package address — overrides the network profile default
    #[arg(long, env = "PACKAGE_ADDRESS")]
    pub package_address: Option<String>,

    /// Fullnode bearer token (falls back to BEARER_TOKEN when absent)
    #[arg(long, env = "NODE_API_KEY")]
    pub node_api_key: Option<String>,

    /// Aptos-compatible fullnode URL — overrides the network profile default
    #[arg(long, env = "APTOS_FULLNODE_URL")]
    pub aptos_fullnode_url: Option<String>,

    /// Override the PerpMarket object address (skips API discovery)
    #[arg(long, env = "MARKET_ADDR")]
    pub market_addr_override: Option<String>,

    /// Decibel REST API base URL — overrides the network profile default
    #[arg(long, env = "REST_API_BASE")]
    pub rest_api_base: Option<String>,
}

impl Args {
    /// Resolve the three network-dependent URLs/addresses.
    ///
    /// Priority (highest → lowest):
    ///   CLI flag / env var  >  network profile  >  (no implicit fallback)
    ///
    /// Returns `(rest_api_base, fullnode_url, package_address)`.
    pub fn effective_urls(&self) -> Result<(String, String, String)> {
        let p = profile_for(&self.network)?;
        Ok((
            self.rest_api_base
                .clone()
                .unwrap_or_else(|| p.rest_api_base.to_string()),
            self.aptos_fullnode_url
                .clone()
                .unwrap_or_else(|| p.fullnode_url.to_string()),
            self.package_address
                .clone()
                .unwrap_or_else(|| p.package_address.to_string()),
        ))
    }

    /// Decode and validate the 32-byte Ed25519 private key.
    ///
    /// Accepts 32-byte (seed only) or 64-byte (seed || public key) hex inputs;
    /// always uses the first 32 bytes as the seed.
    pub fn parse_private_key(&self) -> Result<[u8; 32]> {
        let hex_str = self.private_key.trim_start_matches("0x");
        let bytes = hex::decode(hex_str).context("PRIVATE_KEY is not valid hex")?;
        if bytes.len() < 32 {
            anyhow::bail!(
                "PRIVATE_KEY must encode at least 32 bytes, got {}",
                bytes.len()
            );
        }
        let mut key = [0u8; 32];
        key.copy_from_slice(&bytes[..32]);
        Ok(key)
    }

    /// Effective node API key: NODE_API_KEY if set, otherwise BEARER_TOKEN.
    pub fn node_key(&self) -> &str {
        self.node_api_key
            .as_deref()
            .unwrap_or(self.bearer_token.as_str())
    }

    /// Market name normalized for case-insensitive comparison (e.g. "BTC/USD" → "BTC-USD").
    pub fn normalized_market_name(&self) -> String {
        normalize_market(self.market_name.trim())
    }
}

/// Normalize a market name for fuzzy comparison: upper-case and "/" → "-".
pub fn normalize_market(name: &str) -> String {
    name.replace('/', "-").to_uppercase()
}
