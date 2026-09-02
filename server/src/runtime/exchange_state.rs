use poise_application::exchange::{
    ExchangeOrder, ExecutionPort, OrderStatus, Position, UserDataEvent, UserDataPayload,
};
use poise_application::{TrackInput, TrackMutationError, TrackUpdateReceipt};
use poise_core::track::TrackId;
use poise_engine::observation::{OrderObservation, PositionObservation};

use crate::exchange_freshness::ExchangeFreshnessReason;
use crate::order_outcome::{ReconcileReason, ReconcileRequest};

use super::diagnostics::describe_runtime_bindings;
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
                .runtime
                .handle(
                    &TrackId::new(track_id),
                    TrackInput::Position(position_observation(&position)),
                )
                .await
                .map_err(preserve_track_mutation_error)?;
        }
        UserDataPayload::OrderUpdate(order) => {
            let receipt = state
                .runtime
                .handle(
                    &TrackId::new(track_id),
                    TrackInput::Order(order_observation(&order)),
                )
                .await
                .map_err(preserve_track_mutation_error)?;
            let TrackUpdateReceipt::OrderObserved { absorb_result } = receipt else {
                unreachable!("order input must return its update receipt");
            };
            if absorb_result == poise_engine::executor::OrderUpdateAbsorbResult::Unabsorbed {
                if is_terminal_no_fill_unknown_order(&order) {
                    return Ok(());
                }
                let runtime = state
                    .runtime
                    .track_runtime_view(track_id)
                    .await
                    .ok()
                    .flatten();
                tracing::warn!(
                    track_id,
                    symbol = %instrument.symbol,
                    order_id = %order.order_id,
                    client_order_id = %order.client_order_id,
                    status = ?order.status,
                    side = ?order.side,
                    price = order.price,
                    qty = order.qty,
                    filled_qty = order.filled_qty,
                    local_bindings = ?describe_runtime_bindings(runtime.as_ref()),
                    "unabsorbed order update encountered before reconcile"
                );
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
            }
        }
        UserDataPayload::TrackPnl(record) => {
            state
                .runtime
                .handle(&TrackId::new(track_id), TrackInput::TrackPnl(record))
                .await
                .map_err(preserve_track_mutation_error)?;
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
        status: order.status,
    }
}

fn is_terminal_no_fill_unknown_order(order: &ExchangeOrder) -> bool {
    !order.status.keeps_working_order()
        && order.status != OrderStatus::Filled
        && order.filled_qty.abs() <= f64::EPSILON
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use poise_application::exchange::{
        ExchangeOpenOrderSnapshot, ExchangeOrder, ExecutionPort, OrderReceipt, OrderRequest,
        OrderStatus, Position, UserDataEvent, UserDataPayload,
    };
    use poise_application::{TrackEffectJournal, TrackMutationStore, TrackQueryStore};
    use poise_core::track::{Instrument, TrackDefinition, Venue};
    use poise_storage::sqlite::SqliteStorage;

    use crate::runtime::exchange_state::apply_user_data_event;
    use crate::test_support::{
        build_runtime_and_effect_worker_test_contexts, build_test_application_services,
        test_instrument_spec, unavailable_account_monitor,
    };

    #[tokio::test]
    async fn absorbed_filled_order_update_does_not_trigger_immediate_exchange_sync() {
        let repository = Arc::new(SqliteStorage::in_memory().unwrap());
        let (notifications, _) = tokio::sync::broadcast::channel(16);
        let account_margin_guard = Arc::new(crate::runtime::AccountMarginGuardStore::default());
        let services = build_test_application_services(
            test_manager(),
            repository.clone() as Arc<dyn TrackMutationStore>,
            repository.clone() as Arc<dyn TrackQueryStore>,
            repository.clone() as Arc<dyn TrackEffectJournal>,
            notifications.clone(),
            account_margin_guard,
        );
        let account_monitor = unavailable_account_monitor(notifications);
        let (runtime_context, _effect_worker_context) =
            build_runtime_and_effect_worker_test_contexts(
                &services,
                repository.clone() as Arc<dyn TrackQueryStore>,
                repository.clone() as Arc<dyn TrackEffectJournal>,
                account_monitor,
            );

        let request = runtime_context
            .seed_market_order_request("btc-core", 95.0)
            .await
            .unwrap();
        let exchange = SyncCountingExchange::new(Position {
            instrument: request.instrument.clone(),
            qty: request.quantity,
            avg_price: request.price,
            unrealized_pnl: 0.0,
            mark_price: None,
        });

        apply_user_data_event(
            &runtime_context,
            &exchange,
            "btc-core",
            UserDataEvent {
                event_time: chrono::Utc::now(),
                payload: UserDataPayload::OrderUpdate(ExchangeOrder {
                    instrument: request.instrument.clone(),
                    order_id: "filled-order".to_string(),
                    client_order_id: request.client_order_id.clone(),
                    side: request.side,
                    price: request.price,
                    qty: request.quantity,
                    filled_qty: request.quantity,
                    status: OrderStatus::Filled,
                }),
            },
        )
        .await
        .unwrap();

        assert_eq!(exchange.get_position_calls.load(Ordering::SeqCst), 0);
        assert_eq!(exchange.get_open_orders_calls.load(Ordering::SeqCst), 0);
    }

    fn test_manager() -> poise_engine::manager::TrackManager {
        let mut manager =
            poise_engine::manager::TrackManager::new(Arc::new(crate::assembly::SystemClock));
        manager
            .add_track(
                TrackDefinition::try_new(
                    poise_core::track::TrackId::new("btc-core"),
                    Instrument::new(Venue::Binance, "BTCUSDT"),
                    poise_core::strategy::TrackConfig {
                        lower_price: 90.0,
                        upper_price: 110.0,
                        long_exposure_units: 8.0,
                        short_exposure_units: 8.0,
                        notional_per_unit: 375.0,
                        min_rebalance_units: 0.5,
                        shape_family: poise_core::strategy::ShapeFamily::Linear,
                        out_of_band_policy: poise_core::strategy::BandProtectionPolicy::Freeze,
                        risk_acquisition: Default::default(),
                    },
                    Some(3_000.0),
                    poise_core::risk::LossLimits {
                        daily_loss_limit: 300.0,
                        total_loss_limit: 600.0,
                    },
                    None,
                )
                .unwrap(),
                test_instrument_spec(),
            )
            .unwrap();
        manager
    }

    struct SyncCountingExchange {
        position: Position,
        get_position_calls: AtomicUsize,
        get_open_orders_calls: AtomicUsize,
    }

    impl SyncCountingExchange {
        fn new(position: Position) -> Self {
            Self {
                position,
                get_position_calls: AtomicUsize::new(0),
                get_open_orders_calls: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl ExecutionPort for SyncCountingExchange {
        async fn submit_order(
            &self,
            _req: OrderRequest,
        ) -> poise_application::exchange::ExecutionResult<OrderReceipt> {
            Err(poise_application::exchange::ExecutionPortError::failed(
                "submit_order is not used in this test",
            ))
        }

        async fn cancel_order(
            &self,
            _instrument: &Instrument,
            _order_id: &str,
        ) -> poise_application::exchange::ExecutionResult<OrderReceipt> {
            Err(poise_application::exchange::ExecutionPortError::failed(
                "cancel_order is not used in this test",
            ))
        }

        async fn cancel_all(
            &self,
            _instrument: &Instrument,
        ) -> poise_application::exchange::ExecutionResult<()> {
            Err(poise_application::exchange::ExecutionPortError::failed(
                "cancel_all is not used in this test",
            ))
        }

        async fn get_position(
            &self,
            instrument: &Instrument,
        ) -> poise_application::exchange::ExecutionResult<Position> {
            assert_eq!(instrument, &self.position.instrument);
            self.get_position_calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.position.clone())
        }

        async fn get_open_orders(
            &self,
            instrument: &Instrument,
        ) -> poise_application::exchange::ExecutionResult<ExchangeOpenOrderSnapshot> {
            assert_eq!(instrument, &self.position.instrument);
            self.get_open_orders_calls.fetch_add(1, Ordering::SeqCst);
            Ok(ExchangeOpenOrderSnapshot::from_complete_exchange_query(
                Vec::new(),
            ))
        }
    }
}
