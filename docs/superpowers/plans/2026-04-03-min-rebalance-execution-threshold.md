# Min Rebalance Execution Threshold Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把 `min_rebalance_units` 从“停手阈值”改成“触发下一次执行动作的最小目标变化”，避免执行中频繁 supersede / cancel-replace，同时保持系统分轮逼近最新策略目标。

**Architecture:** `reconciler` 继续只产出最新原始策略目标；executor 内新增单一的 `rebalance trigger` 抽象，统一拥有执行锚点选择和门槛比较语义；`planning` 与 `submit recovery` 只消费这个结果，不再各自实现一套规则。`manager` 继续保存最新 `target_exposure`，不把执行防抖上推到更外层。

**Implementation constraint:** shared trigger 抽象和它的两个 engine 消费者必须在同一个 task / commit 内一起切换，不允许出现“planning 已切到新语义、submit recovery 仍停留旧语义”的中间态。

**Tech Stack:** Rust workspace, Cargo tests, Markdown

---

## Files And Responsibilities

- Create: `engine/src/executor/rebalance_trigger.rs`
  单点拥有执行锚点选择、`trigger_delta` 计算和 `min_rebalance_units` 比较。
- Modify: `engine/src/executor/planning.rs`
  消费 shared trigger 决策，并映射成 `NoOp / Submit / CancelReplace`。
- Modify: `engine/src/executor/recovery.rs`
  让 submit recovery 消费同一份 trigger 决策，避免 pending submit 在小幅 target 漂移下被连续 supersede。
- Modify: `engine/src/executor/mod.rs`
  增加 executor 级回归测试，锁住 `Empty / Working / SubmitPending` 三种状态下的新语义。
- Modify: `engine/src/manager.rs`
  补 manager 级行为测试，确认 `target_exposure` 仍保持最新原始策略目标，且生命周期结束后还能开启下一轮。
- Modify: `server/src/runtime.rs`
  增加 runtime 级回归测试，锁住“重复 tick 不再产生 supersede storm”。
- Modify: `README.md`
  必须同步更新用户可见配置说明，明确 `min_rebalance_units` 的新语义和参考点。
- Modify: `docs/superpowers/specs/2026-04-02-strategy-min-rebalance-units-design.md`
  如实现中 helper 命名或边界表述有细化，回写最终设计说明。

### Task 1: 在 engine 内统一 trigger owner，并同时切换 planning / recovery

**Files:**
- Create: `engine/src/executor/rebalance_trigger.rs`
- Modify: `engine/src/executor/planning.rs`
- Modify: `engine/src/executor/recovery.rs`
- Modify: `engine/src/executor/mod.rs`
- Modify: `engine/src/manager.rs`
- Test: `engine/src/executor/mod.rs`
- Test: `engine/src/manager.rs`

- [x] **Step 1: 先写失败测试，覆盖 planning 和 submit recovery 的统一 trigger 语义**

在 `engine/src/executor/mod.rs` 和 `engine/src/manager.rs` 增加至少这几条测试：

```rust
#[test]
fn empty_slot_below_min_rebalance_units_does_not_start_new_round() {}

#[test]
fn working_order_target_drift_within_min_rebalance_units_is_kept() {}

#[test]
fn submit_pending_target_drift_within_min_rebalance_units_is_kept() {}

#[test]
fn working_order_target_drift_crossing_min_rebalance_units_replans() {}

#[test]
fn submit_recovery_keeps_pending_submit_when_latest_target_drift_is_within_min_rebalance_units_of_anchor() {}

#[test]
fn submit_recovery_supersedes_pending_submit_when_target_drift_crosses_min_rebalance_units() {}

#[test]
fn observe_market_keeps_latest_strategy_target_while_preserving_active_execution_anchor() {}
```

覆盖点：

- `Empty` 时仍以 `current_exposure` 为锚点
- `Working` / `SubmitPending` 时改用当前生命周期的 `target_exposure` 为锚点
- 小于门槛不触发下一次执行动作
- 大于等于门槛才重新进入 planning
- recovery 也必须使用同一份 trigger 决策，而不是继续比较“旧请求是否匹配最新计划请求”
- 锚点选择和门槛比较最终必须由单一 trigger 抽象拥有，而不是 planning / recovery 各自再写一套 helper

- [x] **Step 2: 运行定向测试，确认当前 planning 和 recovery 都会失败**

Run:

- `cargo test -p poise-engine executor::tests::empty_slot_below_min_rebalance_units_does_not_start_new_round -- --exact --nocapture`
- `cargo test -p poise-engine executor::tests::working_order_target_drift_within_min_rebalance_units_is_kept -- --exact --nocapture`
- `cargo test -p poise-engine executor::tests::submit_pending_target_drift_within_min_rebalance_units_is_kept -- --exact --nocapture`
- `cargo test -p poise-engine executor::tests::submit_recovery_keeps_pending_submit_when_latest_target_drift_is_within_min_rebalance_units_of_anchor -- --exact --nocapture`
- `cargo test -p poise-engine executor::tests::submit_recovery_supersedes_pending_submit_when_target_drift_crosses_min_rebalance_units -- --exact --nocapture`
- `cargo test -p poise-engine manager::tests::observe_market_keeps_latest_strategy_target_while_preserving_active_execution_anchor -- --exact --nocapture`

Expected:

- 当前实现会失败，因为：
  - planning 仍直接使用 `current_exposure -> latest_target` 判断，并把 active lifecycle 的小幅漂移当成需要重规划
  - recovery 仍主要按“旧请求是否匹配最新计划请求”来决定 supersede

- [x] **Step 3: 做最小实现，把共享 trigger 模块和两个 consumer 一起切换**

先创建 `engine/src/executor/rebalance_trigger.rs`，定义单一入口，例如：

- `RebalanceTriggerInput`
- `RebalanceTriggerDecision`
- `evaluate_rebalance_trigger(...)`

要求：

- 由该模块单点拥有：
  - `Empty`：锚点 = `current_exposure`
  - `SubmitPending / Working`：锚点 = slot 中记录的 `working_order.target_exposure`
  - `trigger_delta` 计算
  - `min_rebalance_units` 比较与内部容差
- `planning.rs` 只消费 `RebalanceTriggerDecision`
- `recovery.rs` 也只消费 `RebalanceTriggerDecision`
- 不允许在 `planning.rs` 或 `recovery.rs` 里再落一套局部锚点 / 比较 helper
- 本 step 允许编辑过程中短暂不通过，但提交前必须同时完成两个 consumer 的切换；不允许留下半切换中间态

然后在 `engine/src/executor/planning.rs`：

- 当 active lifecycle 且 trigger 决策为“不触发下一次执行动作”时：
  - 返回 `NoOp`
  - 保留当前 slot
  - 不走 `desired_order = None -> CancelOrder` 旧路径
- 当 trigger 决策为“允许触发”时，才继续针对最新目标走现有 planning / floor / replacement 逻辑

同时在 `engine/src/executor/recovery.rs`：

- 直接消费 shared trigger 决策
- 当 `current_plan` 相对当前 pending submit 的锚点漂移仍在门槛内：
  - 返回 `AwaitExchangeState`
  - 不 supersede 当前 pending submit
- 当漂移跨过门槛：
  - 才按最新目标继续进入 supersede / replacement submit 逻辑

在 `engine/src/manager.rs`：

- 保持 `track.target_exposure` 写入最新原始策略目标
- 不新增 manager 侧旁路逻辑

- [x] **Step 4: 跑 engine 回归**

Run:

- `cargo test -p poise-engine executor::tests:: -- --nocapture`
- `cargo test -p poise-engine manager::tests::observe_market_ -- --nocapture`
- `cargo test -p poise-engine manager::tests::recover_submit_effect_ -- --nocapture`

Expected:

- 新增测试通过
- planning / recovery 都已改为消费 shared trigger 抽象
- `target_exposure` 和 `ExposureTargetChanged` 语义保持不变
- 现有交易所 floor、replacement gate、submit pending 相关测试保持为绿

- [x] **Step 5: Commit**

```bash
git add engine/src/executor/rebalance_trigger.rs engine/src/executor/planning.rs engine/src/executor/recovery.rs engine/src/executor/mod.rs engine/src/manager.rs
git commit -m "feat(engine): anchor execution drift decisions to shared trigger semantics"
```

Commit: `3071a21`

### Task 2: 补 server/runtime 回归，锁住真实场景中的 supersede storm 不再出现

**Files:**
- Modify: `server/src/runtime.rs`
- Test: `server/src/runtime.rs`

- [ ] **Step 1: 先写 runtime 级失败测试，复现连续 supersede 场景**

新增测试，至少覆盖：

```rust
#[tokio::test]
async fn repeated_ticks_do_not_supersede_submit_effect_when_target_drift_stays_within_min_rebalance_units() {}

#[tokio::test]
async fn active_working_order_is_not_cancel_replaced_for_small_target_drift() {}
```

要求：

- 用接近用户现场的重复 tick / 部分成交场景
- 断言 recent effects 中不会出现一串连续 `Superseded`
- 断言当前 lifecycle 仍保留

- [ ] **Step 2: 运行定向测试，确认当前行为仍会失败**

Run:

- `cargo test -p poise-server runtime::tests::repeated_ticks_do_not_supersede_submit_effect_when_target_drift_stays_within_min_rebalance_units -- --exact --nocapture`
- `cargo test -p poise-server runtime::tests::active_working_order_is_not_cancel_replaced_for_small_target_drift -- --exact --nocapture`

Expected:

- 当前实现会失败，能复现 runtime 层的 supersede / cancel-replace 风暴。

- [ ] **Step 3: 如有必要做最小集成修正**

原则：

- 优先依赖 engine 层语义收敛
- 只有在 server 层仍有额外 lifecycle 触发点时，才做最小修正
- 不把执行锚点知识再复制到 server 层

- [ ] **Step 4: 跑 server 定向回归**

Run:

- `cargo test -p poise-server runtime::tests::repeated_ticks_do_not_supersede_submit_effect_when_target_drift_stays_within_min_rebalance_units -- --exact --nocapture`
- `cargo test -p poise-server runtime::tests::active_working_order_is_not_cancel_replaced_for_small_target_drift -- --exact --nocapture`
- `cargo test -p poise-server runtime::tests::recovery_task_ -- --nocapture`

Expected:

- runtime 层不再出现 supersede storm
- 现有 recovery anomaly 回归保持为绿

- [ ] **Step 5: Commit**

```bash
git add server/src/runtime.rs
git commit -m "test(server): cover min rebalance execution drift behavior"
```

### Task 3: 文档同步、迁移说明与全量回归

**Files:**
- Modify: `docs/superpowers/specs/2026-04-02-strategy-min-rebalance-units-design.md`
- Modify: `README.md`
- Modify: `docs/superpowers/plans/2026-04-03-min-rebalance-execution-threshold.md`

- [ ] **Step 1: 跑最终回归**

Run:

- `cargo test -p poise-engine executor::tests:: -- --nocapture`
- `cargo test -p poise-engine manager::tests:: -- --nocapture`
- `cargo test -p poise-server runtime::tests:: -- --nocapture`
- `cargo test --workspace --no-run`

Expected:

- engine / server 相关回归全部通过
- workspace 构建保持通过

- [ ] **Step 2: 同步文档**

要求：

- 如果实现中的 helper 命名、锚点来源或 runtime 场景表述有细化，回写到 spec
- 必须在 spec 和 README 中写清楚迁移说明：
  - 旧语义：`min_rebalance_units` 更像 `current_exposure -> latest_target` 的停手阈值
  - 新语义：`min_rebalance_units` 表示触发下一次执行动作的最小目标变化
  - 已有 `0.5` 配置在 active lifecycle 下会比第一版更少触发 supersede / cancel-replace
  - 如果需要更频繁跟随最新目标，应调低该值；如果需要更稳，应调高该值
- 必须更新 README 的配置说明，明确：
  - 无活动单时参考点是 `current_exposure`
  - 有活动生命周期时参考点是当前执行目标
  - `min_rebalance_units` 表示“是否触发下一次执行动作”，不再只是停手阈值
- 在本计划中回写 commit SHA，并勾选完成步骤

- [ ] **Step 3: Commit**

```bash
git add docs/superpowers/specs/2026-04-02-strategy-min-rebalance-units-design.md README.md docs/superpowers/plans/2026-04-03-min-rebalance-execution-threshold.md
git commit -m "docs: align min rebalance execution threshold plan and spec"
```
