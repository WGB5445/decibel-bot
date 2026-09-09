use anyhow::Result;
use chrono::Utc;
use decibel_grid_tui::*;

/// Journal a full cancel lifecycle: intent → broadcast → observed.
/// If there is no tracked bulk ladder, skips the cancel entirely.
pub async fn journaled_bulk_cancel(
    journal: Option<&journal::Journal>,
    run_state: &mut journal::RunState,
    network: &str,
    private_key: &str,
    subaccount: &str,
    market: &Market,
    gas_station: Option<&GasStationConfig>,
) -> Result<Option<String>> {
    let op_id = match run_state.bulk_ladder.as_ref().map(|l| l.operation_id.clone()) {
        Some(id) => id,
        None => return Ok(None),
    };
    let journal = match journal {
        Some(j) => j,
        None => return Ok(None),
    };
    let cancel_intent = journal::JournalEvent::BulkCancelIntentRecorded {
        at: Utc::now(),
        operation_id: op_id.clone(),
    };
    journal.append(&cancel_intent)?;
    run_state.apply(&cancel_intent);
    journal.save_state(&run_state)?;

    let cancel_op_id = op_id.clone();
    let hash = spot_lifecycle::cancel_bulk_ladder_with_broadcast(
        network,
        private_key,
        subaccount,
        market,
        gas_station,
        |tx_hash| {
            let broadcast = journal::JournalEvent::BulkCancelBroadcast {
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

    let observed = journal::JournalEvent::BulkCancelledObserved {
        at: Utc::now(),
        operation_id: op_id,
    };
    journal.append(&observed)?;
    run_state.apply(&observed);
    journal.save_state(&run_state)?;
    Ok(Some(hash))
}