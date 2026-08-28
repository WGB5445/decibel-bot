//! Durable append-only event journal for grid run state.
//!
//! This is intentionally append-only: events are never modified or deleted. Snapshots cache the
//! latest projection for fast startup; on corruption the journal is replayed to rebuild the
//! snapshot.

use std::{fs, path::PathBuf};

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::reconcile::ActualOrder;

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
    BulkOrderFailed {
        at: DateTime<Utc>,
        error: String,
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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReconciliationSummary {
    pub at: DateTime<Utc>,
    pub matched: usize,
    pub missing: usize,
    pub unmanaged: Vec<ActualOrder>,
    pub is_converged: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunState {
    pub metadata: RunMetadata,
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
            return Ok(None);
        }
        let content = fs::read_to_string(&self.state_path)
            .with_context(|| format!("could not read {}", self.state_path.display()))?;
        serde_json::from_str(&content)
            .map(Some)
            .context("could not parse run state")
    }
}

pub fn generate_run_id() -> String {
    use rand::Rng;
    let suffix: u32 = rand::thread_rng().r#gen();
    format!("run_{:08x}", suffix)
}

#[cfg(test)]
mod tests {
    use super::RunMetadata;
    use chrono::Utc;

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
}
