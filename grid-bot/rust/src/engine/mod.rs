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
use tokio::sync::mpsc;

pub(crate) fn print_snapshot(snapshot: &MonitorSnapshot, config: &GridConfig) {
    let profit = snapshot.plan.profit_preview(config.maker_fee_rate);
    println!(
        "{} {:?} mid={} net-scenario={}",
        snapshot.market.name, snapshot.market.product, snapshot.plan.mid, profit.net_capture
    );
    for level in snapshot.plan.all_levels() {
        println!(
            "{:3} {:>12} × {:>10} {:?}",
            level.side.as_str(),
            format_decimal(level.price, 8),
            format_decimal(level.size, 8),
            level.state
        );
    }
}

pub(crate) fn optional_subaccount(settings: &Settings) -> Option<&str> {
    (!settings.subaccount.trim().is_empty()).then_some(settings.subaccount.as_str())
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
    let run_id = if execute && config.product == Product::Spot {
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
            "Recovered durable Spot grid state for run {run_id}; reconciling exchange state before replacement."
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
    // A small mid-price move can change many quantized levels and otherwise cause a full bulk
    // replacement every refresh. Keep a short cooldown for same-sized ladders; initial placement
    // and structural changes (level count changes, fills, funding/affordability changes) bypass it.
    const BULK_REPLACEMENT_COOLDOWN: Duration = Duration::from_secs(30);
    let mut last_bulk_replacement_at: Option<tokio::time::Instant> = None;
    // Total resting levels (bid_count + ask_count) from the last submitted bulk ladder. A
    // changed level count means inventory/affordability moved (a fill, funding, or a PFS-driven
    // shrink), which must replace immediately regardless of the cooldown.
    let mut last_submitted_level_count: Option<usize> = None;
    let mut out_of_range_handled = false;
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
    let (spot_event_tx, mut spot_event_rx) = mpsc::channel(256);
    let _spot_event_listener =
        if execute && config.product == Product::Spot && !settings.subaccount.trim().is_empty() {
            let market = api
                .market(&config.market_name, Product::Spot)
                .await
                .context("resolve Spot market before subscribing to lifecycle events")?;
            Some(events::spawn_spot_event_listener(
                api.clone(),
                market.address,
                settings.subaccount.clone(),
                config.spot.ws_reconnect_backoff.clone(),
                spot_event_tx,
                Arc::clone(&cancel),
            ))
        } else {
            None
        };
    macro_rules! check_cancel {
        () => {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
        };
    }

    loop {
        let cycle_start = tokio::time::Instant::now();
        let snapshot = match fetch_snapshot(&api, &config, optional_subaccount(&settings)).await {
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
                snapshot.plan =
                    decibel_grid_tui::strategy::perp::runtime::prepare_perp_executable_plan(
                        &config,
                        snapshot.plan.clone(),
                        snapshot.account.position.size,
                        snapshot.account.available_margin,
                    )?;
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
            );
            runtime
                .update_status(|status| {
                    status.planning_price =
                        snapshot.plan.planning_price.map(|value| value.to_string());
                    status.position = Some(snapshot.account.position.size.to_string());
                    status.target_position =
                        snapshot.plan.target_position.map(|value| value.to_string());
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
        if execute {
            let actual = match api
                .open_orders(&settings.subaccount, &snapshot.market)
                .await
            {
                Ok(orders) => orders,
                Err(e) => {
                    eprintln!("reconciliation failed (open_orders): {e:#}; skipping cycle");
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

        if execute {
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

            println!("RECONCILE CYCLE — {}", reconcile_result.summary());
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
                    // Skip convergence when already at target — avoids an unnecessary
                    // order-book fetch and IOC attempt for zero-delta plans.
                    let skip_convergence = exec_plan
                        .target_position
                        .zip(Some(snapshot.account.position.size))
                        .is_some_and(|(target, current)| {
                            (target - current).abs() < snapshot.market.lot_size
                        });
                    if !skip_convergence {
                        match decibel_grid_tui::strategy::perp::runtime::run_perp_convergence(
                        &settings.network,
                        &api,
                        &settings.aptos_private_key,
                        &settings.subaccount,
                        &snapshot.market,
                        &exec_plan,
                        &config.spot,
                        gas_station,
                    )
                    .await
                    {
                        Ok(convergence) => {
                            println!(
                                "  Perp convergence: position {} -> target {} (delta {})",
                                convergence.current, convergence.target, convergence.delta
                            );
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
                            let reason = format!("Perp convergence failed: {error:#}");
                            eprintln!("  {reason}");
                            if let Some(journal) = &journal {
                                let event = journal::JournalEvent::RiskRejected {
                                    at: Utc::now(),
                                    reason: reason.clone(),
                                };
                                journal.append(&event)?;
                                run_state.apply(&event);
                                journal.save_state(&run_state)?;
                            }
                            check_cancel!();
                            tokio::time::sleep(config.refresh).await;
                            continue;
                        }
                    }
                    } // end if !skip_convergence
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
                    let account = api
                        .account(Some(&settings.subaccount), &snapshot.market)
                        .await
                        .ok();
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
                                )
                        {
                            decibel_grid_tui::strategy::perp::runtime::record_perp_risk_rejection(
                                reason,
                                journal.as_ref(),
                                &mut run_state,
                            )?;
                        } else {
                            match execute_bulk_grid(
                                &settings.network,
                                &settings.api_key,
                                &settings.aptos_private_key,
                                &settings.subaccount,
                                &snapshot.market,
                                &exec_plan,
                                gas_station,
                            )
                            .await
                            {
                                Ok(execution) => {
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
                                        let ladder = decibel_grid_tui::control::ladder_from_submitted_plan(
                                            &exec_plan,
                                        );
                                        runtime
                                            .update_status(|status| {
                                                status.ladder = ladder;
                                            })
                                            .await;
                                    }
                                    if let Some(journal) = &journal {
                                        let event = journal::JournalEvent::BulkOrderSubmitted {
                                            at: Utc::now(),
                                            transaction_hash: execution.transaction_hash,
                                            bid_count: execution.bid_count,
                                            ask_count: execution.ask_count,
                                        };
                                        journal.append(&event)?;
                                        run_state.apply(&event);
                                        journal.save_state(&run_state)?;
                                    }
                                }
                                Err(error) => {
                                    consecutive_bulk_failures =
                                        consecutive_bulk_failures.saturating_add(1);
                                    eprintln!(
                                        "  bulk order failed ({}/{}): {error:#}",
                                        consecutive_bulk_failures,
                                        config.spot.max_consecutive_bulk_failures
                                    );
                                    if let Some(journal) = &journal {
                                        let event = journal::JournalEvent::BulkOrderFailed {
                                            at: Utc::now(),
                                            error: format!("{error:#}"),
                                        };
                                        journal.append(&event)?;
                                        run_state.apply(&event);
                                        journal.save_state(&run_state)?;
                                    }
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
                                        if let Some(journal) = &journal {
                                            let event = journal::JournalEvent::RiskRejected {
                                                at: Utc::now(),
                                                reason,
                                            };
                                            journal.append(&event)?;
                                            run_state.apply(&event);
                                            journal.save_state(&run_state)?;
                                        }
                                        match spot_lifecycle::cancel_bulk_ladder(
                                            &settings.network,
                                            &settings.aptos_private_key,
                                            &settings.subaccount,
                                            &snapshot.market,
                                            gas_station,
                                        )
                                        .await
                                        {
                                            Ok(hash) => println!(
                                                "Failure-circuit cancellation submitted in tx {hash}"
                                            ),
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
        if execute && config.product == Product::Spot {
            let sleep = tokio::time::sleep(wait);
            tokio::pin!(sleep);
            loop {
                tokio::select! {
                    _ = &mut sleep => break,
                    event = spot_event_rx.recv() => match event {
                        Some(events::SpotEvent::BulkFill(fill)) => {
                            println!("Spot bulk fill {} {} at {}; reconciling immediately.", fill.size, fill.market_addr, fill.price);
                            if let Some(runtime) = &engine_runtime {
                                let message = format!("fill: {} {} at {}", fill.size, fill.market_addr, fill.price);
                                runtime.update_status(|status| {
                                    status.events.push(decibel_grid_tui::control::EngineEvent { at: Utc::now(), message });
                                    if status.events.len() > 200 { status.events.drain(..status.events.len() - 200); }
                                }).await;
                            }
                            if let Some(journal) = &journal {
                                let event = journal::JournalEvent::SpotFill {
                                    at: Utc::now(),
                                    market: fill.market_addr,
                                    price: fill.price.normalize().to_string(),
                                    size: fill.size.normalize().to_string(),
                                    side: fill.side,
                                    event_uid: fill.event_uid,
                                };
                                journal.append(&event)?;
                                run_state.apply(&event);
                                journal.save_state(&run_state)?;
                            }
                            break;
                        }
                        Some(events::SpotEvent::BulkOrderRejected(rejected)) => {
                            eprintln!("Spot bulk order rejected for {}: {}; reconciling immediately.", rejected.market_addr, rejected.reason);
                            if let Some(runtime) = &engine_runtime {
                                let message = format!("bulk rejected: {}", rejected.reason);
                                runtime.update_status(|status| {
                                    status.events.push(decibel_grid_tui::control::EngineEvent { at: Utc::now(), message });
                                    if status.events.len() > 200 { status.events.drain(..status.events.len() - 200); }
                                }).await;
                            }
                            if let Some(journal) = &journal {
                                let event = journal::JournalEvent::RiskRejected {
                                    at: Utc::now(),
                                    reason: format!("bulk order rejected for {}: {}", rejected.market_addr, rejected.reason),
                                };
                                journal.append(&event)?;
                                run_state.apply(&event);
                                journal.save_state(&run_state)?;
                            }
                            break;
                        }
                        Some(events::SpotEvent::Reconnected(reconnected)) => {
                            println!("Spot event connection re-established ({}); reconciling REST state.", reconnected.reconnect_count);
                            break;
                        }
                        Some(events::SpotEvent::Mid(_) | events::SpotEvent::Depth(_)) => {
                            // Depth/mid updates refresh the local feed but only fills, rejects,
                            // reconnects, or the periodic timer may trigger a ladder replacement.
                        }
                        None => break,
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
                    match exit_sell_assets(
                        &settings.network,
                        &settings.api_key,
                        &settings.aptos_private_key,
                        &settings.subaccount,
                        &market,
                        (market.product == Product::Spot).then(|| {
                            (
                                &config.spot,
                                spot_fee_rates
                                    .as_ref()
                                    .expect("live Spot execution fetched fee rates"),
                            )
                        }),
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
                ExitAssetPolicy::Retain => {
                    println!(
                        "Exit policy is RETAIN: cancelling the ladder and retaining released assets."
                    );
                    match spot_lifecycle::cancel_bulk_ladder(
                        &settings.network,
                        &settings.aptos_private_key,
                        &settings.subaccount,
                        &market,
                        gas_station,
                    )
                    .await
                    {
                        Ok(hash) => {
                            println!("Bulk ladder cancelled in tx {hash}; assets retained.")
                        }
                        Err(error) => eprintln!(
                            "Bulk cancellation failed; ladder may still be live: {error:#}"
                        ),
                    }
                }
            }
        } else {
            println!("No market snapshot was loaded; no ladder lifecycle action was sent.");
        }
    }
    Ok(())
}
