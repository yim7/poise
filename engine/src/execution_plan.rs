use serde::{Deserialize, Serialize};

use crate::executor::binding::SubmitRecoveryToken;
use poise_core::types::{ExchangeRules, Exposure};

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

pub fn is_meetable_minimum(price: f64, quantity: f64, rules: &ExchangeRules) -> bool {
    if quantity + f64::EPSILON < rules.min_qty {
        return false;
    }
    if rules.notional_from_native_qty(quantity, price) + f64::EPSILON < rules.min_notional {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use poise_core::types::{ExchangeRules, QuantityKind};

    use super::{DecisionReason, is_meetable_minimum};

    fn inverse_rules(min_notional: f64) -> ExchangeRules {
        ExchangeRules {
            price_tick: 0.1,
            price_precision: Default::default(),
            quantity_kind: QuantityKind::InverseContract,
            contract_notional: Some(100.0),
            settlement_asset: "BTC".to_string(),
            quantity_step: 1.0,
            min_qty: 1.0,
            min_notional,
            maker_fee_rate: 0.0,
            taker_fee_rate: 0.0,
        }
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
