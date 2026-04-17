# Runtime 启动 Bootstrap 边界调整 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把启动期实时交易所状态探测、保证金预检、guard seed 和初始 exchange state sync 从 `assembly` 收到 runtime-owned `startup_bootstrap`，删除两套启动探测语义，同时保持现有启动外部行为不变。

**Architecture:** `poise-application` 在 `TrackPreparedDefinition` 上新增 `TrackStartupDefinition` 行为投影，让 startup 预算语义继续留在 definition owner 内部；`poise-server` 的 `assembly` 只负责静态装配和传递 startup definitions，不再查询 live `position` / `account_capacity_snapshot`。`server::runtime::startup_bootstrap` 统一拥有 `subscribe_user_data -> get_server_time -> probe -> preflight -> apply -> replay`，并删除独立 `startup_sync` owner 与 `with_account_capacity_snapshots` 启动注入路径。

**Tech Stack:** Rust workspace, Tokio, Cargo tests, chrono, Markdown

---

执行本计划时，按仓库规则逐个 task 验收。每个 task 验收通过后，必须立即 `git add`、`git commit`，并把实际 commit SHA 回写到对应 task 下，再进入下一个 task。

## Files And Responsibilities

- Modify: `application/src/track_definition.rs`
  由 definition owner 新增 `TrackStartupDefinition` 和 `TrackPreparedDefinition::startup_definition()`，把 `required_additional_notional(position_qty)` 这层 startup 预算语义固定在 application 边界内。
- Modify: `application/src/lib.rs`
  导出 `TrackStartupDefinition`，供 server runtime 构造和测试支撑使用。
- Modify: `core/src/strategy.rs`
  由 core owner 提供 `position_qty -> exposure -> abs_notional` 换算 helper，供 startup definition 和 runtime 仓位吸收共用。
- Create: `server/src/runtime/startup_bootstrap.rs`
  单独拥有 startup probe、预检、guard seed、exchange state apply 和 buffered replay。
- Modify: `server/src/runtime/mod.rs`
  runtime 持有 startup definitions，`start()` 改成调用 `startup_bootstrap::complete_startup`，删除 `with_account_capacity_snapshots` 路径和独立 `startup_sync` owner。
- Delete: `server/src/runtime/startup_sync.rs`
  删除旧的启动探测 owner；保留的 helper 要么移动到 `startup_bootstrap.rs`，要么留在 `runtime/mod.rs` 作为不访问端口的共享小函数。
- Modify: `server/src/runtime/tests/mod.rs`
  把 startup 测试模块从 `startup_sync` 改成 `startup`。
- Create: `server/src/runtime/tests/startup.rs`
  统一承接 startup bootstrap 的行为测试：live state 恢复、buffered replay、margin guard seed、保证金预检失败。
- Delete: `server/src/runtime/tests/startup_sync.rs`
  移除旧命名和旧 owner 对应的测试文件。
- Modify: `server/src/test_support.rs`
  扩展现有 prepared registry 测试 helper，让 server tests 继续通过 prepared definition owner 获取 `startup_definition()`，而不是在 runtime tests support 里手工重建输入字段。
- Modify: `server/src/runtime/tests/support.rs`
  给 fake exchange / fake account 增加 startup bootstrap 所需的 account capacity snapshot 计数、失败注入和可配置 notional；startup definition fixture 则改为复用 `server/src/test_support.rs` 里的 prepared registry helper。
- Modify: `server/src/runtime/tests/execution.rs`
  更新 startup retry、startup sampling 和 margin guard 相关测试，使其通过新的 startup bootstrap 路径断言行为。
- Modify: `server/src/runtime/tests/user_data.rs`
  如果 runtime 构造函数签名变化，更新构造入口；保留 user data live apply 行为不变。
- Modify: `server/src/runtime/tests/reconcile.rs`
  如果 `apply_user_data_event` / observation helper 的归属移动，更新测试导入并保持语义不变。
- Modify: `server/src/assembly.rs`
  删除装配期 live `position` / `account_capacity_snapshot` 查询、删除装配期保证金预检，只保留 startup leverage、`exchange_info` 和 `TrackManager` 构造；把 startup definitions 传给 runtime。
- Modify: `TODO.md`
  为这条任务补上 plan 链接，执行过程中按仓库规则回写 commit SHA。

### Task 1: 在 application 边界新增 TrackStartupDefinition

**Files:**
- Modify: `core/src/strategy.rs`
- Modify: `application/src/track_definition.rs`
- Modify: `application/src/lib.rs`
- Test: `core/src/strategy.rs`
- Test: `application/src/track_definition.rs`

- [x] **Step 1: 先写失败测试，锁住 core/application 两层的 startup 预算语义**

在 `core/src/strategy.rs` 的 tests 模块里先增加这两条回归测试：

```rust
#[test]
fn exposure_from_position_qty_uses_base_qty_per_unit() {
    let config = neutral_config();

    let exposure = config.exposure_from_position_qty(195.0);

    assert!((exposure.0 - 52.0).abs() < 0.01);
}

#[test]
fn abs_notional_from_position_qty_reuses_exposure_conversion() {
    let config = neutral_config();

    let notional = config.abs_notional_from_position_qty(195.0);

    assert!((notional - 19_500.0).abs() < 0.01);
}
```

在 `application/src/track_definition.rs` 的 tests 模块里增加这组测试和 helper：

```rust
fn startup_definition_fixture(max_notional: Option<f64>) -> TrackPreparedDefinition {
    TrackPreparedDefinition::from_configured(
        ConfiguredTrackDefinition::try_from_input(ConfiguredTrackInput {
            track_id: TrackId::new("btc-core"),
            venue: Venue::Binance,
            symbol: "BTCUSDT".into(),
            lower_price: 90.0,
            upper_price: 110.0,
            long_exposure_units: 8.0,
            short_exposure_units: 8.0,
            notional_per_unit: 375.0,
            min_rebalance_units: Some(0.5),
            shape_family: Some(ShapeFamily::Linear),
            out_of_band_policy: Some(OutOfBandPolicy::Freeze),
            max_notional,
            daily_loss_limit: 300.0,
            total_loss_limit: 600.0,
            tick_timeout_secs: Some(30),
        })
        .unwrap(),
    )
}

#[test]
fn prepared_track_definition_projects_startup_definition() {
    let prepared = startup_definition_fixture(Some(3_000.0));
    let startup = prepared.startup_definition();

    assert_eq!(startup.track_id().as_str(), "btc-core");
    assert_eq!(startup.instrument().symbol, "BTCUSDT");
}

#[test]
fn startup_definition_required_additional_notional_subtracts_existing_position_notional() {
    let prepared = startup_definition_fixture(Some(3_000.0));
    let startup = prepared.startup_definition();

    // center = 100, notional_per_unit = 375, 所以 1 unit = 3.75 qty。
    // 现有 4 units -> qty = 15.0，对应已占用 1_500 notional。
    assert_eq!(startup.required_additional_notional(15.0), 1_500.0);
}

#[test]
fn startup_definition_required_additional_notional_clamps_to_zero() {
    let prepared = startup_definition_fixture(Some(3_000.0));
    let startup = prepared.startup_definition();

    // 8 units -> qty = 30.0，正好覆盖 3_000 notional。
    assert_eq!(startup.required_additional_notional(30.0), 0.0);
    assert_eq!(startup.required_additional_notional(45.0), 0.0);
}
```

- [x] **Step 2: 运行定向测试，确认当前实现失败**

Run:

- `cargo test -p poise-core strategy::tests::exposure_from_position_qty_uses_base_qty_per_unit -- --exact --nocapture`
- `cargo test -p poise-core strategy::tests::abs_notional_from_position_qty_reuses_exposure_conversion -- --exact --nocapture`
- `cargo test -p poise-application prepared_track_definition_projects_startup_definition -- --exact --nocapture`
- `cargo test -p poise-application startup_definition_required_additional_notional_subtracts_existing_position_notional -- --exact --nocapture`
- `cargo test -p poise-application startup_definition_required_additional_notional_clamps_to_zero -- --exact --nocapture`

Expected:

- `TrackConfig` 还没有 `exposure_from_position_qty()` / `abs_notional_from_position_qty()`
- `TrackPreparedDefinition` 还没有 `startup_definition()`
- `TrackStartupDefinition` 还不存在

- [x] **Step 3: 先补 core helper，再实现 TrackStartupDefinition 和 startup_definition() 投影**

先在 `core/src/strategy.rs` 的 `impl TrackConfig` 里增加：

```rust
pub fn exposure_from_position_qty(&self, qty: f64) -> Exposure {
    let unit_qty = self.base_qty_per_unit();
    if !unit_qty.is_finite() || unit_qty <= f64::EPSILON {
        Exposure(0.0)
    } else {
        Exposure(qty / unit_qty)
    }
}

pub fn abs_notional_from_position_qty(&self, qty: f64) -> f64 {
    self.exposure_from_position_qty(qty).0.abs() * self.notional_per_unit
}
```

在 `application/src/track_definition.rs` 增加：

```rust
#[derive(Debug, Clone, PartialEq)]
pub struct TrackStartupDefinition {
    track_id: TrackId,
    instrument: Instrument,
    track_config: TrackConfig,
    budget: CapacityBudget,
}

impl TrackStartupDefinition {
    pub fn track_id(&self) -> &TrackId {
        &self.track_id
    }

    pub fn instrument(&self) -> &Instrument {
        &self.instrument
    }

    pub fn required_additional_notional(&self, position_qty: f64) -> f64 {
        let current_position_notional =
            self.track_config.abs_notional_from_position_qty(position_qty);
        (self.budget.max_notional - current_position_notional).max(0.0)
    }
}
```

并在 `TrackPreparedDefinition` 上增加：

```rust
pub fn startup_definition(&self) -> TrackStartupDefinition {
    TrackStartupDefinition {
        track_id: self.track_id.clone(),
        instrument: self.instrument.clone(),
        track_config: self.track_config.clone(),
        budget: self.budget.clone(),
    }
}
```

同时在 `application/src/lib.rs` 导出：

```rust
pub use track_definition::{
    ConfiguredTrackDefinition, ConfiguredTrackInput, PreparedTrackRegistry,
    TrackPreparedDefinition, TrackReadDefinition, TrackStartupDefinition,
};
```

- [x] **Step 4: 跑 Task 1 回归**

Run:

- `cargo test -p poise-core strategy::tests:: -- --nocapture`
- `cargo test -p poise-application track_definition::tests:: -- --nocapture`

Expected:

- `qty -> strategy budget` 的换算固定在 core owner
- startup definition 的 owner 固定在 application 边界
- runtime 只需要 `required_additional_notional(position_qty)`

- [x] **Step 5: Commit**

```bash
git add core/src/strategy.rs application/src/track_definition.rs application/src/lib.rs docs/superpowers/plans/2026-04-18-runtime-startup-bootstrap-boundary.md
git commit -m "feat(application): add track startup definitions"
```

执行后在本 task 下追加一行：`Implemented in: <commit-sha>`

Implemented in: `3219271`

### Task 2: 在同一个提交边界内完成 startup owner 切换

**Files:**
- Create: `server/src/runtime/startup_bootstrap.rs`
- Modify: `server/src/runtime/mod.rs`
- Modify: `server/src/assembly.rs`
- Modify: `server/src/runtime/tests/mod.rs`
- Create: `server/src/runtime/tests/startup.rs`
- Delete: `server/src/runtime/tests/startup_sync.rs`
- Delete: `server/src/runtime/startup_sync.rs`
- Modify: `server/src/test_support.rs`
- Modify: `server/src/runtime/tests/support.rs`
- Modify: `server/src/runtime/tests/execution.rs`
- Modify: `server/src/runtime/tests/user_data.rs`
- Modify: `server/src/runtime/tests/reconcile.rs`
- [x] **Step 1: 先写失败测试，锁住 owner 切换后的外部行为**

在 `server/src/runtime/tests/startup.rs` 新增这组测试：

```rust
#[tokio::test]
async fn startup_bootstrap_restores_claimed_live_order_before_first_tick() {
    let snapshot = test_snapshot();
    let live_order = btc_exchange_order(
        "snapshot-1",
        "snapshot-1",
        Side::Buy,
        94.5,
        0.25,
        0.0,
        OrderStatus::New,
    );
    let fixture = runtime_fixture(
        Some(snapshot),
        btc_position(7.5, 3.0),
        vec![live_order],
        test_budget(),
    )
    .await;

    let handles = fixture.runtime.start().await.unwrap();
    let instance = current_instance(&fixture.state).await;

    assert_eq!(instance.current_exposure, Exposure(2.0));
    assert_eq!(instance.desired_exposure, Some(Exposure(6.0)));

    shutdown(handles).await;
}

#[tokio::test]
async fn startup_bootstrap_seeds_account_margin_guard_from_capacity_probe() {
    let fixture = runtime_fixture_with_account_capacity(
        None,
        btc_position(0.0, 0.0),
        vec![],
        test_budget(),
        500.0,
    )
    .await;

    let handles = fixture.runtime.start().await.unwrap();
    let constraint = fixture.state.account_margin_guard.constraint_for(&btc_instrument());

    assert_eq!(constraint.max_increase_notional, Some(500.0));
    shutdown(handles).await;
}

#[tokio::test]
async fn startup_bootstrap_rejects_insufficient_remaining_margin() {
    let mut budget = test_budget();
    budget.max_notional = 20_000.0;
    let fixture = runtime_fixture_with_account_capacity(
        None,
        btc_position(195.0, 0.0),
        vec![],
        budget,
        499.0,
    )
    .await;

    let error = fixture.runtime.start().await.err().unwrap();

    assert!(error.to_string().contains("required 500"));
    assert!(error.to_string().contains("available 499"));
}
```

并在 `server/src/runtime/tests/execution.rs` 的 `start_retries_transient_startup_failures` 里增加 account capacity probe 的重试断言：

```rust
fixture.exchange.fail_next_server_time_requests(2);
fixture.exchange.fail_next_open_orders_requests(1);
fixture.exchange.fail_next_account_capacity_requests(1);

assert_eq!(
    fixture.exchange.get_account_capacity_snapshot_calls.load(Ordering::SeqCst),
    2
);
```

同时把 `server/src/assembly.rs` 里这组旧测试迁出本文件：

- `startup_margin_preflight_fails_when_configured_max_notional_exceeds_account_capacity`
- `startup_margin_preflight_allows_when_existing_position_covers_part_of_max_notional`
- `startup_margin_preflight_still_fails_when_remaining_required_notional_exceeds_capacity`

并新增一个装配期顺序测试替代它们：

```rust
#[tokio::test]
async fn startup_preparation_builds_runtime_exchange_before_setting_leverage_and_loading_exchange_info()
{
    let repository = Arc::new(SqliteStorage::in_memory().unwrap());
    let call_log = Arc::new(Mutex::new(Vec::new()));
    let startup_exchange = Arc::new(StartupOrderExchange::new(call_log.clone(), 1_000_000.0));
    let config = Config {
        bind_address: "127.0.0.1:0".into(),
        tracks: vec![TrackDefinition {
            track_id: "btc-core".into(),
            symbol: "BTCUSDT".into(),
            lower_price: 90.0,
            upper_price: 110.0,
            long_exposure_units: 8.0,
            short_exposure_units: 8.0,
            notional_per_unit: 375.0,
            min_rebalance_units: Some(0.5),
            shape_family: Some(poise_core::strategy::ShapeFamily::Linear),
            out_of_band_policy: Some(poise_core::strategy::OutOfBandPolicy::Freeze),
            max_notional: Some(3_000.0),
            leverage: Some(20),
            daily_loss_limit: 300.0,
            total_loss_limit: 600.0,
            tick_timeout_secs: None,
        }],
        exchange: ExchangeConfig::default(),
        account_monitor: Default::default(),
    };
    let prepared_registry = test_prepared_registry(&config);
    let exchange = super::build_exchange_and_prepare_startup_with(
        &config,
        prepared_registry.as_ref(),
        {
            let call_log = call_log.clone();
            let startup_exchange = startup_exchange.clone();
            move || {
                let call_log = call_log.clone();
                let startup_exchange = startup_exchange.clone();
                async move {
                    call_log.lock().unwrap().push("build_exchange".to_string());
                    Ok(Exchange::new(
                        Venue::Binance,
                        startup_exchange.clone(),
                        Arc::new(FakeMarketData::empty()),
                        Arc::new(FakeAccountSummaryPort),
                        startup_exchange.clone(),
                        startup_exchange,
                    ))
                }
            }
        },
        {
            let call_log = call_log.clone();
            move || {
                Ok(
                    Arc::new(RecordingSymbolLeverageSetter::succeed(call_log.clone()))
                        as Arc<dyn SymbolLeverageSetter>,
                )
            }
        },
    )
    .await
    .unwrap();

    super::assemble_with_state_store(
        &config,
        prepared_registry,
        exchange,
        StateRepositories::new(repository),
        Arc::new(SystemClock),
    )
    .await
    .unwrap();

    assert_eq!(
        *call_log.lock().unwrap(),
        vec![
            "build_exchange".to_string(),
            "set_leverage:BTCUSDT:20".to_string(),
            "get_exchange_info:BTCUSDT".to_string(),
        ]
    );
}
```

- [x] **Step 2: 运行定向测试，确认当前实现失败**

Run:

- `cargo test -p poise-server runtime::tests::startup::startup_bootstrap_restores_claimed_live_order_before_first_tick -- --exact --nocapture`
- `cargo test -p poise-server runtime::tests::startup::startup_bootstrap_seeds_account_margin_guard_from_capacity_probe -- --exact --nocapture`
- `cargo test -p poise-server runtime::tests::startup::startup_bootstrap_rejects_insufficient_remaining_margin -- --exact --nocapture`
- `cargo test -p poise-server runtime::tests::execution::start_retries_transient_startup_failures -- --exact --nocapture`
- `cargo test -p poise-server assembly::tests::startup_preparation_builds_runtime_exchange_before_setting_leverage_and_loading_exchange_info -- --exact --nocapture`

Expected:

- `runtime::tests::startup` 模块和 fixture helper 还不存在
- fake exchange 还不能配置 account capacity snapshot 或统计其调用次数
- assembly 现有调用序列里仍然包含 `get_position` / `get_account_capacity_snapshot`

- [x] **Step 3: 扩展 runtime 测试支撑，但 startup definition 继续由 prepared definition owner 产出**

先在 `server/src/test_support.rs` 把现有私有 helper 提升成 server tests 可复用入口：

```rust
pub(crate) fn test_prepared_registry_with_budget(
    track_id: &str,
    symbol: &str,
    budget: CapacityBudget,
) -> Arc<PreparedTrackRegistry> {
    prepared_registry_for(track_id, symbol, budget)
}
```

在 `server/src/runtime/tests/support.rs` 的 `FakeExchange` 上增加：

```rust
pub(crate) get_account_capacity_snapshot_calls: AtomicUsize,
account_capacity_failures_remaining: AtomicUsize,
max_increase_notional: Mutex<f64>,
```

补上 helper：

```rust
pub(crate) fn fail_next_account_capacity_requests(&self, count: usize) {
    self.account_capacity_failures_remaining
        .store(count, Ordering::SeqCst);
}

pub(crate) fn set_max_increase_notional(&self, value: f64) {
    *self.max_increase_notional.lock().unwrap() = value;
}

pub(crate) fn test_startup_definition(budget: CapacityBudget) -> TrackStartupDefinition {
    crate::test_support::test_prepared_registry_with_budget(
        "BTCUSDT",
        "BTCUSDT",
        budget,
    )
    .get(&TrackId::new("BTCUSDT"))
    .unwrap()
    .startup_definition()
}
```

并新增 fixture：

```rust
pub(crate) async fn runtime_fixture_with_account_capacity(
    restored_snapshot: Option<TrackRuntimeSnapshot>,
    position: Position,
    open_orders: Vec<ExchangeOrder>,
    budget: CapacityBudget,
    max_increase_notional: f64,
) -> RuntimeFixture {
    let fixture = runtime_fixture(restored_snapshot, position, open_orders, budget).await;
    fixture
        .exchange
        .set_max_increase_notional(max_increase_notional);
    fixture
}
```

- [x] **Step 4: 在同一个 task 里同时接入 runtime bootstrap，并删除旧 owner**

在 `server/src/runtime/mod.rs` 上做这几件事：

1. 模块声明从：

```rust
mod startup_sync;
```

改成：

```rust
mod startup_bootstrap;
```

2. `ServerRuntime` 增加字段：

```rust
    startup_definitions: Vec<poise_application::TrackStartupDefinition>,
```

3. 生产构造函数改成：

```rust
pub(crate) fn with_startup_definitions(
    state: RuntimeState,
    effect_worker_state: EffectWorkerState,
    ports: RuntimePorts,
    startup_definitions: Vec<poise_application::TrackStartupDefinition>,
    recovery_retry_interval: Duration,
) -> Self
```

4. `start()` 改成：

```rust
pub async fn start(&self) -> Result<RuntimeHandles> {
    let mut user_receiver = self.account.subscribe_user_data().await?;
    let startup_cutoff =
        retry_startup_step("get_server_time", || self.metadata.get_server_time()).await?;
    startup_bootstrap::complete_startup(self, &mut user_receiver, startup_cutoff).await?;
    // 之后保留现有 startup pending seed 与 task spawn 逻辑
}
```

5. 新建 `server/src/runtime/startup_bootstrap.rs`，实现：

```rust
struct TrackStartupProbe {
    track_id: String,
    instrument: Instrument,
    position: Position,
    open_orders: Vec<ExchangeOrder>,
    account_capacity_snapshot: AccountCapacitySnapshot,
}

struct TrackStartupSeed {
    track_id: String,
    position: PositionObservation,
    open_orders: Vec<OrderObservation>,
}

pub(super) async fn complete_startup(
    runtime: &ServerRuntime,
    receiver: &mut mpsc::Receiver<UserDataEvent>,
    startup_cutoff: DateTime<Utc>,
) -> Result<()> {
    let mut account_capacity_snapshots = HashMap::new();
    let mut track_seeds = Vec::new();

    for track in &runtime.startup_definitions {
        let instrument = track.instrument().clone();
        let position = super::retry_startup_step("get_position", || {
            runtime.execution.get_position(&instrument)
        })
        .await?;
        let open_orders = super::retry_startup_step("get_open_orders", || {
            runtime.execution.get_open_orders(&instrument)
        })
        .await?;
        let account_capacity_snapshot = super::retry_startup_step(
            "get_account_capacity_snapshot",
            || runtime.account.get_account_capacity_snapshot(&instrument),
        )
        .await?;

        let required_additional_notional =
            track.required_additional_notional(position.qty);
        if required_additional_notional > account_capacity_snapshot.max_increase_notional {
            return Err(anyhow!(
                "insufficient account margin for configured max_notional on track `{}`: required {}, available {}",
                track.track_id().as_str(),
                required_additional_notional,
                account_capacity_snapshot.max_increase_notional
            ));
        }

        account_capacity_snapshots.insert(instrument.clone(), account_capacity_snapshot);
        track_seeds.push(TrackStartupSeed {
            track_id: track.track_id().as_str().to_string(),
            position: super::position_observation(&position),
            open_orders: open_orders.iter().map(super::order_observation).collect(),
        });
    }

    runtime
        .state
        .account_margin_guard
        .replace_snapshots(account_capacity_snapshots);

    for seed in track_seeds {
        runtime
            .state
            .reconcile
            .observation_service
            .sync_exchange_state(&seed.track_id, seed.position, seed.open_orders)
            .await?;
    }

    replay_buffered_user_data(runtime, receiver, startup_cutoff).await
}
```

并把 buffered replay 逻辑移到本模块里，保留 `apply_user_data_event` 复用。

在同一个未提交工作区里，同时完成旧 owner 删除，避免形成已提交的双轨状态：

1. `server/src/assembly.rs` 删除 live `position` / `account_capacity_snapshot` 查询和装配期保证金预检：

```rust
let mut startup_definitions = Vec::new();

for track in prepared_registry.iter() {
    let track_id = track.track_id().clone();
    let instrument = track.instrument().clone();
    let info = load_exchange_info_with_retry(exchange.metadata(), &instrument).await?;
    startup_definitions.push(track.startup_definition());

    manager.add_track_with_tick_timeout_secs(
        track_id.clone(),
        instrument,
        track.track_config().clone(),
        track.budget(),
        info.rules,
        track.tick_timeout_secs(),
    )?;
    if let Some(snapshot) = repositories.load_track_state(track_id.as_str()).await? {
        manager.restore_track_state(&snapshot)?;
    }
}
```

并把 runtime 构造改成：

```rust
runtime: ServerRuntime::with_startup_definitions(
    runtime_state,
    effect_worker_state,
    RuntimePorts::new(
        exchange.execution_port(),
        exchange.market_data_port(),
        exchange.account_port(),
        exchange.metadata_port(),
        clock,
    ),
    startup_definitions,
    Duration::from_secs(1),
),
```

同时删除：

- `load_position_with_retry`
- `load_account_capacity_snapshot_with_retry`

2. `server/src/runtime/startup_sync.rs` 整文件删除

3. `server/src/runtime/tests/startup_sync.rs` 整文件删除

4. `server/src/runtime/mod.rs` 删除旧转发：

```rust
async fn startup_sync(&self) -> Result<()> {
    startup_sync::startup_sync(self).await
}

async fn replay_startup_user_data(
    &self,
    receiver: &mut mpsc::Receiver<UserDataEvent>,
    startup_cutoff: chrono::DateTime<chrono::Utc>,
) -> Result<()> {
    startup_sync::replay_startup_user_data(self, receiver, startup_cutoff).await
}
```

5. 把 `retry_startup_step` 保留在 `runtime/mod.rs`

6. 把 `position_observation`、`order_observation`、`apply_user_data_event` 留在 `runtime/mod.rs` 或 `startup_bootstrap.rs`，但它们不能再经由 `startup_sync.rs`

- [x] **Step 5: 更新 runtime tests 模块组织，并让所有构造入口能提供 startup definitions**

在 `server/src/runtime/tests/mod.rs`：

```rust
mod execution;
mod reconcile;
mod startup;
mod support;
mod user_data;
```

在 `server/src/runtime/tests/support.rs` 的 runtime 构造里，把 `ServerRuntime::with_reconcile_and_account_refresh_intervals`、`with_reconcile_intervals`、`new` 都切到带 startup definitions 的版本。例如：

```rust
runtime: ServerRuntime::with_startup_definitions(
    state.runtime_state(),
    worker_state.effect_worker_state.clone(),
    RuntimePorts::new(
        execution,
        market_data as Arc<dyn MarketDataPort>,
        account,
        metadata,
        clock,
    ),
    vec![test_startup_definition(budget)],
    options.recovery_retry_interval,
)
```

`server/src/runtime/tests/user_data.rs`、`server/src/runtime/tests/reconcile.rs`、`server/src/runtime/tests/execution.rs` 里任何直接调用 runtime 构造函数的地方，也都统一切到新的 constructor。

- [x] **Step 6: 跑 Task 2 回归**

Run:

- `cargo test -p poise-server runtime::tests::startup::startup_bootstrap_restores_claimed_live_order_before_first_tick -- --exact --nocapture`
- `cargo test -p poise-server runtime::tests::startup::startup_bootstrap_seeds_account_margin_guard_from_capacity_probe -- --exact --nocapture`
- `cargo test -p poise-server runtime::tests::startup::startup_bootstrap_rejects_insufficient_remaining_margin -- --exact --nocapture`
- `cargo test -p poise-server runtime::tests::execution::start_retries_transient_startup_failures -- --exact --nocapture`
- `cargo test -p poise-server runtime::tests::execution::startup_preflight_marks_all_pending_submit_effects_not_only_dispatchable_ones -- --exact --nocapture`
- `cargo test -p poise-server assembly::tests::startup_preparation_builds_runtime_exchange_before_setting_leverage_and_loading_exchange_info -- --exact --nocapture`
- `cargo test -p poise-server runtime::tests::reconcile::apply_user_data_event_preserves_write_service_mutation_error_kind -- --exact --nocapture`
- `cargo test -p poise-server runtime::tests::startup:: -- --nocapture`
- `cargo test -p poise-server runtime::tests:: -- --nocapture`

Expected:

- startup 探测、预检、guard seed 和 replay 由 runtime bootstrap 统一拥有
- retry 行为覆盖 `get_server_time`、`get_position`、`get_open_orders`、`get_account_capacity_snapshot`
- assembly 不再拥有 live probe
- `apply_user_data_event` / observation helper 仍可被 `user_data` 与 `reconcile` 复用
- 依赖“启动前已缓存 live quote，可在 start 后直接继续 submit”的旧测试已删除；恢复态 submit 必须等待 first fresh tick

- [x] **Step 7: Commit**

```bash
git add server/src/assembly.rs server/src/runtime/mod.rs server/src/runtime/startup_bootstrap.rs server/src/runtime/tests/mod.rs server/src/runtime/tests/startup.rs server/src/runtime/tests/support.rs server/src/runtime/tests/execution.rs server/src/runtime/tests/user_data.rs server/src/runtime/tests/reconcile.rs server/src/test_support.rs docs/superpowers/plans/2026-04-18-runtime-startup-bootstrap-boundary.md
git add -u server/src/runtime/startup_sync.rs server/src/runtime/tests/startup_sync.rs
git commit -m "refactor(server): move startup probing into runtime bootstrap"
```

执行后在本 task 下追加一行：`Implemented in: <commit-sha>`

Implemented in: `18865fc`

### Task 3: 全量验收并同步文档

**Files:**
- Modify: `docs/superpowers/specs/2026-04-18-runtime-startup-bootstrap-boundary-design.md` if implementation forces wording adjustment
- Modify: `docs/superpowers/plans/2026-04-18-runtime-startup-bootstrap-boundary.md`
- Modify: `TODO.md`

- [ ] **Step 1: 跑全量验收，确认 startup 语义和周边测试都通过**

Run:

- `cargo test -p poise-application track_definition::tests:: -- --nocapture`
- `cargo test -p poise-server assembly::tests:: -- --nocapture`
- `cargo test -p poise-server runtime::tests:: -- --nocapture`
- `cargo test -p poise-server -- --nocapture`

Expected:

- application 的 startup definition 行为固定
- assembly 只剩静态装配
- runtime tests 全部通过新 bootstrap owner
- server crate 全量通过

- [ ] **Step 2: 对照 spec 回扫实现，修正文档差异**

核对：

- `TrackStartupDefinition` 是否只暴露行为接口
- `startup_bootstrap` 是否真的同时拥有 probe / preflight / apply / replay
- `startup_sync.rs` 是否已删除
- `with_account_capacity_snapshots` 是否已删除
- runtime tests 是否继续通过 prepared definition owner 获取 `startup_definition()`

如果实现与 spec 或 plan 命名有小偏差，先改文档，再结束。

- [ ] **Step 3: Commit**

```bash
git add docs/superpowers/specs/2026-04-18-runtime-startup-bootstrap-boundary-design.md docs/superpowers/plans/2026-04-18-runtime-startup-bootstrap-boundary.md TODO.md
git commit -m "docs: sync runtime startup bootstrap plan and spec"
```

执行后在本 task 下追加一行：`Implemented in: <commit-sha>`

## Guardrails

- `qty -> strategy budget` 的换算继续留在 `TrackConfig` / `TrackStartupDefinition` owner 一侧，runtime 不得重新拼公式
- `assembly` 不得重新引入 live `position` / `account_capacity_snapshot` 查询
- `startup_bootstrap` 是唯一 startup probe owner；不得再长出第二条 `startup_sync` 端口访问路径
- raw `Position` / `ExchangeOrder` 只能留在 `startup_bootstrap` 内部，不能变成跨模块公共结果类型
- runtime 不得通过测试专用 `manager()` 或 query 接口反向读取静态定义
