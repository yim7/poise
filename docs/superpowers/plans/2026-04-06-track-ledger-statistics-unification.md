# Track Ledger 统计统一 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 统一 track 统计口径，让列表和详情都基于同一套 ledger 事实计算收益；在详情视图中进一步展示毛已实现收益、净已实现收益、手续费累计、资金费累计和未解决 ledger 缺口。

**Architecture:** 不再把收益相关事实分散在 `RiskState`、订单事件和额外补丁字段里，而是引入单一 `TrackLedgerState` 模块承载所有账务事实。交易所适配层把单条用户流消息归一成语义清晰的 `TrackLedgerEvent` 枚举，而不是用可选字段拼成袋状结构；写侧只通过统一 `apply_track_ledger_event` / `apply_ledger_delta` 入口更新状态。持久化层把 `TrackLedgerState` 作为值对象整体存取，避免 schema 泄漏内部结构；读侧先投影成共享的 `LedgerSummary`，列表和详情都从这一个 summary 派生，避免再次出现两套 `total_pnl` 口径和两套协议命名。

**Tech Stack:** Rust, serde, tokio, axum, ratatui, cargo test

---

## File Structure

- `engine/src/ledger.rs`
  - 定义 `TrackLedgerState`
  - 定义 `LedgerDelta`
  - 定义 `TrackLedgerEvent`
  - 定义 `ExecutionLedgerUpdate`
  - 定义 `LedgerAdjustmentEvent`
  - 定义 `LedgerGapRecord`
- `engine/src/runtime.rs`
  - `TrackRuntime` / `RiskState` 改为持有或引用统一账务状态
- `engine/src/observation.rs`
  - 对外观察值只保留高层事件，不暴露分散的收益碎片
- `engine/src/manager.rs`
  - 所有收益相关更新都通过统一账务入口
- `engine/src/snapshot.rs`
  - 快照透传 `ledger_state`
- `engine/src/ports.rs`
  - 交易所用户流 payload 改为高层 `TrackLedgerEvent`
- `storage/src/schema.rs`
  - `grid_snapshots` 增加 `ledger_state_json`
- `storage/src/sqlite.rs`
  - 统一序列化/反序列化 `ledger_state_json`
- `exchanges/binance/src/websocket.rs`
  - 把 Binance 用户流归一成 `ExecutionLedgerEvent`
- `server/src/runtime.rs`
  - 路由高层账务事件到对应 track
- `server/src/write_service.rs`
  - 增加统一账务写入入口
- `server/src/read_model.rs`
  - 读模型持有 `TrackLedgerState`
- `server/src/projector.rs`
  - 先投影共享 `LedgerSummary`
  - 各类 track 视图都从同一个 summary 派生
- `protocol/src/lib.rs`
  - 新增 `TrackLedgerView`
  - 新增轻量 `TrackListLedgerView`
  - 各类 track 协议共享同一套 ledger 口径和命名
  - 详情协议从 `pnl` 迁到 `ledger`
- `server/src/http.rs`
  - detail 响应测试补 `ledger` 断言
- `server/src/websocket.rs`
  - `track_detail_changed` 测试补 `ledger` 断言
- `tui/src/views/instance.rs`
  - 详情统计区改读 `ledger`
- `tui/tests/fixtures/track_detail_view.json`
  - detail fixture 改成 `ledger`
- `tui/tests/fixtures/ws_track_detail_changed.json`
  - websocket fixture 改成 `ledger`

## Implementation Notes

- 单一账务状态：
  - `TrackLedgerState`
  - 包含日内窗口、累计账务、未解决 gap 集合
- 单一共享读侧摘要：
  - `LedgerSummary`
  - 包含列表和详情共用的核心口径：
    - `gross_realized_pnl`
    - `net_realized_pnl`
    - `unrealized_pnl`
    - `total_pnl`
    - `trading_fee_cumulative`
    - `funding_fee_cumulative`
    - `has_unresolved_gaps`
- 单一高层 ledger 事件：
  - `TrackLedgerEvent`
  - 第一版拆成两个语义分支：
    - `Execution(ExecutionLedgerUpdate)`
    - `Adjustment(LedgerAdjustmentEvent)`
- 单一账务增量：
  - `LedgerDelta`
  - 第一版包含 `GrossRealizedPnl`、`TradingFee`、`FundingFee`
- 事件约束：
  - `Execution` 分支必须带 `order_update`
  - `Adjustment` 分支不能带 `order_update`
  - 不再用“可选 `order_update` + Vec”表达多种不同语义
- 账务口径：
  - `gross_realized_pnl = ledger.gross_realized_pnl_cumulative`
  - `net_realized_pnl = gross_realized_pnl - trading_fee_cumulative + funding_fee_cumulative`
  - `total_pnl = net_realized_pnl + unrealized_pnl`
- 日内窗口归属：
  - `realized_pnl_today` 和日切信息也迁入 `TrackLedgerState`
  - `RiskState` 不再拥有收益原始事实，只消费账务派生结果
- gap 语义：
  - 未解决 gap 是集合，不是单个布尔值，也不是单个最近状态
  - 第一版可用 `Vec<LedgerGapRecord>` 持有所有未解决缺口
  - `LedgerGapRecord` 至少包含：
    - `gap_key`
    - `reason`
    - `observed_at`
    - `source`
  - `gap_key` 基于来源事件的稳定指纹生成，未来补账/清 gap 以它为准，不做模糊匹配
  - 当前范围内不做自动清除
  - 后续若增加补账/回放能力，再通过明确的补账流程清除对应 gap
- 持久化边界：
  - `TrackLedgerState` 作为值对象整体序列化到 `ledger_state_json`
  - schema 不展开内部字段，避免未来加账务维度时放大改动面
- track 视图协议边界：
  - list 和 detail 都不再各自定义账务计算逻辑
  - list 和 detail 都不再暴露旧 `pnl` 命名
  - 统一以 `ledger` 语义表达 track 收益账本；list 取其轻量子集，detail 取其完整视图
- 非目标：
  - 不引入账户级资金费分摊
  - 不实现补账流程

### Task 1: 建立统一账务模块与持久化边界

**Files:**
- Create: `engine/src/ledger.rs`
- Modify: `engine/src/runtime.rs`
- Modify: `engine/src/manager.rs`
- Modify: `engine/src/snapshot.rs`
- Modify: `storage/src/schema.rs`
- Modify: `storage/src/sqlite.rs`
- Test: `engine/src/ledger.rs`
- Test: `engine/src/manager.rs`
- Test: `storage/src/sqlite.rs`

- [x] **Step 1: 写失败测试，固定账务状态的边界和行为**

在 `engine/src/ledger.rs` 新增测试：

```rust
#[test]
fn apply_execution_ledger_event_updates_order_and_ledger_in_one_step() {
    // 一个执行事件同时带 order_update、gross realized、trading fee
    // 断言状态一次性更新，不依赖多次写入顺序
}

#[test]
fn unresolved_gaps_accumulate_without_overwriting_previous_records() {
    // 连续写入两个不同原因的 gap
    // 断言未解决 gap 集合保留两条记录
}

#[test]
fn ledger_state_owns_daily_realized_window() {
    // 跨 UTC 日切写入 gross realized
    // 断言日内窗口和累计窗口都在 ledger 模块内更新
}

#[test]
fn ledger_gap_record_has_stable_gap_key() {
    // 同一来源事件重复生成 gap
    // 断言 gap_key 稳定，不依赖运行时随机值
}
```

在 `storage/src/sqlite.rs` 的 snapshot roundtrip 测试补断言：

```rust
assert_eq!(loaded.ledger_state.unresolved_gaps.len(), 2);
assert_eq!(
    loaded.ledger_state.unresolved_gaps[0].gap_key,
    "binance:order_trade_update:btcusdt:12345:commission_asset"
);
assert!((loaded.ledger_state.trading_fee_cumulative - 3.2).abs() < f64::EPSILON);
assert!((loaded.ledger_state.funding_fee_cumulative + 1.5).abs() < f64::EPSILON);
```

- [x] **Step 2: 运行定向测试，确认当前红灯**

Run: `cargo test -p poise-engine manager::tests::apply_execution_ledger_event_updates_order_and_ledger_in_one_step -- --exact`
Expected: FAIL，原因是还没有统一账务模块

Run: `cargo test -p poise-engine runtime::tests::unresolved_gaps_accumulate_without_overwriting_previous_records -- --exact`
Expected: FAIL，原因是还没有 gap 集合

Run: `cargo test -p poise-storage sqlite::tests::save_and_load_grid_runtime_snapshot_roundtrip -- --exact`
Expected: FAIL，原因是还没有 `ledger_state_json`

- [x] **Step 3: 最小实现统一账务模块**

在 `engine/src/ledger.rs` 定义：

```rust
pub struct TrackLedgerState {
    pub realized_pnl_day: Option<NaiveDate>,
    pub gross_realized_pnl_today: f64,
    pub gross_realized_pnl_cumulative: f64,
    pub trading_fee_cumulative: f64,
    pub funding_fee_cumulative: f64,
    pub unresolved_gaps: Vec<LedgerGapRecord>,
}

pub enum LedgerDelta {
    GrossRealizedPnl(f64),
    TradingFee(f64),
    FundingFee(f64),
}

pub struct ExecutionLedgerUpdate {
    pub order_update: OrderObservation,
    pub ledger_deltas: Vec<LedgerDelta>,
    pub ledger_gaps: Vec<LedgerGapRecord>,
}

pub struct LedgerAdjustmentEvent {
    pub ledger_deltas: Vec<LedgerDelta>,
    pub ledger_gaps: Vec<LedgerGapRecord>,
    pub source: LedgerAdjustmentSource,
}

pub enum TrackLedgerEvent {
    Execution(ExecutionLedgerUpdate),
    Adjustment(LedgerAdjustmentEvent),
}
```

在 `engine/src/runtime.rs` 和 `engine/src/manager.rs`：

- 把 `realized_pnl_today` / `realized_pnl_day` 从 `RiskState` 迁出
- `TrackRuntime` 持有 `ledger_state`
- `manager` 只通过统一入口更新账务状态

在 `storage/src/schema.rs` / `storage/src/sqlite.rs`：

```sql
ledger_state_json TEXT NOT NULL
```

序列化整个 `TrackLedgerState`，不要把内部字段打平成独立列。

- [x] **Step 4: 重新运行定向测试，确认转绿**

Run: `cargo test -p poise-engine manager::tests::apply_execution_ledger_event_updates_order_and_ledger_in_one_step -- --exact`
Expected: PASS

Run: `cargo test -p poise-engine runtime::tests::unresolved_gaps_accumulate_without_overwriting_previous_records -- --exact`
Expected: PASS

Run: `cargo test -p poise-storage sqlite::tests::save_and_load_grid_runtime_snapshot_roundtrip -- --exact`
Expected: PASS

- [x] **Step 5: 运行本 task 回归测试**

Run: `cargo test -p poise-engine`
Expected: PASS

Run: `cargo test -p poise-storage`
Expected: PASS

- [x] **Step 6: 提交本 task**

```bash
git add engine/src/ledger.rs engine/src/runtime.rs engine/src/manager.rs engine/src/snapshot.rs storage/src/schema.rs storage/src/sqlite.rs
git commit -m "refactor: introduce unified track ledger state"
```

- [x] **Step 7: 回写 commit SHA 到本任务**

Commit SHA: `676f5c6`

---

### Task 2: 把交易所消息归一成语义清晰的 ledger 事件

**Files:**
- Modify: `engine/src/ports.rs`
- Modify: `engine/src/observation.rs`
- Modify: `exchanges/binance/src/websocket.rs`
- Modify: `server/src/runtime.rs`
- Modify: `server/src/write_service.rs`
- Test: `exchanges/binance/src/websocket.rs`
- Test: `server/src/runtime.rs`

- [ ] **Step 1: 写失败测试，固定“单条消息 -> 单个高层事件”的契约**

在 `exchanges/binance/src/websocket.rs` 新增测试：

```rust
#[test]
fn parses_order_trade_update_into_track_ledger_execution_event() {
    // 一条 ORDER_TRADE_UPDATE
    // 断言产出 TrackLedgerEvent::Execution
    // 其中同时包含 order_update、gross realized、trading fee
}

#[test]
fn parses_funding_fee_account_update_into_track_ledger_adjustment_event() {
    // 一条 FUNDING_FEE ACCOUNT_UPDATE
    // 断言产出 TrackLedgerEvent::Adjustment
    // 其中 ledger_deltas 只有 FundingFee
}

#[test]
fn parses_unsupported_commission_asset_into_execution_gap_record() {
    // commission 无法归一
    // 断言仍然只产出一个 TrackLedgerEvent::Execution
    // gap 写在该 execution event 里，不额外拆成另一条消息
}
```

在 `server/src/runtime.rs` 新增测试：

```rust
#[tokio::test]
async fn apply_user_data_event_persists_track_ledger_event_atomically() {
    // 构造一个 TrackLedgerEvent::Execution
    // 断言写服务只走一次统一入口
}
```

- [ ] **Step 2: 运行定向测试，确认当前红灯**

Run: `cargo test -p poise-binance parses_order_trade_update_into_track_ledger_execution_event -- --exact`
Expected: FAIL，原因是还没有枚举化的高层 ledger 事件

Run: `cargo test -p poise-server apply_user_data_event_persists_track_ledger_event_atomically -- --exact`
Expected: FAIL，原因是 runtime / write_service 还没有统一入口

- [ ] **Step 3: 最小实现高层事件归一**

在 `engine/src/ports.rs`：

```rust
pub enum UserDataPayload {
    PositionUpdate(Position),
    TrackLedger(TrackLedgerEvent),
}
```

在 `exchanges/binance/src/websocket.rs`：

- `ORDER_TRADE_UPDATE` 产出一个 `TrackLedgerEvent::Execution`
- `ACCOUNT_UPDATE(FUNDING_FEE)` 产出一个 `TrackLedgerEvent::Adjustment`
- 无法归一化的手续费或资金费归属问题写进该事件的 `ledger_gaps`

在 `server/src/runtime.rs` / `server/src/write_service.rs`：

- 新增单个写入入口，例如 `apply_track_ledger_event`
- 不再把同一条交易所消息拆成多个独立写入

- [ ] **Step 4: 重新运行定向测试，确认转绿**

Run: `cargo test -p poise-binance parses_order_trade_update_into_track_ledger_execution_event -- --exact`
Expected: PASS

Run: `cargo test -p poise-binance parses_funding_fee_account_update_into_track_ledger_adjustment_event -- --exact`
Expected: PASS

Run: `cargo test -p poise-binance parses_unsupported_commission_asset_into_execution_gap_record -- --exact`
Expected: PASS

Run: `cargo test -p poise-server apply_user_data_event_persists_track_ledger_event_atomically -- --exact`
Expected: PASS

- [ ] **Step 5: 运行本 task 回归测试**

Run: `cargo test -p poise-binance`
Expected: PASS

Run: `cargo test -p poise-server runtime::tests -- --nocapture`
Expected: PASS

- [ ] **Step 6: 提交本 task**

```bash
git add engine/src/ports.rs engine/src/observation.rs exchanges/binance/src/websocket.rs server/src/runtime.rs server/src/write_service.rs
git commit -m "refactor: normalize exchange messages into execution ledger events"
```

- [ ] **Step 7: 回写 commit SHA 到本任务**

Commit SHA: `<待回写>`

---

### Task 3: 建立共享 ledger 投影，并统一 list/detail 协议命名

**Files:**
- Modify: `server/src/read_model.rs`
- Modify: `server/src/projector.rs`
- Modify: `protocol/src/lib.rs`
- Modify: `server/src/http.rs`
- Modify: `server/src/websocket.rs`
- Modify: `tui/src/main.rs`
- Test: `server/src/projector.rs`
- Test: `protocol/src/lib.rs`
- Test: `server/src/http.rs`
- Test: `server/src/websocket.rs`

- [ ] **Step 1: 写失败测试，固定独立账务视图语义**

在 `server/src/projector.rs` 新增测试：

```rust
#[test]
fn projects_list_item_total_pnl_from_shared_ledger_summary() {
    // list item 的 total_pnl 和 detail.ledger.total_pnl 必须来自同一 summary
}

#[test]
fn projects_list_item_lightweight_ledger_view() {
    // list item 使用 TrackListLedgerView，而不是旧 TrackListPnlView
}

#[test]
fn projects_detail_ledger_from_unified_ledger_state() {
    // 断言 detail.ledger.gross_realized_pnl / net_realized_pnl / fees / funding 来自同一 ledger_state
}

#[test]
fn projects_all_unresolved_ledger_gaps() {
    // ledger_state 有两条 unresolved_gaps
    // 断言 detail.ledger.unresolved_gaps 全部投影出来
}
```

在 `protocol/src/lib.rs` 增加序列化/反序列化断言：

```json
"ledger": {
  "gross_realized_pnl": 980.1,
  "net_realized_pnl": 963.8,
  "unrealized_pnl": 265.2,
  "total_pnl": 1229.0,
  "trading_fee_cumulative": 12.3,
  "funding_fee_cumulative": -4.0,
  "unresolved_gaps": []
}
```

- [ ] **Step 2: 运行定向测试，确认当前红灯**

Run: `cargo test -p poise-server projects_list_item_total_pnl_from_shared_ledger_summary -- --exact`
Expected: FAIL，原因是各 track 视图还没有共享 ledger 投影

Run: `cargo test -p poise-server projects_list_item_lightweight_ledger_view -- --exact`
Expected: FAIL，原因是列表协议还沿用旧 `pnl`

Run: `cargo test -p poise-server projects_detail_ledger_from_unified_ledger_state -- --exact`
Expected: FAIL，原因是 detail 还没有 `ledger` 视图

Run: `cargo test -p poise-protocol deserializes_track_detail_with_pnl_and_execution_stats -- --exact`
Expected: FAIL，原因是协议还沿用 `pnl`

- [ ] **Step 3: 最小实现独立账务视图**

在 `server/src/projector.rs` 先新增内部共享摘要：

```rust
struct LedgerSummary {
    gross_realized_pnl: f64,
    net_realized_pnl: f64,
    unrealized_pnl: f64,
    total_pnl: f64,
    trading_fee_cumulative: f64,
    funding_fee_cumulative: f64,
    has_unresolved_gaps: bool,
}
```

并新增单一投影入口：

```rust
fn project_ledger_summary(source: &TrackReadModel) -> LedgerSummary
```

`project_list_item` 和 `project_detail` 都调用它。

在 `server/src/read_model.rs`：

```rust
pub ledger_state: TrackLedgerState,
```

在 `protocol/src/lib.rs`：

```rust
pub struct TrackListLedgerView {
    pub total_pnl: f64,
    pub has_unresolved_gaps: bool,
}
```

并把：

```rust
pub pnl: TrackListPnlView
```

改成：

```rust
pub ledger: TrackListLedgerView
```

同时新增：

```rust
pub struct TrackLedgerGapView {
    pub gap_key: String,
    pub reason: TrackLedgerGapReasonView,
    pub observed_at: String,
}
```

在 `protocol/src/lib.rs`：

```rust
pub struct TrackLedgerView {
    pub gross_realized_pnl: f64,
    pub net_realized_pnl: f64,
    pub unrealized_pnl: f64,
    pub total_pnl: f64,
    pub trading_fee_cumulative: f64,
    pub funding_fee_cumulative: f64,
    pub unresolved_gaps: Vec<TrackLedgerGapView>,
}
```

在 `TrackDetailView` 中把：

```rust
pub pnl: TrackPnlView
```

改成：

```rust
pub ledger: TrackLedgerView
```

在 `server/src/projector.rs` 用 `ledger_state` 统一投影，不再从多个字段拼装。

`TrackListItemView.ledger.total_pnl` 和 `TrackListItemView.ledger.has_unresolved_gaps` 都来自 `project_ledger_summary(source)`。

- [ ] **Step 4: 重新运行定向测试，确认转绿**

Run: `cargo test -p poise-server projects_list_item_total_pnl_from_shared_ledger_summary -- --exact`
Expected: PASS

Run: `cargo test -p poise-server projects_list_item_lightweight_ledger_view -- --exact`
Expected: PASS

Run: `cargo test -p poise-server projects_detail_ledger_from_unified_ledger_state -- --exact`
Expected: PASS

Run: `cargo test -p poise-server projects_all_unresolved_ledger_gaps -- --exact`
Expected: PASS

Run: `cargo test -p poise-protocol deserializes_track_detail_with_pnl_and_execution_stats -- --exact`
Expected: PASS

Run: `cargo test -p poise-server get_track_detail_returns_track_detail_view -- --exact`
Expected: PASS

Run: `cargo test -p poise-server broadcasts_track_detail_changed_after_write_commit -- --exact`
Expected: PASS

- [ ] **Step 5: 运行本 task 回归测试**

Run: `cargo test -p poise-protocol`
Expected: PASS

Run: `cargo test -p poise-server projector::tests http::tests websocket::tests -- --nocapture`
Expected: PASS

- [ ] **Step 6: 提交本 task**

```bash
git add server/src/read_model.rs server/src/projector.rs protocol/src/lib.rs server/src/http.rs server/src/websocket.rs
git commit -m "refactor: project shared track ledger views"
```

- [ ] **Step 7: 回写 commit SHA 到本任务**

Commit SHA: `<待回写>`

---

### Task 4: 更新 TUI track 视图，让列表和详情使用统一的 ledger 命名与口径

**Files:**
- Modify: `tui/src/views/instance.rs`
- Modify: `tui/tests/fixtures/track_detail_view.json`
- Modify: `tui/tests/fixtures/ws_track_detail_changed.json`
- Modify: `tui/src/app.rs`
- Modify: `tui/src/main.rs`
- Test: `tui/src/views/instance.rs`
- Test: `tui/src/app.rs`
- Test: `tui/src/main.rs`

- [ ] **Step 1: 写失败测试，固定 TUI track ledger 展示**

在 `tui/src/views/instance.rs` 新增或扩展测试，断言 detail ledger 区包含：

```text
total ↑ +1229.00 | unrealized ↑ +265.20 | gross realized ↑ +980.10 | net realized ↑ +963.80
fee cumulative ↓ -12.30 | funding cumulative ↓ -4.00
ledger gaps: none
```

另加一个 gap 场景：

```text
ledger gaps: unsupported commission asset (2026-04-06T10:00:00Z)
```

在 `tui/src/main.rs` 或对应列表测试新增断言：

```rust
#[tokio::test]
async fn list_and_detail_show_same_total_pnl_for_same_track() {
    // 同一个 track 的 list item ledger.total_pnl 和 detail.ledger.total_pnl 一致
}
```

- [ ] **Step 2: 运行定向测试，确认当前红灯**

Run: `cargo test -p poise-tui renders_track_detail_execution_activity_and_commands -- --exact`
Expected: FAIL，原因是 fixture 和视图还没迁到 `ledger`

Run: `cargo test -p poise-tui websocket_event_applies_projected_detail -- --exact`
Expected: FAIL，原因是 websocket fixture 结构已变化

Run: `cargo test -p poise-tui list_and_detail_show_same_total_pnl_for_same_track -- --exact`
Expected: FAIL，原因是 TUI 各 track 视图还没共享新口径

- [ ] **Step 3: 最小实现 TUI 展示**

在 `tui/src/views/instance.rs`：

- 统计区改读 `detail.ledger`
- 第一行显示 `total / unrealized / gross realized / net realized`
- 第二行显示 `fee cumulative / funding cumulative`
- 第三行显示 unresolved gaps 摘要

fixture 全部迁到 `ledger` 结构。

列表展示保持 UI 结构不变，但协议和测试都改成读取 `item.ledger`，并覆盖各 track 视图的 `total_pnl` 一致性。

- [ ] **Step 4: 重新运行定向测试，确认转绿**

Run: `cargo test -p poise-tui renders_track_detail_execution_activity_and_commands -- --exact`
Expected: PASS

Run: `cargo test -p poise-tui websocket_event_applies_projected_detail -- --exact`
Expected: PASS

Run: `cargo test -p poise-tui list_and_detail_show_same_total_pnl_for_same_track -- --exact`
Expected: PASS

- [ ] **Step 5: 运行本 task 回归测试**

Run: `cargo test -p poise-tui`
Expected: PASS

- [ ] **Step 6: 提交本 task**

```bash
git add tui/src/views/instance.rs tui/tests/fixtures/track_detail_view.json tui/tests/fixtures/ws_track_detail_changed.json tui/src/app.rs tui/src/main.rs
git commit -m "refactor: render shared track ledger summaries"
```

- [ ] **Step 7: 回写 commit SHA 到本任务**

Commit SHA: `<待回写>`
