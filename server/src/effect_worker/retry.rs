use anyhow::Result;
use poise_core::track::Instrument;

use crate::order_outcome::{OutcomeUnknownRecovery, ReconcileRequest};
use crate::runtime;

use super::EffectWorker;

pub(super) async fn trigger_immediate_reconcile(
    worker: &EffectWorker,
    track_id: &str,
    instrument: &Instrument,
    reason: crate::order_outcome::ReconcileReason,
) -> Result<()> {
    runtime::enqueue_reconcile_request(
        &worker.state.reconcile,
        worker.execution.as_ref(),
        ReconcileRequest {
            track_id: track_id.to_string(),
            reason,
        },
        instrument,
    )
    .await?;
    Ok(())
}

pub(super) async fn recover_unknown_outcome(
    worker: &EffectWorker,
    track_id: &str,
    instrument: &Instrument,
    recovery: OutcomeUnknownRecovery,
) -> Result<()> {
    worker
        .state
        .reconcile
        .exchange_freshness
        .mark_stale(track_id, recovery.freshness_reason)
        .await;
    trigger_immediate_reconcile(worker, track_id, instrument, recovery.reconcile_reason).await
}
