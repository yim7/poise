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
    if price * quantity + f64::EPSILON < rules.min_notional {
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
