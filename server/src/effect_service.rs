use std::sync::Arc;

use anyhow::Result;
use grid_engine::grid::GridId;
use grid_engine::ports::{GridSnapshot, PersistedGridEffect, StateRepositoryPort};
use grid_engine::runtime::SubmitRecoveryAnchor;
use grid_engine::transition::GridEffect;
use tokio::sync::broadcast;

use crate::notifications::GridInternalNotification;

#[derive(Clone)]
pub struct EffectService {
    repository: Arc<dyn StateRepositoryPort>,
    notifications: broadcast::Sender<GridInternalNotification>,
}

impl EffectService {
    pub fn new(
        repository: Arc<dyn StateRepositoryPort>,
        notifications: broadcast::Sender<GridInternalNotification>,
    ) -> Self {
        Self {
            repository,
            notifications,
        }
    }

    pub async fn load_grid_state(&self, id: &str) -> Result<Option<GridSnapshot>> {
        self.repository.load_grid_state(id).await
    }

    pub async fn list_pending_effects(&self) -> Result<Vec<PersistedGridEffect>> {
        self.repository.list_pending_effects().await
    }

    pub async fn complete_effect_succeeded(&self, id: &str, effect_id: &str) -> Result<()> {
        self.repository.mark_effect_succeeded(effect_id).await?;
        self.emit_effect_state_changed(id);
        Ok(())
    }

    pub async fn complete_effect_failed(
        &self,
        id: &str,
        effect_id: &str,
        error: &str,
    ) -> Result<()> {
        self.repository.mark_effect_failed(effect_id, error).await?;
        self.emit_effect_state_changed(id);
        Ok(())
    }

    pub async fn submit_recovery_anchor(&self, id: &str) -> Result<Option<SubmitRecoveryAnchor>> {
        let Some(snapshot) = self.load_grid_state(id).await? else {
            return Ok(None);
        };
        let Some(anchor) = snapshot
            .executor_state
            .as_ref()
            .and_then(SubmitRecoveryAnchor::from_executor_state)
        else {
            return Ok(None);
        };

        let pending_effects = self.list_pending_effects().await?;
        Ok(pending_effects.into_iter().find_map(|effect| {
            if effect.grid_id.as_str() != id {
                return None;
            }

            match effect.effect {
                GridEffect::SubmitOrder { request, .. }
                    if request.client_order_id == anchor.client_order_id =>
                {
                    Some(anchor.clone())
                }
                _ => None,
            }
        }))
    }

    fn emit_effect_state_changed(&self, id: &str) {
        let _ = self
            .notifications
            .send(GridInternalNotification::GridEffectStateChanged {
                grid_id: GridId::new(id),
            });
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use anyhow::{Result, anyhow};
    use chrono::Utc;
    use grid_core::strategy::{GridConfig, OutOfBandPolicy, ShapeFamily};
    use grid_core::types::{Exposure, Side};
    use grid_engine::executor::{ExecutionMode, OrderRole, OrderSlot};
    use grid_engine::grid::{GridId, Instrument, Venue};
    use grid_engine::ports::{
        CommittedGridWrite, EffectStatus, EffectStatusUpdate, PersistedGridEffect,
        StateRepositoryPort,
    };
    use grid_engine::runtime::{
        ExecutionSlot, ExecutionStats, ExecutorState, GridStatus, RiskState, SlotState,
        WorkingOrder,
    };
    use grid_engine::snapshot::{GridRuntimeSnapshot, ObservedState};
    use grid_engine::transition::GridEffect;

    use crate::notifications::GridInternalNotification;

    use super::EffectService;

    #[tokio::test]
    async fn submit_recovery_anchor_only_exists_for_matching_pending_submit_effect() {
        let repository = Arc::new(MemoryRepository::default());
        let (notifications, _) = tokio::sync::broadcast::channel(16);
        let service = EffectService::new(repository.clone(), notifications);

        repository.seed_snapshot(snapshot_with_executor_order(WorkingOrder {
            order_id: None,
            client_order_id: "client-1".into(),
            side: Side::Buy,
            price: 94.0,
            quantity: 0.25,
            target_exposure: Exposure(6.0),
            status: grid_engine::ports::OrderStatus::Submitting,
            role: OrderRole::IncreaseInventory,
        }));
        repository.seed_effect(submit_effect("btc-core:batch:0", "client-1"));
        assert_eq!(
            service.submit_recovery_anchor("btc-core").await.unwrap(),
            Some(grid_engine::runtime::SubmitRecoveryAnchor {
                client_order_id: "client-1".into(),
                kind: grid_engine::runtime::SubmitRecoveryKind::Submitting,
            })
        );

        repository.clear_effects();
        assert_eq!(
            service.submit_recovery_anchor("btc-core").await.unwrap(),
            None
        );

        repository.seed_snapshot(snapshot_with_executor_order(WorkingOrder {
            order_id: Some("order-1".into()),
            client_order_id: "client-1".into(),
            side: Side::Buy,
            price: 94.0,
            quantity: 0.25,
            target_exposure: Exposure(6.0),
            status: grid_engine::ports::OrderStatus::New,
            role: OrderRole::IncreaseInventory,
        }));
        repository.seed_effect(submit_effect("btc-core:batch:1", "client-1"));
        assert_eq!(
            service.submit_recovery_anchor("btc-core").await.unwrap(),
            Some(grid_engine::runtime::SubmitRecoveryAnchor {
                client_order_id: "client-1".into(),
                kind: grid_engine::runtime::SubmitRecoveryKind::ReceiptBacked,
            })
        );
    }

    #[tokio::test]
    async fn submit_recovery_anchor_returns_none_without_executor_state() {
        let repository = Arc::new(MemoryRepository::default());
        let (notifications, _) = tokio::sync::broadcast::channel(16);
        let service = EffectService::new(repository.clone(), notifications);

        repository.seed_snapshot(snapshot_without_executor_state());
        repository.seed_effect(submit_effect("btc-core:batch:legacy", "client-legacy"));

        assert_eq!(
            service.submit_recovery_anchor("btc-core").await.unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn complete_effect_succeeded_marks_status_and_emits_notification() {
        let repository = Arc::new(MemoryRepository::default());
        let (notifications, _) = tokio::sync::broadcast::channel(16);
        let service = EffectService::new(repository.clone(), notifications);
        let mut receiver = service.notifications.subscribe();

        repository.seed_effect(PersistedGridEffect {
            effect_id: "btc-core:batch:0".into(),
            grid_id: GridId::new("btc-core"),
            batch_id: "batch".into(),
            sequence: 0,
            effect: GridEffect::NoOp,
            status: EffectStatus::Pending,
            attempt_count: 0,
            last_error: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        });

        service
            .complete_effect_succeeded("btc-core", "btc-core:batch:0")
            .await
            .unwrap();

        assert_eq!(
            receiver.recv().await.unwrap(),
            GridInternalNotification::GridEffectStateChanged {
                grid_id: GridId::new("btc-core"),
            }
        );
        assert_eq!(
            repository.effect("btc-core:batch:0").unwrap().status,
            EffectStatus::Succeeded
        );
    }

    fn snapshot_without_executor_state() -> GridRuntimeSnapshot {
        GridRuntimeSnapshot {
            grid_id: GridId::new("btc-core"),
            instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
            config: GridConfig {
                lower_price: 90.0,
                upper_price: 110.0,
                long_exposure_units: 8.0,
                short_exposure_units: 8.0,
                notional_per_unit: 375.0,
                shape_family: ShapeFamily::Linear,
                out_of_band_policy: OutOfBandPolicy::Freeze,
            },
            status: GridStatus::Active,
            current_exposure: Exposure(0.0),
            target_exposure: Some(Exposure(6.0)),
            executor_state: None,
            replacement_gate_reason: None,
            risk: RiskState::default(),
            observed: ObservedState {
                reference_price: Some(95.0),
                out_of_band_since: None,
            },
        }
    }

    fn snapshot_with_executor_order(order: WorkingOrder) -> GridRuntimeSnapshot {
        GridRuntimeSnapshot {
            grid_id: GridId::new("btc-core"),
            instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
            config: GridConfig {
                lower_price: 90.0,
                upper_price: 110.0,
                long_exposure_units: 8.0,
                short_exposure_units: 8.0,
                notional_per_unit: 375.0,
                shape_family: ShapeFamily::Linear,
                out_of_band_policy: OutOfBandPolicy::Freeze,
            },
            status: GridStatus::Active,
            current_exposure: Exposure(0.0),
            target_exposure: Some(Exposure(6.0)),
            executor_state: Some(ExecutorState {
                mode: ExecutionMode::Passive,
                inventory_gap: Exposure(6.0),
                gap_started_at: Some(Utc::now()),
                last_reprice_at: None,
                slots: vec![ExecutionSlot {
                    slot: OrderSlot::new("inventory_core"),
                    state: if order.order_id.is_some() {
                        SlotState::Working
                    } else {
                        SlotState::SubmitPending
                    },
                    working_order: Some(order),
                }],
                last_execution_reason: None,
                recovery_anomaly: None,
                stats: ExecutionStats {
                    started_at: Utc::now(),
                    max_inventory_gap_abs: Exposure(0.0),
                    max_gap_age_ms: 0,
                },
            }),
            replacement_gate_reason: None,
            risk: RiskState::default(),
            observed: ObservedState {
                reference_price: Some(95.0),
                out_of_band_since: None,
            },
        }
    }

    fn submit_effect(effect_id: &str, client_order_id: &str) -> PersistedGridEffect {
        PersistedGridEffect {
            effect_id: effect_id.into(),
            grid_id: GridId::new("btc-core"),
            batch_id: "batch".into(),
            sequence: 0,
            effect: GridEffect::SubmitOrder {
                request: grid_engine::ports::OrderRequest {
                    instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                    side: Side::Buy,
                    price: 94.0,
                    quantity: 0.25,
                    client_order_id: client_order_id.into(),
                },
                target_exposure: Exposure(6.0),
            },
            status: EffectStatus::Pending,
            attempt_count: 0,
            last_error: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[derive(Default)]
    struct MemoryRepository {
        snapshots: Mutex<HashMap<String, GridRuntimeSnapshot>>,
        effects: Mutex<Vec<PersistedGridEffect>>,
    }

    impl MemoryRepository {
        fn seed_snapshot(&self, snapshot: GridRuntimeSnapshot) {
            self.snapshots
                .lock()
                .unwrap()
                .insert(snapshot.grid_id.as_str().to_string(), snapshot);
        }

        fn seed_effect(&self, effect: PersistedGridEffect) {
            self.effects.lock().unwrap().push(effect);
        }

        fn clear_effects(&self) {
            self.effects.lock().unwrap().clear();
        }

        fn effect(&self, effect_id: &str) -> Option<PersistedGridEffect> {
            self.effects
                .lock()
                .unwrap()
                .iter()
                .find(|effect| effect.effect_id == effect_id)
                .cloned()
        }
    }

    #[async_trait::async_trait]
    impl StateRepositoryPort for MemoryRepository {
        async fn save_transition_with_effect_status(
            &self,
            _id: &str,
            _state: &GridRuntimeSnapshot,
            _events: &[grid_core::events::DomainEvent],
            _effects: &[GridEffect],
            _effect_status_update: Option<&EffectStatusUpdate>,
        ) -> Result<CommittedGridWrite> {
            Err(anyhow!("not used in tests"))
        }

        async fn load_grid_state(&self, id: &str) -> Result<Option<GridRuntimeSnapshot>> {
            Ok(self.snapshots.lock().unwrap().get(id).cloned())
        }

        async fn list_events(&self, _id: &str) -> Result<Vec<grid_core::events::DomainEvent>> {
            Ok(Vec::new())
        }

        async fn list_pending_effects(&self) -> Result<Vec<PersistedGridEffect>> {
            Ok(self
                .effects
                .lock()
                .unwrap()
                .iter()
                .filter(|effect| effect.status == EffectStatus::Pending)
                .cloned()
                .collect())
        }

        async fn mark_effect_executing(&self, _effect_id: &str) -> Result<()> {
            Err(anyhow!("not used in tests"))
        }

        async fn mark_effect_succeeded(&self, effect_id: &str) -> Result<()> {
            let mut effects = self.effects.lock().unwrap();
            let effect = effects
                .iter_mut()
                .find(|effect| effect.effect_id == effect_id)
                .ok_or_else(|| anyhow!("missing effect `{effect_id}`"))?;
            effect.status = EffectStatus::Succeeded;
            Ok(())
        }

        async fn mark_effect_superseded(&self, effect_id: &str) -> Result<()> {
            let mut effects = self.effects.lock().unwrap();
            let effect = effects
                .iter_mut()
                .find(|effect| effect.effect_id == effect_id)
                .ok_or_else(|| anyhow!("missing effect `{effect_id}`"))?;
            effect.status = EffectStatus::Superseded;
            effect.last_error = None;
            Ok(())
        }

        async fn mark_effect_failed(&self, effect_id: &str, error: &str) -> Result<()> {
            let mut effects = self.effects.lock().unwrap();
            let effect = effects
                .iter_mut()
                .find(|effect| effect.effect_id == effect_id)
                .ok_or_else(|| anyhow!("missing effect `{effect_id}`"))?;
            effect.status = EffectStatus::Failed;
            effect.last_error = Some(error.to_string());
            Ok(())
        }
    }
}
