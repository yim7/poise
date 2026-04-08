use std::sync::Arc;

use anyhow::Result;
#[cfg(any(test, feature = "server-test-support"))]
use poise_engine::manager::TrackManager;
#[cfg(any(test, feature = "server-test-support"))]
use tokio::sync::RwLock;

use crate::mutation_executor::{MutationExecutor, PreparedSubmitExecution};

#[derive(Clone)]
pub struct TrackEffectService {
    executor: Arc<MutationExecutor>,
}

impl TrackEffectService {
    pub(crate) fn from_executor(executor: Arc<MutationExecutor>) -> Self {
        Self { executor }
    }

    pub async fn prepare_submit_execution(
        &self,
        id: &str,
        effect_id: &str,
        request: &poise_engine::ports::OrderRequest,
        desired_exposure: poise_core::types::Exposure,
        live_order: Option<&poise_engine::ports::ExchangeOrder>,
    ) -> Result<Option<PreparedSubmitExecution>> {
        self.executor
            .prepare_submit_execution(id, effect_id, request, desired_exposure, live_order)
            .await
    }

    pub async fn recover_submit_effect(
        &self,
        id: &str,
        effect_id: &str,
        request: &poise_engine::ports::OrderRequest,
        desired_exposure: poise_core::types::Exposure,
        live_order: Option<&poise_engine::ports::ExchangeOrder>,
    ) -> Result<poise_engine::executor::SubmitRecoveryResolution> {
        self.executor
            .recover_submit_effect(id, effect_id, request, desired_exposure, live_order)
            .await
    }

    pub async fn complete_submit_execution(
        &self,
        id: &str,
        effect_id: &str,
        request: &poise_engine::ports::OrderRequest,
        desired_exposure: poise_core::types::Exposure,
        receipt: &poise_engine::ports::OrderReceipt,
    ) -> Result<()> {
        self.executor
            .complete_submit_execution(id, effect_id, request, desired_exposure, receipt)
            .await
    }

    pub async fn record_submit_failure(
        &self,
        id: &str,
        effect_id: &str,
        client_order_id: &str,
        error: &str,
    ) -> Result<()> {
        self.executor
            .record_submit_failure(id, effect_id, client_order_id, error)
            .await
    }

    pub async fn record_cancel_order_success(
        &self,
        id: &str,
        effect_id: &str,
        batch_id: &str,
        sequence: u32,
        order_id: &str,
    ) -> Result<()> {
        self.executor
            .record_cancel_order_success(id, effect_id, batch_id, sequence, order_id)
            .await
    }

    pub async fn record_cancel_all_success(&self, id: &str, effect_id: &str) -> Result<()> {
        self.executor.record_cancel_all_success(id, effect_id).await
    }

    pub async fn complete_effect_succeeded(&self, id: &str, effect_id: &str) -> Result<()> {
        self.executor.complete_effect_succeeded(id, effect_id).await
    }

    pub async fn complete_effect_failed(
        &self,
        id: &str,
        effect_id: &str,
        error: &str,
    ) -> Result<()> {
        self.executor
            .complete_effect_failed(id, effect_id, error)
            .await
    }

    pub async fn retire_stale_follow_up_submit(
        &self,
        id: &str,
        request: &crate::FollowUpRetirementRequest,
    ) -> Result<bool> {
        self.executor
            .retire_stale_follow_up_submit(id, request)
            .await
    }

    pub async fn request_follow_up_retirement(
        &self,
        id: &str,
        request: crate::FollowUpRetirementRequest,
    ) -> Result<()> {
        self.executor
            .request_follow_up_retirement(id, request)
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

    use crate::mutation_executor::test_support::{
        MemoryRepository, manager_with_pending_submit, track_write_services,
    };
    use crate::{ApplicationNotification, EffectStatus};

    use super::TrackEffectService;
    use poise_engine::track::TrackId;

    #[tokio::test]
    async fn effect_service_records_submit_failure_and_updates_effect_status() {
        let repository = Arc::new(MemoryRepository::default());
        let service = test_service(repository.clone());
        let mut receiver = service.1.subscribe();

        let pending = repository.pending_effects();
        assert!(!pending.is_empty());

        service
            .0
            .record_submit_failure(
                "btc-core",
                "btc-core:batch-1:0",
                "client-1",
                "submit order rejected",
            )
            .await
            .unwrap();

        assert_eq!(
            receiver.recv().await.unwrap(),
            ApplicationNotification::TrackChanged {
                track_id: TrackId::new("btc-core"),
            }
        );
        assert_eq!(repository.pending_effects()[0].status, EffectStatus::Failed);
    }

    fn test_service(
        repository: Arc<MemoryRepository>,
    ) -> (
        TrackEffectService,
        tokio::sync::broadcast::Sender<ApplicationNotification>,
    ) {
        repository.seed_pending_submit_effect();
        let (services, notifications) =
            track_write_services(manager_with_pending_submit(), repository);
        (services.effect, notifications)
    }
}
