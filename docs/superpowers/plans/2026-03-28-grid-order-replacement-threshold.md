# Grid Order Replacement Threshold Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让网格在每个 tick 继续重算，但只有当候选限价单的改善足以覆盖 Binance 固定双边手续费和 `5 bps` 安全垫时，才替换现有挂单。

**Architecture:** 主要修改 `engine/src/reconciler.rs`，在生成 `CancelAll + SubmitOrder` 前判断候选订单与当前 `pending_order` 是否等价、方向是否反转、以及价格改善是否超过替换门槛。Binance 固定手续费率通过 engine 内部辅助函数暴露，避免把 exchange adapter 依赖拉进 reconciler。

**Tech Stack:** Rust, cargo test, 现有 engine/reconciler 测试框架

---

### Task 1: 定义替换门槛行为测试

**Files:**
- Modify: `engine/src/reconciler.rs`
- Test: `engine/src/reconciler.rs`

- [ ] **Step 1: 写失败测试，覆盖“候选订单与现有挂单按交易所步长等价时不重挂”**

- [ ] **Step 2: 运行单测确认红灯**

Run: `cargo test -p grid-engine reconcile_keeps_existing_pending_order_when_candidate_order_matches_exchange_rounded_values -- --exact`

- [ ] **Step 3: 写失败测试，覆盖“同方向但改善不足手续费加 `5 bps` 时不重挂”**

- [ ] **Step 4: 运行单测确认红灯**

Run: `cargo test -p grid-engine reconcile_keeps_existing_pending_order_when_price_improvement_does_not_cover_replacement_threshold -- --exact`

- [ ] **Step 5: 写失败测试，覆盖“同方向且改善超过门槛时撤旧换新”**

- [ ] **Step 6: 运行单测确认红灯**

Run: `cargo test -p grid-engine reconcile_replaces_pending_order_when_price_improvement_covers_replacement_threshold -- --exact`

- [ ] **Step 7: 写失败测试，覆盖“方向反转时立即撤旧换新”**

- [ ] **Step 8: 运行单测确认红灯**

Run: `cargo test -p grid-engine reconcile_replaces_pending_order_when_side_flips -- --exact`

### Task 2: 在 reconciler 实现替换门槛

**Files:**
- Modify: `engine/src/reconciler.rs`

- [ ] **Step 1: 提取候选订单构造前后的辅助判断函数**

需要的最小辅助函数：
- 判断候选订单与现有挂单是否按交易所步长等价
- 计算 Binance 固定双边手续费加 `5 bps` 的替换门槛
- 判断同方向挂单是否达到价格改善门槛

- [ ] **Step 2: 在已有 `pending_order` 的普通重算路径中接入门槛判断**

规则：
- 候选单与旧单等价：`NoOp`
- 方向相反：立即替换
- 方向相同但改善不足：`NoOp`
- 方向相同且改善足够：维持现有 `CancelAll + SubmitOrder`

- [ ] **Step 3: 保持现有最小下单门槛逻辑不变**

已有逻辑：
- 新候选单不满足最小门槛时，有旧单则 `CancelAll`，无旧单则 `NoOp`

- [ ] **Step 4: 运行 Task 1 的全部单测，确认转绿**

Run: `cargo test -p grid-engine reconcile_ -- --nocapture`

### Task 3: 回归验证

**Files:**
- Modify: `engine/src/reconciler.rs`

- [ ] **Step 1: 运行 engine 全量测试**

Run: `cargo test -p grid-engine`

- [ ] **Step 2: 运行工作区全量测试**

Run: `cargo test --workspace`

- [ ] **Step 3: 检查格式**

Run: `cargo fmt --all --check`

- [ ] **Step 4: 若全仓格式检查受既有未格式化文件影响，至少格式化本次修改文件并记录结果**

Run: `cargo fmt --all -- engine/src/reconciler.rs`

