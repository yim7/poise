use serde::{Deserialize, Serialize};

use crate::strategy::{BandBoundary, OutOfBandPolicy};
use crate::types::Exposure;

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
        policy: OutOfBandPolicy,
    },
    RiskCapApplied {
        intended: Exposure,
        capped: Exposure,
    },
    RiskDenied {
        reason: String,
    },
}
