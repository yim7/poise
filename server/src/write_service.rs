#![cfg(test)]
#![allow(dead_code)]

use std::sync::Arc;

use anyhow::Result;
use poise_application::{
    AccountCapacityGuard, ApplicationNotification, ApplyTrackLedgerEventResult,
    FollowUpRetirementRequest, PreparedSubmitExecution, TrackEffectService, TrackEffectStore,
    TrackInstrument, TrackMutationStore, TrackObservationService, TrackWriteServices,
};
use poise_engine::command::TrackCommand;
use poise_engine::executor::{OrderUpdateAbsorbResult, SubmitRecoveryResolution};
use poise_engine::ledger::TrackLedgerEvent;
use poise_engine::manager::TrackManager;
use poise_engine::observation::{OrderObservation, PositionObservation};
use poise_engine::ports::{ExchangeOrder, OrderReceipt, OrderRequest};
use poise_engine::track::Instrument;
use poise_engine::transition::TrackTransition;
use tokio::sync::{RwLock, broadcast};

use crate::runtime::AccountMarginGuardStore;

#[derive(Clone)]
pub struct TrackWriteService {
    command_service: Arc<poise_application::TrackCommandService>,
    observation_service: Arc<TrackObservationService>,
    effect_service: Arc<TrackEffectService>,
    notifications: broadcast::Sender<ApplicationNotification>,
    account_margin_guard: Arc<AccountMarginGuardStore>,
}

impl TrackWriteService {
    pub fn new(
        manager: TrackManager,
        mutation_store: Arc<dyn TrackMutationStore>,
        effect_store: Arc<dyn TrackEffectStore>,
        notifications: broadcast::Sender<ApplicationNotification>,
        account_margin_guard: Arc<AccountMarginGuardStore>,
    ) -> Self {
        let services = TrackWriteServices::new(
            manager,
            mutation_store,
            effect_store,
            notifications.clone(),
            account_margin_guard.clone() as Arc<dyn AccountCapacityGuard>,
        );
        Self {
            command_service: Arc::new(services.command),
            observation_service: Arc::new(services.observation),
            effect_service: Arc::new(services.effect),
            notifications,
            account_margin_guard,
        }
    }

    pub fn command_service(&self) -> Arc<poise_application::TrackCommandService> {
        Arc::clone(&self.command_service)
    }

    pub fn observation_service(&self) -> Arc<TrackObservationService> {
        Arc::clone(&self.observation_service)
    }

    pub fn effect_service(&self) -> Arc<TrackEffectService> {
        Arc::clone(&self.effect_service)
    }

    pub fn notification_sender(&self) -> broadcast::Sender<ApplicationNotification> {
        self.notifications.clone()
    }

    pub fn subscribe_notifications(&self) -> broadcast::Receiver<ApplicationNotification> {
        self.notifications.subscribe()
    }

    pub(crate) fn uses_account_margin_guard(
        &self,
        expected: &Arc<AccountMarginGuardStore>,
    ) -> bool {
        Arc::ptr_eq(&self.account_margin_guard, expected)
    }

    pub(crate) fn emit_internal_notification(&self, notification: ApplicationNotification) {
        let _ = self.notifications.send(notification);
    }

    pub async fn has_track(&self, id: &str) -> bool {
        self.command_service.has_track(id).await
    }

    pub async fn command(&self, id: &str, command: TrackCommand) -> Result<TrackTransition> {
        self.command_service.command(id, command).await
    }

    pub async fn track_instruments(&self) -> Vec<TrackInstrument> {
        self.observation_service.track_instruments().await
    }

    pub async fn resolve_track_id(&self, instrument: &Instrument) -> Option<String> {
        self.observation_service.resolve_track_id(instrument).await
    }

    pub async fn observe_market(&self, id: &str, reference_price: f64) -> Result<TrackTransition> {
        self.observation_service
            .observe_market(id, reference_price)
            .await
    }

    pub async fn refresh_market_data_health(&self, id: &str) -> Result<TrackTransition> {
        self.observation_service
            .refresh_market_data_health(id)
            .await
    }

    pub async fn observe_position(
        &self,
        id: &str,
        observation: PositionObservation,
    ) -> Result<TrackTransition> {
        self.observation_service
            .observe_position(id, observation)
            .await
    }

    pub async fn observe_order_with_absorb_result(
        &self,
        id: &str,
        observation: OrderObservation,
    ) -> Result<(TrackTransition, OrderUpdateAbsorbResult)> {
        self.observation_service
            .observe_order_with_absorb_result(id, observation)
            .await
    }

    pub async fn apply_track_ledger_event(
        &self,
        id: &str,
        event: TrackLedgerEvent,
    ) -> Result<ApplyTrackLedgerEventResult> {
        self.observation_service
            .apply_track_ledger_event(id, event)
            .await
    }

    pub async fn sync_exchange_state(
        &self,
        id: &str,
        position: PositionObservation,
        open_orders: Vec<OrderObservation>,
    ) -> Result<TrackTransition> {
        self.observation_service
            .sync_exchange_state(id, position, open_orders)
            .await
    }

    pub async fn sync_exchange_state_without_reconcile(
        &self,
        id: &str,
        position: PositionObservation,
        open_orders: Vec<OrderObservation>,
    ) -> Result<TrackTransition> {
        self.observation_service
            .sync_exchange_state_without_reconcile(id, position, open_orders)
            .await
    }

    pub async fn prepare_submit_execution(
        &self,
        id: &str,
        effect_id: &str,
        request: &OrderRequest,
        desired_exposure: poise_core::types::Exposure,
        live_order: Option<&ExchangeOrder>,
    ) -> Result<Option<PreparedSubmitExecution>> {
        self.effect_service
            .prepare_submit_execution(id, effect_id, request, desired_exposure, live_order)
            .await
    }

    pub async fn recover_submit_effect(
        &self,
        id: &str,
        effect_id: &str,
        request: &OrderRequest,
        desired_exposure: poise_core::types::Exposure,
        live_order: Option<&ExchangeOrder>,
    ) -> Result<SubmitRecoveryResolution> {
        self.effect_service
            .recover_submit_effect(id, effect_id, request, desired_exposure, live_order)
            .await
    }

    pub async fn complete_submit_execution(
        &self,
        id: &str,
        effect_id: &str,
        request: &OrderRequest,
        desired_exposure: poise_core::types::Exposure,
        receipt: &OrderReceipt,
    ) -> Result<()> {
        self.effect_service
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
        self.effect_service
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
        self.effect_service
            .record_cancel_order_success(id, effect_id, batch_id, sequence, order_id)
            .await
    }

    pub async fn record_cancel_all_success(&self, id: &str, effect_id: &str) -> Result<()> {
        self.effect_service
            .record_cancel_all_success(id, effect_id)
            .await
    }

    pub async fn complete_effect_succeeded(&self, id: &str, effect_id: &str) -> Result<()> {
        self.effect_service
            .complete_effect_succeeded(id, effect_id)
            .await
    }

    pub async fn complete_effect_failed(
        &self,
        id: &str,
        effect_id: &str,
        error: &str,
    ) -> Result<()> {
        self.effect_service
            .complete_effect_failed(id, effect_id, error)
            .await
    }

    pub async fn retire_stale_follow_up_submit(
        &self,
        id: &str,
        request: &FollowUpRetirementRequest,
    ) -> Result<bool> {
        self.effect_service
            .retire_stale_follow_up_submit(id, request)
            .await
    }

    pub async fn request_follow_up_retirement(
        &self,
        id: &str,
        request: FollowUpRetirementRequest,
    ) -> Result<()> {
        self.effect_service
            .request_follow_up_retirement(id, request)
            .await
    }

    pub fn manager(&self) -> Arc<RwLock<TrackManager>> {
        self.command_service.manager()
    }
}

pub use poise_application::TrackMutationError;
