# Grid Runtime Boundary Redesign Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把当前按 `symbol` 驱动、由 `server` 外层拼装状态的实现，重构成以 `GridId` 和 `GridRuntime` 为中心的清晰边界。

**Architecture:** 这次重构分四段推进：先收敛术语和身份模型，再把运行态快照下沉到 `engine`，然后用 `GridObservation` / `GridCommand` 取代分散入口，最后把 `server` 压回事务与传输边界。每一段都先补验收测试，再做最小实现，最后跑定向验证。

**Tech Stack:** Rust workspace, tokio, axum, serde, rusqlite, anyhow, chrono

---

## File Structure

### 新建文件

```text
engine/src/grid.rs                # GridId、Venue、Instrument、GridDefinition
engine/src/runtime.rs             # GridRuntime、ObservedState、运行态方法
engine/src/observation.rs         # GridObservation 与归一化观察载荷
engine/src/command.rs             # GridCommand
engine/src/snapshot.rs            # GridRuntimeSnapshot、GridView
engine/src/transition.rs          # GridTransition、GridEffect
```

### 删除文件

```text
engine/src/key.rs                 # 旧 GridId 所在文件，迁移到 grid.rs
engine/src/instance.rs            # 旧 StrategyInstance，迁移到 runtime.rs
```

### 修改文件

- `engine/src/lib.rs`：导出新模块，删除旧模块导出
- `engine/src/manager.rs`：`InstanceManager` 重构为 `GridManager`
- `engine/src/reconciler.rs`：改为围绕 `GridRuntime` 和 transition 产物工作
- `engine/src/ports.rs`：移除运行态快照类型，只保留端口和交易所类型
- `server/src/config.rs`：显式解析 `track_id` 和 `venue`
- `server/src/assembly.rs`：按 `GridDefinition` 和 `Instrument` 装配
- `server/src/application.rs`：只保留事务边界，改用 `observe()` / `command()`
- `server/src/runtime.rs`：把外部流翻译成 `GridObservation` / `GridCommand`
- `server/src/http.rs`：控制命令只走 `command()` 路径
- `server/src/websocket.rs`：消费 transition 广播结果
- `server/src/main.rs`：更新启动流程和示例说明
- `storage/src/schema.rs`：表名和列名改为 `grid_*`
- `storage/src/sqlite.rs`：读写 `GridRuntimeSnapshot`
- `README.md`：更新架构说明和配置示例
- `configs/test.toml`：更新显式 `track_id` 与 `venue`
- `docs/protocol-contract.md`：示例中不再假设 `track_id == symbol`
- `docs/superpowers/specs/2026-03-24-grid-platform-architecture-design.md`：同步旧架构总览
- `docs/superpowers/specs/2026-03-25-grid-runtime-boundary-redesign.md`：实现过程中同步已确认的落地细节
- `tui/tests/fixtures/instance_summaries.json`
- `tui/tests/fixtures/instance_snapshot.json`
- `tui/tests/fixtures/command_response.json`
- `tui/tests/fixtures/ws_event.json`

---

### Task 1: 收敛术语与身份模型

**Files:**
- Create: `engine/src/grid.rs`
- Create: `engine/src/runtime.rs`
- Modify: `engine/src/lib.rs`
- Modify: `engine/src/manager.rs`
- Modify: `server/src/config.rs`
- Modify: `server/src/assembly.rs`
- Modify: `README.md`
- Modify: `configs/test.toml`
- Delete: `engine/src/key.rs`
- Delete: `engine/src/instance.rs`
- Test: `engine/src/manager.rs`
- Test: `server/src/config.rs`
- Test: `server/src/assembly.rs`

- [x] **Step 1: 先写失败测试，锁住显式 `track_id` 和市场绑定约束**

```rust
#[test]
fn parses_config_with_explicit_track_id_and_venue() {
    let config = parse_config(
        r#"
environment = "paper"

[[tracks]]
track_id = "btc-core"
venue = "binance"
symbol = "BTCUSDT"
lower_price = 90.0
upper_price = 110.0
long_exposure_units = 8.0
short_exposure_units = 8.0
notional_per_unit = 375.0
"#,
    )
    .unwrap();

    assert_eq!(config.grids[0].track_id.as_str(), "btc-core");
    assert_eq!(config.grids[0].market.symbol, "BTCUSDT");
    assert_eq!(config.grids[0].market.venue.as_str(), "binance");
}

#[test]
fn assemble_rejects_duplicate_instruments() {
    let error = validate_unique_instruments([
        Instrument::new(Venue::Binance, "BTCUSDT"),
        Instrument::new(Venue::Binance, "BTCUSDT"),
    ])
    .unwrap_err();

    assert!(error.to_string().contains("duplicate instrument"));
}

#[test]
fn resolve_track_id_returns_registered_track_id() {
    let mut manager = test_manager();
    manager
        .register(test_definition("btc-core", "BTCUSDT"), test_exchange_rules())
        .unwrap();

    assert_eq!(
        manager.resolve_track_id(&Instrument::new(Venue::Binance, "BTCUSDT")),
        Some(GridId::new("btc-core"))
    );
}
```

- [x] **Step 2: 运行定向测试确认失败**

Run:

```bash
cargo test -p poise-server config::tests::parses_config_with_explicit_track_id_and_venue
cargo test -p poise-server assembly::tests::assemble_rejects_duplicate_instruments
cargo test -p poise-engine manager::tests::resolve_track_id_returns_registered_track_id
```

Expected: 编译失败或新测试失败，因为当前配置仍从 `symbol` 派生 `GridId`，engine 也还没有市场绑定索引。

- [x] **Step 3: 引入 `GridId`、`Venue`、`Instrument`、`GridDefinition`**

```rust
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GridId(String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Venue {
    Binance,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Instrument {
    pub venue: Venue,
    pub symbol: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GridDefinition {
    pub id: GridId,
    pub instrument: Instrument,
    pub config: GridConfig,
    pub budget: CapacityBudget,
}
```

- [x] **Step 4: 把 `StrategyInstance` / `InstanceManager` 彻底改名为 `GridRuntime` / `GridManager`**

要求：

- 删除核心代码中的 `instance` 旧词，不保留双写
- `GridManager` 内部用 `HashMap<GridId, GridRuntime>` 和 `HashMap<Instrument, GridId>`
- `list_instances()`、`pause_instance()`、`resume_instance()` 一律改成 `list_grids()`、`pause_grid()`、`resume_grid()`

- [x] **Step 5: 配置层改成显式 `track_id` 与 `venue`**

要求：

- `server/src/config.rs` 中的 `GridDefinition` 解析新字段
- 不再保留 `track_id()` 由 `symbol` 派生的逻辑
- `assemble()` 同时校验 `track_id` 唯一和 `Instrument` 唯一
- `README.md` 与 `configs/test.toml` 示例同步更新

- [x] **Step 6: 运行定向测试**

Run:

```bash
cargo test -p poise-engine manager::tests::
cargo test -p poise-server config::tests:: assembly::tests::
```

Expected: 新增身份模型测试通过，旧测试全部改成 `grid` 术语并保持语义一致。

- [ ] **Step 7: 提交**

```bash
git add engine/src/grid.rs engine/src/runtime.rs engine/src/lib.rs engine/src/manager.rs server/src/config.rs server/src/assembly.rs README.md configs/test.toml
git rm engine/src/key.rs engine/src/instance.rs
git commit -m "refactor(engine): converge grid identity and terminology"
```

---

### Task 2: 把运行态快照和恢复逻辑收回 `engine`

**Files:**
- Create: `engine/src/snapshot.rs`
- Modify: `engine/src/runtime.rs`
- Modify: `engine/src/manager.rs`
- Modify: `engine/src/lib.rs`
- Modify: `engine/src/ports.rs`
- Modify: `storage/src/schema.rs`
- Modify: `storage/src/sqlite.rs`
- Modify: `server/src/application.rs`
- Test: `engine/src/manager.rs`
- Test: `storage/src/sqlite.rs`
- Test: `server/src/application.rs`

- [x] **Step 1: 先写失败测试，锁住快照组装只属于 `engine`**

```rust
#[test]
fn snapshot_roundtrip_preserves_runtime_state() {
    let runtime = active_runtime_with_pending_order();
    let snapshot = runtime.snapshot();
    let restored = GridRuntime::restore(snapshot.clone(), test_exchange_rules()).unwrap();

    assert_eq!(restored.snapshot(), snapshot);
}

#[tokio::test]
async fn save_and_load_grid_runtime_snapshot_roundtrip() {
    let storage = SqliteStorage::in_memory().unwrap();
    let snapshot = test_grid_runtime_snapshot();

    storage.save_transition(&snapshot.track_id, &snapshot, &[]).await.unwrap();

    let loaded = storage.load_grid_state(&snapshot.track_id).await.unwrap().unwrap();
    assert_eq!(loaded, snapshot);
}

#[tokio::test]
async fn observe_persists_engine_snapshot_without_server_side_snapshot_builder() {
    let service = test_service();
    let transition = service.observe_market("btc-core", market_observation(95.0)).await.unwrap();

    assert_eq!(transition.snapshot.track_id.as_str(), "btc-core");
}
```

- [x] **Step 2: 运行定向测试确认失败**

Run:

```bash
cargo test -p poise-engine manager::tests::snapshot_roundtrip_preserves_runtime_state
cargo test -p poise-storage sqlite::tests::save_and_load_grid_runtime_snapshot_roundtrip
cargo test -p poise-server application::tests::observe_persists_engine_snapshot_without_server_side_snapshot_builder
```

Expected: 失败，因为当前快照类型仍在 `ports.rs`，`server` 还在手工拼装 `GridSnapshot`。

- [x] **Step 3: 引入 `GridRuntimeSnapshot`，由 `GridRuntime` 自己实现 `snapshot()` / `restore()`**

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GridRuntimeSnapshot {
    pub track_id: GridId,
    pub instrument: Instrument,
    pub config: GridConfig,
    pub status: GridStatus,
    pub current_exposure: Exposure,
    pub desired_exposure: Option<Exposure>,
    pub pending_order: Option<PendingOrder>,
    pub risk: RiskState,
    pub observed: ObservedState,
}
```

- [x] **Step 4: `GridManager` 和 storage 全量改用 `GridRuntimeSnapshot`**

要求：

- `engine/src/ports.rs` 删除运行态快照定义
- `storage/src/sqlite.rs` 的序列化目标改成 `GridRuntimeSnapshot`
- `server/src/application.rs` 删除 `snapshot_from_instance()` 之类的外层拼装函数
- 存储接口参数用 `&GridRuntimeSnapshot`

- [x] **Step 5: schema 命名改成 `grid_*`**

要求：

- `instance_snapshots` 改成 `grid_snapshots`
- 快照主键和事件表字段统一使用 `track_id`
- 不保留 `instance_*` 旧命名

- [x] **Step 6: 运行定向测试**

Run:

```bash
cargo test -p poise-engine manager::tests::
cargo test -p poise-storage sqlite::tests::
cargo test -p poise-server application::tests::
```

Expected: 快照 round-trip、持久化和应用服务测试通过，`server` 不再拥有快照结构知识。

- [ ] **Step 7: 提交**

```bash
git add engine/src/snapshot.rs engine/src/runtime.rs engine/src/lib.rs engine/src/manager.rs engine/src/ports.rs storage/src/schema.rs storage/src/sqlite.rs server/src/application.rs
git commit -m "refactor(engine): move runtime snapshots into engine ownership"
```

---

### Task 3: 用 `GridObservation` / `GridCommand` / `GridTransition` 取代分散入口

**Files:**
- Create: `engine/src/observation.rs`
- Create: `engine/src/command.rs`
- Create: `engine/src/transition.rs`
- Modify: `engine/src/lib.rs`
- Modify: `engine/src/manager.rs`
- Modify: `engine/src/reconciler.rs`
- Modify: `engine/src/ports.rs`
- Modify: `server/src/runtime.rs`
- Modify: `server/src/application.rs`
- Test: `engine/src/manager.rs`
- Test: `engine/src/reconciler.rs`
- Test: `server/src/runtime.rs`

- [x] **Step 1: 先写失败测试，锁住 observation / command 语义**

```rust
#[test]
fn observe_market_reconciles_and_returns_effects() {
    let mut manager = test_manager_with_active_grid();
    let transition = manager
        .observe(
            &GridId::new("btc-core"),
            GridObservation::Market(market_observation(95.0)),
        )
        .unwrap();

    assert!(!transition.effects.is_empty());
    assert_eq!(transition.snapshot.observed.reference_price, Some(95.0));
}

#[test]
fn command_reconcile_uses_cached_reference_price() {
    let mut manager = test_manager_with_cached_price(95.0);
    let transition = manager
        .command(&GridId::new("btc-core"), GridCommand::Reconcile)
        .unwrap();

    assert_eq!(transition.snapshot.observed.reference_price, Some(95.0));
    assert!(!transition.effects.is_empty());
}

#[tokio::test]
async fn position_update_reconciles_via_command_without_faking_market_tick() {
    let fixture = runtime_fixture();
    fixture.send_position_update(0.75).await;

    fixture.assert_command_reconcile_called().await;
    fixture.assert_no_synthetic_market_observation().await;
}
```

- [x] **Step 2: 运行定向测试确认失败**

Run:

```bash
cargo test -p poise-engine manager::tests::observe_market_reconciles_and_returns_effects
cargo test -p poise-engine manager::tests::command_reconcile_uses_cached_reference_price
cargo test -p poise-server runtime::tests::position_update_reconciles_via_command_without_faking_market_tick
```

Expected: 失败，因为当前公开入口还是 `on_price_tick()` / `apply_position_update()` / `apply_order_update()`，并且重算依赖伪造 tick。

- [x] **Step 3: 引入归一化观察与命令类型**

```rust
pub enum GridObservation {
    Market(MarketObservation),
    Position(PositionObservation),
    Order(OrderObservation),
}

pub enum GridCommand {
    Pause,
    Resume,
    Reconcile,
}

pub struct GridTransition {
    pub snapshot: GridRuntimeSnapshot,
    pub events: Vec<DomainEvent>,
    pub effects: Vec<GridEffect>,
}
```

- [x] **Step 4: `GridManager` 改成 `observe()` / `command()`**

要求：

- 删除 `on_price_tick()`、`apply_position_update()`、`apply_order_update()` 公开入口
- `Pause` / `Resume` 也统一走 `command()`
- `GridTransition` 成为唯一写侧结果
- `reconciler.rs` 直接返回 effect 列表，不再让外层手工拼 plan 和事件

- [x] **Step 5: `server/runtime` 删除伪造 `PriceTick` 的路径**

要求：

- 市场流到来时调用 `observe(..., GridObservation::Market(...))`
- 仓位和订单更新后如果需要重算，追加 `command(..., GridCommand::Reconcile)`
- 不再保留 `reconcile_context_for_symbol()` 或任何合成 tick 的辅助逻辑

- [x] **Step 6: 运行定向测试**

Run:

```bash
cargo test -p poise-engine manager::tests:: reconciler::tests::
cargo test -p poise-server runtime::tests::
```

Expected: `engine` 和 `runtime` 通过 observation / command 路径工作，现有行为测试改名后仍通过。

- [ ] **Step 7: 提交**

```bash
git add engine/src/observation.rs engine/src/command.rs engine/src/transition.rs engine/src/lib.rs engine/src/manager.rs engine/src/reconciler.rs engine/src/ports.rs server/src/runtime.rs server/src/application.rs
git commit -m "refactor(engine): replace ad hoc mutation entrypoints with observations and commands"
```

---

### Task 4: 把 `server` 压回事务和传输边界

**Files:**
- Modify: `server/src/application.rs`
- Modify: `server/src/runtime.rs`
- Modify: `server/src/http.rs`
- Modify: `server/src/websocket.rs`
- Modify: `server/src/assembly.rs`
- Modify: `server/src/main.rs`
- Test: `server/src/http.rs`
- Test: `server/src/runtime.rs`
- Test: `server/src/websocket.rs`
- Test: `server/src/main.rs`

- [x] **Step 1: 先写失败测试，锁住新的边界**

```rust
#[tokio::test]
async fn submit_command_uses_service_command_api() {
    let service = recording_service();
    let app = router(ServerState::new(service.clone()));

    let response = post_pause_command(&app, "btc-core").await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(service.recorded_commands(), vec![("btc-core".into(), GridCommand::Pause)]);
}

#[tokio::test]
async fn startup_sync_observes_exchange_state_before_subscribing() {
    let fixture = runtime_fixture();
    fixture.runtime.start().await.unwrap();

    assert_eq!(
        fixture.recorded_observations(),
        vec![
            GridObservation::Position(position_observation(0.0)),
            GridObservation::Order(order_observation_new()),
        ]
    );
}

#[tokio::test]
async fn broadcasts_events_from_persisted_transition() {
    let service = test_service();
    let mut receiver = service.subscribe_events();

    service.command("btc-core", GridCommand::Pause).await.unwrap();

    let event = receiver.recv().await.unwrap();
    assert_eq!(event.track_id, "btc-core");
}
```

- [x] **Step 2: 运行定向测试确认失败**

Run:

```bash
cargo test -p poise-server http::tests::submit_command_uses_service_command_api
cargo test -p poise-server runtime::tests::startup_sync_observes_exchange_state_before_subscribing
cargo test -p poise-server websocket::tests::broadcasts_events_from_persisted_transition
```

Expected: 失败，因为 `http` 和 `runtime` 仍然直接依赖 manager 细节，service 事务 API 还不够稳定。

- [x] **Step 3: 收敛 `application.rs` 为写侧事务边界**

要求：

- 对外只暴露高层方法，例如 `observe_market()`、`observe_position()`、`observe_order()`、`command()`
- 内部负责：调用 `GridManager`、保存 transition、广播事件、执行 rollback
- 不再暴露 manager 的内部结构给 transport 层

- [x] **Step 4: `runtime.rs` 与 `http.rs` 只保留适配职责**

要求：

- `runtime.rs` 只把交易所流翻译成 service 调用
- `http.rs` 只把 HTTP 命令翻译成 `GridCommand`
- `websocket.rs` 只发送已持久化 transition 产生的事件
- `assembly.rs` 只做装配，不做规则判断和状态拼接

- [x] **Step 5: 用显式非 symbol 的 `track_id` 跑通服务端集成测试**

要求：

- 测试用例不要再用 `track_id == symbol`
- 至少有一个真实路径用 `track_id = "btc-core"`、`symbol = "BTCUSDT"`

- [x] **Step 6: 运行定向测试**

Run:

```bash
cargo test -p poise-server http::tests:: runtime::tests:: websocket::tests:: tests::
```

Expected: `server` 边界测试通过，`runtime` / `http` / `ws` 各自职责可以用一句话描述清楚。

- [ ] **Step 7: 提交**

```bash
git add server/src/application.rs server/src/runtime.rs server/src/http.rs server/src/websocket.rs server/src/assembly.rs server/src/main.rs
git commit -m "refactor(server): reduce transports to adapters over grid application service"
```

---

### Task 5: 同步外部文档、夹具和全量验收

**Files:**
- Modify: `README.md`
- Modify: `configs/test.toml`
- Modify: `docs/protocol-contract.md`
- Modify: `docs/superpowers/specs/2026-03-24-grid-platform-architecture-design.md`
- Modify: `docs/superpowers/specs/2026-03-25-grid-runtime-boundary-redesign.md`
- Modify: `tui/tests/fixtures/instance_summaries.json`
- Modify: `tui/tests/fixtures/instance_snapshot.json`
- Modify: `tui/tests/fixtures/command_response.json`
- Modify: `tui/tests/fixtures/ws_event.json`
- Modify: `docs/superpowers/plans/2026-03-25-grid-runtime-boundary-redesign-implementation.md`
- Test: `tui/src/protocol.rs`
- Test: `tui/src/api_client.rs`
- Test: `tui/src/main.rs`

- [x] **Step 1: 先写失败测试，锁住 `track_id` 不再等于 `symbol`**

```rust
#[test]
fn deserializes_grid_snapshot_with_non_symbol_track_id() {
    let snapshot: GridSnapshot =
        serde_json::from_str(include_str!("../tests/fixtures/instance_snapshot.json")).unwrap();

    assert_eq!(snapshot.id, "btc-core");
    assert_eq!(snapshot.symbol, "BTCUSDT");
}
```

- [x] **Step 2: 运行定向测试确认失败**

Run:

```bash
cargo test -p poise-tui protocol::tests::deserializes_grid_snapshot_with_non_symbol_track_id
```

Expected: 失败，因为当前 fixture 和部分断言仍默认 `id == symbol`。

- [x] **Step 3: 更新文档、示例和 fixture**

要求：

- `README.md` 示例配置改成显式 `track_id` 和 `venue`
- `docs/protocol-contract.md` 的响应样例展示非 symbol 的 `track_id`
- TUI fixture 使用 `btc-core` 一类显式 id，保留 `symbol = BTCUSDT`
- 保证用户能从文档中看出 `track_id` 与 `symbol` 的职责不同

- [x] **Step 4: 跑分层验证**

Run:

```bash
cargo test -p poise-engine
cargo test -p poise-storage
cargo test -p poise-server
cargo test -p poise-tui
```

Expected: 各 crate 单独通过。

- [x] **Step 5: 跑全量验收**

Run:

```bash
cargo test
```

Expected: 工作区全部测试通过，端到端测试继续通过。

- [x] **Step 6: 同步任务清单状态**

要求：

- 勾选本计划中已完成的 checkbox
- 如果实施过程中出现更合理的文件边界或任务顺序，更新本计划与 spec
- 不保留已经放弃的过渡方案说明

- [ ] **Step 7: 提交**

```bash
git add README.md configs/test.toml docs/protocol-contract.md docs/superpowers/specs/2026-03-24-grid-platform-architecture-design.md docs/superpowers/specs/2026-03-25-grid-runtime-boundary-redesign.md docs/superpowers/plans/2026-03-25-grid-runtime-boundary-redesign-implementation.md tui/tests/fixtures/instance_summaries.json tui/tests/fixtures/instance_snapshot.json tui/tests/fixtures/command_response.json tui/tests/fixtures/ws_event.json
git commit -m "docs: align grid runtime boundary redesign docs and fixtures"
```
