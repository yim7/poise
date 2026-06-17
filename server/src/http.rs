use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use poise_application::{
    DiagnosticSeverity, TrackMutationError, build_account_analysis_read_model_with_account,
};
use poise_core::track::TrackId;
use poise_engine::command::TrackCommand;
use poise_protocol::{
    AccountSummaryView, ActivityLevelView, HealthResponse, HealthStatusView, HealthTaskStatusView,
    HealthTaskView, InstrumentView, PnlBackfillStatusView, RecentFillAuditItemView,
    RecentFillCoverageView, RecentFillsAuditResponse, TrackCommandAccepted, TrackCommandRequest,
    TrackCommandType, TrackDetailView, TrackDiagnosticItemView, TrackDiagnosticsView,
    TrackListResponse,
};
use serde::Serialize;
use tower_http::cors::CorsLayer;

use crate::runtime::{RuntimeHealthSnapshot, RuntimeHealthStatus, RuntimeTaskHealthSnapshot};
use crate::server_context::{HttpState, WebSocketState};

#[derive(Debug, Clone, PartialEq, Serialize)]
struct ErrorResponse {
    error: String,
}

pub fn router(http_state: HttpState, websocket_state: WebSocketState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/account", get(get_account))
        .route("/tracks", get(list_tracks))
        .route("/tracks/:id", get(get_track_detail))
        .route("/debug/tracks/:id/diagnostics", get(get_track_diagnostics))
        .route(
            "/debug/tracks/:id/recent-fills-audit",
            get(get_recent_fills_audit),
        )
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
            source.recovery_issue.is_some()
                || source.has_account_margin_guard
                || source.has_stale_market_data
        })
        .count();
    let runtime_health = state.runtime_health.snapshot();
    let task_attention_required = runtime_health_has_degraded_task(&runtime_health);
    let status = if attention_required_count == 0 && !task_attention_required {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };

    Ok((
        status,
        Json(HealthResponse {
            status: if attention_required_count == 0 && !task_attention_required {
                HealthStatusView::Ok
            } else {
                HealthStatusView::AttentionRequired
            },
            track_count: sources.len(),
            attention_required_count,
            tasks: project_runtime_health(runtime_health),
            pnl_backfill: project_pnl_backfill_status(state.pnl_backfill_status.snapshot()),
        }),
    ))
}

fn runtime_health_has_degraded_task(snapshot: &RuntimeHealthSnapshot) -> bool {
    snapshot
        .tasks
        .iter()
        .any(|task| task.status == RuntimeHealthStatus::Degraded)
}

fn project_runtime_health(snapshot: RuntimeHealthSnapshot) -> Vec<HealthTaskView> {
    snapshot
        .tasks
        .into_iter()
        .map(project_runtime_health_task)
        .collect()
}

fn project_runtime_health_task(task: RuntimeTaskHealthSnapshot) -> HealthTaskView {
    HealthTaskView {
        component: task.component.as_str().to_string(),
        status: match task.status {
            RuntimeHealthStatus::Unknown => HealthTaskStatusView::Unknown,
            RuntimeHealthStatus::Ok => HealthTaskStatusView::Ok,
            RuntimeHealthStatus::Degraded => HealthTaskStatusView::Degraded,
        },
        last_success_at: task.last_success_at.map(|value| value.to_rfc3339()),
        last_error_at: task.last_error_at.map(|value| value.to_rfc3339()),
        last_error: task.last_error,
    }
}

fn project_pnl_backfill_status(
    snapshot: crate::runtime::PnlBackfillSnapshot,
) -> PnlBackfillStatusView {
    PnlBackfillStatusView {
        last_completed_at: snapshot.last_completed_at.map(|value| value.to_rfc3339()),
        records_seen: snapshot.records_seen,
        records_inserted: snapshot.records_inserted,
        records_skipped: snapshot.records_skipped,
        failures: snapshot.failures,
        last_error_at: snapshot.last_error_at.map(|value| value.to_rfc3339()),
        last_error: snapshot.last_error,
    }
}

async fn get_account(
    State(state): State<HttpState>,
) -> Result<Json<AccountSummaryView>, (StatusCode, Json<ErrorResponse>)> {
    let summary = state.account_monitor.current_summary().await;
    let analysis = match state.query_service.list_track_sources().await {
        Ok(sources) => Some(build_account_analysis_read_model_with_account(
            summary.as_ref(),
            &sources,
        )),
        Err(error) => {
            tracing::warn!("failed to load account analysis for HTTP account summary: {error}");
            None
        }
    };

    Ok(Json(state.account_projector.project_summary_with_analysis(
        summary.as_ref(),
        analysis.as_ref(),
    )))
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

async fn get_recent_fills_audit(
    Path(id): Path<String>,
    State(state): State<HttpState>,
) -> Result<Json<RecentFillsAuditResponse>, (StatusCode, Json<ErrorResponse>)> {
    let track_id = TrackId::new(id.clone());
    let source = state
        .query_service
        .load_track_detail_source(&track_id)
        .await
        .map_err(map_query_error)?
        .ok_or_else(|| not_found(format!("track `{id}` not found")))?;
    let audit = state
        .recent_fills_auditor
        .audit_recent_fills(&track_id, &source.instrument)
        .await
        .map_err(map_query_error)?;

    Ok(Json(project_recent_fills_audit(
        source.track_id,
        source.instrument,
        audit,
    )))
}

fn project_recent_fills_audit(
    track_id: String,
    instrument: poise_core::track::Instrument,
    audit: crate::pnl_audit::RecentFillsAudit,
) -> RecentFillsAuditResponse {
    RecentFillsAuditResponse {
        track_id,
        instrument: InstrumentView {
            venue: instrument.venue.as_str().to_string(),
            symbol: instrument.symbol,
        },
        records_seen: audit.records_seen,
        records_recorded: audit.records_recorded,
        records_missing: audit.records_missing,
        records_unkeyed: audit.records_unkeyed,
        items: audit
            .items
            .into_iter()
            .map(|item| RecentFillAuditItemView {
                source_key: item.record.source_key,
                trade_id: item.record.trade_id,
                occurred_at: item.record.occurred_at.to_rfc3339(),
                coverage: match item.coverage {
                    crate::pnl_audit::RecentFillCoverage::Recorded => {
                        RecentFillCoverageView::Recorded
                    }
                    crate::pnl_audit::RecentFillCoverage::Missing => {
                        RecentFillCoverageView::Missing
                    }
                    crate::pnl_audit::RecentFillCoverage::Unkeyed => {
                        RecentFillCoverageView::Unkeyed
                    }
                },
            })
            .collect(),
    }
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
        CommittedTrackWrite, EffectJournalEntry, EffectStatusUpdate, PersistedTrackEffect,
        StoredTrackEvent, TrackEffectJournal, TrackMutationStore, TrackQueryStore,
    };
    use poise_core::risk::LossLimits;
    use poise_core::strategy::{BandProtectionPolicy, ShapeFamily, TrackConfig};
    use poise_core::track::{Instrument, TrackDefinition, TrackId, Venue};
    use poise_core::{
        events::DomainEvent,
        types::{ExchangeRules, Exposure, Side},
    };
    use poise_engine::ledger::TrackPnlRecord;
    use poise_engine::manager::TrackManager;
    use poise_engine::ports::{
        AccountCapacitySnapshot, AccountPort, AccountSummarySnapshot, ClockPort, UserDataEvent,
    };
    use poise_protocol::{
        AccountSummaryView, ExecutionBindingIntentView, ExecutionBindingStatusView,
        ExecutionStatusView, RecentFillCoverageView, RecentFillsAuditResponse, RiskSignalView,
        TrackCommandAccepted, TrackCommandRequest, TrackCommandType, TrackDetailView,
        TrackDiagnosticsView, TrackListResponse, TrackStatus,
    };
    use poise_storage::sqlite::SqliteStorage;
    use tokio::sync::mpsc;
    use tower::ServiceExt;

    use crate::account_projector::AccountProjector;
    use crate::pnl_audit::RecentFillsAuditor;
    use crate::projector::TrackProjector;
    use crate::runtime::{PnlBackfillStatus, RuntimeHealth, RuntimeHealthComponent};
    use crate::server_context::{HttpState, WebSocketState};
    use crate::test_support::{
        build_http_state, build_test_application_services, build_websocket_state,
        test_track_definition_registry, unavailable_account_monitor,
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
            price_precision: Default::default(),
            quantity_kind: Default::default(),
            contract_notional: None,
            settlement_asset: "USDT".to_string(),
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

    struct FailingQueryStore;

    #[async_trait::async_trait]
    impl TrackQueryStore for FailingQueryStore {
        async fn list_recent_track_events(
            &self,
            _track_id: &TrackId,
            _limit: usize,
        ) -> anyhow::Result<Vec<StoredTrackEvent>> {
            Err(anyhow!("query unavailable"))
        }

        async fn list_recent_track_effects(
            &self,
            _track_id: &TrackId,
            _limit: usize,
        ) -> anyhow::Result<Vec<PersistedTrackEffect>> {
            Err(anyhow!("query unavailable"))
        }

        async fn load_track_control_state(
            &self,
            _track_id: &TrackId,
        ) -> anyhow::Result<Option<poise_application::TrackControlState>> {
            Err(anyhow!("query unavailable"))
        }

        async fn load_track_pnl_stats(
            &self,
            _track_id: &TrackId,
            _pnl_utc_day: chrono::NaiveDate,
        ) -> anyhow::Result<poise_engine::ledger::TrackPnlStats> {
            Err(anyhow!("query unavailable"))
        }

        async fn list_track_pnl_source_keys(
            &self,
            _track_id: &TrackId,
            _source_keys: &[String],
        ) -> anyhow::Result<Vec<String>> {
            Err(anyhow!("query unavailable"))
        }

        async fn load_track_updated_at(
            &self,
            _track_id: &TrackId,
        ) -> anyhow::Result<Option<chrono::DateTime<chrono::Utc>>> {
            Err(anyhow!("query unavailable"))
        }
    }

    async fn app_state() -> HttpTestState {
        let repository = Arc::new(SqliteStorage::in_memory().unwrap());
        build_test_state(repository).await
    }

    async fn build_test_state<R>(repository: Arc<R>) -> HttpTestState
    where
        R: TrackMutationStore + TrackEffectJournal + TrackQueryStore + 'static,
    {
        let mut manager = test_manager();
        let mut snapshot = manager
            .mutation_frame("btc-core")
            .expect("seeded manager should expose mutation frame");
        seed_frame_pnl_stats(&mut snapshot);
        manager.rollback_track_state(&snapshot).unwrap();
        observe_seed_market(&mut manager);
        repository
            .commit_track_transition(
                "btc-core",
                None,
                &[DomainEvent::ExposureTargetChanged {
                    from: Exposure(3.5),
                    to: Exposure(4.0),
                }],
            )
            .await
            .unwrap();
        let (notifications, _) = tokio::sync::broadcast::channel::<ApplicationNotification>(16);
        let mutation_store: Arc<dyn TrackMutationStore> = repository.clone();
        let effect_store: Arc<dyn TrackEffectJournal> = repository.clone();
        let query_store: Arc<dyn TrackQueryStore> = repository.clone();
        let account_margin_guard = Arc::new(crate::runtime::AccountMarginGuardStore::default());
        let services = build_test_application_services(
            manager,
            mutation_store.clone(),
            query_store.clone(),
            effect_store.clone(),
            notifications,
            account_margin_guard.clone(),
        );
        let query_service = Arc::new(TrackQueryService::new(
            query_store.clone(),
            test_track_definition_registry("btc-core"),
            services.observation_service.clone(),
        ));
        let debug_query_service = Arc::new(TrackDebugQueryService::new(
            query_store.clone(),
            services.observation_service.clone(),
        ));
        let projector = Arc::new(TrackProjector::new());
        let account_monitor = unavailable_account_monitor(services.notifications.clone());
        let account_projector = Arc::new(AccountProjector::new());
        HttpTestState {
            http_state: build_http_state(
                &services,
                query_store.clone(),
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
                    test_track_definition_registry("btc-core"),
                    services.observation_service.clone(),
                )),
                projector,
                account_monitor,
                account_projector,
            ),
        }
    }

    async fn build_test_state_with_recent_fills_account<R>(
        repository: Arc<R>,
        account: Arc<dyn AccountPort>,
    ) -> HttpTestState
    where
        R: TrackMutationStore + TrackEffectJournal + TrackQueryStore + 'static,
    {
        let mut manager = test_manager();
        let mut snapshot = manager
            .mutation_frame("btc-core")
            .expect("seeded manager should expose mutation frame");
        seed_frame_pnl_stats(&mut snapshot);
        manager.rollback_track_state(&snapshot).unwrap();
        let (notifications, _) = tokio::sync::broadcast::channel::<ApplicationNotification>(16);
        let mutation_store: Arc<dyn TrackMutationStore> = repository.clone();
        let effect_store: Arc<dyn TrackEffectJournal> = repository.clone();
        let query_store: Arc<dyn TrackQueryStore> = repository.clone();
        let account_margin_guard = Arc::new(crate::runtime::AccountMarginGuardStore::default());
        let services = build_test_application_services(
            manager,
            mutation_store,
            query_store.clone(),
            effect_store,
            notifications,
            account_margin_guard,
        );
        let query_service = Arc::new(TrackQueryService::new(
            query_store.clone(),
            test_track_definition_registry("btc-core"),
            services.observation_service.clone(),
        ));
        let debug_query_service = Arc::new(TrackDebugQueryService::new(
            query_store.clone(),
            services.observation_service.clone(),
        ));
        let projector = Arc::new(TrackProjector::new());
        let account_monitor = unavailable_account_monitor(services.notifications.clone());
        let account_projector = Arc::new(AccountProjector::new());
        HttpTestState {
            http_state: crate::assembly::build_http_state(
                Arc::clone(&services.command_service),
                query_service.clone(),
                debug_query_service,
                projector.clone(),
                account_monitor.clone(),
                account_projector.clone(),
                Arc::new(RuntimeHealth::new()),
                Arc::new(PnlBackfillStatus::new()),
                Arc::new(RecentFillsAuditor::new(account, query_store.clone())),
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

    async fn app_state_with_account_summary() -> HttpTestState {
        let repository = Arc::new(SqliteStorage::in_memory().unwrap());
        let mut manager = test_manager();
        let mut snapshot = manager
            .mutation_frame("btc-core")
            .expect("seeded manager should expose mutation frame");
        seed_frame_pnl_stats(&mut snapshot);
        manager.rollback_track_state(&snapshot).unwrap();
        observe_seed_market(&mut manager);
        repository
            .commit_track_transition(
                "btc-core",
                None,
                &[DomainEvent::ExposureTargetChanged {
                    from: Exposure(3.5),
                    to: Exposure(4.0),
                }],
            )
            .await
            .unwrap();
        let (notifications, _) = tokio::sync::broadcast::channel::<ApplicationNotification>(16);
        let mutation_store: Arc<dyn TrackMutationStore> = repository.clone();
        let effect_store: Arc<dyn TrackEffectJournal> = repository.clone();
        let query_store: Arc<dyn TrackQueryStore> = repository.clone();
        let account_margin_guard = Arc::new(crate::runtime::AccountMarginGuardStore::default());
        let services = build_test_application_services(
            manager,
            mutation_store,
            query_store.clone(),
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
                    available_by_asset: Default::default(),
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
            query_store.clone(),
            test_track_definition_registry("btc-core"),
            services.observation_service.clone(),
        ));
        let debug_query_service = Arc::new(TrackDebugQueryService::new(
            query_store.clone(),
            services.observation_service.clone(),
        ));
        HttpTestState {
            http_state: build_http_state(
                &services,
                query_store.clone(),
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
                TrackDefinition::try_new(
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
                        risk_acquisition: Default::default(),
                    },
                    Some(3000.0),
                    LossLimits {
                        daily_loss_limit: 100.0,
                        total_loss_limit: 300.0,
                    },
                    None,
                )
                .unwrap(),
                test_exchange_rules(),
            )
            .unwrap();
        observe_seed_market(&mut manager);
        manager
    }

    fn observe_seed_market(manager: &mut TrackManager) {
        manager
            .observe(
                &TrackId::new("btc-core"),
                poise_engine::observation::TrackObservation::Market(
                    poise_engine::observation::MarketObservation::ExecutionQuote {
                        execution_quote: poise_engine::ports::ExecutionQuote {
                            best_bid: 95.0,
                            best_ask: 95.0,
                        },
                    },
                ),
            )
            .unwrap();
    }

    fn seed_frame_pnl_stats(snapshot: &mut poise_engine::mutation_frame::TrackMutationFrame) {
        snapshot.set_unrealized_pnl(265.2);
        let mut pnl_stats = snapshot.pnl_stats().clone();
        pnl_stats.pnl_utc_day = chrono::NaiveDate::from_ymd_opt(2026, 3, 24).unwrap();
        pnl_stats.gross_realized_pnl_today = 980.1;
        pnl_stats.gross_realized_pnl_cumulative = 980.1;
        pnl_stats.trading_fee_cumulative = 12.3;
        pnl_stats.funding_fee_cumulative = -4.0;
        snapshot.replace_pnl_stats(pnl_stats);
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
        assert!(payload.items[0].execution.active_binding_count > 0);
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
        assert_eq!(payload["tasks"].as_array().unwrap().len(), 6);
        let pnl_backfill = payload["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|task| task["component"] == "pnl_backfill")
            .unwrap();
        assert_eq!(pnl_backfill["status"], "unknown");
        assert!(pnl_backfill["last_success_at"].is_null());
        assert!(pnl_backfill["last_error"].is_null());
        assert!(payload["pnl_backfill"]["last_completed_at"].is_null());
        assert_eq!(payload["pnl_backfill"]["records_seen"], 0);
        assert_eq!(payload["pnl_backfill"]["records_inserted"], 0);
        assert_eq!(payload["pnl_backfill"]["records_skipped"], 0);
        assert_eq!(payload["pnl_backfill"]["failures"], 0);
        assert!(payload["pnl_backfill"]["last_error_at"].is_null());
        assert!(payload["pnl_backfill"]["last_error"].is_null());
    }

    #[tokio::test]
    async fn health_returns_service_unavailable_when_attention_required_present() {
        let repository = Arc::new(SqliteStorage::in_memory().unwrap());
        let mut manager = test_manager();
        let mut snapshot = manager
            .mutation_frame("btc-core")
            .expect("seeded manager should expose mutation frame");
        snapshot.set_market_data_stale_since(Some(Utc::now()));
        manager.rollback_track_state(&snapshot).unwrap();
        let state = {
            let (notifications, _) = tokio::sync::broadcast::channel::<ApplicationNotification>(16);
            let mutation_store: Arc<dyn TrackMutationStore> = repository.clone();
            let effect_store: Arc<dyn TrackEffectJournal> = repository.clone();
            let query_store: Arc<dyn TrackQueryStore> = repository.clone();
            let account_margin_guard = Arc::new(crate::runtime::AccountMarginGuardStore::default());
            let services = build_test_application_services(
                manager,
                mutation_store.clone(),
                query_store.clone(),
                effect_store.clone(),
                notifications,
                account_margin_guard.clone(),
            );
            let query_service = Arc::new(TrackQueryService::new(
                query_store.clone(),
                test_track_definition_registry("btc-core"),
                services.observation_service.clone(),
            ));
            let debug_query_service = Arc::new(TrackDebugQueryService::new(
                query_store.clone(),
                services.observation_service.clone(),
            ));
            let projector = Arc::new(TrackProjector::new());
            let account_monitor = unavailable_account_monitor(services.notifications.clone());
            let account_projector = Arc::new(AccountProjector::new());
            HttpTestState {
                http_state: build_http_state(
                    &services,
                    query_store.clone(),
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
                        test_track_definition_registry("btc-core"),
                        services.observation_service.clone(),
                    )),
                    projector,
                    account_monitor,
                    account_projector,
                ),
            }
        };

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
    async fn health_returns_service_unavailable_when_runtime_task_degraded() {
        let state = app_state().await;
        state.http_state.runtime_health.record_error(
            RuntimeHealthComponent::PnlBackfill,
            Utc.with_ymd_and_hms(2026, 6, 17, 1, 2, 3).unwrap(),
            "temporary okx outage",
        );

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
        assert_eq!(payload["attention_required_count"], 0);
        let pnl_backfill = payload["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|task| task["component"] == "pnl_backfill")
            .unwrap();
        assert_eq!(pnl_backfill["status"], "degraded");
        assert_eq!(pnl_backfill["last_error_at"], "2026-06-17T01:02:03+00:00");
        assert_eq!(pnl_backfill["last_error"], "temporary okx outage");
    }

    #[tokio::test]
    async fn health_returns_pnl_backfill_observability() {
        let state = app_state().await;
        let observed_at = Utc.with_ymd_and_hms(2026, 6, 17, 1, 2, 3).unwrap();
        state.http_state.pnl_backfill_status.replace_snapshot(
            crate::runtime::PnlBackfillSnapshot {
                last_completed_at: Some(observed_at),
                records_seen: 5,
                records_inserted: 2,
                records_skipped: 3,
                failures: 1,
                last_error_at: Some(observed_at),
                last_error: Some("failed to persist backfilled track pnl record".to_string()),
            },
        );

        let response = router(state)
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
        assert_eq!(
            payload["pnl_backfill"]["last_completed_at"],
            "2026-06-17T01:02:03+00:00"
        );
        assert_eq!(payload["pnl_backfill"]["records_seen"], 5);
        assert_eq!(payload["pnl_backfill"]["records_inserted"], 2);
        assert_eq!(payload["pnl_backfill"]["records_skipped"], 3);
        assert_eq!(payload["pnl_backfill"]["failures"], 1);
        assert_eq!(
            payload["pnl_backfill"]["last_error_at"],
            "2026-06-17T01:02:03+00:00"
        );
        assert_eq!(
            payload["pnl_backfill"]["last_error"],
            "failed to persist backfilled track pnl record"
        );
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
            payload_json["pnl"]["gross_realized_pnl"].as_f64(),
            Some(980.1)
        );
        assert!((payload_json["pnl"]["net_realized_pnl"].as_f64().unwrap() - 963.8).abs() < 1e-9);
        assert!((payload_json["pnl"]["total_pnl"].as_f64().unwrap() - 1229.0).abs() < 1e-9);
        assert_eq!(payload_json["pnl"]["unrealized_pnl"].as_f64(), Some(265.2));
        assert_eq!(
            payload.execution.execution_status,
            ExecutionStatusView::Normal
        );
        assert!(payload.execution.active_binding_count > 0);
        assert_eq!(
            payload.execution.active_binding_count,
            payload.execution.bindings.len() as u32
        );
        assert!(
            payload
                .execution
                .bindings
                .iter()
                .all(|binding| binding.label.starts_with("maker ")
                    || binding.label.starts_with("target "))
        );
        assert!(
            payload
                .execution
                .bindings
                .iter()
                .all(|binding| binding.status == ExecutionBindingStatusView::SubmitPending)
        );
        assert!(payload.execution.bindings.iter().all(|binding| matches!(
            binding.intent,
            ExecutionBindingIntentView::IncreaseInventory
                | ExecutionBindingIntentView::DecreaseInventory
        )));
        assert!(!payload.available_commands.is_empty());
        assert!(
            payload_json["execution"]["bindings"][0]
                .get("phase")
                .is_none()
        );
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

        assert_eq!(payload.equity, Some(12_500.0));
        assert_eq!(payload.available, Some(9_000.0));
        assert_eq!(payload.unrealized_pnl, Some(-350.0));
        assert_eq!(payload.day_change_pct, Some(-3.8461538461538463));
        assert_eq!(payload.risk_signal, RiskSignalView::Attention);
        assert_eq!(payload.reason.as_deref(), Some("day_change -3.8%"));
        assert_eq!(
            payload.day_base_at.as_deref(),
            Some("2026-04-04T00:00:01+00:00")
        );
        assert_eq!(
            payload.updated_at.as_deref(),
            Some("2026-04-04T01:23:45+00:00")
        );

        let analysis = payload
            .analysis
            .expect("account analysis should be present");
        assert_eq!(analysis.tracks.len(), 1);
        assert_eq!(analysis.tracks[0].track_id, "btc-core");
        assert_eq!(analysis.tracks[0].settlement_asset, "USDT");
        assert_eq!(analysis.tracks[0].pnl_asset, "USDT");
    }

    #[tokio::test]
    async fn get_account_keeps_summary_when_analysis_query_fails() {
        let mut state = app_state_with_account_summary().await;
        state.http_state.query_service = Arc::new(TrackQueryService::new(
            Arc::new(FailingQueryStore),
            test_track_definition_registry("btc-core"),
            state.websocket_state.observation_service.clone(),
        ));

        let response = router(state)
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

        assert_eq!(payload.equity, Some(12_500.0));
        assert_eq!(payload.available, Some(9_000.0));
        assert_eq!(payload.analysis, None);
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
        repository.seed_track(&TrackId::new("btc-core"));
        let (notifications, _) = tokio::sync::broadcast::channel::<ApplicationNotification>(16);
        let mutation_store = repository.clone() as Arc<dyn TrackMutationStore>;
        let effect_store = repository.clone() as Arc<dyn TrackEffectJournal>;
        let account_margin_guard = Arc::new(crate::runtime::AccountMarginGuardStore::default());
        let services = build_test_application_services(
            manager,
            mutation_store,
            repository.clone() as Arc<dyn TrackQueryStore>,
            effect_store,
            notifications,
            account_margin_guard,
        );
        let query_store = repository.clone() as Arc<dyn TrackQueryStore>;
        let query_service = Arc::new(TrackQueryService::new(
            query_store.clone(),
            test_track_definition_registry("btc-core"),
            services.observation_service.clone(),
        ));
        let projector = Arc::new(TrackProjector::new());
        let account_monitor = unavailable_account_monitor(services.notifications.clone());
        let account_projector = Arc::new(AccountProjector::new());
        let debug_query_service = Arc::new(TrackDebugQueryService::new(
            query_store.clone(),
            services.observation_service.clone(),
        ));
        let app = router(HttpTestState {
            http_state: build_http_state(
                &services,
                query_store.clone(),
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
        assert_eq!(payload.status.lifecycle.status, TrackStatus::Active);
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

    #[tokio::test]
    async fn get_recent_fills_audit_compares_exchange_and_local_pnl_records() {
        let repository = Arc::new(SqliteStorage::in_memory().unwrap());
        let track_id = TrackId::new("btc-core");
        let instrument = Instrument::new(Venue::Binance, "BTCUSDT");
        let recorded = audit_trade_record(
            instrument.clone(),
            Some("okx:fills:recorded"),
            Some("recorded"),
        );
        TrackMutationStore::insert_track_pnl_record(&*repository, &track_id, &recorded)
            .await
            .unwrap();
        let missing = audit_trade_record(
            instrument.clone(),
            Some("okx:fills:missing"),
            Some("missing"),
        );
        let unkeyed = audit_trade_record(instrument, None, Some("unkeyed"));
        let account = Arc::new(FakeRecentFillsAccount {
            records: vec![recorded, missing, unkeyed],
        });
        let state = build_test_state_with_recent_fills_account(repository, account).await;

        let response = router(state)
            .oneshot(
                Request::builder()
                    .uri("/debug/tracks/btc-core/recent-fills-audit")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let payload: RecentFillsAuditResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(payload.track_id, "btc-core");
        assert_eq!(payload.instrument.symbol, "BTCUSDT");
        assert_eq!(payload.records_seen, 3);
        assert_eq!(payload.records_recorded, 1);
        assert_eq!(payload.records_missing, 1);
        assert_eq!(payload.records_unkeyed, 1);
        assert_eq!(payload.items[0].coverage, RecentFillCoverageView::Recorded);
        assert_eq!(payload.items[1].coverage, RecentFillCoverageView::Missing);
        assert_eq!(payload.items[2].coverage, RecentFillCoverageView::Unkeyed);
        assert_eq!(
            payload.items[1].source_key.as_deref(),
            Some("okx:fills:missing")
        );
    }

    fn audit_trade_record(
        instrument: Instrument,
        source_key: Option<&str>,
        trade_id: Option<&str>,
    ) -> TrackPnlRecord {
        TrackPnlRecord::trade(
            instrument,
            Utc.with_ymd_and_hms(2026, 6, 17, 1, 2, 3).unwrap(),
            "okx:fills".to_string(),
            source_key.map(str::to_string),
            Some("order-1".to_string()),
            trade_id.map(str::to_string),
            Side::Sell,
            62_000.0,
            0.2,
            0.0001,
            0.00001,
            "USDT",
        )
    }

    struct FakeRecentFillsAccount {
        records: Vec<TrackPnlRecord>,
    }

    #[async_trait::async_trait]
    impl AccountPort for FakeRecentFillsAccount {
        async fn get_account_capacity_snapshot(
            &self,
            _instrument: &Instrument,
        ) -> anyhow::Result<AccountCapacitySnapshot> {
            Ok(AccountCapacitySnapshot {
                max_increase_notional: 1_000_000.0,
            })
        }

        async fn get_recent_track_pnl_records(
            &self,
            _instrument: &Instrument,
        ) -> anyhow::Result<Vec<TrackPnlRecord>> {
            Ok(self.records.clone())
        }

        async fn subscribe_user_data(&self) -> anyhow::Result<mpsc::Receiver<UserDataEvent>> {
            let (_sender, receiver) = mpsc::channel(1);
            Ok(receiver)
        }
    }

    #[derive(Default)]
    struct FailingRepository {
        updated_at: std::sync::Mutex<std::collections::HashMap<String, chrono::DateTime<Utc>>>,
    }

    impl FailingRepository {
        fn seed_track(&self, track_id: &TrackId) {
            self.updated_at
                .lock()
                .unwrap()
                .insert(track_id.as_str().to_string(), Utc::now());
        }
    }

    #[async_trait::async_trait]
    impl TrackMutationStore for FailingRepository {
        async fn commit_track_transition(
            &self,
            _id: &str,
            _control_state: Option<&poise_application::TrackControlState>,
            _events: &[poise_core::events::DomainEvent],
        ) -> anyhow::Result<CommittedTrackWrite> {
            Err(anyhow!("persistence unavailable"))
        }

        async fn list_track_events(
            &self,
            _id: &str,
        ) -> anyhow::Result<Vec<poise_core::events::DomainEvent>> {
            Ok(Vec::new())
        }

        async fn save_track_control_state(
            &self,
            _track_id: &TrackId,
            _state: &poise_application::TrackControlState,
        ) -> anyhow::Result<()> {
            Err(anyhow!("persistence unavailable"))
        }

        async fn insert_track_pnl_record(
            &self,
            _track_id: &TrackId,
            _record: &poise_engine::ledger::TrackPnlRecord,
        ) -> anyhow::Result<bool> {
            Err(anyhow!("persistence unavailable"))
        }
    }

    #[async_trait::async_trait]
    impl TrackEffectJournal for FailingRepository {
        async fn append_entries(&self, _entries: &[EffectJournalEntry]) -> anyhow::Result<()> {
            Err(anyhow!("persistence unavailable"))
        }

        async fn record_effect_outcomes(
            &self,
            _outcomes: &[EffectStatusUpdate],
        ) -> anyhow::Result<()> {
            Err(anyhow!("persistence unavailable"))
        }
    }

    #[async_trait::async_trait]
    impl TrackQueryStore for FailingRepository {
        async fn load_track_updated_at(
            &self,
            track_id: &TrackId,
        ) -> anyhow::Result<Option<chrono::DateTime<chrono::Utc>>> {
            Ok(self
                .updated_at
                .lock()
                .unwrap()
                .get(track_id.as_str())
                .copied())
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

        async fn load_track_control_state(
            &self,
            _track_id: &TrackId,
        ) -> anyhow::Result<Option<poise_application::TrackControlState>> {
            Ok(None)
        }

        async fn load_track_pnl_stats(
            &self,
            _track_id: &TrackId,
            pnl_utc_day: chrono::NaiveDate,
        ) -> anyhow::Result<poise_engine::ledger::TrackPnlStats> {
            Ok(poise_engine::ledger::TrackPnlStats {
                pnl_utc_day,
                ..poise_engine::ledger::TrackPnlStats::default()
            })
        }

        async fn list_track_pnl_source_keys(
            &self,
            _track_id: &TrackId,
            _source_keys: &[String],
        ) -> anyhow::Result<Vec<String>> {
            Ok(Vec::new())
        }
    }
}
