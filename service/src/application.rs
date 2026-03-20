use std::{path::Path, sync::Arc};

use anyhow::Result;
use serde::Serialize;
use tokio::sync::broadcast;
use tracing::warn;
use uuid::Uuid;

use crate::{
    background::spawn_task,
    integrations::binance::{
        BinanceConfig, BinanceTransport, prepare_bootstrap_runtime, spawn_supervisor,
    },
    kernel::{
        EngineEvent, EngineHandle, ReadModel, SharedReadModel, now_utc, spawn_engine,
        spawn_engine_with_runtime,
    },
    protocol::{
        AlertRecord, AlertsFilters, AlertsQueryResult, AuthDescriptor, CommandAccepted,
        CommandRequest, CommandType, CommandsFilters, CommandsQueryResult,
        ControlPlaneCapabilities, DEFAULT_INSTANCE_ID, DeploymentDescriptor, EndpointGroup,
        FillsFilters, FillsQueryResult, HttpAuthDescriptor, HttpSuccessEnvelope, OrdersFilters,
        OrdersQueryResult, PROTOCOL_VERSION, Pagination, PriceUpdated, QueryCollection,
        RuntimeQueryResult, RuntimeSnapshot, ServerEnvelope, ServerEvent, WebSocketAuthDescriptor,
        WebSocketDescriptor,
    },
    storage::{PersistedRuntime, SqliteStorage},
};

#[derive(Clone)]
pub struct Application {
    engine: EngineHandle,
    read_model: SharedReadModel,
    events_tx: broadcast::Sender<ServerEnvelope>,
    instance_id: String,
}

pub struct RuntimeStreamSubscription {
    pub initial_snapshot: ServerEnvelope,
    pub receiver: broadcast::Receiver<ServerEnvelope>,
    pub snapshot_sequence: u64,
}

impl Application {
    pub fn bootstrap() -> Self {
        Self::build_from_engine(spawn_engine())
    }

    pub fn bootstrap_with_binance(
        config: BinanceConfig,
        transport: Arc<dyn BinanceTransport>,
    ) -> Self {
        let runtime = prepare_bootstrap_runtime(PersistedRuntime::in_memory_bootstrap(), &config);
        let application = Self::build_from_engine(spawn_engine_with_runtime(runtime, None));
        application.start_binance_supervisor(config, transport);
        application
    }

    pub fn bootstrap_with_runtime_and_binance(
        runtime: PersistedRuntime,
        config: BinanceConfig,
        transport: Arc<dyn BinanceTransport>,
    ) -> Self {
        let runtime = prepare_bootstrap_runtime(runtime, &config);
        let application = Self::build_from_engine(spawn_engine_with_runtime(runtime, None));
        application.start_binance_supervisor(config, transport);
        application
    }

    pub fn bootstrap_with_sqlite(path: impl AsRef<Path>) -> Result<Self> {
        let storage = SqliteStorage::open(path)?;
        let runtime = match storage.load_runtime()? {
            Some(runtime) => runtime,
            None => {
                let runtime = PersistedRuntime::sqlite_bootstrap();
                storage.persist_runtime(&runtime)?;
                runtime
            }
        };
        Ok(Self::build_from_engine(spawn_engine_with_runtime(
            runtime,
            Some(storage),
        )))
    }

    pub fn bootstrap_with_sqlite_and_binance(
        path: impl AsRef<Path>,
        config: BinanceConfig,
        transport: Arc<dyn BinanceTransport>,
    ) -> Result<Self> {
        let storage = SqliteStorage::open(path)?;
        let runtime = match storage.load_runtime()? {
            Some(runtime) => runtime,
            None => PersistedRuntime::sqlite_bootstrap(),
        };
        let runtime = prepare_bootstrap_runtime(runtime, &config);
        storage.persist_runtime(&runtime)?;
        let application =
            Self::build_from_engine(spawn_engine_with_runtime(runtime, Some(storage)));
        application.start_binance_supervisor(config, transport);
        Ok(application)
    }

    fn build_from_engine(
        (engine, read_model, mut engine_events_rx): (
            EngineHandle,
            SharedReadModel,
            tokio::sync::mpsc::Receiver<crate::kernel::SequencedEngineEvent>,
        ),
    ) -> Self {
        let (events_tx, _) = broadcast::channel(256);
        let publish_tx = events_tx.clone();

        spawn_task(async move {
            while let Some(event) = engine_events_rx.recv().await {
                let envelope = wrap_event(event.event.into(), Some(event.sequence));
                if publish_tx.send(envelope).is_err() {
                    warn!("no websocket subscribers for published engine event");
                }
            }
        });

        Self {
            engine,
            read_model,
            events_tx,
            instance_id: default_instance_id(),
        }
    }

    pub fn snapshot(&self) -> RuntimeSnapshot {
        self.read_model().snapshot()
    }

    pub fn open_orders(&self) -> Vec<crate::protocol::OpenOrder> {
        self.read_model().open_orders()
    }

    pub fn recent_fills(&self) -> Vec<crate::protocol::RecentFill> {
        self.read_model().recent_fills()
    }

    pub fn risk_events(&self) -> Vec<crate::protocol::RiskEvent> {
        self.read_model().risk_events()
    }

    pub fn system_events(&self) -> Vec<crate::protocol::SystemEvent> {
        self.read_model().system_events()
    }

    pub fn query_runtime(&self) -> RuntimeQueryResult {
        RuntimeQueryResult {
            instance_id: self.instance_id.clone(),
            snapshot: self.snapshot(),
        }
    }

    pub fn query_orders(
        &self,
        page: usize,
        per_page: usize,
        filters: OrdersFilters,
    ) -> OrdersQueryResult {
        let mut items = self.open_orders();
        if let Some(side) = filters.side.as_deref() {
            items.retain(|order| order.side.eq_ignore_ascii_case(side));
        }
        if let Some(status) = filters.status.as_deref() {
            items.retain(|order| order.status.eq_ignore_ascii_case(status));
        }
        items.sort_by(|left, right| {
            right
                .updated_at
                .cmp(&left.updated_at)
                .then_with(|| right.order_id.cmp(&left.order_id))
        });
        query_collection(
            &self.instance_id,
            items,
            page,
            per_page,
            filters,
            "updated_at_desc",
        )
    }

    pub fn query_fills(
        &self,
        page: usize,
        per_page: usize,
        filters: FillsFilters,
    ) -> FillsQueryResult {
        let mut items = self.recent_fills();
        if let Some(side) = filters.side.as_deref() {
            items.retain(|fill| fill.side.eq_ignore_ascii_case(side));
        }
        if let Some(order_id) = filters.order_id.as_deref() {
            items.retain(|fill| fill.order_id == order_id);
        }
        if let Some(client_order_id) = filters.client_order_id.as_deref() {
            items.retain(|fill| fill.client_order_id.as_deref() == Some(client_order_id));
        }
        items.sort_by(|left, right| {
            right
                .event_time
                .cmp(&left.event_time)
                .then_with(|| right.trade_id.cmp(&left.trade_id))
        });
        query_collection(
            &self.instance_id,
            items,
            page,
            per_page,
            filters,
            "event_time_desc",
        )
    }

    pub fn query_alerts(
        &self,
        page: usize,
        per_page: usize,
        filters: AlertsFilters,
        sort: &str,
    ) -> AlertsQueryResult {
        let mut items = self
            .risk_events()
            .into_iter()
            .map(|event| AlertRecord {
                category: "risk".into(),
                severity: enum_text(&event.severity),
                source: "risk".into(),
                code: Some(event.code),
                message: event.message,
                created_at: event.created_at,
                acknowledged_at: event.acknowledged_at,
            })
            .chain(self.system_events().into_iter().map(|event| AlertRecord {
                category: "system".into(),
                severity: event.level.clone(),
                source: event.source,
                code: None,
                message: event.message,
                created_at: event.created_at,
                acknowledged_at: None,
            }))
            .collect::<Vec<_>>();

        if let Some(category) = filters.category.as_deref() {
            items.retain(|alert| alert.category.eq_ignore_ascii_case(category));
        }
        if let Some(severity) = filters.severity.as_deref() {
            items.retain(|alert| alert.severity.eq_ignore_ascii_case(severity));
        }
        if let Some(source) = filters.source.as_deref() {
            items.retain(|alert| alert.source.eq_ignore_ascii_case(source));
        }
        if let Some(acknowledged) = filters.acknowledged {
            items.retain(|alert| {
                alert.category == "risk" && (alert.acknowledged_at.is_some() == acknowledged)
            });
        }

        match sort {
            "created_at_asc" => items.sort_by(|left, right| {
                left.created_at
                    .cmp(&right.created_at)
                    .then_with(|| left.message.cmp(&right.message))
            }),
            _ => items.sort_by(|left, right| {
                right
                    .created_at
                    .cmp(&left.created_at)
                    .then_with(|| right.message.cmp(&left.message))
            }),
        }

        query_collection(
            &self.instance_id,
            items,
            page,
            per_page,
            filters,
            normalize_alerts_sort(sort),
        )
    }

    pub fn query_commands(
        &self,
        page: usize,
        per_page: usize,
        filters: CommandsFilters,
        sort: &str,
    ) -> CommandsQueryResult {
        let mut items = self.snapshot().execution.recent_commands;
        if let Some(command) = filters.command.as_deref() {
            items.retain(|record| enum_text(&record.command).eq_ignore_ascii_case(command));
        }
        if let Some(status) = filters.status.as_deref() {
            items.retain(|record| enum_text(&record.status).eq_ignore_ascii_case(status));
        }

        match sort {
            "requested_at_asc" => items.sort_by(|left, right| {
                left.requested_at
                    .cmp(&right.requested_at)
                    .then_with(|| left.command_id.cmp(&right.command_id))
            }),
            "finished_at_desc" => items.sort_by(|left, right| {
                command_finished_key(right)
                    .cmp(command_finished_key(left))
                    .then_with(|| right.command_id.cmp(&left.command_id))
            }),
            "finished_at_asc" => items.sort_by(|left, right| {
                command_finished_key(left)
                    .cmp(command_finished_key(right))
                    .then_with(|| left.command_id.cmp(&right.command_id))
            }),
            _ => items.sort_by(|left, right| {
                right
                    .requested_at
                    .cmp(&left.requested_at)
                    .then_with(|| right.command_id.cmp(&left.command_id))
            }),
        }

        query_collection(
            &self.instance_id,
            items,
            page,
            per_page,
            filters,
            normalize_commands_sort(sort),
        )
    }

    pub fn control_plane_capabilities(&self) -> ControlPlaneCapabilities {
        ControlPlaneCapabilities {
            instance_id: self.instance_id.clone(),
            deployment: DeploymentDescriptor {
                mode: "lan".into(),
                scope: "single_instance".into(),
            },
            auth: AuthDescriptor {
                mode: "optional_static_token".into(),
                http: HttpAuthDescriptor {
                    header: "authorization".into(),
                    query_param: "access_token".into(),
                },
            },
            endpoint_groups: vec![
                EndpointGroup {
                    name: "runtime".into(),
                    paths: vec!["/runtime/snapshot".into(), "/query/runtime".into()],
                },
                EndpointGroup {
                    name: "orders".into(),
                    paths: vec!["/orders/open".into(), "/query/orders".into()],
                },
                EndpointGroup {
                    name: "fills".into(),
                    paths: vec!["/fills/recent".into(), "/query/fills".into()],
                },
                EndpointGroup {
                    name: "alerts".into(),
                    paths: vec![
                        "/risk/events".into(),
                        "/system/events".into(),
                        "/query/alerts".into(),
                    ],
                },
                EndpointGroup {
                    name: "commands".into(),
                    paths: vec![
                        "/commands/pause".into(),
                        "/commands/resume".into(),
                        "/commands/cancel-all".into(),
                        "/commands/flatten-now".into(),
                        "/commands/shutdown-after-flatten".into(),
                        "/query/commands".into(),
                    ],
                },
            ],
            websocket: WebSocketDescriptor {
                path: "/ws".into(),
                subscriptions: vec!["runtime_stream".into()],
                auth: WebSocketAuthDescriptor {
                    query_param: "access_token".into(),
                    first_message: false,
                },
            },
            minimal_web_capabilities: vec![
                "runtime_snapshot".into(),
                "order_list".into(),
                "fill_list".into(),
                "alerts_list".into(),
                "command_timeline".into(),
                "control_commands".into(),
            ],
        }
    }

    pub async fn submit_command(
        &self,
        command: CommandType,
        request: CommandRequest,
    ) -> Result<CommandAccepted> {
        self.engine.submit_command(command, request).await
    }

    pub async fn emit_price_tick(&self) -> Result<PriceUpdated> {
        self.engine.emit_price_tick().await
    }

    pub fn subscribe_events(&self) -> broadcast::Receiver<ServerEnvelope> {
        self.events_tx.subscribe()
    }

    pub fn runtime_snapshot_event(&self) -> ServerEnvelope {
        let (snapshot, sequence) = self.snapshot_with_sequence();
        wrap_event(ServerEvent::RuntimeSnapshot(snapshot), Some(sequence))
    }

    pub fn subscribe_runtime_stream(&self) -> RuntimeStreamSubscription {
        let receiver = self.subscribe_events();
        let (snapshot, snapshot_sequence) = self.snapshot_with_sequence();
        RuntimeStreamSubscription {
            initial_snapshot: wrap_event(
                ServerEvent::RuntimeSnapshot(snapshot),
                Some(snapshot_sequence),
            ),
            receiver,
            snapshot_sequence,
        }
    }
}

fn wrap_event(event: ServerEvent, sequence: Option<u64>) -> ServerEnvelope {
    ServerEnvelope {
        version: PROTOCOL_VERSION.into(),
        event_id: format!("evt_{}", Uuid::new_v4().simple()),
        emitted_at: now_utc(),
        sequence,
        event,
    }
}

fn ok<T>(data: T) -> HttpSuccessEnvelope<T> {
    HttpSuccessEnvelope {
        version: PROTOCOL_VERSION.into(),
        status: "ok".into(),
        data,
    }
}

pub fn success<T>(data: T) -> HttpSuccessEnvelope<T> {
    ok(data)
}

impl From<EngineEvent> for ServerEvent {
    fn from(value: EngineEvent) -> Self {
        match value {
            EngineEvent::CommandAck(ack) => Self::CommandAck(ack),
            EngineEvent::PriceUpdated(price) => Self::PriceUpdated(price),
            EngineEvent::RiskAlert(alert) => Self::RiskAlert(alert),
            EngineEvent::RuntimeSnapshot(snapshot) => Self::RuntimeSnapshot(snapshot),
            EngineEvent::ConnectionChanged(connection) => Self::ConnectionChanged(connection),
        }
    }
}

impl Application {
    fn start_binance_supervisor(
        &self,
        config: BinanceConfig,
        transport: Arc<dyn BinanceTransport>,
    ) {
        spawn_supervisor(
            self.engine.clone(),
            self.snapshot().connection,
            config,
            transport,
        );
    }

    fn snapshot_with_sequence(&self) -> (RuntimeSnapshot, u64) {
        let read_model = self.read_model();
        (read_model.snapshot(), read_model.last_sequence())
    }

    fn read_model(&self) -> std::sync::RwLockReadGuard<'_, ReadModel> {
        self.read_model
            .read()
            .expect("service read model rwlock poisoned")
    }
}

fn default_instance_id() -> String {
    std::env::var("GRID_PLATFORM_INSTANCE_ID")
        .ok()
        .and_then(|value| {
            let trimmed = value.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        })
        .unwrap_or_else(|| DEFAULT_INSTANCE_ID.into())
}

fn query_collection<T: Clone, F>(
    instance_id: &str,
    items: Vec<T>,
    page: usize,
    per_page: usize,
    filters: F,
    sort: &str,
) -> QueryCollection<T, F> {
    let page = page.max(1);
    let per_page = per_page.clamp(1, 100);
    let total_items = items.len();
    let pagination = Pagination::new(page, per_page, total_items);
    let start = per_page.saturating_mul(page.saturating_sub(1));
    let items = if start >= total_items {
        Vec::new()
    } else {
        items.into_iter().skip(start).take(per_page).collect()
    };
    QueryCollection {
        instance_id: instance_id.into(),
        items,
        pagination,
        filters,
        sort: sort.into(),
    }
}

fn enum_text<T: Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .unwrap_or_default()
}

fn command_finished_key(record: &crate::protocol::CommandRecord) -> &str {
    record
        .finished_at
        .as_deref()
        .or(record.accepted_at.as_deref())
        .unwrap_or(record.requested_at.as_str())
}

fn normalize_alerts_sort(sort: &str) -> &str {
    match sort {
        "created_at_asc" => "created_at_asc",
        _ => "created_at_desc",
    }
}

fn normalize_commands_sort(sort: &str) -> &str {
    match sort {
        "requested_at_asc" => "requested_at_asc",
        "finished_at_desc" => "finished_at_desc",
        "finished_at_asc" => "finished_at_asc",
        _ => "requested_at_desc",
    }
}
