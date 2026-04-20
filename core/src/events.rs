use serde::{Deserialize, Serialize};

use crate::strategy::{BandBoundary, BandProtectionPolicy};
use crate::types::Exposure;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReplacementGateReason {
    RoundedMatch,
    ImprovementBelowThreshold {
        improvement_bps: f64,
        threshold_bps: f64,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionGateReason {
    AccountCapacityInsufficient {
        required_notional: f64,
        available_notional: f64,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DomainEvent {
    ExposureTargetChanged {
        from: Exposure,
        to: Exposure,
    },
    BandBreached {
        boundary: BandBoundary,
        price: f64,
    },
    BandReentered {
        price: f64,
    },
    PolicyTriggered {
        policy: BandProtectionPolicy,
    },
    RiskCapApplied {
        intended: Exposure,
        capped: Exposure,
    },
    ExecutionGateApplied {
        reason: ExecutionGateReason,
    },
    ReplacementGateApplied {
        reason: ReplacementGateReason,
    },
}
