use grid_core::events::DomainEvent;

use crate::ports::OrderRequest;

#[derive(Debug, Clone)]
pub struct ExecutionPlan {
    pub actions: Vec<ExecutionAction>,
    pub events: Vec<DomainEvent>,
}

#[derive(Debug, Clone)]
pub enum ExecutionAction {
    SubmitOrder(OrderRequest),
    CancelOrder { order_id: String },
    CancelAll,
    NoOp,
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
        self.actions.iter().any(|a| !matches!(a, ExecutionAction::NoOp))
    }
}
