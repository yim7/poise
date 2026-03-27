# Grid Replacement Gate Observability Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 grid detail 和 TUI 直接显示“为什么当前 pending order 没有被替换”，并在 activity 中保留原因变化记录。

**Architecture:** 在 engine snapshot 中新增“当前替换门槛原因”字段，并通过新的 `DomainEvent` 把原因变化投影到 activity。server/projector 把当前原因投到 `GridExecutionView`，TUI instance view 在 Execution 区域新增一行显示。

**Tech Stack:** Rust, serde, cargo test, 现有 server/projector/TUI 测试夹具

---

### Task 1: 定义 engine 可观测性模型

**Files:**
- Modify: `core/src/events.rs`
- Modify: `engine/src/runtime.rs`
- Modify: `engine/src/snapshot.rs`
- Modify: `engine/src/reconciler.rs`
- Test: `engine/src/reconciler.rs`

- [ ] **Step 1: 写失败测试，覆盖“rounded match 会写入当前 replacement gate 原因”**

- [ ] **Step 2: 运行单测确认红灯**

Run: `cargo test -p grid-engine replacement_gate_reason_rounded_match -- --exact`

- [ ] **Step 3: 写失败测试，覆盖“改善不足会写入 bps 原因”**

- [ ] **Step 4: 运行单测确认红灯**

Run: `cargo test -p grid-engine replacement_gate_reason_improvement_below_threshold -- --exact`

- [ ] **Step 5: 写失败测试，覆盖“替换发生后会清空当前原因”**

- [ ] **Step 6: 运行单测确认红灯**

Run: `cargo test -p grid-engine replacement_gate_reason_clears_when_order_is_replaced -- --exact`

- [ ] **Step 7: 最小实现 engine 侧结构和返回值**

### Task 2: 定义 activity 事件和去重规则

**Files:**
- Modify: `core/src/events.rs`
- Modify: `engine/src/manager.rs`
- Test: `engine/src/manager.rs`

- [ ] **Step 1: 写失败测试，覆盖“原因首次出现时产生活动事件”**

- [ ] **Step 2: 写失败测试，覆盖“相同原因重复 tick 不重复产生活动事件”**

- [ ] **Step 3: 写失败测试，覆盖“原因变化时产生活动事件”**

- [ ] **Step 4: 运行 manager 相关单测确认红灯**

Run: `cargo test -p grid-engine observe_market_replacement_gate -- --nocapture`

- [ ] **Step 5: 最小实现 manager 中的原因变更比较和事件发出**

### Task 3: 投影到 protocol 和 server detail

**Files:**
- Modify: `protocol/src/lib.rs`
- Modify: `server/src/projector.rs`
- Test: `server/src/projector.rs`

- [ ] **Step 1: 写失败测试，覆盖 detail.execution 中的 replacement gate 字段**

- [ ] **Step 2: 写失败测试，覆盖 activity message 投影**

- [ ] **Step 3: 运行 projector 单测确认红灯**

Run: `cargo test -p grid-server projector::tests::project_ -- --nocapture`

- [ ] **Step 4: 最小实现 protocol 结构和 projector 投影**

### Task 4: 渲染到 TUI

**Files:**
- Modify: `tui/src/views/instance.rs`
- Test: `tui/src/views/instance.rs`
- Test fixture update if needed: `tui/tests/fixtures/grid_detail_view.json`

- [ ] **Step 1: 写失败测试，覆盖 Execution 区域显示 replacement gate 行**

- [ ] **Step 2: 运行 TUI 视图单测确认红灯**

Run: `cargo test -p grid-tui renders_grid_detail_execution_activity_and_commands -- --exact`

- [ ] **Step 3: 最小实现 instance view 渲染**

### Task 5: 回归验证

**Files:**
- Modify: `engine/src/reconciler.rs`
- Modify: `engine/src/manager.rs`
- Modify: `server/src/projector.rs`
- Modify: `tui/src/views/instance.rs`

- [ ] **Step 1: 运行 `grid-engine` 全量测试**

Run: `cargo test -p grid-engine`

- [ ] **Step 2: 运行 `grid-server` 全量测试**

Run: `cargo test -p grid-server`

- [ ] **Step 3: 运行 `grid-tui` 全量测试**

Run: `cargo test -p grid-tui`

- [ ] **Step 4: 运行工作区全量测试**

Run: `cargo test --workspace`

- [ ] **Step 5: 运行格式检查**

Run: `cargo fmt --all --check`

