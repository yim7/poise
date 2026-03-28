use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use grid_engine::manager::SubmitRecoveryResolution;
use grid_engine::ports::{ExchangePort, OrderRequest, PersistedGridEffect};
use grid_engine::transition::GridEffect;
use tokio::task::JoinHandle;
use tokio::time::sleep;

use crate::assembly::ServerState;

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
        let mut seen_effects = HashSet::new();

        loop {
            let Some(effect) = self
                .state
                .effect_service
                .list_pending_effects()
                .await?
                .into_iter()
                .find(|effect| !seen_effects.contains(&effect.effect_id))
            else {
                break;
            };
            let effect_id = effect.effect_id.clone();
            if let Err(error) = self.process_effect(effect).await {
                tracing::warn!("failed to process persisted effect: {error}");
            }
            seen_effects.insert(effect_id);
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
                    .effect_service
                    .complete_effect_succeeded(persisted.grid_id.as_str(), &persisted.effect_id)
                    .await?;
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
        match self
            .handle_recovered_submit(persisted, &request, target_exposure.clone())
            .await?
        {
            SubmitRecovery::Proceed => {}
            SubmitRecovery::Recovered | SubmitRecovery::AwaitExchangeState => return Ok(()),
        }

        match self.exchange.submit_order(request.clone()).await {
            Ok(receipt) => {
                if let Err(error) = self
                    .state
                    .write_service
                    .record_submit_receipt(
                        persisted.grid_id.as_str(),
                        &request,
                        target_exposure,
                        &receipt,
                    )
                    .await
                {
                    self.state
                        .effect_service
                        .complete_effect_failed(
                            persisted.grid_id.as_str(),
                            &persisted.effect_id,
                            &error.to_string(),
                        )
                        .await?;
                    return Err(error);
                }

                self.state
                    .effect_service
                    .complete_effect_succeeded(persisted.grid_id.as_str(), &persisted.effect_id)
                    .await?;
                Ok(())
            }
            Err(error) => {
                match self
                    .state
                    .write_service
                    .clear_pending_submit(persisted.grid_id.as_str(), &request.client_order_id)
                    .await
                {
                    Ok(()) => {
                        let failure_message = error.to_string();
                        self.state
                            .effect_service
                            .complete_effect_failed(
                                persisted.grid_id.as_str(),
                                &persisted.effect_id,
                                &failure_message,
                            )
                            .await?;
                        Err(anyhow!(failure_message))
                    }
                    Err(clear_error) => Err(anyhow!(
                        "submit order failed: {error}; failed to clear submitting pending order: {clear_error}"
                    )),
                }
            }
        }
    }

    async fn handle_recovered_submit(
        &self,
        persisted: &PersistedGridEffect,
        request: &OrderRequest,
        target_exposure: grid_core::types::Exposure,
    ) -> Result<SubmitRecovery> {
        let Some(snapshot) = self
            .state
            .effect_service
            .load_grid_state(persisted.grid_id.as_str())
            .await?
        else {
            return Ok(SubmitRecovery::Proceed);
        };
        let restored_pending = snapshot
            .pending_order
            .as_ref()
            .filter(|pending| pending.client_order_id == request.client_order_id);

        let live_order = if restored_pending
            .and_then(|pending| pending.order_id.as_ref())
            .is_some()
        {
            self.exchange
                .get_open_orders(&request.instrument)
                .await?
                .into_iter()
                .find(|order| order.client_order_id == request.client_order_id)
        } else {
            None
        };

        match self
            .state
            .write_service
            .recover_submit_effect(
                persisted.grid_id.as_str(),
                &persisted.effect_id,
                request,
                target_exposure,
                live_order.as_ref(),
            )
            .await?
        {
            SubmitRecoveryResolution::Proceed => Ok(SubmitRecovery::Proceed),
            SubmitRecoveryResolution::AwaitExchangeState => Ok(SubmitRecovery::AwaitExchangeState),
            SubmitRecoveryResolution::Succeeded | SubmitRecoveryResolution::Superseded => {
                Ok(SubmitRecovery::Recovered)
            }
        }
    }

    async fn execute_cancellation(
        &self,
        persisted: &PersistedGridEffect,
        cancellation: Cancellation,
    ) -> Result<()> {
        let result = match cancellation {
            Cancellation::One {
                instrument,
                order_id,
            } => self.exchange.cancel_order(&instrument, &order_id).await,
            Cancellation::All { instrument } => self.exchange.cancel_all(&instrument).await,
        };

        match result {
            Ok(()) => {
                self.state
                    .effect_service
                    .complete_effect_succeeded(persisted.grid_id.as_str(), &persisted.effect_id)
                    .await?;
                Ok(())
            }
            Err(error) => {
                self.state
                    .effect_service
                    .complete_effect_failed(
                        persisted.grid_id.as_str(),
                        &persisted.effect_id,
                        &error.to_string(),
                    )
                    .await?;
                Err(error)
            }
        }
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
    AwaitExchangeState,
}
