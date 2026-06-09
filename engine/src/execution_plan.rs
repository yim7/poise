use serde::{Deserialize, Serialize};

use crate::executor::binding::SubmitRecoveryToken;
use poise_core::events::DomainEvent;
use poise_core::types::{ExchangeRules, Exposure};

use crate::ports::OrderRequest;
use crate::price_gate::SubmitPurpose;
use poise_core::track::Instrument;

#[derive(Debug, Clone)]
pub struct ExecutionPlan {
    pub actions: Vec<TrackEffect>,
    pub events: Vec<DomainEvent>,
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
    NoOp,
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

impl ExecutionPlan {
    pub fn noop() -> Self {
        Self {
            actions: vec![TrackEffect::NoOp],
            events: vec![],
        }
    }

    pub fn hold(_reason: String) -> Self {
        Self {
            actions: vec![TrackEffect::NoOp],
            events: vec![],
        }
    }

    pub fn has_actions(&self) -> bool {
        self.actions.iter().any(|a| !matches!(a, TrackEffect::NoOp))
    }
}

#[cfg(test)]
mod tests {
    use poise_core::types::{ExchangeRules, QuantityKind};

    use super::is_meetable_minimum;

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
}
