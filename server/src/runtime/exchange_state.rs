use poise_application::TrackMutationError;
use poise_engine::observation::{OrderObservation, PositionObservation};
use poise_engine::ports::{
    ExchangeOrder, ExecutionPort, OrderStatus, Position, UserDataEvent, UserDataPayload,
};

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
                if is_terminal_no_fill_unknown_order(&order) {
                    return Ok(());
                }
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

fn is_terminal_no_fill_unknown_order(order: &ExchangeOrder) -> bool {
    !order.status.keeps_working_order()
        && order.status != OrderStatus::Filled
        && order.filled_qty.abs() <= f64::EPSILON
        && order.realized_pnl.abs() <= f64::EPSILON
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use anyhow::{Result, anyhow};
    use poise_application::{TrackEffectStore, TrackMutationStore, TrackQueryStore};
    use poise_engine::ports::{
        ExchangeOpenOrderSnapshot, ExchangeOrder, ExecutionPort, OrderReceipt, OrderRequest,
        OrderStatus, Position, UserDataEvent, UserDataPayload,
    };
    use poise_engine::track::{Instrument, Venue};
    use poise_engine::transition::TrackEffect;
    use poise_storage::sqlite::SqliteStorage;

    use crate::runtime::exchange_state::apply_user_data_event;
    use crate::test_support::{
        build_runtime_and_effect_worker_test_contexts, build_test_application_services,
        unavailable_account_monitor,
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
            repository.clone() as Arc<dyn TrackEffectStore>,
            notifications.clone(),
            account_margin_guard,
        );
        let account_monitor = unavailable_account_monitor(notifications);
        let (runtime_context, _effect_worker_context) =
            build_runtime_and_effect_worker_test_contexts(
                &services,
                repository.clone() as Arc<dyn TrackQueryStore>,
                repository.clone() as Arc<dyn TrackEffectStore>,
                account_monitor,
            );

        let transition = runtime_context
            .observe_market("btc-core", 95.0)
            .await
            .unwrap();
        let request = transition
            .effects
            .iter()
            .find_map(|effect| match effect {
                TrackEffect::SubmitOrder { request, .. } => Some(request.clone()),
                _ => None,
            })
            .expect("seed market should create a tracked submit order");
        let exchange = SyncCountingExchange::new(Position {
            instrument: request.instrument.clone(),
            qty: request.quantity,
            avg_price: request.price,
            unrealized_pnl: 0.0,
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
                    realized_pnl: 0.0,
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
                poise_engine::track::TrackId::new("btc-core"),
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
                },
                3_000.0,
                poise_core::risk::LossLimits {
                    daily_loss_limit: 300.0,
                    total_loss_limit: 600.0,
                },
                poise_core::types::ExchangeRules {
                    price_tick: 0.1,
                    quantity_step: 0.001,
                    min_qty: 0.001,
                    min_notional: 5.0,
                    maker_fee_rate: 0.0,
                    taker_fee_rate: 0.0,
                },
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
        async fn submit_order(&self, _req: OrderRequest) -> Result<OrderReceipt> {
            Err(anyhow!("submit_order is not used in this test"))
        }

        async fn cancel_order(
            &self,
            _instrument: &Instrument,
            _order_id: &str,
        ) -> Result<OrderReceipt> {
            Err(anyhow!("cancel_order is not used in this test"))
        }

        async fn cancel_all(&self, _instrument: &Instrument) -> Result<()> {
            Err(anyhow!("cancel_all is not used in this test"))
        }

        async fn get_position(&self, instrument: &Instrument) -> Result<Position> {
            assert_eq!(instrument, &self.position.instrument);
            self.get_position_calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.position.clone())
        }

        async fn get_open_orders(
            &self,
            instrument: &Instrument,
        ) -> Result<ExchangeOpenOrderSnapshot> {
            assert_eq!(instrument, &self.position.instrument);
            self.get_open_orders_calls.fetch_add(1, Ordering::SeqCst);
            Ok(ExchangeOpenOrderSnapshot::from_complete_exchange_query(
                Vec::new(),
            ))
        }
    }
}
