use anyhow::Error;
use poise_engine::ports::{ExecutionPortError, ExecutionPortErrorKind};

use crate::exchange_freshness::ExchangeFreshnessReason;

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcileReason {
    PeriodicAudit,
    SyncAfterSubmitOutcomeUnknown,
    SyncAfterCancelOutcomeUnknown,
    UnabsorbedOrderUpdate,
    SyncBeforeSideEffect,
    ManualRecovery,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconcileRequest {
    pub track_id: String,
    pub reason: ReconcileReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcileTriggerClass {
    Periodic,
    Emergency,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconcileExecution {
    pub track_id: String,
    pub trigger_class: ReconcileTriggerClass,
    pub merged_reasons: Vec<ReconcileReason>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutcomeUnknownRecovery {
    pub freshness_reason: ExchangeFreshnessReason,
    pub reconcile_reason: ReconcileReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutcomeClass {
    FinalFailure,
    OutcomeUnknown(OutcomeUnknownRecovery),
}

pub fn classify_cancel_error(error: &Error) -> OutcomeClass {
    if error
        .chain()
        .find_map(|cause| cause.downcast_ref::<ExecutionPortError>())
        .is_some_and(|error| error.kind() == ExecutionPortErrorKind::CancelOutcomeUnknown)
    {
        return OutcomeClass::OutcomeUnknown(OutcomeUnknownRecovery {
            freshness_reason: ExchangeFreshnessReason::CancelOutcomeUnknown,
            reconcile_reason: ReconcileReason::SyncAfterCancelOutcomeUnknown,
        });
    }

    OutcomeClass::FinalFailure
}

pub fn cancel_writeback_outcome_unknown() -> OutcomeUnknownRecovery {
    OutcomeUnknownRecovery {
        freshness_reason: ExchangeFreshnessReason::CancelOutcomeUnknown,
        reconcile_reason: ReconcileReason::SyncAfterCancelOutcomeUnknown,
    }
}

pub fn classify_submit_receipt_writeback_error(_error: &Error) -> OutcomeClass {
    OutcomeClass::OutcomeUnknown(OutcomeUnknownRecovery {
        freshness_reason: ExchangeFreshnessReason::SubmitOutcomeUnknown,
        reconcile_reason: ReconcileReason::SyncAfterSubmitOutcomeUnknown,
    })
}

pub fn reconcile_execution(
    track_id: &str,
    merged_reasons: Vec<ReconcileReason>,
) -> ReconcileExecution {
    let trigger_class = if merged_reasons
        .iter()
        .all(|reason| *reason == ReconcileReason::PeriodicAudit)
    {
        ReconcileTriggerClass::Periodic
    } else {
        ReconcileTriggerClass::Emergency
    };

    ReconcileExecution {
        track_id: track_id.to_string(),
        trigger_class,
        merged_reasons,
    }
}

#[cfg(test)]
mod tests {
    use anyhow::anyhow;
    use poise_engine::ports::ExecutionPortError;

    use super::*;

    #[test]
    fn classify_cancel_outcome_unknown_port_error_as_unknown() {
        let error = Error::new(ExecutionPortError::cancel_outcome_unknown(
            "Unknown order sent.",
        ))
        .context("cancel request failed");

        assert_eq!(
            classify_cancel_error(&error),
            OutcomeClass::OutcomeUnknown(OutcomeUnknownRecovery {
                freshness_reason: ExchangeFreshnessReason::CancelOutcomeUnknown,
                reconcile_reason: ReconcileReason::SyncAfterCancelOutcomeUnknown,
            })
        );
    }

    #[test]
    fn classify_plain_cancel_error_as_final_failure() {
        let error = anyhow!("unknown order sent");

        assert_eq!(classify_cancel_error(&error), OutcomeClass::FinalFailure);
    }

    #[test]
    fn classify_submit_receipt_writeback_error_returns_recovery_mapping() {
        let error = anyhow!("submit receipt did not match executor slot");

        assert_eq!(
            classify_submit_receipt_writeback_error(&error),
            OutcomeClass::OutcomeUnknown(OutcomeUnknownRecovery {
                freshness_reason: ExchangeFreshnessReason::SubmitOutcomeUnknown,
                reconcile_reason: ReconcileReason::SyncAfterSubmitOutcomeUnknown,
            })
        );
    }

    #[test]
    fn classify_submit_receipt_persistence_failure_as_outcome_unknown() {
        let error = anyhow!("injected receipt persistence failure");

        assert_eq!(
            classify_submit_receipt_writeback_error(&error),
            OutcomeClass::OutcomeUnknown(OutcomeUnknownRecovery {
                freshness_reason: ExchangeFreshnessReason::SubmitOutcomeUnknown,
                reconcile_reason: ReconcileReason::SyncAfterSubmitOutcomeUnknown,
            })
        );
    }
}
