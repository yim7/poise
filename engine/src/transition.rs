use grid_core::events::DomainEvent;

use crate::execution_plan::ExecutionAction;
use crate::snapshot::GridRuntimeSnapshot;

pub type GridEffect = ExecutionAction;

#[derive(Debug, Clone)]
pub struct GridTransition {
    pub snapshot: GridRuntimeSnapshot,
    pub events: Vec<DomainEvent>,
    pub effects: Vec<GridEffect>,
}
