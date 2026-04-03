# TUI 详情页重画 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 `poise-tui` 的详情页先展示醒目的状态摘要，再完整展示关键策略参数，并在小终端高度下按固定规则退化。

**Architecture:** 这次改动分两段推进。第一段先扩展稳定 detail contract，把详情页需要的原始策略事实从 `server` 投影到 `protocol` 和 TUI fixture，保证显示所需数据是完整且稳定的。第二段把详情页渲染重构成“布局策略 + 区块渲染”，用单一布局策略函数决定 `标准 / 紧凑 / 最小` 模式，并把 `Activity` / `Diagnostics` 收进统一的 `Trace` 区。

**Tech Stack:** Rust, ratatui, serde, cargo test

---

## 文件结构

- `protocol/src/lib.rs`
  详情读模型 contract。扩展 `GridStrategyView`，只放稳定的原始策略字段。
- `server/src/read_model.rs`
  从 `TrackRuntimeSnapshot` 提取详情页需要的策略事实，避免 projector 再回看 runtime 内部结构。
- `server/src/projector.rs`
  把 read model 投影成 detail contract，并保持 TUI 不复制业务推导。
- `server/src/http.rs`
  详情接口定向回归，确认扩展后的 `TrackDetailView` 能通过 HTTP 返回。
- `tui/src/protocol.rs`
  fixture 反序列化测试入口，锁住扩展后的 detail wire format。
- `tui/tests/fixtures/track_detail_view.json`
  详情 fixture，补齐完整策略字段。
- `tui/tests/fixtures/ws_track_detail_changed.json`
  WebSocket 详情 fixture，补齐完整策略字段。
- `tui/src/views/instance_layout.rs`
  新增的详情页布局策略模块，负责模式选择和区块约束分配。
- `tui/src/views/mod.rs`
  注册新的 `instance_layout` 模块。
- `tui/src/views/instance.rs`
  详情页主渲染逻辑，消费布局策略结果并渲染 `Status / Overview / Strategy / Execution / Statistics / Trace`。
- `docs/superpowers/specs/2026-04-03-tui-track-detail-redesign-design.md`
  如果实现过程中需要澄清边界，只同步已经确认的设计收敛，不扩 scope。
- `docs/superpowers/plans/2026-04-03-tui-track-detail-redesign.md`
  执行时回写任务状态、验收命令和 commit SHA。

### Task 1: 扩展详情协议和服务端投影

**Files:**
- Modify: `protocol/src/lib.rs`
- Modify: `server/src/read_model.rs`
- Modify: `server/src/projector.rs`
- Modify: `server/src/http.rs`
- Modify: `tui/src/protocol.rs`
- Modify: `tui/tests/fixtures/track_detail_view.json`
- Modify: `tui/tests/fixtures/ws_track_detail_changed.json`
- Modify: `docs/superpowers/plans/2026-04-03-tui-track-detail-redesign.md`
- Test: `protocol/src/lib.rs`
- Test: `server/src/projector.rs`
- Test: `server/src/http.rs`
- Test: `tui/src/protocol.rs`

- [x] **Step 1: 写失败测试，锁住扩展后的 detail strategy contract**

在 `tui/src/protocol.rs` 和 `protocol/src/lib.rs` 补断言，确认 detail strategy 至少包含：

```rust
assert_eq!(detail.strategy.long_exposure_units, 8.0);
assert_eq!(detail.strategy.short_exposure_units, 8.0);
assert_eq!(detail.strategy.notional_per_unit, 375.0);
assert_eq!(detail.strategy.min_rebalance_units, 0.5);
```

同时更新 `tui/tests/fixtures/track_detail_view.json` 和 `tui/tests/fixtures/ws_track_detail_changed.json`，让 fixture 先表达新 contract。

- [x] **Step 2: 运行协议和 TUI 反序列化测试，确认红灯**

Run: `cargo test -p poise-protocol tests::deserializes_track_stream_detail_changed_with_track_id -- --exact`

Run: `cargo test -p poise-tui protocol::tests::deserializes_grid_detail_view -- --exact`

Expected: FAIL，因为 `GridStrategyView` 还没有这些字段。

- [x] **Step 3: 写失败测试，锁住 read model 和 projector 会带出完整策略字段**

在 `server/src/projector.rs` 的 detail 投影测试里补断言：

```rust
assert_eq!(detail.strategy.long_exposure_units, 8.0);
assert_eq!(detail.strategy.short_exposure_units, 8.0);
assert_eq!(detail.strategy.notional_per_unit, 375.0);
assert_eq!(detail.strategy.min_rebalance_units, 0.5);
```

在 `server/src/http.rs` 的详情接口测试里补断言，确认 HTTP 返回的 `TrackDetailView` 也带出这些字段。

新增 HTTP 测试名固定为：

```rust
#[tokio::test]
async fn get_track_detail_returns_track_detail_view() { /* ... */ }
```

- [x] **Step 4: 运行服务端定向测试，确认红灯**

Run: `cargo test -p poise-server projector::tests::project_detail_includes_available_commands_and_activity -- --exact`

Run: `cargo test -p poise-server http::tests::get_track_detail_returns_track_detail_view -- --exact`

Expected: FAIL，因为 `TrackReadModel` 和 projector 还没有补这些策略字段。

- [x] **Step 5: 写最小实现，补齐 detail contract、read model 和 projector**

实现范围只限稳定原始策略事实：

```rust
pub struct GridStrategyView {
    pub lower_price: f64,
    pub upper_price: f64,
    pub long_exposure_units: f64,
    pub short_exposure_units: f64,
    pub notional_per_unit: f64,
    pub min_rebalance_units: f64,
    pub shape_family: ShapeFamily,
    pub out_of_band_policy: OutOfBandPolicy,
}
```

`server/src/read_model.rs` 同步从 `snapshot.config` 提取这些字段，`server/src/projector.rs` 只做字段映射，不新增派生展示值。

- [x] **Step 6: 运行定向测试，确认转绿**

Run: `cargo test -p poise-protocol tests::deserializes_track_stream_detail_changed_with_track_id -- --exact`

Run: `cargo test -p poise-tui protocol::tests::deserializes_grid_detail_view -- --exact`

Run: `cargo test -p poise-server projector::tests::project_detail_includes_available_commands_and_activity -- --exact`

Run: `cargo test -p poise-server http::tests::get_track_detail_returns_track_detail_view -- --exact`

Expected: PASS

- [x] **Step 7: 运行相关回归测试**

Run: `cargo test -p poise-protocol`

Run: `cargo test -p poise-server projector::tests -- --nocapture`

Run: `cargo test -p poise-server http::tests::get_track_detail_returns_track_detail_view -- --exact`

Run: `cargo test -p poise-tui protocol::tests::deserializes_grid_stream_detail_changed -- --exact`

Expected: PASS

- [ ] **Step 8: 回写计划记录并提交**

Run:

```bash
git add protocol/src/lib.rs server/src/read_model.rs server/src/projector.rs server/src/http.rs tui/src/protocol.rs tui/tests/fixtures/track_detail_view.json tui/tests/fixtures/ws_track_detail_changed.json docs/superpowers/plans/2026-04-03-tui-track-detail-redesign.md
git commit -m "feat: expose detail strategy facts"
```

**Task 记录：**
- 状态：已完成
- 验收：
  - `cargo test -p poise-protocol`
  - `cargo test -p poise-server projector::tests -- --nocapture`
  - `cargo test -p poise-server http::tests::get_track_detail_returns_track_detail_view -- --exact`
  - `cargo test -p poise-tui protocol::tests::deserializes_grid_stream_detail_changed -- --exact`
- 实现 commit SHA：

### Task 2: 重画详情页并收敛布局退化规则

**Files:**
- Create: `tui/src/views/instance_layout.rs`
- Modify: `tui/src/views/mod.rs`
- Modify: `tui/src/views/instance.rs`
- Modify: `tui/src/theme.rs`
- Modify: `tui/tests/fixtures/track_detail_view.json`
- Modify: `tui/tests/fixtures/track_diagnostics_view.json`
- Modify: `docs/superpowers/plans/2026-04-03-tui-track-detail-redesign.md`
- Test: `tui/src/views/instance_layout.rs`
- Test: `tui/src/views/instance.rs`

- [ ] **Step 1: 写失败测试，锁住新的详情页主区块和命令位置**

在 `tui/src/views/instance.rs` 新增测试 `renders_redesigned_detail_sections_and_status_commands`，确认默认详情页：

- 包含 `Status`、`Overview`、`Strategy`、`Execution`、`Statistics`、`Trace`
- 不再包含独立 `Commands`
- `Status` 区块中包含紧凑命令提示，例如 `commands: p pause`
- `Strategy` 区块显示新增字段，例如 `min rebalance units`

- [ ] **Step 2: 写失败测试，锁住 `Trace` 行为和 diagnostics 归位**

新增测试 `renders_trace_panel_with_diagnostics_in_debug_view`，补两组断言：

```rust
assert!(text.contains("Trace"));
assert!(text.contains("Activity"));
assert!(!text.contains("Diagnostics"));
```

以及开启 diagnostics 后：

```rust
assert!(text.contains("Trace"));
assert!(text.contains("Diagnostics"));
assert!(text.contains("target exposure 3.5000 -> 4.0000"));
```

要求 diagnostics 只出现在 `Trace` 区，不新增独立主区块。

- [ ] **Step 3: 写失败测试，直接锁住布局策略模块的模式选择**

在 `tui/src/views/instance_layout.rs` 新增模块测试，直接断言：

- `Rect::new(0, 0, 100, 36)` -> `DetailLayoutMode::Standard`
- `Rect::new(0, 0, 100, 24)` -> `DetailLayoutMode::Compact`
- `Rect::new(0, 0, 100, 16)` -> `DetailLayoutMode::Minimal`

并额外断言最小模式下：

- `Statistics` 独立区块不可见
- `Trace` 不可见

这些测试只验证模式和区块可见性，不验证最终文案。

- [ ] **Step 4: 写失败测试，锁住 `标准 / 紧凑 / 最小` 三档退化在渲染层的表现**

在 `tui/src/views/instance.rs` 增加不同高度的渲染断言，至少覆盖：

- `100x36`：标准模式，六个主区块都可见
- `100x24`：紧凑模式，`Statistics` 压缩为摘要，但 `Trace` 仍可见
- `100x16`：最小模式，只保留 `Status / Overview / Strategy / Execution`，`Trace` 隐藏

同时确认模式切换不依赖内容多少，只依赖渲染区域高度。

- [ ] **Step 5: 运行 TUI 定向测试，确认红灯**

Run: `cargo test -p poise-tui instance_layout -- --nocapture`

Run: `cargo test -p poise-tui renders_redesigned_detail_sections_and_status_commands -- --exact`

Run: `cargo test -p poise-tui renders_trace_panel_with_diagnostics_in_debug_view -- --exact`

Run: `cargo test -p poise-tui renders_compact_detail_layout_when_height_is_limited -- --exact`

Run: `cargo test -p poise-tui renders_minimal_detail_layout_when_height_is_tight -- --exact`

Expected: FAIL，因为当前还没有布局策略模块测试，也没有新的详情页布局行为。

- [ ] **Step 6: 写最小实现，先落布局策略模块，再重写详情页渲染**

新增 `tui/src/views/instance_layout.rs`，把模式选择和区块约束集中到单一函数：

```rust
pub enum DetailLayoutMode {
    Standard,
    Compact,
    Minimal,
}

pub struct DetailSections {
    pub mode: DetailLayoutMode,
    // 各区块 Rect / 可见性
}

pub fn resolve_detail_layout(area: Rect) -> DetailSections {
    // 只按 body 区高度选择模式
}
```

`tui/src/views/instance.rs` 只消费 `DetailSections`：

- 渲染 `Status / Overview / Strategy / Execution / Statistics / Trace`
- 把命令提示压进 `Status`
- 把 `Activity` / `Diagnostics` 收进 `Trace`
- 最小模式隐藏 `Trace`，并把统计摘要并入 `Overview`

如果现有样式不够，再在 `tui/src/theme.rs` 增加详情页状态块或 Trace 所需的最小样式 helper，但不要把布局判断放进样式层。

- [ ] **Step 7: 运行定向测试，确认转绿**

Run: `cargo test -p poise-tui instance_layout -- --nocapture`

Run: `cargo test -p poise-tui renders_redesigned_detail_sections_and_status_commands -- --exact`

Run: `cargo test -p poise-tui renders_trace_panel_with_diagnostics_in_debug_view -- --exact`

Run: `cargo test -p poise-tui renders_compact_detail_layout_when_height_is_limited -- --exact`

Run: `cargo test -p poise-tui renders_minimal_detail_layout_when_height_is_tight -- --exact`

Run: `cargo test -p poise-tui renders_attention_required_block_with_reason -- --exact`

Expected: PASS

- [ ] **Step 8: 运行 TUI 回归测试**

Run: `cargo test -p poise-tui`

Expected: PASS

- [ ] **Step 9: 回写计划记录并提交**

Run:

```bash
git add tui/src/views/mod.rs tui/src/views/instance.rs tui/src/views/instance_layout.rs tui/src/theme.rs tui/tests/fixtures/track_detail_view.json tui/tests/fixtures/track_diagnostics_view.json docs/superpowers/plans/2026-04-03-tui-track-detail-redesign.md
git commit -m "feat: redesign tui track detail view"
```

**Task 记录：**
- 状态：未开始
- 验收：
  - `cargo test -p poise-tui`
- 实现 commit SHA：
