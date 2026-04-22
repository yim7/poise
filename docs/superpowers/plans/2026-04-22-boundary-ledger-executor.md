# Boundary Ledger Executor Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 以重新实现的方式落地基于单曲线 `boundary progress ledger` 的执行器内核，先完成最小可工作的 `CatchUpPolicy` 垂直切片，再补 `CurveMakerPolicy` 和串行仲裁。

**Architecture:** 上游继续只有一条 `desired_exposure(price)` 曲线；执行内核直接重建成 `level / boundary -> boundary progress ledger -> execution policy -> live order binding`。这次不做旧模型迁移设计：`round + slot`、`rebalance_trigger`、旧 recovery helper 和依赖它们的夹具都按“可删除旧实现”来处理，只保留对新内核仍有价值的 effect 接口和外部协议。

**Tech Stack:** Rust workspace, Cargo, Serde, chrono, Markdown

---

## Design Constraints

- `BoundaryId / BoundaryProgress / BoundaryOperation / LiveOrderBinding` 是新的共享执行边界；实现时直接围绕它们写新代码，不为旧 `round + slot` 结构保留适配层。
- 不允许同时保留两套执行状态：
  - `slots` 与 `bindings`
  - `active_round` 与 `boundary progress`
  - `round_policy` 与 `policy arbitration`
- `current_exposure` 始终是唯一物理仓位真值；`expected_exposure` 只做账实校验，不得进入 planner owner。
- `CurveMakerPolicy` 的宽限时机只能留在 policy 私有状态，不得再泄漏回通用 binding 结构。
- `TrackConfig` / profile revision 变化时只能有一种行为：重开新账本，不做 remap。
- 不要求兼容旧 snapshot 形状、旧 executor fixture 或旧 helper；如果旧测试完全绑定 `round + slot`，直接删掉并用新语义重写。
- 第一阶段外部 effect 接口继续保持：
  - `ExecutionAction`
  - `OrderRequest / OrderReceipt / OrderStatus`
- 第一阶段不改 `server`、`protocol` 对外字段形状；新的边界账本仍只存在于 engine 内部。

## Files And Responsibilities

### New executor domain files

- Create: `engine/src/executor/boundary.rs`
  定义 `BoundaryId`、`BoundaryBlueprint`、`BoundaryDirection`、`BoundaryOperation`，以及曲线离散化和 `trigger_price_for_boundary(...)` 纯函数。
- Create: `engine/src/executor/ledger.rs`
  定义 `BoundaryProgress`、`BoundaryLedgerState`、`BoundaryLedgerView` 及 `remaining / due / expected_exposure` 派生逻辑。
- Create: `engine/src/executor/binding.rs`
  定义 `LiveOrderBinding`、`BindingStatus`、`BindingProposal`、`BindingPolicyState`、diff 匹配键和 active binding 约束。
- Create: `engine/src/executor/policy.rs`
  定义 `PolicyKind`、`CoverageReservation`、串行 policy runner，以及 `CatchUpPolicy` / `CurveMakerPolicy` / `FlattenPolicy` / `ManualOverridePolicy` 的 shared interface。

### Existing executor files to rewrite

- Modify: `engine/src/executor/mod.rs`
  重新导出 boundary/ledger/binding/policy/planning/recovery/recording；删除旧 round/slot/rebalance 入口。
- Modify: `engine/src/executor/planning.rs`
  改成 `plan(input) -> ledger view -> policy runner -> binding diff -> effects` 的总入口。
- Modify: `engine/src/executor/recording.rs`
  改成“先吸收到 binding，再回写 boundary progress”的两阶段回报吸收。
- Modify: `engine/src/executor/recovery.rs`
  改成基于 `BoundaryLedgerState + LiveOrderBinding` 的恢复和认领。
- Delete: `engine/src/executor/round_policy.rs`
- Delete: `engine/src/executor/rebalance_trigger.rs`
- Delete: `engine/src/executor/slots.rs`

### Runtime / snapshot / manager boundary

- Modify: `engine/src/runtime.rs`
  用 `BoundaryLedgerState + Vec<LiveOrderBinding>` 替换 `active_round + slots + stats` 这组执行状态；保留读模型所需的派生输入，不在 runtime 中持久化 `mode` 和 `gap`。
- Modify: `engine/src/snapshot.rs`
  持久化新的 `ExecutorState` 结构。
- Modify: `engine/src/manager.rs`
  改成把曲线、物理仓位、market/order 事实交给新 planner，不再调用旧 round/slot 语义。

### Effect boundary

- Keep: `engine/src/execution_plan.rs`
  继续作为 executor 对外 effect 边界；新的 `planning.rs` 必须继续产出 `ExecutionAction`，并复用 `round_to_step`、`is_meetable_minimum` 这类已有 helper。若某个 helper 在新设计下彻底无用，只允许在最终清理 task 里删除。

### Cross-cutting consumers

- Modify: `engine/src/persisted_runtime.rs`
  如有 `ExecutorState` 兼容/codec 校验，需要随 snapshot 一起调整。
- Modify: `engine/src/executor/mod.rs` tests
- Modify: `engine/src/runtime.rs` tests
- Modify: `engine/src/executor/recording.rs` tests
- Modify: `engine/src/executor/recovery.rs` tests
- Modify: `engine/src/manager.rs` tests
- Modify: `docs/superpowers/specs/2026-04-22-curve-boundary-ledger-execution-design.md`
  若实现中有具体类型名或边界修正，回写最终设计说明。
- Modify: `docs/superpowers/plans/2026-04-22-boundary-ledger-executor.md`
  执行时勾选任务并记录 commit SHA。

## Non-Goals

- 不实现多曲线合成
- 不实现跨 revision remap
- 不把 boundary/binding 结构直接暴露到 protocol / server API
- 不在第一阶段给不同 policy 引入独立预算 owner
- 不保留旧 executor 作为 fallback 开关

## Task 1: 落 pure boundary domain 与曲线离散化

**Files:**

- Create: `engine/src/executor/boundary.rs`
- Modify: `engine/src/executor/mod.rs`
- Test: `engine/src/executor/boundary.rs`

- [x] **Step 1: 先写失败测试，锁住 boundary 原语和反解**

在 `engine/src/executor/boundary.rs` 新增测试，至少覆盖：

```rust
#[test]
fn discretize_boundaries_builds_adjacent_levels_across_full_curve_range() {}

#[test]
fn trigger_price_for_boundary_matches_linear_shape_boundary() {}

#[test]
fn boundary_id_uses_profile_revision_and_adjacent_exposures_only() {}

#[test]
fn profile_revision_for_config_is_deterministic_for_identical_configs() {}
```

覆盖点：

- 边界目录来自整条曲线的相邻 level 图，不依赖运行时仓位
- `trigger_price_for_boundary()` 对线性 shape 可稳定反解
- `BoundaryId` 只编码 revision 和相邻 exposure 边界，不编码方向
- `ProfileRevision` 的生成规则是确定性的

- [x] **Step 2: 运行定向测试，确认当前实现失败**

Run:

- `cargo test -p poise-engine executor::boundary::tests::discretize_boundaries_builds_adjacent_levels_across_full_curve_range -- --exact --nocapture`
- `cargo test -p poise-engine executor::boundary::tests::trigger_price_for_boundary_matches_linear_shape_boundary -- --exact --nocapture`
- `cargo test -p poise-engine executor::boundary::tests::boundary_id_uses_profile_revision_and_adjacent_exposures_only -- --exact --nocapture`

Expected:

- 当前实现失败，因为 `boundary.rs` 尚不存在，旧 executor 也没有边界目录和反解函数。

- [x] **Step 3: 做最小实现，建立 boundary 纯函数模块**

在 `engine/src/executor/boundary.rs` 实现最小骨架：

```rust
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ProfileRevision(pub String);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct BoundaryId {
    pub profile_revision: ProfileRevision,
    pub lower_exposure_bp: i64,
    pub upper_exposure_bp: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BoundaryBlueprint {
    pub id: BoundaryId,
    pub lower_exposure: Exposure,
    pub upper_exposure: Exposure,
    pub trigger_price: f64,
    pub step_size: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BoundaryDirection {
    Up,
    Down,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct BoundaryOperation {
    pub boundary_id: BoundaryId,
    pub direction: BoundaryDirection,
}

pub fn discretize_boundaries(
    config: &TrackConfig,
    profile_revision: ProfileRevision,
) -> Vec<BoundaryBlueprint> {}

pub fn profile_revision_for_config(config: &TrackConfig) -> ProfileRevision {}

pub fn trigger_price_for_boundary(
    boundary_upper: Exposure,
    config: &TrackConfig,
) -> f64 {}
```

要求：

- `discretize_boundaries()` 只依赖 `TrackConfig + ProfileRevision`
- 第一阶段把 `ProfileRevision` 明确定义成 `serde_json::to_string(config)` 生成的稳定字符串，不先引入 hash crate；后续若要压缩成摘要，再单独设计
- `BoundaryOperation` 只是查询视角，不落持久化 owner
- 这一 task 不引入 runtime 状态变更，也不改 planner；它只是建立后续迁移所需的纯域模型

- [x] **Step 4: 运行 Task 1 回归**

Run:

- `cargo test -p poise-engine executor::boundary::tests:: -- --nocapture`

Expected:

- 边界目录和反解测试通过
- 当前 executor 其他行为不变

- [x] **Step 5: Commit**

```bash
git add engine/src/executor/boundary.rs engine/src/executor/mod.rs
git commit -m "feat(engine): add boundary discretization domain"
```

Commit:

- `3f045e7`

## Task 2: 落 pure ledger / binding / policy 模块，不接 manager

**Files:**

- Create: `engine/src/executor/ledger.rs`
- Create: `engine/src/executor/binding.rs`
- Create: `engine/src/executor/policy.rs`
- Modify: `engine/src/executor/mod.rs`
- Test: `engine/src/executor/ledger.rs`
- Test: `engine/src/executor/binding.rs`
- Test: `engine/src/executor/policy.rs`

- [x] **Step 1: 先写失败测试，锁住纯模块边界**

新增测试至少覆盖：

```rust
#[test]
fn boundary_progress_derives_remaining_from_anchor_and_cumulative_deltas() {}

#[test]
fn due_direction_flips_when_spot_target_crosses_boundary() {}

#[test]
fn binding_proposal_key_is_policy_plus_ordered_operations() {}

#[test]
fn catch_up_policy_selects_due_uncovered_operations_only() {}
```

覆盖点：

- `BoundaryProgress` 的 `effective_crossed_qty / up_remaining / down_remaining`
- `due / future` 只由当前 `spot_target` 和 `remaining` 派生
- `BindingProposal` 的匹配键固定为 `(policy, ordered_operation_keys)`
- `CatchUpPolicy` 作为纯策略函数只选择 `due && remaining > 0 && uncovered` 的操作

- [x] **Step 2: 运行定向测试，确认当前实现失败**

Run:

- `cargo test -p poise-engine executor::ledger::tests::boundary_progress_derives_remaining_from_anchor_and_cumulative_deltas -- --exact --nocapture`
- `cargo test -p poise-engine executor::policy::tests::catch_up_policy_selects_due_uncovered_operations_only -- --exact --nocapture`
- `cargo test -p poise-engine executor::binding::tests::binding_proposal_key_is_policy_plus_ordered_operations -- --exact --nocapture`

Expected:

- 当前实现失败，因为 `ledger.rs`、`binding.rs`、`policy.rs` 尚不存在。

- [x] **Step 3: 做最小实现，建立纯模块**

要求：

- `ledger.rs` 定义：
  - `BoundaryProgress`
  - `BoundaryLedgerState`
  - `BoundaryLedgerView`
  - `effective_crossed_qty / remaining / due / expected_exposure`
- `binding.rs` 定义：
  - `LiveOrderBinding`
  - `BindingStatus`
  - `BindingProposal`
  - `proposal_key()`
- `policy.rs` 定义：
  - `PolicyKind`
  - `CoverageReservation`
  - `select_catch_up_operations(...)`
- 这一 task 不改 runtime / snapshot / manager；新模块先作为纯域层存在

- [x] **Step 4: 运行 Task 2 回归**

Run:

- `cargo test -p poise-engine executor::ledger::tests:: -- --nocapture`
- `cargo test -p poise-engine executor::binding::tests:: -- --nocapture`
- `cargo test -p poise-engine executor::policy::tests:: -- --nocapture`

Expected:

- 纯 boundary progress / binding / catch-up policy 模块通过
- 旧 executor 行为仍未切换

- [x] **Step 5: Commit**

```bash
git add engine/src/executor/ledger.rs engine/src/executor/binding.rs engine/src/executor/policy.rs engine/src/executor/mod.rs
git commit -m "feat(engine): add boundary ledger core modules"
```

Commit:

- `323488a`

## Task 3: 用 CatchUp-only 垂直切片替换 executor 内核

**Files:**

- Modify: `engine/src/runtime.rs`
- Modify: `engine/src/snapshot.rs`
- Modify: `engine/src/persisted_runtime.rs`
- Modify: `engine/src/manager.rs`
- Modify: `engine/src/executor/mod.rs`
- Modify: `engine/src/executor/planning.rs`
- Modify: `engine/src/executor/recording.rs`
- Modify: `engine/src/executor/recovery.rs`
- Delete: `engine/src/executor/round_policy.rs`
- Delete: `engine/src/executor/rebalance_trigger.rs`
- Delete: `engine/src/executor/slots.rs`
- Test: `engine/src/runtime.rs`
- Test: `engine/src/executor/mod.rs`
- Test: `engine/src/executor/recording.rs`
- Test: `engine/src/executor/recovery.rs`

- [x] **Step 1: 先写失败测试，锁住新状态边界和 CatchUp-only 中间态**

新增测试至少覆盖：

```rust
#[test]
fn snapshot_round_trips_boundary_ledger_state_and_bindings() {}

#[test]
fn catch_up_policy_submits_buy_for_due_up_operation_when_uncovered() {}

#[test]
fn planning_no_longer_depends_on_active_round_or_slots() {}

#[test]
fn recording_applies_fill_to_binding_then_updates_boundary_progress() {}
```

覆盖点：

- `ExecutorState` 共享边界已经变成 `BoundaryLedgerState + bindings`
- `planning.rs` 在没有 maker policy 的情况下，仍能对 `due` 操作生成 `ExecutionAction`
- `recording.rs` 已改成“先更新 binding，再回写 boundary progress”
- 旧 `round + slot` 语义已从 executor 内核移除

- [x] **Step 2: 运行定向测试，确认旧模型失败**

Run:

- `cargo test -p poise-engine runtime::tests::snapshot_round_trips_boundary_ledger_state_and_bindings -- --exact --nocapture`
- `cargo test -p poise-engine executor::planning::tests::catch_up_policy_submits_buy_for_due_up_operation_when_uncovered -- --exact --nocapture`
- `cargo test -p poise-engine executor::planning::tests::planning_no_longer_depends_on_active_round_or_slots -- --exact --nocapture`
- `cargo test -p poise-engine executor::recording::tests::recording_applies_fill_to_binding_then_updates_boundary_progress -- --exact --nocapture`
- `cargo test -p poise-engine executor::recovery::tests::recovery_does_not_fabricate_boundary_progress_from_live_order_alone -- --exact --nocapture`

Expected:

- 当前实现失败，因为 runtime 仍持有 `active_round + slots`，planner 也仍依赖旧模型。

- [x] **Step 3: 做最小实现，切到新内核**

要求：

- `runtime.rs` 的 `ExecutorState` 改成：
  - `BoundaryLedgerState`
  - `Vec<LiveOrderBinding>`
  - `recent_terminal_orders`
  - `recovery_anomaly`
- `snapshot.rs` / `persisted_runtime.rs` 同步持久化新结构
- `planning.rs` 先只保留 `CatchUpPolicy`
- `planning.rs` 继续产出 `ExecutionAction`，并继续使用 `engine/src/execution_plan.rs` 的 helper
- `recording.rs` 改成“binding -> boundary progress”
- `recovery.rs` 改成消费 `BoundaryLedgerState + bindings` 的最小版本
- 为保证 crate 在 Task 3 结束时可编译，`manager.rs` 中直接依赖旧 executor API 的调用点允许做最小接线调整；本 task 不要求补齐 manager 行为测试
- `round_policy.rs`、`rebalance_trigger.rs`、`slots.rs` 在本 task 内一起删除
- 如果现有 executor 测试 helper 过度依赖 slot/round，直接重写，不做兼容包装

- [x] **Step 4: 运行 Task 3 回归**

Run:

- `cargo test -p poise-engine runtime::tests:: -- --nocapture`
- `cargo test -p poise-engine executor::boundary::tests:: -- --nocapture`
- `cargo test -p poise-engine executor::ledger::tests:: -- --nocapture`
- `cargo test -p poise-engine executor::binding::tests:: -- --nocapture`
- `cargo test -p poise-engine executor::policy::tests:: -- --nocapture`
- `cargo test -p poise-engine executor::planning::tests:: -- --nocapture`
- `cargo test -p poise-engine executor::recording::tests:: -- --nocapture`
- `cargo test -p poise-engine executor::recovery::tests:: -- --nocapture`
- `cargo test -p poise-engine -- --list`

Expected:

- 新的 `ExecutorState` 已完全替换旧 `round + slot`
- `CatchUpPolicy` 垂直切片跑通
- snapshot / restore / recording / recovery 已使用同一套状态语义

- [x] **Step 5: Commit**

```bash
git add engine/src/runtime.rs engine/src/snapshot.rs engine/src/persisted_runtime.rs engine/src/manager.rs engine/src/executor/mod.rs engine/src/executor/planning.rs engine/src/executor/recording.rs engine/src/executor/recovery.rs
git rm engine/src/executor/round_policy.rs engine/src/executor/rebalance_trigger.rs engine/src/executor/slots.rs
git commit -m "refactor(engine): rebuild executor core on boundary ledger"
```

Commit:

- `887b8f4`

## Task 4: 接 manager reconcile 路径，但只补 focused manager 测试

**Files:**

- Modify: `engine/src/manager.rs`
- Modify: `engine/src/executor/planning.rs`
- Modify: `engine/src/runtime.rs`
- Modify: `engine/src/executor/mod.rs`
- Test: `engine/src/manager.rs`

- [ ] **Step 1: 先写失败测试，锁住 manager 对新 executor 的最小集成**

新增测试至少覆盖：

```rust
#[test]
fn reconcile_track_submits_catch_up_action_from_due_boundary_operation() {}

#[test]
fn reconcile_track_reopens_boundary_ledger_when_profile_revision_changes() {}

#[test]
fn reconcile_track_projects_no_round_or_slot_state_after_executor_refresh() {}
```

覆盖点：

- manager 能把曲线、物理仓位和 market/order 事实交给新 planner
- revision 变化时只会重开新账本
- manager 不再依赖 round/slot 字段

- [ ] **Step 2: 运行定向测试，确认 manager 仍绑定旧模型**

Run:

- `cargo test -p poise-engine manager::tests::reconcile_track_submits_catch_up_action_from_due_boundary_operation -- --exact --nocapture`
- `cargo test -p poise-engine manager::tests::reconcile_track_reopens_boundary_ledger_when_profile_revision_changes -- --exact --nocapture`

Expected:

- 当前实现失败，因为 manager 仍按旧 executor 调用路径组织输入和测试夹具。

- [ ] **Step 3: 做最小实现，接通 manager 到新 planner**

要求：

- `manager.rs` 只负责组织：
  - 当前曲线读数
  - 当前物理仓位
  - 当前 market/order 事实
  - 当前 `ExecutorState`
- `manager.rs` 不再解释 `round / slot`
- 本 task 只补 focused reconcile 测试，不要求把 4800 行 manager 测试全部迁回；与新 executor 无关的其余测试留到最终清理 task 再处理

- [ ] **Step 4: 运行 Task 4 回归**

Run:

- `cargo test -p poise-engine manager::tests::reconcile_track_ -- --nocapture`

Expected:

- manager 的核心 reconcile 路径已切到新 executor
- focused manager 测试通过

- [ ] **Step 5: Commit**

```bash
git add engine/src/manager.rs engine/src/executor/planning.rs engine/src/runtime.rs engine/src/executor/mod.rs
git commit -m "refactor(engine): wire manager to boundary ledger executor"
```

Commit:

- `待执行时回写 SHA`

## Task 5: 加入 CurveMakerPolicy 与串行 policy 仲裁

**Files:**

- Modify: `engine/src/executor/policy.rs`
- Modify: `engine/src/executor/planning.rs`
- Modify: `engine/src/executor/binding.rs`
- Modify: `engine/src/executor/recording.rs`
- Modify: `engine/src/executor/mod.rs`
- Test: `engine/src/executor/mod.rs`
- Test: `engine/src/executor/recording.rs`

- [ ] **Step 1: 先写失败测试，锁住 maker policy 与 CatchUp 抢占关系**

新增测试至少覆盖：

```rust
#[test]
fn curve_maker_policy_emits_future_operations_near_spot() {}

#[test]
fn catch_up_policy_preempts_curve_maker_after_due_grace_expires() {}

#[test]
fn curve_maker_policy_state_is_private_to_binding() {}

#[test]
fn binding_diff_replaces_maker_binding_with_catch_up_binding_on_preemption() {}
```

覆盖点：

- `CurveMakerPolicy` 只选择 `future` 操作，且按每侧最近 `N = 3` 工作
- `CatchUpPolicy` 只有在 `due_grace_started_at > curve_maker_grace_ms` 后才抢占
- `due_grace_started_at` 只存在于 `BindingPolicyState::CurveMaker`
- diff 看到更高优 proposal 时会取消 maker binding 并提交 catch-up binding

- [ ] **Step 2: 运行定向测试，确认当前中间态失败**

Run:

- `cargo test -p poise-engine executor::tests::curve_maker_policy_emits_future_operations_near_spot -- --exact --nocapture`
- `cargo test -p poise-engine executor::tests::catch_up_policy_preempts_curve_maker_after_due_grace_expires -- --exact --nocapture`
- `cargo test -p poise-engine executor::tests::binding_diff_replaces_maker_binding_with_catch_up_binding_on_preemption -- --exact --nocapture`

Expected:

- 当前实现失败，因为前面只有 `CatchUpPolicy`，也还没有 maker 私有状态和串行仲裁。

- [ ] **Step 3: 做最小实现，补齐 maker policy 与 arbitration**

要求：

- `CurveMakerPolicy` 只生成 future 操作的 limit binding
- `CatchUpPolicy` 只处理 `due && remaining > 0` 操作
- policy runner 必须按 `ManualOverride > Flatten > CatchUp > CurveMaker` 串行运行
- `CoverageReservation` 是唯一覆盖 owner
- `BindingProposal` 的匹配键固定为 `(policy, ordered_operation_keys)`
- `CurveMakerBindingState` 的创建、清空和抢占只在 owner policy 代码里维护

- [ ] **Step 4: 运行 Task 5 回归**

Run:

- `cargo test -p poise-engine executor::tests::curve_maker_policy_ -- --nocapture`
- `cargo test -p poise-engine executor::tests::catch_up_policy_preempts_ -- --nocapture`
- `cargo test -p poise-engine recording::tests::binding_diff_replaces_maker_binding_with_catch_up_binding_on_preemption -- --exact --nocapture`

Expected:

- `CurveMakerPolicy` 与 `CatchUpPolicy` 可并存
- maker 升级时机只留在 policy 私有状态
- 同一 `BoundaryOperation` 仍然最多只有一个 active binding

- [ ] **Step 5: Commit**

```bash
git add engine/src/executor/policy.rs engine/src/executor/planning.rs engine/src/executor/binding.rs engine/src/executor/recording.rs engine/src/executor/mod.rs
git commit -m "feat(engine): add curve maker arbitration on boundary ledger"
```

Commit:

- `待执行时回写 SHA`

## Task 6: 收紧 recovery、补 broader manager tests，并完成文档回写

**Files:**

- Modify: `engine/src/executor/recovery.rs`
- Modify: `engine/src/executor/recording.rs`
- Modify: `engine/src/executor/binding.rs`
- Modify: `engine/src/runtime.rs`
- Modify: `engine/src/manager.rs`
- Modify: `engine/src/executor/planning.rs`
- Modify: `engine/src/executor/mod.rs`
- Modify: `docs/superpowers/specs/2026-04-22-curve-boundary-ledger-execution-design.md`
- Modify: `docs/superpowers/plans/2026-04-22-boundary-ledger-executor.md`
- Test: `engine/src/executor/recovery.rs`
- Test: `engine/src/manager.rs`
- Test: `engine/src/runtime.rs`
- Test: `engine/src/executor/mod.rs`

- [ ] **Step 1: 先写失败测试，锁住恢复与最终派生诊断**

新增测试至少覆盖：

```rust
#[test]
fn recovery_matches_live_order_to_single_expected_binding_candidate() {}

#[test]
fn recovery_marks_unknown_live_order_when_no_binding_candidate_matches() {}

#[test]
fn recovery_marks_ambiguous_live_order_when_multiple_binding_candidates_match() {}

#[test]
fn recovery_does_not_fabricate_boundary_progress_from_live_order_alone() {}

#[test]
fn runtime_live_view_derives_gap_and_mode_from_boundary_ledger_without_persisting_them() {}
```

覆盖点：

- live order 认领优先 binding 快照
- 无快照时只能做受限结构匹配
- `UnknownLiveOrder` / `AmbiguousLiveOrder` 保留
- 恢复路径不会凭 live order 推导已完成进度
- `gap / mode / core_resting` 只在 planner 或 read path 派生

- [ ] **Step 2: 运行定向测试，确认尾部规则仍需收紧**

Run:

- `cargo test -p poise-engine recovery::tests::recovery_matches_live_order_to_single_expected_binding_candidate -- --exact --nocapture`
- `cargo test -p poise-engine recovery::tests::recovery_does_not_fabricate_boundary_progress_from_live_order_alone -- --exact --nocapture`
- `cargo test -p poise-engine runtime::tests::runtime_live_view_derives_gap_and_mode_from_boundary_ledger_without_persisting_them -- --exact --nocapture`

Expected:

- 若恢复匹配过宽、诊断仍被持久化，测试会失败。

- [ ] **Step 3: 做最小实现，完成恢复和收尾**

要求：

- `recover_working_orders(...)` 和 `recover_submit_effect(...)` 只消费 `BoundaryLedgerState + bindings`
- 无快照 backing 的 live order 认领只能使用：
  - `side`
  - `price ± tick tolerance`
  - `qty ± step tolerance`
  - 唯一候选 binding
- `recovery_anomaly` 只记录异常，不得顺手补写 boundary progress
- `runtime.rs` / `manager.rs` 中的 `gap`、`mode`、`core/resting` 全部改成派生
- 把与新 executor 紧密相关但在 Task 4 暂未补回的 manager 测试在本 task 完成
- 若实际落地的类型名、函数名、文件边界与 spec 有细小修正，必须在本 task 回写 spec
- 执行过程中同步勾选本 plan 的步骤并回写 commit SHA

- [ ] **Step 4: 运行最终 focused 回归**

Run:

- `cargo test -p poise-engine executor::boundary::tests:: -- --nocapture`
- `cargo test -p poise-engine executor::ledger::tests:: -- --nocapture`
- `cargo test -p poise-engine executor::binding::tests:: -- --nocapture`
- `cargo test -p poise-engine executor::policy::tests:: -- --nocapture`
- `cargo test -p poise-engine executor::tests:: -- --nocapture`
- `cargo test -p poise-engine recovery::tests:: -- --nocapture`
- `cargo test -p poise-engine recording::tests:: -- --nocapture`
- `cargo test -p poise-engine runtime::tests:: -- --nocapture`
- `cargo test -p poise-engine manager::tests:: -- --nocapture`

Expected:

- 新 executor 内核的 focused 覆盖全部通过
- 没有 round/slot 旧状态残留
- spec 和 plan 已与最终实现边界一致

- [ ] **Step 5: Commit**

```bash
git add engine/src/executor/recovery.rs engine/src/executor/recording.rs engine/src/executor/binding.rs engine/src/runtime.rs engine/src/manager.rs engine/src/executor/planning.rs engine/src/executor/mod.rs docs/superpowers/specs/2026-04-22-curve-boundary-ledger-execution-design.md docs/superpowers/plans/2026-04-22-boundary-ledger-executor.md
git commit -m "refactor(engine): finalize boundary ledger executor integration"
```

Commit:

- `待执行时回写 SHA`

## Coverage Check

- spec 第 1-5 节：Task 1 和 Task 2 覆盖曲线、boundary、progress、binding 新骨架。
- spec 第 6-7 节：Task 1 和 Task 2 覆盖 `Level / Boundary / BoundaryProgress / expected_exposure / due`。
- spec 第 8 节：Task 3 和 Task 6 覆盖 `LiveOrderBinding`、progress 回写、恢复认领。
- spec 第 9 节：Task 3 先落 `CatchUpPolicy`，Task 5 再补 `CurveMakerPolicy` 与串行仲裁。
- spec 第 10-11 节：Task 3、Task 5、Task 6 覆盖 reconcile 流程、binding diff 和 recovery。
- spec 第 12-15 节：Task 3 删除旧 round/slot 文件，Task 4 和 Task 6 完成 manager 接线、派生诊断和最终验收。

## Placeholder Scan

- 本计划没有 `TODO` / `TBD` / “之后再补” 类占位。
- 每个 task 都包含具体文件、测试入口、实现边界、回归命令和提交命令。

## Type Consistency Check

- 统一使用：
  - `BoundaryId`
  - `BoundaryBlueprint`
  - `BoundaryDirection`
  - `BoundaryOperation`
  - `BoundaryProgress`
  - `BoundaryLedgerState`
  - `LiveOrderBinding`
  - `BindingProposal`
- 不再使用：
  - `ExecutionRound`
  - `ExecutionSlot`
  - `RoundLifecycleDecision`
  - `RebalanceTrigger`
  - `Claim*`
