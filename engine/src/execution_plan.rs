use serde::{Deserialize, Serialize};

use poise_core::events::DomainEvent;
use poise_core::types::{ExchangeRules, Exposure};

use crate::ports::OrderRequest;
use crate::price_gate::SubmitPurpose;
use crate::track::Instrument;

#[derive(Debug, Clone)]
pub struct ExecutionPlan {
    pub actions: Vec<ExecutionAction>,
    pub events: Vec<DomainEvent>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ExecutionAction {
    SubmitOrder {
        request: OrderRequest,
        desired_exposure: Exposure,
        submit_purpose: SubmitPurpose,
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
            actions: vec![ExecutionAction::NoOp],
            events: vec![],
        }
    }

    pub fn hold(reason: String) -> Self {
        Self {
            actions: vec![ExecutionAction::NoOp],
            events: vec![DomainEvent::RiskDenied { reason }],
        }
    }

    pub fn has_actions(&self) -> bool {
        self.actions
            .iter()
            .any(|a| !matches!(a, ExecutionAction::NoOp))
    }
}
