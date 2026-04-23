use poise_application::TrackMutationError;
use poise_engine::observation::{OrderObservation, PositionObservation};
use poise_engine::ports::{ExchangeOrder, ExecutionPort, Position, UserDataEvent, UserDataPayload};

use crate::exchange_freshness::ExchangeFreshnessReason;
use crate::order_outcome::{ReconcileReason, ReconcileRequest};

use super::{ReconcileStateAccess, enqueue_reconcile_request, preserve_track_mutation_error};

pub(super) async fn apply_user_data_event(
    state: &impl ReconcileStateAccess,
    execution: &dyn ExecutionPort,
    track_id: &str,
    event: UserDataEvent,
) -> std::result::Result<(), TrackMutationError> {
    let state = state.reconcile_state_view();
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
                    &state,
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
                    &state,
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
        filled_qty: order.filled_qty,
        realized_pnl: order.realized_pnl,
        status: order.status,
    }
}
