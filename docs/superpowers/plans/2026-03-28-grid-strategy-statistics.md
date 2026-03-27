# Grid Strategy Statistics Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 为 grid detail 补齐稳定的收益统计链路，第一版在详情页显示 `Total PnL` 和 `Realized PnL`。

**Architecture:** 先在 `engine` / `storage` 里新增并持久化“累计已实现收益”运行时事实，再由 `server/projector` 把该事实解释为 `statistics` 读模型，最后由 `grid-tui` 把它渲染成独立 `Statistics` 区块。展示样式先按视觉方案 C 落地，但结构上保留随时切回列表式方案 B 的空间。

**Tech Stack:** Rust, serde, rusqlite, axum, ratatui, cargo test

---

## File Structure

- `engine/src/runtime.rs`
  - `RiskState` 结构定义，新增累计收益字段
- `engine/src/manager.rs`
  - 订单观察回写逻辑，累计 `realized_pnl_cumulative`
- `engine/src/snapshot.rs`
  - 快照结构透传累计收益字段
- `storage/src/schema.rs`
  - SQLite `grid_snapshots` 表增加累计收益列
- `storage/src/sqlite.rs`
  - SQLite 读写累计收益列
- `protocol/src/lib.rs`
  - `GridDetailView` 新增 `statistics`
  - 新增 `GridStatisticsView`
- `server/src/projector.rs`
  - detail 投影组装 `statistics`
- `server/src/http.rs`
  - detail HTTP 响应测试补统计断言
- `server/src/query_service.rs`
  - 查询层测试夹具补齐新的 `RiskState` 字段
- `tui/src/protocol.rs`
  - 共享协议反序列化测试补统计断言
- `tui/src/api_client.rs`
  - API client detail 解码测试补统计断言
- `tui/src/views/instance.rs`
  - 详情页渲染 `Statistics` 区块
- `tui/tests/fixtures/grid_detail_view.json`
  - detail fixture 增加 `statistics`
- `tui/tests/fixtures/ws_grid_detail_changed.json`
  - ws detail fixture 增加 `statistics`

## Implementation Notes

- `realized_pnl_today` 继续只服务日内风控。
- 新增 `realized_pnl_cumulative`，只由订单更新里的 `realized_pnl` 增量累加，不按日切重置。
- `total_pnl = realized_pnl_cumulative + unrealized_pnl`。
- 第一版统计展示固定只做两项：
  - `Total PnL`
  - `Realized PnL`
- PnL 文本先统一保留 2 位小数并显式带正负号，例如 `+1245.30`、`-12.50`。

### Task 1: 补累计已实现收益状态和持久化

**Files:**
- Modify: `engine/src/runtime.rs`
- Modify: `engine/src/manager.rs`
- Modify: `engine/src/snapshot.rs`
- Modify: `storage/src/schema.rs`
- Modify: `storage/src/sqlite.rs`
- Modify: `server/src/query_service.rs`
- Modify: `server/src/projector.rs`
- Test: `engine/src/manager.rs`
- Test: `storage/src/sqlite.rs`

- [x] **Step 1: 先写失败测试，覆盖累计收益不按日切丢失**

在 `engine/src/manager.rs` 新增一个专用测试，建议命名：

```rust
#[test]
fn observe_order_keeps_cumulative_realized_pnl_when_utc_day_changes() {
    // 第一天先累计一次 realized pnl
    // 第二天再回放新的 realized pnl
    // 断言 realized_pnl_today 被日切重置后重新累计
    // 断言 realized_pnl_cumulative 保留两天累计总和
}
```

同时在 `storage/src/sqlite.rs` 的快照 roundtrip 测试里新增断言：

```rust
assert!((loaded.risk.realized_pnl_cumulative - 17.5).abs() < f64::EPSILON);
```

- [x] **Step 2: 运行定向测试，确认当前红灯**

Run: `cargo test -p grid-engine observe_order_keeps_cumulative_realized_pnl_when_utc_day_changes -- --exact`
Expected: FAIL，原因是 `RiskState` 里还没有 `realized_pnl_cumulative`

Run: `cargo test -p grid-storage save_and_load_grid_runtime_snapshot_roundtrip -- --exact`
Expected: FAIL，原因是 snapshot / sqlite 还没有累计收益字段

- [x] **Step 3: 最小实现累计收益字段**

在 `engine/src/runtime.rs` 把 `RiskState` 扩成：

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RiskState {
    pub realized_pnl_day: Option<NaiveDate>,
    pub realized_pnl_today: f64,
    pub realized_pnl_cumulative: f64,
    pub unrealized_pnl: f64,
}
```

在 `engine/src/manager.rs` 的订单观察逻辑里保持现有日切语义，同时补累计：

```rust
if grid.risk_state.realized_pnl_day != Some(today) {
    grid.risk_state.realized_pnl_day = Some(today);
    grid.risk_state.realized_pnl_today = 0.0;
}

if observation.realized_pnl.abs() > f64::EPSILON {
    grid.risk_state.realized_pnl_today += observation.realized_pnl;
    grid.risk_state.realized_pnl_cumulative += observation.realized_pnl;
}
```

在 `engine/src/snapshot.rs`、`storage/src/schema.rs`、`storage/src/sqlite.rs` 打通这个字段：

```rust
realized_pnl_cumulative REAL NOT NULL DEFAULT 0
```

SQLite 插入和读取都要带上该列。

`server/src/query_service.rs` 与 `server/src/projector.rs` 中只要有 `RiskState { ... }` 测试夹具，也同步补 `realized_pnl_cumulative: 0.0`，保证 workspace 仍可编译。

- [x] **Step 4: 重新运行定向测试，确认转绿**

Run: `cargo test -p grid-engine observe_order_keeps_cumulative_realized_pnl_when_utc_day_changes -- --exact`
Expected: PASS

Run: `cargo test -p grid-storage save_and_load_grid_runtime_snapshot_roundtrip -- --exact`
Expected: PASS

- [x] **Step 5: 运行本 task 的回归测试**

Run: `cargo test -p grid-engine`
Expected: PASS

Run: `cargo test -p grid-storage`
Expected: PASS

- [x] **Step 6: 提交本 task**

```bash
git add engine/src/runtime.rs engine/src/manager.rs engine/src/snapshot.rs storage/src/schema.rs storage/src/sqlite.rs server/src/query_service.rs server/src/projector.rs
git commit -m "feat: persist cumulative realized pnl"
```

- [x] **Step 7: 回写 commit SHA 到本任务**

Commit SHA: `8c7b611bf1c01f2892ed2ea9ae4930c8f337f604`

---

### Task 2: 投影 statistics 到 detail 协议和 HTTP/API 契约

**Files:**
- Modify: `protocol/src/lib.rs`
- Modify: `server/src/projector.rs`
- Modify: `server/src/http.rs`
- Modify: `tui/src/protocol.rs`
- Modify: `tui/src/api_client.rs`
- Modify: `tui/tests/fixtures/grid_detail_view.json`
- Modify: `tui/tests/fixtures/ws_grid_detail_changed.json`
- Test: `server/src/projector.rs`
- Test: `server/src/http.rs`
- Test: `tui/src/protocol.rs`
- Test: `tui/src/api_client.rs`

- [x] **Step 1: 先写失败测试，覆盖 statistics 读模型**

在 `server/src/projector.rs` 新增专用测试，建议命名：

```rust
#[test]
fn project_detail_projects_statistics_from_risk_state() {
    let detail = GridProjector::new().project_detail(&source_with_submitting_effect());

    assert!((detail.statistics.realized_pnl - 980.1).abs() < f64::EPSILON);
    assert!((detail.statistics.total_pnl - 1245.3).abs() < f64::EPSILON);
}
```

并扩展这些现有测试的断言：

- `server/src/http.rs:get_grid_detail_returns_projected_detail`
- `tui/src/protocol.rs:deserializes_grid_detail_view`
- `tui/src/protocol.rs:deserializes_grid_stream_detail_changed`
- `tui/src/api_client.rs:get_grid_detail_decodes_projected_detail`

fixture 里的 `statistics` 先写成：

```json
"statistics": {
  "total_pnl": 1245.3,
  "realized_pnl": 980.1
}
```

- [x] **Step 2: 运行定向测试，确认当前红灯**

Run: `cargo test -p grid-server project_detail_projects_statistics_from_risk_state -- --exact`
Expected: FAIL，原因是 `GridDetailView` 里还没有 `statistics`

Run: `cargo test -p grid-tui deserializes_grid_detail_view -- --exact`
Expected: FAIL，原因是 fixture 新增 `statistics` 后协议结构还没接上

- [x] **Step 3: 最小实现 protocol 和 projector**

在 `protocol/src/lib.rs` 新增：

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GridStatisticsView {
    pub total_pnl: f64,
    pub realized_pnl: f64,
}
```

并把它接到 `GridDetailView`：

```rust
pub struct GridDetailView {
    pub identity: GridIdentityView,
    pub status: GridStatusPanelView,
    pub strategy: GridStrategyView,
    pub market: GridMarketView,
    pub position: GridPositionView,
    pub statistics: GridStatisticsView,
    pub execution: GridExecutionView,
    pub activity: Vec<GridActivityItemView>,
    pub available_commands: Vec<GridCommandView>,
}
```

在 `server/src/projector.rs` 组装：

```rust
statistics: GridStatisticsView {
    total_pnl: source.snapshot.risk.realized_pnl_cumulative
        + source.snapshot.risk.unrealized_pnl,
    realized_pnl: source.snapshot.risk.realized_pnl_cumulative,
},
```

同步更新：

- `server/src/http.rs` 的 detail 测试断言
- `tui/src/protocol.rs` 的 re-export / fixture 断言
- `tui/src/api_client.rs` 的 detail 解码断言
- 两个 JSON fixture

- [x] **Step 4: 重新运行定向测试，确认转绿**

Run: `cargo test -p grid-server project_detail_projects_statistics_from_risk_state -- --exact`
Expected: PASS

Run: `cargo test -p grid-server get_grid_detail_returns_projected_detail -- --exact`
Expected: PASS

Run: `cargo test -p grid-tui deserializes_grid_detail_view -- --exact`
Expected: PASS

Run: `cargo test -p grid-tui deserializes_grid_stream_detail_changed -- --exact`
Expected: PASS

Run: `cargo test -p grid-tui get_grid_detail_decodes_projected_detail -- --exact`
Expected: PASS

- [x] **Step 5: 运行本 task 的回归测试**

Run: `cargo test -p grid-server projector::tests -- --nocapture`
Expected: PASS

Run: `cargo test -p grid-tui protocol::tests -- --nocapture`
Expected: PASS

Run: `cargo test -p grid-tui api_client::tests -- --nocapture`
Expected: PASS

- [x] **Step 6: 提交本 task**

```bash
git add protocol/src/lib.rs server/src/projector.rs server/src/http.rs tui/src/protocol.rs tui/src/api_client.rs tui/tests/fixtures/grid_detail_view.json tui/tests/fixtures/ws_grid_detail_changed.json
git commit -m "feat: project grid statistics"
```

- [x] **Step 7: 回写 commit SHA 到本任务**

Commit SHA: `0ee6c4b4077ca71f261f9615f3e2097126e16ee2`

---

### Task 3: 在 TUI 详情页渲染 Statistics 区块

**Files:**
- Modify: `tui/src/views/instance.rs`
- Test: `tui/src/views/instance.rs`

- [x] **Step 1: 先写失败测试，覆盖 Statistics 区块**

扩展 `tui/src/views/instance.rs` 里的现有测试 `renders_grid_detail_execution_activity_and_commands`，补这些断言：

```rust
assert!(text.contains("Statistics"));
assert!(text.contains("Total PnL"));
assert!(text.contains("Realized PnL"));
assert!(text.contains("+1245.30"));
assert!(text.contains("+980.10"));
```

- [x] **Step 2: 运行定向测试，确认当前红灯**

Run: `cargo test -p grid-tui views::instance::tests::renders_grid_detail_execution_activity_and_commands -- --exact`
Expected: FAIL，原因是当前视图还没有 `Statistics` 区块

- [x] **Step 3: 最小实现双列强调样式**

在 `tui/src/views/instance.rs`：

- 把纵向布局从 5 段扩成 6 段
- 在 `Overview` 后插入 `Statistics`
- `Statistics` 先按视觉方案 C 实现为两列强调，但仍保持纯文本 TUI 风格

建议渲染结构：

```rust
let statistics_lines = vec![
    Line::from("Total PnL      Realized PnL"),
    Line::from(format!(
        "{:<14}{}",
        format_pnl(detail.statistics.total_pnl),
        format_pnl(detail.statistics.realized_pnl),
    )),
];
```

新增一个小 helper：

```rust
fn format_pnl(value: f64) -> String {
    format!("{value:+.2}")
}
```

区块标题固定为 `Statistics`。

- [x] **Step 4: 重新运行定向测试，确认转绿**

Run: `cargo test -p grid-tui views::instance::tests::renders_grid_detail_execution_activity_and_commands -- --exact`
Expected: PASS

- [x] **Step 5: 运行最终回归测试**

Run: `cargo test -p grid-tui`
Expected: PASS

Run: `cargo test`
Expected: PASS

- [x] **Step 6: 提交本 task**

```bash
git add tui/src/views/instance.rs
git commit -m "feat: render statistics in tui detail"
```

- [x] **Step 7: 回写 commit SHA 到本任务**

Commit SHA: `2d9e7cd31eba84e5464179b6c2456ee36016c1d2`
