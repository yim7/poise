use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use poise_application::{PersistedTrackEffect, PreparedSubmitExecution};
use poise_engine::ports::{ExchangePort, OrderRequest};
use poise_engine::track::Instrument;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::sleep;

use crate::order_outcome::OutcomeUnknownRecovery;
use crate::server_context::EffectWorkerState;
use crate::submit_preflight::SubmitPreflightDecision;

mod dispatch;
mod execute;
mod retry;
#[derive(Clone)]
pub struct EffectWorker {
    state: EffectWorkerState,
    exchange: Arc<dyn ExchangePort>,
    poll_interval: Duration,
    shutdown_rx: watch::Receiver<bool>,
}

impl EffectWorker {
    #[cfg(test)]
    pub fn new(
        state: impl Into<EffectWorkerState>,
        exchange: Arc<dyn ExchangePort>,
        poll_interval: Duration,
    ) -> Self {
        let (_, shutdown_rx) = watch::channel(false);
        Self::with_shutdown_rx(state, exchange, poll_interval, shutdown_rx)
    }

    pub fn with_shutdown_rx(
        state: impl Into<EffectWorkerState>,
        exchange: Arc<dyn ExchangePort>,
        poll_interval: Duration,
        shutdown_rx: watch::Receiver<bool>,
    ) -> Self {
        Self {
            state: state.into(),
            exchange,
            poll_interval,
            shutdown_rx,
        }
    }

    pub fn spawn(&self) -> JoinHandle<()> {
        let worker = self.clone();
        tokio::spawn(async move {
            if let Err(error) = worker.run_until_shutdown().await {
                tracing::warn!("effect worker iteration failed: {error}");
            }
        })
    }

    pub async fn run_until_shutdown(&self) -> Result<()> {
        let mut shutdown_rx = self.shutdown_rx.clone();

        loop {
            if *shutdown_rx.borrow() {
                return Ok(());
            }

            tokio::select! {
                biased;
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        return Ok(());
                    }
                }
                _ = sleep(self.poll_interval) => {
                    self.run_once().await?;
                }
            }
        }
    }

    pub async fn run_once(&self) -> Result<()> {
        dispatch::run_once(self).await
    }

    async fn process_effect(&self, persisted: PersistedTrackEffect) -> Result<()> {
        dispatch::process_effect(self, persisted).await
    }

    async fn execute_submit(
        &self,
        persisted: &PersistedTrackEffect,
        request: OrderRequest,
        desired_exposure: poise_core::types::Exposure,
    ) -> Result<()> {
        execute::execute_submit(self, persisted, request, desired_exposure).await
    }

    async fn prepare_submit_execution(
        &self,
        persisted: &PersistedTrackEffect,
        request: &OrderRequest,
        desired_exposure: poise_core::types::Exposure,
        preflight_decision: SubmitPreflightDecision,
    ) -> Result<Option<PreparedSubmitExecution>> {
        execute::prepare_submit_execution(
            self,
            persisted,
            request,
            desired_exposure,
            preflight_decision,
        )
        .await
    }

    async fn execute_cancellation(
        &self,
        persisted: &PersistedTrackEffect,
        cancellation: Cancellation,
    ) -> Result<()> {
        execute::execute_cancellation(self, persisted, cancellation).await
    }

    async fn trigger_immediate_reconcile(
        &self,
        track_id: &str,
        instrument: &Instrument,
        reason: crate::order_outcome::ReconcileReason,
    ) -> Result<()> {
        retry::trigger_immediate_reconcile(self, track_id, instrument, reason).await
    }

    async fn recover_unknown_outcome(
        &self,
        track_id: &str,
        instrument: &Instrument,
        recovery: OutcomeUnknownRecovery,
    ) -> Result<()> {
        retry::recover_unknown_outcome(self, track_id, instrument, recovery).await
    }
}

fn is_insufficient_margin_failure(message: &str) -> bool {
    message.contains(r#""code":-2019"#) || message.contains("Margin is insufficient")
}

enum Cancellation {
    One {
        instrument: poise_engine::track::Instrument,
        order_id: String,
    },
    All {
        instrument: poise_engine::track::Instrument,
    },
}

impl Cancellation {
    fn instrument(&self) -> &Instrument {
        match self {
            Self::One { instrument, .. } | Self::All { instrument } => instrument,
        }
    }
}

#[cfg(test)]
mod tests;
