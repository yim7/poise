# Executor Round-Driven Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把当前 tick-driven 的库存执行器改成 round-driven 执行器，拆开 `desired_exposure` 与当前执行承诺，收紧 `target` 和 round 有效性 owner，并让 `mode` 真正参与单槽位执行决策。

**Architecture:** 第一阶段不引入 `lane`，继续保留单个 `inventory_core` slot。`reconciler` 只产出 `desired_exposure`，对外协议保留字段名 `desired_exposure`，但它只投影 `desired_exposure`，不表示 `ExecutionRound.desired_exposure`。`round_policy` 单点拥有 round 生命周期和 `mode` 变化，并通过共享 `RoundPolicyInput` 构造入口接收运行态摘要；`executor` 只负责应用 policy 结果并生成最终报价与 effect。`ExecutionRound.desired_exposure` 是执行层 target 的唯一 owner，`WorkingOrder.desired_exposure` 在同一 task 删除，避免双 owner。

**Implementation constraint:** `desired_exposure` 重命名必须与协议投影不变量一起落地；`round_policy` 与共享 `RoundPolicyInput` 构造必须先于 `active_round` 生效；`ExecutionRound.desired_exposure` owner 切换和 `WorkingOrder.desired_exposure` 删除必须在同一个 task 原子完成；整个过程不允许留下双 owner、双规则或双输入拼装入口的中间态。

**Tech Stack:** Rust workspace, Cargo tests, serde, chrono, Markdown

---

## Files And Responsibilities

- Create: `engine/src/executor/round_policy.rs`
  单点拥有 round 生命周期决策：`Start / Continue / Switch / Finish` 与 `mode` 变化，并定义 `RoundPolicyInput`、`RoundPolicySlotSummary` 和共享输入构造入口。
- Modify: `engine/src/runtime.rs`
  引入 `desired_exposure`、`ExecutionRound`、`ExecutorDiagnostics`，删除 `WorkingOrder.desired_exposure`。
- Modify: `engine/src/snapshot.rs`
  持久化 `desired_exposure`、`active_round` 和新的 `ExecutorState` 结构，并兼容旧快照别名。
- Modify: `engine/src/reconciler.rs`
  保持只产出系统当前希望到达的目标，不再把外层字段称为 `desired_exposure`。
- Modify: `engine/src/manager.rs`
  只串联 `desired_exposure -> round_policy -> executor planning`，不再隐含 round owner。
- Modify: `engine/src/executor/planning.rs`
  消费共享 `round_policy` 结果，并在当前 round 内做单槽位报价、替换和 stale 判断。
- Modify: `engine/src/executor/recovery.rs`
  recovery 只消费共享 `round_policy`，不再单独维护 round 有效性判断。
- Modify: `engine/src/executor/rebalance_trigger.rs`
  迁移或删除依赖 `WorkingOrder.desired_exposure` 的旧锚点逻辑，避免与 `ExecutionRound.desired_exposure` 重复。
- Modify: `engine/src/executor/recording.rs`
  让订单事实吸收只记录交易所事实与执行角色，不再写 target owner 副本。
- Modify: `engine/src/executor/slots.rs`
  slot rebuild 和 role 推导只依赖 `active_round` 与订单事实，不从 `working_order` 读 target。
- Modify: `engine/src/executor/mod.rs`
  增加 executor 级回归测试，锁住 round owner、policy owner 和 mode 行为。
- Modify: `server/src/read_model.rs`
  内部读取 `desired_exposure`，但第一阶段继续向外投影现有 `desired_exposure` 协议字段，且该字段只表达系统当前希望达到的目标。
- Modify: `server/src/projector.rs`
  保持现有协议和 UI 字段稳定，继续把 `desired_exposure` 投影为现有 `desired_exposure` 展示语义，不把它解释成当前 round 锚点。
- Modify: `server/src/query_service.rs`
  补查询回归，锁住外部协议未变但内部 owner 已切换。
- Modify: `server/src/runtime.rs`
  增加 runtime 集成测试，锁住 round-driven 行为。
- Modify: `docs/superpowers/specs/2026-04-03-executor-round-driven-design.md`
  若实现中 helper 命名、边界或迁移约束需要收紧，回写最终设计说明。
- Modify: `docs/superpowers/plans/2026-04-03-executor-round-driven.md`
  执行时勾选任务并记录 commit SHA。

### Task 1: 把外层 `desired_exposure` 收紧成 `desired_exposure`，保持对外协议不变

**Files:**
- Modify: `engine/src/runtime.rs`
- Modify: `engine/src/snapshot.rs`
- Modify: `engine/src/reconciler.rs`
- Modify: `engine/src/manager.rs`
- Modify: `server/src/read_model.rs`
- Modify: `server/src/projector.rs`
- Modify: `server/src/query_service.rs`
- Modify: `server/src/debug_query_service.rs`
- Modify: `server/src/effect_worker.rs`
- Modify: `server/src/runtime.rs`
- Modify: `server/src/write_service.rs`
- Modify: `storage/src/sqlite.rs`
- Test: `engine/src/runtime.rs`
- Test: `engine/src/manager.rs`
- Test: `server/src/query_service.rs`

- [x] **Step 1: 先写失败测试，锁住内部重命名和外部协议稳定**

在 `engine/src/runtime.rs`、`engine/src/manager.rs`、`server/src/query_service.rs` 至少增加这些测试：

```rust
#[test]
fn snapshot_deserializes_legacy_desired_exposure_into_desired_exposure() {}

#[test]
fn observe_market_updates_desired_exposure_without_changing_protocol_target_projection() {}

#[test]
fn query_service_projects_desired_exposure_as_desired_exposure_for_clients() {}
```

覆盖点：

- 旧快照里的 `desired_exposure` 能被新字段 `desired_exposure` 兼容读取
- `TrackRuntime` 内部字段完成重命名
- 外部协议里的 `desired_exposure` 暂不改名，但它只表达当前系统希望达到的目标，不得表示 `ExecutionRound.desired_exposure`

- [x] **Step 2: 运行定向测试，确认当前实现失败**

Run:

- `cargo test -p poise-engine runtime::tests::snapshot_deserializes_legacy_desired_exposure_into_desired_exposure -- --exact --nocapture`
- `cargo test -p poise-engine manager::tests::observe_market_updates_desired_exposure_without_changing_protocol_target_projection -- --exact --nocapture`
- `cargo test -p poise-server query_service::tests::query_service_projects_desired_exposure_as_desired_exposure_for_clients -- --exact --nocapture`

Expected:

- 当前实现会失败，因为内部仍普遍使用 `desired_exposure` 作为运行态字段名。

- [x] **Step 3: 做最小实现，完成 `desired_exposure` 切换并保留 API 稳定**

要求：

- `TrackRuntime`、`TrackRuntimeSnapshot`、`TrackReadModel` 内部 owner 改成 `desired_exposure`
- `TrackRuntimeSnapshot` 对旧字段增加 `serde(alias = "desired_exposure")`
- `reconciler`、`manager`、query 层统一改为新名字
- `server` 对外协议和现有 UI DTO 暂不改字段名，继续把 `desired_exposure` 投影为现有 `desired_exposure`
- 在 `read_model` / `projector` / `query_service` 明确固定这条不变量：协议层 `desired_exposure` 只投影 `desired_exposure`
- 快照字段改名会同步影响 `storage` 和依赖快照测试夹具的 `server` 模块，这些接线也在本 task 内一起完成
- 本 task 内不引入 `ExecutionRound`，只完成外层 owner 的重命名与别名兼容

- [x] **Step 4: 跑 Task 1 回归**

Run:

- `cargo test -p poise-engine runtime::tests:: -- --nocapture`
- `cargo test -p poise-engine manager::tests::observe_market_ -- --nocapture`
- `cargo test -p poise-server query_service::tests:: -- --nocapture`

Expected:

- 内部重命名通过
- 对外协议仍保持 `desired_exposure`
- 旧快照兼容测试通过

- [x] **Step 5: Commit**

```bash
git add engine/src/runtime.rs engine/src/snapshot.rs engine/src/reconciler.rs engine/src/manager.rs server/src/read_model.rs server/src/projector.rs server/src/query_service.rs
git commit -m "refactor(engine): rename runtime target to desired exposure"
```

Commit:

- `07e821b` `refactor(engine): rename runtime target to desired exposure`

### Task 2: 先引入共享 `round_policy` 和唯一 `RoundPolicyInput` 构造，收紧 round 生命周期 owner

**Files:**
- Create: `engine/src/executor/round_policy.rs`
- Modify: `engine/src/executor/mod.rs`
- Modify: `engine/src/executor/planning.rs`
- Modify: `engine/src/executor/recovery.rs`
- Modify: `engine/src/manager.rs`
- Test: `engine/src/executor/mod.rs`
- Test: `engine/src/manager.rs`

- [x] **Step 1: 先写失败测试，锁住 `round_policy` 与共享输入构造是唯一 round 规则入口**

新增测试至少覆盖：

```rust
#[test]
fn round_policy_starts_execution_when_gap_requires_action() {}

#[test]
fn round_policy_continues_execution_when_drift_stays_within_tolerance() {}

#[test]
fn planning_and_recovery_consume_the_same_round_decision() {}

#[test]
fn round_policy_input_from_state_is_shared_by_planning_and_recovery() {}
```

覆盖点：

- `Start / Continue / Switch / Finish` 都由共享 owner 给出
- `planning` 和 `recovery` 对同一输入必须拿到同一类决策
- `RoundPolicyInput` 必须通过唯一构造入口生成，不允许各自拼 slot 摘要

- [x] **Step 2: 运行定向测试，确认当前实现失败**

Run:

- `cargo test -p poise-engine executor::tests::round_policy_starts_execution_when_gap_requires_action -- --exact --nocapture`
- `cargo test -p poise-engine executor::tests::planning_and_recovery_consume_the_same_round_decision -- --exact --nocapture`
- `cargo test -p poise-engine executor::tests::round_policy_input_from_state_is_shared_by_planning_and_recovery -- --exact --nocapture`

Expected:

- 当前实现会失败，因为 `planning` 和 `recovery` 仍各自持有一部分 round 判断，也没有共享输入构造。

- [x] **Step 3: 做最小实现，先把 round 生命周期规则收进单点 owner**

先创建 `engine/src/executor/round_policy.rs`，定义至少这些对象：

- `RoundPolicySlotSummary`
- `RoundPolicyInput`
- `RoundDecision`
- `round_policy_input_from_state(...)`
- `evaluate_round_policy(...)`

要求：

- 第一阶段 `RoundPolicySlotSummary` 只包含：
  - `slot`
  - `phase`
  - `working_side`
  - `working_price`
  - `working_quantity`
- `RoundPolicyInput.active_round` 在 Task 2 必须显式建模为 `Option`
- 在 `active_round` 还未进入 runtime / snapshot 的提交里，`RoundPolicyInput.active_round` 只能为 `None`
- `planning.rs` 和 `recovery.rs` 只消费 `RoundDecision`
- `planning.rs` 和 `recovery.rs` 都只能通过 `round_policy_input_from_state(...)` 取输入
- `manager.rs` 只把运行态事实交给 executor，不新增外层 round 规则
- 若这一 task 仍需要沿用现有执行锚点，只允许在 `round_policy` 内部临时读取或适配，外部不得保留第二份 round 有效性判断
- Task 2 不得为了满足 `RoundPolicyInput` 签名，提前在 runtime 层引入未使用的 `active_round` 占位字段

- [x] **Step 4: 跑 Task 2 回归**

Run:

- `cargo test -p poise-engine executor::tests:: -- --nocapture`
- `cargo test -p poise-engine manager::tests::observe_market_ -- --nocapture`
- `cargo test -p poise-engine manager::tests::recover_submit_effect_ -- --nocapture`

Expected:

- `round_policy` 成为唯一 round 生命周期 owner
- `planning` 和 `recovery` 对相同输入不再出现分叉行为
- `RoundPolicyInput` 只剩一个构造入口

- [x] **Step 5: Commit**

```bash
git add engine/src/executor/round_policy.rs engine/src/executor/mod.rs engine/src/executor/planning.rs engine/src/executor/recovery.rs engine/src/manager.rs
git commit -m "feat(engine): centralize round lifecycle decisions"
```

Commit:

- `edfed7f` `feat(engine): centralize round lifecycle decisions`

### Task 3: 引入 `ExecutionRound` / `active_round`，并原子切换执行层 target owner

**Files:**
- Modify: `engine/src/runtime.rs`
- Modify: `engine/src/snapshot.rs`
- Modify: `engine/src/manager.rs`
- Modify: `engine/src/executor/round_policy.rs`
- Modify: `engine/src/executor/planning.rs`
- Modify: `engine/src/executor/recovery.rs`
- Modify: `engine/src/executor/recording.rs`
- Modify: `engine/src/executor/rebalance_trigger.rs`
- Modify: `engine/src/executor/slots.rs`
- Modify: `engine/src/executor/mod.rs`
- Modify: `server/src/effect_worker.rs`
- Test: `engine/src/runtime.rs`
- Test: `engine/src/executor/mod.rs`
- Test: `engine/src/manager.rs`
- Test: `engine/src/executor/recording.rs`
- Test: `server/src/effect_worker.rs`

- [x] **Step 1: 先写失败测试，锁住 `active_round` 生命周期和 target 单 owner**

新增测试至少覆盖：

```rust
#[test]
fn empty_executor_state_has_no_active_round_and_empty_inventory_core_slot() {}

#[test]
fn planning_starts_active_round_when_execution_first_begins() {}

#[test]
fn refresh_state_preserves_active_round_when_only_desired_exposure_changes() {}

#[test]
fn snapshot_round_trips_active_round_and_diagnostics() {}

#[test]
fn recording_submit_request_does_not_store_target_on_working_order() {}

#[test]
fn recovery_uses_active_round_target_when_receipt_and_live_order_are_replayed() {}

#[tokio::test]
async fn effect_worker_writeback_keeps_round_target_without_working_order_target_copy() {}
```

覆盖点：

- 空态下没有 `active_round`
- 开始执行时会显式创建 `active_round`
- 仅 `desired_exposure` 漂移时，不会自动重写当前 round target
- `ExecutionRound.desired_exposure` 成为执行层唯一 target owner
- `WorkingOrder.desired_exposure` 在同一个 task 内删除

- [x] **Step 2: 运行定向测试，确认当前实现失败**

Run:

- `cargo test -p poise-engine runtime::tests::empty_executor_state_has_no_active_round_and_empty_inventory_core_slot -- --exact --nocapture`
- `cargo test -p poise-engine executor::tests::planning_starts_active_round_when_execution_first_begins -- --exact --nocapture`
- `cargo test -p poise-engine executor::tests::recording_submit_request_does_not_store_target_on_working_order -- --exact --nocapture`
- `cargo test -p poise-engine executor::tests::recovery_uses_active_round_target_when_receipt_and_live_order_are_replayed -- --exact --nocapture`

Expected:

- 当前实现会失败，因为还没有 `active_round`，且多个 executor 模块仍从 `WorkingOrder.desired_exposure` 读锚点。

- [x] **Step 3: 做原子切换，引入 `ExecutionRound` 并删除 `WorkingOrder.desired_exposure`**

要求：

- 在 `engine/src/runtime.rs` 引入 `ExecutionRound` 和 `ExecutorDiagnostics`
- 现有 `mode / inventory_gap / gap_started_at / last_reprice_at / last_execution_reason / recovery_anomaly` 收入 `ExecutorDiagnostics`
- `ExecutorState` 新增 `active_round`
- 从 `WorkingOrder` 删除 `desired_exposure`
- `round_policy` 的输入在同一个 task 内把 `RoundPolicyInput.active_round` 从 `None` 过渡到真实 `active_round`
- `recording`、`recovery`、`rebalance_trigger`、`slots`、`planning` 在同一个 task 内全部改为读取 `active_round.desired_exposure`
- 若发现“有非空 slot 但没有 `active_round`”，按 spec 视为 recovery anomaly，不允许从 slot/order 反推 target
- 第一阶段继续保留单个 `inventory_core` slot，不做 lane

- [x] **Step 4: 跑 Task 3 回归**

Run:

- `cargo test -p poise-engine runtime::tests:: -- --nocapture`
- `cargo test -p poise-engine executor::tests:: -- --nocapture`
- `cargo test -p poise-engine executor::recording::tests:: -- --nocapture`
- `cargo test -p poise-server effect_worker::tests:: -- --nocapture`

Expected:

- `ExecutorState` 新结构通过序列化和核心行为回归
- 执行层 target owner 只剩 `active_round.desired_exposure`
- 不再存在 `WorkingOrder.desired_exposure` 决策副本

- [x] **Step 5: Commit**

```bash
git add engine/src/runtime.rs engine/src/snapshot.rs engine/src/manager.rs engine/src/executor/round_policy.rs engine/src/executor/planning.rs engine/src/executor/recovery.rs engine/src/executor/recording.rs engine/src/executor/rebalance_trigger.rs engine/src/executor/slots.rs engine/src/executor/mod.rs server/src/effect_worker.rs
git commit -m "feat(engine): introduce execution round state"
```

Commit:

- `024ee3e` `feat(engine): introduce execution round state`

### Task 4: 让 `mode` 真正参与单槽位报价、替换和 stale 判断

**Files:**
- Modify: `engine/src/executor/planning.rs`
- Modify: `engine/src/executor/recovery.rs`
- Modify: `engine/src/executor/mod.rs`
- Modify: `engine/src/manager.rs`
- Modify: `server/src/runtime.rs`
- Test: `engine/src/executor/mod.rs`
- Test: `engine/src/manager.rs`
- Test: `server/src/runtime.rs`

- [x] **Step 1: 先写失败测试，锁住 `Passive / Rebalance / CatchUp` 的行为差异**

新增测试至少覆盖：

```rust
#[test]
fn passive_mode_keeps_current_working_order_under_small_price_drift() {}

#[test]
fn rebalance_mode_replaces_stale_working_order_sooner_than_passive() {}

#[test]
fn catch_up_mode_uses_most_aggressive_limit_replacement_policy() {}

#[tokio::test]
async fn runtime_small_drift_does_not_loop_replacing_orders_once_round_is_active() {}
```

覆盖点：

- `mode` 不再只是诊断标签
- `Passive / Rebalance / CatchUp` 至少在报价容忍度、替换积极度、stale 条件上有真实差异
- runtime 集成场景中，不再因为小漂移和链路延迟持续调整

- [x] **Step 2: 运行定向测试，确认当前实现失败**

Run:

- `cargo test -p poise-engine executor::tests::passive_mode_keeps_current_working_order_under_small_price_drift -- --exact --nocapture`
- `cargo test -p poise-engine executor::tests::rebalance_mode_replaces_stale_working_order_sooner_than_passive -- --exact --nocapture`
- `cargo test -p poise-server runtime::tests::runtime_small_drift_does_not_loop_replacing_orders_once_round_is_active -- --exact --nocapture`

Expected:

- 当前实现会失败，因为 `mode` 目前不会实质影响报价和替换逻辑。

- [x] **Step 3: 做最小实现，让 `mode` 进入 round 内执行策略**

要求：

- `planning.rs` 根据 `ExecutionMode` 决定单槽位报价容忍度、替换积极度、stale 判断
- `recovery.rs` 只在需要消费当前 round mode 的地方读取 `RoundDecision` 结果，不新增第二份 mode policy
- `manager.rs` 和 `server/src/runtime.rs` 只补集成测试与最小接线，不新增旁路 mode 知识
- 第一阶段继续只做单槽位，不引入 `lane`

- [x] **Step 4: 跑 Task 4 回归**

Run:

- `cargo test -p poise-engine executor::tests:: -- --nocapture`
- `cargo test -p poise-engine manager::tests:: -- --nocapture`
- `cargo test -p poise-server runtime::tests:: -- --nocapture`

Expected:

- `mode` 对单槽位执行有真实影响
- runtime 层不再出现旧的 tick-driven 调整风暴

- [x] **Step 5: Commit**

```bash
git add engine/src/executor/planning.rs engine/src/executor/recovery.rs engine/src/executor/mod.rs engine/src/manager.rs server/src/runtime.rs
git commit -m "feat(engine): make execution mode drive round quote behavior"
```

Commit:

- `fbd2672` `feat(engine): make execution mode drive round quote behavior`

### Task 5: 全量回归、文档回写和计划同步

**Files:**
- Modify: `docs/superpowers/specs/2026-04-03-executor-round-driven-design.md`
- Modify: `docs/superpowers/plans/2026-04-03-executor-round-driven.md`
- Modify: `server/src/write_service.rs`
- Modify: `storage/src/sqlite.rs`

- [x] **Step 1: 跑最终回归**

Run:

- `cargo test -p poise-engine`
- `cargo test -p poise-server`
- `cargo test -p poise-storage`
- `cargo test -p poise-tui`
- `cargo test --workspace --no-run`

Expected:

- engine / server / storage / tui 相关 crate 保持通过
- workspace 构建通过

- [x] **Step 2: 回写文档**

要求：

- 若 `round_policy`、`ExecutionRound`、`ExecutorDiagnostics` 的最终命名或边界有细化，回写到 spec
- 若快照兼容或对外投影策略有实现约束，也回写到 spec
- 在本 plan 中为已完成 task 勾选并记录 commit SHA

- [x] **Step 3: Commit**

```bash
git add docs/superpowers/specs/2026-04-03-executor-round-driven-design.md docs/superpowers/plans/2026-04-03-executor-round-driven.md
git commit -m "docs: finalize executor round-driven implementation plan notes"
```

Regression fix commit:

- `b1e1587` `test(server): align regression fixtures with round-driven state`

Commit:

- `87eb4d7` `docs: finalize executor round-driven implementation plan notes`
