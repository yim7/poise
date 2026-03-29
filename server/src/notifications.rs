use grid_engine::grid::GridId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GridInternalNotification {
    GridWriteCommitted {
        grid_id: GridId,
        recovery_anomaly_active: bool,
    },
    GridEffectStateChanged {
        grid_id: GridId,
    },
}
