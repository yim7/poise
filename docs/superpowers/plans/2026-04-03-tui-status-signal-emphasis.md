# TUI 状态与方向信号强化 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 `poise-tui` 的顶部状态栏、列表页和详情页都能清楚表达全局状态、执行异常、仓位方向和 PnL 方向。

**Architecture:** 这次改动跨 `protocol`、`server projector` 和 `tui` 三层。列表页新增 PnL 列需要先补齐 `TrackListItemView.statistics`，服务端 projector 同步投影，再由 `tui` 用统一的方向渲染函数和样式函数在顶部状态栏、列表页、详情页中展示。

**Tech Stack:** Rust, ratatui, serde, cargo test

---

### Task 1: 统一 TUI 状态栏、执行异常和方向信号

**Files:**
- Modify: `protocol/src/lib.rs`
- Modify: `server/src/projector.rs`
- Modify: `tui/src/app.rs`
- Modify: `tui/src/main.rs`
- Create: `tui/src/signal.rs`
- Modify: `tui/src/theme.rs`
- Modify: `tui/src/views/mod.rs`
- Modify: `tui/src/views/dashboard.rs`
- Modify: `tui/src/views/instance.rs`
- Modify: `tui/tests/fixtures/track_list_response.json`
- Modify: `tui/tests/fixtures/ws_track_list_item_changed.json`
- Modify: `docs/superpowers/specs/2026-04-03-tui-status-signal-emphasis-design.md`
- Modify: `docs/superpowers/plans/2026-04-03-tui-status-signal-emphasis.md`
- Test: `protocol/src/lib.rs`
- Test: `server/src/projector.rs`
- Test: `tui/src/views/mod.rs`
- Test: `tui/src/views/dashboard.rs`
- Test: `tui/src/views/instance.rs`

- [x] **Step 1: 写失败测试，覆盖列表协议新增统计字段**

在 `protocol/src/lib.rs` 补一个反序列化断言，确认 `TrackListItemView` 能解析列表统计字段。

- [x] **Step 2: 运行协议单测确认红灯**

Run: `cargo test -p poise-protocol track_list_response -- --nocapture`
Expected: FAIL，因为当前列表协议还没有 `statistics`

- [x] **Step 3: 写失败测试，覆盖服务端列表投影会带出总 PnL**

在 `server/src/projector.rs` 补断言，确认 `project_list_item` 的统计字段包含 `total_pnl`。

- [x] **Step 4: 运行服务端投影单测确认红灯**

Run: `cargo test -p poise-server projects_execution_badge_from_working_orders -- --nocapture`
Expected: FAIL，因为当前列表项没有统计字段

- [x] **Step 5: 写失败测试，覆盖 TUI 顶部状态栏和底部键位栏职责分离**

在 `tui/src/views/mod.rs` 补断言，确认顶部不再是固定 `Poise`，底部始终显示快捷键文案。

- [x] **Step 6: 写失败测试，覆盖列表页执行异常、仓位方向和 PnL 方向**

在 `tui/src/views/dashboard.rs` 补断言，确认：
- 列表列头包含 `PnL`
- 异常行显示 `! ATTN`
- `Exposure` 显示方向箭头
- `PnL` 显示方向箭头和值

- [x] **Step 7: 写失败测试，覆盖详情页异常块、仓位方向和 PnL 方向**

在 `tui/src/views/instance.rs` 补断言，确认：
- `Overview` 中仓位摘要带方向
- `Statistics` 中 `Total PnL` / `Realized PnL` 带箭头
- `Execution` 异常时显示 `! ATTENTION REQUIRED`

- [x] **Step 8: 运行 TUI 相关单测确认红灯**

Run: `cargo test -p poise-tui renders_runtime_status_in_header_and_keeps_keys_in_footer -- --nocapture`

Run: `cargo test -p poise-tui renders_attention_badge_for_anomalous_track -- --nocapture`
Expected: FAIL，因为当前 UI 还没有这些新语义

- [x] **Step 9: 写最小实现**

实现要求：
- `TrackListItemView` 增加 `statistics`
- `server projector` 列表投影填充统计字段
- `App` 暴露顶部状态栏需要的当前选中实例摘要
- 新增 `tui/src/signal.rs` 统一方向箭头与数值格式
- `Theme` 增加状态栏、执行异常、方向箭头和 PnL 样式
- `views/mod.rs` 重写顶部状态栏和底部键位栏职责
- `dashboard.rs` 改成新的列表列结构和带样式单元格渲染
- `instance.rs` 统一详情页异常块和方向格式

- [x] **Step 10: 运行针对性单测确认转绿**

Run: `cargo test -p poise-protocol track_list_response -- --nocapture`

Run: `cargo test -p poise-server projects_execution_badge_from_working_orders -- --nocapture`

Run: `cargo test -p poise-tui renders_runtime_status_in_header_and_keeps_keys_in_footer -- --nocapture`

Run: `cargo test -p poise-tui renders_attention_badge_for_anomalous_track -- --nocapture`

Run: `cargo test -p poise-tui renders_reduce_signal_and_negative_pnl_in_dashboard -- --nocapture`

Run: `cargo test -p poise-tui renders_grid_detail_execution_activity_and_commands -- --nocapture`

Run: `cargo test -p poise-tui renders_statistics_with_explicit_separator_for_large_pnl_values -- --nocapture`

Run: `cargo test -p poise-tui renders_attention_required_block_with_reason -- --nocapture`

- [x] **Step 11: 运行相关回归测试**

Run: `cargo test -p poise-protocol`

Run: `cargo test -p poise-server projector::tests`

Run: `cargo test -p poise-tui`

- [x] **Step 12: 更新计划记录并提交**

Run:
```bash
git add protocol/src/lib.rs server/src/projector.rs tui/src/app.rs tui/src/theme.rs tui/src/views/mod.rs tui/src/views/dashboard.rs tui/src/views/instance.rs tui/tests/fixtures/track_list_response.json tui/tests/fixtures/ws_track_list_item_changed.json docs/superpowers/specs/2026-04-03-tui-status-signal-emphasis-design.md docs/superpowers/plans/2026-04-03-tui-status-signal-emphasis.md
git commit -m "feat: emphasize tui status and direction signals"
```

**Task 记录：**
- 状态：已完成
- 验收：
  - `cargo test -p poise-protocol`
  - `cargo test -p poise-server projector::tests -- --nocapture`
  - `cargo test -p poise-tui`
- 实现 commit SHA：`2d7c459`
- Review 修复 commit SHA：
  - `5194e2c`
  - `d6edc95`
