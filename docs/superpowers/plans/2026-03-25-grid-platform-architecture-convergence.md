# 网格平台架构收敛实施计划

> 实施状态：已完成并通过本地验收，以下清单已按实际落地同步。

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [x]`) syntax for tracking.

**Goal:** 收敛网格身份模型、应用编排边界、共享协议、事件语义和关键命名，在不破坏“同交易所同 symbol 只有一个网格”约束的前提下，降低后续改动面的复杂度。

**Architecture:** 保留 `core` 的纯策略/风控和 `engine` 的状态机骨架，但把当前散落在 `server` 内的 mutate/persist/publish 流程收敛到单一应用服务。身份模型不退化成裸 `symbol`，而是引入名义化 `GridId`，当前值仍由 `symbol` 派生；对外协议下沉到独立 `poise-protocol` crate，`storage` 通过单一状态仓储接口原子保存快照和领域事件。同时把几个已经确认有语义偏差的名字一并修正：外部一等对象统一为 `Grid`，无语义容器名改成职责名，单位信息直接进字段名，订单和价格模型与真实数据来源保持一致。详见[平台架构 spec](../specs/2026-03-24-grid-platform-architecture-design.md) 和[策略研究](../../grid-strategy-product-theory-research.md)。

**Tech Stack:** Rust workspace, tokio, axum, serde, rusqlite

**前置约束：**
- 当前业务约束成立：同一个交易所内，一个 `symbol` 只能管理一个网格。
- 第一版只接 Binance，但 spec 仍把“多交易所”视为未来约束，所以内部不能把身份模型退化成无语义的裸字符串。
- 工作树当前有用户自己的未提交删除/修改，执行计划时不要恢复这些无关变更。
- 探索阶段不保留旧命名兼容层；旧词确认废弃后，直接同步代码、测试、夹具和配置。

## 已采纳的命名调整

- `Instance` 没有承载真实领域概念。执行顺序上，先在配置和内部身份模型里落 `GridId` / `GridDefinition`，再在 Task 5 统一把对外 `InstanceSummary` / `InstanceSnapshot` / `InstanceStatus` 改成 `Grid*`。
- `AppState` / `Platform` 这类通用容器名不再保留。Task 3 要把它们改成职责名，例如 `GridPlatformService`、`ServerState`、`ServerPlatform`。
- `GridConfig` 里的数量词必须显式编码单位。`long_capacity` / `short_capacity` / `capacity_notional` / `capacity_unit_qty()` 在 Task 1 改成带单位的命名，并同步更新配置文件和测试，不保留双写法。
- `OpenOrder` 与实际语义不符。Task 4 把它改成不预设生命周期阶段的名字，计划默认用 `ExchangeOrder`。
- `PriceTick.last_price` 当前实际承载策略参考价。Task 4 把它改成 `reference_price`，并在 Binance 适配层明确“当前来源是 mark price”；如果还需要原始 `mark_price`，字段继续保留为显式来源名。

---

## File Structure

### 新建文件

```text
engine/src/key.rs                  # GridId：当前一等网格身份，先包住 symbol
server/src/application.rs         # GridPlatformService：应用事务边界
protocol/Cargo.toml               # 共享协议 crate
protocol/src/lib.rs               # 对外导出 DTO 与事件
```

### 修改文件

- `Cargo.toml`：把 `poise-protocol` 加入 workspace members
- `core/src/strategy.rs`：把容量相关字段和 helper 改成显式单位命名
- `docs/superpowers/specs/2026-03-24-grid-platform-architecture-design.md`：保持目标架构文档与实现计划一致
- `engine/src/lib.rs`：导出 `key`
- `engine/src/manager.rs`：使用 `GridId`，移除未使用的跨层端口依赖
- `engine/src/ports.rs`：把快照存储口升级为状态仓储口
- `engine/src/reconciler.rs`：补齐带外/回带/策略触发事件
- `engine/src/instance.rs`：必要时补充事件判断所需的最小状态
- `server/src/config.rs`：去掉 `instance_id()` 语义，改为 `track_id()`
- `server/src/assembly.rs`：组装 `GridPlatformService`
- `server/src/http.rs`：仅做 HTTP 适配，不直接持有 manager 细节
- `server/src/runtime.rs`：仅做流订阅与调用 service，不直接编排事务
- `server/src/main.rs`：启动时只创建 service 和 transport
- `server/src/websocket.rs`：从 service / event bus 订阅对外事件
- `exchanges/binance/src/adapter.rs`：适配订单模型重命名
- `exchanges/binance/src/types.rs`：适配订单模型重命名
- `exchanges/binance/src/websocket.rs`：把 Binance mark price 明确映射到 `reference_price`
- `storage/src/lib.rs`：暴露状态仓储实现
- `storage/src/schema.rs`：让 `domain_events` 真正服务于当前设计
- `storage/src/sqlite.rs`：原子保存快照 + 事件
- `tui/src/api_client.rs`：改用 `poise-protocol`
- `tui/src/app.rs`：改用共享 DTO；只保留展示态
- `tui/src/main.rs`：改用共享 DTO
- `tui/src/views/dashboard.rs`：适配共享快照结构
- `tui/src/views/instance.rs`：适配共享事件/快照结构
- `tui/src/protocol.rs`：改成对 `poise-protocol` 的 re-export，并保留 fixture 测试

---

### Task 1: 收敛网格身份与核心命名，并清理 engine 泄漏边界

**Files:**
- Create: `engine/src/key.rs`
- Modify: `core/src/strategy.rs`
- Modify: `engine/src/lib.rs`
- Modify: `engine/src/manager.rs`
- Modify: `engine/src/ports.rs`
- Modify: `server/src/config.rs`
- Modify: `server/src/assembly.rs`
- Test: `core/src/strategy.rs`
- Test: `engine/src/manager.rs`
- Test: `server/src/config.rs`
- Test: `server/src/assembly.rs`

- [x] **Step 1: 先写失败测试，锁住“唯一性按 GridId 管理，不再依赖 instance_id 伪概念”**

```rust
#[test]
fn add_grid_rejects_duplicate_track_ids() {
    let key = GridId::from_symbol("BTCUSDT");
    manager.add_grid(key.clone(), "BTCUSDT".into(), config(), budget(), rules()).unwrap();
    let err = manager.add_grid(key, "BTCUSDT".into(), config(), budget(), rules()).unwrap_err();
    assert!(err.to_string().contains("duplicate grid id"));
}
```

同时补 `core` / `server` 测试，验证：
- `track_id()` 由 `symbol` 派生，重复 `symbol` 仍被拒绝
- `GridConfig` 改用带单位字段名后，配置解析和数量换算行为不变

- [x] **Step 2: 运行定向测试确认失败**

Run:

```bash
cargo test -p poise-core strategy::tests::
cargo test -p poise-engine manager::tests::add_grid_rejects_duplicate_track_ids
cargo test -p poise-server config::tests::parses_config_file_with_instances_and_exchange assembly::tests::assemble_rejects_duplicate_symbols
```

Expected: 新测试失败，旧测试按需要调整失败信息。

- [x] **Step 3: 引入名义化 `GridId`，并先完成配置与核心术语的第一轮收敛**

```rust
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GridId(String);

impl GridId {
    pub fn from_symbol(symbol: impl Into<String>) -> Self {
        Self(symbol.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}
```

同时完成：
- `server::config` 里的 `InstanceConfig` 改成 `GridDefinition`，`instance_id()` 改成 `track_id()`
- `GridConfig` 里的 `long_capacity` / `short_capacity` / `capacity_notional` / `capacity_unit_qty()` 改成显式单位命名，例如 `long_exposure_units` / `short_exposure_units` / `notional_per_unit` / `base_qty_per_unit()`
- TOML 配置、测试数据和断言一起改，不保留旧字段兼容

不要把 key 直接展开成裸 `String` 贯穿所有层；server/TUI 仍可对外显示 `symbol`。对外 DTO 的 `Instance*` 改名留到 Task 5，避免在抽共享协议前重复修改两轮。

- [x] **Step 4: 删除 `InstanceManager` 未使用的 `ExchangePort` / `PersistencePort` 持有与 accessor**

`InstanceManager` 只保留真正参与状态计算的依赖；当前应只留下 `ClockPort`。这一步必须和 `GridId` 一起完成，避免后面引入 `GridPlatformService` 时形成“双层持有端口”的新复杂度。

- [x] **Step 5: 跑定向测试并确保现有行为不退化**

Run:

```bash
cargo test -p poise-core
cargo test -p poise-engine manager::tests::
cargo test -p poise-server config::tests:: assembly::tests::
```

Expected: 相关 manager/config/assembly 测试全部通过。

- [x] **Step 6: 提交**

```bash
git add core/src/strategy.rs engine/src/key.rs engine/src/lib.rs engine/src/manager.rs engine/src/ports.rs server/src/config.rs server/src/assembly.rs
git commit -m "refactor: introduce nominal grid id and clean engine ownership"
```

---

### Task 2: 把快照和事件收敛到单一状态仓储接口

**Files:**
- Modify: `engine/src/ports.rs`
- Modify: `storage/src/lib.rs`
- Modify: `storage/src/schema.rs`
- Modify: `storage/src/sqlite.rs`
- Test: `storage/src/sqlite.rs`
- Test: `server/src/http.rs`
- Test: `server/src/runtime.rs`

- [x] **Step 1: 先写失败测试，锁住“快照与领域事件同一事务保存”**

```rust
#[tokio::test]
async fn save_transition_persists_snapshot_and_events_atomically() {
    let repo = SqliteStorage::in_memory().unwrap();
    repo.save_transition(&key(), &snapshot(), &[DomainEvent::BandBreached {
        boundary: BandBoundary::Above,
        price: 120.0,
    }]).await.unwrap();

    let loaded = repo.load_grid_state(&key()).await.unwrap().unwrap();
    let events = repo.list_events(&key()).await.unwrap();
    assert_eq!(loaded.id, key().as_str());
    assert_eq!(events.len(), 1);
}
```

同时补服务端测试，验证保存失败时不会广播事件，也不会留下半写入状态。

- [x] **Step 2: 运行定向测试确认失败**

Run:

```bash
cargo test -p poise-storage sqlite::tests::save_transition_persists_snapshot_and_events_atomically
cargo test -p poise-server runtime::tests::runtime_start_fails_when_buffered_user_data_replay_cannot_be_persisted
```

Expected: 新测试失败，因为当前接口只会存 snapshot。

- [x] **Step 3: 用更深的 `StateRepositoryPort` 取代当前 `PersistencePort`**

```rust
pub struct PersistedGridState {
    pub snapshot: InstanceSnapshot,
    pub events: Vec<DomainEvent>,
}

#[async_trait]
pub trait StateRepositoryPort: Send + Sync {
    async fn save_transition(
        &self,
        key: &GridId,
        snapshot: &InstanceSnapshot,
        events: &[DomainEvent],
    ) -> Result<()>;

    async fn load_grid_state(&self, key: &GridId) -> Result<Option<InstanceSnapshot>>;
    async fn list_events(&self, key: &GridId) -> Result<Vec<DomainEvent>>;
}
```

重点不是换名字，而是把“快照 + 事件”这个设计决策收进仓储边界。

- [x] **Step 4: 在 SQLite 实现中使用事务一次性写入 `instance_snapshots` 和 `domain_events`**

要求：
- 一个 `save_transition()` 只开启一次事务
- 事务失败时 snapshot 和 event 都不能落库
- `domain_events` 记录要带 `track_id`、序列化后的事件和时间戳

- [x] **Step 5: 跑 storage/server 定向测试**

Run:

```bash
cargo test -p poise-storage
cargo test -p poise-server http::tests:: runtime::tests::
```

Expected: storage 全绿；server 原有依赖 persistence 的回滚语义测试继续通过。

- [x] **Step 6: 提交**

```bash
git add engine/src/ports.rs storage/src/lib.rs storage/src/schema.rs storage/src/sqlite.rs server/src/http.rs server/src/runtime.rs
git commit -m "refactor: persist grid snapshot transitions and domain events atomically"
```

---

### Task 3: 引入 `GridPlatformService`，收拢应用事务边界并移除无语义容器名

**Files:**
- Create: `server/src/application.rs`
- Modify: `docs/superpowers/specs/2026-03-24-grid-platform-architecture-design.md`
- Modify: `server/src/assembly.rs`
- Modify: `server/src/http.rs`
- Modify: `server/src/runtime.rs`
- Modify: `server/src/main.rs`
- Modify: `server/src/websocket.rs`
- Test: `server/src/http.rs`
- Test: `server/src/runtime.rs`
- Test: `server/src/main.rs`

- [x] **Step 1: 先写失败测试，锁住“HTTP 与 runtime 只通过 service 操作”**

新增测试至少覆盖：
- `pause/resume` 通过 service 触发并保存状态
- `PriceTick` 和 `UserDataEvent` 都经过同一事务路径
- 事件只在保存成功后发布

- [x] **Step 2: 运行定向测试确认失败**

Run:

```bash
cargo test -p poise-server http::tests:: runtime::tests:: tests::startup_flow_serves_instances_and_snapshots
```

Expected: 新增 service 相关测试失败，旧测试中会暴露 `AppState` 结构变化。

- [x] **Step 3: 实现最小 `GridPlatformService` 接口，不做 pass-through 包装**

```rust
pub struct GridPlatformService { /* manager, repository, event bus, exchange */ }

impl GridPlatformService {
    pub async fn list_grids(&self) -> Vec<GridSummaryInternal>;
    pub async fn snapshot(&self, id: &GridId) -> Result<GridSnapshotInternal>;
    pub async fn pause(&self, id: &GridId) -> Result<()>;
    pub async fn resume(&self, id: &GridId) -> Result<()>;
    pub async fn apply_price_tick(&self, tick: &PriceTick) -> Result<()>;
    pub async fn apply_user_event(&self, event: UserDataEvent) -> Result<()>;
}
```

同时完成服务端顶层命名修正：
- `AppState` 改成 `ServerState` 或 `HttpState`
- `Platform` 改成 `ServerPlatform` 或 `AssemblyResult`

transport 层状态只持有 service（以及必要的 ws 订阅入口），不要再把 `manager` / `mutation_lock` / repository 细节直接暴露给 transport。

- [x] **Step 4: 把 `mutate_instance_and_persist()`、symbol 路由和事件广播迁入 service**

具体要求：
- 从 `assembly.rs` 移走事务编排函数
- 从 `runtime.rs` 移走 `instance_id_for_symbol()` / `reconcile_context_for_symbol()` 这类路由细节
- `http.rs` 不再直接读写 manager

- [x] **Step 5: 同步 spec 里的服务端边界描述**

要求：
- `poise-server` 文件结构包含 `application.rs` 和 `runtime.rs`
- 装配产物、transport state、应用服务的命名与实现一致
- 整体数据流明确由 `GridPlatformService` 拥有 mutate / persist / publish

- [x] **Step 6: 跑 server 定向测试**

Run:

```bash
cargo test -p poise-server
```

Expected: server 全部测试通过，行为不变但边界更清晰。

- [x] **Step 7: 提交**

```bash
git add docs/superpowers/specs/2026-03-24-grid-platform-architecture-design.md server/src/application.rs server/src/assembly.rs server/src/http.rs server/src/runtime.rs server/src/main.rs server/src/websocket.rs
git commit -m "refactor(server): introduce grid platform service"
```

---

### Task 4: 补齐领域事件语义，并修正订单与价格模型命名

**Files:**
- Modify: `core/src/events.rs`
- Modify: `engine/src/reconciler.rs`
- Modify: `engine/src/instance.rs`
- Modify: `engine/src/ports.rs`
- Modify: `exchanges/binance/src/adapter.rs`
- Modify: `exchanges/binance/src/types.rs`
- Modify: `exchanges/binance/src/websocket.rs`
- Modify: `server/src/application.rs`
- Test: `engine/src/reconciler.rs`
- Test: `engine/src/ports.rs`
- Test: `exchanges/binance/src/websocket.rs`
- Test: `server/src/runtime.rs`

- [x] **Step 1: 先写失败测试，覆盖当前未形成闭环的事件**

至少补下面几类：
- `BandBreached`
- `BandReentered`
- `PolicyTriggered`

```rust
#[test]
fn reconcile_emits_band_breached_and_policy_triggered_when_price_leaves_band() {
    let result = reconcile(&active_instance(), 120.0, &budget());
    assert!(result.plan.events.iter().any(|e| matches!(e, DomainEvent::BandBreached { .. })));
    assert!(result.plan.events.iter().any(|e| matches!(e, DomainEvent::PolicyTriggered { .. })));
}
```

- [x] **Step 2: 运行定向测试确认失败**

Run:

```bash
cargo test -p poise-engine reconciler::tests::
```

Expected: 新增事件测试失败，因为当前只发 `ExposureTargetChanged` / `RiskCapApplied` / `RiskDenied`。

- [x] **Step 3: 在最小必要状态上补齐边界事件**

原则：
- 不为了发事件引入大块新状态
- 只补足判断“刚出带 / 刚回带 / 触发何种策略”所需的最小信息
- 事件由 `engine` 产生，发布和持久化由 `GridPlatformService` 负责

- [x] **Step 4: 同步修正订单与价格模型的语义命名**

要求：
- `OpenOrder` 改成不预设生命周期阶段的名字，计划默认用 `ExchangeOrder`
- `PriceTick.last_price` 改成 `reference_price`
- Binance websocket 适配层明确把 mark price 映射到 `reference_price`
- 如果保留 `mark_price` 字段，它只表达原始来源，不再兼任策略输入语义

- [x] **Step 5: 确保保存和广播的事件集一致**

service 只广播已经进入 `save_transition()` 的事件，不允许“WS 收到但仓储没有”的分叉语义。

- [x] **Step 6: 跑 engine/binance/server 相关测试**

Run:

```bash
cargo test -p poise-engine
cargo test -p poise-binance
cargo test -p poise-server runtime::tests::
```

Expected: 事件相关测试全部通过，现有风险/目标变化测试不回归。

- [x] **Step 7: 提交**

```bash
git add core/src/events.rs engine/src/instance.rs engine/src/ports.rs engine/src/reconciler.rs exchanges/binance/src/adapter.rs exchanges/binance/src/types.rs exchanges/binance/src/websocket.rs server/src/application.rs server/src/runtime.rs
git commit -m "feat(engine): complete domain event emission semantics"
```

---

### Task 5: 抽出 `poise-protocol`，让 server 和 TUI 共用外部契约并完成对外 `Grid` 命名

**Files:**
- Create: `poise-protocol/Cargo.toml`
- Create: `poise-protocol/src/lib.rs`
- Create: `poise-protocol/src/grid.rs`
- Create: `poise-protocol/src/event.rs`
- Modify: `Cargo.toml`
- Modify: `server/src/http.rs`
- Modify: `server/src/websocket.rs`
- Modify: `tui/src/api_client.rs`
- Modify: `tui/src/app.rs`
- Modify: `tui/src/main.rs`
- Modify: `tui/src/views/dashboard.rs`
- Modify: `tui/src/views/instance.rs`
- Delete: `tui/src/protocol.rs`
- Test: `server/src/http.rs`
- Test: `server/src/websocket.rs`
- Test: `tui/src/api_client.rs`
- Test: `tui/src/app.rs`
- Test: `tui/src/main.rs`

- [x] **Step 1: 先写失败测试，锁住 wire format 兼容性**

在新 crate 里补 `serde` round-trip 测试，覆盖：
- `GridSummary`
- `GridSnapshot`
- `CommandRequest` / `CommandResponse`
- `WsEvent`

保留现有 snake_case 表现，避免 server 与 TUI 协议悄悄变化。

- [x] **Step 2: 运行定向测试确认失败**

Run:

```bash
cargo test -p poise-server http::tests:: websocket::tests::
cargo test -p poise-tui api_client::tests:: protocol::tests::
```

Expected: 旧测试在协议模块迁移前失败或无法编译。

- [x] **Step 3: 在 `poise-protocol` 里定义客户端 DTO，而不是复用内部 engine/core 类型**

```rust
pub struct GridSummary { ... }
pub struct GridSnapshot { ... }
pub struct CommandRequest { ... }
pub struct CommandResponse { ... }
pub enum GridStatus { ... }
pub enum DomainEventDto { ... }
pub struct WsEvent { ... }
```

注意：
- `BandState` 如果仍只是展示派生值，可以留在 `tui`，不要为了“完全共享”把展示逻辑反推回协议层
- server 负责 internal -> DTO 的转换
- `InstanceSummary` / `InstanceSnapshot` / `InstanceStatus` 在这一任务中统一删除，外部协议只保留 `Grid*`

- [x] **Step 4: 迁移 server 和 TUI 到共享协议 crate**

要求：
- `server/src/http.rs` / `websocket.rs` 只输出 `poise-protocol` DTO
- `tui` 删除本地协议重复定义
- `server` / `tui` / fixture / 端到端测试全部改用 `Grid*` 命名
- 如需过渡，允许先把 `tui/src/protocol.rs` 改成短暂 re-export，再在同任务末尾删除

- [x] **Step 5: 跑 server/tui 定向与端到端测试**

Run:

```bash
cargo test -p poise-server
cargo test -p poise-tui
```

Expected: HTTP、WS、TUI 交互测试全绿，wire format 保持兼容。

- [x] **Step 6: 提交**

```bash
git add Cargo.toml poise-protocol server/src/http.rs server/src/websocket.rs tui/src/api_client.rs tui/src/app.rs tui/src/main.rs tui/src/views/dashboard.rs tui/src/views/instance.rs
git rm tui/src/protocol.rs
git commit -m "refactor: extract shared grid protocol crate"
```

---

### Task 6: 全量验证、清理和计划勾选同步

**Files:**
- Modify: `docs/superpowers/plans/2026-03-25-grid-platform-architecture-convergence.md`
- Modify: `docs/superpowers/specs/2026-03-24-grid-platform-architecture-design.md`

- [x] **Step 1: 跑全量测试**

Run:

```bash
cargo test
```

Expected: workspace 全绿。

- [x] **Step 2: 做最小清理**

要求：
- 删除不再使用的 helper、重复 DTO、死 accessor
- 不恢复用户当前已删除的文档文件
- 不引入过渡兼容层长期保留

- [x] **Step 3: 同步任务清单和架构文档**

把本计划中已完成的 checkbox 勾上；如果执行过程中新增必须任务，直接补进本计划，不另开临时 TODO。若最终实现把接口或命名进一步细化，也要同步更新 spec，而不是只改计划。

- [x] **Step 4: 提交**

```bash
git add docs/superpowers/plans/2026-03-25-grid-platform-architecture-convergence.md docs/superpowers/specs/2026-03-24-grid-platform-architecture-design.md
git commit -m "docs: finalize architecture convergence plan progress"
```

---

## 验收标准

- `GridId` 成为当前统一网格身份，`instance_id` 伪概念不再继续扩散
- `GridConfig` 及相关配置字段带有显式单位命名，不再混用抽象 `capacity`
- `InstanceManager` 不再持有未使用的跨层端口
- `server` transport 层不直接编排 mutate/persist/publish
- `AppState` / `Platform` 这类无语义容器名从服务端主路径移除
- 快照与领域事件通过单一仓储接口原子保存
- `BandBreached` / `BandReentered` / `PolicyTriggered` 形成真实发射路径
- `OpenOrder` / `last_price` 这类与真实语义不符的名字被替换为准确术语
- `server` 与 `tui` 使用同一套共享协议 DTO，外部 `Instance*` 命名完成迁移到 `Grid*`
- `cargo test` 全绿
