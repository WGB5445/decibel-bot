use anyhow::Result;
use serde_json::{json, Value};
use std::time::Duration;
use tracing::{debug, error, info, warn};

use crate::{
    api::{ApiClient, MarketConfig, OpenOrder},
    aptos::AptosClient,
    config::Args,
    pricing::compute_quotes,
};

// ─────────────────────────────────────────────────────────────────────────────
// Entry point
// ─────────────────────────────────────────────────────────────────────────────

pub async fn run(args: Args) -> Result<()> {
    // Resolve network-dependent URLs (network profile < explicit env/flag override).
    let (rest_api, fullnode, package_address) = args.effective_urls()?;
    info!(
        network   = %args.network,
        rest_api  = %rest_api,
        fullnode  = %fullnode,
        "Network profile"
    );
    info!(
        market     = %args.market_name,
        spread     = args.spread,
        order_size = args.order_size,
        max_inv    = args.max_inventory,
        dry_run    = args.dry_run,
        "Starting Decibel Market Maker"
    );
    info!(perp_engine = %args.perp_engine_global_address, "Perp engine global");

    let private_key = args.parse_private_key()?;

    let api   = ApiClient::new(&rest_api, &args.bearer_token);
    let aptos = AptosClient::new(&fullnode, args.node_key(), private_key);

    info!(sender    = %aptos.sender_address(), "Derived sender address");
    info!(subaccount = %args.subaccount_address, "Subaccount");

    // ── Market discovery ──────────────────────────────────────────────────────

    let market_cfg: MarketConfig = match &args.market_addr_override {
        Some(override_addr) => {
            // Try to fetch metadata but fall back to sensible defaults
            match api.find_market(&args.market_name).await {
                Ok(mut cfg) => {
                    cfg.market_addr = override_addr.clone();
                    cfg
                }
                Err(_) => {
                    warn!("Could not fetch market metadata; using override address with default params");
                    MarketConfig {
                        market_addr:  override_addr.clone(),
                        market_name:  args.market_name.clone(),
                        tick_size:    1.0,
                        lot_size:     0.00001,
                        min_size:     0.00002,
                        px_decimals:  2,
                        sz_decimals:  5,
                    }
                }
            }
        }
        None => api.find_market(&args.market_name).await?,
    };

    info!(
        market_addr  = %market_cfg.market_addr,
        tick_size    = market_cfg.tick_size,
        lot_size     = market_cfg.lot_size,
        min_size     = market_cfg.min_size,
        px_decimals  = market_cfg.px_decimals,
        sz_decimals  = market_cfg.sz_decimals,
        "Market config loaded"
    );

    let initial_spread = args.spread;
    let mut bot = MarketMaker {
        args,
        api,
        aptos,
        market_cfg,
        package_address,
        effective_spread: initial_spread,
        last_inventory: 0.0,
        no_fill_cycles: 0,
        first_cycle: true,
    };

    bot.run_loop().await
}

// ─────────────────────────────────────────────────────────────────────────────
// MarketMaker
// ─────────────────────────────────────────────────────────────────────────────

struct MarketMaker {
    args: Args,
    api: ApiClient,
    aptos: AptosClient,
    market_cfg: MarketConfig,
    /// Resolved Move package address (from network profile or explicit override).
    package_address: String,
    // ── Adaptive spread state ─────────────────────────────────────────────────
    /// Current effective spread (may differ from args.spread when auto_spread is on).
    effective_spread: f64,
    /// Inventory observed at end of previous cycle (for fill detection).
    last_inventory: f64,
    /// Consecutive cycles without a detected fill.
    no_fill_cycles: u32,
    /// Skip fill detection on the very first cycle.
    first_cycle: bool,
}

impl MarketMaker {
    /// Main infinite loop.  Errors inside a cycle are logged but do not exit.
    async fn run_loop(&mut self) -> Result<()> {
        let mut cycle: u64 = 1;
        loop {
            info!(cycle, "─── Cycle start ───────────────────────────────────");
            if let Err(e) = self.run_cycle().await {
                error!(cycle, error = %e, "Cycle failed");
            }
            info!(cycle, sleep_s = self.args.refresh_interval, "Sleeping");
            tokio::time::sleep(Duration::from_secs_f64(self.args.refresh_interval)).await;
            cycle += 1;
        }
    }

    /// Single cycle: fetch → guard → quote → cancel → place.
    async fn run_cycle(&mut self) -> Result<()> {
        // ── 1. Parallel state fetch ───────────────────────────────────────────
        let state = self
            .api
            .fetch_state(&self.args.subaccount_address, &self.market_cfg.market_addr)
            .await?;

        info!(
            equity       = state.equity,
            margin_usage = state.margin_usage,
            inventory    = state.inventory,
            mid          = ?state.mid,
            open_orders  = state.open_orders.len(),
            "State snapshot"
        );

        // ── 2. Adaptive spread: fill detection ───────────────────────────────
        if !self.first_cycle {
            let inv_change = (state.inventory - self.last_inventory).abs();
            let fill_detected = inv_change > self.market_cfg.lot_size * 0.5;
            if fill_detected {
                info!(
                    inv_change,
                    effective_spread = self.effective_spread,
                    "Fill detected — spread is working"
                );
                self.no_fill_cycles = 0;
            } else {
                self.no_fill_cycles += 1;
                let suggested = (self.effective_spread - self.args.spread_step)
                    .max(self.args.spread_min);
                if self.no_fill_cycles >= self.args.spread_no_fill_cycles
                    && suggested < self.effective_spread
                {
                    if self.args.auto_spread {
                        self.effective_spread = suggested;
                        self.no_fill_cycles = 0;
                        warn!(
                            spread = self.effective_spread,
                            "Auto-spread: narrowed (no fill for {} cycles)",
                            self.args.spread_no_fill_cycles
                        );
                    } else {
                        warn!(
                            current_spread  = self.effective_spread,
                            suggested_spread = suggested,
                            no_fill_cycles  = self.no_fill_cycles,
                            "Suggestion: spread may be too wide — consider narrowing \
                             (add --auto-spread to automate)"
                        );
                    }
                }
            }
        }
        self.last_inventory = state.inventory;
        self.first_cycle = false;

        // ── 3. Risk guard: margin ─────────────────────────────────────────────
        if state.margin_usage > self.args.max_margin_usage {
            warn!(
                margin_usage = state.margin_usage,
                threshold    = self.args.max_margin_usage,
                "PAUSED — margin usage too high"
            );
            return Ok(());
        }

        // ── 3. Risk guard: no price ───────────────────────────────────────────
        let mid = match state.mid {
            Some(p) => p,
            None => {
                warn!("PAUSED — mid price unavailable");
                return Ok(());
            }
        };

        // ── 4. Compute quotes ─────────────────────────────────────────────────
        let quotes = match compute_quotes(
            mid,
            state.inventory,
            self.effective_spread,
            self.args.skew_per_unit,
            self.args.max_inventory,
            self.market_cfg.tick_size,
            self.market_cfg.lot_size,
            self.market_cfg.min_size,
            self.args.order_size,
        ) {
            Ok(Some(q)) => q,
            Ok(None) => {
                // Inventory at limit
                info!(
                    inventory    = state.inventory,
                    max_inventory = self.args.max_inventory,
                    "Inventory limit — cancelling and optionally flattening"
                );
                self.cancel_all_orders(&state.open_orders).await?;
                if self.args.auto_flatten {
                    self.place_flatten_order(state.inventory, mid).await?;
                } else {
                    warn!(
                        "Position at max_inventory; manually flatten or enable --auto-flatten"
                    );
                }
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        };

        info!(
            bid  = quotes.bid,
            ask  = quotes.ask,
            size = quotes.size,
            "Computed quotes"
        );

        // ── 5. Cancel all resting orders ──────────────────────────────────────
        let (n_ok, n_fail) = self.cancel_all_orders(&state.open_orders).await?;
        if n_ok > 0 || n_fail > 0 {
            info!(cancelled = n_ok, failed = n_fail, "Cancel results");
        }

        if n_fail > 0 {
            warn!(
                cancel_resync_s = self.args.cancel_resync_s,
                "Failed cancels — waiting for chain resync"
            );
            tokio::time::sleep(Duration::from_secs_f64(self.args.cancel_resync_s)).await;

            let fresh = self
                .api
                .fetch_open_orders(&self.args.subaccount_address)
                .await?;
            let still_open: Vec<_> = fresh
                .into_iter()
                .filter(|o| {
                    crate::api::addr_eq_pub(&o.market_addr, &self.market_cfg.market_addr)
                })
                .collect();
            if !still_open.is_empty() {
                warn!(
                    count = still_open.len(),
                    "Still have resting orders after resync — skipping cycle"
                );
                return Ok(());
            }
        }

        // ── 6. Place bid ──────────────────────────────────────────────────────
        self.place_post_only_order(quotes.bid, quotes.size, true).await?;

        // ── 7. Cooldown ───────────────────────────────────────────────────────
        tokio::time::sleep(Duration::from_secs_f64(self.args.cooldown_s)).await;

        // ── 8. Place ask ──────────────────────────────────────────────────────
        self.place_post_only_order(quotes.ask, quotes.size, false).await?;

        info!("Cycle complete");
        Ok(())
    }

    // ── Order management ──────────────────────────────────────────────────────

    /// Cancel every order in `orders`.  Returns `(succeeded, genuinely_failed)`.
    async fn cancel_all_orders(&self, orders: &[OpenOrder]) -> Result<(usize, usize)> {
        if orders.is_empty() {
            return Ok((0, 0));
        }
        info!(count = orders.len(), "Cancelling all resting orders");

        let mut n_ok = 0usize;
        let mut n_fail = 0usize;

        for order in orders {
            match self.cancel_single_order(&order.order_id).await {
                Ok(_) => n_ok += 1,
                Err(e) => {
                    warn!(order_id = %order.order_id, error = %e, "Cancel failed");
                    n_fail += 1;
                }
            }
        }
        Ok((n_ok, n_fail))
    }

    async fn cancel_single_order(&self, order_id: &str) -> Result<()> {
        let function = format!(
            "{}::dex_accounts_entry::cancel_order_to_subaccount",
            self.package_address
        );

        info!(order_id, dry_run = self.args.dry_run, "Cancelling order");

        if self.args.dry_run {
            return Ok(());
        }

        let result = self
            .aptos
            .submit_entry_function(
                &function,
                vec![],
                vec![
                    // 1. subaccount_addr  (Object<Subaccount>)
                    Value::String(self.args.subaccount_address.clone()),
                    // 2. order_id         (u128 as STRING)
                    Value::String(order_id.to_string()),
                    // 3. market_addr      (Object<PerpMarket>)
                    Value::String(self.market_cfg.market_addr.clone()),
                ],
            )
            .await?;

        if result.cancel_succeeded() {
            debug!(order_id, vm_status = %result.vm_status, "Cancel accepted");
            Ok(())
        } else {
            anyhow::bail!(
                "Cancel rejected for order {order_id}: vm_status={}",
                result.vm_status
            )
        }
    }

    /// Place a POST_ONLY limit order (time_in_force = 1).
    async fn place_post_only_order(&self, price: f64, size: f64, is_buy: bool) -> Result<()> {
        let side = if is_buy { "BID" } else { "ASK" };
        let price_int = self.scale_price(price);
        let size_int  = self.scale_size(size);

        info!(
            side, price, size, price_int, size_int,
            dry_run = self.args.dry_run,
            "Placing POST_ONLY order"
        );

        if self.args.dry_run {
            return Ok(());
        }

        let result = self
            .aptos
            .submit_entry_function(
                &format!(
                    "{}::dex_accounts_entry::place_order_to_subaccount",
                    self.package_address
                ),
                vec![],
                build_place_order_args(
                    &self.args.subaccount_address,
                    &self.market_cfg.market_addr,
                    price_int,
                    size_int,
                    is_buy,
                    1,     // time_in_force = POST_ONLY
                    false, // is_reduce_only
                ),
            )
            .await?;

        if result.success {
            info!(side, price, size, tx_hash = %result.hash, "Order placed");
            Ok(())
        } else {
            anyhow::bail!(
                "Place order failed: side={side} price={price} size={size} \
                 vm_status={}",
                result.vm_status
            )
        }
    }

    /// Place a reduce-only GTC order to flatten an oversized position.
    ///
    /// - Long  (inventory > 0): sell at `mid × (1 − flatten_aggression)`, rounded DOWN.
    /// - Short (inventory < 0): buy  at `mid × (1 + flatten_aggression)`, rounded UP.
    async fn place_flatten_order(&self, inventory: f64, mid: f64) -> Result<()> {
        let is_buy = inventory < 0.0; // buy to close a short
        let abs_inv = inventory.abs();

        let raw_price = if is_buy {
            let p = mid * (1.0 + self.args.flatten_aggression);
            (p / self.market_cfg.tick_size).ceil() * self.market_cfg.tick_size
        } else {
            let p = mid * (1.0 - self.args.flatten_aggression);
            (p / self.market_cfg.tick_size).floor() * self.market_cfg.tick_size
        };

        let size = (abs_inv / self.market_cfg.lot_size).round() * self.market_cfg.lot_size;
        if size < self.market_cfg.min_size || size <= 0.0 {
            warn!(
                size, min_size = self.market_cfg.min_size,
                "Flatten order size too small — skipping"
            );
            return Ok(());
        }

        let price_int = self.scale_price(raw_price);
        let size_int  = self.scale_size(size);
        let side = if is_buy { "BUY" } else { "SELL" };

        info!(
            side, price = raw_price, size,
            dry_run = self.args.dry_run,
            "Placing reduce-only GTC flatten order"
        );

        if self.args.dry_run {
            return Ok(());
        }

        let result = self
            .aptos
            .submit_entry_function(
                &format!(
                    "{}::dex_accounts_entry::place_order_to_subaccount",
                    self.package_address
                ),
                vec![],
                build_place_order_args(
                    &self.args.subaccount_address,
                    &self.market_cfg.market_addr,
                    price_int,
                    size_int,
                    is_buy,
                    0,    // time_in_force = GTC
                    true, // is_reduce_only
                ),
            )
            .await?;

        if result.success {
            info!(side, price = raw_price, size, tx_hash = %result.hash, "Flatten order placed");
            Ok(())
        } else {
            anyhow::bail!(
                "Flatten order failed: vm_status={}",
                result.vm_status
            )
        }
    }

    // ── Scaling helpers ───────────────────────────────────────────────────────

    #[inline]
    fn scale_price(&self, price: f64) -> u64 {
        let scale = 10u64.pow(self.market_cfg.px_decimals);
        (price * scale as f64).round() as u64
    }

    #[inline]
    fn scale_size(&self, size: f64) -> u64 {
        let scale = 10u64.pow(self.market_cfg.sz_decimals);
        (size * scale as f64).round() as u64
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// ABI argument builder for place_order_to_subaccount
// ─────────────────────────────────────────────────────────────────────────────

/// Build the 15 ABI arguments for `dex_accounts_entry::place_order_to_subaccount`.
///
/// All Option<u64> fields use the Move `{"vec": []}` / `{"vec": ["value"]}` encoding.
fn build_place_order_args(
    subaccount_addr: &str,
    market_addr: &str,
    price_int: u64,
    size_int: u64,
    is_buy: bool,
    time_in_force: u8,  // 0=GTC, 1=POST_ONLY, 2=IOC
    is_reduce_only: bool,
) -> Vec<Value> {
    let none_u64:    Value = json!({"vec": []});
    let none_addr:   Value = json!({"vec": []});
    let no_client_id: Value = json!({"vec": []});

    vec![
        Value::String(subaccount_addr.to_string()),   //  1. subaccount_addr
        Value::String(market_addr.to_string()),        //  2. market_addr
        Value::String(price_int.to_string()),          //  3. price (u64 as string)
        Value::String(size_int.to_string()),           //  4. size  (u64 as string)
        Value::Bool(is_buy),                           //  5. is_buy
        json!(time_in_force),                          //  6. time_in_force (u8)
        Value::Bool(is_reduce_only),                   //  7. is_reduce_only
        no_client_id,                                  //  8. client_order_id  Option<String>
        none_u64.clone(),                              //  9. stop_price       Option<u64>
        none_u64.clone(),                              // 10. tp_trigger        Option<u64>
        none_u64.clone(),                              // 11. tp_limit           Option<u64>
        none_u64.clone(),                              // 12. sl_trigger         Option<u64>
        none_u64.clone(),                              // 13. sl_limit            Option<u64>
        none_addr,                                     // 14. builder_addr    Option<address>
        none_u64,                                      // 15. builder_fees       Option<u64>
    ]
}
