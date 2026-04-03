use anyhow::{Context, Result};
use reqwest::Client;
use serde::Deserialize;
use tracing::debug;

use crate::config::normalize_market;

// ─────────────────────────────────────────────────────────────────────────────
// Response types
// ─────────────────────────────────────────────────────────────────────────────

/// GET /account_overviews?account={addr}
#[derive(Debug, Deserialize)]
pub struct AccountOverview {
    /// Cross-margin utilization ratio in [0, 1].  NOT a percentage.
    pub cross_margin_ratio: f64,
    /// Perpetual equity balance in USD.
    pub perp_equity_balance: f64,
}

/// Single entry from GET /account_positions?account={addr}
#[derive(Debug, Deserialize)]
pub struct Position {
    #[serde(rename = "market")]
    pub market_addr: String,
    /// Net position size: positive = long, negative = short.
    pub size: f64,
}

/// Single entry from GET /open_orders?account={addr}
#[derive(Debug, Deserialize, Clone)]
pub struct OpenOrder {
    /// Exchange order ID encoded as a string (u128).
    pub order_id: String,
    #[serde(rename = "market")]
    pub market_addr: String,
}

/// Pagination wrapper for GET /open_orders
#[derive(Debug, Deserialize)]
struct OpenOrdersPage {
    items: Vec<OpenOrder>,
}

/// Single entry from GET /prices?market={market_addr}  (API returns an array)
#[derive(Debug, Deserialize)]
pub struct PriceInfo {
    pub market: String,
    pub mid_px: Option<f64>,
    pub mark_px: Option<f64>,
}

impl PriceInfo {
    /// Best available mid price: prefer mid_px, fall back to mark_px.
    pub fn mid(&self) -> Option<f64> {
        self.mid_px.or(self.mark_px)
    }
}

/// Single entry from GET /markets
#[derive(Debug, Deserialize, Clone)]
pub struct MarketConfig {
    /// PerpMarket object address on-chain.
    pub market_addr: String,
    /// Human-readable symbol, e.g. "BTC/USD".
    pub market_name: String,
    /// Minimum price increment (e.g. 1.0 for $1).
    pub tick_size: f64,
    /// Minimum size increment (e.g. 0.00001 BTC).
    pub lot_size: f64,
    /// Minimum order size (e.g. 0.00002 BTC).
    pub min_size: f64,
    /// Decimal places for price: price_int = price_float × 10^px_decimals.
    pub px_decimals: u32,
    /// Decimal places for size: size_int = size_float × 10^sz_decimals.
    pub sz_decimals: u32,
}

// ─────────────────────────────────────────────────────────────────────────────
// State snapshot
// ─────────────────────────────────────────────────────────────────────────────

/// Everything the bot needs to make decisions for one cycle.
#[derive(Debug)]
pub struct StateSnapshot {
    /// Cross-margin utilization in [0, 1].
    pub margin_usage: f64,
    /// Equity in USD (for display).
    pub equity: f64,
    /// Net position size for the target market.
    pub inventory: f64,
    /// Resting orders on the target market.
    pub open_orders: Vec<OpenOrder>,
    /// Best mid price (`None` if unavailable).
    pub mid: Option<f64>,
}

// ─────────────────────────────────────────────────────────────────────────────
// API client
// ─────────────────────────────────────────────────────────────────────────────

pub struct ApiClient {
    client: Client,
    base_url: String,
    bearer_token: String,
}

impl ApiClient {
    pub fn new(base_url: &str, bearer_token: &str) -> Self {
        Self {
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .expect("Failed to build reqwest client"),
            base_url: base_url.trim_end_matches('/').to_string(),
            bearer_token: bearer_token.to_string(),
        }
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    fn get(&self, path: &str) -> reqwest::RequestBuilder {
        let url = format!("{}{}", self.base_url, path);
        debug!(url = %url, "GET");
        self.client.get(url).bearer_auth(&self.bearer_token)
    }

    // ── Public endpoints ──────────────────────────────────────────────────────

    pub async fn fetch_overview(&self, subaccount: &str) -> Result<AccountOverview> {
        let resp = self
            .get(&format!("/account_overviews?account={subaccount}"))
            .send()
            .await
            .context("fetch_overview: send failed")?;
        resp.error_for_status_ref()
            .context("fetch_overview: HTTP error")?;
        resp.json::<AccountOverview>()
            .await
            .context("fetch_overview: JSON decode failed")
    }

    pub async fn fetch_positions(&self, subaccount: &str) -> Result<Vec<Position>> {
        let resp = self
            .get(&format!("/account_positions?account={subaccount}"))
            .send()
            .await
            .context("fetch_positions: send failed")?;
        resp.error_for_status_ref()
            .context("fetch_positions: HTTP error")?;
        resp.json::<Vec<Position>>()
            .await
            .context("fetch_positions: JSON decode failed")
    }

    pub async fn fetch_open_orders(&self, subaccount: &str) -> Result<Vec<OpenOrder>> {
        let resp = self
            .get(&format!("/open_orders?account={subaccount}"))
            .send()
            .await
            .context("fetch_open_orders: send failed")?;
        resp.error_for_status_ref()
            .context("fetch_open_orders: HTTP error")?;
        let page = resp
            .json::<OpenOrdersPage>()
            .await
            .context("fetch_open_orders: JSON decode failed")?;
        Ok(page.items)
    }

    pub async fn fetch_price(&self, market_addr: &str) -> Result<PriceInfo> {
        let resp = self
            .get(&format!("/prices?market={market_addr}"))
            .send()
            .await
            .context("fetch_price: send failed")?;
        resp.error_for_status_ref()
            .context("fetch_price: HTTP error")?;
        let list = resp
            .json::<Vec<PriceInfo>>()
            .await
            .context("fetch_price: JSON decode failed")?;
        list.into_iter()
            .find(|p| addr_eq(&p.market, market_addr))
            .ok_or_else(|| anyhow::anyhow!("fetch_price: no price entry for {market_addr}"))
    }

    pub async fn fetch_markets(&self) -> Result<Vec<MarketConfig>> {
        let resp = self
            .get("/markets")
            .send()
            .await
            .context("fetch_markets: send failed")?;
        resp.error_for_status_ref()
            .context("fetch_markets: HTTP error")?;
        let mut markets = resp
            .json::<Vec<MarketConfig>>()
            .await
            .context("fetch_markets: JSON decode failed")?;
        // API returns raw chain units; convert to human-readable floats.
        for m in &mut markets {
            let px_scale = 10f64.powi(m.px_decimals as i32);
            let sz_scale = 10f64.powi(m.sz_decimals as i32);
            m.tick_size /= px_scale;
            m.lot_size  /= sz_scale;
            m.min_size  /= sz_scale;
        }
        Ok(markets)
    }

    /// Fetch the market config for `market_name`, normalizing "/" ↔ "-".
    pub async fn find_market(&self, market_name: &str) -> Result<MarketConfig> {
        let markets = self.fetch_markets().await?;
        let target = normalize_market(market_name);
        markets
            .into_iter()
            .find(|m| normalize_market(&m.market_name) == target)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Market '{market_name}' not found in /markets response"
                )
            })
    }

    // ── Parallel state fetch ──────────────────────────────────────────────────

    /// Fetch account overview, positions, open orders, and price IN PARALLEL.
    ///
    /// Filters positions and orders to those matching `market_addr` (case-insensitive).
    pub async fn fetch_state(
        &self,
        subaccount: &str,
        market_addr: &str,
    ) -> Result<StateSnapshot> {
        let (overview, positions, all_orders, price_info) = tokio::try_join!(
            self.fetch_overview(subaccount),
            self.fetch_positions(subaccount),
            self.fetch_open_orders(subaccount),
            self.fetch_price(market_addr),
        )?;

        let inventory = positions
            .iter()
            .find(|p| addr_eq(&p.market_addr, market_addr))
            .map(|p| p.size)
            .unwrap_or(0.0);

        let open_orders = all_orders
            .into_iter()
            .filter(|o| addr_eq(&o.market_addr, market_addr))
            .collect();

        Ok(StateSnapshot {
            margin_usage: overview.cross_margin_ratio,
            equity: overview.perp_equity_balance,
            inventory,
            open_orders,
            mid: price_info.mid(),
        })
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Case-insensitive address comparison (handles 0x-prefix and leading-zero differences).
pub fn addr_eq_pub(a: &str, b: &str) -> bool {
    normalize_addr(a) == normalize_addr(b)
}

fn addr_eq(a: &str, b: &str) -> bool {
    addr_eq_pub(a, b)
}

fn normalize_addr(addr: &str) -> String {
    addr.trim_start_matches("0x")
        .to_lowercase()
        .trim_start_matches('0')
        .to_string()
}
