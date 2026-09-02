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
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use chrono::Utc;
    use poise_application::{
        DeferredUntil, EffectStatus, SessionEffectOutcome, SessionTrackEffect, TrackInput,
        TrackQueryStore,
    };
    use poise_core::track::{Instrument, TrackId, Venue};
    use poise_engine::observation::{MarketObservation, OrderObservation};
    use poise_engine::ports::{
        ExchangeOpenOrderSnapshot, ExchangeOrder, ExecutionPort, ExecutionPortError,
        ExecutionPortErrorKind, ExecutionQuote, OrderReceipt, OrderRequest, OrderStatus, Position,
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
        let runtime = Arc::clone(&effect_worker_context.effect_worker_state.reconcile.runtime);
        runtime
            .handle_market_for_test(
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
        runtime
            .handle_market_for_test(
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
    async fn submit_outcome_unknown_first_clean_snapshot_retains_owner_until_later_confirmation() {
        let repository = Arc::new(SqliteStorage::in_memory().unwrap());
        let effect_worker_context = build_effect_worker_context_for_repository(repository.clone());
        let effect = claim_planned_submit(&effect_worker_context).await;
        let effect_id = effect.effect_id.clone();
        let execution = Arc::new(SubmitOutcomeUnknownExecution::with_clean_snapshot());
        let worker = EffectWorker::new(
            effect_worker_context.clone(),
            execution.clone(),
            Arc::new(NoopAccountPort),
            Duration::from_millis(1),
        );

        let result = worker.process_effect(effect).await.unwrap();

        assert!(matches!(
            result,
            super::dispatch::SessionDispatchResult::Outcome(SessionEffectOutcome::Deferred {
                until: DeferredUntil::ExchangeState,
            })
        ));
        assert_eq!(execution.submit_call_count(), 1);
        assert!(
            effect_worker_context
                .effect_worker_state
                .reconcile
                .exchange_freshness
                .is_stale("btc-core")
                .await,
            "readiness must stay stale while the unknown submit still owns the gap"
        );
        assert!(
            effect_worker_context
                .effect_worker_state
                .session_effect_queue
                .active_submit_effect_ids()
                .contains(&effect_id),
            "the immediate empty snapshot must not prove that the request cannot materialize later"
        );
        let pending = repository
            .list_recent_track_effects(&TrackId::new("btc-core"), 10)
            .await
            .unwrap()
            .into_iter()
            .find(|effect| effect.effect_id == effect_id)
            .expect("the submit effect should remain in the audit journal");
        assert_eq!(pending.status, EffectStatus::Pending);

        effect_worker_context
            .effect_worker_state
            .session_effect_queue
            .record_outcome(
                &effect_id,
                SessionEffectOutcome::Deferred {
                    until: DeferredUntil::ExchangeState,
                },
            );
        worker
            .trigger_immediate_reconcile(
                "btc-core",
                &Instrument::new(Venue::Binance, "BTCUSDT"),
                crate::order_outcome::ReconcileReason::PeriodicAudit,
            )
            .await
            .unwrap();

        assert!(
            !effect_worker_context
                .effect_worker_state
                .session_effect_queue
                .active_submit_effect_ids()
                .contains(&effect_id),
            "a later independent clean snapshot may resolve the old submit owner"
        );
        assert!(
            !effect_worker_context
                .effect_worker_state
                .reconcile
                .exchange_freshness
                .is_stale("btc-core")
                .await,
            "the confirming snapshot may restore exchange freshness after ownership resolves"
        );
        let resolved = repository
            .list_recent_track_effects(&TrackId::new("btc-core"), 10)
            .await
            .unwrap()
            .into_iter()
            .find(|effect| effect.effect_id == effect_id)
            .expect("the submit effect should remain in the audit journal");
        assert_eq!(resolved.status, EffectStatus::Succeeded);
    }

    #[tokio::test]
    async fn ws_order_fact_before_unknown_return_resolves_owner_when_reconcile_fails() {
        let repository = Arc::new(SqliteStorage::in_memory().unwrap());
        let effect_worker_context = build_effect_worker_context_for_repository(repository.clone());
        let effect = claim_planned_submit(&effect_worker_context).await;
        let effect_id = effect.effect_id.clone();
        let request = match &effect.effect {
            poise_engine::execution_plan::TrackEffect::SubmitOrder { request, .. } => {
                request.clone()
            }
            other => panic!("expected submit effect, got {other:?}"),
        };
        let execution = Arc::new(BlockingSubmitOutcomeUnknownExecution::default());
        let worker = EffectWorker::new(
            effect_worker_context.clone(),
            execution.clone(),
            Arc::new(NoopAccountPort),
            Duration::from_millis(1),
        );
        let worker_task = {
            let worker = worker.clone();
            tokio::spawn(async move { worker.process_effect(effect).await })
        };

        timeout(Duration::from_secs(1), execution.wait_for_submit_started())
            .await
            .expect("submit should reach the exchange before the WS fact arrives");
        effect_worker_context
            .effect_worker_state
            .reconcile
            .runtime
            .handle(
                &TrackId::new("btc-core"),
                TrackInput::Order(OrderObservation {
                    order_id: "accepted-before-network-error".to_string(),
                    client_order_id: request.client_order_id,
                    side: request.side,
                    price: request.price,
                    quantity: request.quantity,
                    filled_qty: 0.0,
                    status: OrderStatus::New,
                }),
            )
            .await
            .unwrap();
        execution.release_submit();

        let result = timeout(Duration::from_secs(1), worker_task)
            .await
            .expect("worker should finish after the blocked submit returns unknown")
            .unwrap()
            .unwrap();

        assert!(matches!(
            result,
            super::dispatch::SessionDispatchResult::Outcome(SessionEffectOutcome::Deferred {
                until: DeferredUntil::ExchangeState,
            })
        ));
        assert!(
            !effect_worker_context
                .effect_worker_state
                .session_effect_queue
                .active_submit_effect_ids()
                .contains(&effect_id),
            "the already-absorbed WS fact should resolve the old submit owner"
        );
        let persisted = repository
            .list_recent_track_effects(&TrackId::new("btc-core"), 10)
            .await
            .unwrap()
            .into_iter()
            .find(|effect| effect.effect_id == effect_id)
            .expect("the submit effect should remain in the audit journal");
        assert_eq!(persisted.status, EffectStatus::Succeeded);
    }

    #[tokio::test]
    async fn submit_outcome_unknown_failed_reconcile_stays_deferred_without_resubmit() {
        let repository = Arc::new(SqliteStorage::in_memory().unwrap());
        let effect_worker_context = build_effect_worker_context_for_repository(repository.clone());
        let effect = claim_planned_submit(&effect_worker_context).await;
        let effect_id = effect.effect_id.clone();
        let execution = Arc::new(SubmitOutcomeUnknownExecution::with_failed_reconcile());
        let worker = EffectWorker::new(
            effect_worker_context.clone(),
            execution.clone(),
            Arc::new(NoopAccountPort),
            Duration::from_millis(1),
        );

        let result = worker.process_effect(effect).await.unwrap();

        assert!(matches!(
            result,
            super::dispatch::SessionDispatchResult::Outcome(SessionEffectOutcome::Deferred {
                until: DeferredUntil::ExchangeState,
            })
        ));
        effect_worker_context
            .effect_worker_state
            .session_effect_queue
            .record_outcome(
                &effect_id,
                SessionEffectOutcome::Deferred {
                    until: DeferredUntil::ExchangeState,
                },
            );
        assert!(
            effect_worker_context
                .effect_worker_state
                .session_effect_queue
                .active_submit_effect_ids()
                .contains(&effect_id),
            "failed reconciliation must retain the submit owner"
        );

        worker.run_once().await.unwrap();

        assert_eq!(
            execution.submit_call_count(),
            1,
            "an unresolved submit must never be dispatched a second time"
        );
        let persisted = repository
            .list_recent_track_effects(&TrackId::new("btc-core"), 10)
            .await
            .unwrap()
            .into_iter()
            .find(|effect| effect.effect_id == effect_id)
            .expect("the unresolved submit should remain in the audit journal");
        assert_eq!(persisted.status, EffectStatus::Pending);
    }

    #[tokio::test]
    async fn submit_outcome_unknown_failed_first_snapshot_needs_two_later_clean_snapshots() {
        let repository = Arc::new(SqliteStorage::in_memory().unwrap());
        let effect_worker_context = build_effect_worker_context_for_repository(repository.clone());
        let effect = claim_planned_submit(&effect_worker_context).await;
        let effect_id = effect.effect_id.clone();
        let execution = Arc::new(SubmitOutcomeUnknownExecution::with_failed_reconcile());
        let worker = EffectWorker::new(
            effect_worker_context.clone(),
            execution,
            Arc::new(NoopAccountPort),
            Duration::from_millis(1),
        );

        let result = worker.process_effect(effect).await.unwrap();
        assert!(matches!(
            result,
            super::dispatch::SessionDispatchResult::Outcome(SessionEffectOutcome::Deferred {
                until: DeferredUntil::ExchangeState,
            })
        ));
        effect_worker_context
            .effect_worker_state
            .session_effect_queue
            .record_outcome(
                &effect_id,
                SessionEffectOutcome::Deferred {
                    until: DeferredUntil::ExchangeState,
                },
            );

        worker
            .trigger_immediate_reconcile(
                "btc-core",
                &Instrument::new(Venue::Binance, "BTCUSDT"),
                crate::order_outcome::ReconcileReason::PeriodicAudit,
            )
            .await
            .unwrap();
        assert!(
            effect_worker_context
                .effect_worker_state
                .session_effect_queue
                .active_submit_effect_ids()
                .contains(&effect_id),
            "the first successful snapshot after a failed immediate sync only establishes the first clean observation"
        );
        assert!(
            effect_worker_context
                .effect_worker_state
                .reconcile
                .exchange_freshness
                .is_stale("btc-core")
                .await,
            "the first clean snapshot must not restore readiness while ownership remains"
        );
        let pending = repository
            .list_recent_track_effects(&TrackId::new("btc-core"), 10)
            .await
            .unwrap()
            .into_iter()
            .find(|effect| effect.effect_id == effect_id)
            .expect("the submit effect should remain in the audit journal");
        assert_eq!(pending.status, EffectStatus::Pending);

        worker
            .trigger_immediate_reconcile(
                "btc-core",
                &Instrument::new(Venue::Binance, "BTCUSDT"),
                crate::order_outcome::ReconcileReason::PeriodicAudit,
            )
            .await
            .unwrap();
        assert!(
            !effect_worker_context
                .effect_worker_state
                .session_effect_queue
                .active_submit_effect_ids()
                .contains(&effect_id),
            "a second independent clean snapshot may resolve the unknown submit"
        );
        assert!(
            !effect_worker_context
                .effect_worker_state
                .reconcile
                .exchange_freshness
                .is_stale("btc-core")
                .await,
            "the second clean snapshot may restore readiness after ownership resolves"
        );
        let resolved = repository
            .list_recent_track_effects(&TrackId::new("btc-core"), 10)
            .await
            .unwrap()
            .into_iter()
            .find(|effect| effect.effect_id == effect_id)
            .expect("the submit effect should remain in the audit journal");
        assert_eq!(resolved.status, EffectStatus::Succeeded);
    }

    #[tokio::test]
    async fn live_order_on_first_successful_snapshot_resolves_initial_owner_and_runs_cancel() {
        let repository = Arc::new(SqliteStorage::in_memory().unwrap());
        let effect_worker_context = build_effect_worker_context_for_repository(repository.clone());
        let effect = claim_planned_submit(&effect_worker_context).await;
        let effect_id = effect.effect_id.clone();
        let execution = Arc::new(FailThenLiveOrderExecution::default());
        let worker = EffectWorker::new(
            effect_worker_context.clone(),
            execution.clone(),
            Arc::new(NoopAccountPort),
            Duration::from_millis(1),
        );

        let result = worker.process_effect(effect).await.unwrap();
        assert!(matches!(
            result,
            super::dispatch::SessionDispatchResult::Outcome(SessionEffectOutcome::Deferred {
                until: DeferredUntil::ExchangeState,
            })
        ));
        effect_worker_context
            .effect_worker_state
            .session_effect_queue
            .record_outcome(
                &effect_id,
                SessionEffectOutcome::Deferred {
                    until: DeferredUntil::ExchangeState,
                },
            );
        effect_worker_context
            .effect_worker_state
            .reconcile
            .runtime
            .handle_market_for_test(
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

        worker
            .trigger_immediate_reconcile(
                "btc-core",
                &Instrument::new(Venue::Binance, "BTCUSDT"),
                crate::order_outcome::ReconcileReason::PeriodicAudit,
            )
            .await
            .unwrap();
        assert!(
            !effect_worker_context
                .effect_worker_state
                .session_effect_queue
                .active_submit_effect_ids()
                .contains(&effect_id),
            "a matching live order should resolve the initial unknown owner immediately"
        );
        worker.run_once().await.unwrap();

        assert_eq!(
            execution.cancelled_order_ids(),
            vec!["accepted-after-first-sync-failed".to_string()]
        );
        let persisted = repository
            .list_recent_track_effects(&TrackId::new("btc-core"), 10)
            .await
            .unwrap()
            .into_iter()
            .find(|effect| effect.effect_id == effect_id)
            .expect("the submit effect should remain in the audit journal");
        assert_eq!(persisted.status, EffectStatus::Succeeded);
    }

    #[tokio::test]
    async fn anomalous_initial_snapshot_does_not_consume_absence_confirmation() {
        let repository = Arc::new(SqliteStorage::in_memory().unwrap());
        let effect_worker_context = build_effect_worker_context_for_repository(repository);
        let effect = claim_planned_submit(&effect_worker_context).await;
        let effect_id = effect.effect_id.clone();
        let execution = Arc::new(AnomalousFirstSnapshotExecution::default());
        let worker = EffectWorker::new(
            effect_worker_context.clone(),
            execution,
            Arc::new(NoopAccountPort),
            Duration::from_millis(1),
        );

        let result = worker.process_effect(effect).await.unwrap();
        assert!(matches!(
            result,
            super::dispatch::SessionDispatchResult::Outcome(SessionEffectOutcome::Deferred {
                until: DeferredUntil::ExchangeState,
            })
        ));
        effect_worker_context
            .effect_worker_state
            .session_effect_queue
            .record_outcome(
                &effect_id,
                SessionEffectOutcome::Deferred {
                    until: DeferredUntil::ExchangeState,
                },
            );

        worker
            .trigger_immediate_reconcile(
                "btc-core",
                &Instrument::new(Venue::Binance, "BTCUSDT"),
                crate::order_outcome::ReconcileReason::PeriodicAudit,
            )
            .await
            .unwrap();
        assert!(
            effect_worker_context
                .effect_worker_state
                .session_effect_queue
                .active_submit_effect_ids()
                .contains(&effect_id),
            "the first clean snapshot after an anomaly may only establish initial confirmation"
        );
        assert!(
            effect_worker_context
                .effect_worker_state
                .reconcile
                .exchange_freshness
                .is_stale("btc-core")
                .await,
            "readiness must remain stale after promotion while the owner is unresolved"
        );

        worker
            .trigger_immediate_reconcile(
                "btc-core",
                &Instrument::new(Venue::Binance, "BTCUSDT"),
                crate::order_outcome::ReconcileReason::PeriodicAudit,
            )
            .await
            .unwrap();
        assert!(
            !effect_worker_context
                .effect_worker_state
                .session_effect_queue
                .active_submit_effect_ids()
                .contains(&effect_id)
        );
        assert!(
            !effect_worker_context
                .effect_worker_state
                .reconcile
                .exchange_freshness
                .is_stale("btc-core")
                .await
        );
    }

    #[tokio::test]
    async fn submit_fill_between_open_orders_and_position_queries_does_not_plan_replacement() {
        let repository = Arc::new(SqliteStorage::in_memory().unwrap());
        let effect_worker_context = build_effect_worker_context_for_repository(repository);
        let effect = claim_planned_submit_at_quote(&effect_worker_context, 98.9, 99.1).await;
        let effect_id = effect.effect_id.clone();
        let execution = Arc::new(FillBetweenSnapshotQueriesExecution::default());
        let worker = EffectWorker::new(
            effect_worker_context.clone(),
            execution.clone(),
            Arc::new(NoopAccountPort),
            Duration::from_millis(1),
        );

        let result = worker.process_effect(effect).await.unwrap();
        assert!(matches!(
            result,
            super::dispatch::SessionDispatchResult::Outcome(SessionEffectOutcome::Deferred {
                until: DeferredUntil::ExchangeState,
            })
        ));
        effect_worker_context
            .effect_worker_state
            .session_effect_queue
            .record_outcome(
                &effect_id,
                SessionEffectOutcome::Deferred {
                    until: DeferredUntil::ExchangeState,
                },
            );

        worker
            .trigger_immediate_reconcile(
                "btc-core",
                &Instrument::new(Venue::Binance, "BTCUSDT"),
                crate::order_outcome::ReconcileReason::PeriodicAudit,
            )
            .await
            .unwrap();
        worker.run_once().await.unwrap();

        let submitted = execution.submitted_requests();
        assert!(
            submitted
                .iter()
                .skip(1)
                .all(|request| request.side != submitted[0].side || request.reduce_only),
            "a fill between the two REST queries must prevent another same-side risk increase: {submitted:?}"
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

    struct SubmitOutcomeUnknownExecution {
        submit_calls: AtomicUsize,
        reconcile_failures_remaining: AtomicUsize,
    }

    #[derive(Default)]
    struct FillBetweenSnapshotQueriesExecution {
        open_order_snapshots: AtomicUsize,
        fill_visible: AtomicBool,
        requests: Mutex<Vec<OrderRequest>>,
    }

    struct FailThenLiveOrderExecution {
        request: Mutex<Option<OrderRequest>>,
        position_failures_remaining: AtomicUsize,
        open_order_snapshots: AtomicUsize,
        cancelled_order_ids: Mutex<Vec<String>>,
    }

    impl Default for FailThenLiveOrderExecution {
        fn default() -> Self {
            Self {
                request: Mutex::new(None),
                position_failures_remaining: AtomicUsize::new(1),
                open_order_snapshots: AtomicUsize::new(0),
                cancelled_order_ids: Mutex::new(Vec::new()),
            }
        }
    }

    #[derive(Default)]
    struct AnomalousFirstSnapshotExecution {
        request: Mutex<Option<OrderRequest>>,
        open_order_snapshots: AtomicUsize,
    }

    struct BlockingSubmitOutcomeUnknownExecution {
        submit_calls: AtomicUsize,
        submit_started: Notify,
        submit_release: Semaphore,
    }

    impl SubmitOutcomeUnknownExecution {
        fn with_clean_snapshot() -> Self {
            Self {
                submit_calls: AtomicUsize::new(0),
                reconcile_failures_remaining: AtomicUsize::new(0),
            }
        }

        fn with_failed_reconcile() -> Self {
            Self {
                submit_calls: AtomicUsize::new(0),
                reconcile_failures_remaining: AtomicUsize::new(1),
            }
        }

        fn submit_call_count(&self) -> usize {
            self.submit_calls.load(Ordering::SeqCst)
        }
    }

    impl BlockingSubmitOutcomeUnknownExecution {
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
    }

    impl Default for BlockingSubmitOutcomeUnknownExecution {
        fn default() -> Self {
            Self {
                submit_calls: AtomicUsize::new(0),
                submit_started: Notify::new(),
                submit_release: Semaphore::new(0),
            }
        }
    }

    async fn claim_planned_submit(
        effect_worker_context: &crate::test_support::EffectWorkerTestContext,
    ) -> SessionTrackEffect {
        claim_planned_submit_at_quote(effect_worker_context, 94.9, 95.1).await
    }

    async fn claim_planned_submit_at_quote(
        effect_worker_context: &crate::test_support::EffectWorkerTestContext,
        best_bid: f64,
        best_ask: f64,
    ) -> SessionTrackEffect {
        effect_worker_context
            .effect_worker_state
            .reconcile
            .runtime
            .handle_market_for_test(
                "btc-core",
                MarketObservation::ExecutionQuote {
                    execution_quote: ExecutionQuote { best_bid, best_ask },
                },
            )
            .await
            .unwrap();
        effect_worker_context
            .effect_worker_state
            .session_effect_queue
            .claim_next()
            .expect("the market observation should plan one submit")
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

    #[async_trait::async_trait]
    impl ExecutionPort for SubmitOutcomeUnknownExecution {
        async fn submit_order(
            &self,
            _request: OrderRequest,
        ) -> poise_engine::ports::ExecutionResult<OrderReceipt> {
            self.submit_calls.fetch_add(1, Ordering::SeqCst);
            Err(ExecutionPortError::new(
                ExecutionPortErrorKind::SubmitOutcomeUnknown,
                anyhow::anyhow!("submit response body timed out"),
            ))
        }

        async fn cancel_order(
            &self,
            _instrument: &Instrument,
            order_id: &str,
        ) -> poise_engine::ports::ExecutionResult<OrderReceipt> {
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
            if self.reconcile_failures_remaining.load(Ordering::SeqCst) > 0 {
                self.reconcile_failures_remaining
                    .fetch_sub(1, Ordering::SeqCst);
                return Err(ExecutionPortError::failed(
                    "position snapshot failed during unknown submit reconciliation",
                ));
            }
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

    impl FillBetweenSnapshotQueriesExecution {
        fn submitted_requests(&self) -> Vec<OrderRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    impl FailThenLiveOrderExecution {
        fn cancelled_order_ids(&self) -> Vec<String> {
            self.cancelled_order_ids.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl ExecutionPort for FillBetweenSnapshotQueriesExecution {
        async fn submit_order(
            &self,
            request: OrderRequest,
        ) -> poise_engine::ports::ExecutionResult<OrderReceipt> {
            self.requests.lock().unwrap().push(request);
            Err(ExecutionPortError::new(
                ExecutionPortErrorKind::SubmitOutcomeUnknown,
                anyhow::anyhow!("submit response body timed out before a delayed fill"),
            ))
        }

        async fn cancel_order(
            &self,
            _instrument: &Instrument,
            order_id: &str,
        ) -> poise_engine::ports::ExecutionResult<OrderReceipt> {
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
            let qty = if self.fill_visible.load(Ordering::SeqCst) {
                self.requests
                    .lock()
                    .unwrap()
                    .first()
                    .expect("submit request should be captured before reconciliation")
                    .quantity
            } else {
                0.0
            };
            Ok(Position {
                instrument: instrument.clone(),
                qty,
                avg_price: if qty > 0.0 { 95.0 } else { 0.0 },
                unrealized_pnl: 0.0,
                mark_price: None,
            })
        }

        async fn get_open_orders(
            &self,
            _instrument: &Instrument,
        ) -> poise_engine::ports::ExecutionResult<ExchangeOpenOrderSnapshot> {
            if self.open_order_snapshots.fetch_add(1, Ordering::SeqCst) == 1 {
                self.fill_visible.store(true, Ordering::SeqCst);
            }
            Ok(ExchangeOpenOrderSnapshot::from_complete_exchange_query(
                Vec::new(),
            ))
        }
    }

    #[async_trait::async_trait]
    impl ExecutionPort for FailThenLiveOrderExecution {
        async fn submit_order(
            &self,
            request: OrderRequest,
        ) -> poise_engine::ports::ExecutionResult<OrderReceipt> {
            *self.request.lock().unwrap() = Some(request);
            Err(ExecutionPortError::new(
                ExecutionPortErrorKind::SubmitOutcomeUnknown,
                anyhow::anyhow!("submit timed out before the live order became queryable"),
            ))
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
                client_order_id: self
                    .request
                    .lock()
                    .unwrap()
                    .as_ref()
                    .expect("submit request should exist")
                    .client_order_id
                    .clone(),
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
            if self.position_failures_remaining.load(Ordering::SeqCst) > 0 {
                self.position_failures_remaining
                    .fetch_sub(1, Ordering::SeqCst);
                return Err(ExecutionPortError::failed(
                    "first position query fails after submit timeout",
                ));
            }
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
            instrument: &Instrument,
        ) -> poise_engine::ports::ExecutionResult<ExchangeOpenOrderSnapshot> {
            if self.open_order_snapshots.fetch_add(1, Ordering::SeqCst) == 0 {
                return Ok(ExchangeOpenOrderSnapshot::from_complete_exchange_query(
                    Vec::new(),
                ));
            }
            let request = self
                .request
                .lock()
                .unwrap()
                .clone()
                .expect("submit request should exist");
            Ok(ExchangeOpenOrderSnapshot::from_complete_exchange_query(
                vec![ExchangeOrder {
                    instrument: instrument.clone(),
                    order_id: "accepted-after-first-sync-failed".to_string(),
                    client_order_id: request.client_order_id,
                    side: request.side,
                    price: request.price,
                    qty: request.quantity,
                    filled_qty: 0.0,
                    status: OrderStatus::New,
                }],
            ))
        }
    }

    #[async_trait::async_trait]
    impl ExecutionPort for AnomalousFirstSnapshotExecution {
        async fn submit_order(
            &self,
            request: OrderRequest,
        ) -> poise_engine::ports::ExecutionResult<OrderReceipt> {
            *self.request.lock().unwrap() = Some(request);
            Err(ExecutionPortError::new(
                ExecutionPortErrorKind::SubmitOutcomeUnknown,
                anyhow::anyhow!("submit timed out before anomalous reconciliation"),
            ))
        }

        async fn cancel_order(
            &self,
            _instrument: &Instrument,
            order_id: &str,
        ) -> poise_engine::ports::ExecutionResult<OrderReceipt> {
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
            instrument: &Instrument,
        ) -> poise_engine::ports::ExecutionResult<ExchangeOpenOrderSnapshot> {
            if self.open_order_snapshots.fetch_add(1, Ordering::SeqCst) > 0 {
                return Ok(ExchangeOpenOrderSnapshot::from_complete_exchange_query(
                    Vec::new(),
                ));
            }
            let request = self
                .request
                .lock()
                .unwrap()
                .clone()
                .expect("submit request should exist");
            Ok(ExchangeOpenOrderSnapshot::from_complete_exchange_query(
                vec![ExchangeOrder {
                    instrument: instrument.clone(),
                    order_id: "unmatched-live-order".to_string(),
                    client_order_id: "unmatched-client-order".to_string(),
                    side: match request.side {
                        poise_core::types::Side::Buy => poise_core::types::Side::Sell,
                        poise_core::types::Side::Sell => poise_core::types::Side::Buy,
                    },
                    price: request.price * 2.0,
                    qty: request.quantity * 2.0,
                    filled_qty: 0.0,
                    status: OrderStatus::New,
                }],
            ))
        }
    }

    #[async_trait::async_trait]
    impl ExecutionPort for BlockingSubmitOutcomeUnknownExecution {
        async fn submit_order(
            &self,
            _request: OrderRequest,
        ) -> poise_engine::ports::ExecutionResult<OrderReceipt> {
            self.submit_calls.fetch_add(1, Ordering::SeqCst);
            self.submit_started.notify_waiters();
            let _permit = self
                .submit_release
                .acquire()
                .await
                .expect("submit release semaphore should stay open");
            Err(ExecutionPortError::new(
                ExecutionPortErrorKind::SubmitOutcomeUnknown,
                anyhow::anyhow!("submit response body timed out after WS acceptance"),
            ))
        }

        async fn cancel_order(
            &self,
            _instrument: &Instrument,
            order_id: &str,
        ) -> poise_engine::ports::ExecutionResult<OrderReceipt> {
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
            _instrument: &Instrument,
        ) -> poise_engine::ports::ExecutionResult<Position> {
            Err(ExecutionPortError::failed(
                "position snapshot failed after WS acceptance",
            ))
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
