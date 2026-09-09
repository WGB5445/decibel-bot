use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result};
use chrono::Utc;
use decibel_grid_tui::*;
use rust_decimal::Decimal;

use crate::cli::settings::Settings;
use crate::cli::settings::decimal;

// The engine predates the structured logger and has many operational messages. Keep legacy call
// sites on one parseable sink while migration adds typed fields to high-value state transitions.
macro_rules! println {
    ($($arg:tt)*) => {
        tracing::info!(target: "engine", "{}", format_args!($($arg)*))
    };
}

macro_rules! eprintln {
    ($($arg:tt)*) => {
        tracing::warn!(target: "engine", "{}", format_args!($($arg)*))
    };
}

fn perp_pnl_status(
    accounting: &decibel_grid_tui::strategy::perp::accounting::PerpAccounting,
    exchange_position: Decimal,
    mark_price: Option<Decimal>,
) -> control::PerpPnlStatus {
    let snapshot = accounting.pnl_snapshot(exchange_position, mark_price);
    control::PerpPnlStatus {
        exchange_position_base: snapshot.exchange_position_base.to_string(),
        ledger_position_base: snapshot.ledger_position_base.to_string(),
        reconciliation_delta_base: snapshot.reconciliation_delta_base.to_string(),
        average_entry_price: snapshot.average_entry_price.map(|value| value.to_string()),
        mark_price: snapshot.mark_price.map(|value| value.to_string()),
        unrealized_gross_quote: snapshot
            .unrealized_gross_quote
            .map(|value| value.to_string()),
        realized_gross_quote: snapshot.realized_gross_quote.to_string(),
        trade_fees_quote: snapshot.trade_fees_quote.map(|value| value.to_string()),
        funding_pnl_quote: snapshot.funding_pnl_quote.map(|value| value.to_string()),
        net_pnl_quote: snapshot.net_pnl_quote.map(|value| value.to_string()),
        fees_complete: accounting.fees_complete,
        funding_complete: accounting.funding_complete,
        last_fill_at: accounting.last_fill_at,
        last_funding_at: accounting.last_funding_at,
    }
}

pub(crate) fn print_snapshot(snapshot: &MonitorSnapshot, config: &GridConfig) {
    let profit = snapshot.plan.profit_preview(config.maker_fee_rate);
    tracing::debug!(
        target: "engine::plan",
        market = %snapshot.market.name,
        product = ?snapshot.market.product,
        mid = %snapshot.plan.mid,
        net_scenario = %profit.net_capture,
        bid_levels = snapshot.plan.bids.len(),
        ask_levels = snapshot.plan.asks.len(),
        "plan snapshot"
    );
    for level in snapshot.plan.all_levels() {
        tracing::debug!(
            target: "engine::plan",
            side = level.side.as_str(),
            price = %format_decimal(level.price, 8),
            size = %format_decimal(level.size, 8),
            level_state = ?level.state,
            "planned level"
        );
    }
}

pub(crate) fn optional_subaccount(settings: &Settings) -> Option<&str> {
    (!settings.subaccount.trim().is_empty()).then_some(settings.subaccount.as_str())
}

/// Compute the current execution phase from WS state and journal lifecycle. This is the single
/// authority for whether the bot may compute plans, reconcile, and submit orders. Every execution
/// path consults this before acting; non-Ready phases still run status updates for visibility.
/// Journal a full cancel lifecycle: intent → broadcast → observed. Returns the cancel outcome.
/// If there is no tracked bulk ladder, skips the cancel entirely.
async fn journaled_bulk_cancel(
    journal: Option<&decibel_grid_tui::journal::Journal>,
    run_state: &mut decibel_grid_tui::journal::RunState,
    network: &str,
    private_key: &str,
    subaccount: &str,
    market: &decibel_grid_tui::Market,
    gas_station: Option<&decibel_grid_tui::GasStationConfig>,
) -> Result<Option<String>> {
    let operation_id = run_state
        .bulk_ladder
        .as_ref()
        .map(|ladder| ladder.operation_id.clone());
    let Some(op_id) = operation_id else {
        return Ok(None);
    };
    let journal = match journal {
        Some(j) => j,
        None => return Ok(None),
    };
    let cancel_intent = decibel_grid_tui::journal::JournalEvent::BulkCancelIntentRecorded {
        at: Utc::now(),
        operation_id: op_id.clone(),
    };
    journal.append(&cancel_intent)?;
    run_state.apply(&cancel_intent);
    journal.save_state(&run_state)?;

    let cancel_op_id = op_id.clone();
    let hash = decibel_grid_tui::spot_lifecycle::cancel_bulk_ladder_with_broadcast(
        network,
        private_key,
        subaccount,
        market,
        gas_station,
        |tx_hash| {
            let broadcast = decibel_grid_tui::journal::JournalEvent::BulkCancelBroadcast {
                at: Utc::now(),
                operation_id: cancel_op_id.clone(),
                transaction_hash: tx_hash.to_owned(),
            };
            journal.append(&broadcast)?;
            run_state.apply(&broadcast);
            journal.save_state(&run_state)
        },
    )
    .await?;

    let observed = decibel_grid_tui::journal::JournalEvent::BulkCancelledObserved {
        at: Utc::now(),
        operation_id: op_id,
    };
    journal.append(&observed)?;
    run_state.apply(&observed);
    journal.save_state(&run_state)?;
    Ok(Some(hash))
}

fn engine_phase(
    ws_state: &decibel_grid_tui::ws_state::WsStateHandle,
    run_state: &decibel_grid_tui::journal::RunState,
) -> control::EnginePhase {
    if !decibel_grid_tui::ws_state::can_execute(ws_state) {
        let state = ws_state.read().expect("WS state lock poisoned");
        if !state.subscriptions_ready {
            return control::EnginePhase::Connecting;
        }
        if state.orders_desynced || state.bulk_ladder_desynced {
            return control::EnginePhase::Desynced;
        }
        return control::EnginePhase::Hydrating;
    }
    // Market data freshness is checked inside fetch_snapshot_ws_first. This phase gate focuses on
    // the WS state that determines whether a plan can be computed safely.
    if run_state.bulk_ladder.as_ref().is_some_and(|ladder| {
        matches!(
            ladder.state,
            decibel_grid_tui::journal::BulkLadderState::IntentRecorded
                | decibel_grid_tui::journal::BulkLadderState::BroadcastPending
                | decibel_grid_tui::journal::BulkLadderState::BroadcastUnknown
                | decibel_grid_tui::journal::BulkLadderState::Committed
                | decibel_grid_tui::journal::BulkLadderState::Diverged
                | decibel_grid_tui::journal::BulkLadderState::CancelPending
        )
    }) {
        return control::EnginePhase::LifecycleBlocked;
    }
    control::EnginePhase::Ready
}

/// Run the grid from a non-interactive terminal.
///
/// Modes:
/// - default: dry-run monitor (fetch + print, no exchange mutations)
/// - `-e` / `--execute`: reconciliation-based live execution. Each cycle:
///   1. Fetch snapshot + open orders
///   2. Reconcile desired vs actual
///   3. If any open orders exist (no client-order ID), halt new submissions
///   4. If market is empty and desired levels exist, submit the **full** desired plan
///   5. Persist every step to an append-only event journal
pub async fn run_cli(
    settings: Settings,
    execute: bool,
    confirm_mainnet: Option<&str>,
    engine_runtime: Option<control::EngineHandle>,
    notifier: Option<decibel_grid_tui::notify::Notifier>,
) -> Result<()> {
    if execute
        && (settings.api_key.trim().is_empty()
            || settings.aptos_private_key.trim().is_empty()
            || settings.subaccount.trim().is_empty())
    {
        anyhow::bail!("-e requires DECIBEL_API_KEY, APTOS_PRIVATE_KEY, and SUBACCOUNT_ADDRESS")
    }
    if execute
        && settings.network.eq_ignore_ascii_case("mainnet")
        && confirm_mainnet != Some("MAINNET")
    {
        anyhow::bail!(
            "Mainnet execution requires --confirm-mainnet MAINNET (or CONFIRM_MAINNET=MAINNET)"
        )
    }
    let mut config = settings.to_grid_config()?;
    let api = settings.api_client()?;
    let gas_station_config = settings.gas_station_config()?;
    let gas_station = gas_station_config.as_ref();
    if execute {
        match gas_station {
            None => println!("gas station: off"),
            Some(_) => println!("gas station: geomi {}", settings.network),
        }
    }
    let spot_fee_rates = if execute && config.product == Product::Spot {
        let rates = api
            .spot_fee_rates(&settings.subaccount)
            .await
            .context("fetch required live Spot fee rates")?;
        config.maker_fee_rate = rates.maker_rate;
        println!(
            "Spot fee schedule: maker={} taker={}",
            rates.maker_rate, rates.taker_rate
        );
        Some(rates)
    } else {
        None
    };
    let run_id = if execute {
        journal::persistent_run_id(&settings.network, &settings.subaccount, &config.market_name)
    } else {
        journal::generate_run_id()
    };
    let journal = if execute {
        Some(
            journal::Journal::new(&run_id)
                .context("live execution requires a writable run journal")?,
        )
    } else {
        journal::Journal::new(&run_id).ok()
    };
    let config_hash = {
        use sha3::{Digest, Sha3_256};
        hex::encode(Sha3_256::digest(format!("{config:?}")))
    };
    let mut metadata = journal::RunMetadata {
        run_id: run_id.clone(),
        started_at: Utc::now(),
        network: settings.network.clone(),
        subaccount: settings.subaccount.clone(),
        market: config.market_name.clone(),
        product: format!("{:?}", config.product).to_lowercase(),
        config_hash,
        program_version: env!("CARGO_PKG_VERSION").to_owned(),
    };
    metadata.fingerprint_subaccount();
    let mut resumed = false;
    let mut run_state = if let Some(journal) = &journal {
        match journal.load_state()? {
            Some(previous)
                if previous
                    .metadata
                    .network
                    .eq_ignore_ascii_case(&metadata.network)
                    && previous.metadata.subaccount == metadata.subaccount
                    && previous
                        .metadata
                        .market
                        .eq_ignore_ascii_case(&metadata.market)
                    && previous.metadata.product == metadata.product
                    && previous.metadata.config_hash == metadata.config_hash =>
            {
                resumed = true;
                previous
            }
            _ => journal::RunState::new(metadata.clone()),
        }
    } else {
        journal::RunState::new(metadata.clone())
    };
    if let Some(journal) = &journal {
        journal.append(&journal::JournalEvent::RunStart(metadata))?;
        journal.save_state(&run_state)?;
    }
    if resumed {
        println!(
            "Recovered durable grid state for run {run_id}; reconciling exchange state before replacement."
        );
    }
    println!(
        "{}",
        if execute {
            format!(
                "Live grid execution, run {run_id}. Full ladder replacement is guarded by reconciliation."
            )
        } else {
            format!("Grid monitor, run {run_id}. Pass -e to submit bulk orders.")
        }
    );
    if config.product == Product::Spot {
        println!("Spot: only PFS balances will be used. No automatic Cross→PFS transfer.");
    }

    // Spot base inventory is acquired at most ONCE per process. Acquiring the inventory a
    // two-sided grid needs is a capital-allocation decision; topping it up again after a sell
    // fill is not. The grid's profit comes from selling high and letting the *bid* side buy back
    // lower, so re-buying at the ask after every fill would return the captured spread, plus
    // taker fees, to the market on every round trip.
    // Parse the optional Spot stop-loss once: a malformed value must fail at startup, not on the
    // cycle where the market happens to reach it.
    let spot_exit_price = match settings.spot_exit_price.as_deref().map(str::trim) {
        Some(raw) if !raw.is_empty() => {
            Some(decimal(raw).with_context(|| format!("invalid SPOT_EXIT_PRICE {raw:?}"))?)
        }
        _ => None,
    };
    if let Some(stop) = spot_exit_price {
        println!("Spot stop-loss armed: liquidate and stop when price <= {stop}.");
    }
    // Set once the stop-loss has liquidated, so the shutdown exit policy does not sell again.
    let mut stop_loss_liquidated = false;
    // A range breaker cancels the ladder but must never flow into the configured stop-time asset
    // disposition; an abnormal market state requires an explicit later operator decision.
    let mut paused_by_breakout = false;
    // A submission-failure circuit breaker also cancels and pauses without liquidating.
    let mut paused_by_failure_circuit = false;
    // Local PFS preflight rejections and submitted-chain failures are intentionally independent.
    let mut consecutive_local_preflight_rejections = 0usize;
    let mut consecutive_bulk_failures = 0usize;
    // Bootstrap eligibility is always derived from the newest PFS snapshot. Never cache a
    // historical "funded" success: fills, withdrawals, and a changed pinned plan can all make
    // an old success record unsafe.
    let mut consecutive_spot_funding_failures: usize = 0;
    let mut paused_by_spot_funding_circuit = false;
    // Spot grid geometry is initialized once per process and then pinned. A moving mid may
    // detect fills and trigger replacement, but it must never move the configured boundaries or
    // regenerate prices; otherwise the strategy becomes a moving target instead of a grid.
    let mut pinned_spot_plan: Option<GridPlan> = run_state
        .spot_runtime
        .as_ref()
        .map(|state| state.pinned_plan.clone());
    // Retain a short Spot-only cooldown for transient reconciliation drift. Perp geometry is
    // pinned below, so a mid-price refresh cannot create price-only ladder drift.
    const BULK_REPLACEMENT_COOLDOWN: Duration = Duration::from_secs(30);
    // A divergent bulk submission is exceptional. Retry the targeted REST recovery, but never
    // turn a temporarily lagging indexer into one HTTP request (or one log entry) per cycle.
    const DIVERGED_BULK_RECOVERY_INTERVAL: Duration = Duration::from_secs(30);
    let mut last_bulk_replacement_at: Option<tokio::time::Instant> = None;
    let mut last_diverged_bulk_recovery_at: Option<tokio::time::Instant> = None;
    let mut last_bulk_lifecycle_blocked_operation: Option<String> = None;
    // Total resting levels (bid_count + ask_count) from the last submitted bulk ladder. A
    // changed level count means inventory/affordability moved (a fill, funding, or a PFS-driven
    // shrink), which must replace immediately regardless of the cooldown.
    let mut last_submitted_level_count: Option<usize> = None;
    let mut out_of_range_handled = false;
    let existing_perp_runtime = run_state.perp_runtime.clone();
    let mut perp_runtime = match existing_perp_runtime {
        Some(state) => state,
        None if resumed && config.product == Product::Perp => {
            journal::PerpRuntimeState::legacy_unknown()
        }
        None => journal::PerpRuntimeState::new(),
    };
    let mut legacy_perp_state = config.product == Product::Perp
        && perp_runtime.bootstrap_status == journal::PerpBootstrapStatus::LegacyUnknown;
    let mut perp_accounting = perp_runtime.accounting.clone();
    // A new strategy is allowed to establish a ledger only while flat. A resumed strategy first
    // applies unseen fills, then must agree with the exchange before it can submit more risk.
    let mut perp_accounting_initialized =
        resumed && config.product == Product::Perp && !legacy_perp_state;
    let mut perp_accounting_blocked: Option<String> = None;
    let mut perp_risk_pause_reason: Option<String> = None;
    let mut last_perp_preflight_deferral: Option<String> = None;
    let mut last_perp_submission_block_reason: Option<String> = None;
    // Trade history is the reliable fill signal. Bulk synthetic order IDs change on every
    // replacement because the sequence number changes, so comparing those IDs would falsely
    // classify every replacement as a fill.
    let mut last_seen_trade_ms: Option<i64> = run_state
        .spot_runtime
        .as_ref()
        .and_then(|state| state.last_seen_trade_ms);
    // Retained so the shutdown path can act on the market without re-fetching after Ctrl+C.
    let mut last_market: Option<Market> = None;
    // A shared cancellation token so every long `.await` in the loop body can bail promptly
    // on Ctrl+C rather than only checking between sleep-drain cycles.
    let cancel = engine_runtime
        .as_ref()
        .map(|runtime| runtime.cancel())
        .unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
    {
        let cancel = Arc::clone(&cancel);
        tokio::spawn(async move {
            #[cfg(unix)]
            {
                use tokio::signal::unix::{SignalKind, signal};
                let mut terminate =
                    signal(SignalKind::terminate()).expect("install SIGTERM handler");
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = terminate.recv() => {}
                }
            }
            #[cfg(not(unix))]
            tokio::signal::ctrl_c().await.ok();
            cancel.store(true, Ordering::Relaxed);
            println!("\nShutdown requested.");
        });
    }
    let (ws_lifecycle_tx, mut ws_lifecycle_rx) = ws_state::lifecycle_channel();
    let ws_state = ws_state::new_handle();
    let _ws_session = if !settings.subaccount.trim().is_empty() {
        let market = api
            .market(&config.market_name, config.product)
            .await
            .context("resolve market before subscribing to WebSocket state")?;
        Some(ws_state::spawn_ws_session(
            api.clone(),
            ws_state::WsSessionConfig {
                product: config.product,
                market_address: market.address,
                subaccount: settings.subaccount.clone(),
                depth_aggregation: ws_state::DEFAULT_DEPTH_AGGREGATION,
                reconnect_backoff: config.spot.ws_reconnect_backoff.clone(),
            },
            Arc::clone(&ws_state),
            Arc::clone(&cancel),
            ws_lifecycle_tx,
        ))
    } else {
        None
    };
    let mut last_ws_generation = ws_state::generation(&ws_state);
    macro_rules! check_cancel {
        () => {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
        };
    }

    // ── Boot-time health diagnostics ─────────────────────────────────────────
    if execute && _ws_session.is_some() {
        tracing::info!(target: "engine::boot", "Running pre-flight diagnostics...");
        // 1. Wait for WS subscriptions to acknowledge (up to 15s)
        let ws_ready = {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
            let mut ready = false;
            while tokio::time::Instant::now() < deadline {
                if ws_state::can_execute(&ws_state) {
                    ready = true;
                    break;
                }
                // Even if can_execute is false, subscriptions_ready might be true but
                // bulk_ladder_hydrated is false — try REST to unblock.
                {
                    let s = ws_state.read().expect("WS lock");
                    if s.subscriptions_ready && !s.bulk_ladder_hydrated {
                        drop(s);
                        let market = api.market(&config.market_name, config.product).await.ok();
                        if let Some(m) = market {
                            if let Ok(rest_ladder) =
                                api.active_bulk_ladder(&settings.subaccount, &m).await
                            {
                                ws_state::recover_bulk_ladder(&ws_state, rest_ladder);
                            }
                        }
                    }
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
                check_cancel!();
            }
            ready
        };
        if !ws_ready {
            let s = ws_state.read().expect("WS lock");
            let reasons: Vec<&str> = {
                let mut r = Vec::new();
                if !s.subscriptions_ready {
                    r.push("subscriptions not acknowledged");
                }
                if s.last_error.is_some() {
                    r.push("connection error");
                }
                r
            };
            drop(s);
            anyhow::bail!(
                "WebSocket did not become ready within 15s: {}. Check network connectivity, API key validity, and testnet status.",
                reasons.join(", ")
            );
        }
        tracing::info!(target: "engine::boot", "WS ready, subscriptions acknowledged.");

        // 2. Check market data sanity
        let market = api
            .market(&config.market_name, config.product)
            .await
            .context("resolve market for health check")?;
        // Check depth crossed
        let depth_ok = {
            let s = ws_state.read().expect("WS lock");
            s.depth.as_ref().map(|d| {
                let bid = d.value.bids.first().map(|l| l.price);
                let ask = d.value.asks.first().map(|l| l.price);
                (bid, ask)
            })
        };
        match depth_ok {
            Some((Some(bid), Some(ask))) if bid >= ask => {
                tracing::warn!(target: "engine::boot", bid = %bid, ask = %ask, "depth crossed (bid >= ask) — market data may be stale or abnormal");
            }
            Some((Some(bid), Some(ask))) => {
                tracing::info!(target: "engine::boot", bid = %bid, ask = %ask, spread = %(ask - bid), "depth healthy");
            }
            _ => {
                tracing::warn!(target: "engine::boot", "depth not yet available; will retry during cycles");
            }
        }
        // Check mid price / perp price
        if let Ok(mid) = api.mid_price(&market, config.price_source).await {
            if mid <= Decimal::ZERO {
                tracing::warn!(target: "engine::boot", "mid price is non-positive ({mid}); market may be untradable");
            } else {
                tracing::info!(target: "engine::boot", mid = %mid, "price source healthy");
            }
        } else {
            tracing::warn!(target: "engine::boot", "mid price unavailable at boot; will retry during cycles");
        }
        tracing::info!(target: "engine::boot", "Pre-flight diagnostics complete.");
    }

    loop {
        let cycle_start = tokio::time::Instant::now();
        let snapshot_result = if settings.subaccount.trim().is_empty() {
            fetch_snapshot(&api, &config, optional_subaccount(&settings)).await
        } else {
            fetch_snapshot_ws_first(
                &api,
                &config,
                optional_subaccount(&settings),
                &ws_state,
                Duration::from_secs(2),
            )
            .await
        };
        let snapshot = match snapshot_result {
            Ok(s) => s,
            Err(e) => {
                let error = format!("{e:#}");
                eprintln!("grid refresh failed: {error}");
                if let Some(runtime) = &engine_runtime {
                    runtime
                        .update_status(|status| {
                            status.last_error = Some(error.clone());
                        })
                        .await;
                }
                check_cancel!();
                tokio::time::sleep(config.refresh).await;
                continue;
            }
        };
        check_cancel!();
        let phase = if execute {
            engine_phase(&ws_state, &run_state)
        } else {
            control::EnginePhase::Ready
        };
        // If phase is Hydrating because bulk_ladder snapshot hasn't arrived yet, proactively
        // query REST to unblock execution rather than waiting indefinitely for a WS push.
        if execute && phase == control::EnginePhase::Hydrating {
            if let Ok(Some(active)) = api
                .active_bulk_ladder(&settings.subaccount, &snapshot.market)
                .await
            {
                let seq = active.sequence;
                ws_state::recover_bulk_ladder(&ws_state, Some(active));
                tracing::info!(target: "engine", sequence = seq, "bulk ladder hydrated via REST");
            } else {
                // REST also has no ladder — seed an empty bulk snapshot so the engine can
                // derive sequence 1 and proceed.
                ws_state::recover_bulk_ladder(&ws_state, None);
                tracing::info!(target: "engine", "no active bulk ladder on venue; proceeding with empty state");
            }
        }
        if let Some(runtime) = &engine_runtime {
            let mid = snapshot.plan.mid.to_string();
            let funds = snapshot.account.spot_funds.as_ref().map(|funds| {
                (
                    funds.base_symbol.clone(),
                    funds.base_balance.to_string(),
                    funds.quote_symbol.clone(),
                    funds.quote_balance.to_string(),
                )
            });
            let is_perp = snapshot.market.product == Product::Perp;
            let perp_mode = if is_perp {
                Some(format!("{:?}", config.perp_mode).to_lowercase())
            } else {
                None
            };
            let max_position = if is_perp {
                config.max_position.map(|value| value.to_string())
            } else {
                None
            };
            let position = if is_perp {
                Some(snapshot.account.position.size.to_string())
            } else {
                None
            };
            let target_position = if is_perp {
                snapshot.plan.target_position.map(|value| value.to_string())
            } else {
                None
            };
            let planning_price = if is_perp {
                snapshot.plan.planning_price.map(|value| value.to_string())
            } else {
                None
            };
            let worst_long = if is_perp {
                snapshot.plan.worst_long.map(|value| value.to_string())
            } else {
                None
            };
            let worst_short = if is_perp {
                snapshot.plan.worst_short.map(|value| value.to_string())
            } else {
                None
            };
            let perp_blocked = if is_perp {
                snapshot.plan.perp_blocked_reason.clone()
            } else {
                None
            };
            let out_of_range_action = if is_perp {
                snapshot.plan.out_of_range_action_applied.clone()
            } else {
                None
            };
            let available_margin = if is_perp {
                snapshot
                    .account
                    .available_margin
                    .map(|value| value.to_string())
            } else {
                None
            };
            let estimated_margin = if is_perp {
                snapshot
                    .plan
                    .estimated_margin
                    .map(|value| value.to_string())
            } else {
                None
            };
            runtime
                .update_status(|status| {
                    status.phase = "running".to_owned();
                    status.engine_phase = phase.clone();
                    status.last_cycle_at = Some(Utc::now());
                    status.mid = Some(mid);
                    status.last_error = None;
                    status.perp_mode = perp_mode.clone();
                    status.max_position = max_position.clone();
                    status.position = position.clone();
                    status.target_position = target_position.clone();
                    status.planning_price = planning_price.clone();
                    status.worst_long = worst_long.clone();
                    status.worst_short = worst_short.clone();
                    status.perp_blocked_reason = perp_blocked.clone();
                    status.out_of_range_action = out_of_range_action.clone();
                    status.paused_by_out_of_range = is_perp && snapshot.plan.paused_by_out_of_range;
                    status.available_margin = available_margin.clone();
                    status.estimated_margin = estimated_margin.clone();
                    if let Some((base_symbol, base_balance, quote_symbol, quote_balance)) = funds {
                        status.pfs_base_symbol = Some(base_symbol);
                        status.pfs_base_balance = Some(base_balance);
                        status.pfs_quote_symbol = Some(quote_symbol);
                        status.pfs_quote_balance = Some(quote_balance);
                    }
                })
                .await;
        }
        // Rebuild the plan without trade-history markers. Historical fills are a UI hint and
        // must not suppress a future desired order during reconciliation or execution.
        let mut snapshot = snapshot;
        last_market = Some(snapshot.market.clone());
        if execute
            && run_state.bulk_ladder.as_ref().is_some_and(|ladder| {
                matches!(
                    ladder.state,
                    journal::BulkLadderState::IntentRecorded
                        | journal::BulkLadderState::BroadcastPending
                        | journal::BulkLadderState::BroadcastUnknown
                        | journal::BulkLadderState::Committed
                )
            })
        {
            let expected = run_state
                .bulk_ladder
                .as_ref()
                .expect("checked above")
                .clone();
            if ws_state::active_bulk_ladder(&ws_state)?
                .is_some_and(|active| active.matches(&expected))
            {
                let event = journal::JournalEvent::BulkVenueObserved {
                    at: Utc::now(),
                    operation_id: expected.operation_id.clone(),
                };
                let journal = journal
                    .as_ref()
                    .expect("live execution always has a durable journal");
                journal.append(&event)?;
                run_state.apply(&event);
                journal.save_state(&run_state)?;
                println!(
                    "Recovered bulk operation {} as venue-observed sequence {}.",
                    expected.operation_id, expected.sequence
                );
            }
        }
        if execute && snapshot.market.product == Product::Perp && !legacy_perp_state {
            if !perp_accounting_initialized {
                match api
                    .perp_fill_history(&settings.subaccount, &snapshot.market, None)
                    .await
                {
                    Ok(fills) => {
                        if !snapshot.account.position.size.is_zero() {
                            anyhow::bail!(
                                "refusing Perp startup with existing position {}; no verified grid journal is available",
                                snapshot.account.position.size
                            )
                        }
                        // Existing historical trades are not strategy PnL for this new run.
                        perp_accounting.seed_historical_fills(&fills, snapshot.observed_at);
                        perp_accounting_initialized = true;
                        perp_runtime.accounting = perp_accounting.clone();
                        run_state.perp_runtime = Some(perp_runtime.clone());
                        if let Some(journal) = &journal {
                            journal.save_state(&run_state)?;
                        }
                    }
                    Err(error) => {
                        perp_accounting_blocked = Some(format!(
                            "Perp accounting unavailable; refusing new risk: {error:#}"
                        ));
                    }
                }
            } else {
                let current_generation = ws_state::generation(&ws_state);
                if current_generation != last_ws_generation {
                    last_ws_generation = current_generation;
                    // The private stream has no replay cursor. Recover the bounded overlap once
                    // per new connection generation before trusting newly hydrated WS trades.
                    match api
                        .perp_fill_history(
                            &settings.subaccount,
                            &snapshot.market,
                            perp_accounting.history_cursor(),
                        )
.await
                         {
                             Ok(fills) => {
                                 for fill in fills {
                                     if !perp_accounting.has_processed_fill(&fill.id) {
                                         let event = journal::JournalEvent::PerpFillApplied {
                                             at: Utc::now(),
                                             fill: fill.clone(),
                                         };
                                         let journal = journal
                                             .as_ref()
                                             .expect("live Perp execution always has a durable journal");
journal.append(&event)?;
                             run_state.apply(&event);
                             perp_accounting.apply_fill(&fill)?;
                             if config.perp_mode == decibel_grid_tui::PerpMode::Rotate {
                                 if let Some(ref mut rot_state) = perp_runtime.rotating_state {
                                     decibel_grid_tui::strategy::perp::rotate::apply_fill_to_rotating_state(
                                         rot_state,
                                         &fill,
                                         snapshot.market.lot_size,
                                     );
                                 }
                             }
                             journal.save_state(&run_state)?;
                                     }
                                 }
                             }
                        Err(error) => {
                            perp_accounting_blocked = Some(format!(
                                "Perp trade backfill after WS reconnect failed: {error:#}"
                            ));
                        }
                    }
                }
                match ws_state::perp_fills(&ws_state) {
                    Ok(fills) => {
                        for fill in fills {
                            if perp_accounting.has_processed_fill(&fill.id) {
                                continue;
                            }
                            let event = journal::JournalEvent::PerpFillApplied {
                                at: Utc::now(),
                                fill: fill.clone(),
                            };
                            let journal = journal
                                .as_ref()
                                .expect("live Perp execution always has a durable journal");
                            journal.append(&event)?;
                            run_state.apply(&event);
                            perp_accounting.apply_fill(&fill)?;
                            journal.save_state(&run_state)?;
                        }
                        perp_runtime.accounting = perp_accounting.clone();
                        run_state.perp_runtime = Some(perp_runtime.clone());
                    }
                    Err(error) => {
                        // A Net action has no trustworthy side in the WS DTO. This is the narrow
                        // recovery path that may consult order history through the REST helper.
                        match api
                            .perp_fill_history(
                                &settings.subaccount,
                                &snapshot.market,
                                perp_accounting.history_cursor(),
                            )
                            .await
                        {
                            Ok(fills) => {
                                for fill in fills {
                                    if !perp_accounting.has_processed_fill(&fill.id) {
                                        let event = journal::JournalEvent::PerpFillApplied {
                                            at: Utc::now(),
                                            fill: fill.clone(),
                                        };
                                        let journal = journal.as_ref().expect(
                                            "live Perp execution always has a durable journal",
                                        );
                                        journal.append(&event)?;
                                        run_state.apply(&event);
                                        perp_accounting.apply_fill(&fill)?;
                                        journal.save_state(&run_state)?;
                                    }
                                }
                            }
                            Err(recovery_error) => {
                                perp_accounting_blocked = Some(format!(
                                    "Perp WS trade could not be resolved ({error:#}); recovery failed: {recovery_error:#}"
                                ))
                            }
                        }
                    }
                }
                if perp_accounting_blocked.is_none()
                    && !perp_accounting.position_matches_exchange(
                        snapshot.account.position.size,
                        snapshot.market.lot_size,
                    )
                {
                    perp_accounting_blocked = Some(format!(
                        "Perp accounting mismatch: exchange position {} vs ledger {}",
                        snapshot.account.position.size, perp_accounting.position_base
                    ));
                } else if perp_accounting_blocked.is_none() {
                    perp_accounting_blocked = None;
                }
            }
            let mark_price = match api.mark_price(&snapshot.market).await {
                Ok(price) => Some(price),
                Err(error) => {
                    let message =
                        format!("Perp mark-price unavailable; PnL is incomplete: {error:#}");
                    eprintln!("{message}");
                    if let Some(runtime) = &engine_runtime {
                        runtime
                            .update_status(|status| {
                                status.events.push(decibel_grid_tui::control::EngineEvent {
                                    at: Utc::now(),
                                    message: message.clone(),
                                });
                                if status.events.len() > 200 {
                                    status.events.drain(..status.events.len() - 200);
                                }
                            })
                            .await;
                    }
                    None
                }
            };
            if let Some(runtime) = &engine_runtime {
                let pnl =
                    perp_pnl_status(&perp_accounting, snapshot.account.position.size, mark_price);
                let accounting_blocked = perp_accounting_blocked.clone();
                runtime
                    .update_status(|status| {
                        status.perp_pnl = Some(pnl);
                        status.realized_pnl =
                            Some(perp_accounting.realized_gross_quote.to_string());
                        if accounting_blocked.is_some() {
                            status.perp_blocked_reason = accounting_blocked;
                        }
                    })
                    .await;
            }
        }
        // Trade history is returned newest-first. A newly observed trade is a strong trigger for
        // replacement; a small price-only drift is not. Seed the cursor on the first cycle so
        // historical fills from before this process started do not cause an immediate refresh.
        let latest_trade_ms = snapshot.trades.iter().map(|trade| trade.timestamp_ms).max();
        let new_trade_observed = match (last_seen_trade_ms, latest_trade_ms) {
            (Some(previous), Some(latest)) => latest > previous,
            (None, Some(_)) => false,
            _ => false,
        };
        if let Some(latest) = latest_trade_ms {
            last_seen_trade_ms =
                Some(last_seen_trade_ms.map_or(latest, |previous| previous.max(latest)));
        }
        if snapshot.market.product == Product::Spot {
            use decibel_grid_tui::strategy::spot::runtime::{
                SpotCycleContext, SpotCycleOutcome, run_spot_cycle,
            };
            match run_spot_cycle(&mut SpotCycleContext {
                execute,
                spot_exit_price,
                spot_fee_rates: spot_fee_rates.as_ref(),
                network: &settings.network,
                api_key: &settings.api_key,
                aptos_private_key: &settings.aptos_private_key,
                gas_station,
                subaccount: &settings.subaccount,
                config: &mut config,
                journal: journal.as_ref(),
                run_state: &mut run_state,
                pinned_spot_plan: &mut pinned_spot_plan,
                snapshot: &mut snapshot,
                stop_loss_liquidated: &mut stop_loss_liquidated,
                paused_by_breakout: &mut paused_by_breakout,
                cancelled: Arc::clone(&cancel),
            })
            .await?
            {
                SpotCycleOutcome::BreakLoop => break,
                SpotCycleOutcome::ContinueOuterLoop => continue,
                SpotCycleOutcome::Completed => {}
            }
        } else {
            let offline = decibel_grid_tui::simulation::run_offline_cycle(
                decibel_grid_tui::simulation::OfflineCycleInput {
                    config: config.clone(),
                    market: snapshot.market.clone(),
                    mid: snapshot.plan.mid,
                    account: snapshot.account.clone(),
                    pinned_spot_plan: None,
                    spot_exit_price: None,
                },
            )?;
            snapshot.plan = offline.plan;
            if snapshot.market.product == Product::Perp {
                // The Perp grid is a ladder, not a market-making quote. Rebuilding it from every
                // refreshed price would atomically cancel orders just before they can fill and
                // chase the market away. Evaluate range breaks from the fresh plan, but otherwise
                // retain the first accepted geometry and only replace it after a real fill.
                let fresh_plan = snapshot.plan.clone();
                if fresh_plan.out_of_range_action_applied.is_none() {
                    if let Some(pinned) = &perp_runtime.pinned_plan {
                        snapshot.plan = pinned.clone();
                        snapshot.plan.raw_planning_price = fresh_plan.raw_planning_price;
} else {
                         perp_runtime.pinned_plan = Some(fresh_plan);
                         perp_runtime.accounting = perp_accounting.clone();
                         if config.perp_mode == decibel_grid_tui::PerpMode::Rotate
                             && perp_runtime.rotating_state.is_none()
                         {
                             let bid_prices: Vec<_> = snapshot
                                 .plan
                                 .bids
                                 .iter()
                                 .map(|l| l.price)
                                 .collect();
                             let ask_prices: Vec<_> = snapshot
                                 .plan
                                 .asks
                                 .iter()
                                 .map(|l| l.price)
                                 .collect();
                             perp_runtime.rotating_state = Some(
                                 decibel_grid_tui::strategy::perp::rotate::RotatingGridState::new_with_pinned_prices(
                                     bid_prices, ask_prices,
                                 ),
                             );
                         }
                         run_state.perp_runtime = Some(perp_runtime.clone());
                         if let Some(journal) = &journal {
                             journal.save_state(&run_state)?;
                         }
}
                 }
                 if config.perp_mode == decibel_grid_tui::PerpMode::Rotate
                     && let Some(ref rot_state) = perp_runtime.rotating_state
                 {
                     let planning_price = snapshot.plan.planning_price.unwrap_or(snapshot.plan.mid);
                     match decibel_grid_tui::strategy::perp::rotate::build_rotate_ladder(
                         &config,
                         &snapshot.market,
                         planning_price,
                         rot_state,
                     ) {
                         Ok(rotate_plan) => {
                             snapshot.plan = rotate_plan;
                         }
                         Err(error) => {
                             eprintln!("Rotate ladder build failed: {error:#}; falling back to pinned plan");
                         }
                     }
                 }
                 snapshot.plan =
                    match decibel_grid_tui::strategy::perp::runtime::finalize_perp_executable_plan(
                        &config,
                        snapshot.plan.clone(),
                        snapshot.account.position.size,
                        snapshot.account.available_margin,
                    ) {
                        Ok(plan) => {
                            perp_risk_pause_reason = None;
                            plan
                        }
                        Err(error) => {
                            let reason = format!("Perp risk evaluation paused: {error:#}");
                            eprintln!("RISK PAUSED: {reason}");
                            if let Some(runtime) = &engine_runtime {
                                runtime
                                    .update_status(|status| {
                                        status.phase = "risk_paused".to_owned();
                                        status.last_error = Some(reason.clone());
                                        status.perp_blocked_reason = Some(reason.clone());
                                    })
                                    .await;
                            }
                            if perp_risk_pause_reason.is_none() {
                                let event = journal::JournalEvent::RiskRejected {
                                    at: Utc::now(),
                                    reason: reason.clone(),
                                };
                                if let Some(journal) = &journal {
                                    if let Err(journal_error) = journal.append(&event) {
                                        eprintln!(
                                            "could not persist Perp risk pause: {journal_error:#}"
                                        );
                                    } else {
                                        run_state.apply(&event);
                                        if let Err(journal_error) = journal.save_state(&run_state) {
                                            eprintln!(
                                                "could not save Perp risk pause state: {journal_error:#}"
                                            );
                                        }
                                    }
                                }
                                if execute {
                                    match journaled_bulk_cancel(
                                        journal.as_ref(),
                                        &mut run_state,
                                        &settings.network,
                                        &settings.aptos_private_key,
                                        &settings.subaccount,
                                        &snapshot.market,
                                        gas_station,
                                    )
                                    .await
                                    {
                                        Ok(Some(hash)) => println!(
                                            "Perp risk pause cancelled the active ladder in tx {hash}; position retained."
                                        ),
                                        Ok(None) => {}
                                        Err(cancel_error) => eprintln!(
                                            "Perp risk pause could not cancel the active ladder: {cancel_error:#}"
                                        ),
                                    }
                                }
                            }
                            perp_risk_pause_reason = Some(reason);
                            if let Some(notifier) = &notifier {
                                let notifier = notifier.clone();
                                let reason = perp_risk_pause_reason.clone().unwrap_or_default();
                                tokio::spawn(async move {
                                    notifier.send("Decibel Perp risk paused", &reason).await;
                                });
                            }
                            tokio::time::sleep(config.refresh).await;
                            continue;
                        }
                    };
                if perp_runtime.bootstrap_status == journal::PerpBootstrapStatus::Pending
                    && perp_runtime.bootstrap_target_position.is_none()
                {
                    match snapshot.plan.target_position {
                        Some(target)
                            if decibel_grid_tui::strategy::perp::risk::perp_bootstrap_target_is_safe(
                                &config, target,
                            ) =>
                        {
                            let locked = perp_runtime
                                .lock_bootstrap_target(target)
                                .expect("pending bootstrap accepts its first target");
                            println!("Perp bootstrap target locked at {locked}.");
                        }
                        Some(target) => {
                            perp_runtime.bootstrap_status = journal::PerpBootstrapStatus::Blocked;
                            snapshot.plan.perp_blocked_reason = Some(format!(
                                "Perp bootstrap blocked: target position {target} exceeds GRID_MAX_POSITION"
                            ));
                        }
                        None => {
                            perp_runtime.bootstrap_status = journal::PerpBootstrapStatus::Blocked;
                            snapshot.plan.perp_blocked_reason = Some(
                                "Perp bootstrap blocked: initial plan has no target position"
                                    .to_owned(),
                            );
                        }
                    }
                    perp_runtime.accounting = perp_accounting.clone();
                    run_state.perp_runtime = Some(perp_runtime.clone());
                    if let Some(journal) = &journal {
                        journal.save_state(&run_state)?;
                    }
                }
                if let Some(reason) = perp_accounting_blocked.clone() {
                    snapshot.plan.perp_blocked_reason = Some(reason);
                }
                snapshot.plan.convergence_delta =
                    if perp_runtime.bootstrap_status == journal::PerpBootstrapStatus::Pending {
                        perp_runtime
                            .bootstrap_target_position
                            .map(|target| target - snapshot.account.position.size)
                    } else {
                        None
                    };
                if perp_runtime.bootstrap_status == journal::PerpBootstrapStatus::Blocked {
                    // Preserve the initial fail-closed cause (invalid target, missing target, or
                    // convergence failure). Replacing it with this generic status hides the
                    // operator action required to unblock the durable bootstrap state.
                    snapshot.plan.perp_blocked_reason.get_or_insert_with(|| {
                        "Perp bootstrap blocked; automatic market convergence and grid submission are paused"
                            .to_owned()
                    });
                }
                if execute {
                    decibel_grid_tui::strategy::perp::runtime::handle_perp_out_of_range(
                        &config,
                        &snapshot.plan,
                        &settings.network,
                        &settings.aptos_private_key,
                        &settings.subaccount,
                        &snapshot.market,
                        &api,
                        execute,
                        gas_station,
                        &mut out_of_range_handled,
                    )
                    .await?;
                }
            }
        }

        if snapshot.market.product == Product::Perp
            && let Some(runtime) = &engine_runtime
        {
            let configured_action = match config.out_of_range_action {
                decibel_grid_tui::OutOfRangeAction::Pause => "pause",
                decibel_grid_tui::OutOfRangeAction::CancelOrders => "cancel_orders",
                decibel_grid_tui::OutOfRangeAction::ClosePosition => "close_position",
                decibel_grid_tui::OutOfRangeAction::ClampContinue => "clamp_continue",
            };
            let blocked = decibel_grid_tui::strategy::perp::runtime::perp_submission_blocked(
                &config,
                &snapshot.plan,
                snapshot.account.position.size,
                snapshot.account.available_margin,
                snapshot.market.lot_size,
                perp_runtime.bootstrap_status == journal::PerpBootstrapStatus::Pending,
            );
            runtime
                .update_status(|status| {
                    status.planning_price =
                        snapshot.plan.planning_price.map(|value| value.to_string());
                    status.position = Some(snapshot.account.position.size.to_string());
                    status.target_position = perp_runtime
                        .bootstrap_target_position
                        .map(|value| value.to_string());
                    status.perp_bootstrap_status =
                        Some(format!("{:?}", perp_runtime.bootstrap_status).to_lowercase());
                    status.convergence_delta = snapshot
                        .plan
                        .convergence_delta
                        .map(|value| value.to_string());
                    status.worst_long = snapshot.plan.worst_long.map(|value| value.to_string());
                    status.worst_short = snapshot.plan.worst_short.map(|value| value.to_string());
                    status.perp_blocked_reason =
                        snapshot.plan.perp_blocked_reason.clone().or(blocked);
                    status.out_of_range_action = Some(configured_action.to_owned());
                    status.paused_by_out_of_range = snapshot.plan.paused_by_out_of_range;
                })
                .await;
        }

        // Read the resting orders BEFORE funding or fitting a Spot plan. The executable Spot
        // plan is pinned; it must not be shrunk to today's free balance, because insufficient
        // inventory is a precondition to resolve before the ladder is submitted.
        // ask side to whatever base is already held, which makes the plan match the chain
        // exactly (`0 missing`) and hides the very shortfall that funding is supposed to close.
        let mut actual_for_execution = None;
        let execution_permitted = execute && phase == control::EnginePhase::Ready;
        if execute {
            if ws_state::bulk_ladder_desynced(&ws_state) {
                eprintln!(
                    "WebSocket bulk sequence gap detected; recovering the authoritative ladder once."
                );
                match api
                    .active_bulk_ladder(&settings.subaccount, &snapshot.market)
                    .await
                {
                    Ok(ladder) => ws_state::recover_bulk_ladder(&ws_state, ladder),
                    Err(error) => {
                        eprintln!("bulk ladder recovery failed: {error:#}; skipping cycle");
                        check_cancel!();
                        tokio::time::sleep(config.refresh).await;
                        continue;
                    }
                }
            }
            let actual = match ws_state::actual_orders(&ws_state) {
                Ok(orders) => orders,
                Err(e) => {
                    eprintln!("reconciliation failed (WebSocket orders): {e:#}; skipping cycle");
                    check_cancel!();
                    tokio::time::sleep(config.refresh).await;
                    continue;
                }
            };
            check_cancel!();
            // Every bootstrap decision comes from the current PFS snapshot. This same shortfall
            // helper is used at startup, after fills/re-centering, and before bulk submission.
            if config.spot.auto_convert_missing_base
                && !paused_by_spot_funding_circuit
                && snapshot.market.product == Product::Spot
                && let Some(funds) = &snapshot.account.spot_funds
                && let Some(base_gap) =
                    decibel_grid_tui::spot_base_shortfall(&snapshot.plan, funds, &snapshot.market)
                && decibel_grid_tui::reconcile::blocking_orders(&actual).is_empty()
            {
                let quote_spare = (funds.available_quote_for_bulk() - snapshot.plan.quote_required)
                    .max(Decimal::ZERO);
                let funding_result = spot_taker::execute_guarded_spot_ioc(
                    &settings.network,
                    &api,
                    &settings.aptos_private_key,
                    &settings.subaccount,
                    &snapshot.market,
                    spot_taker::TakerSide::Buy,
                    base_gap,
                    Some(quote_spare),
                    spot_fee_rates
                        .as_ref()
                        .expect("live Spot execution fetched fee rates"),
                    &config.spot,
                    gas_station,
                    Some(&ws_state),
                )
                .await;
                check_cancel!();
                match &funding_result {
                    Ok(funding) if funding.filled_total > Decimal::ZERO => {
                        println!(
                            "Spot base funding: filled {} of {} across {} IOC attempt(s).",
                            funding.filled_total, base_gap, funding.attempts
                        );
                    }
                    Ok(_) => {}
                    Err(error) => eprintln!("Spot base funding skipped: {error:#}"),
                }
                // Re-read balances after the IOC attempt — refinancing the plan with up-to-date
                // PFS balances is better than using stale pre-funding data.
                match api
                    .account(Some(&settings.subaccount), &snapshot.market)
                    .await
                {
                    Ok(account) => {
                        snapshot.account = account;
                        let accepted_partial = funding_result.as_ref().is_ok_and(|funding| {
                            base_gap > Decimal::ZERO
                                && funding.filled_total / base_gap
                                    >= config.spot.entry_min_fill_ratio
                        });
                        let available_base = snapshot
                            .account
                            .spot_funds
                            .as_ref()
                            .expect("Spot funds refreshed")
                            .available_base_for_bulk();
                        let needed_base = snapshot.plan.base_required;
                        if available_base >= needed_base {
                            consecutive_spot_funding_failures = 0;
                            println!(
                                "Spot base funding target reached; proceeding to the full pinned grid."
                            );
                        } else if accepted_partial {
                            let reduced = snapshot
                                .plan
                                .reduce_asks_to_available_base(available_base)?;
                            let dropped = snapshot.plan.asks.len() - reduced.asks.len();
                            let received_ratio = if needed_base > Decimal::ZERO {
                                available_base / needed_base
                            } else {
                                Decimal::ONE
                            };
                            if reduced.asks.is_empty() {
                                eprintln!(
                                    "Spot entry received {:.2}% of required base but cannot fund even one ask level; startup remains failed.",
                                    received_ratio * Decimal::from(100)
                                );
                            } else {
                                println!(
                                    "Spot entry received {:.2}% of required base ({} available vs {} needed); reduced asks from {} to {} and dropped {} unfundable level(s).",
                                    received_ratio * Decimal::from(100),
                                    available_base,
                                    needed_base,
                                    snapshot.plan.asks.len(),
                                    reduced.asks.len(),
                                    dropped
                                );
                                pinned_spot_plan = Some(reduced.clone());
                                snapshot.plan = reduced
                                    .project_spot(snapshot.plan.mid, snapshot.market.tick_size)?;
                                consecutive_spot_funding_failures = 0;
                            }
                        } else {
                            consecutive_spot_funding_failures += 1;
                            if consecutive_spot_funding_failures
                                >= config.spot.entry_exit_max_attempts
                            {
                                paused_by_spot_funding_circuit = true;
                                let reason = format!(
                                    "Spot entry funding failed to reach the {:.2}% minimum fill ratio {} consecutive time(s); paused pending manual intervention",
                                    config.spot.entry_min_fill_ratio * Decimal::from(100),
                                    consecutive_spot_funding_failures
                                );
                                eprintln!("RISK REJECTED: {reason}");
                                if let Some(journal) = &journal {
                                    let event = journal::JournalEvent::RiskRejected {
                                        at: Utc::now(),
                                        reason,
                                    };
                                    journal.append(&event)?;
                                    run_state.apply(&event);
                                    journal.save_state(&run_state)?;
                                }
                            }
                        }
                    }
                    Err(error) => eprintln!("  balance refresh after funding failed: {error:#}"),
                }
            }
            // Do not sell base to manufacture quote during startup. The initial rebalance is
            // one-way by design: preserve the bid reserve and buy only the missing ask inventory.
            check_cancel!();
            actual_for_execution = Some(actual);
        }

        // This runs even when the remaining gap is smaller than one legal IOC. Such a gap is not
        // sufficient to claim the full ladder is funded, but it can fund fewer whole ask levels.
        if execute
            && snapshot.market.product == Product::Spot
            && actual_for_execution.as_ref().is_some_and(|orders| {
                decibel_grid_tui::reconcile::blocking_orders(orders).is_empty()
            })
            && let Some(funds) = &snapshot.account.spot_funds
            && funds.available_base_for_bulk() < snapshot.plan.base_required
        {
            let reduced = snapshot
                .plan
                .reduce_asks_to_available_base(funds.available_base_for_bulk())?;
            if !reduced.asks.is_empty() && reduced.asks.len() < snapshot.plan.asks.len() {
                let dropped = snapshot.plan.asks.len() - reduced.asks.len();
                let ratio = funds.available_base_for_bulk() / snapshot.plan.base_required;
                println!(
                    "Spot base is {:.2}% funded ({} available vs {} needed); reduced asks from {} to {} and dropped {} unfundable level(s).",
                    ratio * Decimal::from(100),
                    funds.available_base_for_bulk(),
                    snapshot.plan.base_required,
                    snapshot.plan.asks.len(),
                    reduced.asks.len(),
                    dropped
                );
                pinned_spot_plan = Some(reduced.clone());
                snapshot.plan =
                    reduced.project_spot(snapshot.plan.mid, snapshot.market.tick_size)?;
            } else if reduced.asks.is_empty() {
                eprintln!("Spot base cannot fund even one ask level; full ladder remains blocked.");
            }
        }

        if let Some(adjustment) = fit_spot_snapshot_to_pfs(&mut snapshot)? {
            println!("Spot funding check: {adjustment}");
        }
        // A live attach view must retain the last exchange-confirmed ladder until
        // this cycle's reconciliation replaces it below. Replacing it here with
        // the freshly generated plan would make resting orders flicker as Planned.
        if !execute && let Some(runtime) = &engine_runtime {
            let ladder = decibel_grid_tui::control::ladder_from_plan(&snapshot.plan);
            runtime
                .update_status(|status| {
                    status.ladder = ladder;
                })
                .await;
        }
        print_snapshot(&snapshot, &config);
        if config.product == Product::Spot
            && let Some(pinned_plan) = &pinned_spot_plan
        {
            run_state.spot_runtime = Some(journal::SpotRuntimeState {
                pinned_plan: pinned_plan.clone(),
                last_seen_trade_ms,
            });
        }
        if let Some(journal) = &journal {
            let event = journal::JournalEvent::PlanGenerated {
                at: Utc::now(),
                mid: snapshot.plan.mid.normalize().to_string(),
                bid_levels: snapshot.plan.bids.len(),
                ask_levels: snapshot.plan.asks.len(),
                quote_required: snapshot.plan.quote_required.normalize().to_string(),
                base_required: snapshot.plan.base_required.normalize().to_string(),
            };
            journal.append(&event)?;
            run_state.apply(&event);
            journal.save_state(&run_state)?;
        }

        if execution_permitted {
            // 1. Reconcile using the order snapshot fetched before Spot funding/fitting.
            let actual = actual_for_execution
                .ok_or_else(|| anyhow::anyhow!("execution order snapshot was not available"))?;
            let desired = decibel_grid_tui::reconcile::desired_orders(
                &snapshot.plan,
                snapshot.market.tick_size,
                snapshot.market.lot_size,
            );
            let reconcile_result = decibel_grid_tui::reconcile::reconcile(
                &desired,
                &actual,
                snapshot.market.tick_size,
                snapshot.market.lot_size,
            );

            tracing::debug!(
                target: "engine::reconcile",
                matched = reconcile_result.matched.len(),
                missing = reconcile_result.missing.len(),
                unmanaged = reconcile_result.unmanaged.len(),
                converged = reconcile_result.is_converged(),
                "reconciliation complete"
            );
            if let Some(runtime) = &engine_runtime {
                let matched = reconcile_result.matched.len();
                let missing = reconcile_result.missing.len();
                let unmanaged = reconcile_result.unmanaged.len();
                let ladder = decibel_grid_tui::control::ladder_from_reconciliation(
                    &desired,
                    &reconcile_result,
                );
                runtime
                    .update_status(|status| {
                        status.matched = Some(matched);
                        status.missing = Some(missing);
                        status.unmanaged = Some(unmanaged);
                        status.ladder = ladder;
                        status.events.push(decibel_grid_tui::control::EngineEvent {
                            at: Utc::now(),
                            message: format!("reconcile: {matched} matched, {missing} missing, {unmanaged} unmanaged"),
                        });
                        if status.events.len() > 200 { status.events.drain(..status.events.len() - 200); }
                    })
                    .await;
            }
            if let Some(journal) = &journal {
                let event = journal::JournalEvent::ReconciliationResult {
                    at: Utc::now(),
                    matched: reconcile_result.matched.len(),
                    missing: reconcile_result.missing.len(),
                    unmanaged: reconcile_result.unmanaged.clone(),
                    is_converged: reconcile_result.is_converged(),
                };
                journal.append(&event)?;
                run_state.apply(&event);
                journal.save_state(&run_state)?;
            }

            // State files written before bootstrap tracking must never be interpreted as a new
            // strategy that is allowed to send a market order. Only a fully matched active bulk
            // ladder proves this process can safely regard the old run as already bootstrapped.
            if snapshot.market.product == Product::Perp
                && perp_runtime.bootstrap_status == journal::PerpBootstrapStatus::LegacyUnknown
            {
                let has_active_bulk_ladder = actual
                    .iter()
                    .any(|order| order.order_id.starts_with("bulk:"));
                if has_active_bulk_ladder && reconcile_result.is_converged() {
                    match api
                        .perp_fill_history(&settings.subaccount, &snapshot.market, None)
                        .await
                    {
                        Ok(fills) => {
                            perp_accounting.seed_historical_fills(&fills, snapshot.observed_at);
                            perp_accounting_initialized = true;
                            legacy_perp_state = false;
                            perp_runtime.accounting = perp_accounting.clone();
                            perp_runtime.bootstrap_status = journal::PerpBootstrapStatus::Completed;
                            println!(
                                "Perp legacy state migrated: active bulk ladder matched; bootstrap marked completed without market convergence."
                            );
                        }
                        Err(error) => {
                            let reason = format!(
                                "Perp bootstrap blocked: cannot establish a historical accounting baseline for legacy state: {error:#}"
                            );
                            eprintln!("{reason}");
                            perp_runtime.bootstrap_status = journal::PerpBootstrapStatus::Blocked;
                            snapshot.plan.perp_blocked_reason = Some(reason);
                        }
                    }
                } else {
                    let reason = "Perp bootstrap blocked: legacy state has no fully matched active bulk ladder; restart only after manual ownership review".to_owned();
                    eprintln!("{reason}");
                    perp_runtime.bootstrap_status = journal::PerpBootstrapStatus::Blocked;
                    snapshot.plan.perp_blocked_reason = Some(reason);
                }
                run_state.perp_runtime = Some(perp_runtime.clone());
                if let Some(journal) = &journal {
                    journal.save_state(&run_state)?;
                }
            }

            // 2. Standalone orders carry no client-order ID, so ownership cannot be proven and a
            // bulk submission could silently remove a manual order — those still halt execution.
            // Levels of this (subaccount, market)'s own bulk ladder are different: only one bulk
            // ladder can exist per pair and a new submission replaces it atomically by design, so
            // they must not block the very replacement that supersedes them.
            let blocking = decibel_grid_tui::reconcile::blocking_orders(&actual);
            if !blocking.is_empty() {
                let reason = format!(
                    "{} standalone open order(s) of unprovable ownership; live replacement halted until operator review",
                    blocking.len()
                );
                println!("  {reason}");
                if let Some(journal) = &journal {
                    let event = journal::JournalEvent::RiskRejected {
                        at: Utc::now(),
                        reason,
                    };
                    journal.append(&event)?;
                    run_state.apply(&event);
                    journal.save_state(&run_state)?;
                }
            } else if !reconcile_result.missing.is_empty() {
                // 3. Submit the FULL desired plan. The Decibel bulk ABI atomically replaces the
                // entire order ladder for this (subaccount, market) pair — it does not merge.

                // The Spot plan is pinned for this run. Rebalancing (IOC) already ran earlier in
                // the cycle when inventory was short, so this is the post-rebalance state.
                let mut exec_plan = snapshot.plan.clone();

                if snapshot.market.product == Product::Perp
                    && execute
                    && !exec_plan.paused_by_out_of_range
                    && !reconcile_result.missing.is_empty()
                {
                    if perp_runtime.bootstrap_status == journal::PerpBootstrapStatus::Completed
                        && !decibel_grid_tui::perp_position_is_safe(
                            snapshot.account.position.size,
                            &exec_plan,
                            &config,
                        )
                    {
                        match exec_plan.target_position {
                            Some(target)
                                if decibel_grid_tui::strategy::perp::risk::perp_bootstrap_target_is_safe(
                                    &config, target,
                                ) =>
                            {
                                println!(
                                    "Perp grid re-entry: position {} cannot safely support the replacement ladder; converging to target {target}.",
                                    snapshot.account.position.size
                                );
                                // A replacement is about to create a new full ladder. Reuse the
                                // guarded bootstrap path to establish the ladder's initial state.
                                perp_runtime.bootstrap_status = journal::PerpBootstrapStatus::Pending;
                                perp_runtime.bootstrap_target_position = Some(target);
                            }
                            Some(target) => {
                                exec_plan.perp_blocked_reason = Some(format!(
                                    "Perp grid re-entry blocked: target position {target} exceeds GRID_MAX_POSITION"
                                ));
                            }
                            None => {
                                exec_plan.perp_blocked_reason = Some(
                                    "Perp grid re-entry blocked: replacement plan has no target position"
                                        .to_owned(),
                                );
                            }
                        }
                    }
                    match perp_runtime.bootstrap_status {
                        journal::PerpBootstrapStatus::Pending => {
                            let Some(target) = perp_runtime.bootstrap_target_position else {
                                let reason =
                                    "Perp bootstrap blocked: locked target is missing".to_owned();
                                perp_runtime.block_bootstrap();
                                exec_plan.perp_blocked_reason = Some(reason.clone());
                                eprintln!("{reason}");
                                perp_runtime.accounting = perp_accounting.clone();
                                run_state.perp_runtime = Some(perp_runtime.clone());
                                if let Some(journal) = &journal {
                                    journal.save_state(&run_state)?;
                                }
                                continue;
                            };
                            let bootstrap_state = decibel_grid_tui::strategy::perp::convergence::perp_convergence_plan(
                                snapshot.account.position.size,
                                target,
                                snapshot.market.lot_size,
                            );
                            if !perp_runtime.requires_bootstrap_convergence(
                                snapshot.account.position.size,
                                snapshot.market.lot_size,
                            ) {
                                println!(
                                    "Perp bootstrap completed without market convergence: position {} already matches locked target {}.",
                                    bootstrap_state.current, target
                                );
                                perp_runtime.complete_bootstrap();
                            } else {
                                let mut bootstrap_plan = exec_plan.clone();
                                bootstrap_plan.target_position = Some(target);
                                bootstrap_plan.convergence_delta = Some(bootstrap_state.delta);
                                let side = if bootstrap_state.delta > Decimal::ZERO {
                                    "buy"
                                } else {
                                    "sell"
                                };
                                let intent_event = journal::JournalEvent::BootstrapIntentRecorded {
                                    at: Utc::now(),
                                    target: target.to_string(),
                                    delta: bootstrap_state.delta.to_string(),
                                    side: side.to_owned(),
                                };
                                if let Some(journal) = &journal {
                                    journal.append(&intent_event)?;
                                    run_state.apply(&intent_event);
                                    journal.save_state(&run_state)?;
                                }
                                match decibel_grid_tui::strategy::perp::runtime::run_perp_convergence(
                                    &settings.network,
                                    &api,
                                    &settings.aptos_private_key,
                                    &settings.subaccount,
                                    &snapshot.market,
                                    &bootstrap_plan,
                                    &config.spot,
                                    gas_station,
                                    Some(&ws_state),
                                )
                                .await
                                {
                                    Ok(convergence) => {
                                        println!(
                                            "Perp bootstrap convergence: position {} -> locked target {} (delta {})",
                                            convergence.current, convergence.target, convergence.delta
                                        );
                                        let converged_event =
                                            journal::JournalEvent::BootstrapConverged {
                                                at: Utc::now(),
                                                target: target.to_string(),
                                                position_before: convergence.current.to_string(),
                                                position_after: convergence
                                                    .current
                                                    .to_string(),
                                            };
                                        if let Some(journal) = &journal {
                                            journal.append(&converged_event)?;
                                            run_state.apply(&converged_event);
                                            journal.save_state(&run_state)?;
                                        }
                                        perp_runtime.complete_bootstrap();
                                        let account = api
                                            .account(Some(&settings.subaccount), &snapshot.market)
                                            .await?;
                                        exec_plan =
                                            decibel_grid_tui::strategy::perp::runtime::finalize_perp_executable_plan(
                                                &config,
                                                exec_plan,
                                                account.position.size,
                                                account.available_margin,
                                            )?;
                                        snapshot.account = account;
                                    }
                                    Err(error) => {
                                        if decibel_grid_tui::strategy::perp::convergence::preflight_failure_is_retryable(&error) {
                                            let reason = format!(
                                                "Perp bootstrap waiting for a safe executable book: {error:#}"
                                            );
                                            if last_perp_preflight_deferral.as_deref()
                                                != Some(reason.as_str())
                                            {
                                                eprintln!("{reason}");
                                            }
                                            last_perp_preflight_deferral = Some(reason.clone());
                                            exec_plan.perp_blocked_reason = Some(reason);
                                            perp_runtime.accounting = perp_accounting.clone();
                                            run_state.perp_runtime = Some(perp_runtime.clone());
                                            if let Some(journal) = &journal {
                                                journal.save_state(&run_state)?;
                                            }
                                            tokio::time::sleep(config.refresh).await;
                                            continue;
                                        }
                                        last_perp_preflight_deferral = None;
                                        let reason = format!("Perp bootstrap blocked: convergence failed: {error:#}");
                                        perp_runtime.block_bootstrap();
                                        eprintln!("{reason}");
                                        if let Some(journal) = &journal {
                                            let failed_event =
                                                journal::JournalEvent::BootstrapFailed {
                                                    at: Utc::now(),
                                                    target: target.to_string(),
                                                    reason: reason.clone(),
                                                };
                                            journal.append(&failed_event)?;
                                            run_state.apply(&failed_event);
                                        }
                                        perp_runtime.accounting = perp_accounting.clone();
                                        run_state.perp_runtime = Some(perp_runtime.clone());
                                        if let Some(journal) = &journal {
                                            journal.save_state(&run_state)?;
                                        }
                                        continue;
                                    }
                                }
                            }
                            perp_runtime.accounting = perp_accounting.clone();
                            run_state.perp_runtime = Some(perp_runtime.clone());
                            if let Some(journal) = &journal {
                                journal.save_state(&run_state)?;
                            }
                        }
                        journal::PerpBootstrapStatus::Completed => {
                            println!(
                                "Perp grid replacement: no market convergence after completed bootstrap."
                            );
                        }
                        journal::PerpBootstrapStatus::Blocked
                        | journal::PerpBootstrapStatus::LegacyUnknown => {
                            let reason = "Perp bootstrap blocked; refusing automatic grid submission until operator restarts or explicitly retries".to_owned();
                            exec_plan.perp_blocked_reason = Some(reason.clone());
                        }
                    }
                } // end Perp convergence block

                // Spot bulk orders source PFS only. On replacement, the existing bulk escrow is
                // credited by the Move entry function, so the currently reserved side counts
                // toward what the replacement can fund.
                let mut spot_underfunded = None;
                if snapshot.market.product == Product::Spot
                    && let Some(funds) = &snapshot.account.spot_funds
                    && (funds.available_quote_for_bulk() < exec_plan.quote_required
                        || decibel_grid_tui::spot_base_shortfall(
                            &exec_plan,
                            funds,
                            &snapshot.market,
                        )
                        .is_some())
                {
                    let account = match api
                        .account(Some(&settings.subaccount), &snapshot.market)
                        .await
                    {
                        Ok(account) => Some(account),
                        Err(error) => {
                            eprintln!(
                                "could not refresh Spot funds before underfunded submission check; retaining conservative skip: {error:#}"
                            );
                            None
                        }
                    };
                    let fresh_funds = account
                        .and_then(|a| a.spot_funds)
                        .unwrap_or_else(|| funds.clone());
                    if fresh_funds.available_quote_for_bulk() < exec_plan.quote_required
                        || decibel_grid_tui::spot_base_shortfall(
                            &exec_plan,
                            &fresh_funds,
                            &snapshot.market,
                        )
                        .is_some()
                    {
                        // Never shrink a pinned ladder to fit: that silently converts the
                        // configured grid into a different, narrower one. Skip the submission
                        // and report exactly which asset must be topped up.
                        spot_underfunded = Some(format!(
                            "quote needs {} (available {}), base needs {} (available {})",
                            exec_plan.quote_required,
                            fresh_funds.available_quote_for_bulk(),
                            exec_plan.base_required,
                            fresh_funds.available_base_for_bulk()
                        ));
                    }
                }

                if let Some(shortfall) = spot_underfunded {
                    consecutive_local_preflight_rejections =
                        consecutive_local_preflight_rejections.saturating_add(1);
                    eprintln!(
                        "  LOCAL PFS PRECHECK REJECTED ({}/{}): {shortfall}",
                        consecutive_local_preflight_rejections, config.spot.entry_exit_max_attempts
                    );
                    if consecutive_local_preflight_rejections >= config.spot.entry_exit_max_attempts
                    {
                        let reason = format!(
                            "{} consecutive local PFS precheck rejections; inspect funding and bootstrap logic",
                            consecutive_local_preflight_rejections
                        );
                        eprintln!(
                            "LOCAL PRECHECK CIRCUIT BREAKER: {reason}; pausing automatic replacement."
                        );
                        paused_by_failure_circuit = true;
                        if let Some(journal) = &journal {
                            let event = journal::JournalEvent::RiskRejected {
                                at: Utc::now(),
                                reason,
                            };
                            journal.append(&event)?;
                            run_state.apply(&event);
                            journal.save_state(&run_state)?;
                        }
                    }
                    if let Some(journal) = &journal {
                        let event = journal::JournalEvent::RiskRejected {
                            at: Utc::now(),
                            reason: format!("pinned Spot grid underfunded: {shortfall}"),
                        };
                        journal.append(&event)?;
                        run_state.apply(&event);
                        journal.save_state(&run_state)?;
                    }
                } else {
                    consecutive_local_preflight_rejections = 0;
                    if exec_plan.bids.is_empty() && exec_plan.asks.is_empty() {
                        println!("  No levels can be placed (budget exhausted).");
                    } else {
                        let desired_level_count = exec_plan.bids.len() + exec_plan.asks.len();
                        let structural_change = new_trade_observed
                            || !reconcile_result.missing.is_empty()
                            || last_submitted_level_count
                                .is_none_or(|previous| previous != desired_level_count);
                        let cooldown_active = last_bulk_replacement_at.is_some_and(|submitted| {
                            submitted.elapsed() < BULK_REPLACEMENT_COOLDOWN
                        });
                        if cooldown_active && !structural_change {
                            println!(
                                "  bulk replacement skipped: minor ladder drift during {}s cooldown ({} desired levels; no level-count change)",
                                BULK_REPLACEMENT_COOLDOWN.as_secs(),
                                desired_level_count
                            );
                        } else if snapshot.market.product == Product::Perp
                            && let Some(reason) =
                                decibel_grid_tui::strategy::perp::runtime::perp_submission_blocked(
                                    &config,
                                    &exec_plan,
                                    snapshot.account.position.size,
                                    snapshot.account.available_margin,
                                    snapshot.market.lot_size,
                                    perp_runtime.bootstrap_status
                                        == journal::PerpBootstrapStatus::Pending,
                                )
                        {
                            if last_perp_submission_block_reason.as_deref() != Some(reason.as_str())
                            {
                                decibel_grid_tui::strategy::perp::runtime::record_perp_risk_rejection(
                                    reason.clone(),
                                    journal.as_ref(),
                                    &mut run_state,
                                )?;
                                last_perp_submission_block_reason = Some(reason);
                            }
                        } else if run_state.bulk_ladder.as_ref().is_some_and(|ladder| {
                            matches!(
                                ladder.state,
                                journal::BulkLadderState::IntentRecorded
                                    | journal::BulkLadderState::BroadcastPending
                                    | journal::BulkLadderState::BroadcastUnknown
                                    | journal::BulkLadderState::Committed
                                    | journal::BulkLadderState::CancelPending
                                    | journal::BulkLadderState::Diverged
                            )
                        }) {
                            let ladder = run_state.bulk_ladder.clone().expect("checked above");
                            if ladder.state == journal::BulkLadderState::Diverged
                                && last_diverged_bulk_recovery_at.is_none_or(|last| {
                                    last.elapsed() >= DIVERGED_BULK_RECOVERY_INTERVAL
                                })
                            {
                                last_diverged_bulk_recovery_at = Some(tokio::time::Instant::now());
                                match api
                                    .active_bulk_ladder(&settings.subaccount, &snapshot.market)
                                    .await
                                {
                                    Ok(Some(active)) if active.matches(&ladder) => {
                                        ws_state::recover_bulk_ladder(&ws_state, Some(active));
                                        let observed = journal::JournalEvent::BulkVenueObserved {
                                            at: Utc::now(),
                                            operation_id: ladder.operation_id.clone(),
                                        };
                                        if let Some(journal) = &journal {
                                            journal.append(&observed)?;
                                            run_state.apply(&observed);
                                            journal.save_state(&run_state)?;
                                        }
                                        last_bulk_lifecycle_blocked_operation = None;
                                        println!(
                                            "Bulk lifecycle recovered: operation {} now matches the exchange ladder.",
                                            ladder.operation_id
                                        );
                                    }
                                    Ok(_) | Err(_) => {
                                        if last_bulk_lifecycle_blocked_operation.as_deref()
                                            != Some(ladder.operation_id.as_str())
                                        {
                                            eprintln!(
                                                "BULK LIFECYCLE BLOCKED: operation {} is Diverged; waiting for targeted REST recovery before another replacement.",
                                                ladder.operation_id
                                            );
                                            last_bulk_lifecycle_blocked_operation =
                                                Some(ladder.operation_id.clone());
                                        }
                                    }
                                }
                            } else if ladder.state != journal::BulkLadderState::Diverged
                                && last_bulk_lifecycle_blocked_operation.as_deref()
                                    != Some(ladder.operation_id.as_str())
                            {
                                eprintln!(
                                    "BULK LIFECYCLE BLOCKED: operation {} is {:?}; refusing another replacement until recovery resolves it",
                                    ladder.operation_id, ladder.state
                                );
                                last_bulk_lifecycle_blocked_operation =
                                    Some(ladder.operation_id.clone());
                            }
                        } else {
                            last_perp_submission_block_reason = None;
                            last_bulk_lifecycle_blocked_operation = None;

                            // ---- pre-submission race guard ----
                            // A resting entry may have filled between risk evaluation and this
                            // point. Re-read the account position and active ladder, then
                            // rebuild and re-check worst-case exposure before signing.
                            if snapshot.market.product == Product::Perp {
                                match api
                                    .account(Some(&settings.subaccount), &snapshot.market)
                                    .await
                                {
                                    Ok(refreshed) => {
                                        let latest_position = refreshed.position.size;
                                        let latest_margin = refreshed.available_margin;
                                        if (latest_position - snapshot.account.position.size).abs()
                                            > snapshot.market.lot_size
                                        {
                                            println!(
                                                "Pre-submit position changed: {} → {}. Rebuilding plan.",
                                                snapshot.account.position.size, latest_position
                                            );
                                        }
                                        snapshot.account = refreshed;
                                        exec_plan = decibel_grid_tui::strategy::perp::runtime::finalize_perp_executable_plan(
                                            &config,
                                            exec_plan.clone(),
                                            latest_position,
                                            latest_margin,
                                        )?;
                                        // Re-check submission gates.
                                        if let Some(reason) =
                                            decibel_grid_tui::strategy::perp::runtime::perp_submission_blocked(
                                                &config,
                                                &exec_plan,
                                                latest_position,
                                                latest_margin,
                                                snapshot.market.lot_size,
                                                perp_runtime.bootstrap_status
                                                    == journal::PerpBootstrapStatus::Pending,
                                            )
                                        {
                                            eprintln!("PRE-SUBMIT BLOCKED: {reason}");
                                            decibel_grid_tui::strategy::perp::runtime::record_perp_risk_rejection(
                                                reason.clone(),
                                                journal.as_ref(),
                                                &mut run_state,
                                            )?;
                                            last_perp_submission_block_reason = Some(reason);
                                            tokio::time::sleep(config.refresh).await;
                                            continue;
                                        }
                                    }
                                    Err(error) => {
                                        eprintln!(
                                            "PRE-SUBMIT account refresh failed: {error:#}; submitting with stale position {}",
                                            snapshot.account.position.size
                                        );
                                    }
                                }
                            }

                            let observed = ws_state::active_bulk_ladder(&ws_state)?;
                            let sequence = ws_state::next_bulk_sequence(&ws_state)?;
                            let intent = bulk_ladder_intent(
                                format!(
                                    "{}:{}:{}",
                                    snapshot.market.address,
                                    sequence,
                                    Utc::now().timestamp_millis()
                                ),
                                snapshot.market.product,
                                snapshot.market.address.clone(),
                                sequence,
                                observed.as_ref().map(|ladder| ladder.sequence),
                                &exec_plan,
                                &snapshot.market,
                            )?;
                            let operation_id = intent.operation_id.clone();
                            let jrnl = journal.as_ref().ok_or_else(|| {
                                anyhow::anyhow!(
                                    "live bulk execution requires a durable journal before broadcast"
                                )
                            })?;
                            let intent_event = journal::JournalEvent::BulkIntentRecorded {
                                at: Utc::now(),
                                ladder: intent.clone(),
                            };
                            jrnl.append(&intent_event)?;
                            run_state.apply(&intent_event);
                            jrnl.save_state(&run_state)?;

                            match execute_bulk_grid_with_broadcast(
                                BulkGridExecutionRequest {
                                    network: &settings.network,
                                    api_key: &settings.api_key,
                                    private_key: &settings.aptos_private_key,
                                    subaccount: &settings.subaccount,
                                    market: &snapshot.market,
                                    plan: &exec_plan,
                                    expected_sequence: Some(sequence),
                                    gas_station,
                                },
                                |transaction_hash| {
                                    let broadcast = journal::JournalEvent::BulkBroadcast {
                                        at: Utc::now(),
                                        operation_id: operation_id.clone(),
                                        transaction_hash: transaction_hash.to_owned(),
                                    };
                                    jrnl.append(&broadcast)?;
                                    run_state.apply(&broadcast);
                                    jrnl.save_state(&run_state)
                                },
                            )
                            .await
                            {
                                Ok(execution) => {
                                    const WS_OBSERVATION_WAIT_MS: &[u64] =
                                        &[1_000, 2_000, 4_000, 8_000];
                                    let mut venue_observed = false;
                                    for delay_ms in WS_OBSERVATION_WAIT_MS {
                                        tokio::time::sleep(Duration::from_millis(*delay_ms)).await;
                                        check_cancel!();
                                        // Fast path: WS event arrived.
                                        if ws_state::active_bulk_ladder(&ws_state)?
                                            .is_some_and(|active| active.matches(&intent))
                                        {
                                            venue_observed = true;
                                            break;
                                        }
                                        // Slow path: REST authoritative ladder present.
                                        if let Some(active) = api
                                            .active_bulk_ladder(
                                                &settings.subaccount,
                                                &snapshot.market,
                                            )
                                            .await?
                                            .filter(|active| active.matches(&intent))
                                        {
                                            ws_state::recover_bulk_ladder(&ws_state, Some(active));
                                            venue_observed = true;
                                            break;
                                        }
                                    }
                                    if !venue_observed {
                                        let blocked = journal::JournalEvent::BulkLifecycleBlocked {
                                            at: Utc::now(),
                                            operation_id,
                                            reason: format!(
                                                "transaction {} committed but /bulk_orders did not expose expected sequence {} and levels after {} total observation",
                                                execution.transaction_hash,
                                                execution.bulk_sequence,
                                                WS_OBSERVATION_WAIT_MS.iter().sum::<u64>()
                                            ),
                                        };
                                        jrnl.append(&blocked)?;
                                        run_state.apply(&blocked);
                                        jrnl.save_state(&run_state)?;
                                        eprintln!(
                                            "BULK LIFECYCLE BLOCKED: committed ladder was not observed after ~{}s; automatic replacement paused.",
                                            WS_OBSERVATION_WAIT_MS.iter().sum::<u64>() / 1000
                                        );
                                        continue;
                                    }
                                    let observed_event = journal::JournalEvent::BulkVenueObserved {
                                        at: Utc::now(),
                                        operation_id,
                                    };
                                    jrnl.append(&observed_event)?;
                                    run_state.apply(&observed_event);
                                    jrnl.save_state(&run_state)?;
                                    consecutive_bulk_failures = 0;
                                    last_bulk_replacement_at = Some(tokio::time::Instant::now());
                                    last_submitted_level_count =
                                        Some(execution.bid_count + execution.ask_count);
                                    println!(
                                        "  FULL ladder replaced: {} bid(s), {} ask(s) in tx {}",
                                        execution.bid_count,
                                        execution.ask_count,
                                        execution.transaction_hash
                                    );
                                    if let Some(runtime) = &engine_runtime {
                                        let ladder =
                                            decibel_grid_tui::control::ladder_from_submitted_plan(
                                                &exec_plan,
                                            );
                                        runtime
                                            .update_status(|status| {
                                                status.ladder = ladder;
                                            })
                                            .await;
                                    }
                                    let event = journal::JournalEvent::BulkOrderSubmitted {
                                        at: Utc::now(),
                                        transaction_hash: execution.transaction_hash,
                                        bid_count: execution.bid_count,
                                        ask_count: execution.ask_count,
                                    };
                                    jrnl.append(&event)?;
                                    run_state.apply(&event);
                                    jrnl.save_state(&run_state)?;
                                }
                                Err(error) => {
                                    // `execute_bulk_grid` may fail after a broadcast timeout, so
                                    // retrying with another sequence could create two ladders. Keep
                                    // the intent durable and require recovery to prove the outcome.
                                    let blocked = journal::JournalEvent::BulkLifecycleBlocked {
                                        at: Utc::now(),
                                        operation_id,
                                        reason: format!(
                                            "bulk submission outcome is unresolved after intent was recorded: {error:#}"
                                        ),
                                    };
                                    jrnl.append(&blocked)?;
                                    run_state.apply(&blocked);
                                    jrnl.save_state(&run_state)?;
                                    consecutive_bulk_failures =
                                        consecutive_bulk_failures.saturating_add(1);
                                    eprintln!(
                                        "  bulk order failed ({}/{}): {error:#}",
                                        consecutive_bulk_failures,
                                        config.spot.max_consecutive_bulk_failures
                                    );
                                    let event = journal::JournalEvent::BulkOrderFailed {
                                        at: Utc::now(),
                                        error: format!("{error:#}"),
                                    };
                                    jrnl.append(&event)?;
                                    run_state.apply(&event);
                                    jrnl.save_state(&run_state)?;
                                    if consecutive_bulk_failures
                                        >= config.spot.max_consecutive_bulk_failures
                                    {
                                        let reason = format!(
                                            "{} consecutive bulk replacement failures",
                                            consecutive_bulk_failures
                                        );
                                        eprintln!(
                                            "FAILURE CIRCUIT BREAKER: {reason}; cancelling ladder and pausing."
                                        );
                                        let event = journal::JournalEvent::RiskRejected {
                                            at: Utc::now(),
                                            reason,
                                        };
                                        jrnl.append(&event)?;
                                        run_state.apply(&event);
                                        jrnl.save_state(&run_state)?;
                                        match journaled_bulk_cancel(
                                            journal.as_ref(),
                                            &mut run_state,
                                            &settings.network,
                                            &settings.aptos_private_key,
                                            &settings.subaccount,
                                            &snapshot.market,
                                            gas_station,
                                        )
                                        .await
                                        {
                                            Ok(Some(hash)) => println!(
                                                "Failure-circuit cancellation submitted in tx {hash}"
                                            ),
                                            Ok(None) => {}
                                            Err(cancel_error) => eprintln!(
                                                "Failure-circuit cancellation failed: {cancel_error:#}"
                                            ),
                                        }
                                        paused_by_failure_circuit = true;
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        let elapsed = cycle_start.elapsed();
        let interval = if config.product == Product::Spot {
            config.spot.reconciliation_interval
        } else {
            config.refresh
        };
        let wait = interval.saturating_sub(elapsed);
        if execute {
            let sleep = tokio::time::sleep(wait);
            tokio::pin!(sleep);
            loop {
                tokio::select! {
                    _ = &mut sleep => break,
                    event = ws_lifecycle_rx.recv() => match event {
                        Ok(ws_state::WsLifecycleEvent::BulkFillApplied(fill)) => {
                            println!("Bulk fill {} at {}; reconciling immediately.", fill.size, fill.price);
                            if let Some(runtime) = &engine_runtime {
                                let message = format!("bulk fill: {} at {}", fill.size, fill.price);
                                runtime.update_status(|status| {
                                    status.events.push(decibel_grid_tui::control::EngineEvent { at: Utc::now(), message });
                                    if status.events.len() > 200 { status.events.drain(..status.events.len() - 200); }
                                }).await;
                            }
                            if let Some(journal) = &journal {
                                let event = journal::JournalEvent::BulkLevelFilled {
                                    at: Utc::now(),
                                    trade_id: fill.trade_id,
                                    event_uid: fill.event_uid.clone(),
                                    sequence: fill.sequence,
                                    side: fill.side,
                                    price: fill.price,
                                    size: fill.size,
                                };
                                journal.append(&event)?;
                                run_state.apply(&event);
                                journal.save_state(&run_state)?;
                            }
                            break;
                        }
                        Ok(ws_state::WsLifecycleEvent::TradeApplied | ws_state::WsLifecycleEvent::BulkOrderChanged | ws_state::WsLifecycleEvent::OrderChanged) => {
                            break;
                        }
                        Ok(ws_state::WsLifecycleEvent::Desynced(reason)) => {
                            eprintln!("WebSocket state desynced: {reason}; recovery cycle required.");
                            break;
                        }
                        Ok(ws_state::WsLifecycleEvent::Ready | ws_state::WsLifecycleEvent::Reconnected | ws_state::WsLifecycleEvent::Disconnected) => break,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
        } else {
            tokio::time::sleep(wait).await;
        }
        check_cancel!();
    }

    let exit_policy = engine_runtime
        .as_ref()
        .and_then(|runtime| runtime.requested_exit_mode())
        .map(|mode| match mode {
            control::ExitMode::Hold => ExitAssetPolicy::Retain,
            control::ExitMode::Liquidate => ExitAssetPolicy::Sell,
        })
        .unwrap_or(settings.exit_asset_policy);
    if paused_by_breakout || paused_by_failure_circuit {
        println!(
            "Risk pause complete: ladder cancellation was attempted; assets were not liquidated."
        );
    } else if stop_loss_liquidated {
        println!("Stop-loss already liquidated this market; skipping the exit sell policy.");
    } else if execute {
        if let Some(market) = last_market {
            match exit_policy {
                ExitAssetPolicy::Sell => {
                    println!(
                        "Exit policy is SELL: cancelling the ladder and liquidating assets..."
                    );
                    if market.product == Product::Perp {
                        match decibel_grid_tui::strategy::perp::runtime::cancel_and_flatten_perp(
                            &settings.network,
                            &api,
                            &settings.aptos_private_key,
                            &settings.subaccount,
                            &market,
                            &config.spot,
                            gas_station,
                        )
                        .await
                        {
                            Ok(result) => println!(
                                "Perp exit completed: cancelled {} and position {} -> {}",
                                result.cancel_transaction_hash,
                                result.position_before,
                                result.position_after
                            ),
                            Err(error) => eprintln!("Perp exit failed: {error:#}"),
                        }
                    } else {
                        match exit_sell_assets(
                            &settings.network,
                            &settings.api_key,
                            &settings.aptos_private_key,
                            &settings.subaccount,
                            &market,
                            Some((
                                &config.spot,
                                spot_fee_rates
                                    .as_ref()
                                    .expect("live Spot execution fetched fee rates"),
                            )),
                            gas_station,
                        )
                        .await
                        {
                            Ok(hashes) => println!(
                                "Exit cleanup completed: {} transaction(s): {:?}",
                                hashes.len(),
                                hashes
                            ),
                            Err(error) => eprintln!("Exit cleanup failed: {error:#}"),
                        }
                    }
                }
                ExitAssetPolicy::Retain => {
                    println!(
                        "Exit policy is RETAIN: cancelling the ladder and retaining released assets."
                    );
                    match journaled_bulk_cancel(
                        journal.as_ref(),
                        &mut run_state,
                        &settings.network,
                        &settings.aptos_private_key,
                        &settings.subaccount,
                        &market,
                        gas_station,
                    )
                    .await
                    {
                        Ok(Some(hash)) => {
                            println!("Bulk ladder cancelled in tx {hash}; assets retained.")
                        }
                        Ok(None) => {}
                        Err(error) => eprintln!(
                            "Bulk cancellation failed; ladder may still be live: {error:#}"
                        ),
                    }
                }
            }
        } else {
            println!("No market snapshot was loaded; no ladder lifecycle action was sent.");
        }
        if let Some(journal) = journal.as_ref() {
            let shutdown_event = decibel_grid_tui::journal::JournalEvent::Shutdown {
                at: Utc::now(),
                reason: format!(
                    "engine stopped (exit policy: {})",
                    match engine_runtime
                        .as_ref()
                        .and_then(|r| r.requested_exit_mode())
                    {
                        Some(decibel_grid_tui::control::ExitMode::Hold) => "retain",
                        Some(decibel_grid_tui::control::ExitMode::Liquidate) => "sell",
                        None => "terminated",
                    }
                ),
            };
            let _ = journal.append(&shutdown_event);
            let _ = journal.save_state(&run_state);
        }
    }
    Ok(())
}
