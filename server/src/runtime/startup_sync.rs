use std::future::Future;

use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use poise_engine::observation::{OrderObservation, PositionObservation};
use poise_engine::ports::{
    ExchangeOrder, ExecutionPort, Position, UserDataEvent, UserDataPayload,
};
use tokio::sync::mpsc;
use tokio::time::sleep;

use crate::exchange_freshness::ExchangeFreshnessReason;
use crate::order_outcome::{ReconcileReason, ReconcileRequest};
use crate::server_context::ReconcileState;

use super::{
    STARTUP_RETRY_ATTEMPTS, STARTUP_RETRY_DELAY, ServerRuntime, TrackMutationError,
    enqueue_reconcile_request, preserve_track_mutation_error,
};

pub(super) async fn startup_sync(runtime: &ServerRuntime) -> Result<()> {
    for track in runtime
        .state
        .reconcile
        .observation_service
        .track_instruments()
        .await
    {
        let position = runtime.execution.get_position(&track.instrument).await?;
        let open_orders = runtime.execution.get_open_orders(&track.instrument).await?;
        runtime
            .state
            .reconcile
            .observation_service
            .sync_exchange_state(
                &track.id,
                position_observation(&position),
                open_orders.iter().map(order_observation).collect(),
            )
            .await?;
    }

    Ok(())
}

pub(super) async fn replay_startup_user_data(
    runtime: &ServerRuntime,
    receiver: &mut mpsc::Receiver<UserDataEvent>,
    startup_cutoff: DateTime<Utc>,
) -> Result<()> {
    let mut buffered_events = Vec::new();
    while let Ok(event) = receiver.try_recv() {
        buffered_events.push(event);
    }

    buffered_events.sort_by_key(|event| event.event_time);
    for event in buffered_events {
        if event.event_time > startup_cutoff {
            let instrument = event.instrument().clone();
            let Some(track_id) = runtime
                .state
                .reconcile
                .observation_service
                .resolve_track_id(&instrument)
                .await
            else {
                tracing::warn!(
                    "received user data for unknown instrument {}:{}",
                    instrument.venue.as_str(),
                    instrument.symbol
                );
                continue;
            };
            apply_user_data_event(
                &runtime.state.reconcile,
                runtime.execution.as_ref(),
                &track_id,
                event,
            )
                .await
                .map_err(super::mutate_error)?;
        }
    }

    Ok(())
}

pub(super) async fn retry_startup_step<T, F, Fut>(
    step_name: &'static str,
    mut operation: F,
) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let mut last_error = None;

    for attempt in 0..STARTUP_RETRY_ATTEMPTS {
        match operation().await {
            Ok(value) => return Ok(value),
            Err(error) => {
                if attempt + 1 == STARTUP_RETRY_ATTEMPTS {
                    return Err(error);
                }
                tracing::warn!(
                    step = step_name,
                    attempt = attempt + 1,
                    max_attempts = STARTUP_RETRY_ATTEMPTS,
                    "startup step failed: {error}"
                );
                last_error = Some(error);
            }
        }

        sleep(STARTUP_RETRY_DELAY).await;
    }

    Err(last_error.unwrap_or_else(|| anyhow!("startup step `{step_name}` failed")))
}

pub(super) async fn apply_user_data_event(
    state: &ReconcileState,
    execution: &dyn ExecutionPort,
    track_id: &str,
    event: UserDataEvent,
) -> std::result::Result<(), TrackMutationError> {
    let instrument = event.instrument().clone();
    match event.payload {
        UserDataPayload::PositionUpdate(position) => {
            let _ = state
                .observation_service
                .observe_position(track_id, position_observation(&position))
                .await
                .map_err(preserve_track_mutation_error)?;
        }
        UserDataPayload::OrderUpdate(order) => {
            let (_, absorb_result): (_, poise_engine::executor::OrderUpdateAbsorbResult) = state
                .observation_service
                .observe_order_with_absorb_result(track_id, order_observation(&order))
                .await
                .map_err(preserve_track_mutation_error)?;
            if absorb_result == poise_engine::executor::OrderUpdateAbsorbResult::Unabsorbed {
                state
                    .exchange_freshness
                    .mark_stale(track_id, ExchangeFreshnessReason::UnabsorbedOrderUpdate)
                    .await;
                enqueue_reconcile_request(
                    state,
                    execution,
                    ReconcileRequest {
                        track_id: track_id.to_string(),
                        reason: ReconcileReason::UnabsorbedOrderUpdate,
                    },
                    &instrument,
                )
                .await?;
            } else if order.status == poise_engine::ports::OrderStatus::Filled {
                state
                    .exchange_freshness
                    .mark_stale(track_id, ExchangeFreshnessReason::FilledAwaitingSync)
                    .await;
            }
        }
        UserDataPayload::TrackLedger(update) => {
            let result = state
                .observation_service
                .apply_track_ledger_event(track_id, update.event)
                .await
                .map_err(preserve_track_mutation_error)?;
            if result.absorb_result
                == Some(poise_engine::executor::OrderUpdateAbsorbResult::Unabsorbed)
            {
                state
                    .exchange_freshness
                    .mark_stale(track_id, ExchangeFreshnessReason::UnabsorbedOrderUpdate)
                    .await;
                enqueue_reconcile_request(
                    state,
                    execution,
                    ReconcileRequest {
                        track_id: track_id.to_string(),
                        reason: ReconcileReason::UnabsorbedOrderUpdate,
                    },
                    &instrument,
                )
                .await?;
            } else if result.order_status == Some(poise_engine::ports::OrderStatus::Filled) {
                state
                    .exchange_freshness
                    .mark_stale(track_id, ExchangeFreshnessReason::FilledAwaitingSync)
                    .await;
            }
        }
    }

    Ok(())
}

pub(super) fn position_observation(position: &Position) -> PositionObservation {
    PositionObservation {
        qty: position.qty,
        unrealized_pnl: position.unrealized_pnl,
    }
}

pub(super) fn order_observation(order: &ExchangeOrder) -> OrderObservation {
    OrderObservation {
        order_id: order.order_id.clone(),
        client_order_id: order.client_order_id.clone(),
        side: order.side,
        price: order.price,
        quantity: order.qty,
        realized_pnl: order.realized_pnl,
        status: order.status,
    }
}
