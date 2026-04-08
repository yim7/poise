use chrono::{DateTime, Utc};

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

#[cfg(test)]
mod tests {
    use super::{DiagnosticSeverity, TrackDiagnosticItem};

    #[test]
    fn diagnostics_model_constructs_minimal_item() {
        let item = TrackDiagnosticItem {
            observed_at: chrono::Utc::now(),
            severity: DiagnosticSeverity::Warn,
            message: "replacement gate active".to_string(),
        };

        assert_eq!(item.severity, DiagnosticSeverity::Warn);
        assert_eq!(item.message, "replacement gate active");
    }
}
