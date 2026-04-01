# Order Operation Confirmation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让运行中的订单生命周期对交易所真实状态持续对齐，避免 slot 被错误替换、未知挂单只能靠重启发现，以及 `submit/cancel` 竞态长期阻碍系统。

**Architecture:** 保留当前单 `inventory_core` slot 和现有 executor 恢复模型，不引入独立订单台账。实现分四层推进：先收紧 executor 的 slot 所有权，避免 receipt-backed working order 被后续 submit 抢占；再引入统一 `ReconcileRequest / ReconcileReason` 抽象、`ReconcileExecution` 可观测载体与 outcome classifier，把三类触发源收敛到单一对账入口；然后给所有正常 track 增加低频 `position + openOrders` 巡检；最后补 write-side 生命周期退休规则与 runtime 级竞态回归，锁住旧单终结后旧 batch 残留与新 batch 并发的问题。

**Tech Stack:** Rust workspace, Tokio, Axum, Binance Futures REST/WebSocket adapter, SQLite, Cargo tests, Markdown

---

## Files And Responsibilities

- Modify: `engine/src/executor/recovery.rs`
  负责 `submit recovery` 的执行器判定，确保后续 submit 不会覆盖 receipt-backed working order。
- Modify: `engine/src/executor/mod.rs`
  补 executor 级失败/回归测试，锁住 slot 所有权和 submit recovery 边界。
- Modify: `engine/src/executor/recording.rs`
  负责 `OrderUpdateAbsorbResult` 与“无法吸收”规则的唯一实现，建议收敛在 `apply_order_observation(...)` 或紧邻它的单一 helper，避免 runtime 和 classifier 各自维护半套判定。
- Create: `server/src/order_outcome.rs`
  负责把 REST 错误、receipt 回写异常翻译成 `OutcomeClass` / `ReconcileReason`。
- Modify: `server/src/runtime.rs`
  消费 `OrderUpdateAbsorbResult` 与 `ReconcileRequest`，负责统一 per-track 对账排队、`ReconcileExecution` 合并语义与巡检调度。
- Modify: `server/src/effect_worker.rs`
  把 `submit receipt unmatched`、`Unknown order sent` 等情况收敛为 `ReconcileRequest`，不直接散落对账逻辑。
- Modify: `server/src/write_service.rs`
  拥有 stale follow-up submit 的退休规则，消费显式 `FollowUpRetirementRequest`，基于 per-track 写事务清理旧 lifecycle 的残留 effect。
- Modify: `docs/superpowers/specs/2026-04-01-order-operation-confirmation-design.md`
  在实现后回写与最终行为一致的说明。
- Modify: `docs/superpowers/plans/2026-04-01-order-operation-confirmation.md`
  执行时勾选步骤，并在每个完成 task 后记录 commit SHA。

### Task 1: 收紧 executor slot 所有权

**Files:**
- Modify: `engine/src/executor/mod.rs`
- Modify: `engine/src/executor/recovery.rs`
- Test: `engine/src/executor/mod.rs`

- [x] **Step 1: 写失败测试，锁住“receipt-backed 大单不能被后续小 submit recovery 抢 slot”**

在 `engine/src/executor/mod.rs` 增加一个 executor 测试：

```rust
#[test]
fn submit_recovery_does_not_overwrite_receipt_backed_large_order_with_current_small_submit() {
    // previous_state 中保留一张 receipt-backed 的 working sell
    // 新 request 是一张匹配当前小 gap 的 reduce_only buy
    // 预期 recovery 返回 AwaitExchangeState，而不是 Proceed
}
```

- [x] **Step 2: 运行测试确认失败**

Run: `cargo test -p poise-engine executor::tests::submit_recovery_does_not_overwrite_receipt_backed_large_order_with_current_small_submit -- --exact --nocapture`

Expected: FAIL，原因是当前 `recover_submit_effect()` 会让后续小 submit 继续 `Proceed`。

- [x] **Step 3: 最小实现，阻止 foreign receipt-backed working order 被覆盖**

在 `engine/src/executor/recovery.rs`：

- 如果 `previous_state` 中存在 `order_id.is_some()` 且 `client_order_id != input.request.client_order_id` 的 working order
- 则 `recover_submit_effect()` 直接返回 `AwaitExchangeState`
- 不再允许新的 submit recovery 覆盖当前 slot

- [x] **Step 4: 运行回归验证**

Run:

- `cargo test -p poise-engine executor::tests::submit_recovery_does_not_overwrite_receipt_backed_large_order_with_current_small_submit -- --exact --nocapture`
- `cargo test -p poise-engine executor::tests::submit_recovery_supersedes_stale_effect_when_current_plan_changed -- --exact --nocapture`
- `cargo test -p poise-engine executor::tests::submit_recovery_does_not_supersede_receipt_backed_working_order_when_plan_changes -- --exact --nocapture`

Expected: 全部 PASS。

- [x] **Step 5: Commit**

```bash
git add engine/src/executor/mod.rs engine/src/executor/recovery.rs
git commit -m "fix(engine): preserve receipt-backed slot ownership during submit recovery"
```

Commit: `fe0dff7`

### Task 2: 引入统一对账触发抽象与结果归类层

**Files:**
- Create: `server/src/order_outcome.rs`
- Modify: `engine/src/executor/recording.rs`
- Modify: `server/src/effect_worker.rs`
- Modify: `server/src/runtime.rs`
- Test: `server/src/order_outcome.rs`
- Test: `server/src/effect_worker.rs`
- Test: `engine/src/executor/recording.rs`

- [ ] **Step 1: 写失败测试，锁住 `OutcomeUnknown -> ReconcileRequest`**

在 `server/src/order_outcome.rs` 增加测试：

```rust
#[test]
fn classify_unknown_order_sent_as_cancel_outcome_unknown() {
    // 输入 Binance -2011
    // 输出 OutcomeUnknown(ReconcileReason::CancelOutcomeUnknown)
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p poise-server order_outcome::tests::classify_unknown_order_sent_as_cancel_outcome_unknown -- --exact --nocapture`

Expected: FAIL，原因是当前还没有统一 outcome classifier。

- [ ] **Step 3: 最小实现，新增 `OutcomeClass` / `ReconcileReason`**

在 `server/src/order_outcome.rs`：

- 定义：
  - `OutcomeClass`
  - `ReconcileReason`
- 提供纯函数：
  - REST 错误 -> `OutcomeClass`
  - receipt 回写异常 -> `OutcomeClass`

在 `server/src/effect_worker.rs`：

- 不直接做 Binance 错误语义判断
- 只把结果翻译成 `ReconcileRequest`

在 `server/src/runtime.rs`：

- 提供统一 `enqueue_reconcile_request(track_id, reason)` 入口
- 明确 per-track 队列规则：
  - 同 track 同时最多一个 in-flight reconcile
  - `PeriodicAudit` 可被紧急 reason 升级
  - 紧急请求存在时，新的 `PeriodicAudit` 直接丢弃
  - 每次实际执行都产出或可观察到一个等价 `ReconcileExecution`
  - `ReconcileExecution` 至少保留：
    - `trigger_class`
    - `merged_reasons`
- 此阶段只要求先跑通立即对账请求，不要求实现巡检

在 `server` 的 order update 路径：

- 不经过 `order_outcome`
- 先由 `engine/src/executor/recording.rs::apply_order_observation(...)` 或紧邻它的唯一 helper 返回 `OrderUpdateAbsorbResult`
- 再由 `runtime` 把 `Unabsorbed` 映射成 `ReconcileRequest::UnabsorbedOrderUpdate`

- [ ] **Step 4: 运行相关回归**

Run:

- `cargo test -p poise-server order_outcome::tests:: -- --nocapture`
- `cargo test -p poise-server effect_worker::tests::submit_receipt_unmatched_resyncs_exchange_state_before_marking_effect_failed -- --exact --nocapture`
- `cargo test -p poise-engine executor::recording::tests:: -- --nocapture`

Expected: 全部 PASS。

- [ ] **Step 5: Commit**

```bash
git add server/src/order_outcome.rs server/src/effect_worker.rs server/src/runtime.rs
git commit -m "refactor(server): unify reconcile trigger classification"
```

### Task 3: 给正常 track 增加低频对账巡检

**Files:**
- Modify: `server/src/runtime.rs`
- Test: `server/src/runtime.rs`

- [ ] **Step 1: 写失败测试，锁住“未知挂单不重启也能被发现”**

在 `server/src/runtime.rs` 增加测试：

```rust
#[tokio::test]
async fn normal_track_low_frequency_reconcile_discovers_untracked_live_orders_without_restart() {
    // 初始 snapshot 为 normal，无 recovery_anomaly
    // 交易所 open_orders 中注入一张本地无法认领的 live order
    // 不重启，仅等待巡检触发
    // 预期进入现有恢复路径，而不是持续 normal
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p poise-server runtime::tests::normal_track_low_frequency_reconcile_discovers_untracked_live_orders_without_restart -- --exact --nocapture`

Expected: FAIL，原因是当前 recovery task 只轮询已进入 anomaly 的 track。

- [ ] **Step 3: 最小实现，把 recovery task 改成“正常轨道低频巡检 + 异常轨道快速重试”**

在 `server/src/runtime.rs`：

- 保留当前 anomaly track 的快速重试逻辑
- 新增统一 `ReconcileRequest::PeriodicAudit`
- 对所有非终止 track 的低频巡检只负责发 `PeriodicAudit`
- 统一入口负责真正执行 `sync_exchange_state_from_exchange(...)`
- 低频巡检测试必须覆盖：
  - `PeriodicAudit` 不会与紧急 reconcile 并发
  - 同 track 的紧急请求会升级待执行的低频请求
  - 合并执行后的 `ReconcileExecution` 仍能区分“紧急”与“巡检”
- 不新增新的 executor 接口；仍由 executor 产出 `Rebuilt / Anomaly`

- [ ] **Step 4: 运行 runtime 回归**

Run:

- `cargo test -p poise-server runtime::tests::normal_track_low_frequency_reconcile_discovers_untracked_live_orders_without_restart -- --exact --nocapture`
- `cargo test -p poise-server runtime::tests::recovery_task_resyncs_recovery_anomaly_automatically_without_user_data -- --exact --nocapture`
- `cargo test -p poise-server runtime::tests::recovery_task_cancels_unknown_live_orders_automatically -- --exact --nocapture`

Expected: 全部 PASS。

- [ ] **Step 5: Commit**

```bash
git add server/src/runtime.rs
git commit -m "feat(server): add low-frequency exchange reconcile for normal tracks"
```

### Task 4: 锁住“旧单已成交、后续小单继续出现”的完整竞态

**Files:**
- Modify: `server/src/runtime.rs`
- Modify: `server/src/write_service.rs`
- Test: `server/src/runtime.rs`

- [ ] **Step 1: 写失败测试，复现“旧 batch 卡住 + 新 batch 继续发小单”**

在 `server/src/runtime.rs` 增加端到端测试，场景包括：

- receipt-backed 大单在准备 cancel 时，交易所先给出 terminal `Filled`
- 同 batch follow-up submit 仍被 pending 堵住
- 下一轮 `position reconcile` 又产出新的小 `reduce_only` submit
- 断言不会出现“旧 batch 残留 + 新 batch 继续发”的长期堆积
- 同时明确旧 lifecycle 的 follow-up submit 由 write-side 清理，而不是 runtime 特判

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p poise-server runtime::tests::filled_order_after_failed_cancel_does_not_leave_stale_follow_up_submit_blocking_new_lifecycle -- --exact --nocapture`

Expected: FAIL，原因是当前实现允许旧 batch 的 follow-up submit 挂着，而新的小单继续产生。

- [ ] **Step 3: 最小实现，清理已失效 lifecycle 的 stale follow-up submit**

在 `server/src/write_service.rs` 修复：

- 新增显式输入，例如 `FollowUpRetirementRequest`
- 当 cancel effect 因 `Unknown order sent` 进入对账，且对账/terminal order update 已经证明旧 working order 生命周期终结
- 则通过 `FollowUpRetirementRequest` 驱动同 batch 被其阻塞的 follow-up submit 被 `superseded` 或等价清理
- 避免旧 lifecycle 的 pending submit 永远残留
- runtime 不拥有旧 batch 退休逻辑，只负责把相关事实送进统一对账入口

- [ ] **Step 4: 运行完整回归**

Run:

- `cargo test -p poise-server runtime::tests::filled_order_after_failed_cancel_does_not_leave_stale_follow_up_submit_blocking_new_lifecycle -- --exact --nocapture`
- `cargo test -p poise-server runtime::tests::filled_order_after_failed_cancel_allows_new_small_submit_while_old_follow_up_stays_pending -- --exact --nocapture`
- `cargo test -p poise-server runtime::tests::effect_worker_does_not_submit_follow_up_effect_after_failed_cancel_in_same_batch -- --exact --nocapture`

Expected: 新测试 PASS；`filled_order_after_failed_cancel_allows_new_small_submit_while_old_follow_up_stays_pending` 必须改名或改断言，迁移到“旧 lifecycle 的 follow-up submit 被 write-side retired”这一新语义；不再允许旧 lifecycle 的 pending submit 长期残留。

- [ ] **Step 5: Commit**

```bash
git add server/src/runtime.rs server/src/write_service.rs
git commit -m "fix(server): retire stale follow-up submits after terminal cancel races"
```

### Task 5: 同步文档并跑验收

**Files:**
- Modify: `docs/superpowers/specs/2026-04-01-order-operation-confirmation-design.md`
- Modify: `docs/superpowers/plans/2026-04-01-order-operation-confirmation.md`

- [ ] **Step 1: 回写最终行为到 spec**

更新 `docs/superpowers/specs/2026-04-01-order-operation-confirmation-design.md`：

- 记录最终 `ReconcileRequest / ReconcileReason` 抽象
- 记录最终 `ReconcileExecution` 或等价可观测载体
- 记录最终 `OrderUpdateAbsorbResult` 抽象
- 记录最终 per-track 队列合并策略
- 记录最终 `trigger_class = periodic|emergency` 语义
- 记录最终采用的低频巡检频率
- 记录 `Unknown order sent` 的最终处理语义
- 记录 `UnabsorbedOrderUpdate` 的最终可检验定义
- 记录 `UnabsorbedOrderUpdate` 判定实现的唯一落点
- 记录 `FollowUpRetirementRequest` 或等价输入的最终形状
- 记录 stale follow-up submit 的清理规则

- [ ] **Step 2: 运行重点验收**

Run:

- `cargo test -p poise-engine executor::tests:: -- --nocapture`
- `cargo test -p poise-server effect_worker::tests:: -- --nocapture`
- `cargo test -p poise-server runtime::tests:: -- --nocapture`

Expected: 全部 PASS。

- [ ] **Step 3: 更新计划勾选与提交记录**

在本计划文件中：

- 勾选已完成步骤
- 在每个 Task 末尾记录对应 commit SHA

- [ ] **Step 4: Commit**

```bash
git add docs/superpowers/specs/2026-04-01-order-operation-confirmation-design.md docs/superpowers/plans/2026-04-01-order-operation-confirmation.md
git commit -m "docs: sync order operation confirmation plan and spec"
```
