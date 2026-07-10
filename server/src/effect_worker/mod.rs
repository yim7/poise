use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use poise_application::{SessionEffectOutcome, SessionTrackEffect};
use poise_core::track::Instrument;
use poise_engine::executor::SubmitRecoveryToken;
use poise_engine::ports::{
    AccountPort, ExecutionPort, ExecutionPortError, ExecutionPortErrorKind, OrderRequest,
};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::sleep;

use crate::order_outcome::OutcomeUnknownRecovery;
use crate::runtime::RuntimeHealthComponent;
use crate::server_context::EffectWorkerState;

mod dispatch;
mod execute;
mod retry;

const DEFAULT_ERROR_BACKOFF_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone)]
pub struct EffectWorker {
    state: EffectWorkerState,
    execution: Arc<dyn ExecutionPort>,
    account: Arc<dyn AccountPort>,
    poll_interval: Duration,
    error_backoff_interval: Duration,
    shutdown_rx: watch::Receiver<bool>,
}

impl EffectWorker {
    #[cfg(test)]
    pub fn new(
        state: impl Into<EffectWorkerState>,
        execution: Arc<dyn ExecutionPort>,
        account: Arc<dyn AccountPort>,
        poll_interval: Duration,
    ) -> Self {
        let (_, shutdown_rx) = watch::channel(false);
        Self::with_shutdown_rx(state, execution, account, poll_interval, shutdown_rx)
    }

    pub fn with_shutdown_rx(
        state: impl Into<EffectWorkerState>,
        execution: Arc<dyn ExecutionPort>,
        account: Arc<dyn AccountPort>,
        poll_interval: Duration,
        shutdown_rx: watch::Receiver<bool>,
    ) -> Self {
        Self::with_shutdown_rx_and_error_backoff(
            state,
            execution,
            account,
            poll_interval,
            DEFAULT_ERROR_BACKOFF_INTERVAL,
            shutdown_rx,
        )
    }

    fn with_shutdown_rx_and_error_backoff(
        state: impl Into<EffectWorkerState>,
        execution: Arc<dyn ExecutionPort>,
        account: Arc<dyn AccountPort>,
        poll_interval: Duration,
        error_backoff_interval: Duration,
        shutdown_rx: watch::Receiver<bool>,
    ) -> Self {
        Self {
            state: state.into(),
            execution,
            account,
            poll_interval,
            error_backoff_interval,
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
                    if let Err(error) = self.run_once().await {
                        self.state.runtime_health.record_error_now(
                            RuntimeHealthComponent::EffectWorker,
                            error.to_string(),
                        );
                        tracing::warn!("effect worker iteration failed: {error}");
                        if wait_for_backoff_or_shutdown(
                            &mut shutdown_rx,
                            self.error_backoff_interval,
                        )
                        .await
                        {
                            return Ok(());
                        }
                    } else {
                        self.state
                            .runtime_health
                            .record_success_now(RuntimeHealthComponent::EffectWorker);
                    }
                }
            }
        }
    }

    pub async fn run_once(&self) -> Result<()> {
        dispatch::run_once(self).await
    }

    async fn process_effect(
        &self,
        effect: SessionTrackEffect,
    ) -> Result<dispatch::SessionDispatchResult> {
        dispatch::process_effect(self, effect).await
    }

    async fn execute_submit(
        &self,
        effect: &SessionTrackEffect,
        request: OrderRequest,
        recovery_token: SubmitRecoveryToken,
        desired_exposure: poise_core::types::Exposure,
    ) -> Result<SessionEffectOutcome> {
        execute::execute_submit(self, effect, request, recovery_token, desired_exposure).await
    }

    async fn execute_cancellation(
        &self,
        effect: &SessionTrackEffect,
        cancellation: Cancellation,
    ) -> Result<dispatch::SessionDispatchResult> {
        execute::execute_cancellation(self, effect, cancellation).await
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

async fn wait_for_backoff_or_shutdown(
    shutdown_rx: &mut watch::Receiver<bool>,
    backoff_interval: Duration,
) -> bool {
    if *shutdown_rx.borrow() {
        return true;
    }

    tokio::select! {
        biased;
        changed = shutdown_rx.changed() => changed.is_err() || *shutdown_rx.borrow(),
        _ = sleep(backoff_interval) => false,
    }
}

fn is_insufficient_margin_failure(error: &ExecutionPortError) -> bool {
    error.kind() == ExecutionPortErrorKind::InsufficientMargin
}

enum Cancellation {
    One {
        instrument: poise_core::track::Instrument,
        order_id: String,
    },
    All {
        instrument: poise_core::track::Instrument,
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
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use chrono::Utc;
    use poise_core::track::{Instrument, TrackId, Venue};
    use poise_engine::observation::MarketObservation;
    use poise_engine::ports::{
        ExchangeOpenOrderSnapshot, ExecutionPort, ExecutionQuote, OrderReceipt, OrderRequest,
        OrderStatus, Position,
    };
    use poise_storage::sqlite::SqliteStorage;
    use tokio::sync::{Notify, Semaphore, watch};
    use tokio::time::timeout;

    use crate::effect_worker::EffectWorker;
    use crate::test_support::{NoopAccountPort, build_effect_worker_context_for_repository};

    #[test]
    fn insufficient_margin_detection_uses_execution_error_kind_not_message_text() {
        let typed_error = poise_engine::ports::ExecutionPortError::new(
            poise_engine::ports::ExecutionPortErrorKind::InsufficientMargin,
            anyhow::anyhow!("exchange-specific margin rejection"),
        );
        let string_only_error =
            poise_engine::ports::ExecutionPortError::failed(r#"{"code":-2019}"#);

        assert!(super::is_insufficient_margin_failure(&typed_error));
        assert!(!super::is_insufficient_margin_failure(&string_only_error));
    }

    #[tokio::test]
    async fn protection_after_submit_prepare_dispatches_cancel_follow_up() {
        let repository = Arc::new(SqliteStorage::in_memory().unwrap());
        let effect_worker_context = build_effect_worker_context_for_repository(repository);
        let observation_service = Arc::clone(
            &effect_worker_context
                .effect_worker_state
                .reconcile
                .observation_service,
        );
        observation_service
            .observe_market(
                "btc-core",
                MarketObservation::ExecutionQuote {
                    execution_quote: ExecutionQuote {
                        best_bid: 94.9,
                        best_ask: 95.1,
                    },
                },
            )
            .await
            .unwrap();

        let execution = Arc::new(BlockingSubmitExecution::default());
        let worker = EffectWorker::new(
            effect_worker_context.clone(),
            execution.clone(),
            Arc::new(NoopAccountPort),
            Duration::from_millis(1),
        );
        let worker_task = tokio::spawn(async move { worker.run_once().await });

        timeout(Duration::from_secs(1), execution.wait_for_submit_started())
            .await
            .expect("submit should reach the exchange after prepare");
        observation_service
            .observe_market(
                "btc-core",
                MarketObservation::ExecutionQuote {
                    execution_quote: ExecutionQuote {
                        best_bid: 84.9,
                        best_ask: 85.1,
                    },
                },
            )
            .await
            .unwrap();
        execution.release_submit();

        timeout(Duration::from_secs(1), worker_task)
            .await
            .expect("worker should finish submit writeback and cancel follow-up")
            .unwrap()
            .unwrap();

        assert_eq!(execution.submit_call_count(), 1);
        assert_eq!(
            execution.cancelled_order_ids(),
            vec!["accepted-before-protection".to_string()]
        );
        assert_eq!(
            effect_worker_context
                .effect_worker_state
                .session_effect_queue
                .pending_effect_count_for_test(&TrackId::new("btc-core")),
            0
        );
    }

    #[tokio::test]
    async fn worker_continues_after_iteration_error() {
        let repository = Arc::new(SqliteStorage::in_memory().unwrap());
        let effect_worker_context = build_effect_worker_context_for_repository(repository);
        let track_id = TrackId::new("btc-core");
        let instrument = Instrument::new(Venue::Binance, "BTCUSDT");
        effect_worker_context
            .effect_worker_state
            .session_effect_queue
            .enqueue_transition_effects_for_test(
                &track_id,
                &[poise_engine::execution_plan::TrackEffect::CancelOrder {
                    instrument: instrument.clone(),
                    order_id: "unknown-order".into(),
                }],
                Utc::now(),
            );

        let execution = Arc::new(ReconcileFailThenRecordExecution::default());
        let account = Arc::new(NoopAccountPort);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let worker = EffectWorker::with_shutdown_rx_and_error_backoff(
            effect_worker_context.clone(),
            execution.clone(),
            account,
            Duration::from_millis(1),
            Duration::from_millis(10),
            shutdown_rx,
        );
        let worker_task = tokio::spawn(async move { worker.run_until_shutdown().await });

        execution.wait_for_reconcile_attempt().await;
        let follow_up_track_id = TrackId::new("eth-core");
        let follow_up_instrument = Instrument::new(Venue::Binance, "ETHUSDT");
        effect_worker_context
            .effect_worker_state
            .session_effect_queue
            .enqueue_transition_effects_for_test(
                &follow_up_track_id,
                &[poise_engine::execution_plan::TrackEffect::CancelAll {
                    instrument: follow_up_instrument,
                }],
                Utc::now(),
            );

        timeout(Duration::from_millis(100), execution.wait_for_cancel_all())
            .await
            .expect("effect worker should continue processing effects after one failed iteration");
        shutdown_tx.send(true).unwrap();
        worker_task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn worker_backs_off_after_iteration_error_before_next_poll() {
        let repository = Arc::new(SqliteStorage::in_memory().unwrap());
        let effect_worker_context = build_effect_worker_context_for_repository(repository);
        let track_id = TrackId::new("btc-core");
        let instrument = Instrument::new(Venue::Binance, "BTCUSDT");
        effect_worker_context
            .effect_worker_state
            .session_effect_queue
            .enqueue_transition_effects_for_test(
                &track_id,
                &[poise_engine::execution_plan::TrackEffect::CancelOrder {
                    instrument: instrument.clone(),
                    order_id: "unknown-order".into(),
                }],
                Utc::now(),
            );

        let execution = Arc::new(ReconcileFailThenRecordExecution::default());
        let account = Arc::new(NoopAccountPort);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let worker = EffectWorker::with_shutdown_rx_and_error_backoff(
            effect_worker_context.clone(),
            execution.clone(),
            account,
            Duration::from_millis(1),
            Duration::from_millis(75),
            shutdown_rx,
        );
        let worker_task = tokio::spawn(async move { worker.run_until_shutdown().await });

        execution.wait_for_reconcile_attempt().await;
        let follow_up_track_id = TrackId::new("eth-core");
        let follow_up_instrument = Instrument::new(Venue::Binance, "ETHUSDT");
        effect_worker_context
            .effect_worker_state
            .session_effect_queue
            .enqueue_transition_effects_for_test(
                &follow_up_track_id,
                &[poise_engine::execution_plan::TrackEffect::CancelAll {
                    instrument: follow_up_instrument,
                }],
                Utc::now(),
            );

        timeout(Duration::from_millis(20), execution.wait_for_cancel_all())
            .await
            .expect_err("effect worker should back off after a failed iteration");
        timeout(Duration::from_millis(200), execution.wait_for_cancel_all())
            .await
            .expect("effect worker should continue after the error backoff interval");
        shutdown_tx.send(true).unwrap();
        worker_task.await.unwrap().unwrap();
    }

    #[derive(Default)]
    struct ReconcileFailThenRecordExecution {
        reconcile_attempts: AtomicUsize,
        cancel_all_calls: AtomicUsize,
        reconcile_notify: Notify,
        cancel_all_notify: Notify,
    }

    struct BlockingSubmitExecution {
        submit_calls: AtomicUsize,
        submit_started: Notify,
        submit_release: Semaphore,
        cancelled_order_ids: Mutex<Vec<String>>,
    }

    impl Default for BlockingSubmitExecution {
        fn default() -> Self {
            Self {
                submit_calls: AtomicUsize::new(0),
                submit_started: Notify::new(),
                submit_release: Semaphore::new(0),
                cancelled_order_ids: Mutex::new(Vec::new()),
            }
        }
    }

    impl BlockingSubmitExecution {
        async fn wait_for_submit_started(&self) {
            loop {
                if self.submit_calls.load(Ordering::SeqCst) > 0 {
                    return;
                }
                self.submit_started.notified().await;
            }
        }

        fn release_submit(&self) {
            self.submit_release.add_permits(1);
        }

        fn submit_call_count(&self) -> usize {
            self.submit_calls.load(Ordering::SeqCst)
        }

        fn cancelled_order_ids(&self) -> Vec<String> {
            self.cancelled_order_ids.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl ExecutionPort for BlockingSubmitExecution {
        async fn submit_order(
            &self,
            request: OrderRequest,
        ) -> poise_engine::ports::ExecutionResult<OrderReceipt> {
            let call = self.submit_calls.fetch_add(1, Ordering::SeqCst);
            self.submit_started.notify_waiters();
            if call == 0 {
                let _permit = self
                    .submit_release
                    .acquire()
                    .await
                    .expect("submit release semaphore should stay open");
            }
            Ok(OrderReceipt {
                order_id: "accepted-before-protection".to_string(),
                client_order_id: request.client_order_id,
                filled_qty: 0.0,
                status: OrderStatus::New,
            })
        }

        async fn cancel_order(
            &self,
            _instrument: &Instrument,
            order_id: &str,
        ) -> poise_engine::ports::ExecutionResult<OrderReceipt> {
            self.cancelled_order_ids
                .lock()
                .unwrap()
                .push(order_id.to_string());
            Ok(OrderReceipt {
                order_id: order_id.to_string(),
                client_order_id: String::new(),
                filled_qty: 0.0,
                status: OrderStatus::Canceled,
            })
        }

        async fn cancel_all(
            &self,
            _instrument: &Instrument,
        ) -> poise_engine::ports::ExecutionResult<()> {
            Ok(())
        }

        async fn get_position(
            &self,
            instrument: &Instrument,
        ) -> poise_engine::ports::ExecutionResult<Position> {
            Ok(Position {
                instrument: instrument.clone(),
                qty: 0.0,
                avg_price: 0.0,
                unrealized_pnl: 0.0,
                mark_price: None,
            })
        }

        async fn get_open_orders(
            &self,
            _instrument: &Instrument,
        ) -> poise_engine::ports::ExecutionResult<ExchangeOpenOrderSnapshot> {
            Ok(ExchangeOpenOrderSnapshot::from_complete_exchange_query(
                Vec::new(),
            ))
        }
    }

    impl ReconcileFailThenRecordExecution {
        async fn wait_for_reconcile_attempt(&self) {
            loop {
                if self.reconcile_attempts.load(Ordering::SeqCst) > 0 {
                    return;
                }
                self.reconcile_notify.notified().await;
            }
        }

        async fn wait_for_cancel_all(&self) {
            loop {
                if self.cancel_all_calls.load(Ordering::SeqCst) > 0 {
                    return;
                }
                self.cancel_all_notify.notified().await;
            }
        }
    }

    #[async_trait::async_trait]
    impl ExecutionPort for ReconcileFailThenRecordExecution {
        async fn submit_order(
            &self,
            req: OrderRequest,
        ) -> poise_engine::ports::ExecutionResult<OrderReceipt> {
            Ok(OrderReceipt {
                order_id: "test-order".into(),
                client_order_id: req.client_order_id,
                filled_qty: 0.0,
                status: OrderStatus::New,
            })
        }

        async fn cancel_order(
            &self,
            _instrument: &Instrument,
            _order_id: &str,
        ) -> poise_engine::ports::ExecutionResult<OrderReceipt> {
            Err(poise_engine::ports::ExecutionPortError::new(
                poise_engine::ports::ExecutionPortErrorKind::CancelOutcomeUnknown,
                anyhow::anyhow!("Unknown order sent."),
            ))
        }

        async fn cancel_all(
            &self,
            _instrument: &Instrument,
        ) -> poise_engine::ports::ExecutionResult<()> {
            self.cancel_all_calls.fetch_add(1, Ordering::SeqCst);
            self.cancel_all_notify.notify_waiters();
            Ok(())
        }

        async fn get_position(
            &self,
            instrument: &Instrument,
        ) -> poise_engine::ports::ExecutionResult<Position> {
            self.reconcile_attempts.fetch_add(1, Ordering::SeqCst);
            self.reconcile_notify.notify_waiters();
            Err(poise_engine::ports::ExecutionPortError::failed(format!(
                "simulated reconcile failure for {}",
                instrument.symbol
            )))
        }

        async fn get_open_orders(
            &self,
            _instrument: &Instrument,
        ) -> poise_engine::ports::ExecutionResult<ExchangeOpenOrderSnapshot> {
            Ok(ExchangeOpenOrderSnapshot::from_complete_exchange_query(
                Vec::new(),
            ))
        }
    }
}
