use std::{path::Path, sync::Arc};

use anyhow::Result;
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
        CommandAccepted, CommandRequest, CommandType, HttpSuccessEnvelope, PROTOCOL_VERSION,
        PriceUpdated, RuntimeSnapshot, ServerEnvelope, ServerEvent,
    },
    storage::{PersistedRuntime, SqliteStorage},
};

#[derive(Clone)]
pub struct Application {
    engine: EngineHandle,
    read_model: SharedReadModel,
    events_tx: broadcast::Sender<ServerEnvelope>,
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
