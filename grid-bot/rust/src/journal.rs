//! Durable append-only event journal for grid run state.
//!
//! This is intentionally append-only: events are never modified or deleted. Snapshots cache the
//! latest projection for fast startup; on corruption the journal is replayed to rebuild the
//! snapshot.

use std::{fs, path::PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    GridPlan, Product, Side, reconcile::ActualOrder, strategy::perp::accounting::PerpAccounting,
};
use rust_decimal::Decimal;

/// Irreversible fingerprint used in place of the raw subaccount address in durable storage,
/// so a leaked event log cannot identify the on-chain account.
fn subaccount_fingerprint(raw: &str) -> String {
    use sha3::{Digest, Sha3_256};
    hex::encode(Sha3_256::digest(raw.as_bytes()))
}

/// Unique run identifier generated once per process lifetime.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RunMetadata {
    pub run_id: String,
    pub started_at: DateTime<Utc>,
    pub network: String,
    /// Irreversible SHA3-256 fingerprint of the subaccount address. The raw address is never
    /// written to durable storage so a leaked event or state file cannot identify the account.
    pub subaccount: String,
    pub market: String,
    pub product: String,
    pub config_hash: String,
    pub program_version: String,
}

impl RunMetadata {
    /// Replaces the raw subaccount address with an irreversible fingerprint before persistence.
    pub fn fingerprint_subaccount(&mut self) {
        self.subaccount = subaccount_fingerprint(&self.subaccount);
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum JournalEvent {
    RunStart(RunMetadata),
    PlanGenerated {
        at: DateTime<Utc>,
        mid: String,
        bid_levels: usize,
        ask_levels: usize,
        quote_required: String,
        base_required: String,
    },
    ReconciliationResult {
        at: DateTime<Utc>,
        matched: usize,
        missing: usize,
        unmanaged: Vec<ActualOrder>,
        is_converged: bool,
    },
    BulkOrderSubmitted {
        at: DateTime<Utc>,
        transaction_hash: String,
        bid_count: usize,
        ask_count: usize,
    },
    /// Written and synced before signing a bulk transaction. It is the durable proof that a
    /// sequence and its exact quantized levels belong to this run, even if the process dies while
    /// the transaction is being broadcast.
    BulkIntentRecorded {
        at: DateTime<Utc>,
        ladder: BulkLadder,
    },
    BulkBroadcast {
        at: DateTime<Utc>,
        operation_id: String,
        transaction_hash: String,
    },
    BulkVenueObserved {
        at: DateTime<Utc>,
        operation_id: String,
    },
    BulkCancelIntentRecorded {
        at: DateTime<Utc>,
        operation_id: String,
    },
    BulkCancelBroadcast {
        at: DateTime<Utc>,
        operation_id: String,
        transaction_hash: String,
    },
    BulkCancelledObserved {
        at: DateTime<Utc>,
        operation_id: String,
    },
    BulkLifecycleBlocked {
        at: DateTime<Utc>,
        operation_id: String,
        reason: String,
    },
    BulkOrderFailed {
        at: DateTime<Utc>,
        error: String,
    },
    SpotFill {
        at: DateTime<Utc>,
        market: String,
        price: String,
        size: String,
        side: Option<String>,
        event_uid: String,
        #[serde(default)]
        bulk_sequence_number: Option<String>,
        #[serde(default)]
        order_id: Option<String>,
        #[serde(default)]
        fee: Option<String>,
        #[serde(default)]
        venue_timestamp: Option<String>,
    },
    RiskRejected {
        at: DateTime<Utc>,
        reason: String,
    },
    Shutdown {
        at: DateTime<Utc>,
        reason: String,
    },
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BulkLadderState {
    #[default]
    IntentRecorded,
    BroadcastPending,
    BroadcastUnknown,
    Committed,
    VenueObserved,
    CancelPending,
    CancelledObserved,
    Replaced,
    Diverged,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BulkLevelState {
    pub side: Side,
    pub index: usize,
    pub price: Decimal,
    pub original_size: Decimal,
    #[serde(default)]
    pub filled_size: Decimal,
}

/// The exact quantized ladder submitted to Decibel. `operation_id` is local and stable across
/// restarts; Decibel's bulk sequence identifies the venue generation, not individual orders.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BulkLadder {
    pub operation_id: String,
    pub product: Product,
    pub market_address: String,
    pub sequence: u64,
    pub prior_sequence: Option<u64>,
    pub levels: Vec<BulkLevelState>,
    pub intent_at: DateTime<Utc>,
    #[serde(default)]
    pub transaction_hash: Option<String>,
    #[serde(default)]
    pub cancel_transaction_hash: Option<String>,
    #[serde(default)]
    pub state: BulkLadderState,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReconciliationSummary {
    pub at: DateTime<Utc>,
    pub matched: usize,
    pub missing: usize,
    pub unmanaged: Vec<ActualOrder>,
    pub is_converged: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SpotRuntimeState {
    /// The exact pinned geometry last accepted for this strategy. Restoring it prevents a restart
    /// from silently rebuilding a different range around the then-current mid price.
    pub pinned_plan: GridPlan,
    pub last_seen_trade_ms: Option<i64>,
}

/// Whether the one-time Perp bootstrap position has been established.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PerpBootstrapStatus {
    /// Only assigned to old state files that predate bootstrap tracking. The engine must migrate
    /// it through exchange reconciliation and must never treat it as permission to send a market
    /// order.
    #[default]
    LegacyUnknown,
    Pending,
    Completed,
    Blocked,
}

/// Durable projection of fills and the one-time position bootstrap for a Perp strategy. The
/// position endpoint remains the risk authority and is reconciled before the engine submits risk.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PerpRuntimeState {
    pub accounting: PerpAccounting,
    /// The exact Perp ladder geometry accepted for this run. Keeping it durable prevents price
    /// refreshes (and process restarts) from cancelling near-fill orders to chase the market.
    #[serde(default)]
    pub pinned_plan: Option<GridPlan>,
    #[serde(default)]
    pub bootstrap_status: PerpBootstrapStatus,
    #[serde(default)]
    pub bootstrap_target_position: Option<Decimal>,
}

impl PerpRuntimeState {
    pub fn new() -> Self {
        Self {
            accounting: PerpAccounting::default(),
            pinned_plan: None,
            bootstrap_status: PerpBootstrapStatus::Pending,
            bootstrap_target_position: None,
        }
    }

    pub fn legacy_unknown() -> Self {
        Self {
            accounting: PerpAccounting::default(),
            pinned_plan: None,
            bootstrap_status: PerpBootstrapStatus::LegacyUnknown,
            bootstrap_target_position: None,
        }
    }

    /// Capture the first target only. Later grid geometry changes must not move the bootstrap
    /// position or cause a new market-order convergence.
    pub fn lock_bootstrap_target(&mut self, target: Decimal) -> Option<Decimal> {
        if self.bootstrap_status == PerpBootstrapStatus::Pending
            && self.bootstrap_target_position.is_none()
        {
            self.bootstrap_target_position = Some(target);
        }
        self.bootstrap_target_position
    }

    pub fn requires_bootstrap_convergence(&self, current: Decimal, lot_size: Decimal) -> bool {
        self.bootstrap_status == PerpBootstrapStatus::Pending
            && self
                .bootstrap_target_position
                .is_some_and(|target| (target - current).abs() >= lot_size)
    }

    pub fn complete_bootstrap(&mut self) {
        if self.bootstrap_status == PerpBootstrapStatus::Pending {
            self.bootstrap_status = PerpBootstrapStatus::Completed;
        }
    }

    pub fn block_bootstrap(&mut self) {
        if self.bootstrap_status == PerpBootstrapStatus::Pending {
            self.bootstrap_status = PerpBootstrapStatus::Blocked;
        }
    }
}

impl Default for PerpRuntimeState {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunState {
    pub metadata: RunMetadata,
    #[serde(default)]
    pub spot_runtime: Option<SpotRuntimeState>,
    #[serde(default)]
    pub perp_runtime: Option<PerpRuntimeState>,
    /// The only ladder this process is permitted to treat as bot-owned. A non-terminal state
    /// blocks another replacement until recovery proves what reached the venue.
    #[serde(default)]
    pub bulk_ladder: Option<BulkLadder>,
    /// Bounded Spot WebSocket event identities. Decibel may redeliver fills after a reconnect;
    /// applying one twice would fabricate an extra fill and trigger an unsafe replacement.
    #[serde(default)]
    pub processed_spot_fill_uids: Vec<String>,
    pub last_reconciliation: Option<ReconciliationSummary>,
    pub plan_generation: u64,
    pub submitted_orders: u64,
    pub failed_orders: u64,
    pub last_event_at: DateTime<Utc>,
}

impl RunState {
    pub fn new(metadata: RunMetadata) -> Self {
        Self {
            last_event_at: metadata.started_at,
            metadata,
            spot_runtime: None,
            perp_runtime: None,
            bulk_ladder: None,
            processed_spot_fill_uids: Vec::new(),
            last_reconciliation: None,
            plan_generation: 0,
            submitted_orders: 0,
            failed_orders: 0,
        }
    }

    pub fn apply(&mut self, event: &JournalEvent) {
        self.last_event_at = Utc::now();
        match event {
            JournalEvent::PlanGenerated { .. } => self.plan_generation += 1,
            JournalEvent::BulkOrderSubmitted { .. } => self.submitted_orders += 1,
            JournalEvent::BulkIntentRecorded { ladder, .. } => {
                self.bulk_ladder = Some(ladder.clone());
            }
            JournalEvent::BulkBroadcast {
                operation_id,
                transaction_hash,
                ..
            } => {
                if let Some(ladder) = self.bulk_ladder.as_mut()
                    && ladder.operation_id == *operation_id
                {
                    ladder.transaction_hash = Some(transaction_hash.clone());
                    ladder.state = BulkLadderState::BroadcastPending;
                }
            }
            JournalEvent::BulkVenueObserved { operation_id, .. } => {
                if let Some(ladder) = self.bulk_ladder.as_mut()
                    && ladder.operation_id == *operation_id
                {
                    ladder.state = BulkLadderState::VenueObserved;
                }
            }
            JournalEvent::BulkCancelIntentRecorded { operation_id, .. } => {
                if let Some(ladder) = self.bulk_ladder.as_mut()
                    && ladder.operation_id == *operation_id
                {
                    ladder.state = BulkLadderState::CancelPending;
                }
            }
            JournalEvent::BulkCancelBroadcast {
                operation_id,
                transaction_hash,
                ..
            } => {
                if let Some(ladder) = self.bulk_ladder.as_mut()
                    && ladder.operation_id == *operation_id
                {
                    ladder.cancel_transaction_hash = Some(transaction_hash.clone());
                    ladder.state = BulkLadderState::CancelPending;
                }
            }
            JournalEvent::BulkCancelledObserved { operation_id, .. } => {
                if let Some(ladder) = self.bulk_ladder.as_mut()
                    && ladder.operation_id == *operation_id
                {
                    ladder.state = BulkLadderState::CancelledObserved;
                }
            }
            JournalEvent::BulkLifecycleBlocked { operation_id, .. } => {
                if let Some(ladder) = self.bulk_ladder.as_mut()
                    && ladder.operation_id == *operation_id
                {
                    ladder.state = BulkLadderState::Diverged;
                }
            }
            JournalEvent::SpotFill { event_uid, .. } => {
                if !self
                    .processed_spot_fill_uids
                    .iter()
                    .any(|uid| uid == event_uid)
                {
                    self.processed_spot_fill_uids.push(event_uid.clone());
                    const MAX_PROCESSED_SPOT_FILLS: usize = 1_000;
                    if self.processed_spot_fill_uids.len() > MAX_PROCESSED_SPOT_FILLS {
                        let excess = self.processed_spot_fill_uids.len() - MAX_PROCESSED_SPOT_FILLS;
                        self.processed_spot_fill_uids.drain(..excess);
                    }
                }
            }
            JournalEvent::BulkOrderFailed { .. } => self.failed_orders += 1,
            JournalEvent::ReconciliationResult {
                at,
                matched,
                missing,
                unmanaged,
                is_converged,
            } => {
                self.last_reconciliation = Some(ReconciliationSummary {
                    at: *at,
                    matched: *matched,
                    missing: *missing,
                    unmanaged: unmanaged.clone(),
                    is_converged: *is_converged,
                });
            }
            _ => {}
        }
    }
}

pub struct Journal {
    event_path: PathBuf,
    state_path: PathBuf,
}

impl Journal {
    pub fn new(run_id: &str) -> Result<Self> {
        // A caller may redirect durable run data in a sandbox, container, or service unit without
        // changing its HOME. The default remains the platform data directory for normal users.
        let base = std::env::var_os("DECIBEL_GRID_DATA_DIR")
            .map(PathBuf::from)
            .or_else(|| dirs::data_local_dir().or_else(dirs::data_dir))
            .ok_or_else(|| anyhow!("could not determine the local data directory"))?;
        let dir = base.join("decibel-grid").join("runs").join(run_id);
        fs::create_dir_all(&dir)
            .with_context(|| format!("could not create run directory {}", dir.display()))?;
        Ok(Self {
            event_path: dir.join("events.jsonl"),
            state_path: dir.join("state.json"),
        })
    }

    pub fn append(&self, event: &JournalEvent) -> Result<()> {
        let line = serde_json::to_string(event)?;
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.event_path)
            .with_context(|| format!("could not open {}", self.event_path.display()))?;
        use std::io::Write;
        writeln!(file, "{line}")
            .with_context(|| format!("could not write to {}", self.event_path.display()))?;
        file.sync_all()
            .with_context(|| format!("could not sync {}", self.event_path.display()))?;
        Ok(())
    }

    pub fn save_state(&self, state: &RunState) -> Result<()> {
        let content = serde_json::to_string_pretty(state)?;
        let tmp = self.state_path.with_extension("json.tmp");
        {
            use std::io::Write;
            let mut file = fs::File::create(&tmp)
                .with_context(|| format!("could not create {}", tmp.display()))?;
            file.write_all(content.as_bytes())
                .with_context(|| format!("could not write {}", tmp.display()))?;
            file.sync_all()
                .with_context(|| format!("could not sync {}", tmp.display()))?;
        }
        fs::rename(&tmp, &self.state_path)
            .with_context(|| format!("could not rename {}", self.state_path.display()))?;
        let parent = self
            .state_path
            .parent()
            .ok_or_else(|| anyhow!("run state path has no parent directory"))?;
        fs::File::open(parent)
            .with_context(|| format!("could not open {} for sync", parent.display()))?
            .sync_all()
            .with_context(|| format!("could not sync {}", parent.display()))?;
        Ok(())
    }

    pub fn load_state(&self) -> Result<Option<RunState>> {
        if !self.state_path.exists() {
            return self.replay_state();
        }
        let content = fs::read_to_string(&self.state_path)
            .with_context(|| format!("could not read {}", self.state_path.display()))?;
        match serde_json::from_str(&content) {
            Ok(state) => Ok(Some(state)),
            Err(error) => {
                eprintln!(
                    "could not parse {}; rebuilding state from event journal: {error}",
                    self.state_path.display()
                );
                self.replay_state()
            }
        }
    }

    fn replay_state(&self) -> Result<Option<RunState>> {
        if !self.event_path.exists() {
            return Ok(None);
        }
        use std::io::BufRead;
        let file = fs::File::open(&self.event_path)
            .with_context(|| format!("could not open {}", self.event_path.display()))?;
        let mut state = None;
        for (index, line) in std::io::BufReader::new(file).lines().enumerate() {
            let line = line.with_context(|| {
                format!(
                    "could not read event {} from {}",
                    index + 1,
                    self.event_path.display()
                )
            })?;
            if line.trim().is_empty() {
                continue;
            }
            let event: JournalEvent = serde_json::from_str(&line).with_context(|| {
                format!(
                    "could not parse event {} from {}",
                    index + 1,
                    self.event_path.display()
                )
            })?;
            match (&mut state, &event) {
                (None, JournalEvent::RunStart(metadata)) => {
                    state = Some(RunState::new(metadata.clone()))
                }
                (Some(state), _) => state.apply(&event),
                (None, _) => {
                    bail!(
                        "event journal {} starts with {:?}, not RunStart",
                        self.event_path.display(),
                        std::mem::discriminant(&event)
                    )
                }
            }
        }
        Ok(state)
    }
}

/// Deterministic storage key for a logical strategy. The raw subaccount never appears in the
/// directory name, allowing a restarted process to load the exact prior state without leaking the
/// account address through the filesystem.
pub fn persistent_run_id(network: &str, subaccount: &str, market: &str) -> String {
    use sha3::{Digest, Sha3_256};
    let canonical_subaccount = subaccount
        .trim()
        .trim_start_matches("0x")
        .trim_start_matches('0')
        .to_ascii_lowercase();
    let material = format!(
        "{}|{}|{}",
        network.trim().to_ascii_lowercase(),
        if canonical_subaccount.is_empty() {
            "0"
        } else {
            &canonical_subaccount
        },
        market.trim().to_ascii_uppercase(),
    );
    let digest = hex::encode(Sha3_256::digest(material.as_bytes()));
    format!("spot_{}", &digest[..24])
}

/// Retained for non-resumable monitor/shadow sessions.
pub fn generate_run_id() -> String {
    use rand::Rng;
    let suffix: u32 = rand::thread_rng().r#gen();
    format!("run_{suffix:08x}")
}

#[cfg(test)]
mod tests {
    use super::{
        BulkLadder, BulkLadderState, JournalEvent, PerpBootstrapStatus, PerpRuntimeState,
        RunMetadata, RunState, persistent_run_id,
    };
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
            run_id: "run_test".to_owned(),
            started_at: Utc::now(),
            network: "testnet".to_owned(),
            subaccount: raw.to_owned(),
            market: "APT/USDC".to_owned(),
            product: "spot".to_owned(),
            config_hash: "config".to_owned(),
            program_version: "test".to_owned(),
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
            run_id: "test".to_owned(),
            started_at: Utc::now(),
            network: "testnet".to_owned(),
            subaccount: "fingerprint".to_owned(),
            market: "BTC/USD".to_owned(),
            product: "perp".to_owned(),
            config_hash: "config".to_owned(),
            program_version: "test".to_owned(),
        };
        let ladder = BulkLadder {
            operation_id: "bulk-42".to_owned(),
            product: crate::Product::Perp,
            market_address: "0x1".to_owned(),
            sequence: 42,
            prior_sequence: Some(41),
            levels: vec![],
            intent_at: Utc::now(),
            transaction_hash: None,
            cancel_transaction_hash: None,
            state: BulkLadderState::IntentRecorded,
        };
        let mut state = RunState::new(metadata);
        state.apply(&JournalEvent::BulkIntentRecorded {
            at: Utc::now(),
            ladder,
        });
        state.apply(&JournalEvent::BulkBroadcast {
            at: Utc::now(),
            operation_id: "bulk-42".to_owned(),
            transaction_hash: "0xabc".to_owned(),
        });
        let active = state.bulk_ladder.expect("intent is retained");
        assert_eq!(active.transaction_hash.as_deref(), Some("0xabc"));
        assert_eq!(active.state, BulkLadderState::BroadcastPending);
    }

    #[test]
    fn bootstrap_target_is_locked_once_and_completed_never_retries() {
        let mut state = PerpRuntimeState::new();
        assert_eq!(
            state.lock_bootstrap_target(dec!(0.00434)),
            Some(dec!(0.00434))
        );
        assert!(state.requires_bootstrap_convergence(dec!(0), dec!(0.00001)));
        // A changed mid may generate a different derived grid target, but not a new bootstrap.
        assert_eq!(
            state.lock_bootstrap_target(dec!(0.00403)),
            Some(dec!(0.00434))
        );
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
        struct FakeMarketClient {
            submitted_market_orders: usize,
        }
        impl FakeMarketClient {
            fn submit_bootstrap_market_order(&mut self) {
                self.submitted_market_orders += 1;
            }
        }

        let mut state = PerpRuntimeState::new();
        state.lock_bootstrap_target(dec!(1));
        state.complete_bootstrap();
        let mut client = FakeMarketClient {
            submitted_market_orders: 0,
        };
        // This mirrors the engine's only call gate around run_perp_convergence.
        if state.requires_bootstrap_convergence(dec!(0), dec!(0.01)) {
            client.submit_bootstrap_market_order();
        }
        assert_eq!(client.submitted_market_orders, 0);
    }
}
