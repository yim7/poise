use grid_engine::grid::GridId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GridInternalNotification {
    GridWriteCommitted { grid_id: GridId },
    GridEffectStateChanged { grid_id: GridId },
}
