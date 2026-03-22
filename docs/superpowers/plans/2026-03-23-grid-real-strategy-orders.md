# Grid 页真实策略挂单 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 Grid 页左侧订单表只显示当前真实存在的策略挂单，不再把库存占用网格层伪装成订单。

**Architecture:** 保持服务端协议不变，只调整 TUI 选择器、表格列和帮助文案。订单表只在存在实时交易所挂单源时显示真实策略订单；没有实时交易所订单源时保持为空；右侧摘要继续保留库存和策略状态语义。

**Tech Stack:** Rust、ratatui、现有 TUI 选择器与渲染测试

---

### Task 1: 先写失败测试，锁定真实订单表语义

**Files:**
- Modify: `tui/src/selectors.rs`
- Modify: `tui/src/render.rs`

- [ ] **Step 1: 在 `tui/src/selectors.rs` 增加失败测试**

新增测试覆盖：

- `strategy_orders` 只返回实时交易所订单源里的策略挂单
- `occupied` 网格层在没有真实挂单时不出现在订单表
- 无实时交易所订单源时，订单表为空

- [ ] **Step 2: 运行选择器测试确认失败**

Run: `cargo test -p grid-platform-tui strategy_orders -- --nocapture`  
Expected: 至少 1 条与旧语义相关的断言失败

- [ ] **Step 3: 在 `tui/src/render.rs` 增加失败断言**

新增或修改 Grid 页渲染测试，确认表头变成 `Side / Price / Qty / Status`，且不再出现 `Placement`

- [ ] **Step 4: 运行渲染测试确认失败**

Run: `cargo test -p grid-platform-tui grid_page_shows_strategy_orders_columns -- --nocapture`  
Expected: 旧表头断言失败

### Task 2: 实现真实订单选择器

**Files:**
- Modify: `tui/src/selectors.rs`

- [ ] **Step 1: 增加真实订单源选择函数**

实现“只有 `exchange_open_orders_source == ExchangeLive` 才显示订单，否则返回空列表”的选择逻辑。

- [ ] **Step 2: 过滤为策略管理订单**

实现策略订单识别规则：

- `client_order_id` 以 `grid_` 开头，或
- 能匹配当前 `strategy.levels` 的 `client_order_id / order_id`

- [ ] **Step 3: 改写 `strategy_orders` 输出结构**

将表格行改成真实订单字段：

- `side`
- `price`
- `qty`
- `status`

- [ ] **Step 4: 运行选择器测试确认通过**

Run: `cargo test -p grid-platform-tui selectors -- --nocapture`  
Expected: 新增测试和现有相关测试通过

### Task 3: 调整 Grid 页表头和帮助文案

**Files:**
- Modify: `tui/src/render.rs`
- Modify: `tui/src/locale.rs`

- [ ] **Step 1: 修改 Grid 页表格列**

把左侧表格列改为 `Side / Price / Qty / Status`

- [ ] **Step 2: 调整帮助页 glossary 文案**

将 `Strategy Orders` 改成“当前真实策略挂单”语义，避免再写成“策略生成的目标订单”

- [ ] **Step 3: 运行渲染测试确认通过**

Run: `cargo test -p grid-platform-tui render -- --nocapture`  
Expected: Grid 页相关测试通过

### Task 4: 全量验证与收尾

**Files:**
- Modify: `TODO.md`

- [ ] **Step 1: 运行格式化**

Run: `cargo fmt`

- [ ] **Step 2: 运行 TUI 目标测试**

Run: `cargo test -p grid-platform-tui -- --nocapture`

- [ ] **Step 3: 运行服务端关键回归，确认本次只改显示语义**

Run: `cargo test -p grid-platform-service --test binance_integration -- --nocapture`

- [ ] **Step 4: 更新 `TODO.md` 最近验证记录**

记录本次新增的 TUI 验证命令

- [ ] **Step 5: 提交**

```bash
git add TODO.md tui/src/selectors.rs tui/src/render.rs tui/src/locale.rs docs/superpowers/specs/2026-03-23-grid-real-strategy-orders-design.md docs/superpowers/plans/2026-03-23-grid-real-strategy-orders.md
git commit -m "调整 Grid 页只显示真实策略挂单"
```
