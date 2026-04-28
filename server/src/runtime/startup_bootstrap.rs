use std::collections::HashMap;
use std::future::Future;

use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use poise_core::track::Instrument;
use poise_engine::ports::{AccountCapacitySnapshot, UserDataEvent};
use poise_engine::runtime::FreshSessionExternalInputs;
use tokio::sync::mpsc;
use tokio::time::sleep;

use super::{
    RuntimeStartupCapacityMode, RuntimeStartupDefinition, STARTUP_RETRY_ATTEMPTS,
    STARTUP_RETRY_DELAY, ServerRuntime, exchange_state,
};

struct TrackStartupSeed {
    definition: RuntimeStartupDefinition,
    cleanup_filter: CleanupReplayFilter,
}

impl TrackStartupSeed {
    fn track_id(&self) -> &str {
        self.definition.track_id().as_str()
    }

    fn instrument(&self) -> &Instrument {
        self.definition.instrument()
    }

    fn required_additional_notional(&self, position_qty: f64) -> f64 {
        self.definition.required_additional_notional(position_qty)
    }

    fn exposure_from_position_qty(&self, position_qty: f64) -> poise_core::types::Exposure {
        self.definition.exposure_from_position_qty(position_qty)
    }

    fn startup_capacity_mode(&self) -> &RuntimeStartupCapacityMode {
        self.definition.startup_capacity_mode()
    }
}

#[derive(Debug, Clone, Default)]
struct CleanupReplayFilter {
    orders: Vec<CleanupOrderIdentity>,
}

#[derive(Debug, Clone)]
struct CleanupOrderIdentity {
    order_id: String,
    client_order_id: String,
}

impl CleanupOrderIdentity {
    fn matches(&self, order: &poise_engine::ports::ExchangeOrder) -> bool {
        self.order_id == order.order_id || self.client_order_id == order.client_order_id
    }
}

impl CleanupReplayFilter {
    fn from_orders(orders: &[poise_engine::ports::ExchangeOrder]) -> Self {
        Self {
            orders: orders
                .iter()
                .map(|order| CleanupOrderIdentity {
                    order_id: order.order_id.clone(),
                    client_order_id: order.client_order_id.clone(),
                })
                .collect(),
        }
    }

    fn matches_order(&self, order: &poise_engine::ports::ExchangeOrder) -> bool {
        self.orders.iter().any(|expected| expected.matches(order))
    }
}

#[derive(Debug, Default)]
struct CleanupTracker {
    filters: HashMap<String, CleanupReplayFilter>,
}

impl CleanupTracker {
    fn from_track_seeds(track_seeds: &[TrackStartupSeed]) -> Self {
        Self {
            filters: track_seeds
                .iter()
                .map(|seed| (seed.track_id().to_string(), seed.cleanup_filter.clone()))
                .collect(),
        }
    }

    fn should_ignore(&self, track_id: &str, event: &UserDataEvent) -> bool {
        let Some(filter) = self.filters.get(track_id) else {
            return false;
        };
        match &event.payload {
            poise_engine::ports::UserDataPayload::OrderUpdate(order) => filter.matches_order(order),
            _ => false,
        }
    }
}

struct StartupReplayEvent {
    track_id: String,
    event: UserDataEvent,
}

pub(super) async fn complete_startup(
    runtime: &ServerRuntime,
    receiver: &mut mpsc::Receiver<UserDataEvent>,
    // Only classifies user-data events already buffered during startup replay.
    // Steady-state does not receive or apply this time boundary.
    startup_replay_floor: DateTime<Utc>,
) -> Result<()> {
    let mut track_seeds = Vec::new();

    for track in &runtime.startup_definitions {
        let seed = prepare_track_startup_seed(runtime, track.clone()).await?;
        track_seeds.push(seed);
    }

    for seed in &track_seeds {
        runtime
            .state
            .reconcile
            .runtime_lifecycle_service
            .prepare_fresh_session_for_activation(seed.track_id())
            .await?;
    }

    rebuild_fresh_sessions(runtime, &track_seeds).await?;

    let cleanup_tracker = CleanupTracker::from_track_seeds(&track_seeds);
    let replay_events =
        collect_startup_replay_events(runtime, receiver, startup_replay_floor, &cleanup_tracker)
            .await?;
    apply_startup_replay_events(runtime, replay_events).await?;

    Ok(())
}

async fn prepare_track_startup_seed(
    runtime: &ServerRuntime,
    track: RuntimeStartupDefinition,
) -> Result<TrackStartupSeed> {
    let instrument = track.instrument().clone();
    let cleanup_filter = clear_inherited_open_orders(runtime, &instrument).await?;
    Ok(TrackStartupSeed {
        definition: track,
        cleanup_filter,
    })
}

async fn rebuild_fresh_sessions(
    runtime: &ServerRuntime,
    track_seeds: &[TrackStartupSeed],
) -> Result<()> {
    let mut account_capacity_snapshots: HashMap<Instrument, AccountCapacitySnapshot> =
        HashMap::new();
    let current_utc_day = runtime.clock.now().date_naive();

    for seed in track_seeds {
        let instrument = seed.instrument().clone();
        let position = retry_startup_step("get_position", || {
            runtime.execution.get_position(&instrument)
        })
        .await?;
        let account_capacity_snapshot = probe_startup_account_capacity(runtime, seed).await?;
        let required_additional_notional = seed.required_additional_notional(position.qty);
        if required_additional_notional > account_capacity_snapshot.max_increase_notional {
            return Err(anyhow!(
                "insufficient account margin for configured max_notional on track `{}`: required {}, available {}",
                seed.track_id(),
                required_additional_notional,
                account_capacity_snapshot.max_increase_notional
            ));
        }
        let exchange_info = retry_startup_step("get_exchange_info", || {
            runtime.metadata.get_exchange_info(&instrument)
        })
        .await?;
        let applied = runtime
            .state
            .reconcile
            .runtime_lifecycle_service
            .fresh_start_track_runtime(
                &poise_core::track::TrackId::new(seed.track_id()),
                current_utc_day,
                FreshSessionExternalInputs {
                    current_exposure: seed.exposure_from_position_qty(position.qty),
                    market_data: None,
                    exchange_rules: exchange_info.rules,
                },
            )
            .await?;
        if !applied {
            return Err(anyhow!(
                "track `{}` missing during fresh-session startup rebuild",
                seed.track_id()
            ));
        }
        runtime
            .state
            .reconcile
            .observation_service
            .observe_position(
                seed.track_id(),
                exchange_state::position_observation(&position),
            )
            .await?;
        account_capacity_snapshots.insert(instrument, account_capacity_snapshot);
    }

    runtime
        .state
        .account_margin_guard
        .replace_snapshots(account_capacity_snapshots);

    Ok(())
}

async fn clear_inherited_open_orders(
    runtime: &ServerRuntime,
    instrument: &Instrument,
) -> Result<CleanupReplayFilter> {
    let open_orders = retry_startup_step("get_open_orders", || {
        runtime.execution.get_open_orders(instrument)
    })
    .await?;
    if open_orders.is_empty() {
        return Ok(CleanupReplayFilter::default());
    }
    let cleanup_filter = CleanupReplayFilter::from_orders(open_orders.orders());

    retry_startup_step("cancel_all", || runtime.execution.cancel_all(instrument)).await?;
    retry_startup_step("await_open_orders_cleared", || async {
        let remaining = runtime.execution.get_open_orders(instrument).await?;
        if remaining.is_empty() {
            Ok(())
        } else {
            Err(anyhow!(
                "inherited open orders still present for {}:{} after cancel_all",
                instrument.venue.as_str(),
                instrument.symbol
            ))
        }
    })
    .await?;

    Ok(cleanup_filter)
}

async fn probe_startup_account_capacity(
    runtime: &ServerRuntime,
    track: &TrackStartupSeed,
) -> Result<AccountCapacitySnapshot> {
    match track.startup_capacity_mode() {
        RuntimeStartupCapacityMode::AvailableBalanceTimesLeverage { leverage } => {
            let summary = retry_startup_step("get_account_summary", || {
                runtime.account_summary.get_account_summary()
            })
            .await?;
            Ok(AccountCapacitySnapshot {
                max_increase_notional: summary.available * *leverage as f64,
            })
        }
        RuntimeStartupCapacityMode::AccountCapacitySnapshot => {
            let instrument = track.instrument().clone();
            retry_startup_step("get_account_capacity_snapshot", || {
                runtime.account.get_account_capacity_snapshot(&instrument)
            })
            .await
        }
    }
}

async fn collect_startup_replay_events(
    runtime: &ServerRuntime,
    receiver: &mut mpsc::Receiver<UserDataEvent>,
    // This is not a handoff boundary. It only prevents replaying buffered events
    // that predate the fresh session inputs collected during startup.
    startup_replay_floor: DateTime<Utc>,
    cleanup_tracker: &CleanupTracker,
) -> Result<Vec<StartupReplayEvent>> {
    let mut buffered_events = Vec::new();
    while let Ok(event) = receiver.try_recv() {
        buffered_events.push(event);
    }

    buffered_events.sort_by_key(|event| event.event_time);
    let mut replay_events = Vec::new();
    for event in buffered_events {
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
        if cleanup_tracker.should_ignore(&track_id, &event) {
            continue;
        }
        if event.event_time > startup_replay_floor {
            replay_events.push(StartupReplayEvent { track_id, event });
        }
    }

    Ok(replay_events)
}

async fn apply_startup_replay_events(
    runtime: &ServerRuntime,
    replay_events: Vec<StartupReplayEvent>,
) -> Result<()> {
    for replay_event in replay_events {
        exchange_state::apply_user_data_event(
            &runtime.state.reconcile,
            runtime.execution.as_ref(),
            &replay_event.track_id,
            replay_event.event,
        )
        .await
        .map_err(super::mutate_error)?;
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use anyhow::{Result, anyhow};
    use chrono::{TimeZone, Utc};
    use poise_application::{
        EffectStatus, TrackEffectJournal, TrackMutationStore, TrackQueryStore,
    };
    use poise_core::risk::LossLimits;
    use poise_core::strategy::{BandProtectionPolicy, ShapeFamily, TrackConfig};
    use poise_core::track::{Instrument, TrackId, Venue};
    use poise_core::types::{ExchangeRules, Exposure, Side};
    use poise_engine::manager::TrackManager;
    use poise_engine::ports::{
        AccountPort, AccountSummaryPort, ExchangeInfo, ExchangeOrder, ExecutionPort,
        MarketDataTick, MetadataPort, OrderReceipt, OrderStatus, Position, UserDataEvent,
        UserDataPayload,
    };
    use poise_engine::runtime::{TerminationCause, TrackStatus};
    use poise_storage::sqlite::SqliteStorage;
    use tokio::sync::mpsc;

    use crate::assembly::SystemClock;
    use crate::runtime::{RuntimePorts, RuntimeStartupCapacityMode, RuntimeStartupDefinition};
    use crate::test_support::{
        build_runtime_and_effect_worker_test_contexts, build_test_application_services,
        seed_persisted_pending_submit_effect, test_prepared_registry, unavailable_account_monitor,
    };

    use super::complete_startup;

    #[tokio::test]
    async fn complete_startup_cancels_inherited_orders_and_rebuilds_fresh_executor_state() {
        let repository = Arc::new(SqliteStorage::in_memory().unwrap());
        let manager = seeded_manager();
        seed_persisted_pending_submit_effect(repository.as_ref(), "btc-core")
            .await
            .unwrap();
        let (notifications, _) = tokio::sync::broadcast::channel(16);
        let account_margin_guard = Arc::new(crate::runtime::AccountMarginGuardStore::default());
        let services = build_test_application_services(
            manager,
            repository.clone() as Arc<dyn TrackMutationStore>,
            repository.clone() as Arc<dyn TrackQueryStore>,
            repository.clone() as Arc<dyn TrackEffectJournal>,
            notifications.clone(),
            account_margin_guard,
        );
        let account_monitor = unavailable_account_monitor(notifications.clone());
        let (runtime_context, effect_worker_context) =
            build_runtime_and_effect_worker_test_contexts(
                &services,
                repository.clone() as Arc<dyn TrackQueryStore>,
                repository.clone() as Arc<dyn TrackEffectJournal>,
                account_monitor,
            );
        let exchange = Arc::new(StartupExchange::with_inherited_order("BTCUSDT"));
        let runtime = super::ServerRuntime::new(
            runtime_context.runtime_state(),
            effect_worker_context.effect_worker_state,
            RuntimePorts::new(
                exchange.clone(),
                exchange.clone(),
                exchange.clone(),
                exchange.clone(),
                exchange.clone(),
                Arc::new(SystemClock),
            ),
            vec![RuntimeStartupDefinition::new(
                test_prepared_registry("btc-core")
                    .get(&TrackId::new("btc-core"))
                    .unwrap()
                    .startup_definition(),
                RuntimeStartupCapacityMode::AccountCapacitySnapshot,
            )],
        );
        let (sender, mut receiver) = mpsc::channel(8);
        let startup_replay_floor = Utc::now();
        sender
            .send(cleanup_canceled_event("BTCUSDT", startup_replay_floor))
            .await
            .unwrap();

        complete_startup(&runtime, &mut receiver, startup_replay_floor)
            .await
            .unwrap();

        assert_eq!(exchange.cancel_all_calls.load(Ordering::SeqCst), 1);
        assert!(
            !runtime_context
                .submit_preflight
                .has_tracked_submit_effects()
                .await
        );
        let effects = repository
            .list_recent_track_effects(&TrackId::new("btc-core"), 8)
            .await
            .unwrap();
        assert!(!effects.is_empty());
        assert!(
            effects
                .iter()
                .all(|effect| effect.status == EffectStatus::Pending),
            "startup should leave previous-session persisted effects as diagnostic journal rows"
        );

        let snapshot = services
            .observation_service
            .manager()
            .read()
            .await
            .mutation_frame("btc-core")
            .unwrap();
        assert!(!snapshot.has_executor_bindings());
        assert!(!snapshot.recovery_anomaly_active());
        assert_eq!(snapshot.status(), TrackStatus::WaitingMarketData);
    }

    #[tokio::test]
    async fn complete_startup_rebuilds_from_persisted_control_and_ledger_not_dirty_manager_runtime()
    {
        let repository = Arc::new(SqliteStorage::in_memory().unwrap());
        let manager = seeded_manager();
        repository
            .save_track_control_state(
                &TrackId::new("btc-core"),
                &poise_application::TrackControlState::Paused {
                    resume_mode: poise_application::PersistedControlMode::Automatic,
                },
            )
            .await
            .unwrap();
        repository
            .save_track_ledger_state(
                &TrackId::new("btc-core"),
                &poise_engine::ledger::TrackLedgerState {
                    gross_realized_pnl_cumulative: 42.0,
                    ..poise_engine::ledger::TrackLedgerState::default()
                },
            )
            .await
            .unwrap();

        let (notifications, _) = tokio::sync::broadcast::channel(16);
        let account_margin_guard = Arc::new(crate::runtime::AccountMarginGuardStore::default());
        let services = build_test_application_services(
            manager,
            repository.clone() as Arc<dyn TrackMutationStore>,
            repository.clone() as Arc<dyn TrackQueryStore>,
            repository.clone() as Arc<dyn TrackEffectJournal>,
            notifications.clone(),
            account_margin_guard,
        );
        {
            let manager_handle = services.observation_service.manager();
            let mut manager = manager_handle.write().await;
            let mut snapshot = manager.mutation_frame("btc-core").unwrap();
            snapshot.set_runtime_state(poise_engine::runtime::TrackState::Terminated {
                cause: TerminationCause::ManualCommand,
            });
            snapshot.set_exposure_state(Exposure(9.0), Some(Exposure(4.0)));
            snapshot.set_recovery_anomaly(Some(
                poise_engine::executor::RecoveryAnomaly::UnknownLiveOrder,
            ));
            manager.rollback_track_state(&snapshot).unwrap();
        }

        let account_monitor = unavailable_account_monitor(notifications.clone());
        let (runtime_context, effect_worker_context) =
            build_runtime_and_effect_worker_test_contexts(
                &services,
                repository.clone() as Arc<dyn TrackQueryStore>,
                repository.clone() as Arc<dyn TrackEffectJournal>,
                account_monitor,
            );
        let exchange = Arc::new(StartupExchange::with_inherited_order("BTCUSDT"));
        let runtime = super::ServerRuntime::new(
            runtime_context.runtime_state(),
            effect_worker_context.effect_worker_state,
            RuntimePorts::new(
                exchange.clone(),
                exchange.clone(),
                exchange.clone(),
                exchange.clone(),
                exchange.clone(),
                Arc::new(SystemClock),
            ),
            vec![RuntimeStartupDefinition::new(
                test_prepared_registry("btc-core")
                    .get(&TrackId::new("btc-core"))
                    .unwrap()
                    .startup_definition(),
                RuntimeStartupCapacityMode::AccountCapacitySnapshot,
            )],
        );
        let (sender, mut receiver) = mpsc::channel(8);
        let startup_replay_floor = Utc::now();
        sender
            .send(cleanup_canceled_event("BTCUSDT", startup_replay_floor))
            .await
            .unwrap();

        complete_startup(&runtime, &mut receiver, startup_replay_floor)
            .await
            .unwrap();

        let snapshot = services
            .observation_service
            .manager()
            .read()
            .await
            .mutation_frame("btc-core")
            .unwrap();
        assert_eq!(snapshot.status(), TrackStatus::Paused);
        assert_eq!(snapshot.current_exposure(), &Exposure(0.0));
        assert_eq!(snapshot.desired_exposure(), None);
        assert!(!snapshot.has_executor_bindings());
        assert!(!snapshot.recovery_anomaly_active());
        assert_eq!(snapshot.ledger_state().gross_realized_pnl_cumulative, 42.0);
    }

    #[tokio::test]
    async fn complete_startup_ignores_cleanup_order_updates_but_replays_new_session_events() {
        let repository = Arc::new(SqliteStorage::in_memory().unwrap());
        let manager = seeded_manager();
        repository
            .commit_track_transition(
                "btc-core",
                None,
                &poise_engine::ledger::TrackLedgerState::default(),
                &[],
            )
            .await
            .unwrap();

        let (notifications, _) = tokio::sync::broadcast::channel(16);
        let account_margin_guard = Arc::new(crate::runtime::AccountMarginGuardStore::default());
        let services = build_test_application_services(
            manager,
            repository.clone() as Arc<dyn TrackMutationStore>,
            repository.clone() as Arc<dyn TrackQueryStore>,
            repository.clone() as Arc<dyn TrackEffectJournal>,
            notifications.clone(),
            account_margin_guard,
        );
        let account_monitor = unavailable_account_monitor(notifications.clone());
        let (runtime_context, effect_worker_context) =
            build_runtime_and_effect_worker_test_contexts(
                &services,
                repository.clone() as Arc<dyn TrackQueryStore>,
                repository.clone() as Arc<dyn TrackEffectJournal>,
                account_monitor,
            );
        let exchange = Arc::new(StartupExchange::with_inherited_order("BTCUSDT"));
        let runtime = super::ServerRuntime::new(
            runtime_context.runtime_state(),
            effect_worker_context.effect_worker_state,
            RuntimePorts::new(
                exchange.clone(),
                exchange.clone(),
                exchange.clone(),
                exchange.clone(),
                exchange.clone(),
                Arc::new(SystemClock),
            ),
            vec![RuntimeStartupDefinition::new(
                test_prepared_registry("btc-core")
                    .get(&TrackId::new("btc-core"))
                    .unwrap()
                    .startup_definition(),
                RuntimeStartupCapacityMode::AccountCapacitySnapshot,
            )],
        );
        let (sender, mut receiver) = mpsc::channel(8);
        let startup_replay_floor = Utc.with_ymd_and_hms(2026, 4, 23, 12, 0, 0).unwrap();

        sender
            .send(cleanup_canceled_event("BTCUSDT", startup_replay_floor))
            .await
            .unwrap();
        sender
            .send(UserDataEvent {
                event_time: startup_replay_floor + chrono::TimeDelta::seconds(2),
                payload: UserDataPayload::PositionUpdate(Position {
                    instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                    qty: 1.25,
                    avg_price: 100.0,
                    unrealized_pnl: 12.0,
                }),
            })
            .await
            .unwrap();
        drop(sender);

        complete_startup(&runtime, &mut receiver, startup_replay_floor)
            .await
            .unwrap();

        assert!(
            !runtime_context
                .exchange_freshness
                .is_stale("btc-core")
                .await
        );
        assert_eq!(exchange.get_open_orders_calls.load(Ordering::SeqCst), 2);
        assert_eq!(exchange.get_position_calls.load(Ordering::SeqCst), 1);
        let snapshot = services
            .observation_service
            .manager()
            .read()
            .await
            .mutation_frame("btc-core")
            .unwrap();
        assert_eq!(snapshot.current_exposure(), &Exposure(0.3333333333333333));
        assert_eq!(snapshot.unrealized_pnl(), 12.0);
    }

    #[tokio::test]
    async fn complete_startup_uses_rest_open_orders_barrier_without_waiting_for_cleanup_update() {
        let repository = Arc::new(SqliteStorage::in_memory().unwrap());
        let manager = seeded_manager();
        repository
            .commit_track_transition(
                "btc-core",
                None,
                &poise_engine::ledger::TrackLedgerState::default(),
                &[],
            )
            .await
            .unwrap();

        let (notifications, _) = tokio::sync::broadcast::channel(16);
        let account_margin_guard = Arc::new(crate::runtime::AccountMarginGuardStore::default());
        let services = build_test_application_services(
            manager,
            repository.clone() as Arc<dyn TrackMutationStore>,
            repository.clone() as Arc<dyn TrackQueryStore>,
            repository.clone() as Arc<dyn TrackEffectJournal>,
            notifications.clone(),
            account_margin_guard,
        );
        let account_monitor = unavailable_account_monitor(notifications.clone());
        let (runtime_context, effect_worker_context) =
            build_runtime_and_effect_worker_test_contexts(
                &services,
                repository.clone() as Arc<dyn TrackQueryStore>,
                repository.clone() as Arc<dyn TrackEffectJournal>,
                account_monitor,
            );
        let exchange = Arc::new(StartupExchange::with_inherited_order("BTCUSDT"));
        let runtime = super::ServerRuntime::new(
            runtime_context.runtime_state(),
            effect_worker_context.effect_worker_state,
            RuntimePorts::new(
                exchange.clone(),
                exchange.clone(),
                exchange.clone(),
                exchange.clone(),
                exchange.clone(),
                Arc::new(SystemClock),
            ),
            vec![RuntimeStartupDefinition::new(
                test_prepared_registry("btc-core")
                    .get(&TrackId::new("btc-core"))
                    .unwrap()
                    .startup_definition(),
                RuntimeStartupCapacityMode::AccountCapacitySnapshot,
            )],
        );
        let (_sender, mut receiver) = mpsc::channel(8);
        let startup_replay_floor = Utc.with_ymd_and_hms(2026, 4, 23, 12, 0, 0).unwrap();

        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            complete_startup(&runtime, &mut receiver, startup_replay_floor),
        )
        .await
        .expect("startup should use REST open-orders cleanup barrier instead of waiting for user-data terminal update")
        .unwrap();
        assert!(
            !runtime_context
                .exchange_freshness
                .is_stale("btc-core")
                .await
        );
    }

    #[tokio::test]
    async fn user_task_processes_late_events_after_startup_handoff_even_when_event_time_is_old() {
        let repository = Arc::new(SqliteStorage::in_memory().unwrap());
        let manager = seeded_manager();
        repository
            .commit_track_transition(
                "btc-core",
                None,
                &poise_engine::ledger::TrackLedgerState::default(),
                &[],
            )
            .await
            .unwrap();

        let (notifications, _) = tokio::sync::broadcast::channel(16);
        let account_margin_guard = Arc::new(crate::runtime::AccountMarginGuardStore::default());
        let services = build_test_application_services(
            manager,
            repository.clone() as Arc<dyn TrackMutationStore>,
            repository.clone() as Arc<dyn TrackQueryStore>,
            repository.clone() as Arc<dyn TrackEffectJournal>,
            notifications.clone(),
            account_margin_guard,
        );
        let account_monitor = unavailable_account_monitor(notifications.clone());
        let (runtime_context, effect_worker_context) =
            build_runtime_and_effect_worker_test_contexts(
                &services,
                repository.clone() as Arc<dyn TrackQueryStore>,
                repository.clone() as Arc<dyn TrackEffectJournal>,
                account_monitor,
            );
        let exchange = Arc::new(StartupExchange::with_inherited_order("BTCUSDT"));
        let runtime = super::ServerRuntime::new(
            runtime_context.runtime_state(),
            effect_worker_context.effect_worker_state,
            RuntimePorts::new(
                exchange.clone(),
                exchange.clone(),
                exchange.clone(),
                exchange.clone(),
                exchange.clone(),
                Arc::new(SystemClock),
            ),
            vec![RuntimeStartupDefinition::new(
                test_prepared_registry("btc-core")
                    .get(&TrackId::new("btc-core"))
                    .unwrap()
                    .startup_definition(),
                RuntimeStartupCapacityMode::AccountCapacitySnapshot,
            )],
        );
        let (sender, mut receiver) = mpsc::channel(8);
        let startup_replay_floor = Utc.with_ymd_and_hms(2026, 4, 23, 12, 0, 0).unwrap();

        complete_startup(&runtime, &mut receiver, startup_replay_floor)
            .await
            .unwrap();
        let user_task = runtime.spawn_user_task(receiver, runtime.shutdown_tx.subscribe());

        sender
            .send(cleanup_canceled_event("BTCUSDT", startup_replay_floor))
            .await
            .unwrap();
        sender
            .send(UserDataEvent {
                event_time: startup_replay_floor - chrono::TimeDelta::milliseconds(1),
                payload: UserDataPayload::PositionUpdate(Position {
                    instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                    qty: 1.25,
                    avg_price: 100.0,
                    unrealized_pnl: 12.0,
                }),
            })
            .await
            .unwrap();

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                let snapshot = services
                    .observation_service
                    .manager()
                    .read()
                    .await
                    .mutation_frame("btc-core")
                    .unwrap();
                if snapshot.current_exposure() == &Exposure(0.3333333333333333)
                    && snapshot.unrealized_pnl() == 12.0
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        assert_eq!(exchange.get_open_orders_calls.load(Ordering::SeqCst), 2);
        assert_eq!(exchange.get_position_calls.load(Ordering::SeqCst), 1);
        assert!(
            !runtime_context
                .exchange_freshness
                .is_stale("btc-core")
                .await
        );

        runtime.shutdown_tx.send(true).unwrap();
        user_task.await.unwrap();
    }

    fn seeded_manager() -> TrackManager {
        let mut manager = TrackManager::new(Arc::new(SystemClock));
        manager
            .add_track(
                TrackId::new("btc-core"),
                Instrument::new(Venue::Binance, "BTCUSDT"),
                TrackConfig {
                    lower_price: 90.0,
                    upper_price: 110.0,
                    long_exposure_units: 8.0,
                    short_exposure_units: 8.0,
                    notional_per_unit: 375.0,
                    min_rebalance_units: 0.5,
                    shape_family: ShapeFamily::Linear,
                    out_of_band_policy: BandProtectionPolicy::Freeze,
                },
                3_000.0,
                LossLimits {
                    daily_loss_limit: 300.0,
                    total_loss_limit: 600.0,
                },
                ExchangeRules {
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

    fn cleanup_canceled_event(
        symbol: &str,
        startup_replay_floor: chrono::DateTime<Utc>,
    ) -> UserDataEvent {
        UserDataEvent {
            event_time: startup_replay_floor + chrono::TimeDelta::seconds(1),
            payload: UserDataPayload::OrderUpdate(ExchangeOrder {
                instrument: Instrument::new(Venue::Binance, symbol),
                order_id: "legacy-order".into(),
                client_order_id: "legacy-client-order".into(),
                side: Side::Buy,
                price: 99.0,
                qty: 0.1,
                filled_qty: 0.0,
                realized_pnl: 0.0,
                status: OrderStatus::Canceled,
            }),
        }
    }

    struct StartupExchange {
        inherited_order_present: AtomicBool,
        cancel_all_calls: AtomicUsize,
        get_position_calls: AtomicUsize,
        get_open_orders_calls: AtomicUsize,
        instrument: Instrument,
    }

    impl StartupExchange {
        fn with_inherited_order(symbol: &str) -> Self {
            Self {
                inherited_order_present: AtomicBool::new(true),
                cancel_all_calls: AtomicUsize::new(0),
                get_position_calls: AtomicUsize::new(0),
                get_open_orders_calls: AtomicUsize::new(0),
                instrument: Instrument::new(Venue::Binance, symbol),
            }
        }
    }

    #[async_trait::async_trait]
    impl ExecutionPort for StartupExchange {
        async fn submit_order(
            &self,
            _req: poise_engine::ports::OrderRequest,
        ) -> Result<poise_engine::ports::OrderReceipt> {
            Err(anyhow!(
                "submit_order is not used during startup bootstrap tests"
            ))
        }

        async fn cancel_order(
            &self,
            _instrument: &Instrument,
            _order_id: &str,
        ) -> Result<OrderReceipt> {
            Err(anyhow!(
                "cancel_order is not used during startup bootstrap tests"
            ))
        }

        async fn cancel_all(&self, instrument: &Instrument) -> Result<()> {
            assert_eq!(instrument, &self.instrument);
            self.cancel_all_calls.fetch_add(1, Ordering::SeqCst);
            self.inherited_order_present.store(false, Ordering::SeqCst);
            Ok(())
        }

        async fn get_position(&self, instrument: &Instrument) -> Result<Position> {
            assert_eq!(instrument, &self.instrument);
            self.get_position_calls.fetch_add(1, Ordering::SeqCst);
            Ok(Position {
                instrument: instrument.clone(),
                qty: 0.0,
                avg_price: 100.0,
                unrealized_pnl: 0.0,
            })
        }

        async fn get_open_orders(
            &self,
            instrument: &Instrument,
        ) -> Result<poise_engine::ports::ExchangeOpenOrderSnapshot> {
            assert_eq!(instrument, &self.instrument);
            self.get_open_orders_calls.fetch_add(1, Ordering::SeqCst);
            if !self.inherited_order_present.load(Ordering::SeqCst) {
                return Ok(
                    poise_engine::ports::ExchangeOpenOrderSnapshot::from_complete_exchange_query(
                        Vec::new(),
                    ),
                );
            }

            Ok(
                poise_engine::ports::ExchangeOpenOrderSnapshot::from_complete_exchange_query(vec![
                    ExchangeOrder {
                        instrument: instrument.clone(),
                        order_id: "legacy-order".into(),
                        client_order_id: "legacy-client-order".into(),
                        side: Side::Buy,
                        price: 99.0,
                        qty: 0.1,
                        filled_qty: 0.0,
                        realized_pnl: 0.0,
                        status: OrderStatus::New,
                    },
                ]),
            )
        }
    }

    #[async_trait::async_trait]
    impl AccountSummaryPort for StartupExchange {
        async fn get_account_summary(&self) -> Result<poise_engine::ports::AccountSummarySnapshot> {
            Ok(poise_engine::ports::AccountSummarySnapshot {
                equity: 1_000_000.0,
                available: 1_000_000.0,
                unrealized_pnl: 0.0,
                observed_at: Utc::now(),
            })
        }
    }

    #[async_trait::async_trait]
    impl AccountPort for StartupExchange {
        async fn get_account_capacity_snapshot(
            &self,
            instrument: &Instrument,
        ) -> Result<poise_engine::ports::AccountCapacitySnapshot> {
            assert_eq!(instrument, &self.instrument);
            Ok(poise_engine::ports::AccountCapacitySnapshot {
                max_increase_notional: 1_000_000.0,
            })
        }

        async fn subscribe_user_data(
            &self,
        ) -> Result<mpsc::Receiver<poise_engine::ports::UserDataEvent>> {
            let (_sender, receiver) = mpsc::channel(1);
            Ok(receiver)
        }
    }

    #[async_trait::async_trait]
    impl MetadataPort for StartupExchange {
        async fn get_exchange_info(&self, instrument: &Instrument) -> Result<ExchangeInfo> {
            assert_eq!(instrument, &self.instrument);
            Ok(ExchangeInfo {
                instrument: instrument.clone(),
                rules: ExchangeRules {
                    price_tick: 0.1,
                    quantity_step: 0.001,
                    min_qty: 0.001,
                    min_notional: 5.0,
                    maker_fee_rate: 0.0,
                    taker_fee_rate: 0.0,
                },
            })
        }

        async fn get_server_time(&self) -> Result<chrono::DateTime<Utc>> {
            Ok(Utc::now())
        }
    }

    #[async_trait::async_trait]
    impl poise_engine::ports::MarketDataPort for StartupExchange {
        async fn subscribe_prices(
            &self,
            _instrument: &Instrument,
        ) -> Result<mpsc::Receiver<MarketDataTick>> {
            let (_sender, receiver) = mpsc::channel(1);
            Ok(receiver)
        }
    }
}
