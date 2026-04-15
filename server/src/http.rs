use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use poise_application::{DiagnosticSeverity, TrackMutationError};
use poise_engine::command::TrackCommand;
use poise_engine::track::TrackId;
use poise_protocol::{
    AccountSummaryView, ActivityLevelView, TrackCommandAccepted, TrackCommandRequest,
    TrackCommandType, TrackDetailView, TrackDiagnosticItemView, TrackDiagnosticsView,
    TrackListResponse,
};
use serde::Serialize;
use tower_http::cors::CorsLayer;

use crate::server_context::{HttpState, WebSocketState};

#[derive(Debug, Clone, PartialEq, Serialize)]
struct ErrorResponse {
    error: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct HealthResponse {
    status: String,
    track_count: usize,
    attention_required_count: usize,
}

pub fn router(http_state: HttpState, websocket_state: WebSocketState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/account", get(get_account))
        .route("/tracks", get(list_tracks))
        .route("/tracks/:id", get(get_track_detail))
        .route("/debug/tracks/:id/diagnostics", get(get_track_diagnostics))
        .route("/tracks/:id/commands", post(submit_command))
        .route(
            "/ws",
            get(move |ws| crate::websocket::ws_handler(ws, websocket_state.clone())),
        )
        .layer(CorsLayer::permissive())
        .with_state(http_state)
}

async fn list_tracks(
    State(state): State<HttpState>,
) -> Result<Json<TrackListResponse>, (StatusCode, Json<ErrorResponse>)> {
    let sources = state
        .query_service
        .list_track_sources()
        .await
        .map_err(map_query_error)?;
    let items = sources
        .iter()
        .map(|source| state.projector.project_list_item(source))
        .collect();
    Ok(Json(TrackListResponse { items }))
}

async fn health(
    State(state): State<HttpState>,
) -> Result<(StatusCode, Json<HealthResponse>), (StatusCode, Json<ErrorResponse>)> {
    let sources = state
        .query_service
        .list_track_sources()
        .await
        .map_err(map_query_error)?;
    let attention_required_count = sources
        .iter()
        .filter(|source| {
            source.has_recovery_anomaly
                || source.has_account_margin_guard
                || source.has_stale_market_data
        })
        .count();
    let status = if attention_required_count == 0 {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };

    Ok((
        status,
        Json(HealthResponse {
            status: if attention_required_count == 0 {
                "ok".to_string()
            } else {
                "attention_required".to_string()
            },
            track_count: sources.len(),
            attention_required_count,
        }),
    ))
}

async fn get_account(
    State(state): State<HttpState>,
) -> Result<Json<AccountSummaryView>, (StatusCode, Json<ErrorResponse>)> {
    let summary = state
        .account_monitor
        .current_summary()
        .await
        .map(|model| state.account_projector.project_summary(&model))
        .unwrap_or_default();
    Ok(Json(summary))
}

async fn get_track_detail(
    Path(id): Path<String>,
    State(state): State<HttpState>,
) -> Result<Json<TrackDetailView>, (StatusCode, Json<ErrorResponse>)> {
    let track_id = TrackId::new(id.clone());
    let source = state
        .query_service
        .load_track_detail_source(&track_id)
        .await
        .map_err(map_query_error)?
        .ok_or_else(|| not_found(format!("track `{id}` not found")))?;
    Ok(Json(state.projector.project_detail(&source)))
}

async fn get_track_diagnostics(
    Path(id): Path<String>,
    State(state): State<HttpState>,
) -> Result<Json<TrackDiagnosticsView>, (StatusCode, Json<ErrorResponse>)> {
    let track_id = TrackId::new(id.clone());
    let diagnostics = state
        .debug_query_service
        .load_track_diagnostics(&track_id)
        .await
        .map_err(map_query_error)?
        .ok_or_else(|| not_found(format!("track `{id}` not found")))?;

    Ok(Json(TrackDiagnosticsView {
        items: diagnostics
            .into_iter()
            .map(|item| TrackDiagnosticItemView {
                ts: item.observed_at.to_rfc3339(),
                message: item.message,
                level: project_diagnostic_severity(item.severity),
            })
            .collect(),
    }))
}

async fn submit_command(
    Path(id): Path<String>,
    State(state): State<HttpState>,
    Json(request): Json<TrackCommandRequest>,
) -> Result<Json<TrackCommandAccepted>, (StatusCode, Json<ErrorResponse>)> {
    if !state.command_service.has_track(&id).await {
        return Err(not_found(format!("track `{id}` not found")));
    }

    let command = map_command(request.command)?;
    state
        .command_service
        .command(&id, command)
        .await
        .map_err(map_command_error)?;

    Ok(Json(TrackCommandAccepted {
        track_id: id,
        command: request.command,
        accepted: true,
    }))
}

fn map_command(
    command: TrackCommandType,
) -> Result<TrackCommand, (StatusCode, Json<ErrorResponse>)> {
    match command {
        TrackCommandType::Pause => Ok(TrackCommand::Pause),
        TrackCommandType::Resume => Ok(TrackCommand::Resume),
        TrackCommandType::Terminate => Ok(TrackCommand::Terminate),
        TrackCommandType::Flatten => Ok(TrackCommand::Flatten),
    }
}

fn bad_request(message: String) -> (StatusCode, Json<ErrorResponse>) {
    (
        StatusCode::BAD_REQUEST,
        Json(ErrorResponse { error: message }),
    )
}

fn not_found(message: String) -> (StatusCode, Json<ErrorResponse>) {
    (
        StatusCode::NOT_FOUND,
        Json(ErrorResponse { error: message }),
    )
}

fn internal_error(message: String) -> (StatusCode, Json<ErrorResponse>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorResponse { error: message }),
    )
}

fn map_query_error(error: anyhow::Error) -> (StatusCode, Json<ErrorResponse>) {
    internal_error(error.to_string())
}

fn project_diagnostic_severity(severity: DiagnosticSeverity) -> ActivityLevelView {
    match severity {
        DiagnosticSeverity::Info => ActivityLevelView::Info,
        DiagnosticSeverity::Warn => ActivityLevelView::Warn,
    }
}

fn map_command_error(error: anyhow::Error) -> (StatusCode, Json<ErrorResponse>) {
    match error.downcast::<TrackMutationError>() {
        Ok(TrackMutationError::LoadedTrackInvariant { track_id }) => {
            internal_error(TrackMutationError::LoadedTrackInvariant { track_id }.to_string())
        }
        Ok(TrackMutationError::Mutation(error)) => bad_request(error.to_string()),
        Ok(TrackMutationError::Persistence(error)) => internal_error(error.to_string()),
        Err(error) => internal_error(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use anyhow::anyhow;
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use chrono::{TimeZone, Utc};
    use poise_application::{
        CommittedTrackWrite, EffectStatusUpdate, FollowUpRetirementRequest, PersistedTrackEffect,
        StoredTrackEvent, StoredTrackSnapshot, TrackEffectStore, TrackMutationStore,
        TrackQueryStore,
    };
    use poise_core::risk::CapacityBudget;
    use poise_core::strategy::{OutOfBandPolicy, ShapeFamily, TrackConfig};
    use poise_core::{
        events::DomainEvent,
        types::{ExchangeRules, Exposure},
    };
    use poise_engine::ledger::{LedgerGapReason, LedgerGapRecord};
    use poise_engine::manager::TrackManager;
    use poise_engine::ports::AccountSummarySnapshot;
    use poise_engine::ports::{ClockPort, OrderStatus};
    use poise_engine::track::{Instrument, TrackId, Venue};
    use poise_protocol::{
        AccountSummaryView, ExecutionIntentView, ExecutionSlotPhaseView, ExecutionStatusView,
        RiskSignalView, TrackCommandAccepted, TrackCommandRequest, TrackCommandType,
        TrackDetailView, TrackDiagnosticsView, TrackListResponse, TrackStatus,
    };
    use poise_storage::sqlite::SqliteStorage;
    use tower::ServiceExt;

    use crate::account_projector::AccountProjector;
    use crate::projector::TrackProjector;
    use crate::server_context::{HttpState, WebSocketState};
    use crate::test_support::{
        build_http_state, build_test_application_services, build_websocket_state,
        test_prepared_registry, unavailable_account_monitor,
    };

    use poise_application::{
        AccountMonitor, AccountMonitorConfig, AccountMonitorStore, ApplicationNotification,
        StoredAccountMonitorState, TrackDebugQueryService, TrackQueryService,
    };

    #[derive(Clone)]
    struct HttpTestState {
        http_state: HttpState,
        websocket_state: WebSocketState,
    }

    fn router(state: HttpTestState) -> axum::Router {
        super::router(state.http_state, state.websocket_state)
    }

    fn test_exchange_rules() -> ExchangeRules {
        ExchangeRules {
            price_tick: 0.0,
            quantity_step: 0.0,
            min_qty: 0.0,
            min_notional: 0.0,
            maker_fee_rate: 0.0,
            taker_fee_rate: 0.0,
        }
    }

    struct FakeClock;

    impl ClockPort for FakeClock {
        fn now(&self) -> chrono::DateTime<Utc> {
            Utc::now()
        }
    }

    struct AccountSummaryOnlyExchange;

    #[async_trait::async_trait]
    impl poise_engine::ports::AccountSummaryPort for AccountSummaryOnlyExchange {
        async fn get_account_summary(&self) -> anyhow::Result<AccountSummarySnapshot> {
            Err(anyhow!("not used in tests"))
        }
    }

    async fn app_state() -> HttpTestState {
        let repository = Arc::new(SqliteStorage::in_memory().unwrap());
        build_test_state(repository).await
    }

    async fn build_test_state<R>(repository: Arc<R>) -> HttpTestState
    where
        R: TrackMutationStore + TrackEffectStore + TrackQueryStore + 'static,
    {
        let manager = test_manager();
        let mut snapshot = manager
            .snapshot("btc-core")
            .expect("seeded manager should expose runtime snapshot");
        seed_snapshot_ledger(&mut snapshot);
        repository
            .save_transition(
                "btc-core",
                &snapshot,
                &[DomainEvent::ExposureTargetChanged {
                    from: Exposure(3.5),
                    to: Exposure(4.0),
                }],
                &[],
            )
            .await
            .unwrap();
        let (notifications, _) = tokio::sync::broadcast::channel::<ApplicationNotification>(16);
        let mutation_store: Arc<dyn TrackMutationStore> = repository.clone();
        let effect_store: Arc<dyn TrackEffectStore> = repository.clone();
        let query_store: Arc<dyn TrackQueryStore> = repository.clone();
        let account_margin_guard = Arc::new(crate::runtime::AccountMarginGuardStore::default());
        let services = build_test_application_services(
            manager,
            mutation_store.clone(),
            effect_store.clone(),
            notifications,
            account_margin_guard.clone(),
        );
        let query_service = Arc::new(TrackQueryService::new(
            query_store,
            test_prepared_registry("btc-core"),
        ));
        let debug_query_service = Arc::new(TrackDebugQueryService::new(query_service.clone()));
        let projector = Arc::new(TrackProjector::new());
        let account_monitor = unavailable_account_monitor(services.notifications.clone());
        let account_projector = Arc::new(AccountProjector::new());
        HttpTestState {
            http_state: build_http_state(
                &services,
                query_service.clone(),
                debug_query_service,
                projector.clone(),
                account_monitor.clone(),
                account_projector.clone(),
            ),
            websocket_state: build_websocket_state(
                &services,
                Arc::new(TrackQueryService::new(
                    repository as Arc<dyn TrackQueryStore>,
                    test_prepared_registry("btc-core"),
                )),
                projector,
                account_monitor,
                account_projector,
            ),
        }
    }

    async fn app_state_with_account_summary() -> HttpTestState {
        let repository = Arc::new(SqliteStorage::in_memory().unwrap());
        let manager = test_manager();
        let mut snapshot = manager
            .snapshot("btc-core")
            .expect("seeded manager should expose runtime snapshot");
        seed_snapshot_ledger(&mut snapshot);
        repository
            .save_transition(
                "btc-core",
                &snapshot,
                &[DomainEvent::ExposureTargetChanged {
                    from: Exposure(3.5),
                    to: Exposure(4.0),
                }],
                &[],
            )
            .await
            .unwrap();

        let (notifications, _) = tokio::sync::broadcast::channel::<ApplicationNotification>(16);
        let mutation_store: Arc<dyn TrackMutationStore> = repository.clone();
        let effect_store: Arc<dyn TrackEffectStore> = repository.clone();
        let query_store: Arc<dyn TrackQueryStore> = repository.clone();
        let account_margin_guard = Arc::new(crate::runtime::AccountMarginGuardStore::default());
        let services = build_test_application_services(
            manager,
            mutation_store,
            effect_store,
            notifications.clone(),
            account_margin_guard,
        );
        let account_store: Arc<dyn AccountMonitorStore> =
            Arc::new(SqliteStorage::in_memory().unwrap());
        account_store
            .save_state(&StoredAccountMonitorState {
                trading_day: chrono::NaiveDate::from_ymd_opt(2026, 4, 4).unwrap(),
                baseline_equity: 13_000.0,
                baseline_captured_at: Utc.with_ymd_and_hms(2026, 4, 4, 0, 0, 1).unwrap(),
                last_observed_account_snapshot: Some(AccountSummarySnapshot {
                    equity: 12_500.0,
                    available: 9_000.0,
                    unrealized_pnl: -350.0,
                    observed_at: Utc.with_ymd_and_hms(2026, 4, 4, 1, 23, 45).unwrap(),
                }),
            })
            .await
            .unwrap();
        let account_monitor = Arc::new(
            AccountMonitor::restore(
                Arc::new(AccountSummaryOnlyExchange),
                account_store,
                notifications,
                AccountMonitorConfig::default(),
            )
            .await
            .unwrap(),
        );
        let projector = Arc::new(TrackProjector::new());
        let account_projector = Arc::new(AccountProjector::new());
        let query_service = Arc::new(TrackQueryService::new(
            query_store,
            test_prepared_registry("btc-core"),
        ));
        let debug_query_service = Arc::new(TrackDebugQueryService::new(query_service.clone()));
        HttpTestState {
            http_state: build_http_state(
                &services,
                query_service.clone(),
                debug_query_service,
                projector.clone(),
                account_monitor.clone(),
                account_projector.clone(),
            ),
            websocket_state: build_websocket_state(
                &services,
                query_service,
                projector,
                account_monitor,
                account_projector,
            ),
        }
    }

    fn test_manager() -> TrackManager {
        let mut manager = TrackManager::new(Arc::new(FakeClock));
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
                    out_of_band_policy: OutOfBandPolicy::Freeze,
                },
                CapacityBudget {
                    max_notional: 3000.0,
                    daily_loss_limit: 100.0,
                    total_loss_limit: 300.0,
                },
                test_exchange_rules(),
            )
            .unwrap();
        manager
            .observe(
                &TrackId::new("btc-core"),
                poise_engine::observation::TrackObservation::Market(
                    poise_engine::observation::MarketObservation {
                        mark_price: 95.0,
                        execution_quote: Some(poise_engine::ports::ExecutionQuote {
                            best_bid: 95.0,
                            best_ask: 95.0,
                        }),
                    },
                ),
            )
            .unwrap();
        let track = manager
            .get_track("btc-core")
            .expect("track should still exist")
            .clone();
        let mut snapshot = track.snapshot();
        let slot_order = snapshot
            .executor_state
            .slots
            .first_mut()
            .and_then(|slot| slot.working_order.as_mut())
            .expect("market observe should seed inventory_core working order");
        slot_order.order_id = Some("order-1".into());
        slot_order.status = OrderStatus::New;
        manager.restore_track_state(&snapshot).unwrap();
        manager
    }

    fn seed_snapshot_ledger(snapshot: &mut poise_engine::snapshot::TrackRuntimeSnapshot) {
        snapshot.risk.unrealized_pnl = 265.2;
        snapshot.ledger_state.realized_pnl_day =
            Some(chrono::NaiveDate::from_ymd_opt(2026, 3, 24).unwrap());
        snapshot.ledger_state.gross_realized_pnl_today = 980.1;
        snapshot.ledger_state.gross_realized_pnl_cumulative = 980.1;
        snapshot.ledger_state.trading_fee_cumulative = 12.3;
        snapshot.ledger_state.funding_fee_cumulative = -4.0;
        snapshot.ledger_state.unresolved_gaps = vec![
            LedgerGapRecord {
                gap_key: "binance:order_trade_update:btcusdt:12345:commission_asset".into(),
                reason: LedgerGapReason::UnsupportedCommissionAsset,
                observed_at: Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0).unwrap(),
                source: "ORDER_TRADE_UPDATE".into(),
            },
            LedgerGapRecord {
                gap_key: "binance:funding_fee:btcusdt:2026-03-24T08:00:00+00:00:missing_symbol"
                    .into(),
                reason: LedgerGapReason::MissingSymbol,
                observed_at: Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0).unwrap(),
                source: "ACCOUNT_UPDATE:FUNDING_FEE".into(),
            },
        ];
    }

    #[tokio::test]
    async fn router_accepts_http_state_without_runtime_dependencies() {
        let response = router(app_state().await)
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn list_tracks_returns_track_list_response() {
        let response = router(app_state().await)
            .oneshot(
                Request::builder()
                    .uri("/tracks")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let payload: TrackListResponse = serde_json::from_slice(&body).unwrap();

        assert_eq!(payload.items.len(), 1);
        assert_eq!(payload.items[0].id, "btc-core");
        assert_eq!(payload.items[0].instrument.symbol, "BTCUSDT");
        assert_eq!(
            payload.items[0].execution.execution_status,
            ExecutionStatusView::Normal
        );
        assert_eq!(payload.items[0].execution.active_slot_count, 1);
    }

    #[tokio::test]
    async fn health_returns_ok_for_normal_runtime_state() {
        let response = router(app_state().await)
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(payload["status"], "ok");
        assert_eq!(payload["track_count"], 1);
        assert_eq!(payload["attention_required_count"], 0);
    }

    #[tokio::test]
    async fn health_returns_service_unavailable_when_attention_required_present() {
        let repository = Arc::new(SqliteStorage::in_memory().unwrap());
        let state = build_test_state(repository.clone()).await;
        let mut snapshot = test_manager()
            .snapshot("btc-core")
            .expect("seeded manager should expose runtime snapshot");
        snapshot.observed.market_data_stale_since = Some(Utc::now());
        repository
            .save_transition("btc-core", &snapshot, &[], &[])
            .await
            .unwrap();

        let response = router(state)
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(payload["status"], "attention_required");
        assert_eq!(payload["track_count"], 1);
        assert_eq!(payload["attention_required_count"], 1);
    }

    #[tokio::test]
    async fn get_track_detail_returns_track_detail_view() {
        let response = router(app_state().await)
            .oneshot(
                Request::builder()
                    .uri("/tracks/btc-core")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let payload: TrackDetailView = serde_json::from_slice(&body).unwrap();
        let payload_json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(payload.identity.id, "btc-core");
        assert_eq!(payload.identity.instrument.symbol, "BTCUSDT");
        assert_eq!(
            payload_json["strategy"]["long_exposure_units"].as_f64(),
            Some(8.0)
        );
        assert_eq!(
            payload_json["strategy"]["short_exposure_units"].as_f64(),
            Some(8.0)
        );
        assert_eq!(
            payload_json["strategy"]["notional_per_unit"].as_f64(),
            Some(375.0)
        );
        assert_eq!(
            payload_json["strategy"]["min_rebalance_units"].as_f64(),
            Some(0.5)
        );
        assert_eq!(
            payload_json["ledger"]["gross_realized_pnl"].as_f64(),
            Some(980.1)
        );
        assert!(
            (payload_json["ledger"]["net_realized_pnl"].as_f64().unwrap() - 963.8).abs() < 1e-9
        );
        assert!((payload_json["ledger"]["total_pnl"].as_f64().unwrap() - 1229.0).abs() < 1e-9);
        assert_eq!(
            payload_json["ledger"]["unrealized_pnl"].as_f64(),
            Some(265.2)
        );
        assert_eq!(
            payload_json["execution_stats"]["max_inventory_gap_abs"].as_f64(),
            Some(payload.execution_stats.max_inventory_gap_abs)
        );
        assert_eq!(
            payload_json["execution_stats"]["max_gap_age_ms"].as_i64(),
            Some(0)
        );
        assert!(payload.execution_stats.stats_started_at.is_some());
        assert_eq!(
            payload_json["execution_stats"]["stats_started_at"].as_str(),
            payload.execution_stats.stats_started_at.as_deref()
        );
        assert_eq!(
            payload.execution.execution_status,
            ExecutionStatusView::Normal
        );
        assert_eq!(payload.execution.active_slot_count, 1);
        assert_eq!(payload.execution.slots.len(), 1);
        assert_eq!(payload.execution.slots[0].label, "inventory");
        assert_eq!(
            payload.execution.slots[0].phase,
            ExecutionSlotPhaseView::Opening
        );
        assert_eq!(
            payload.execution.slots[0].intent,
            ExecutionIntentView::IncreaseInventory
        );
        assert!(!payload.available_commands.is_empty());
        assert!(payload_json["execution"]["slots"][0].get("state").is_none());
        assert!(
            !payload
                .activity
                .iter()
                .any(|item| item.message.contains("client-1"))
        );
    }

    #[tokio::test]
    async fn get_account_returns_latest_summary() {
        let response = router(app_state_with_account_summary().await)
            .oneshot(
                Request::builder()
                    .uri("/account")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let payload: AccountSummaryView = serde_json::from_slice(&body).unwrap();

        assert_eq!(
            payload,
            AccountSummaryView {
                equity: Some(12_500.0),
                available: Some(9_000.0),
                unrealized_pnl: Some(-350.0),
                day_change_pct: Some(-3.8461538461538463),
                risk_signal: RiskSignalView::Attention,
                reason: Some("day_change -3.8%".to_string()),
                day_base_at: Some("2026-04-04T00:00:01+00:00".to_string()),
                updated_at: Some("2026-04-04T01:23:45+00:00".to_string()),
            }
        );
    }

    #[tokio::test]
    async fn submit_command_accepts_typed_command() {
        let response = router(app_state().await)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/tracks/btc-core/commands")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&TrackCommandRequest {
                            command: TrackCommandType::Pause,
                        })
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let payload: TrackCommandAccepted = serde_json::from_slice(&body).unwrap();

        assert!(payload.accepted);
        assert_eq!(payload.track_id, "btc-core");
        assert_eq!(payload.command, TrackCommandType::Pause);
    }

    #[tokio::test]
    async fn submit_command_accepts_flatten() {
        let response = router(app_state().await)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/tracks/btc-core/commands")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&TrackCommandRequest {
                            command: TrackCommandType::Flatten,
                        })
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let payload: TrackCommandAccepted = serde_json::from_slice(&body).unwrap();
        assert!(payload.accepted);
        assert_eq!(payload.track_id, "btc-core");
        assert_eq!(payload.command, TrackCommandType::Flatten);
    }

    #[tokio::test]
    async fn submit_command_accepts_terminate() {
        let response = router(app_state().await)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/tracks/btc-core/commands")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&TrackCommandRequest {
                            command: TrackCommandType::Terminate,
                        })
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let payload: TrackCommandAccepted = serde_json::from_slice(&body).unwrap();
        assert!(payload.accepted);
        assert_eq!(payload.track_id, "btc-core");
        assert_eq!(payload.command, TrackCommandType::Terminate);
    }

    #[tokio::test]
    async fn resume_command_rejects_non_paused_track() {
        let response = router(app_state().await)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/tracks/btc-core/commands")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&TrackCommandRequest {
                            command: TrackCommandType::Resume,
                        })
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn submit_command_rolls_back_detail_when_persistence_fails() {
        let repository = Arc::new(FailingRepository::default());
        let manager = test_manager();
        repository.seed_snapshot(
            manager
                .snapshot("btc-core")
                .expect("seeded manager should expose runtime snapshot"),
        );
        let (notifications, _) = tokio::sync::broadcast::channel::<ApplicationNotification>(16);
        let mutation_store = repository.clone() as Arc<dyn TrackMutationStore>;
        let effect_store = repository.clone() as Arc<dyn TrackEffectStore>;
        let query_service = Arc::new(TrackQueryService::new(
            repository.clone() as Arc<dyn TrackQueryStore>,
            test_prepared_registry("btc-core"),
        ));
        let account_margin_guard = Arc::new(crate::runtime::AccountMarginGuardStore::default());
        let services = build_test_application_services(
            manager,
            mutation_store,
            effect_store,
            notifications,
            account_margin_guard,
        );
        let projector = Arc::new(TrackProjector::new());
        let account_monitor = unavailable_account_monitor(services.notifications.clone());
        let account_projector = Arc::new(AccountProjector::new());
        let debug_query_service = Arc::new(TrackDebugQueryService::new(query_service.clone()));
        let app = router(HttpTestState {
            http_state: build_http_state(
                &services,
                query_service.clone(),
                debug_query_service,
                projector.clone(),
                account_monitor.clone(),
                account_projector.clone(),
            ),
            websocket_state: build_websocket_state(
                &services,
                query_service,
                projector,
                account_monitor,
                account_projector,
            ),
        });

        let pause = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/tracks/btc-core/commands")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&TrackCommandRequest {
                            command: TrackCommandType::Pause,
                        })
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(pause.status(), StatusCode::INTERNAL_SERVER_ERROR);

        let detail = app
            .oneshot(
                Request::builder()
                    .uri("/tracks/btc-core")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let body = to_bytes(detail.into_body(), usize::MAX).await.unwrap();
        let payload: TrackDetailView = serde_json::from_slice(&body).unwrap();
        assert_eq!(payload.status.lifecycle.status, TrackStatus::Active);
    }

    #[tokio::test]
    async fn pause_command_updates_detail_status() {
        let app = router(app_state().await);

        let pause = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/tracks/btc-core/commands")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&TrackCommandRequest {
                            command: TrackCommandType::Pause,
                        })
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(pause.status(), StatusCode::OK);

        let detail = app
            .oneshot(
                Request::builder()
                    .uri("/tracks/btc-core")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let body = to_bytes(detail.into_body(), usize::MAX).await.unwrap();
        let payload: TrackDetailView = serde_json::from_slice(&body).unwrap();
        assert_eq!(payload.status.lifecycle.status, TrackStatus::Paused);
        assert_eq!(payload.position.desired_exposure, None);
    }

    #[tokio::test]
    async fn resume_command_reactivates_paused_track() {
        let app = router(app_state().await);

        let pause = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/tracks/btc-core/commands")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&TrackCommandRequest {
                            command: TrackCommandType::Pause,
                        })
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(pause.status(), StatusCode::OK);

        let resume = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/tracks/btc-core/commands")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&TrackCommandRequest {
                            command: TrackCommandType::Resume,
                        })
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resume.status(), StatusCode::OK);

        let detail = app
            .oneshot(
                Request::builder()
                    .uri("/tracks/btc-core")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let body = to_bytes(detail.into_body(), usize::MAX).await.unwrap();
        let payload: TrackDetailView = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            payload.status.lifecycle.status,
            TrackStatus::WaitingMarketData
        );
    }

    #[tokio::test]
    async fn get_track_detail_returns_404_for_missing_track() {
        let response = router(app_state().await)
            .oneshot(
                Request::builder()
                    .uri("/tracks/ETHUSDT")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn get_track_diagnostics_returns_exposure_target_changed_events() {
        let response = router(app_state().await)
            .oneshot(
                Request::builder()
                    .uri("/debug/tracks/btc-core/diagnostics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let payload: TrackDiagnosticsView = serde_json::from_slice(&body).unwrap();

        assert!(
            payload
                .items
                .iter()
                .any(|item| item.message.contains("desired exposure"))
        );
    }

    #[derive(Default)]
    struct FailingRepository {
        snapshots: std::sync::Mutex<std::collections::HashMap<String, StoredTrackSnapshot>>,
    }

    impl FailingRepository {
        fn seed_snapshot(&self, snapshot: poise_engine::snapshot::TrackRuntimeSnapshot) {
            self.snapshots.lock().unwrap().insert(
                snapshot.track_id.as_str().to_string(),
                StoredTrackSnapshot {
                    snapshot,
                    updated_at: Utc::now(),
                },
            );
        }
    }

    #[async_trait::async_trait]
    impl TrackMutationStore for FailingRepository {
        async fn save_transition_with_effect_status(
            &self,
            _id: &str,
            _state: &poise_engine::snapshot::TrackRuntimeSnapshot,
            _events: &[poise_core::events::DomainEvent],
            _effects: &[poise_engine::transition::TrackEffect],
            _effect_status_update: Option<&EffectStatusUpdate>,
        ) -> anyhow::Result<CommittedTrackWrite> {
            Err(anyhow!("persistence unavailable"))
        }

        async fn load_track_state(
            &self,
            _id: &str,
        ) -> anyhow::Result<Option<poise_engine::snapshot::TrackRuntimeSnapshot>> {
            Ok(None)
        }

        async fn list_track_events(
            &self,
            _id: &str,
        ) -> anyhow::Result<Vec<poise_core::events::DomainEvent>> {
            Ok(Vec::new())
        }
    }

    #[async_trait::async_trait]
    impl TrackEffectStore for FailingRepository {
        async fn list_dispatchable_effects(&self) -> anyhow::Result<Vec<PersistedTrackEffect>> {
            Ok(Vec::new())
        }

        async fn list_all_pending_submit_effects(
            &self,
        ) -> anyhow::Result<Vec<PersistedTrackEffect>> {
            Ok(Vec::new())
        }

        async fn list_pending_submit_effects_for_track(
            &self,
            _track_id: &TrackId,
        ) -> anyhow::Result<Vec<PersistedTrackEffect>> {
            Ok(Vec::new())
        }

        async fn list_pending_submit_effects_for_track_batch(
            &self,
            _track_id: &TrackId,
            _batch_id: &str,
        ) -> anyhow::Result<Vec<PersistedTrackEffect>> {
            Ok(Vec::new())
        }

        async fn save_follow_up_retirement_request(
            &self,
            _track_id: &TrackId,
            _request: &FollowUpRetirementRequest,
        ) -> anyhow::Result<()> {
            Ok(())
        }

        async fn list_follow_up_retirement_requests(
            &self,
            _track_id: &TrackId,
        ) -> anyhow::Result<Vec<FollowUpRetirementRequest>> {
            Ok(Vec::new())
        }

        async fn delete_follow_up_retirement_request(
            &self,
            _track_id: &TrackId,
            _request: &FollowUpRetirementRequest,
        ) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl TrackQueryStore for FailingRepository {
        async fn list_track_snapshots(&self) -> anyhow::Result<Vec<StoredTrackSnapshot>> {
            Ok(self.snapshots.lock().unwrap().values().cloned().collect())
        }

        async fn load_track_snapshot(
            &self,
            track_id: &TrackId,
        ) -> anyhow::Result<Option<StoredTrackSnapshot>> {
            Ok(self
                .snapshots
                .lock()
                .unwrap()
                .get(track_id.as_str())
                .cloned())
        }

        async fn list_recent_track_events(
            &self,
            _track_id: &TrackId,
            _limit: usize,
        ) -> anyhow::Result<Vec<StoredTrackEvent>> {
            Ok(Vec::new())
        }

        async fn list_recent_track_effects(
            &self,
            _track_id: &TrackId,
            _limit: usize,
        ) -> anyhow::Result<Vec<PersistedTrackEffect>> {
            Ok(Vec::new())
        }
    }
}
