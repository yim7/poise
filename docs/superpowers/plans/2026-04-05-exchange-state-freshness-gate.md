# Exchange State Freshness Gate Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 引入显式的交易所状态 freshness abstraction，让 `exchange_freshness` 成为唯一语义 owner，由 `runtime` 和 `effect_worker` 作为事实生产者写入 `Fresh / Stale`，并在真实 submit / cancel 前统一决定是否必须先同步交易所状态，同时避免 sync 成功后误清除较新的 stale 事实。

**Architecture:** 第一阶段不从 `executor_state` 或 slot 状态反推 stale，而是在 `server/src/exchange_freshness.rs` 中显式维护每个 `track` 的 freshness state。`runtime` 负责写入它先观察到的时序类 stale 事实，例如 `Filled`、`UnabsorbedOrderUpdate`；`effect_worker` 负责写入它先观察到的执行结果不确定事实，例如 `SubmitOutcomeUnknown`、`CancelOutcomeUnknown`。两者都不拥有 freshness 语义，只调用同一个 owner。成功 `sync_exchange_state_from_exchange(...)` 仍是第一版唯一的清脏路径，但清脏必须通过 sync 前捕获的 token 条件完成，不能再无条件 `clear(track_id)`。

**Tech Stack:** Rust workspace, Tokio, Cargo tests, Markdown

---

## Files And Responsibilities

- Create: `server/src/exchange_freshness.rs`
  单点拥有 per-track `Fresh / Stale` 状态、置脏原因和“某个 effect 是否必须先 sync”的规则。
- Modify: `server/src/main.rs`
  接线 `exchange_freshness` 模块。
- Modify: `server/src/assembly.rs`
  把 `exchange_freshness` 放进 `ServerState`，使 `runtime` 和 `effect_worker` 共用同一份状态。
- Modify: `server/src/runtime.rs`
  在用户事件和交易所同步路径中写入它先观察到的 freshness 事实：`Filled` 置脏、`UnabsorbedOrderUpdate` 置脏并立即 sync、成功 sync 按 token 条件清脏。
- Modify: `server/src/effect_worker.rs`
  在真实 submit / cancel 前消费 freshness state；若必须先 sync，则发起 reconcile 并结束当前 effect 的本轮执行；在 `OutcomeUnknown` 时写入 stale。
- Modify: `server/src/order_outcome.rs`
  引入 freshness gate 专用 `ReconcileReason::SyncBeforeSideEffect`，删除 `FilledOrderUpdate` 这种普通时序事件专用 reason，并保持它与 `ExchangeFreshnessReason` 分层命名。
- Modify: `docs/superpowers/specs/2026-04-05-exchange-state-freshness-gate-design.md`
  若实现中 owner、命名或清脏语义需要微调，回写最终边界。
- Modify: `docs/superpowers/plans/2026-04-05-exchange-state-freshness-gate.md`
  执行时勾选任务并记录 commit SHA。

### Task 1: 引入显式 `exchange_freshness` owner，并挂到 `ServerState`

**Files:**
- Create: `server/src/exchange_freshness.rs`
- Modify: `server/src/main.rs`
- Modify: `server/src/assembly.rs`
- Test: `server/src/exchange_freshness.rs`

- [x] **Step 1: 先写失败测试，锁住 freshness abstraction 不从 slot 反推 stale**

在 `server/src/exchange_freshness.rs` 增加最小测试集合：

```rust
#[tokio::test]
async fn freshness_is_fresh_by_default() {}

#[tokio::test]
async fn mark_stale_sets_track_state_until_cleared() {}

#[tokio::test]
async fn stale_track_blocks_submit_and_cancel_effects() {}

#[tokio::test]
async fn fresh_track_allows_submit_and_cancel_effects() {}

#[tokio::test]
async fn clear_if_current_does_not_erase_newer_stale_fact() {}
```

覆盖点：

- freshness 默认是 `Fresh`
- 置脏与清脏是显式状态变化
- 清脏必须受 freshness token 保护，不能误清除较新的 stale
- 是否拦截 effect 由 freshness state 决定，不由 `ExecutorState` / `SlotState` 决定

- [x] **Step 2: 运行定向测试，确认当前实现失败**

Run:

- `cargo test -p poise-server exchange_freshness::tests::freshness_is_fresh_by_default`
- `cargo test -p poise-server exchange_freshness::tests::stale_track_blocks_submit_and_cancel_effects`

Expected:

- 当前实现失败，因为还没有 `exchange_freshness` 模块和对应的 `ServerState` 挂载点。

- [x] **Step 3: 做最小实现，只引入显式 state owner**

在 `server/src/exchange_freshness.rs` 定义最小对象：

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExchangeFreshnessReason {
    FilledAwaitingSync,
    UnabsorbedOrderUpdate,
    SubmitOutcomeUnknown,
    CancelOutcomeUnknown,
}

#[derive(Default)]
pub struct ExchangeFreshness { /* per-track in-memory state */ }

pub struct ExchangeFreshnessSyncToken { /* opaque generation token */ }

impl ExchangeFreshness {
    pub async fn mark_stale(&self, track_id: &str, reason: ExchangeFreshnessReason) {}
    pub async fn prepare_sync(&self, track_id: &str) -> ExchangeFreshnessSyncToken {}
    pub async fn clear_if_current(&self, token: ExchangeFreshnessSyncToken) {}
    pub async fn is_stale(&self, track_id: &str) -> bool {}
    pub async fn requires_sync_before_effect(&self, track_id: &str, effect: &TrackEffect) -> bool {}
}
```

要求：

- 第一版只做进程内共享状态，不持久化
- `ExchangeFreshnessSyncToken` 隐藏 revision 细节，外层不直接操作 generation number
- `requires_sync_before_effect(...)` 只对 `SubmitOrder` / `CancelOrder` 返回 `true`
- 模块不读取 `ExecutorState`
- `ServerState` 新增 `exchange_freshness: Arc<ExchangeFreshness>`

- [x] **Step 4: 跑 Task 1 回归**

Run:

- `cargo test -p poise-server exchange_freshness::tests::`
- `cargo test -p poise-server assembly::tests::`

Expected:

- freshness owner 已明确落在单独模块
- `ServerState` 已接入共享 store
- 清脏语义已经封装进 token，而不是暴露 revision 细节给调用方
- 仍没有把 stale 规则泄露到 `runtime` / `effect_worker` 之外

- [ ] **Step 5: Commit**

```bash
git add server/src/exchange_freshness.rs server/src/main.rs server/src/assembly.rs docs/superpowers/plans/2026-04-05-exchange-state-freshness-gate.md
git commit -m "feat(server): add exchange freshness state owner"
```

Commit:

- `<填写 commit SHA>`

### Task 2: 让 `runtime` 写入事件侧 freshness 事实，并删掉 `Filled` 专用 sync

**Files:**
- Modify: `server/src/runtime.rs`
- Modify: `server/src/order_outcome.rs`
- Modify: `docs/superpowers/specs/2026-04-05-exchange-state-freshness-gate-design.md`
- Test: `server/src/runtime.rs`

- [ ] **Step 1: 先写失败测试，锁住 runtime 只处理事件侧 freshness 事实**

在 `server/src/runtime.rs` 增加至少这些测试：

```rust
#[tokio::test]
async fn filled_order_update_marks_track_stale_without_immediate_reconcile() {}

#[tokio::test]
async fn unabsorbed_order_update_marks_stale_and_triggers_immediate_reconcile() {}

#[tokio::test]
async fn successful_exchange_sync_clears_stale_state() {}

#[tokio::test]
async fn successful_exchange_sync_does_not_clear_newer_stale_fact() {}
```

覆盖点：

- `Filled` 只置脏，不再专用 sync
- `UnabsorbedOrderUpdate` 仍是异常恢复路径：先置脏，再立即 sync
- 成功 `sync_exchange_state_from_exchange(...)` 只会按 token 条件清除当前代 stale

- [ ] **Step 2: 运行定向测试，确认当前实现失败**

Run:

- `cargo test -p poise-server runtime::tests::filled_order_update_marks_track_stale_without_immediate_reconcile`
- `cargo test -p poise-server runtime::tests::successful_exchange_sync_clears_stale_state`
- `cargo test -p poise-server runtime::tests::successful_exchange_sync_does_not_clear_newer_stale_fact`

Expected:

- 当前实现失败，因为 freshness 事实还没有由 runtime 显式维护，且 `Filled` 仍可能沿旧路径直接 sync。
- 当前实现也还没有 token 化清脏语义，晚到 stale 可能被无条件清除。

- [ ] **Step 3: 做最小实现，把 freshness 写入 runtime 和 sync 边界**

要求：

- `apply_user_data_event(...)` 中：
  - `Filled` -> `exchange_freshness.mark_stale(track_id, FilledAwaitingSync)`
  - `UnabsorbedOrderUpdate` -> `mark_stale(..., UnabsorbedOrderUpdate)` 后立即 `enqueue_reconcile_request(...)`
- `sync_exchange_state_from_exchange(...)` 开始前调用 `let sync_token = exchange_freshness.prepare_sync(track_id)`
- `sync_exchange_state_from_exchange(...)` 成功完成 writeback 后调用 `exchange_freshness.clear_if_current(sync_token)`
- 删除 `ReconcileReason::FilledOrderUpdate`
- 增加 `ReconcileReason::SyncBeforeSideEffect`
- 若实现中命名微调，回写 spec

- [ ] **Step 4: 跑 Task 2 回归**

Run:

- `cargo test -p poise-server runtime::tests::filled_order_update_marks_track_stale_without_immediate_reconcile`
- `cargo test -p poise-server runtime::tests::unabsorbed_order_update_marks_stale_and_triggers_immediate_reconcile`
- `cargo test -p poise-server runtime::tests::successful_exchange_sync_clears_stale_state`
- `cargo test -p poise-server runtime::tests::successful_exchange_sync_does_not_clear_newer_stale_fact`
- `cargo test -p poise-server runtime::tests::unabsorbed_order_update_triggers_immediate_reconcile`

Expected:

- runtime 只负责事件侧 freshness 事实和事件异常恢复
- `Filled` 专用 sync 被删除
- 清脏语义集中在成功交易所同步，并由 token 保护

- [ ] **Step 5: Commit**

```bash
git add server/src/runtime.rs server/src/order_outcome.rs docs/superpowers/specs/2026-04-05-exchange-state-freshness-gate-design.md
git commit -m "refactor(server): route freshness facts through runtime"
```

Commit:

- `<填写 commit SHA>`

### Task 3: 让 `effect_worker` 消费 freshness state，并写入执行结果不确定的 freshness 事实

**Files:**
- Modify: `server/src/effect_worker.rs`
- Test: `server/src/effect_worker.rs`

- [ ] **Step 1: 先写失败测试，锁住 worker 不再从执行状态猜 stale，并负责 outcome-unknown 置脏**

在 `server/src/effect_worker.rs` 增加至少这些测试：

```rust
#[tokio::test]
async fn stale_submit_effect_syncs_exchange_before_submitting() {}

#[tokio::test]
async fn stale_cancel_effect_syncs_exchange_before_canceling() {}

#[tokio::test]
async fn fresh_effects_do_not_trigger_extra_sync() {}

#[tokio::test]
async fn outcome_unknown_marks_track_stale_before_reconcile() {}
```

覆盖点：

- stale submit 不直接 `submit_order(...)`
- stale cancel 不直接 `cancel_order(...)`
- fresh submit / cancel 沿原路径继续执行
- `SubmitOutcomeUnknown` / `CancelOutcomeUnknown` 由 worker 先写入 stale，再发起 reconcile
- worker 不读 slot 状态来决定 stale

- [ ] **Step 2: 运行定向测试，确认当前实现失败**

Run:

- `cargo test -p poise-server effect_worker::tests::stale_submit_effect_syncs_exchange_before_submitting`
- `cargo test -p poise-server effect_worker::tests::stale_cancel_effect_syncs_exchange_before_canceling`
- `cargo test -p poise-server effect_worker::tests::fresh_effects_do_not_trigger_extra_sync`
- `cargo test -p poise-server effect_worker::tests::outcome_unknown_marks_track_stale_before_reconcile`

Expected:

- 当前 stale submit / cancel 测试失败，因为 worker 还没有消费 freshness state。
- outcome unknown 测试失败，因为 worker 还没有在 reconcile 前写入 stale。

- [ ] **Step 3: 做最小实现，把 freshness gate 挂到真实 side effect 前**

要求：

- 在 `execute_submit(...)` 最开头调用：

```rust
if self
    .state
    .exchange_freshness
    .requires_sync_before_effect(persisted.track_id.as_str(), &persisted.effect)
    .await
{
    runtime::enqueue_reconcile_request(
        &self.state,
        &self.exchange,
        ReconcileRequest {
            track_id: persisted.track_id.as_str().to_string(),
            reason: ReconcileReason::SyncBeforeSideEffect,
        },
        &request.instrument,
    ).await?;
    return Ok(());
}
```

- `execute_cancellation(...)` 也复用同一判断
- 不新增从 `ExecutorState` 推导 stale 的逻辑
- 在 `classify_submit_receipt_writeback_error(...)` 和 `classify_cancel_error(...)` 命中 `OutcomeUnknown` 时：
  - 先调用 `exchange_freshness.mark_stale(track_id, SubmitOutcomeUnknown / CancelOutcomeUnknown)`
  - 再调用 `enqueue_reconcile_request(...)`
- 保持 submit preflight、取消 writeback、follow-up retirement 既有职责不变

- [ ] **Step 4: 跑 Task 3 回归**

Run:

- `cargo test -p poise-server effect_worker::tests::stale_submit_effect_syncs_exchange_before_submitting`
- `cargo test -p poise-server effect_worker::tests::stale_cancel_effect_syncs_exchange_before_canceling`
- `cargo test -p poise-server effect_worker::tests::fresh_effects_do_not_trigger_extra_sync`
- `cargo test -p poise-server effect_worker::tests::outcome_unknown_marks_track_stale_before_reconcile`
- `cargo test -p poise-server effect_worker::tests:: -- --nocapture`
- `cargo test -p poise-server runtime::tests::order_update_clears_inventory_core_slot_on_terminal_status -- --exact --nocapture`
- `cargo test -p poise-server runtime::tests::terminal_order_update_reconciles_without_waiting_for_new_tick -- --exact --nocapture`

Expected:

- submit / cancel 都统一走 freshness gate
- worker 消费 freshness abstraction，并负责写入执行结果不确定事实
- 既有 submit preflight 与异常恢复路径不回退

- [ ] **Step 5: Commit**

```bash
git add server/src/effect_worker.rs
git commit -m "feat(server): gate side effects with exchange freshness"
```

Commit:

- `<填写 commit SHA>`
