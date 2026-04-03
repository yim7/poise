# 账户监控面板 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在 `Dashboard` 顶部增加账户监控面板，服务端提供账户摘要读取和实时推送，TUI 能稳定展示 `equity / available / unrealized pnl / day change / risk signal`。

**Architecture:** 这次实现分四段推进。先统一协议和通知外壳，把账户事件接进现有实时链路；再补账户监控的配置、交易所适配、server 内部读模型和持久化边界；随后把 `AccountMonitor` 接到 server 启动、轮询、HTTP 与 WebSocket；最后再让 TUI 按新启动顺序加载并渲染账户区块。`AccountMonitor` 是唯一拥有抓取、基准值、风险规则、diff、持久化和通知条件的深模块；`runtime` 只负责任务生命周期和定时触发，HTTP / WebSocket 只通过 `AccountProjector` 投影协议 DTO，不新增独立 `AccountQueryService`。

**Tech Stack:** Rust workspace, Cargo, Tokio, Axum, Ratatui, Reqwest, Rusqlite, Serde, Binance Futures REST

---

## File Structure

### 新增文件

- `server/src/account_monitor.rs`：账户摘要状态、单次刷新、基准维护、风险计算、diff 与通知条件
- `server/src/account_monitor_store.rs`：server 本地账户监控持久化接口和 SQLite 适配
- `server/src/account_read_model.rs`：账户监控内部读模型，不依赖协议 DTO
- `server/src/account_projector.rs`：把 `AccountReadModel` 投影成 `AccountSummaryView`
- `tui/src/views/account_panel.rs`：账户区块渲染，负责两行摘要文本和信号样式
- `tui/tests/fixtures/account_summary_view.json`：账户摘要 HTTP fixture
- `tui/tests/fixtures/ws_account_summary_changed.json`：账户摘要 WebSocket fixture

### 重点修改文件

- `protocol/src/lib.rs`：新增 `AccountSummaryView`、`RiskSignalView`、统一实时事件 `StreamEvent`
- `engine/src/ports.rs`：只新增账户摘要交易所读取 DTO 和 `ExchangePort::get_account_summary()`
- `exchanges/binance/src/types.rs`：解析 Binance 账户摘要响应
- `exchanges/binance/src/rest.rs`：调用账户摘要 REST 接口
- `exchanges/binance/src/adapter.rs`：把 REST 账户摘要能力接到 `ExchangePort`
- `storage/src/schema.rs`：新增 `account_monitor_state` 表
- `storage/src/sqlite.rs`：实现 SQLite 账户监控状态原始读写能力
- `server/src/config.rs`：新增 `[account_monitor]` 默认值与阈值校验
- `server/src/account_monitor_store.rs`：定义 `StoredAccountMonitorState` 和 server 本地 store trait
- `server/src/notifications.rs`：把内部通知升级为统一 `ServerNotification`
- `server/src/write_service.rs`：改用统一通知类型
- `server/src/assembly.rs`：装配 `AccountMonitor` 并挂到 `ServerState`
- `server/src/http.rs`：新增 `GET /account`
- `server/src/runtime.rs`：新增账户轮询任务和 shutdown handle，但不拥有账户摘要业务规则
- `server/src/websocket.rs`：按统一事件外壳投影 `AccountSummaryChanged`
- `server/src/main.rs`：注册新模块并适配新的 runtime handle
- `server/src/state_bootstrap.rs`：让准备好的仓库装配出 track 仓库和账户监控 store
- `tui/src/protocol.rs`：重新导出协议层新增 DTO
- `tui/src/api_client.rs`：新增 `get_account_summary()`，并改用统一 `StreamEvent`
- `tui/src/app.rs`：新增账户摘要状态与应用方法
- `tui/src/main.rs`：调整启动加载顺序、同步逻辑和账户事件处理
- `tui/src/views/dashboard.rs`：将 `Dashboard` 拆成账户区块 + 轨道表格
- `tui/src/views/mod.rs`：注册账户区块子视图

### 测试落点

- `protocol/src/lib.rs`
- `exchanges/binance/src/types.rs`
- `storage/src/schema.rs`
- `storage/src/sqlite.rs`
- `server/src/config.rs`
- `server/src/account_monitor_store.rs`
- `server/src/account_monitor.rs`
- `server/src/account_projector.rs`
- `server/src/http.rs`
- `server/src/runtime.rs`
- `server/src/websocket.rs`
- `tui/src/api_client.rs`
- `tui/src/app.rs`
- `tui/src/views/dashboard.rs`
- `tui/src/main.rs`

### 实施约束

- 每个 task 先按 `@superpowers/test-driven-development` 写失败测试，再写实现
- 每个 task 验收通过后必须立即 `git add`、`git commit`，并把 commit SHA 回写到本计划
- 未完成当前 task 的提交和计划回写，不得开始下一个 task
- task 完成前按 `@superpowers/verification-before-completion` 跑对应回归
- 允许直接演进现有协议和 WebSocket 外壳，不保留旧的 `TrackStreamEvent`

---

### Task 1: 统一协议与通知外壳

**Files:**
- Modify: `protocol/src/lib.rs`
- Modify: `server/src/notifications.rs`
- Modify: `server/src/write_service.rs`
- Modify: `server/src/websocket.rs`
- Modify: `tui/src/protocol.rs`
- Modify: `tui/src/api_client.rs`
- Modify: `tui/src/app.rs`
- Test: `cargo test -p poise-protocol deserializes_account_summary_changed_stream_event -- --nocapture`
- Test: `cargo test -p poise-server websocket::tests::broadcasts_track_events_with_stream_event_envelope -- --nocapture`
- Test: `cargo test -p poise-tui app::tests::apply_account_summary_event_updates_state -- --nocapture`

- [x] **Step 1: 先写失败测试，固定新的协议壳和内部通知壳**

要求：
- 在 `protocol/src/lib.rs` 增加序列化测试，固定新的事件模型：

```rust
pub enum StreamEvent {
    TrackListItemChanged {
        track_id: String,
        item: TrackListItemView,
    },
    TrackDetailChanged {
        track_id: String,
        detail: Box<TrackDetailView>,
    },
    AccountSummaryChanged {
        summary: AccountSummaryView,
    },
}
```

- 在 `server/src/websocket.rs` 固定 track 事件也必须走 `StreamEvent`
- 在 `tui/src/app.rs` 固定账户事件进入后会写入 `App::account_summary`

- [x] **Step 2: 运行定向测试，确认当前代码还停留在旧壳上**

Run:
`cargo test -p poise-protocol deserializes_account_summary_changed_stream_event -- --nocapture`
`cargo test -p poise-server websocket::tests::broadcasts_track_events_with_stream_event_envelope -- --nocapture`
`cargo test -p poise-tui app::tests::apply_account_summary_event_updates_state -- --nocapture`

Expected:
- 测试失败或编译失败
- 失败原因明确指向：
  - 协议层仍只有 `TrackStreamEvent`
  - server 内部仍只有 `TrackInternalNotification`
  - TUI `App` 尚无账户摘要状态

- [x] **Step 3: 实现统一事件外壳和账户摘要 DTO 骨架**

要求：
- 在 `protocol/src/lib.rs` 新增：
  - `RiskSignalView`
  - `AccountSummaryView`
  - `StreamEvent`
- 删除或替换旧的 `TrackStreamEvent` / `TrackStreamPayload`
- 在 `server/src/notifications.rs` 定义统一 `ServerNotification`：

```rust
pub enum ServerNotification {
    TrackChanged { track_id: TrackId },
    AccountChanged,
}
```

- `TrackWriteService` 改用 `ServerNotification`
- `server/src/websocket.rs` 先把现有 track 推送切到 `StreamEvent`
- `tui/src/protocol.rs`、`tui/src/api_client.rs`、`tui/src/app.rs` 跟着升级到 `StreamEvent`
- 在 `App` 里先增加：
  - `account_summary: Option<AccountSummaryView>`
  - `apply_account_summary(summary)`

- [x] **Step 4: 跑定向回归，确认统一事件壳稳定**

Run:
`cargo test -p poise-protocol`
`cargo test -p poise-server websocket::tests::broadcasts_track_events_with_stream_event_envelope -- --nocapture`
`cargo test -p poise-tui app::tests::apply_account_summary_event_updates_state -- --nocapture`

Expected:
- `poise-protocol` 全量测试通过
- server websocket 测试通过，track 事件已改用统一外壳
- TUI 能正确解码并应用账户事件，即使此时还没有 `/account` HTTP 接口

- [x] **Step 5: 提交并回写 SHA**

```bash
git add protocol/src/lib.rs server/src/notifications.rs server/src/write_service.rs server/src/websocket.rs tui/src/protocol.rs tui/src/api_client.rs tui/src/app.rs
git commit -m "refactor: unify stream and notification envelopes"
```

Task 1 code commit:
`aff8afa`

---

### Task 2: 建立账户监控配置、内部读模型和持久化边界

**Files:**
- Modify: `server/src/config.rs`
- Modify: `engine/src/ports.rs`
- Modify: `exchanges/binance/src/types.rs`
- Modify: `exchanges/binance/src/rest.rs`
- Modify: `exchanges/binance/src/adapter.rs`
- Modify: `storage/src/schema.rs`
- Modify: `storage/src/sqlite.rs`
- Create: `server/src/account_monitor_store.rs`
- Create: `server/src/account_read_model.rs`
- Create: `server/src/account_projector.rs`
- Modify: `server/src/state_bootstrap.rs`
- Create: `server/src/account_monitor.rs`
- Modify: `server/src/main.rs`
- Test: `cargo test -p poise-server config::tests::defaults_account_monitor_thresholds -- --nocapture`
- Test: `cargo test -p poise-server config::tests::rejects_inverted_account_monitor_thresholds -- --nocapture`
- Test: `cargo test -p poise-binance types::tests::converts_account_information_into_account_summary_snapshot -- --nocapture`
- Test: `cargo test -p poise-storage sqlite::tests::save_and_load_account_monitor_state_round_trip -- --nocapture`
- Test: `cargo test -p poise-server account_monitor::tests::marks_equity_below_zero_as_critical -- --nocapture`
- Test: `cargo test -p poise-server account_projector::tests::projects_account_read_model_to_summary_view -- --nocapture`

- [ ] **Step 1: 先写失败测试，固定配置默认值、账户摘要映射和持久化模型**

要求：
- 在 `server/src/config.rs` 固定：
  - `[account_monitor]` 整段缺失时回落默认值
  - 任何非有限值或 `attention < critical` 的配置都拒绝启动
- 在 `exchanges/binance/src/types.rs` 固定 Binance 响应到账户摘要快照的映射：
  - `equity <- totalMarginBalance`
  - `available <- availableBalance`
  - `unrealized_pnl <- totalUnrealizedProfit`
- 在 `storage/src/sqlite.rs` 固定 `StoredAccountMonitorState` 的 round-trip
- 在 `server/src/account_monitor.rs` 固定极端值行为：
  - `equity <= 0` 时 `risk_signal == critical`
  - `day_change == None`
  - `reason` 至少包含 `equity <= 0`
- 在 `server/src/account_projector.rs` 固定 server 内部 `AccountReadModel` 到 `AccountSummaryView` 的投影，不让 `AccountMonitor` 直接依赖协议 DTO

- [ ] **Step 2: 运行定向测试，确认这些边界尚未存在**

Run:
`cargo test -p poise-server config::tests::defaults_account_monitor_thresholds -- --nocapture`
`cargo test -p poise-binance types::tests::converts_account_information_into_account_summary_snapshot -- --nocapture`
`cargo test -p poise-storage sqlite::tests::save_and_load_account_monitor_state_round_trip -- --nocapture`
`cargo test -p poise-server account_monitor::tests::marks_equity_below_zero_as_critical -- --nocapture`
`cargo test -p poise-server account_projector::tests::projects_account_read_model_to_summary_view -- --nocapture`

Expected:
- 测试失败或编译失败
- 失败原因明确指向：
  - 配置层还没有 `account_monitor`
  - `engine` 还没有账户摘要读取能力
  - SQLite 还没有账户监控状态表
  - server 还没有账户监控 store、内部读模型和 projector
  - `AccountMonitor` 模块还不存在

- [ ] **Step 3: 实现账户监控的基础边界**

要求：
- 在 `engine/src/ports.rs` 新增：
  - `AccountSummarySnapshot`
  - `ExchangePort::get_account_summary()`
- 在 Binance 适配层增加对 `/fapi/v3/account` 的读取和映射
- 在 `storage/src/schema.rs` 新增单行表 `account_monitor_state`
- 在 `storage/src/sqlite.rs` 增加账户监控状态的原始 SQLite 读写方法，但不把 server 账户监控 trait 推到 `engine`
- 在 `server/src/account_monitor_store.rs` 定义：
  - `StoredAccountMonitorState`
  - `AccountMonitorStore`
  - `SqliteAccountMonitorStore`
- 在 `server/src/state_bootstrap.rs` 让准备好的仓库同时装配 `Track` 仓库和 `AccountMonitorStore`
- 在 `server/src/config.rs` 增加 `AccountMonitorConfig`、默认值和校验
- 创建 `server/src/account_read_model.rs`，定义 `AccountReadModel`
- 创建 `server/src/account_projector.rs`，集中承担协议投影
- 创建 `server/src/account_monitor.rs`，先实现：
  - `ObservedAccountSnapshot`
  - `InMemoryAccountMonitorState`
  - `AccountMonitor::restore(...)`
  - `AccountMonitor::refresh_once()`
  - 风险计算
  - `day_change` / `day_base_at` 计算
  - 摘要 diff 判定
  - 变化时持久化与 `AccountChanged` 发布条件

- [ ] **Step 4: 跑基础边界回归**

Run:
`cargo test -p poise-binance`
`cargo test -p poise-storage sqlite::tests::save_and_load_account_monitor_state_round_trip -- --nocapture`
`cargo test -p poise-server config::tests::defaults_account_monitor_thresholds -- --nocapture`
`cargo test -p poise-server account_monitor::tests:: -- --nocapture`
`cargo test -p poise-server account_projector::tests::projects_account_read_model_to_summary_view -- --nocapture`

Expected:
- Binance 包测试通过，账户摘要映射稳定
- 存储层能保存并恢复原始账户快照与基准元数据
- `AccountMonitor` 单元测试通过，抓取、diff、持久化和通知条件都封在模块内部
- `AccountProjector` 测试通过，协议 DTO 适配停留在边界层

- [ ] **Step 5: 提交并回写 SHA**

```bash
git add server/src/config.rs engine/src/ports.rs exchanges/binance/src/types.rs exchanges/binance/src/rest.rs exchanges/binance/src/adapter.rs storage/src/schema.rs storage/src/sqlite.rs server/src/account_monitor_store.rs server/src/account_read_model.rs server/src/account_projector.rs server/src/state_bootstrap.rs server/src/account_monitor.rs server/src/main.rs
git commit -m "feat: add account monitor state and exchange summary support"
```

Task 2 code commit:
`<pending>`

---

### Task 3: 把 AccountMonitor 接入 server 启动、轮询、HTTP 和 WebSocket

**Files:**
- Modify: `server/src/assembly.rs`
- Modify: `server/src/http.rs`
- Modify: `server/src/runtime.rs`
- Modify: `server/src/websocket.rs`
- Modify: `server/src/main.rs`
- Modify: `server/src/account_monitor.rs`
- Modify: `server/src/account_projector.rs`
- Test: `cargo test -p poise-server http::tests::get_account_returns_latest_summary -- --nocapture`
- Test: `cargo test -p poise-server websocket::tests::broadcasts_account_summary_changed_after_account_notification -- --nocapture`
- Test: `cargo test -p poise-server runtime::tests::account_monitor_task_triggers_immediate_refresh_and_periodic_refresh -- --nocapture`

- [ ] **Step 1: 先写失败测试，固定 server 读侧和轮询边界**

要求：
- 在 `server/src/http.rs` 固定新增 `GET /account`，返回 `AccountSummaryView`
- 在 `server/src/websocket.rs` 固定收到 `ServerNotification::AccountChanged` 后会发 `StreamEvent::AccountSummaryChanged`
- 在 `server/src/runtime.rs` 固定新的账户轮询任务行为：
  - 启动后立即刷新一次
  - 后续按 `5s` 轮询
  - runtime 只调用 `AccountMonitor::refresh_once()`，不重新实现摘要 diff 和通知规则

- [ ] **Step 2: 运行定向测试，确认 server 还没有这条账户链路**

Run:
`cargo test -p poise-server http::tests::get_account_returns_latest_summary -- --nocapture`
`cargo test -p poise-server websocket::tests::broadcasts_account_summary_changed_after_account_notification -- --nocapture`
`cargo test -p poise-server runtime::tests::account_monitor_task_triggers_immediate_refresh_and_periodic_refresh -- --nocapture`

Expected:
- 测试失败或编译失败
- 失败原因明确指向：
  - `ServerState` 还没有 `AccountMonitor`
  - `/account` 路由不存在
  - runtime 还没有账户轮询任务
  - websocket 还不会通过 projector 重投影账户摘要

- [ ] **Step 3: 实现 server 接线**

要求：
- `ServerState` 增加 `account_monitor: Arc<AccountMonitor>`
- `assembly` 负责构造 `AccountMonitor`，并在启动时恢复持久化状态
- `runtime` 新增账户轮询任务和 shutdown handle，但任务只负责定时调用 `account_monitor.refresh_once()`
- `http` 新增 `/account`
- `http` 通过 `AccountProjector` 把 `AccountReadModel` 投影为 `AccountSummaryView`
- `websocket` 收到 `ServerNotification::AccountChanged` 后从 `AccountMonitor::current_summary()` 取内部读模型，再经 `AccountProjector` 发 `AccountSummaryChanged`
- 单次刷新失败只保留最近一次成功摘要，不发风险异常
- 首次成功刷新和摘要变化是否发 `AccountChanged`，统一留在 `AccountMonitor::refresh_once()` 内决定

- [ ] **Step 4: 跑 server 包回归**

Run:
`cargo test -p poise-server http::tests::get_account_returns_latest_summary -- --nocapture`
`cargo test -p poise-server websocket::tests::broadcasts_account_summary_changed_after_account_notification -- --nocapture`
`cargo test -p poise-server runtime::tests::account_monitor_task_triggers_immediate_refresh_and_periodic_refresh -- --nocapture`
`cargo test -p poise-server`

Expected:
- 新增账户 HTTP/WS 路径测试通过
- `poise-server` 全量测试通过
- runtime 启动、关闭和调度边界稳定
- `AccountMonitor` 继续独占账户摘要业务规则

- [ ] **Step 5: 提交并回写 SHA**

```bash
git add server/src/assembly.rs server/src/http.rs server/src/runtime.rs server/src/websocket.rs server/src/main.rs server/src/account_monitor.rs server/src/account_projector.rs
git commit -m "feat: wire account monitor into server runtime and api"
```

Task 3 code commit:
`<pending>`

---

### Task 4: 在 TUI 接入账户监控面板并完成整体验收

**Files:**
- Modify: `tui/src/api_client.rs`
- Modify: `tui/src/app.rs`
- Modify: `tui/src/main.rs`
- Modify: `tui/src/views/dashboard.rs`
- Modify: `tui/src/views/mod.rs`
- Create: `tui/src/views/account_panel.rs`
- Create: `tui/tests/fixtures/account_summary_view.json`
- Create: `tui/tests/fixtures/ws_account_summary_changed.json`
- Test: `cargo test -p poise-tui api_client::tests::gets_account_summary -- --nocapture`
- Test: `cargo test -p poise-tui tests::load_initial_state_fetches_account_before_tracks -- --nocapture`
- Test: `cargo test -p poise-tui views::dashboard::tests::renders_account_panel_with_attention_signal -- --nocapture`
- Test: `cargo test -p poise-tui tests::handle_ws_event_applies_account_summary_changed -- --nocapture`

- [ ] **Step 1: 先写失败测试，固定 TUI 启动顺序、账户状态和渲染结果**

要求：
- 在 `tui/src/api_client.rs` 固定 `GET /account`
- 在 `tui/src/main.rs` 固定启动顺序是：
  1. `/account`
  2. `/tracks`
  3. `/tracks/:id`
  4. `/ws`
- 在 `tui/src/views/dashboard.rs` 固定 `Dashboard` 顶部出现 `Account` 区块
- 在 `tui/src/main.rs` 或 `tui/src/app.rs` 固定 `AccountSummaryChanged` 事件会更新面板状态

- [ ] **Step 2: 运行定向测试，确认 TUI 还没有账户面板**

Run:
`cargo test -p poise-tui api_client::tests::gets_account_summary -- --nocapture`
`cargo test -p poise-tui tests::load_initial_state_fetches_account_before_tracks -- --nocapture`
`cargo test -p poise-tui views::dashboard::tests::renders_account_panel_with_attention_signal -- --nocapture`
`cargo test -p poise-tui tests::handle_ws_event_applies_account_summary_changed -- --nocapture`

Expected:
- 测试失败或编译失败
- 失败原因明确指向：
  - `ApiClient` 还没有 `/account`
  - 初始加载顺序仍是旧的 track-only bootstrap
  - `Dashboard` 还只有表格
  - 账户事件还没有接到 UI 更新

- [ ] **Step 3: 实现 TUI 账户面板**

要求：
- `ApiClient` 新增 `get_account_summary()`
- `App` 维护 `account_summary`
- `load_initial_state()` 和 `sync_projected_state()` 都先取账户摘要，再取 tracks
- `handle_ws_event()` 增加 `AccountSummaryChanged`
- 新建 `tui/src/views/account_panel.rs`，负责：
  - `equity / available / unrealized pnl / day change`
  - `risk signal / reason / day base at / updated at`
  - `unavailable` fallback
- `dashboard.rs` 只负责布局，把账户区块和表格拼起来

- [ ] **Step 4: 跑 TUI 包和跨包回归**

Run:
`cargo test -p poise-tui`
`cargo test -p poise-protocol -p poise-binance -p poise-storage -p poise-server -p poise-tui`

Expected:
- `poise-tui` 全量测试通过
- 所有受影响包回归通过
- 账户监控面板可以通过 HTTP bootstrap 和 WebSocket 增量更新稳定显示

- [ ] **Step 5: 提交并回写 SHA**

```bash
git add tui/src/api_client.rs tui/src/app.rs tui/src/main.rs tui/src/views/dashboard.rs tui/src/views/mod.rs tui/src/views/account_panel.rs tui/tests/fixtures/account_summary_view.json tui/tests/fixtures/ws_account_summary_changed.json
git commit -m "feat: add dashboard account monitor panel"
```

Task 4 code commit:
`<pending>`
