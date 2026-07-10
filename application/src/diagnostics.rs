use std::sync::RwLock;

use chrono::{DateTime, Utc};
use poise_core::track::TrackId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticSeverity {
    Info,
    Warn,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackDiagnosticItem {
    pub observed_at: DateTime<Utc>,
    pub severity: DiagnosticSeverity,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectJournalFailure {
    pub observed_at: DateTime<Utc>,
    pub track_id: TrackId,
    pub operation: String,
    pub error: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EffectJournalDiagnosticsSnapshot {
    pub failure_count: u64,
    pub last_failure: Option<EffectJournalFailure>,
}

#[derive(Debug, Default)]
pub struct EffectJournalDiagnostics {
    state: RwLock<EffectJournalDiagnosticsSnapshot>,
}

impl EffectJournalDiagnostics {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn snapshot(&self) -> EffectJournalDiagnosticsSnapshot {
        self.state
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub(crate) fn record_failure(
        &self,
        track_id: &TrackId,
        operation: &'static str,
        error: impl ToString,
    ) {
        let mut state = self
            .state
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.failure_count = state.failure_count.saturating_add(1);
        state.last_failure = Some(EffectJournalFailure {
            observed_at: Utc::now(),
            track_id: track_id.clone(),
            operation: operation.to_string(),
            error: error.to_string(),
        });
    }

    #[cfg(any(test, feature = "server-test-support"))]
    pub fn record_failure_for_test(
        &self,
        track_id: &TrackId,
        operation: &'static str,
        error: impl ToString,
    ) {
        self.record_failure(track_id, operation, error);
    }
}

#[cfg(test)]
mod tests {
    use poise_core::track::TrackId;

    use super::{DiagnosticSeverity, EffectJournalDiagnostics, TrackDiagnosticItem};

    #[test]
    fn diagnostics_model_constructs_minimal_item() {
        let item = TrackDiagnosticItem {
            observed_at: chrono::Utc::now(),
            severity: DiagnosticSeverity::Warn,
            message: "boundary ledger anomaly".to_string(),
        };

        assert_eq!(item.severity, DiagnosticSeverity::Warn);
        assert_eq!(item.message, "boundary ledger anomaly");
    }

    #[test]
    fn effect_journal_diagnostics_keep_a_bounded_sticky_failure_summary() {
        let diagnostics = EffectJournalDiagnostics::new();

        diagnostics.record_failure(
            &TrackId::new("btc-core"),
            "append_entries",
            "disk read-only",
        );
        diagnostics.record_failure(
            &TrackId::new("eth-core"),
            "record_outcomes",
            "database busy",
        );

        let snapshot = diagnostics.snapshot();
        assert_eq!(snapshot.failure_count, 2);
        let last = snapshot.last_failure.unwrap();
        assert_eq!(last.track_id, TrackId::new("eth-core"));
        assert_eq!(last.operation, "record_outcomes");
        assert_eq!(last.error, "database busy");
    }
}
