use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use grid_engine::ports::{ExchangePort, OrderRequest, OrderStatus, PersistedGridEffect};
use grid_engine::runtime::PendingOrder;
use grid_engine::transition::GridEffect;
use tokio::task::JoinHandle;
use tokio::time::sleep;

use crate::assembly::ServerState;
use crate::notifications::GridInternalNotification;

#[derive(Clone)]
pub struct EffectWorker {
    state: ServerState,
    exchange: Arc<dyn ExchangePort>,
    poll_interval: Duration,
}

impl EffectWorker {
    pub fn new(
        state: ServerState,
        exchange: Arc<dyn ExchangePort>,
        poll_interval: Duration,
    ) -> Self {
        Self {
            state,
            exchange,
            poll_interval,
        }
    }

    pub fn spawn(&self) -> JoinHandle<()> {
        let worker = self.clone();
        tokio::spawn(async move {
            loop {
                if let Err(error) = worker.run_once().await {
                    tracing::warn!("effect worker iteration failed: {error}");
                }
                sleep(worker.poll_interval).await;
            }
        })
    }

    pub async fn run_once(&self) -> Result<()> {
        let repository = self.state.write_service.repository();
        let effects = repository.list_pending_effects().await?;
        for effect in effects {
            if let Err(error) = self.process_effect(effect).await {
                tracing::warn!("failed to process persisted effect: {error}");
            }
        }
        Ok(())
    }

    async fn process_effect(&self, persisted: PersistedGridEffect) -> Result<()> {
        match persisted.effect {
            GridEffect::SubmitOrder {
                ref request,
                ref target_exposure,
            } => {
                self.execute_submit(&persisted, request.clone(), target_exposure.clone())
                    .await
            }
            GridEffect::CancelOrder {
                ref instrument,
                ref order_id,
            } => {
                self.execute_cancellation(
                    &persisted,
                    Cancellation::One {
                        instrument: instrument.clone(),
                        order_id: order_id.clone(),
                    },
                )
                .await
            }
            GridEffect::CancelAll { ref instrument } => {
                self.execute_cancellation(
                    &persisted,
                    Cancellation::All {
                        instrument: instrument.clone(),
                    },
                )
                .await
            }
            GridEffect::NoOp => {
                self.state
                    .write_service
                    .repository()
                    .mark_effect_succeeded(&persisted.effect_id)
                    .await?;
                self.notify_effect_state_changed(&persisted.grid_id);
                Ok(())
            }
        }
    }

    async fn execute_submit(
        &self,
        persisted: &PersistedGridEffect,
        request: OrderRequest,
        target_exposure: grid_core::types::Exposure,
    ) -> Result<()> {
        let repository = self.state.write_service.repository();
        match self.handle_recovered_submit(persisted, &request).await? {
            SubmitRecovery::Proceed => {}
            SubmitRecovery::Recovered => return Ok(()),
            SubmitRecovery::AwaitingExchangeState => return Ok(()),
        }

        self.state
            .write_service
            .record_pending_order(
                persisted.grid_id.as_str(),
                PendingOrder {
                    order_id: None,
                    client_order_id: request.client_order_id.clone(),
                    side: request.side,
                    price: request.price,
                    quantity: request.quantity,
                    target_exposure: target_exposure.clone(),
                    status: OrderStatus::Submitting,
                },
            )
            .await?;

        match self.exchange.submit_order(request.clone()).await {
            Ok(receipt) => {
                let submitted = PendingOrder {
                    order_id: Some(receipt.order_id),
                    client_order_id: receipt.client_order_id,
                    side: request.side,
                    price: request.price,
                    quantity: request.quantity,
                    target_exposure,
                    status: receipt.status,
                };

                if let Err(error) = self
                    .state
                    .write_service
                    .record_pending_order(persisted.grid_id.as_str(), submitted)
                    .await
                {
                    repository
                        .mark_effect_failed(&persisted.effect_id, &error.to_string())
                        .await?;
                    return Err(error);
                }

                repository
                    .mark_effect_succeeded(&persisted.effect_id)
                    .await?;
                self.notify_effect_state_changed(&persisted.grid_id);
                Ok(())
            }
            Err(error) => {
                let failure_message = match self
                    .state
                    .write_service
                    .clear_pending_order(persisted.grid_id.as_str())
                    .await
                {
                    Ok(()) => error.to_string(),
                    Err(clear_error) => format!(
                        "submit order failed: {error}; failed to clear submitting pending order: {clear_error}"
                    ),
                };

                repository
                    .mark_effect_failed(&persisted.effect_id, &failure_message)
                    .await?;
                self.notify_effect_state_changed(&persisted.grid_id);
                Err(anyhow!(failure_message))
            }
        }
    }

    async fn handle_recovered_submit(
        &self,
        persisted: &PersistedGridEffect,
        request: &OrderRequest,
    ) -> Result<SubmitRecovery> {
        let repository = self.state.write_service.repository();
        let Some(snapshot) = repository
            .load_grid_state(persisted.grid_id.as_str())
            .await?
        else {
            return Ok(SubmitRecovery::Proceed);
        };
        let Some(pending) = snapshot.pending_order else {
            return Ok(SubmitRecovery::Proceed);
        };
        if pending.client_order_id != request.client_order_id {
            return Ok(SubmitRecovery::Proceed);
        }

        if pending.order_id.is_some() || pending.status != OrderStatus::Submitting {
            repository
                .mark_effect_succeeded(&persisted.effect_id)
                .await?;
            return Ok(SubmitRecovery::Recovered);
        }

        if let Some(order) = self
            .exchange
            .get_open_orders(&request.instrument)
            .await?
            .into_iter()
            .find(|order| order.client_order_id == request.client_order_id)
        {
            self.state
                .write_service
                .record_pending_order(
                    persisted.grid_id.as_str(),
                    PendingOrder {
                        order_id: Some(order.order_id),
                        client_order_id: order.client_order_id,
                        side: order.side,
                        price: order.price,
                        quantity: order.qty,
                        target_exposure: pending.target_exposure,
                        status: order.status,
                    },
                )
                .await?;
            repository
                .mark_effect_succeeded(&persisted.effect_id)
                .await?;
            self.notify_effect_state_changed(&persisted.grid_id);
            return Ok(SubmitRecovery::Recovered);
        }

        Ok(SubmitRecovery::AwaitingExchangeState)
    }

    async fn execute_cancellation(
        &self,
        persisted: &PersistedGridEffect,
        cancellation: Cancellation,
    ) -> Result<()> {
        let repository = self.state.write_service.repository();
        let result = match cancellation {
            Cancellation::One {
                instrument,
                order_id,
            } => self.exchange.cancel_order(&instrument, &order_id).await,
            Cancellation::All { instrument } => self.exchange.cancel_all(&instrument).await,
        };

        match result {
            Ok(()) => {
                repository
                    .mark_effect_succeeded(&persisted.effect_id)
                    .await?;
                self.notify_effect_state_changed(&persisted.grid_id);
                Ok(())
            }
            Err(error) => {
                repository
                    .mark_effect_failed(&persisted.effect_id, &error.to_string())
                    .await?;
                self.notify_effect_state_changed(&persisted.grid_id);
                Err(error)
            }
        }
    }

    fn notify_effect_state_changed(&self, grid_id: &grid_engine::grid::GridId) {
        self.state.write_service.emit_internal_notification(
            GridInternalNotification::GridEffectStateChanged {
                grid_id: grid_id.clone(),
            },
        );
    }
}

enum Cancellation {
    One {
        instrument: grid_engine::grid::Instrument,
        order_id: String,
    },
    All {
        instrument: grid_engine::grid::Instrument,
    },
}

enum SubmitRecovery {
    Proceed,
    Recovered,
    AwaitingExchangeState,
}
