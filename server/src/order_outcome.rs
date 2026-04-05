use anyhow::Error;

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcileReason {
    PeriodicAudit,
    SubmitOutcomeUnknown,
    CancelOutcomeUnknown,
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
pub enum OutcomeClass {
    FinalFailure,
    OutcomeUnknown(ReconcileReason),
}

pub fn classify_cancel_error(error: &Error) -> OutcomeClass {
    classify_cancel_error_message(&error.to_string())
}

pub fn classify_cancel_error_message(message: &str) -> OutcomeClass {
    if message.contains("\"code\":-2011") && message.contains("Unknown order sent.") {
        return OutcomeClass::OutcomeUnknown(ReconcileReason::CancelOutcomeUnknown);
    }

    OutcomeClass::FinalFailure
}

pub fn classify_submit_receipt_writeback_error(error: &Error) -> OutcomeClass {
    if error
        .to_string()
        .contains("submit receipt did not match executor slot")
    {
        return OutcomeClass::OutcomeUnknown(ReconcileReason::SubmitOutcomeUnknown);
    }

    OutcomeClass::FinalFailure
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

    use super::*;

    #[test]
    fn classify_unknown_order_sent_as_cancel_outcome_unknown() {
        let error = anyhow!(
            "request DELETE /fapi/v1/order failed with status 400 Bad Request: {{\"code\":-2011,\"msg\":\"Unknown order sent.\"}}"
        );

        assert_eq!(
            classify_cancel_error(&error),
            OutcomeClass::OutcomeUnknown(ReconcileReason::CancelOutcomeUnknown)
        );
    }

    #[test]
    fn classify_unknown_order_sent_message_as_cancel_outcome_unknown() {
        assert_eq!(
            classify_cancel_error_message(
                "request DELETE /fapi/v1/order failed with status 400 Bad Request: {\"code\":-2011,\"msg\":\"Unknown order sent.\"}"
            ),
            OutcomeClass::OutcomeUnknown(ReconcileReason::CancelOutcomeUnknown)
        );
    }
}
