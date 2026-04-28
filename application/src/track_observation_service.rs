use std::sync::Arc;

use anyhow::Result;
#[cfg(any(test, feature = "server-test-support"))]
use poise_engine::manager::TrackManager;
use poise_engine::observation::{
    CompleteOpenOrderSnapshot, MarketObservation, OrderObservation, PositionObservation,
};
use poise_engine::runtime::{QuoteHealthView, StrategyTargetView, TrackLiveView, TrackRuntimeView};
#[cfg(any(test, feature = "server-test-support"))]
use tokio::sync::RwLock;

use crate::mutation_executor::{ApplyTrackLedgerEventResult, MutationExecutor, TrackInstrument};

#[derive(Clone)]
pub struct TrackObservationService {
    executor: Arc<MutationExecutor>,
}

impl TrackObservationService {
    pub(crate) fn from_executor(executor: Arc<MutationExecutor>) -> Self {
        Self { executor }
    }

    pub async fn track_instruments(&self) -> Vec<TrackInstrument> {
        self.executor.track_instruments().await
    }

    pub async fn resolve_track_id(
        &self,
        instrument: &poise_core::track::Instrument,
    ) -> Option<String> {
        self.executor.resolve_track_id(instrument).await
    }

    pub async fn observe_market(
        &self,
        id: &str,
        observation: MarketObservation,
    ) -> Result<poise_engine::transition::TrackTransition> {
        self.executor.observe_market(id, observation).await
    }

    pub async fn refresh_market_data_health(
        &self,
        id: &str,
    ) -> Result<poise_engine::transition::TrackTransition> {
        self.executor.refresh_market_data_health(id).await
    }

    pub async fn market_data_health_deadline(
        &self,
        id: &str,
    ) -> Result<Option<chrono::DateTime<chrono::Utc>>> {
        self.executor.market_data_health_deadline(id).await
    }

    pub async fn track_live_view(&self, id: &str) -> Result<TrackLiveView> {
        self.executor.track_live_view(id).await
    }

    pub async fn track_runtime_view(&self, id: &str) -> Result<Option<TrackRuntimeView>> {
        self.executor.track_runtime_view(id).await
    }

    pub async fn quote_health_view(&self, id: &str) -> Result<QuoteHealthView> {
        self.executor.quote_health_view(id).await
    }

    pub async fn strategy_target_view(&self, id: &str) -> Result<StrategyTargetView> {
        self.executor.strategy_target_view(id).await
    }

    pub async fn observe_position(
        &self,
        id: &str,
        observation: PositionObservation,
    ) -> Result<poise_engine::transition::TrackTransition> {
        self.executor.observe_position(id, observation).await
    }

    pub async fn observe_order_with_absorb_result(
        &self,
        id: &str,
        observation: OrderObservation,
    ) -> Result<(
        poise_engine::transition::TrackTransition,
        poise_engine::executor::OrderUpdateAbsorbResult,
    )> {
        self.executor
            .observe_order_with_absorb_result(id, observation)
            .await
    }

    pub async fn apply_track_ledger_event(
        &self,
        id: &str,
        event: poise_engine::ledger::TrackLedgerEvent,
    ) -> Result<ApplyTrackLedgerEventResult> {
        self.executor.apply_track_ledger_event(id, event).await
    }

    pub async fn sync_exchange_state(
        &self,
        id: &str,
        position: PositionObservation,
        open_orders: CompleteOpenOrderSnapshot,
    ) -> Result<poise_engine::transition::TrackTransition> {
        self.executor
            .sync_exchange_state(id, position, open_orders)
            .await
    }

    pub async fn sync_exchange_state_without_reconcile(
        &self,
        id: &str,
        position: PositionObservation,
        open_orders: CompleteOpenOrderSnapshot,
    ) -> Result<poise_engine::transition::TrackTransition> {
        self.executor
            .sync_exchange_state_without_reconcile(id, position, open_orders)
            .await
    }

    #[cfg(any(test, feature = "server-test-support"))]
    pub fn manager(&self) -> Arc<RwLock<TrackManager>> {
        self.executor.manager()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use poise_core::track::TrackId;
    use poise_engine::observation::MarketObservation;
    use poise_engine::ports::ExecutionQuote;

    use super::TrackObservationService;
    use crate::ApplicationNotification;
    use crate::mutation_executor::test_support::{
        MemoryRepository, seeded_manager, track_write_services,
    };

    #[tokio::test]
    async fn observation_service_persists_market_observation_with_mark_and_quote() {
        let repository = Arc::new(MemoryRepository::default());
        let service = test_service(repository.clone());
        let mut receiver = service.1.subscribe();

        let transition = service
            .0
            .observe_market(
                "btc-core",
                MarketObservation::ExecutionQuote {
                    execution_quote: ExecutionQuote {
                        best_bid: 94.5,
                        best_ask: 95.5,
                    },
                },
            )
            .await
            .unwrap();

        assert!(!transition.effects.is_empty());
        assert_eq!(
            receiver.recv().await.unwrap(),
            ApplicationNotification::TrackChanged {
                track_id: TrackId::new("btc-core"),
            }
        );
        assert!(!repository.pending_effects().is_empty());
    }

    fn test_service(
        repository: Arc<MemoryRepository>,
    ) -> (
        TrackObservationService,
        tokio::sync::broadcast::Sender<ApplicationNotification>,
    ) {
        let (services, notifications) = track_write_services(seeded_manager(), repository);
        (services.observation, notifications)
    }
}
