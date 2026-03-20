use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use grid_platform_service::{
    execution::{
        CancelOrdersRequest, ExecutionAdapter, FakeExecutionAdapter, SubmitOrderRequest,
        SubmitOrderResult,
    },
    kernel::{
        EngineEvent, spawn_engine, spawn_engine_with_adapter, spawn_engine_with_runtime_and_adapter,
    },
    protocol::{
        CommandRequest, CommandStatus, CommandType, OpenOrder, RecentFill, RiskLevel,
        RuntimeSnapshot, StrategyStatus,
    },
    storage::PersistedRuntime,
};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio::time::{Instant, timeout};

struct BlockingExecutionAdapter {
    ready: Arc<Notify>,
}

#[async_trait]
impl ExecutionAdapter for BlockingExecutionAdapter {
    async fn submit_order(
        &self,
        request: SubmitOrderRequest,
        snapshot: &RuntimeSnapshot,
    ) -> anyhow::Result<SubmitOrderResult> {
        self.ready.notified().await;
        Ok(FakeExecutionAdapter.submit_order(request, snapshot).await?)
    }

    async fn cancel_orders(
        &self,
        request: CancelOrdersRequest,
        snapshot: &RuntimeSnapshot,
    ) -> anyhow::Result<Vec<OpenOrder>> {
        self.ready.notified().await;
        Ok(FakeExecutionAdapter
            .cancel_orders(request, snapshot)
            .await?)
    }

    async fn query_open_orders(
        &self,
        snapshot: &RuntimeSnapshot,
    ) -> anyhow::Result<Vec<OpenOrder>> {
        Ok(FakeExecutionAdapter.query_open_orders(snapshot).await?)
    }

    async fn list_recent_fills(
        &self,
        snapshot: &RuntimeSnapshot,
    ) -> anyhow::Result<Vec<RecentFill>> {
        Ok(FakeExecutionAdapter.list_recent_fills(snapshot).await?)
    }
}

#[derive(Default)]
struct FlakyCancelExecutionAdapter {
    failures_remaining: AtomicUsize,
    cancel_attempts: AtomicUsize,
}

impl FlakyCancelExecutionAdapter {
    fn with_failures(count: usize) -> Self {
        Self {
            failures_remaining: AtomicUsize::new(count),
            cancel_attempts: AtomicUsize::new(0),
        }
    }

    fn cancel_attempts(&self) -> usize {
        self.cancel_attempts.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl ExecutionAdapter for FlakyCancelExecutionAdapter {
    async fn submit_order(
        &self,
        request: SubmitOrderRequest,
        snapshot: &RuntimeSnapshot,
    ) -> anyhow::Result<SubmitOrderResult> {
        Ok(FakeExecutionAdapter.submit_order(request, snapshot).await?)
    }

    async fn cancel_orders(
        &self,
        request: CancelOrdersRequest,
        snapshot: &RuntimeSnapshot,
    ) -> anyhow::Result<Vec<OpenOrder>> {
        self.cancel_attempts.fetch_add(1, Ordering::SeqCst);
        let remaining = self.failures_remaining.load(Ordering::SeqCst);
        if remaining > 0 {
            self.failures_remaining.fetch_sub(1, Ordering::SeqCst);
            anyhow::bail!("transient cancel-all failure");
        }
        Ok(FakeExecutionAdapter
            .cancel_orders(request, snapshot)
            .await?)
    }

    async fn query_open_orders(
        &self,
        snapshot: &RuntimeSnapshot,
    ) -> anyhow::Result<Vec<OpenOrder>> {
        Ok(FakeExecutionAdapter.query_open_orders(snapshot).await?)
    }

    async fn list_recent_fills(
        &self,
        snapshot: &RuntimeSnapshot,
    ) -> anyhow::Result<Vec<RecentFill>> {
        Ok(FakeExecutionAdapter.list_recent_fills(snapshot).await?)
    }
}

#[derive(Default)]
struct CountingExecutionAdapter {
    submit_calls: Mutex<Vec<String>>,
}

impl CountingExecutionAdapter {
    fn submit_calls(&self) -> Vec<String> {
        self.submit_calls
            .lock()
            .expect("counting adapter poisoned")
            .clone()
    }
}

#[async_trait]
impl ExecutionAdapter for CountingExecutionAdapter {
    async fn submit_order(
        &self,
        request: SubmitOrderRequest,
        snapshot: &RuntimeSnapshot,
    ) -> anyhow::Result<SubmitOrderResult> {
        self.submit_calls
            .lock()
            .expect("counting adapter poisoned")
            .push(request.client_order_id.clone());
        Ok(FakeExecutionAdapter.submit_order(request, snapshot).await?)
    }

    async fn cancel_orders(
        &self,
        request: CancelOrdersRequest,
        snapshot: &RuntimeSnapshot,
    ) -> anyhow::Result<Vec<OpenOrder>> {
        Ok(FakeExecutionAdapter
            .cancel_orders(request, snapshot)
            .await?)
    }

    async fn query_open_orders(
        &self,
        snapshot: &RuntimeSnapshot,
    ) -> anyhow::Result<Vec<OpenOrder>> {
        Ok(FakeExecutionAdapter.query_open_orders(snapshot).await?)
    }

    async fn list_recent_fills(
        &self,
        snapshot: &RuntimeSnapshot,
    ) -> anyhow::Result<Vec<RecentFill>> {
        Ok(FakeExecutionAdapter.list_recent_fills(snapshot).await?)
    }
}

#[derive(Default)]
struct FailingPlacementExecutionAdapter {
    submit_failures_remaining: AtomicUsize,
}

impl FailingPlacementExecutionAdapter {
    fn with_failures(count: usize) -> Self {
        Self {
            submit_failures_remaining: AtomicUsize::new(count),
        }
    }
}

#[async_trait]
impl ExecutionAdapter for FailingPlacementExecutionAdapter {
    async fn submit_order(
        &self,
        request: SubmitOrderRequest,
        snapshot: &RuntimeSnapshot,
    ) -> anyhow::Result<SubmitOrderResult> {
        if !request.reduce_only && self.submit_failures_remaining.load(Ordering::SeqCst) > 0 {
            self.submit_failures_remaining
                .fetch_sub(1, Ordering::SeqCst);
            anyhow::bail!("strategy placement temporarily failed");
        }
        Ok(FakeExecutionAdapter.submit_order(request, snapshot).await?)
    }

    async fn cancel_orders(
        &self,
        request: CancelOrdersRequest,
        snapshot: &RuntimeSnapshot,
    ) -> anyhow::Result<Vec<OpenOrder>> {
        Ok(FakeExecutionAdapter
            .cancel_orders(request, snapshot)
            .await?)
    }

    async fn query_open_orders(
        &self,
        snapshot: &RuntimeSnapshot,
    ) -> anyhow::Result<Vec<OpenOrder>> {
        Ok(FakeExecutionAdapter.query_open_orders(snapshot).await?)
    }

    async fn list_recent_fills(
        &self,
        snapshot: &RuntimeSnapshot,
    ) -> anyhow::Result<Vec<RecentFill>> {
        Ok(FakeExecutionAdapter.list_recent_fills(snapshot).await?)
    }
}

struct StickyCancelExecutionAdapter;

#[async_trait]
impl ExecutionAdapter for StickyCancelExecutionAdapter {
    async fn submit_order(
        &self,
        request: SubmitOrderRequest,
        snapshot: &RuntimeSnapshot,
    ) -> anyhow::Result<SubmitOrderResult> {
        Ok(FakeExecutionAdapter.submit_order(request, snapshot).await?)
    }

    async fn cancel_orders(
        &self,
        _request: CancelOrdersRequest,
        snapshot: &RuntimeSnapshot,
    ) -> anyhow::Result<Vec<OpenOrder>> {
        Ok(snapshot.execution.open_orders.clone())
    }

    async fn query_open_orders(
        &self,
        snapshot: &RuntimeSnapshot,
    ) -> anyhow::Result<Vec<OpenOrder>> {
        Ok(snapshot.execution.open_orders.clone())
    }

    async fn list_recent_fills(
        &self,
        snapshot: &RuntimeSnapshot,
    ) -> anyhow::Result<Vec<RecentFill>> {
        Ok(snapshot.execution.recent_fills.clone())
    }
}

struct OpenReduceOnlyExecutionAdapter;

#[async_trait]
impl ExecutionAdapter for OpenReduceOnlyExecutionAdapter {
    async fn submit_order(
        &self,
        request: SubmitOrderRequest,
        snapshot: &RuntimeSnapshot,
    ) -> anyhow::Result<SubmitOrderResult> {
        if request.reduce_only {
            return Ok(SubmitOrderResult {
                open_order: Some(OpenOrder {
                    order_id: request.order_id,
                    client_order_id: request.client_order_id,
                    side: request.side,
                    price: request.price,
                    qty: request.qty,
                    filled_qty: 0.0,
                    status: "NEW".into(),
                    created_at: "2025-01-01T00:00:00Z".into(),
                    updated_at: "2025-01-01T00:00:00Z".into(),
                }),
                fill: None,
            });
        }
        Ok(FakeExecutionAdapter.submit_order(request, snapshot).await?)
    }

    async fn cancel_orders(
        &self,
        request: CancelOrdersRequest,
        snapshot: &RuntimeSnapshot,
    ) -> anyhow::Result<Vec<OpenOrder>> {
        Ok(FakeExecutionAdapter
            .cancel_orders(request, snapshot)
            .await?)
    }

    async fn query_open_orders(
        &self,
        snapshot: &RuntimeSnapshot,
    ) -> anyhow::Result<Vec<OpenOrder>> {
        Ok(snapshot.execution.open_orders.clone())
    }

    async fn list_recent_fills(
        &self,
        snapshot: &RuntimeSnapshot,
    ) -> anyhow::Result<Vec<RecentFill>> {
        Ok(snapshot.execution.recent_fills.clone())
    }
}

struct BlockingPlacementExecutionAdapter {
    ready: Arc<Notify>,
}

#[async_trait]
impl ExecutionAdapter for BlockingPlacementExecutionAdapter {
    async fn submit_order(
        &self,
        request: SubmitOrderRequest,
        snapshot: &RuntimeSnapshot,
    ) -> anyhow::Result<SubmitOrderResult> {
        if !request.reduce_only {
            self.ready.notified().await;
        }
        Ok(FakeExecutionAdapter.submit_order(request, snapshot).await?)
    }

    async fn cancel_orders(
        &self,
        request: CancelOrdersRequest,
        snapshot: &RuntimeSnapshot,
    ) -> anyhow::Result<Vec<OpenOrder>> {
        Ok(FakeExecutionAdapter
            .cancel_orders(request, snapshot)
            .await?)
    }

    async fn query_open_orders(
        &self,
        snapshot: &RuntimeSnapshot,
    ) -> anyhow::Result<Vec<OpenOrder>> {
        Ok(FakeExecutionAdapter.query_open_orders(snapshot).await?)
    }

    async fn list_recent_fills(
        &self,
        snapshot: &RuntimeSnapshot,
    ) -> anyhow::Result<Vec<RecentFill>> {
        Ok(FakeExecutionAdapter.list_recent_fills(snapshot).await?)
    }
}

struct NoRefreshCommandExecutionAdapter;

#[async_trait]
impl ExecutionAdapter for NoRefreshCommandExecutionAdapter {
    async fn submit_order(
        &self,
        request: SubmitOrderRequest,
        snapshot: &RuntimeSnapshot,
    ) -> anyhow::Result<SubmitOrderResult> {
        Ok(FakeExecutionAdapter.submit_order(request, snapshot).await?)
    }

    async fn cancel_orders(
        &self,
        request: CancelOrdersRequest,
        snapshot: &RuntimeSnapshot,
    ) -> anyhow::Result<Vec<OpenOrder>> {
        Ok(FakeExecutionAdapter
            .cancel_orders(request, snapshot)
            .await?)
    }

    async fn query_open_orders(
        &self,
        _snapshot: &RuntimeSnapshot,
    ) -> anyhow::Result<Vec<OpenOrder>> {
        anyhow::bail!("kernel should not refresh open orders after command side effect");
    }

    async fn list_recent_fills(
        &self,
        _snapshot: &RuntimeSnapshot,
    ) -> anyhow::Result<Vec<RecentFill>> {
        anyhow::bail!("kernel should not refresh fills after command side effect");
    }
}

struct SlowPlacementExecutionAdapter {
    delay: Duration,
    submit_calls: AtomicUsize,
}

impl SlowPlacementExecutionAdapter {
    fn new(delay: Duration) -> Self {
        Self {
            delay,
            submit_calls: AtomicUsize::new(0),
        }
    }

    fn submit_calls(&self) -> usize {
        self.submit_calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl ExecutionAdapter for SlowPlacementExecutionAdapter {
    async fn submit_order(
        &self,
        request: SubmitOrderRequest,
        snapshot: &RuntimeSnapshot,
    ) -> anyhow::Result<SubmitOrderResult> {
        if !request.reduce_only {
            self.submit_calls.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(self.delay).await;
        }
        Ok(FakeExecutionAdapter.submit_order(request, snapshot).await?)
    }

    async fn cancel_orders(
        &self,
        request: CancelOrdersRequest,
        snapshot: &RuntimeSnapshot,
    ) -> anyhow::Result<Vec<OpenOrder>> {
        Ok(FakeExecutionAdapter
            .cancel_orders(request, snapshot)
            .await?)
    }

    async fn query_open_orders(
        &self,
        _snapshot: &RuntimeSnapshot,
    ) -> anyhow::Result<Vec<OpenOrder>> {
        anyhow::bail!("strategy sync should not refresh open orders after submit");
    }

    async fn list_recent_fills(
        &self,
        _snapshot: &RuntimeSnapshot,
    ) -> anyhow::Result<Vec<RecentFill>> {
        anyhow::bail!("strategy sync should not refresh fills after submit");
    }
}

#[derive(Default)]
struct AbortAwareExecutionAdapter {
    active_submits: AtomicUsize,
    dropped_submits: AtomicUsize,
}

impl AbortAwareExecutionAdapter {
    fn active_submits(&self) -> usize {
        self.active_submits.load(Ordering::SeqCst)
    }

    fn dropped_submits(&self) -> usize {
        self.dropped_submits.load(Ordering::SeqCst)
    }
}

struct InflightSubmitGuard<'a> {
    active: &'a AtomicUsize,
    dropped: &'a AtomicUsize,
}

impl Drop for InflightSubmitGuard<'_> {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::SeqCst);
        self.dropped.fetch_add(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl ExecutionAdapter for AbortAwareExecutionAdapter {
    async fn submit_order(
        &self,
        request: SubmitOrderRequest,
        snapshot: &RuntimeSnapshot,
    ) -> anyhow::Result<SubmitOrderResult> {
        if request.reduce_only {
            self.active_submits.fetch_add(1, Ordering::SeqCst);
            let _guard = InflightSubmitGuard {
                active: &self.active_submits,
                dropped: &self.dropped_submits,
            };
            std::future::pending::<()>().await;
            unreachable!("aborted reduce-only submit should never complete");
        }
        Ok(FakeExecutionAdapter.submit_order(request, snapshot).await?)
    }

    async fn cancel_orders(
        &self,
        request: CancelOrdersRequest,
        snapshot: &RuntimeSnapshot,
    ) -> anyhow::Result<Vec<OpenOrder>> {
        Ok(FakeExecutionAdapter
            .cancel_orders(request, snapshot)
            .await?)
    }

    async fn query_open_orders(
        &self,
        snapshot: &RuntimeSnapshot,
    ) -> anyhow::Result<Vec<OpenOrder>> {
        Ok(FakeExecutionAdapter.query_open_orders(snapshot).await?)
    }

    async fn list_recent_fills(
        &self,
        snapshot: &RuntimeSnapshot,
    ) -> anyhow::Result<Vec<RecentFill>> {
        Ok(FakeExecutionAdapter.list_recent_fills(snapshot).await?)
    }
}

#[derive(Default)]
struct FactlessThenFailPlacementExecutionAdapter {
    submit_calls: AtomicUsize,
}

#[async_trait]
impl ExecutionAdapter for FactlessThenFailPlacementExecutionAdapter {
    async fn submit_order(
        &self,
        request: SubmitOrderRequest,
        snapshot: &RuntimeSnapshot,
    ) -> anyhow::Result<SubmitOrderResult> {
        if !request.reduce_only {
            let call_index = self.submit_calls.fetch_add(1, Ordering::SeqCst);
            if call_index == 0 {
                return Ok(SubmitOrderResult::default());
            }
            anyhow::bail!("second placement failed");
        }
        Ok(FakeExecutionAdapter.submit_order(request, snapshot).await?)
    }

    async fn cancel_orders(
        &self,
        request: CancelOrdersRequest,
        snapshot: &RuntimeSnapshot,
    ) -> anyhow::Result<Vec<OpenOrder>> {
        Ok(FakeExecutionAdapter
            .cancel_orders(request, snapshot)
            .await?)
    }

    async fn query_open_orders(
        &self,
        snapshot: &RuntimeSnapshot,
    ) -> anyhow::Result<Vec<OpenOrder>> {
        Ok(FakeExecutionAdapter.query_open_orders(snapshot).await?)
    }

    async fn list_recent_fills(
        &self,
        snapshot: &RuntimeSnapshot,
    ) -> anyhow::Result<Vec<RecentFill>> {
        Ok(FakeExecutionAdapter.list_recent_fills(snapshot).await?)
    }
}

fn spawn_submit_command(
    engine: grid_platform_service::kernel::EngineHandle,
    command_id: &str,
) -> JoinHandle<anyhow::Result<grid_platform_service::protocol::CommandAccepted>> {
    let request = CommandRequest {
        command_id: command_id.into(),
    };
    tokio::spawn(async move { engine.submit_command(CommandType::CancelAll, request).await })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn command_flow_updates_read_model_and_publishes_ack() -> Result<()> {
    let (engine, read_model, mut events_rx) = spawn_engine();
    assert_eq!(
        read_model.read().expect("read model").system_events()[0].message,
        "Rust in-memory runtime bootstrapped."
    );
    let initial_open_orders = read_model
        .read()
        .expect("read model")
        .snapshot()
        .execution
        .open_orders
        .len();

    let accepted = engine
        .submit_command(
            CommandType::Pause,
            CommandRequest {
                command_id: "cmd_pause_kernel".into(),
            },
        )
        .await?;

    assert_eq!(accepted.status, CommandStatus::Accepted);
    assert_eq!(
        read_model
            .read()
            .expect("read model")
            .snapshot()
            .runtime
            .strategy_state,
        "paused"
    );
    let snapshot = read_model.read().expect("read model").snapshot();
    assert!(
        snapshot
            .execution
            .last_command_ack_event
            .as_ref()
            .is_some_and(|ack| ack.command_id == "cmd_pause_kernel")
    );
    assert_eq!(
        snapshot.execution.recent_commands[0].command_id,
        "cmd_pause_kernel"
    );
    assert_eq!(snapshot.execution.open_orders.len(), initial_open_orders);

    let event = timeout(Duration::from_secs(1), events_rx.recv())
        .await?
        .expect("engine event");
    assert_eq!(event.sequence, 1);
    match event.event {
        EngineEvent::CommandAck(ack) => {
            assert_eq!(ack.command_id, "cmd_pause_kernel");
            assert_eq!(ack.command, CommandType::Pause);
            assert_eq!(ack.status, CommandStatus::Completed);
        }
        other => panic!("unexpected engine event: {other:?}"),
    }

    assert_eq!(
        read_model.read().expect("read model").system_events()[0].source,
        "commands"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn price_tick_updates_read_model_and_publishes_event() -> Result<()> {
    let (engine, read_model, mut events_rx) = spawn_engine();
    let initial = read_model
        .read()
        .expect("read model")
        .snapshot()
        .runtime
        .last_price;

    let tick = engine.emit_price_tick().await?;
    assert!(tick.last_price > initial);

    let snapshot = read_model.read().expect("read model").snapshot();
    assert_eq!(snapshot.runtime.last_price, tick.last_price);
    assert_eq!(snapshot.runtime.mark_price, tick.mark_price);

    let event = timeout(Duration::from_secs(1), events_rx.recv())
        .await?
        .expect("engine event");
    assert_eq!(event.sequence, 1);
    match event.event {
        EngineEvent::PriceUpdated(event) => assert_eq!(event.last_price, tick.last_price),
        other => panic!("unexpected engine event: {other:?}"),
    }

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn execution_command_returns_accepted_before_async_completion() -> Result<()> {
    let ready = Arc::new(Notify::new());
    let adapter = BlockingExecutionAdapter {
        ready: ready.clone(),
    };
    let (engine, read_model, mut events_rx) = spawn_engine_with_adapter(Arc::new(adapter));

    let submit = spawn_submit_command(engine.clone(), "cmd_async_cancel");

    let accepted = timeout(Duration::from_millis(100), submit).await???;
    assert_eq!(accepted.status, CommandStatus::Accepted);

    let snapshot = read_model.read().expect("read model").snapshot();
    assert!(
        snapshot
            .execution
            .pending_commands
            .iter()
            .any(|item| item.command_id == "cmd_async_cancel")
    );
    assert!(snapshot.execution.last_command_ack_event.is_none());

    ready.notify_waiters();

    let event = timeout(Duration::from_secs(1), events_rx.recv())
        .await?
        .expect("engine event");
    match event.event {
        EngineEvent::CommandAck(ack) => {
            assert_eq!(ack.command_id, "cmd_async_cancel");
            assert_eq!(ack.status, CommandStatus::Completed);
        }
        other => panic!("unexpected engine event: {other:?}"),
    }

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn execution_command_times_out_on_service_when_adapter_stalls() -> Result<()> {
    let adapter = BlockingExecutionAdapter {
        ready: Arc::new(Notify::new()),
    };
    let (engine, read_model, mut events_rx) = spawn_engine_with_adapter(Arc::new(adapter));

    let accepted = engine
        .submit_command(
            CommandType::FlattenNow,
            CommandRequest {
                command_id: "cmd_timeout_service".into(),
            },
        )
        .await?;
    assert_eq!(accepted.status, CommandStatus::Accepted);

    let event = timeout(Duration::from_secs(1), events_rx.recv())
        .await?
        .expect("engine event");
    match event.event {
        EngineEvent::CommandAck(ack) => {
            assert_eq!(ack.command_id, "cmd_timeout_service");
            assert_eq!(ack.status, CommandStatus::TimedOut);
            assert!(ack.message.contains("timed out"));
        }
        other => panic!("unexpected engine event: {other:?}"),
    }

    let snapshot = read_model.read().expect("read model").snapshot();
    assert!(
        snapshot
            .execution
            .pending_commands
            .iter()
            .all(|item| item.command_id != "cmd_timeout_service")
    );
    assert_eq!(
        snapshot.execution.recent_commands[0].status,
        CommandStatus::TimedOut
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn late_execution_result_does_not_override_timed_out_terminal_state() -> Result<()> {
    let ready = Arc::new(Notify::new());
    let adapter = BlockingExecutionAdapter {
        ready: ready.clone(),
    };
    let (engine, read_model, mut events_rx) = spawn_engine_with_adapter(Arc::new(adapter));

    let accepted = engine
        .submit_command(
            CommandType::FlattenNow,
            CommandRequest {
                command_id: "cmd_timeout_then_late_result".into(),
            },
        )
        .await?;
    assert_eq!(accepted.status, CommandStatus::Accepted);

    let timed_out = timeout(Duration::from_secs(1), async {
        loop {
            let event = events_rx.recv().await.expect("engine event");
            match event.event {
                EngineEvent::CommandAck(ack)
                    if ack.command_id == "cmd_timeout_then_late_result" =>
                {
                    break ack;
                }
                _ => continue,
            }
        }
    })
    .await?;
    assert_eq!(timed_out.status, CommandStatus::TimedOut);

    ready.notify_waiters();

    assert!(
        timeout(Duration::from_millis(300), async {
            loop {
                let event = events_rx.recv().await.expect("engine event");
                match event.event {
                    EngineEvent::CommandAck(ack)
                        if ack.command_id == "cmd_timeout_then_late_result" =>
                    {
                        break ack;
                    }
                    _ => continue,
                }
            }
        })
        .await
        .is_err()
    );

    let snapshot = read_model.read().expect("read model").snapshot();
    assert_eq!(
        snapshot.execution.recent_commands[0].status,
        CommandStatus::TimedOut
    );
    assert_eq!(
        snapshot.execution.recent_commands[0].summary,
        "Execution timed out while waiting for terminal result."
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timed_out_execution_aborts_background_task_before_next_command() -> Result<()> {
    let adapter = Arc::new(AbortAwareExecutionAdapter::default());
    let (engine, _read_model, mut events_rx) = spawn_engine_with_adapter(adapter.clone());

    let accepted = engine
        .submit_command(
            CommandType::FlattenNow,
            CommandRequest {
                command_id: "cmd_timeout_abort".into(),
            },
        )
        .await?;
    assert_eq!(accepted.status, CommandStatus::Accepted);

    let timed_out = timeout(Duration::from_secs(1), async {
        loop {
            let event = events_rx.recv().await.expect("engine event");
            match event.event {
                EngineEvent::CommandAck(ack) if ack.command_id == "cmd_timeout_abort" => break ack,
                _ => continue,
            }
        }
    })
    .await?;
    assert_eq!(timed_out.status, CommandStatus::TimedOut);

    timeout(Duration::from_secs(1), async {
        loop {
            if adapter.dropped_submits() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    assert_eq!(adapter.active_submits(), 0);

    let accepted = engine
        .submit_command(
            CommandType::CancelAll,
            CommandRequest {
                command_id: "cmd_after_abort".into(),
            },
        )
        .await?;
    assert_eq!(accepted.status, CommandStatus::Accepted);

    let completed = timeout(Duration::from_secs(1), async {
        loop {
            let event = events_rx.recv().await.expect("engine event");
            match event.event {
                EngineEvent::CommandAck(ack) if ack.command_id == "cmd_after_abort" => break ack,
                _ => continue,
            }
        }
    })
    .await?;
    assert_eq!(completed.status, CommandStatus::Completed);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn price_tick_places_missing_active_grid_orders_when_strategy_is_running() -> Result<()> {
    let mut runtime = PersistedRuntime::in_memory_bootstrap();
    runtime.snapshot = RuntimeSnapshot::sample();
    runtime.snapshot.runtime.position_qty = 0.0;
    runtime.snapshot.runtime.position_avg_price = 0.0;
    runtime.snapshot.execution.open_orders.clear();
    runtime.snapshot.strategy.config.levels_per_side = 1;

    let adapter = Arc::new(CountingExecutionAdapter::default());
    let (engine, read_model, _events_rx) =
        spawn_engine_with_runtime_and_adapter(runtime, None, adapter.clone());

    engine.emit_price_tick().await?;

    timeout(Duration::from_secs(1), async {
        loop {
            let snapshot = read_model.read().expect("read model").snapshot();
            if snapshot.execution.open_orders.len() == 2 {
                break snapshot;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;

    let snapshot = read_model.read().expect("read model").snapshot();
    assert_eq!(snapshot.execution.open_orders.len(), 2);
    assert_eq!(
        adapter.submit_calls(),
        vec!["grid_buy_01".to_string(), "grid_sell_01".to_string()]
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_strategy_placement_rolls_back_runtime_without_storage() -> Result<()> {
    let mut runtime = PersistedRuntime::in_memory_bootstrap();
    runtime.snapshot = RuntimeSnapshot::sample();
    runtime.snapshot.runtime.position_qty = 0.0;
    runtime.snapshot.runtime.position_avg_price = 0.0;
    runtime.snapshot.execution.open_orders.clear();
    runtime.snapshot.strategy.config.levels_per_side = 1;

    let initial_price = runtime.snapshot.runtime.last_price;
    let adapter = Arc::new(FailingPlacementExecutionAdapter::with_failures(1));
    let (engine, read_model, _events_rx) =
        spawn_engine_with_runtime_and_adapter(runtime, None, adapter);

    let error = engine
        .emit_price_tick()
        .await
        .expect_err("placement failure should reject price tick");
    assert!(error.to_string().contains("failed to sync strategy orders"));

    let after_failed_tick = read_model.read().expect("read model").snapshot();
    assert_eq!(after_failed_tick.runtime.last_price, initial_price);
    assert!(after_failed_tick.execution.open_orders.is_empty());

    let tick = engine.emit_price_tick().await?;
    let recovered_snapshot = read_model.read().expect("read model").snapshot();
    assert_eq!(tick.last_price, initial_price + 0.11);
    assert_eq!(recovered_snapshot.runtime.last_price, initial_price + 0.11);
    assert_eq!(recovered_snapshot.execution.open_orders.len(), 2);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn factless_strategy_submit_does_not_count_as_partial_success() -> Result<()> {
    let mut runtime = PersistedRuntime::in_memory_bootstrap();
    runtime.snapshot = RuntimeSnapshot::sample();
    runtime.snapshot.runtime.position_qty = 0.0;
    runtime.snapshot.runtime.position_avg_price = 0.0;
    runtime.snapshot.execution.open_orders.clear();
    runtime.snapshot.strategy.config.levels_per_side = 1;

    let initial_price = runtime.snapshot.runtime.last_price;
    let adapter = Arc::new(FactlessThenFailPlacementExecutionAdapter::default());
    let (engine, read_model, _events_rx) =
        spawn_engine_with_runtime_and_adapter(runtime, None, adapter);

    let error = engine
        .emit_price_tick()
        .await
        .expect_err("factless placement should reject price tick");
    assert!(error.to_string().contains("failed to sync strategy orders"));

    let snapshot = read_model.read().expect("read model").snapshot();
    assert_eq!(snapshot.runtime.last_price, initial_price);
    assert!(snapshot.execution.open_orders.is_empty());

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pause_blocks_grid_replacement_until_resume() -> Result<()> {
    let mut runtime = PersistedRuntime::in_memory_bootstrap();
    runtime.snapshot = RuntimeSnapshot::sample();
    runtime.snapshot.runtime.position_qty = 0.0;
    runtime.snapshot.runtime.position_avg_price = 0.0;
    runtime.snapshot.runtime.strategy_state = "paused".into();
    runtime.snapshot.execution.open_orders.clear();
    runtime.snapshot.strategy.config.levels_per_side = 1;

    let adapter = Arc::new(CountingExecutionAdapter::default());
    let (engine, read_model, _events_rx) =
        spawn_engine_with_runtime_and_adapter(runtime, None, adapter.clone());

    engine.emit_price_tick().await?;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let paused_snapshot = read_model.read().expect("read model").snapshot();
    assert!(paused_snapshot.execution.open_orders.is_empty());
    assert!(adapter.submit_calls().is_empty());

    engine
        .submit_command(
            CommandType::Resume,
            CommandRequest {
                command_id: "cmd_resume_reseed".into(),
            },
        )
        .await?;

    timeout(Duration::from_secs(1), async {
        loop {
            let snapshot = read_model.read().expect("read model").snapshot();
            if snapshot.execution.open_orders.len() == 2 {
                break snapshot;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;

    let snapshot = read_model.read().expect("read model").snapshot();
    assert_eq!(snapshot.runtime.strategy_state, "running");
    assert_eq!(snapshot.execution.open_orders.len(), 2);
    assert_eq!(
        adapter.submit_calls(),
        vec!["grid_buy_01".to_string(), "grid_sell_01".to_string()]
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stalled_strategy_placement_times_out_instead_of_blocking_engine_loop() -> Result<()> {
    let mut runtime = PersistedRuntime::in_memory_bootstrap();
    runtime.snapshot = RuntimeSnapshot::sample();
    runtime.snapshot.runtime.position_qty = 0.0;
    runtime.snapshot.runtime.position_avg_price = 0.0;
    runtime.snapshot.execution.open_orders.clear();
    runtime.snapshot.strategy.config.levels_per_side = 1;

    let adapter = Arc::new(BlockingPlacementExecutionAdapter {
        ready: Arc::new(Notify::new()),
    });
    let (engine, read_model, _events_rx) =
        spawn_engine_with_runtime_and_adapter(runtime, None, adapter);

    let result = timeout(Duration::from_secs(2), engine.emit_price_tick()).await;
    let error = result
        .expect("strategy placement should fail before caller timeout")
        .expect_err("strategy placement stall should surface as error");
    assert!(error.to_string().contains("failed to sync strategy orders"));

    let snapshot = read_model.read().expect("read model").snapshot();
    assert_eq!(snapshot.execution.open_orders.len(), 0);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn price_tick_marks_strategy_pending_rebuild_when_inventory_blocks_rebuild() -> Result<()> {
    let mut runtime = PersistedRuntime::in_memory_bootstrap();
    runtime.snapshot = RuntimeSnapshot::sample();
    runtime.snapshot.strategy.config.rebuild_threshold_bps = 0.1;
    runtime.snapshot.strategy.rebuild_reference_price = runtime.snapshot.runtime.last_price;
    let open_orders_before = runtime.snapshot.execution.open_orders.len();

    let (engine, read_model, _events_rx) =
        grid_platform_service::kernel::spawn_engine_with_runtime(runtime, None);

    engine.emit_price_tick().await?;

    let snapshot = read_model.read().expect("read model").snapshot();
    assert_eq!(snapshot.strategy.status, StrategyStatus::PendingRebuild);
    assert!(snapshot.strategy.pending_rebuild_reason.is_some());
    assert_eq!(snapshot.execution.open_orders.len(), open_orders_before);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn price_tick_engages_breaker_and_broadcasts_risk_alert_when_stop_loss_is_triggered()
-> Result<()> {
    let mut runtime = PersistedRuntime::in_memory_bootstrap();
    runtime.snapshot = RuntimeSnapshot::sample();
    runtime.snapshot.runtime.last_price = 100.0;
    runtime.snapshot.runtime.mark_price = 100.0;
    runtime.snapshot.runtime.position_qty = -0.25;
    runtime.snapshot.runtime.position_avg_price = 100.0;
    runtime.snapshot.risk.stop_loss_pct = 0.05;
    let open_orders_before = runtime.snapshot.execution.open_orders.len();

    let (engine, read_model, mut events_rx) =
        grid_platform_service::kernel::spawn_engine_with_runtime(runtime, None);

    engine.emit_price_tick().await?;

    let risk_event = timeout(Duration::from_secs(1), async {
        loop {
            let event = events_rx.recv().await.expect("engine event");
            match event.event {
                EngineEvent::RiskAlert(alert) => break alert,
                _ => continue,
            }
        }
    })
    .await?;

    assert_eq!(risk_event.code, "STOP_LOSS_TRIGGERED");

    let snapshot = read_model.read().expect("read model").snapshot();
    assert!(snapshot.risk.breaker_engaged);
    assert_eq!(snapshot.risk.risk_level, RiskLevel::Danger);
    assert_eq!(snapshot.execution.open_orders.len(), open_orders_before);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn second_execution_command_fails_while_another_is_in_flight() -> Result<()> {
    let ready = Arc::new(Notify::new());
    let adapter = BlockingExecutionAdapter {
        ready: ready.clone(),
    };
    let (engine, read_model, mut events_rx) = spawn_engine_with_adapter(Arc::new(adapter));

    let first = engine
        .submit_command(
            CommandType::CancelAll,
            CommandRequest {
                command_id: "cmd_in_flight_01".into(),
            },
        )
        .await?;
    assert_eq!(first.status, CommandStatus::Accepted);

    let second = engine
        .submit_command(
            CommandType::FlattenNow,
            CommandRequest {
                command_id: "cmd_in_flight_02".into(),
            },
        )
        .await?;
    assert_eq!(second.status, CommandStatus::Accepted);

    let rejected = timeout(Duration::from_millis(100), async {
        loop {
            let event = events_rx.recv().await.expect("engine event");
            match event.event {
                EngineEvent::CommandAck(ack) if ack.command_id == "cmd_in_flight_02" => break ack,
                _ => continue,
            }
        }
    })
    .await?;
    assert_eq!(rejected.status, CommandStatus::Failed);
    assert!(rejected.message.contains("in flight"));

    let snapshot = read_model.read().expect("read model").snapshot();
    assert!(
        snapshot
            .execution
            .pending_commands
            .iter()
            .any(|item| item.command_id == "cmd_in_flight_01")
    );
    assert!(
        snapshot
            .execution
            .pending_commands
            .iter()
            .all(|item| item.command_id != "cmd_in_flight_02")
    );

    ready.notify_waiters();

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn execution_command_retries_transient_adapter_failure_before_succeeding() -> Result<()> {
    let adapter = Arc::new(FlakyCancelExecutionAdapter::with_failures(1));
    let (engine, read_model, mut events_rx) = spawn_engine_with_adapter(adapter.clone());

    let accepted = engine
        .submit_command(
            CommandType::CancelAll,
            CommandRequest {
                command_id: "cmd_retry_cancel".into(),
            },
        )
        .await?;

    assert_eq!(accepted.status, CommandStatus::Accepted);

    let event = timeout(Duration::from_secs(1), events_rx.recv())
        .await?
        .expect("engine event");
    match event.event {
        EngineEvent::CommandAck(ack) => {
            assert_eq!(ack.command_id, "cmd_retry_cancel");
            assert_eq!(ack.status, CommandStatus::Completed);
            assert_eq!(ack.message, "All open orders cancelled.");
        }
        other => panic!("unexpected engine event: {other:?}"),
    }

    let snapshot = read_model.read().expect("read model").snapshot();
    assert!(snapshot.execution.open_orders.is_empty());
    assert_eq!(adapter.cancel_attempts(), 2);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_all_completes_without_follow_up_query_refresh() -> Result<()> {
    let (engine, read_model, mut events_rx) =
        spawn_engine_with_adapter(Arc::new(NoRefreshCommandExecutionAdapter));

    let accepted = engine
        .submit_command(
            CommandType::CancelAll,
            CommandRequest {
                command_id: "cmd_cancel_without_refresh".into(),
            },
        )
        .await?;

    assert_eq!(accepted.status, CommandStatus::Accepted);

    let event = timeout(Duration::from_secs(1), events_rx.recv())
        .await?
        .expect("engine event");
    match event.event {
        EngineEvent::CommandAck(ack) => {
            assert_eq!(ack.command_id, "cmd_cancel_without_refresh");
            assert_eq!(ack.status, CommandStatus::Completed);
            assert_eq!(ack.message, "All open orders cancelled.");
        }
        other => panic!("unexpected engine event: {other:?}"),
    }

    let snapshot = read_model.read().expect("read model").snapshot();
    assert!(snapshot.execution.open_orders.is_empty());

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_all_fails_when_targeted_orders_remain_open() -> Result<()> {
    let (engine, read_model, mut events_rx) =
        spawn_engine_with_adapter(Arc::new(StickyCancelExecutionAdapter));
    let before = read_model.read().expect("read model").snapshot();

    let accepted = engine
        .submit_command(
            CommandType::CancelAll,
            CommandRequest {
                command_id: "cmd_cancel_sticky".into(),
            },
        )
        .await?;

    assert_eq!(accepted.status, CommandStatus::Accepted);

    let event = timeout(Duration::from_secs(1), events_rx.recv())
        .await?
        .expect("engine event");
    match event.event {
        EngineEvent::CommandAck(ack) => {
            assert_eq!(ack.command_id, "cmd_cancel_sticky");
            assert_eq!(ack.status, CommandStatus::Failed);
            assert!(ack.message.contains("did not clear"));
        }
        other => panic!("unexpected engine event: {other:?}"),
    }

    let snapshot = read_model.read().expect("read model").snapshot();
    assert_eq!(snapshot.execution.open_orders, before.execution.open_orders);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_command_records_reason_without_side_effects_after_retry_budget_is_exhausted()
-> Result<()> {
    let adapter = Arc::new(FlakyCancelExecutionAdapter::with_failures(3));
    let (engine, read_model, mut events_rx) = spawn_engine_with_adapter(adapter.clone());
    let before = read_model.read().expect("read model").snapshot();
    let open_orders_before = before.execution.open_orders.len();
    let fills_before = before.execution.recent_fills.len();

    let accepted = engine
        .submit_command(
            CommandType::CancelAll,
            CommandRequest {
                command_id: "cmd_fail_cancel".into(),
            },
        )
        .await?;

    assert_eq!(accepted.status, CommandStatus::Accepted);

    let event = timeout(Duration::from_secs(1), events_rx.recv())
        .await?
        .expect("engine event");
    match event.event {
        EngineEvent::CommandAck(ack) => {
            assert_eq!(ack.command_id, "cmd_fail_cancel");
            assert_eq!(ack.status, CommandStatus::Failed);
            assert!(ack.message.contains("transient cancel-all failure"));
        }
        other => panic!("unexpected engine event: {other:?}"),
    }

    let snapshot = read_model.read().expect("read model").snapshot();
    assert_eq!(snapshot.execution.open_orders.len(), open_orders_before);
    assert_eq!(snapshot.execution.recent_fills.len(), fills_before);
    assert_eq!(
        snapshot.execution.recent_commands[0].status,
        CommandStatus::Failed
    );
    assert!(
        snapshot.execution.recent_commands[0]
            .summary
            .contains("transient cancel-all failure")
    );
    assert_eq!(
        snapshot
            .execution
            .last_command_ack_event
            .as_ref()
            .expect("ack event")
            .status,
        CommandStatus::Failed
    );
    assert_eq!(adapter.cancel_attempts(), 3);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn flatten_now_fails_when_reduce_only_order_has_no_terminal_fill() -> Result<()> {
    let (engine, read_model, mut events_rx) =
        spawn_engine_with_adapter(Arc::new(OpenReduceOnlyExecutionAdapter));
    let before = read_model.read().expect("read model").snapshot();

    let accepted = engine
        .submit_command(
            CommandType::FlattenNow,
            CommandRequest {
                command_id: "cmd_flatten_open_only".into(),
            },
        )
        .await?;

    assert_eq!(accepted.status, CommandStatus::Accepted);

    let event = timeout(Duration::from_secs(1), events_rx.recv())
        .await?
        .expect("engine event");
    match event.event {
        EngineEvent::CommandAck(ack) => {
            assert_eq!(ack.command_id, "cmd_flatten_open_only");
            assert_eq!(ack.status, CommandStatus::Failed);
            assert!(ack.message.contains("terminal fill"));
        }
        other => panic!("unexpected engine event: {other:?}"),
    }

    let snapshot = read_model.read().expect("read model").snapshot();
    assert_eq!(snapshot.runtime.position_qty, before.runtime.position_qty);
    assert_eq!(
        snapshot.execution.recent_fills,
        before.execution.recent_fills
    );
    assert!(
        snapshot
            .execution
            .open_orders
            .iter()
            .any(|order| order.client_order_id == "reduce_only_cmd_flatten_open_only")
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_after_flatten_does_not_pause_when_reduce_only_order_has_no_terminal_fill()
-> Result<()> {
    let (engine, read_model, mut events_rx) =
        spawn_engine_with_adapter(Arc::new(OpenReduceOnlyExecutionAdapter));

    let accepted = engine
        .submit_command(
            CommandType::ShutdownAfterFlatten,
            CommandRequest {
                command_id: "cmd_shutdown_open_only".into(),
            },
        )
        .await?;

    assert_eq!(accepted.status, CommandStatus::Accepted);

    let event = timeout(Duration::from_secs(1), events_rx.recv())
        .await?
        .expect("engine event");
    match event.event {
        EngineEvent::CommandAck(ack) => {
            assert_eq!(ack.command_id, "cmd_shutdown_open_only");
            assert_eq!(ack.status, CommandStatus::Failed);
            assert!(ack.message.contains("terminal fill"));
        }
        other => panic!("unexpected engine event: {other:?}"),
    }

    let snapshot = read_model.read().expect("read model").snapshot();
    assert_eq!(snapshot.runtime.strategy_state, "running");
    assert!(
        snapshot
            .execution
            .open_orders
            .iter()
            .any(|order| order.client_order_id == "reduce_only_cmd_shutdown_open_only")
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_after_flatten_completes_without_follow_up_query_refresh() -> Result<()> {
    let (engine, read_model, mut events_rx) =
        spawn_engine_with_adapter(Arc::new(NoRefreshCommandExecutionAdapter));

    let accepted = engine
        .submit_command(
            CommandType::ShutdownAfterFlatten,
            CommandRequest {
                command_id: "cmd_shutdown_without_refresh".into(),
            },
        )
        .await?;

    assert_eq!(accepted.status, CommandStatus::Accepted);

    let event = timeout(Duration::from_secs(1), events_rx.recv())
        .await?
        .expect("engine event");
    match event.event {
        EngineEvent::CommandAck(ack) => {
            assert_eq!(ack.command_id, "cmd_shutdown_without_refresh");
            assert_eq!(ack.status, CommandStatus::Completed);
        }
        other => panic!("unexpected engine event: {other:?}"),
    }

    let snapshot = read_model.read().expect("read model").snapshot();
    assert_eq!(snapshot.runtime.strategy_state, "paused");
    assert_eq!(snapshot.runtime.position_qty, 0.0);
    assert!(snapshot.execution.open_orders.is_empty());
    assert!(snapshot.execution.recent_fills.iter().any(|fill| {
        fill.client_order_id.as_deref() == Some("reduce_only_cmd_shutdown_without_refresh")
    }));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timed_out_command_records_reason_without_side_effects() -> Result<()> {
    let adapter = BlockingExecutionAdapter {
        ready: Arc::new(Notify::new()),
    };
    let (engine, read_model, mut events_rx) = spawn_engine_with_adapter(Arc::new(adapter));
    let before = read_model.read().expect("read model").snapshot();
    let position_before = before.runtime.position_qty;
    let fills_before = before.execution.recent_fills.len();

    let accepted = engine
        .submit_command(
            CommandType::FlattenNow,
            CommandRequest {
                command_id: "cmd_timeout_flatten".into(),
            },
        )
        .await?;

    assert_eq!(accepted.status, CommandStatus::Accepted);

    let event = timeout(Duration::from_secs(1), events_rx.recv())
        .await?
        .expect("engine event");
    match event.event {
        EngineEvent::CommandAck(ack) => {
            assert_eq!(ack.command_id, "cmd_timeout_flatten");
            assert_eq!(ack.status, CommandStatus::TimedOut);
            assert_eq!(
                ack.message,
                "Execution timed out while waiting for terminal result."
            );
        }
        other => panic!("unexpected engine event: {other:?}"),
    }

    let snapshot = read_model.read().expect("read model").snapshot();
    assert_eq!(snapshot.runtime.position_qty, position_before);
    assert_eq!(snapshot.execution.recent_fills.len(), fills_before);
    assert_eq!(
        snapshot.execution.recent_commands[0].status,
        CommandStatus::TimedOut
    );
    assert_eq!(
        snapshot.execution.recent_commands[0].summary,
        "Execution timed out while waiting for terminal result."
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn strategy_sync_uses_total_time_budget_and_keeps_successful_placements() -> Result<()> {
    let mut runtime = PersistedRuntime::in_memory_bootstrap();
    runtime.snapshot = RuntimeSnapshot::sample();
    runtime.snapshot.runtime.position_qty = 0.0;
    runtime.snapshot.runtime.position_avg_price = 0.0;
    runtime.snapshot.execution.open_orders.clear();
    runtime.snapshot.strategy.config.levels_per_side = 1;
    let initial_price = runtime.snapshot.runtime.last_price;

    let adapter = Arc::new(SlowPlacementExecutionAdapter::new(Duration::from_millis(
        200,
    )));
    let (engine, read_model, _events_rx) =
        spawn_engine_with_runtime_and_adapter(runtime, None, adapter.clone());

    let started_at = Instant::now();
    let tick = engine.emit_price_tick().await?;
    let elapsed = started_at.elapsed();

    assert_eq!(tick.last_price, initial_price + 0.11);
    assert!(
        elapsed < Duration::from_millis(350),
        "strategy sync exceeded total time budget: {elapsed:?}"
    );

    let snapshot = read_model.read().expect("read model").snapshot();
    assert_eq!(snapshot.execution.open_orders.len(), 1);
    assert_eq!(adapter.submit_calls(), 2);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idempotent_command_keeps_single_record_and_reason() -> Result<()> {
    let (engine, read_model, mut events_rx) = spawn_engine();

    engine
        .submit_command(
            CommandType::CancelAll,
            CommandRequest {
                command_id: "cmd_idempotent_cancel".into(),
            },
        )
        .await?;
    let _ = timeout(Duration::from_secs(1), events_rx.recv())
        .await?
        .expect("engine event");

    let first_snapshot = read_model.read().expect("read model").snapshot();
    assert_eq!(first_snapshot.execution.recent_commands.len(), 1);

    engine
        .submit_command(
            CommandType::CancelAll,
            CommandRequest {
                command_id: "cmd_idempotent_cancel".into(),
            },
        )
        .await?;

    let snapshot = read_model.read().expect("read model").snapshot();
    assert_eq!(snapshot.execution.recent_commands.len(), 1);
    assert!(
        snapshot.execution.recent_commands[0]
            .summary
            .contains("Idempotent hit")
    );
    assert_eq!(
        snapshot.execution.recent_commands[0].status,
        CommandStatus::Completed
    );

    let event = timeout(Duration::from_secs(1), events_rx.recv())
        .await?
        .expect("engine event");
    match event.event {
        EngineEvent::CommandAck(ack) => {
            assert_eq!(ack.command_id, "cmd_idempotent_cancel");
            assert_eq!(ack.status, CommandStatus::Completed);
            assert!(ack.message.contains("Idempotent hit"));
        }
        other => panic!("unexpected engine event: {other:?}"),
    }

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reusing_command_id_with_different_command_type_is_rejected() -> Result<()> {
    let (engine, read_model, mut events_rx) = spawn_engine();

    engine
        .submit_command(
            CommandType::CancelAll,
            CommandRequest {
                command_id: "cmd_mismatched_id".into(),
            },
        )
        .await?;
    let _ = timeout(Duration::from_secs(1), events_rx.recv())
        .await?
        .expect("engine event");

    let before = read_model.read().expect("read model").snapshot();
    let error = engine
        .submit_command(
            CommandType::FlattenNow,
            CommandRequest {
                command_id: "cmd_mismatched_id".into(),
            },
        )
        .await
        .expect_err("mismatched command_id reuse should be rejected");
    assert!(error.to_string().contains("different command"));

    let after = read_model.read().expect("read model").snapshot();
    assert_eq!(after.execution.recent_commands.len(), 1);
    assert_eq!(
        after.execution.recent_commands[0].command,
        CommandType::CancelAll
    );
    assert_eq!(
        after.execution.last_command_ack_event,
        before.execution.last_command_ack_event
    );
    assert!(
        timeout(Duration::from_millis(200), events_rx.recv())
            .await
            .is_err()
    );

    Ok(())
}
