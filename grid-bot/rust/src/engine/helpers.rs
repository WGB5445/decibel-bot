use decibel_grid_tui::*;
use rust_decimal::Decimal;

use crate::cli::settings::Settings;

pub fn perp_pnl_status(
    accounting: &strategy::perp::accounting::PerpAccounting,
    exchange_position: Decimal,
    mark_price: Option<Decimal>,
) -> control::PerpPnlStatus {
    let snapshot = accounting.pnl_snapshot(exchange_position, mark_price);
    control::PerpPnlStatus {
        exchange_position_base: snapshot.exchange_position_base.to_string(),
        ledger_position_base: snapshot.ledger_position_base.to_string(),
        reconciliation_delta_base: snapshot.reconciliation_delta_base.to_string(),
        average_entry_price: snapshot.average_entry_price.map(|v| v.to_string()),
        mark_price: snapshot.mark_price.map(|v| v.to_string()),
        unrealized_gross_quote: snapshot.unrealized_gross_quote.map(|v| v.to_string()),
        realized_gross_quote: snapshot.realized_gross_quote.to_string(),
        trade_fees_quote: snapshot.trade_fees_quote.map(|v| v.to_string()),
        funding_pnl_quote: snapshot.funding_pnl_quote.map(|v| v.to_string()),
        net_pnl_quote: snapshot.net_pnl_quote.map(|v| v.to_string()),
        fees_complete: accounting.fees_complete,
        funding_complete: accounting.funding_complete,
        last_fill_at: accounting.last_fill_at,
        last_funding_at: accounting.last_funding_at,
    }
}

pub fn print_snapshot(snapshot: &MonitorSnapshot, config: &GridConfig) {
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

pub fn optional_subaccount(settings: &Settings) -> Option<&str> {
    (!settings.subaccount.trim().is_empty()).then_some(settings.subaccount.as_str())
}

pub fn engine_phase(
    ws_state: &ws_state::WsStateHandle,
    run_state: &journal::RunState,
) -> control::EnginePhase {
    if !ws_state::can_execute(ws_state) {
        let state = ws_state.read().expect("WS state lock poisoned");
        if !state.subscriptions_ready {
            return control::EnginePhase::Connecting;
        }
        if state.orders_desynced || state.bulk_ladder_desynced {
            return control::EnginePhase::Desynced;
        }
        return control::EnginePhase::Hydrating;
    }
    if run_state.bulk_ladder.as_ref().is_some_and(|ladder| {
        matches!(
            ladder.state,
            journal::BulkLadderState::IntentRecorded
                | journal::BulkLadderState::BroadcastPending
                | journal::BulkLadderState::BroadcastUnknown
                | journal::BulkLadderState::Committed
                | journal::BulkLadderState::Diverged
                | journal::BulkLadderState::CancelPending
        )
    }) {
        return control::EnginePhase::LifecycleBlocked;
    }
    control::EnginePhase::Ready
}