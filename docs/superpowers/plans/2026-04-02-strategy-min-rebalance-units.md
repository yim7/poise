# Strategy Min Rebalance Units Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 为每个 track 增加策略级最小调仓单位 `min_rebalance_units`，抑制“交易所可下但策略上仍然太碎”的调仓，同时保持原始 `target_exposure`、`ExposureTargetChanged` 和现有 `Working / SubmitPending / Empty` 状态机语义不变。

**Architecture:** `reconciler` 继续只表达原始策略目标，不引入 deadband；策略级门槛落在 `executor planning`，与现有交易所 floor 串联成“两层门槛”。`manager` 只负责把配置传给 executor，并用回归测试锁住“目标仍保留、事件仍发出、旧 working 可 cancel、submit pending 不丢失”这几条边界。

**Tech Stack:** Rust workspace, serde, TOML config, Cargo tests, Markdown

---

## Files And Responsibilities

- Modify: `core/src/strategy.rs`
  为 `TrackConfig` 增加 `min_rebalance_units`、默认值 helper、配置校验和核心单测。
- Modify: `server/src/config.rs`
  解析 TOML 配置，给 `TrackDefinition` 增加同名字段并映射到 `TrackConfig`。
- Modify: `engine/src/executor/planning.rs`
  在 executor planning 中新增策略级最小调仓门槛判断，确保 `SubmitPending` 在 below-threshold 时保留。
- Modify: `engine/src/executor/mod.rs`
  补 executor 级回归测试，锁住 `Empty / Working / SubmitPending` 三种 slot 在策略门槛和交易所 floor 下的行为。
- Modify: `engine/src/manager.rs`
  把 `min_rebalance_units` 传进 executor，并补 manager 级回归测试，锁住 `target_exposure` 和 `ExposureTargetChanged` 语义。
- Modify: `engine/src/reconciler.rs`
  只补必要的 `TrackConfig` struct literal 字段，不改变逻辑。
- Modify: `engine/src/runtime.rs`
  只补必要的 `TrackConfig` struct literal 字段，不改变逻辑。
- Modify: `engine/src/manager.rs`
  Task 1 只补必要的 `TrackConfig` struct literal 字段；Task 3 再补 manager 级行为回归和 `ExecutorInput` 传参。
- Modify: `server/src/assembly.rs`
  只补必要的 `TrackConfig` struct literal 字段，不改变逻辑。
- Modify: `server/src/effect_worker.rs`
  只补必要的 `TrackConfig` struct literal 字段，不改变逻辑。
- Modify: `server/src/http.rs`
  只补必要的 `TrackConfig` struct literal 字段，不改变逻辑。
- Modify: `server/src/query_service.rs`
  只补必要的 `TrackConfig` struct literal 字段，不改变逻辑。
- Modify: `server/src/runtime.rs`
  只补必要的 `TrackConfig` struct literal 字段，不改变逻辑。
- Modify: `server/src/websocket.rs`
  只补必要的 `TrackConfig` struct literal 字段，不改变逻辑。
- Modify: `server/src/write_service.rs`
  只补必要的 `TrackConfig` struct literal 字段，不改变逻辑。
- Modify: `docs/superpowers/specs/2026-04-02-strategy-min-rebalance-units-design.md`
  如果实现中对比较容差或默认值表述有细化，回写成最终一致的设计说明。
- Modify: `docs/superpowers/plans/2026-04-02-strategy-min-rebalance-units.md`
  执行时勾选步骤，并在每个完成 task 后记录 commit SHA。

### Task 1: 引入 `min_rebalance_units` 配置模型

**Files:**
- Modify: `core/src/strategy.rs`
- Modify: `server/src/config.rs`
- Modify: `engine/src/manager.rs`
- Modify: `engine/src/reconciler.rs`
- Modify: `engine/src/runtime.rs`
- Modify: `server/src/assembly.rs`
- Modify: `server/src/effect_worker.rs`
- Modify: `server/src/http.rs`
- Modify: `server/src/query_service.rs`
- Modify: `server/src/runtime.rs`
- Modify: `server/src/websocket.rs`
- Modify: `server/src/write_service.rs`
- Test: `core/src/strategy.rs`
- Test: `server/src/config.rs`

- [x] **Step 1: 在 `TrackConfig` 和 `TrackDefinition` 加字段，先写失败测试**

在 `core/src/strategy.rs` 和 `server/src/config.rs` 先补测试，覆盖：

```rust
// core/src/strategy.rs
#[test]
fn validate_rejects_negative_min_rebalance_units() { /* ... */ }

#[test]
fn validate_rejects_non_finite_min_rebalance_units() { /* ... */ }

// server/src/config.rs
#[test]
fn defaults_min_rebalance_units_to_point_five() { /* ... */ }
```

- [x] **Step 2: 运行测试确认失败**

Run:

- `cargo test -p poise-core strategy::tests::validate_rejects_negative_min_rebalance_units -- --exact --nocapture`
- `cargo test -p poise-server config::tests::defaults_min_rebalance_units_to_point_five -- --exact --nocapture`

Expected:

- `poise-core` 测试失败，因为 `TrackConfig` 还没有该字段和校验
- `poise-server` 测试失败，因为 TOML 和默认映射还没有该字段

- [x] **Step 3: 做最小实现，补齐默认值与校验**

在 `core/src/strategy.rs`：

```rust
pub struct TrackConfig {
    // ...
    #[serde(default = "default_min_rebalance_units")]
    pub min_rebalance_units: f64,
}
```

新增：

- `default_min_rebalance_units() -> f64 { 0.5 }`
- `validate_config()` 中增加：
  - `config.min_rebalance_units >= 0.0`
  - `config.min_rebalance_units.is_finite()`

在 `server/src/config.rs`：

- `TrackDefinition` 增加 `min_rebalance_units`
- 通过 `#[serde(default = "default_min_rebalance_units")]` 提供默认值
- `track_config()` 把该字段映射到 `TrackConfig`

然后补齐所有 `TrackConfig { ... }` literal，优先复用本模块已有的 `test_config()` / 构造 helper；如果某处只能写字面量，统一写 `0.5`，并明确要求它与 `default_min_rebalance_units()` 保持一致，只做编译修复，不顺手改逻辑。

- [x] **Step 4: 跑核心回归和编译检查**

Run:

- `cargo test -p poise-core strategy::tests:: -- --nocapture`
- `cargo test -p poise-server config::tests:: -- --nocapture`
- `cargo test -p poise-engine --no-run`
- `cargo test -p poise-server --no-run`

Expected: 全部 PASS，workspace 中涉及 `TrackConfig` literal 的 crate 都能通过测试构建。

- [x] **Step 5: Commit**

```bash
git add core/src/strategy.rs server/src/config.rs engine/src/manager.rs engine/src/reconciler.rs engine/src/runtime.rs server/src/assembly.rs server/src/effect_worker.rs server/src/http.rs server/src/query_service.rs server/src/runtime.rs server/src/websocket.rs server/src/write_service.rs
git commit -m "feat(core): add min_rebalance_units to track config"
```

Commit: `49eb9f6`（Task 1 主提交，误落到本地 `main`） / `8b4978f`（review：统一默认值来源） / `e4572c2`（当前分支收尾：TOML 边界校验、storage 兼容回归、plan/spec 恢复）

### Task 2: 在 executor planning 引入策略级最小调仓门槛

**Files:**
- Modify: `engine/src/executor/planning.rs`
- Modify: `engine/src/executor/mod.rs`
- Test: `engine/src/executor/mod.rs`

- [ ] **Step 1: 写 executor 级失败测试，锁住三种 slot 语义**

在 `engine/src/executor/mod.rs` 增加三条测试：

```rust
#[test]
fn small_target_change_below_min_rebalance_units_does_not_submit_new_order() { /* Empty */ }

#[test]
fn small_target_change_below_min_rebalance_units_cancels_existing_working_order() { /* Working */ }

#[test]
fn small_target_change_below_min_rebalance_units_keeps_submit_pending_slot() { /* SubmitPending */ }
```

同时保留并扩展现有 `plan_uses_rounded_order_values_when_checking_exchange_floor`，确认交易所 floor 仍基于 round 后订单语义。

- [ ] **Step 2: 运行测试确认失败**

Run:

- `cargo test -p poise-engine executor::tests::small_target_change_below_min_rebalance_units_does_not_submit_new_order -- --exact --nocapture`
- `cargo test -p poise-engine executor::tests::small_target_change_below_min_rebalance_units_cancels_existing_working_order -- --exact --nocapture`
- `cargo test -p poise-engine executor::tests::small_target_change_below_min_rebalance_units_keeps_submit_pending_slot -- --exact --nocapture`

Expected: FAIL，原因是 planning 还没有策略级门槛判断。

- [ ] **Step 3: 最小实现，新增策略门槛 helper**

在 `engine/src/executor/planning.rs`：

- 给 `ExecutorInput` 增加字段：
  - `min_rebalance_units: f64`
- 在 `desired_inventory_order(...)` 或紧邻它的唯一 helper 中先做：

```rust
let inventory_gap = current_exposure.delta(target_exposure);
if inventory_gap.0.abs() < min_rebalance_units {
    return None;
}
```

约束：

- 对外语义保持 `gap < min => 抑制`, `gap >= min => 允许继续执行`
- 若引入内部容差，只允许放进单一 helper，不能散落多处
- 不把交易所 floor 判断和策略门槛混成同一个 helper
- 在落代码前先核对：当前 `desired_order = None` 是否统一进入 `diff_desired_orders(None)` 这条语义链，并且 `Working -> CancelOrder` 就在这条路径里；如果代码结构不是这样，先做最小重构把“无 desired 时的 slot diff”收敛到唯一入口，再接策略门槛，避免出现一部分 `None` 走 cancel、一部分 `None` 走 `NoOp` 的隐性分叉。

在 `diff_desired_orders(None)` 分支：

- `SlotState::SubmitPending` 保留 pending slot，返回 `NoOp`
- `Working(order_id=Some)` 继续按现有 `CancelOrder` 语义处理
- `Empty` 继续 `NoOp`

- [ ] **Step 4: 运行 executor 回归**

Run:

- `cargo test -p poise-engine executor::tests:: -- --nocapture`

Expected: 全部 PASS，尤其是新加的 `below_min_rebalance_units` 三条和现有 `plan_uses_rounded_order_values_when_checking_exchange_floor` 同时为绿。

- [ ] **Step 5: Commit**

```bash
git add engine/src/executor/planning.rs engine/src/executor/mod.rs
git commit -m "feat(engine): add strategy-level min rebalance gate"
```

Commit: `<fill during execution>`

### Task 3: 在 manager 锁住目标与事件语义

**Files:**
- Modify: `engine/src/manager.rs`
- Test: `engine/src/manager.rs`

- [ ] **Step 1: 写 manager 级失败测试，锁住目标不被改写**

在 `engine/src/manager.rs` 增加测试：

```rust
#[test]
fn observe_market_keeps_strategy_target_while_suppressing_small_rebalance() { /* target_exposure 仍是原始策略目标 */ }

#[test]
fn observe_market_cancels_existing_working_order_when_small_rebalance_is_below_min_rebalance_units() { /* Working */ }

#[test]
fn observe_market_keeps_submit_pending_slot_when_small_rebalance_is_below_min_rebalance_units() { /* SubmitPending */ }
```

覆盖点：

- `target_exposure` 保持原始策略目标
- `ExposureTargetChanged` 仍照常发出
- `Working` 下会 cancel
- `SubmitPending` 不丢 slot

- [ ] **Step 2: 运行测试确认失败**

Run:

- `cargo test -p poise-engine manager::tests::observe_market_keeps_strategy_target_while_suppressing_small_rebalance -- --exact --nocapture`
- `cargo test -p poise-engine manager::tests::observe_market_cancels_existing_working_order_when_small_rebalance_is_below_min_rebalance_units -- --exact --nocapture`
- `cargo test -p poise-engine manager::tests::observe_market_keeps_submit_pending_slot_when_small_rebalance_is_below_min_rebalance_units -- --exact --nocapture`

Expected: FAIL，原因是 `manager` 还没把 `min_rebalance_units` 传进 executor。

- [ ] **Step 3: 最小实现，把配置传进 executor**

在 `engine/src/manager.rs` 的 `plan_inventory_execution_for_track(...)` 和任何构造 `ExecutorInput` 的路径里补传：

```rust
min_rebalance_units: track.config.min_rebalance_units,
```

要求：

- `reconciler::reconcile_target(...)` 不新增 deadband
- `target_exposure` 和 `ExposureTargetChanged` 的语义保持不变
- 不在 `manager` 新增执行状态机旁路判断

- [ ] **Step 4: 跑 manager 回归**

Run:

- `cargo test -p poise-engine manager::tests::observe_market_ -- --nocapture`

Expected: 全部 PASS，包括现有 replacement gate、submit pending、gap stats 和新加的策略门槛测试。

- [ ] **Step 5: Commit**

```bash
git add engine/src/manager.rs
git commit -m "feat(engine): preserve target semantics under min rebalance gate"
```

Commit: `<fill during execution>`

### Task 4: 全量回归与文档同步

**Files:**
- Modify: `docs/superpowers/specs/2026-04-02-strategy-min-rebalance-units-design.md`
- Modify: `docs/superpowers/plans/2026-04-02-strategy-min-rebalance-units.md`

- [ ] **Step 1: 跑最终回归**

Run:

- `cargo test -p poise-core strategy::tests:: -- --nocapture`
- `cargo test -p poise-engine executor::tests:: -- --nocapture`
- `cargo test -p poise-engine manager::tests::observe_market_ -- --nocapture`
- `cargo test -p poise-server --no-run`
- `cargo test --workspace --no-run`

Expected: 全部 PASS；workspace 中所有依赖 `TrackConfig` 的 crate 都能通过测试构建。

- [ ] **Step 2: 同步 spec 与 plan**

如果实现对以下细节有落地差异，回写到 spec：

- 内部浮点容差写法
- 默认值 helper 命名
- `Working / SubmitPending / Empty` 在“过策略门槛但不过交易所 floor”时的最终行为描述

然后在本 plan 中勾选已完成步骤，并把每个 task 的 commit SHA 回写到对应位置。

- [ ] **Step 3: Commit**

```bash
git add docs/superpowers/specs/2026-04-02-strategy-min-rebalance-units-design.md docs/superpowers/plans/2026-04-02-strategy-min-rebalance-units.md
git commit -m "docs: sync strategy min rebalance units plan and spec"
```

Commit: `<fill during execution>`
