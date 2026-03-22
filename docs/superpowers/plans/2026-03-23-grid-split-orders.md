# Grid 页双栏订单与距离显示 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 Grid 页把买卖策略挂单分栏显示，并为每条订单补充相对当前价的比例距离，同时把右侧摘要改成双列 key-value 布局。

**Architecture:** 保持服务端协议和 TUI 状态结构不变，只调整 `selectors` 的订单视图模型和 `render` 的 Grid 页面布局。左侧订单区继续只展示真实策略挂单，但按方向拆分并增加距离百分比；右侧两个摘要面板改成双列排版以降低扫读成本。

**Tech Stack:** Rust、ratatui、现有 TUI 选择器/快照测试

---

### Task 1: 先写失败测试，锁定新布局与距离语义

**Files:**
- Modify: `tui/src/selectors.rs`
- Modify: `tui/src/render.rs`

- [ ] **Step 1: 在 `tui/src/selectors.rs` 增加失败测试**

覆盖以下行为：

- `strategy_orders` 输出订单距离百分比
- 买单按更接近当前价排序，卖单也按更接近当前价排序
- 视图模型可以按方向拆成两组

- [ ] **Step 2: 运行选择器测试确认失败**

Run: `cargo test -p grid-platform-tui strategy_orders -- --nocapture`  
Expected: 至少 1 条与新距离字段或新排序相关的断言失败

- [ ] **Step 3: 在 `tui/src/render.rs` 增加失败断言**

新增或修改 Grid 页渲染测试，确认：

- 左侧出现 `BUY` 与 `SELL` 两个子表
- 表头包含距离百分比列
- 右侧两个面板改成双列 key-value 排版

- [ ] **Step 4: 运行渲染测试确认失败**

Run: `cargo test -p grid-platform-tui grid_page -- --nocapture`  
Expected: 旧布局相关断言失败

### Task 2: 调整选择器，补充方向与距离信息

**Files:**
- Modify: `tui/src/selectors.rs`

- [ ] **Step 1: 扩展订单视图模型**

为策略订单视图增加距离百分比字段，并保留现有真实挂单过滤规则。

- [ ] **Step 2: 实现距离百分比计算**

按 `((order_price - last_price) / last_price) * 100` 计算，格式化为 `+0.12%` / `-0.35%`。

- [ ] **Step 3: 实现方向内排序**

让买单按价格降序、卖单按价格升序，保证顶部订单最接近当前价。

- [ ] **Step 4: 运行选择器测试确认通过**

Run: `cargo test -p grid-platform-tui selectors -- --nocapture`  
Expected: 新增测试和相关既有测试通过

### Task 3: 调整 Grid 页布局

**Files:**
- Modify: `tui/src/render.rs`
- Modify: `tui/src/locale.rs`

- [ ] **Step 1: 改写左侧订单区为双栏**

把原单表改成左右两个子表，分别渲染买单和卖单，并显示距离列。

- [ ] **Step 2: 改写右侧摘要面板**

将 `Grid Summary` 与 `Operator Notes` 改成双列 key-value 布局，压缩纵向占用。

- [ ] **Step 3: 必要时补充文案**

如果表头或标签需要新增本地化文案，在 `tui/src/locale.rs` 中补齐中英文文本。

- [ ] **Step 4: 运行渲染测试确认通过**

Run: `cargo test -p grid-platform-tui render -- --nocapture`  
Expected: Grid 页相关快照与渲染断言通过

### Task 4: 验证与收尾

**Files:**
- Modify: `TODO.md`

- [ ] **Step 1: 运行格式化**

Run: `cargo fmt`

- [ ] **Step 2: 运行 TUI 相关测试**

Run: `cargo test -p grid-platform-tui -- --nocapture`

- [ ] **Step 3: 更新 `TODO.md` 验证记录**

记录本次新增的 Grid 页验证命令

- [ ] **Step 4: 同步任务状态**

把本次已完成的 Grid 页面体验优化项同步到 TODO 清单
