use super::{BulkLadder, BulkLadderState, JournalEvent, PerpBootstrapStatus, PerpRuntimeState, RunMetadata, RunState, persistent_run_id};
use chrono::Utc;
use rust_decimal_macros::dec;

#[test]
fn persistent_run_id_is_stable_and_hides_the_subaccount() {
    let first = persistent_run_id("mainnet", "0x0000AbCd", "BTC/USDC");
    let second = persistent_run_id("MAINNET", "0xabcd", "btc/usdc");
    assert_eq!(first, second);
    assert!(!first.contains("abcd"));
    assert_ne!(first, persistent_run_id("mainnet", "0xabce", "BTC/USDC"));
}

#[test]
fn fingerprint_subaccount_replaces_the_raw_address() {
    let raw = "0x0123456789abcdef";
    let mut metadata = RunMetadata {
        run_id: "run_test".to_owned(), started_at: Utc::now(),
        network: "testnet".to_owned(), subaccount: raw.to_owned(),
        market: "APT/USDC".to_owned(), product: "spot".to_owned(),
        config_hash: "config".to_owned(), program_version: "test".to_owned(),
    };
    metadata.fingerprint_subaccount();
    assert_ne!(metadata.subaccount, raw);
    assert_eq!(metadata.subaccount.len(), 64);
    assert!(metadata.subaccount.chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn legacy_perp_state_defaults_to_unknown_not_pending() {
    let state: PerpRuntimeState = serde_json::from_str(r#"{"accounting":{}}"#).unwrap();
    assert_eq!(state.bootstrap_status, PerpBootstrapStatus::LegacyUnknown);
    assert!(state.pinned_plan.is_none());
    assert_eq!(state.bootstrap_target_position, None);
    assert!(!state.requires_bootstrap_convergence(dec!(0), dec!(0.01)));
}

#[test]
fn new_perp_state_has_no_ladder_until_first_plan_is_accepted() {
    assert!(PerpRuntimeState::new().pinned_plan.is_none());
}

#[test]
fn bulk_lifecycle_events_preserve_the_unresolved_operation() {
    let metadata = RunMetadata {
        run_id: "test".to_owned(), started_at: Utc::now(),
        network: "testnet".to_owned(), subaccount: "fingerprint".to_owned(),
        market: "BTC/USD".to_owned(), product: "perp".to_owned(),
        config_hash: "config".to_owned(), program_version: "test".to_owned(),
    };
    let ladder = BulkLadder {
        operation_id: "bulk-42".to_owned(), product: crate::Product::Perp,
        market_address: "0x1".to_owned(), sequence: 42,
        prior_sequence: Some(41), levels: vec![], intent_at: Utc::now(),
        transaction_hash: None, cancel_transaction_hash: None,
        state: BulkLadderState::IntentRecorded,
    };
    let mut state = RunState::new(metadata);
    state.apply(&JournalEvent::BulkIntentRecorded { at: Utc::now(), ladder });
    state.apply(&JournalEvent::BulkBroadcast { at: Utc::now(), operation_id: "bulk-42".to_owned(), transaction_hash: "0xabc".to_owned() });
    let active = state.bulk_ladder.expect("intent is retained");
    assert_eq!(active.transaction_hash.as_deref(), Some("0xabc"));
    assert_eq!(active.state, BulkLadderState::BroadcastPending);
}

#[test]
fn bootstrap_target_is_locked_once_and_completed_never_retries() {
    let mut state = PerpRuntimeState::new();
    assert_eq!(state.lock_bootstrap_target(dec!(0.00434)), Some(dec!(0.00434)));
    assert!(state.requires_bootstrap_convergence(dec!(0), dec!(0.00001)));
    assert_eq!(state.lock_bootstrap_target(dec!(0.00403)), Some(dec!(0.00434)));
    state.complete_bootstrap();
    assert_eq!(state.bootstrap_status, PerpBootstrapStatus::Completed);
    assert!(!state.requires_bootstrap_convergence(dec!(0), dec!(0.00001)));
}

#[test]
fn blocked_bootstrap_never_retries_without_operator_action() {
    let mut state = PerpRuntimeState::new();
    state.lock_bootstrap_target(dec!(1));
    state.block_bootstrap();
    assert_eq!(state.bootstrap_status, PerpBootstrapStatus::Blocked);
    assert!(!state.requires_bootstrap_convergence(dec!(0), dec!(0.01)));
}

#[test]
fn fake_market_client_is_not_called_for_completed_grid_replacement() {
    struct FakeMarketClient { submitted_market_orders: usize }
    impl FakeMarketClient { fn submit_bootstrap_market_order(&mut self) { self.submitted_market_orders += 1; } }
    let mut state = PerpRuntimeState::new();
    state.lock_bootstrap_target(dec!(1));
    state.complete_bootstrap();
    let mut client = FakeMarketClient { submitted_market_orders: 0 };
    if state.requires_bootstrap_convergence(dec!(0), dec!(0.01)) {
        client.submit_bootstrap_market_order();
    }
    assert_eq!(client.submitted_market_orders, 0);
}