# 网格平台第二阶段应用投影边界实现计划

> **执行说明：** 实施本计划时，优先使用 `subagent-driven-development`（推荐）或 `executing-plans`，按 task 逐项推进，并用 `- [ ]` 复选框跟踪状态。

**目标：** 为网格平台引入轻量双轨应用层，拆分写侧、查询和投影边界，并将 HTTP / WebSocket / TUI 全量切换到新的稳定读模型 contract。

**架构：** 保持第一阶段已经完成的写侧事务边界和 effect outbox，不引入 `CommandBus` 或独立 `ProjectionStore`。第二阶段通过 `GridWriteService + GridQueryService + GridProjector` 重画 `server` 上层边界，`storage` 额外提供只读事实查询端口，`protocol` 重画成 list/detail/stream update 读模型，TUI 改为只消费新读模型，不再依赖 `GridSnapshot` 与原始 `DomainEvent`。

**技术栈：** Rust workspace, tokio, axum, rusqlite, serde, reqwest, tokio-tungstenite, ratatui

---

## 文件结构

### 新建文件

```text
server/src/write_service.rs                    # 写侧应用服务，只处理命令、observation、事务提交和内部通知
server/src/query_service.rs                    # 查询服务，负责聚合 projector 输入
server/src/projector.rs                        # 把内部事实投影成 GridListItemView / GridDetailView
server/src/notifications.rs                    # 内部通知类型与广播入口
tui/tests/fixtures/grid_list_response.json     # 新的列表响应 fixture
tui/tests/fixtures/grid_detail_view.json       # 新的详情响应 fixture
tui/tests/fixtures/ws_grid_list_item_changed.json
tui/tests/fixtures/ws_grid_detail_changed.json
```

### 删除文件

```text
server/src/application.rs                      # 旧的 GridPlatformService，职责过厚，第二阶段直接删除
```

### 修改文件

- `protocol/src/lib.rs`：定义新的读模型 DTO、typed command DTO、WebSocket 流式 envelope
- `engine/src/ports.rs`：新增查询侧只读仓储端口与带时间戳的已存储事件模型
- `storage/src/sqlite.rs`：实现只读查询端口，返回快照、最近事件、最近 effect
- `server/src/assembly.rs`：装配 write/query/projector/notifications 四个边界
- `server/src/http.rs`：从 `/grids/:id/snapshot` 切换到 `/grids/:id`，并改 typed command
- `server/src/websocket.rs`：从领域事件广播切换到读模型更新推送
- `server/src/runtime.rs`：改为只依赖写侧服务与内部目录能力，不再依赖旧应用总服务
- `server/src/effect_worker.rs`：改为通过 `GridWriteService` 回流状态，并发内部通知
- `server/src/main.rs`：更新模块声明和启动流程测试
- `tui/src/protocol.rs`：更新协议反序列化测试和辅助类型
- `tui/src/api_client.rs`：改成读取 `GridListResponse` / `GridDetailView` / `GridStreamEvent`
- `tui/src/app.rs`：改成缓存列表项与详情，不再缓存 `GridSnapshot` 与 `WsEvent`
- `tui/src/main.rs`：启动时改为“拉列表 + 拉当前详情”，WS 直接应用 list/detail 更新
- `tui/src/views/dashboard.rs`：改为渲染 `GridListItemView`
- `tui/src/views/instance.rs`：改为渲染 `GridDetailView`
- `tui/src/input.rs`：更新测试数据和命令可用性联动
- `docs/protocol-contract.md`：同步新 HTTP / WebSocket 协议
- `docs/superpowers/specs/2026-03-26-grid-phase2-application-projection-design.md`：如实现偏离设计，回写 spec
- `docs/superpowers/plans/2026-03-26-grid-phase2-application-projection.md`：执行后更新勾选状态

---

### 任务 1：重画协议 DTO，并先锁住新 contract

**Files:**
- Modify: `protocol/src/lib.rs`
- Modify: `tui/src/protocol.rs`
- Create: `tui/tests/fixtures/grid_list_response.json`
- Create: `tui/tests/fixtures/grid_detail_view.json`
- Create: `tui/tests/fixtures/ws_grid_list_item_changed.json`
- Create: `tui/tests/fixtures/ws_grid_detail_changed.json`

- [x] **Step 1: 先写失败测试，锁住 list/detail/stream 三类 payload**

在 `tui/src/protocol.rs` 先新增这些测试：

```rust
#[test]
fn deserializes_grid_list_response() {
    let response: GridListResponse =
        serde_json::from_str(include_str!("../tests/fixtures/grid_list_response.json")).unwrap();

    assert_eq!(response.items.len(), 1);
    assert_eq!(response.items[0].id, "btc-core");
    assert_eq!(response.items[0].instrument.symbol, "BTCUSDT");
}

#[test]
fn deserializes_grid_detail_view() {
    let detail: GridDetailView =
        serde_json::from_str(include_str!("../tests/fixtures/grid_detail_view.json")).unwrap();

    assert_eq!(detail.identity.id, "btc-core");
    assert_eq!(detail.execution.state, ExecutionStateView::Open);
    assert!(!detail.available_commands.is_empty());
}

#[test]
fn deserializes_grid_stream_detail_changed() {
    let event: GridStreamEvent = serde_json::from_str(
        include_str!("../tests/fixtures/ws_grid_detail_changed.json"),
    )
    .unwrap();

    assert_eq!(event.grid_id, "btc-core");
    assert!(matches!(event.payload, GridStreamPayload::GridDetailChanged { .. }));
}
```

同时删除或改写旧的 `GridSnapshot` / `WsEvent` fixture 测试，不再保留兼容层。

- [x] **Step 2: 运行定向测试，确认现有协议无法通过**

Run:

```bash
cargo test -p grid-tui protocol::tests::deserializes_grid_list_response -- --exact
cargo test -p grid-tui protocol::tests::deserializes_grid_detail_view -- --exact
cargo test -p grid-tui protocol::tests::deserializes_grid_stream_detail_changed -- --exact
```

Expected:

- 因为当前 `protocol/src/lib.rs` 仍定义 `GridSummary` / `GridSnapshot` / `WsEvent`
- 编译失败或测试找不到新类型

- [x] **Step 3: 最小实现新的 protocol DTO**

在 `protocol/src/lib.rs` 引入并导出：

```rust
pub struct GridListResponse {
    pub items: Vec<GridListItemView>,
}

pub struct GridListItemView {
    pub id: String,
    pub instrument: InstrumentView,
    pub lifecycle: GridLifecycleView,
    pub reference_price: Option<f64>,
    pub exposure: ExposureSummaryView,
    pub execution: ExecutionBadgeView,
}

pub struct GridDetailView {
    pub identity: GridIdentityView,
    pub status: GridStatusPanelView,
    pub strategy: GridStrategyView,
    pub market: GridMarketView,
    pub position: GridPositionView,
    pub execution: GridExecutionView,
    pub activity: Vec<GridActivityItemView>,
    pub available_commands: Vec<GridCommandView>,
}

pub struct GridCommandRequest {
    pub command: GridCommandType,
}

pub enum GridStreamPayload {
    GridListItemChanged { item: GridListItemView },
    GridDetailChanged { detail: GridDetailView },
}
```

实现要求：

- DTO 名字直接贴合 spec，不再混用 `snapshot`
- `instrument` 对外明确为 `{ venue, symbol }`
- `available_commands` 直接表达可执行命令和禁用原因
- 删除旧 `CommandRequest { command: String }` 与旧 `WsEvent`

再补齐四个 fixture 文件，对齐新 payload。

- [x] **Step 4: 跑 `grid-tui` 协议测试，确认新 DTO 固定下来**

Run:

```bash
cargo test -p grid-tui protocol::tests::
```

Expected: `tui/src/protocol.rs` 的新反序列化测试全绿，旧 snapshot/ws event 测试已移除或改写完成。

- [x] **Step 5: 提交**

```bash
git add protocol/src/lib.rs tui/src/protocol.rs tui/tests/fixtures/grid_list_response.json tui/tests/fixtures/grid_detail_view.json tui/tests/fixtures/ws_grid_list_item_changed.json tui/tests/fixtures/ws_grid_detail_changed.json
git commit -m "refactor: define phase two grid protocol views"
```

---

### 任务 2：为查询侧引入只读仓储端口和带时间戳的事实模型

**Files:**
- Modify: `engine/src/ports.rs`
- Modify: `storage/src/sqlite.rs`
- Test: `storage/src/sqlite.rs`

- [x] **Step 1: 先写失败测试，锁住查询侧能读到有时间的事件和 effect**

在 `storage/src/sqlite.rs` 新增测试，至少覆盖：

```rust
#[tokio::test]
async fn list_recent_grid_events_returns_timestamped_records_in_order() {
    let storage = SqliteStorage::in_memory().unwrap();
    persist_two_events_for("btc-core", &storage).await;

    let events = storage
        .list_recent_grid_events(&GridId::new("btc-core"), 10)
        .await
        .unwrap();

    assert_eq!(events.len(), 2);
    assert!(events[0].created_at <= events[1].created_at);
}

#[tokio::test]
async fn list_recent_grid_effects_filters_by_grid_id_and_limit() {
    let storage = SqliteStorage::in_memory().unwrap();
    persist_effect_batches_for_two_grids(&storage).await;

    let effects = storage
        .list_recent_grid_effects(&GridId::new("btc-core"), 1)
        .await
        .unwrap();

    assert_eq!(effects.len(), 1);
    assert_eq!(effects[0].grid_id.as_str(), "btc-core");
}
```

- [x] **Step 2: 运行定向测试，确认仓储还没有查询端口**

Run:

```bash
cargo test -p grid-storage sqlite::tests::list_recent_grid_events_returns_timestamped_records_in_order -- --exact
cargo test -p grid-storage sqlite::tests::list_recent_grid_effects_filters_by_grid_id_and_limit -- --exact
```

Expected: 当前 `engine/src/ports.rs` 和 `storage/src/sqlite.rs` 没有这些接口，测试失败。

- [x] **Step 3: 最小实现 `GridReadRepositoryPort` 和 `StoredDomainEvent`**

在 `engine/src/ports.rs` 新增：

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredDomainEvent {
    pub id: i64,
    pub grid_id: GridId,
    pub event: DomainEvent,
    pub created_at: DateTime<Utc>,
}

#[async_trait]
pub trait GridReadRepositoryPort: Send + Sync {
    async fn list_grid_snapshots(&self) -> Result<Vec<GridRuntimeSnapshot>>;
    async fn load_grid_snapshot(&self, grid_id: &GridId) -> Result<Option<GridRuntimeSnapshot>>;
    async fn list_recent_grid_events(
        &self,
        grid_id: &GridId,
        limit: usize,
    ) -> Result<Vec<StoredDomainEvent>>;
    async fn list_recent_grid_effects(
        &self,
        grid_id: &GridId,
        limit: usize,
    ) -> Result<Vec<PersistedGridEffect>>;
}
```

在 `storage/src/sqlite.rs`：

- 解析 `domain_events.id`、`created_at`
- 实现最近事件和最近 effect 的 limit 查询
- 复用现有 `grid_snapshots`、`domain_events`、`grid_effects` 表，不新增 schema

- [x] **Step 4: 跑 `grid-storage` 定向和全量测试**

Run:

```bash
cargo test -p grid-storage sqlite::tests::list_recent_grid_events_returns_timestamped_records_in_order -- --exact
cargo test -p grid-storage sqlite::tests::list_recent_grid_effects_filters_by_grid_id_and_limit -- --exact
cargo test -p grid-storage
```

Expected: 新增测试通过，`grid-storage` 全绿。

本轮 code review 之后，已额外补上两类回归覆盖：

- `list_recent_grid_effects_orders_results_by_updated_at`
- `list_recent_grid_effects_includes_status_updated_effect_in_recent_window`

- [x] **Step 5: 提交**

```bash
git add engine/src/ports.rs storage/src/sqlite.rs
git commit -m "refactor: add grid read repository queries"
```

---

### 任务 3：引入查询服务和 projector，先锁住读模型投影规则

**Files:**
- Create: `server/src/query_service.rs`
- Create: `server/src/projector.rs`
- Test: `server/src/query_service.rs`
- Test: `server/src/projector.rs`

- [x] **Step 1: 先写失败测试，锁住列表项、详情和活动流投影**

在 `server/src/projector.rs` 新增测试，至少覆盖：

```rust
#[test]
fn project_list_item_summarizes_execution_state() {
    let source = source_with_submitting_effect();
    let item = GridProjector::new().project_list_item(&source);

    assert_eq!(item.id, "btc-core");
    assert_eq!(item.execution.state, ExecutionBadgeStateView::Busy);
}

#[test]
fn project_detail_includes_available_commands_and_activity() {
    let source = source_with_failed_effect_and_recent_event();
    let detail = GridProjector::new().project_detail(&source);

    assert!(!detail.available_commands.is_empty());
    assert_eq!(detail.activity.len(), 2);
    assert!(detail.execution.last_error.is_some());
}
```

在 `server/src/query_service.rs` 新增测试，覆盖：

```rust
#[tokio::test]
async fn list_grid_sources_reads_all_registered_snapshots() {
    let service = test_query_service();
    let sources = service.list_grid_sources().await.unwrap();

    assert!(!sources.is_empty());
    assert_eq!(sources[0].snapshot.grid_id.as_str(), "btc-core");
}

#[tokio::test]
async fn load_detail_source_reads_snapshot_events_and_effects() {
    let service = test_query_service();
    let source = service.load_detail_source(&GridId::new("btc-core")).await.unwrap();

    assert_eq!(source.snapshot.grid_id.as_str(), "btc-core");
    assert!(!source.recent_effects.is_empty());
}
```

- [x] **Step 2: 运行定向测试，确认投影规则还不存在**

Run:

```bash
cargo test -p grid-server projector::tests::project_list_item_summarizes_execution_state -- --exact
cargo test -p grid-server query_service::tests::list_grid_sources_reads_all_registered_snapshots -- --exact
cargo test -p grid-server query_service::tests::load_detail_source_reads_snapshot_events_and_effects -- --exact
```

Expected: 新模块不存在，编译失败。

- [x] **Step 3: 最小实现 `GridReadModelSource`、`GridQueryService` 和 `GridProjector`**

在 `server/src/query_service.rs` 中实现：

```rust
pub struct GridReadModelSource {
    pub snapshot: GridRuntimeSnapshot,
    pub recent_domain_events: Vec<StoredDomainEvent>,
    pub recent_effects: Vec<PersistedGridEffect>,
}

pub struct GridQueryService {
    repository: Arc<dyn GridReadRepositoryPort>,
}
```

在 `server/src/projector.rs` 中实现：

```rust
pub struct GridProjector;

impl GridProjector {
    pub fn project_list_item(&self, source: &GridReadModelSource) -> GridListItemView { /* ... */ }
    pub fn project_detail(&self, source: &GridReadModelSource) -> GridDetailView { /* ... */ }
}
```

实现规则：

- `list_grid_sources()` 用于 `/grids` 列表查询
- `GridExecutionView` 只暴露统一执行摘要，不透出原始 `pending_order`
- `activity` 由 `recent_domain_events + recent_effects` 合并投影
- `available_commands` 由当前 lifecycle 和执行状态直接给出

- [x] **Step 4: 跑 `grid-server` 查询 / 投影定向测试**

Run:

```bash
cargo test -p grid-server projector::tests::
cargo test -p grid-server query_service::tests::
```

Expected: projector 与 query service 新测试通过。

- [ ] **Step 5: 提交**

```bash
git add server/src/query_service.rs server/src/projector.rs
git commit -m "feat: add grid query service and projector"
```

---

### 任务 4：拆掉 `GridPlatformService`，引入新的写侧服务与内部通知

**Files:**
- Create: `server/src/write_service.rs`
- Create: `server/src/notifications.rs`
- Delete: `server/src/application.rs`
- Modify: `server/src/assembly.rs`
- Modify: `server/src/runtime.rs`
- Modify: `server/src/effect_worker.rs`
- Modify: `server/src/main.rs`
- Test: `server/src/write_service.rs`
- Test: `server/src/runtime.rs`
- Test: `server/src/assembly.rs`

- [x] **Step 1: 先写失败测试，锁住写侧只发内部通知，不再组装协议 DTO**

在 `server/src/write_service.rs` 新增测试，覆盖：

```rust
#[tokio::test]
async fn command_persists_transition_and_emits_grid_write_committed() {
    let service = test_write_service();
    let mut receiver = service.subscribe_notifications();

    service
        .command("btc-core", GridCommand::Pause)
        .await
        .unwrap();

    assert!(matches!(
        receiver.recv().await.unwrap(),
        GridInternalNotification::GridWriteCommitted { .. }
    ));
}
```

再在 `server/src/runtime.rs` 保留一个回归测试，例如：

```rust
#[tokio::test]
async fn startup_sync_still_restores_submitting_pending_order_after_service_split() { /* ... */ }
```

- [x] **Step 2: 运行定向测试，确认旧服务边界不满足新要求**

Run:

```bash
cargo test -p grid-server write_service::tests::command_persists_transition_and_emits_grid_write_committed -- --exact
cargo test -p grid-server runtime::tests::startup_sync_preserves_submitting_pending_order_until_exchange_catches_up -- --exact
```

Expected:

- 新的写侧模块不存在
- 旧 `application.rs` 仍绑定 protocol DTO 和 `WsEvent`

- [x] **Step 3: 实现 `GridWriteService` 并删除旧 `GridPlatformService`**

实现要点：

```rust
pub struct GridWriteService {
    manager: SharedManager,
    repository: Arc<dyn StateRepositoryPort>,
    mutation_lock: Arc<Mutex<()>>,
    notifications: broadcast::Sender<GridInternalNotification>,
}
```

要求：

- `command()`、`observe_market()`、`observe_position()`、`observe_order()` 继续存在
- `record_pending_order()`、`clear_pending_order()` 暂时保留给 `effect_worker`
- `grid_instruments()`、`resolve_grid_id()` 暂时保留给 `runtime` 做内部路由，不暴露给 HTTP
- `GridMutationError` 留在 `write_service.rs`，让 `http.rs` 继续做命令错误映射
- 删除 `list_grid_summaries()`、`grid_snapshot()`、protocol mapper、`subscribe_events()`
- `runtime` 和 `effect_worker` 改为依赖 `GridWriteService`
- `assembly` 状态对象改为同时持有 `write_service`、`query_service`、`projector`

- [x] **Step 4: 跑 `grid-server` 写侧和运行时回归测试**

Run:

```bash
cargo test -p grid-server write_service::tests::
cargo test -p grid-server runtime::tests::startup_sync_preserves_submitting_pending_order_until_exchange_catches_up -- --exact
cargo test -p grid-server runtime::tests::effect_worker_does_not_resubmit_when_matching_pending_order_is_already_restored -- --exact
```

Expected: 写侧测试通过，关键运行时恢复语义不回退。

- [x] **Step 5: 提交**

```bash
git add server/src/write_service.rs server/src/notifications.rs server/src/assembly.rs server/src/runtime.rs server/src/effect_worker.rs server/src/main.rs
git rm server/src/application.rs
git commit -m "refactor: split grid write service from application layer"
```

实现备注：

- 当前实现已拆掉旧 `application.rs`，并把运行时 / effect worker 改成只依赖 `GridWriteService`
- 为了把 Task 4 和 Task 5 分开验收，`server/src/http.rs` 与 `server/src/websocket.rs` 仍保留本地 snapshot 映射和 `SnapshotUpdated` 临时桥接；彻底切到新读模型 contract 放在任务 5

---

### 任务 5：重写 HTTP 和 WebSocket，使 transport 只搬运读模型

**Files:**
- Modify: `server/src/http.rs`
- Modify: `server/src/websocket.rs`
- Modify: `server/src/assembly.rs`
- Modify: `server/src/main.rs`
- Test: `server/src/http.rs`
- Test: `server/src/websocket.rs`
- Test: `server/src/main.rs`

- [ ] **Step 1: 先写失败测试，锁住新路由和新 WS payload**

在 `server/src/http.rs` 新增或改写测试：

```rust
#[tokio::test]
async fn list_grids_returns_grid_list_response() {
    let app = router(app_state());
    let response = app
        .oneshot(Request::builder().uri("/grids").body(Body::empty()).unwrap())
        .await
        .unwrap();

    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let payload: GridListResponse = serde_json::from_slice(&body).unwrap();
    assert_eq!(payload.items[0].id, "btc-core");
}

#[tokio::test]
async fn get_grid_detail_returns_projected_detail() { /* ... */ }

#[tokio::test]
async fn submit_command_accepts_typed_command() { /* ... */ }
```

在 `server/src/websocket.rs` 新增测试：

```rust
#[tokio::test]
async fn broadcasts_grid_detail_changed_after_write_commit() { /* ... */ }

#[tokio::test]
async fn broadcasts_grid_list_item_changed_after_effect_state_change() { /* ... */ }
```

- [ ] **Step 2: 运行定向测试，确认 transport 仍在使用旧 contract**

Run:

```bash
cargo test -p grid-server http::tests::list_grids_returns_grid_list_response -- --exact
cargo test -p grid-server websocket::tests::broadcasts_grid_detail_changed_after_write_commit -- --exact
```

Expected:

- `/grids` 仍返回 `Vec<GridSummary>`
- `/grids/:id/snapshot` 仍存在
- WS 仍广播旧 `WsEvent`

- [ ] **Step 3: 最小实现新的 HTTP / WS 适配**

在 `server/src/http.rs`：

- `GET /grids` 返回 `GridListResponse`
- `GET /grids/:id` 返回 `GridDetailView`
- 删除 `/grids/:id/snapshot`
- `POST /grids/:id/commands` 使用 `GridCommandRequest { command: GridCommandType }`

在 `server/src/websocket.rs`：

- 订阅 `GridInternalNotification`
- 收到通知后通过 `GridQueryService + GridProjector` 重新投影
- 对外只推 `GridStreamEvent`

伪代码：

```rust
match notification {
    GridInternalNotification::GridWriteCommitted { grid_id }
    | GridInternalNotification::GridEffectStateChanged { grid_id } => {
        let item = projector.project_list_item(&query.load_list_source(&grid_id).await?);
        let detail = projector.project_detail(&query.load_detail_source(&grid_id).await?);
        send(GridStreamPayload::GridListItemChanged { item });
        send(GridStreamPayload::GridDetailChanged { detail });
    }
}
```

- [ ] **Step 4: 跑 `grid-server` transport 定向测试**

Run:

```bash
cargo test -p grid-server http::tests::
cargo test -p grid-server websocket::tests::
cargo test -p grid-server tests::startup_flow_serves_grid_list_and_detail -- --exact
```

Expected:

- `http.rs` 与 `websocket.rs` 新 contract 测试通过
- `server/src/main.rs` 里的启动流测试改为新路由后通过

- [ ] **Step 5: 提交**

```bash
git add server/src/http.rs server/src/websocket.rs server/src/assembly.rs server/src/main.rs
git commit -m "refactor: serve projected grid views over http and websocket"
```

---

### 任务 6：改造 TUI 的 client 和状态模型，让它只依赖 list/detail 读模型

**Files:**
- Modify: `tui/src/api_client.rs`
- Modify: `tui/src/app.rs`
- Modify: `tui/src/main.rs`
- Modify: `tui/src/input.rs`
- Test: `tui/src/api_client.rs`
- Test: `tui/src/app.rs`
- Test: `tui/src/main.rs`

- [ ] **Step 1: 先写失败测试，锁住 TUI 的新读取方式**

在 `tui/src/api_client.rs` 新增测试：

```rust
#[tokio::test]
async fn list_grids_decodes_grid_list_response() {
    let (base_url, _) = spawn_stub_server().await;
    let client = ApiClient::new(base_url);

    let response = client.list_grids().await.unwrap();
    assert_eq!(response.items[0].id, "btc-core");
}

#[tokio::test]
async fn get_grid_detail_decodes_projected_detail() { /* ... */ }
```

在 `tui/src/app.rs` 新增测试：

```rust
#[test]
fn apply_grid_detail_updates_current_detail_without_snapshot_cache() {
    let mut app = App::new(grid_list_items());
    app.apply_grid_detail(detail_view("btc-core"));

    assert_eq!(app.current_grid_detail().unwrap().identity.id, "btc-core");
}
```

在 `tui/src/main.rs` 新增测试：

```rust
#[tokio::test]
async fn load_initial_state_fetches_list_then_selected_detail() { /* ... */ }
```

- [ ] **Step 2: 运行定向测试，确认 TUI 仍绑定旧 snapshot 模型**

Run:

```bash
cargo test -p grid-tui api_client::tests::list_grids_decodes_grid_list_response -- --exact
cargo test -p grid-tui app::tests::apply_grid_detail_updates_current_detail_without_snapshot_cache -- --exact
cargo test -p grid-tui tests::load_initial_state_fetches_list_then_selected_detail -- --exact
```

Expected: 当前 `ApiClient`、`App`、`main.rs` 还在使用 `GridSummary + GridSnapshot + WsEvent`，测试失败。

- [ ] **Step 3: 最小实现新的 client / app / runtime loop**

实现要求：

- `ApiClient` 改为：

```rust
pub async fn list_grids(&self) -> Result<GridListResponse>;
pub async fn get_grid_detail(&self, id: &str) -> Result<GridDetailView>;
pub async fn submit_command(&self, id: &str, cmd: GridCommandType) -> Result<GridCommandAccepted>;
pub async fn connect_ws(&self) -> Result<mpsc::Receiver<GridStreamEvent>>;
```

- `App` 改为缓存：

```rust
pub grids: Vec<GridListItemView>,
pub current_grid: Option<GridDetailView>,
```

- 删除 `snapshot_cache` 和 `recent_events`
- `load_initial_state()` 改为：
  - 先拉 `/grids`
  - 再拉当前选中 grid 的 `/grids/:id`
- `handle_ws_event()` 直接应用 `GridListItemChanged` / `GridDetailChanged`

- [ ] **Step 4: 跑 `grid-tui` 传输与状态测试**

Run:

```bash
cargo test -p grid-tui api_client::tests::
cargo test -p grid-tui app::tests::
cargo test -p grid-tui tests::load_initial_state_fetches_list_then_selected_detail -- --exact
```

Expected: 新 client、app、main loop 测试通过。

- [ ] **Step 5: 提交**

```bash
git add tui/src/api_client.rs tui/src/app.rs tui/src/main.rs tui/src/input.rs
git commit -m "refactor: make tui consume projected grid views"
```

---

### 任务 7：改造 TUI 视图、更新协议文档，并完成全量验收

**Files:**
- Modify: `tui/src/views/dashboard.rs`
- Modify: `tui/src/views/instance.rs`
- Modify: `docs/protocol-contract.md`
- Modify: `docs/superpowers/specs/2026-03-26-grid-phase2-application-projection-design.md`
- Modify: `docs/superpowers/plans/2026-03-26-grid-phase2-application-projection.md`
- Test: `tui/src/views/dashboard.rs`
- Test: `tui/src/views/instance.rs`

- [ ] **Step 1: 先写失败测试，锁住新列表和详情视图**

在 `tui/src/views/dashboard.rs` 新增或改写测试：

```rust
#[test]
fn renders_dashboard_rows_from_grid_list_items() {
    let app = App::new(vec![grid_list_item("btc-core")]);
    // draw...
    assert!(text.contains("BTCUSDT"));
    assert!(text.contains("busy"));
}
```

在 `tui/src/views/instance.rs` 新增或改写测试：

```rust
#[test]
fn renders_grid_detail_execution_activity_and_commands() {
    let mut app = App::new(vec![grid_list_item("btc-core")]);
    app.apply_grid_detail(detail_view("btc-core"));
    // draw...
    assert!(text.contains("Execution"));
    assert!(text.contains("Recent Activity"));
    assert!(text.contains("pause"));
}
```

- [ ] **Step 2: 运行定向测试，确认视图仍在渲染旧 snapshot / event**

Run:

```bash
cargo test -p grid-tui views::dashboard::tests::renders_dashboard_rows_from_grid_list_items -- --exact
cargo test -p grid-tui views::instance::tests::renders_grid_detail_execution_activity_and_commands -- --exact
```

Expected: 旧视图仍读取 `GridSnapshot`、`pending_order` 和 `WsEvent`，测试失败。

- [ ] **Step 3: 最小实现视图切换和文档更新**

实现要点：

- `dashboard.rs` 直接用 `GridListItemView.exposure` 和 `execution`
- `instance.rs` 直接渲染 `GridDetailView.status / strategy / execution / activity / available_commands`
- 不再显示“pending order 原样字段”
- `docs/protocol-contract.md` 更新为：
  - `GET /grids`
  - `GET /grids/:id`
  - `POST /grids/:id/commands`
  - `GridStreamEvent`

如果实际实现与 spec 的字段名或块结构有偏差，回写 spec。

- [ ] **Step 4: 跑模块级和工作区全量验收**

Run:

```bash
cargo test -p grid-server
cargo test -p grid-tui
cargo test
```

Expected:

- `grid-server` 全绿
- `grid-tui` 全绿
- 工作区全绿

- [ ] **Step 5: 同步任务清单并提交**

把本计划已完成项改成 `- [x]`，然后：

```bash
git add tui/src/views/dashboard.rs tui/src/views/instance.rs docs/protocol-contract.md docs/superpowers/specs/2026-03-26-grid-phase2-application-projection-design.md docs/superpowers/plans/2026-03-26-grid-phase2-application-projection.md
git commit -m "refactor: switch grid ui to projected application views"
```

---

## Acceptance Checklist

- [ ] `GridPlatformService` 已删除，`server` 顶层边界变成 `write/query/projector/notifications`
- [ ] `protocol` 不再暴露 `GridSnapshot`、原始 `PendingOrder`、原始 `DomainEvent`
- [ ] `storage` 提供只读查询端口，query 侧不再复用写侧事务接口
- [ ] `GET /grids` 返回 `GridListResponse`
- [ ] `GET /grids/:id` 返回 `GridDetailView`
- [ ] `POST /grids/:id/commands` 使用 typed command
- [ ] WebSocket 只推 `GridListItemChanged` / `GridDetailChanged`
- [ ] TUI 启动流程改为“拉列表 + 拉当前详情”
- [ ] TUI 视图只依赖新读模型
- [ ] `cargo test -p grid-server` 通过
- [ ] `cargo test -p grid-tui` 通过
- [ ] `cargo test` 通过
