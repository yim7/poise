use axum::{Json, extract::State, http::StatusCode};
use chrono::Utc;
use poise_application::exchange::ExchangePorts;
use poise_core::{instrument::PositionBasis, risk::LossGuardSnapshot, types::Side};
use poise_protocol::{
    TrackCreationCandidate, TrackCreationContext, TrackCreationOrderView,
    TrackCreationPreviewRequest, TrackCreationPreviewResponse, TrackCreationSaveRequest,
};
use sha2::{Digest, Sha256};

use crate::config_persistence::{ConfigurationApplicationStoreAdapter, append_track_configuration};
use crate::http::{ErrorResponse, bad_request, internal_error, map_configuration_store_error};
use crate::server_context::HttpState;

type ApiError = (StatusCode, Json<ErrorResponse>);

fn conflict(message: &str) -> ApiError {
    (
        StatusCode::CONFLICT,
        Json(ErrorResponse {
            error: message.into(),
        }),
    )
}

fn store(state: &HttpState) -> Result<&ConfigurationApplicationStoreAdapter, ApiError> {
    state
        .configuration_store
        .as_deref()
        .ok_or_else(|| internal_error("configuration store is unavailable".into()))
}

fn exchange(state: &HttpState) -> Result<&ExchangePorts, ApiError> {
    state
        .track_creation_exchange
        .as_ref()
        .ok_or_else(|| internal_error("exchange preview is unavailable".into()))
}

pub(crate) async fn context(
    State(state): State<HttpState>,
) -> Result<Json<TrackCreationContext>, ApiError> {
    let (raw, version) = store(&state)?
        .creation_snapshot()
        .await
        .map_err(map_configuration_store_error)?;
    let config =
        crate::config::parse_config(&raw).map_err(|error| internal_error(error.to_string()))?;
    let active = state.runtime.committed_track_definitions().await;
    let pending = config
        .tracks
        .iter()
        .filter(|track| {
            !active
                .iter()
                .any(|definition| definition.track_id().as_str() == track.track_id)
        })
        .map(|track| candidate_from_spec(track, config.exchange.venue()))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Json(TrackCreationContext { version, pending }))
}

fn candidate_from_spec(
    track: &crate::config::TrackSpec,
    venue: poise_core::track::Venue,
) -> Result<TrackCreationCandidate, ApiError> {
    let definition = track
        .to_track_definition(venue)
        .map_err(|error| internal_error(error.to_string()))?;
    Ok(TrackCreationCandidate {
        track_id: track.track_id.clone(),
        symbol: track.symbol.clone(),
        leverage: track.leverage.unwrap_or(10),
        tick_timeout_secs: definition.tick_timeout_secs(),
        configuration: crate::config_projection::configuration_view_from_definition(&definition),
    })
}

pub(crate) async fn preview(
    State(state): State<HttpState>,
    Json(request): Json<TrackCreationPreviewRequest>,
) -> Result<Json<TrackCreationPreviewResponse>, ApiError> {
    reject_active_identity(&state, &request.candidate).await?;
    Ok(Json(
        preview_candidate(store(&state)?, exchange(&state)?, &request).await?,
    ))
}

pub(crate) async fn save(
    State(state): State<HttpState>,
    Json(request): Json<TrackCreationSaveRequest>,
) -> Result<Json<TrackCreationContext>, ApiError> {
    reject_active_identity(&state, &request.candidate).await?;
    save_candidate(store(&state)?, exchange(&state)?, &request).await?;
    context(State(state)).await
}

async fn reject_active_identity(
    state: &HttpState,
    candidate: &TrackCreationCandidate,
) -> Result<(), ApiError> {
    if state
        .runtime
        .committed_track_definitions()
        .await
        .iter()
        .any(|definition| {
            definition.track_id().as_str() == candidate.track_id
                || definition.instrument().symbol == candidate.symbol
        })
    {
        return Err(conflict("该 Track ID 或合约已经在运行，请编辑已有 Track"));
    }
    Ok(())
}

async fn save_candidate(
    store: &ConfigurationApplicationStoreAdapter,
    ports: &ExchangePorts,
    request: &TrackCreationSaveRequest,
) -> Result<(), ApiError> {
    if request.preview_token.is_empty() {
        return Err(bad_request("请先预览接管影响".into()));
    }
    let (raw, _) = store
        .creation_snapshot()
        .await
        .map_err(map_configuration_store_error)?;
    let config =
        crate::config::parse_config(&raw).map_err(|error| internal_error(error.to_string()))?;
    if let Some(existing) = config
        .tracks
        .iter()
        .find(|track| track.track_id == request.candidate.track_id)
    {
        // Idempotent recovery after an HTTP response is lost. No runtime is activated here.
        if candidate_from_spec(existing, config.exchange.venue())? == request.candidate {
            return Ok(());
        }
        return Err(conflict("Track ID 已存在，请重新载入配置"));
    }
    let fresh = preview_candidate(
        store,
        ports,
        &TrackCreationPreviewRequest {
            expected_version: request.expected_version.clone(),
            candidate: request.candidate.clone(),
        },
    )
    .await?;
    if fresh.preview_token != request.preview_token {
        return Err(conflict("仓位、挂单或接管目标已变化，请重新预览"));
    }
    store
        .append_track(&request.expected_version, &request.candidate)
        .await
        .map_err(map_configuration_store_error)
}

async fn preview_candidate(
    store: &ConfigurationApplicationStoreAdapter,
    ports: &ExchangePorts,
    request: &TrackCreationPreviewRequest,
) -> Result<TrackCreationPreviewResponse, ApiError> {
    let (raw, version) = store
        .creation_snapshot()
        .await
        .map_err(map_configuration_store_error)?;
    if store
        .active_application()
        .map_err(map_configuration_store_error)?
        .is_some()
    {
        return Err(conflict("已有配置正在应用，请等待完成后新增 Track"));
    }
    if version != request.expected_version {
        return Err(conflict("配置已变化，请重新载入并预览"));
    }
    let patched = append_track_configuration(&raw, &request.candidate)
        .map_err(|error| bad_request(error.to_string()))?;
    let config =
        crate::config::parse_config(&patched).map_err(|error| bad_request(error.to_string()))?;
    let definition = config
        .track_definitions()
        .map_err(|error| bad_request(error.to_string()))?
        .pop()
        .ok_or_else(|| internal_error("missing candidate definition".into()))?;
    let instrument = definition.instrument();
    let execution = ports.execution();
    let metadata = ports.metadata();
    let market = ports.market_data();
    let account = ports.account_summary();
    let facts = tokio::time::timeout(std::time::Duration::from_secs(15), async {
        tokio::try_join!(
            metadata.get_exchange_info(instrument),
            market.get_mark_price(instrument),
            async {
                execution
                    .get_position(instrument)
                    .await
                    .map_err(anyhow::Error::new)
            },
            async {
                execution
                    .get_open_orders(instrument)
                    .await
                    .map_err(anyhow::Error::new)
            },
            account.get_account_summary(),
        )
    })
    .await
    .map_err(|_| {
        (
            StatusCode::BAD_GATEWAY,
            Json(ErrorResponse {
                error: "读取交易所快照超时，请重试预览".into(),
            }),
        )
    })?
    .map_err(|error| {
        (
            StatusCode::BAD_GATEWAY,
            Json(ErrorResponse {
                error: format!("无法核验交易所快照：{error}"),
            }),
        )
    })?;
    let (info, mark_price, position, orders, account) = facts;
    let spec = info.instrument_spec;
    if spec.instrument() != instrument
        || position.instrument != *instrument
        || orders
            .orders()
            .iter()
            .any(|order| order.instrument != *instrument)
    {
        return Err(bad_request("交易所返回的合约与请求不一致".into()));
    }
    if !position.unrealized_pnl.is_finite() {
        return Err(bad_request("未实现盈亏不可用".into()));
    }
    crate::config_explain::explain_track_config(&definition, &spec)
        .map_err(|error| bad_request(error.to_string()))?;
    let capacity = crate::account_capacity::estimate_account_capacity(
        &account,
        &spec,
        Some(mark_price),
        request.candidate.leverage,
    )
    .map_err(|error| bad_request(error.to_string()))?;
    let startup_required = poise_engine::runtime::required_additional_notional(
        definition.track_config(),
        &spec,
        definition.max_notional(),
        position.qty,
    );
    let now = Utc::now();
    let impact = poise_engine::configuration_preview::preview_configuration(
        poise_engine::configuration_preview::ConfigurationPreviewInput {
            active: definition.clone(),
            active_policy_target: None,
            candidate: definition.clone(),
            instrument_spec: spec.clone(),
            current_position_qty: position.qty,
            loss_guard: LossGuardSnapshot {
                net_realized_pnl_today: 0.0,
                net_realized_pnl_cumulative: 0.0,
                unrealized_pnl: position.unrealized_pnl,
            },
            strategy_price: mark_price,
            observed_at: now,
            max_increase_notional: capacity.max_increase_notional,
        },
    )
    .map_err(|error| bad_request(format!("无法计算接管影响：{error:?}")))?;
    if !impact.blockers.is_empty() {
        return Err(bad_request(format!(
            "接管配置未通过交易所最小下单量或账户容量检查：{:?}",
            impact.blockers
        )));
    }
    let required = startup_required.max(impact.required_increase_notional);
    if required > capacity.max_increase_notional {
        return Err(bad_request(format!(
            "账户容量不足：所需增加名义金额 {required}，可用 {}",
            capacity.max_increase_notional
        )));
    }
    let mut orders_to_cancel_on_restart = orders
        .orders()
        .iter()
        .map(|order| TrackCreationOrderView {
            order_id: order.order_id.clone(),
            side: match order.side {
                Side::Buy => "buy",
                Side::Sell => "sell",
            }
            .into(),
            price: order.price,
            quantity: order.qty.value(),
        })
        .collect::<Vec<_>>();
    orders_to_cancel_on_restart.sort_by(|left, right| left.order_id.cmp(&right.order_id));
    let unit = impact.candidate_position_qty_per_unit;
    let initial =
        poise_core::instrument::PositionQty::new(impact.initial_execution_target.0 * unit)
            .map_err(bad_request)?;
    let initial_execution_quantity = spec.normalize_position_qty(initial).value();
    let policy_target_quantity = spec
        .normalize_position_qty(impact.candidate_policy_target_position_qty)
        .value();
    // Bind the confirmation to this draft, file, inherited inventory and normalized
    // target. Always refresh venue facts before saving; timestamps are not identity.
    let proof = serde_json::to_vec(&(
        request,
        position.qty.value(),
        initial_execution_quantity,
        policy_target_quantity,
        &orders_to_cancel_on_restart,
    ))
    .map_err(|error| internal_error(error.to_string()))?;
    Ok(TrackCreationPreviewResponse {
        preview_token: format!("{:x}", Sha256::digest(proof)),
        previewed_at: now.to_rfc3339(),
        mark_price,
        position_quantity: position.qty.value(),
        quantity_unit: match spec.position_basis() {
            PositionBasis::BaseAsset => spec.base_asset().as_str(),
            PositionBasis::Contracts { .. } => "合约",
        }
        .into(),
        notional_asset: spec.notional_asset().as_str().into(),
        settlement_asset: spec.settlement_asset().as_str().into(),
        policy_target_quantity,
        initial_execution_quantity,
        available_increase_notional: capacity.max_increase_notional,
        required_increase_notional: required,
        orders_to_cancel_on_restart,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::config_persistence::ConfigurationApplicationStoreAdapter;
    use anyhow::Result;
    use async_trait::async_trait;
    use chrono::Utc;
    use poise_application::exchange::*;
    use poise_core::{instrument::PositionQty, track::Instrument, types::Side};
    use poise_protocol::{
        TrackCreationCandidate, TrackCreationPreviewRequest, TrackCreationSaveRequest,
    };
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    };

    const CONFIG: &str = "[exchange]\nvenue='binance'\napi_key='secret'\napi_secret='secret'\n[[tracks]]\ntrack_id='btc'\nsymbol='BTCUSDT'\nlower_price=90.0\nupper_price=110.0\nlong_exposure_units=2.0\nshort_exposure_units=0.0\nnotional_per_unit=100.0\ndaily_loss_limit=100.0\ntotal_loss_limit=300.0\n";

    pub(crate) struct ReadOnlyExchange {
        qty: AtomicU64,
        orders: Mutex<Vec<ExchangeOrder>>,
        fail: AtomicBool,
        capacity: AtomicU64,
    }
    #[async_trait]
    impl ExecutionPort for ReadOnlyExchange {
        async fn submit_order(&self, _: OrderRequest) -> ExecutionResult<OrderReceipt> {
            panic!("creation must never submit")
        }
        async fn cancel_order(&self, _: &Instrument, _: &str) -> ExecutionResult<OrderReceipt> {
            panic!("creation must never cancel")
        }
        async fn cancel_all(&self, _: &Instrument) -> ExecutionResult<()> {
            panic!("creation must never cancel")
        }
        async fn get_position(&self, instrument: &Instrument) -> ExecutionResult<Position> {
            if self.fail.load(Ordering::SeqCst) {
                return Err(ExecutionPortError::failed("position unavailable"));
            }
            Ok(Position {
                instrument: instrument.clone(),
                qty: PositionQty::new(f64::from_bits(self.qty.load(Ordering::SeqCst))).unwrap(),
                avg_price: 100.0,
                mark_price: Some(100.0),
                unrealized_pnl: 0.0,
            })
        }
        async fn get_open_orders(
            &self,
            _: &Instrument,
        ) -> ExecutionResult<ExchangeOpenOrderSnapshot> {
            Ok(ExchangeOpenOrderSnapshot::from_complete_exchange_query(
                self.orders.lock().unwrap().clone(),
            ))
        }
    }
    #[async_trait]
    impl MetadataPort for ReadOnlyExchange {
        async fn get_exchange_info(&self, instrument: &Instrument) -> Result<ExchangeInfo> {
            Ok(ExchangeInfo {
                instrument_spec: crate::test_support::test_instrument_spec_with(
                    instrument.clone(),
                    0.1,
                    0.01,
                    0.01,
                    5.0,
                ),
            })
        }
        async fn get_server_time(&self) -> Result<chrono::DateTime<Utc>> {
            Ok(Utc::now())
        }
    }
    #[async_trait]
    impl MarketDataPort for ReadOnlyExchange {
        async fn subscribe_prices(
            &self,
            _: &Instrument,
        ) -> Result<tokio::sync::mpsc::Receiver<MarketDataTick>> {
            panic!("creation must never subscribe")
        }
        async fn get_mark_price(&self, _: &Instrument) -> Result<f64> {
            Ok(100.0)
        }
    }
    #[async_trait]
    impl AccountSummaryPort for ReadOnlyExchange {
        async fn get_account_summary(&self) -> Result<AccountSummarySnapshot> {
            Ok(crate::test_support::test_account_summary_snapshot(
                "USDT",
                2000.0,
                Some(f64::from_bits(self.capacity.load(Ordering::SeqCst))),
                &[],
                None,
                Utc::now(),
            ))
        }
    }

    pub(crate) fn fixture() -> (
        tempfile::TempDir,
        Arc<ConfigurationApplicationStoreAdapter>,
        ExchangePorts,
        TrackCreationCandidate,
        Arc<ReadOnlyExchange>,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        std::fs::write(&path, CONFIG).unwrap();
        let store = Arc::new(
            ConfigurationApplicationStoreAdapter::new(
                path,
                Arc::new(poise_storage::sqlite::SqliteStorage::in_memory().unwrap()),
            )
            .unwrap(),
        );
        let definition = crate::config::parse_config(CONFIG)
            .unwrap()
            .track_definitions()
            .unwrap()
            .remove(0);
        let candidate = TrackCreationCandidate {
            track_id: "gold".into(),
            symbol: "GOLDUSDT".into(),
            leverage: 2,
            tick_timeout_secs: 30,
            configuration: crate::config_projection::configuration_view_from_definition(
                &definition,
            ),
        };
        let exchange = Arc::new(ReadOnlyExchange {
            qty: AtomicU64::new(3.0f64.to_bits()),
            orders: Mutex::new(vec![ExchangeOrder {
                instrument: Instrument::new(poise_core::track::Venue::Binance, "GOLDUSDT"),
                order_id: "gold-order".into(),
                client_order_id: "manual".into(),
                side: Side::Sell,
                price: 110.0,
                qty: PositionQty::new(0.5).unwrap(),
                filled_qty: PositionQty::default(),
                status: OrderStatus::New,
            }]),
            fail: AtomicBool::new(false),
            capacity: AtomicU64::new(1000.0f64.to_bits()),
        });
        let ports = ExchangePorts::new(
            exchange.clone(),
            exchange.clone(),
            exchange.clone(),
            Arc::new(crate::test_support::NoopAccountPort),
            exchange.clone(),
        );
        (directory, store, ports, candidate, exchange)
    }

    #[tokio::test]
    async fn creation_preview_reads_inherited_position_orders_and_fresh_session_reduction() {
        let (_directory, store, ports, candidate, _) = fixture();
        let (_, expected_version) = store.creation_snapshot().await.unwrap();
        let preview = super::preview_candidate(
            &store,
            &ports,
            &TrackCreationPreviewRequest {
                expected_version,
                candidate,
            },
        )
        .await
        .unwrap();
        assert_eq!(preview.position_quantity, 3.0);
        assert_eq!(preview.policy_target_quantity, 1.0);
        assert_eq!(preview.initial_execution_quantity, 1.0);
        assert_eq!(
            preview.orders_to_cancel_on_restart[0].order_id,
            "gold-order"
        );
        assert_eq!(preview.quantity_unit, "GOLD");
    }

    #[tokio::test]
    async fn creation_save_revalidates_position_orders_and_writes_only_after_accepted_preview() {
        let (directory, store, ports, candidate, exchange) = fixture();
        let (_, expected_version) = store.creation_snapshot().await.unwrap();
        let preview = super::preview_candidate(
            &store,
            &ports,
            &TrackCreationPreviewRequest {
                expected_version: expected_version.clone(),
                candidate: candidate.clone(),
            },
        )
        .await
        .unwrap();
        let request = TrackCreationSaveRequest {
            expected_version,
            candidate,
            preview_token: preview.preview_token,
        };
        exchange.qty.store(2.0f64.to_bits(), Ordering::SeqCst);
        let error = super::save_candidate(&store, &ports, &request)
            .await
            .unwrap_err();
        assert_eq!(error.0, axum::http::StatusCode::CONFLICT);
        exchange.qty.store(3.0f64.to_bits(), Ordering::SeqCst);
        exchange.orders.lock().unwrap().clear();
        assert_eq!(
            super::save_candidate(&store, &ports, &request)
                .await
                .unwrap_err()
                .0,
            axum::http::StatusCode::CONFLICT
        );
        let preview = super::preview_candidate(
            &store,
            &ports,
            &TrackCreationPreviewRequest {
                expected_version: request.expected_version.clone(),
                candidate: request.candidate.clone(),
            },
        )
        .await
        .unwrap();
        let request = TrackCreationSaveRequest {
            preview_token: preview.preview_token,
            ..request
        };
        super::save_candidate(&store, &ports, &request)
            .await
            .unwrap();
        let saved = crate::config::load_config(directory.path().join("config.toml")).unwrap();
        assert_eq!(saved.tracks.len(), 2);
        assert_eq!(saved.tracks[1].track_id, "gold");
        // A lost HTTP response can be retried without appending twice.
        super::save_candidate(&store, &ports, &request)
            .await
            .unwrap();
        assert_eq!(
            crate::config::load_config(directory.path().join("config.toml"))
                .unwrap()
                .tracks
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn creation_unavailable_facts_and_insufficient_capacity_cannot_be_saved() {
        let (_directory, store, ports, mut candidate, exchange) = fixture();
        let (_, expected_version) = store.creation_snapshot().await.unwrap();
        exchange.fail.store(true, Ordering::SeqCst);
        assert!(
            super::preview_candidate(
                &store,
                &ports,
                &TrackCreationPreviewRequest {
                    expected_version: expected_version.clone(),
                    candidate: candidate.clone()
                }
            )
            .await
            .is_err()
        );
        exchange.fail.store(false, Ordering::SeqCst);
        candidate.configuration.max_notional = 50_000.0;
        assert_eq!(
            super::preview_candidate(
                &store,
                &ports,
                &TrackCreationPreviewRequest {
                    expected_version,
                    candidate
                }
            )
            .await
            .unwrap_err()
            .0,
            axum::http::StatusCode::BAD_REQUEST
        );
    }
}
