use serde::{Deserialize, Serialize};

use crate::executor::binding::SubmitRecoveryToken;
use poise_core::instrument::{InstrumentSpec, PositionQty};
use poise_core::types::Exposure;

use crate::ports::OrderRequest;
use crate::price_gate::SubmitPurpose;
use poise_core::track::Instrument;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionReason {
    WaitingMarketData,
    Paused,
    AtExecutionTarget,
    WaitingRiskRelease,
    AccountCapacityBlocked,
    RiskInputInvalid,
    BandPolicyBlocked,
    PriceGateBlocked,
    BelowRebalance,
    BelowVenueMinimum,
    CoveredByWorkingOrder,
    RecoveryRequired,
    WaitingCancel,
    SubmitPlanned,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TrackEffect {
    SubmitOrder {
        request: OrderRequest,
        desired_exposure: Exposure,
        submit_purpose: SubmitPurpose,
        recovery_token: SubmitRecoveryToken,
    },
    CancelOrder {
        instrument: Instrument,
        order_id: String,
    },
    CancelAll {
        instrument: Instrument,
    },
}

pub fn round_to_step(value: f64, step: f64) -> f64 {
    if step <= f64::EPSILON {
        return value;
    }
    let steps = (value / step).floor();
    steps * step
}

pub fn is_meetable_minimum(price: f64, quantity: f64, rules: &InstrumentSpec) -> bool {
    PositionQty::new(quantity)
        .and_then(|quantity| rules.meets_minimum(quantity, price))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use poise_core::instrument::InstrumentSpec;

    use super::{DecisionReason, is_meetable_minimum};
    use crate::test_instrument::inverse_spec;

    fn inverse_rules(min_notional: f64) -> InstrumentSpec {
        inverse_spec(0.1, 1.0, 1.0, min_notional, 100.0)
    }

    #[test]
    fn inverse_contract_minimum_uses_contract_notional_not_price_times_contracts() {
        let rules = inverse_rules(500.0);

        assert!(!is_meetable_minimum(50_000.0, 4.0, &rules));
        assert!(is_meetable_minimum(50_000.0, 5.0, &rules));
    }

    #[test]
    fn decision_reason_serializes_as_public_snake_case_vocabulary() {
        assert_eq!(
            serde_json::to_string(&DecisionReason::WaitingRiskRelease).unwrap(),
            r#""waiting_risk_release""#
        );
        assert_eq!(
            serde_json::from_str::<DecisionReason>(r#""submit_planned""#).unwrap(),
            DecisionReason::SubmitPlanned
        );
    }
}
