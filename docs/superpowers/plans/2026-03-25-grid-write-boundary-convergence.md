# Grid Write Boundary Convergence Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 为网格平台引入原子写侧提交与 effect outbox，让 `snapshot + events + effects` 一次性落库，并让运行时只执行已提交 effect。

**Architecture:** 保持 `engine` 继续产出 `GridTransition`，先不做 `GridRuntime` 深度重构。第一阶段只扩展仓储契约和 SQLite schema，让 `server` 写侧提交 effect outbox，并把 `server/runtime` 从“直接执行 transition.effects”改成“轮询并执行已持久化 effect，再把结果回流写侧”。

**Tech Stack:** Rust workspace, tokio, rusqlite, serde, axum

---

## File Structure

### 新建文件

```text
server/src/effect_worker.rs          # 持久化 effect 执行器
```

### 修改文件

- `engine/src/execution_plan.rs`：让 `GridEffect` 可序列化并可持久化
- `engine/src/ports.rs`：扩展仓储契约，加入 outbox 读写接口和 effect 状态模型
- `storage/src/schema.rs`：新增 `grid_effects` 表
- `storage/src/sqlite.rs`：单事务保存 `snapshot + events + effects`，并实现 outbox 状态推进
- `server/src/application.rs`：写侧提交从“快照+事件”升级为“快照+事件+effects”
- `server/src/runtime.rs`：移除直接执行 `transition.effects` 的链路，改用 effect worker
- `server/src/assembly.rs`：装配 effect worker 所需依赖
- `server/src/main.rs`：启动 effect worker
- `docs/superpowers/specs/2026-03-25-grid-write-boundary-convergence-design.md`：如实现细节有偏差，回写 spec
- `docs/superpowers/plans/2026-03-25-grid-write-boundary-convergence.md`：执行后更新勾选状态

---

### Task 1: 扩展 outbox 仓储契约并实现原子持久化

**Files:**
- Modify: `engine/src/execution_plan.rs`
- Modify: `engine/src/ports.rs`
- Modify: `storage/src/schema.rs`
- Modify: `storage/src/sqlite.rs`
- Test: `storage/src/sqlite.rs`

- [x] **Step 1: 先写失败测试，锁住 effect 会和快照、事件一起落库**

在 `storage/src/sqlite.rs` 新增测试，至少覆盖：

```rust
#[tokio::test]
async fn save_transition_persists_snapshot_events_and_effects_atomically() {
    let storage = SqliteStorage::in_memory().unwrap();
    let snapshot = test_snapshot();
    let effects = vec![GridEffect::SubmitOrder {
        request: test_order_request(),
        desired_exposure: Exposure(6.0),
    }];

    let persisted = storage
        .save_transition("test-1", &snapshot, &[test_event()], &effects)
        .await
        .unwrap();

    assert_eq!(persisted.effects.len(), 1);
    assert_eq!(storage.list_pending_effects().await.unwrap().len(), 1);
}
```

再补一个状态推进测试：

```rust
#[tokio::test]
async fn mark_effect_failed_updates_attempt_count_and_last_error() { /* ... */ }
```

- [x] **Step 2: 运行定向测试确认失败**

Run:

```bash
cargo test -p poise-storage sqlite::tests::save_transition_persists_snapshot_events_and_effects_atomically
cargo test -p poise-storage sqlite::tests::mark_effect_failed_updates_attempt_count_and_last_error
```

Expected: 因为当前没有 effect outbox 接口和表结构而失败。

- [x] **Step 3: 最小实现仓储模型与 schema**

完成这些变更：

- `engine/src/execution_plan.rs`
  - 给 `ExecutionAction` 增加 `Serialize` / `Deserialize` / `PartialEq`
- `engine/src/ports.rs`
  - 扩展 `StateRepositoryPort::save_transition()`，接收 `effects: &[GridEffect]`
  - 新增 `PersistedGridEffect` 与 `EffectStatus`
  - 新增 `list_pending_effects()`、`mark_effect_succeeded()`、`mark_effect_failed()`
  - 预留 `mark_effect_executing()` 给后续 lease / timeout 恢复策略
- `storage/src/schema.rs`
  - 新增 `grid_effects` 表与查询索引
- `storage/src/sqlite.rs`
  - 在一个事务里写 `grid_snapshots`、`domain_events`、`grid_effects`
  - 返回已落库的 `PersistedGridEffect`

- [x] **Step 4: 运行 storage 全量测试**

Run:

```bash
cargo test -p poise-storage
```

Expected: `poise-storage` 全绿。

- [x] **Step 5: 提交**

```bash
git add engine/src/execution_plan.rs engine/src/ports.rs storage/src/schema.rs storage/src/sqlite.rs
git commit -m "refactor: persist grid effects in write outbox"
```

---

### Task 2: 让写侧提交 outbox，并停止事务外补写 pending order

**Files:**
- Modify: `server/src/application.rs`
- Modify: `server/src/runtime.rs`
- Test: `server/src/application.rs`
- Test: `server/src/runtime.rs`

- [x] **Step 1: 先写失败测试，锁住写侧会提交 effect outbox**

在 `server/src/application.rs` 新增测试，覆盖：

```rust
#[tokio::test]
async fn mutate_grid_persists_effects_with_snapshot_and_events() {
    let service = test_service();

    let transition = service.observe_market("btc-core", 95.0).await.unwrap();
    assert!(transition.effects.iter().any(|effect| matches!(effect, GridEffect::SubmitOrder { .. })));

    let pending = service.repository().list_pending_effects().await.unwrap();
    assert_eq!(pending.len(), 1);
}
```

再补一个失败用例：

```rust
#[tokio::test]
async fn mutate_grid_rolls_back_when_effect_outbox_persist_fails() { /* ... */ }
```

- [x] **Step 2: 运行 application 定向测试确认失败**

Run:

```bash
cargo test -p poise-server application::tests::mutate_grid_persists_effects_with_snapshot_and_events
cargo test -p poise-server application::tests::mutate_grid_rolls_back_when_effect_outbox_persist_fails
```

Expected: 当前 `application` 没有提交 effect outbox，测试失败。

- [x] **Step 3: 最小实现写侧提交升级**

完成这些变更：

- `server/src/application.rs`
  - `mutate_grid()` 调用扩展后的 `save_transition(..., effects)`
  - 对 `()` mutation 传空 effect
  - 保留现有广播语义
- `server/src/runtime.rs`
  - 删除 `record_submission_intent()`、`record_submitted_order()`、`clear_pending_order()` 这种事务外补写链路
  - 市场/用户流写入只负责调用写侧服务，不再直接执行 `transition.effects`

- [x] **Step 4: 运行 server application/runtime 定向测试**

Run:

```bash
cargo test -p poise-server application::tests::
cargo test -p poise-server runtime::tests::market_tick_submits_order_and_records_pending_order
```

Expected:

- application 测试通过
- 旧的“立即补写 pending order”测试按新设计失败，需要在下一任务改写

- [x] **Step 5: 提交**

```bash
git add server/src/application.rs server/src/runtime.rs
git commit -m "refactor: persist grid write effects with state transitions"
```

---

### Task 3: 引入 effect worker，替换直接执行链并完成验收

**Files:**
- Create: `server/src/effect_worker.rs`
- Modify: `server/src/runtime.rs`
- Modify: `server/src/assembly.rs`
- Modify: `server/src/main.rs`
- Modify: `server/src/application.rs`
- Test: `server/src/runtime.rs`
- Test: `server/src/assembly.rs`
- Test: `server/src/http.rs`

- [x] **Step 1: 先写失败测试，锁住运行时只执行已持久化 effect**

在 `server/src/runtime.rs` 新增测试，至少覆盖：

```rust
#[tokio::test]
async fn effect_worker_executes_persisted_submit_order_and_marks_success() { /* ... */ }

#[tokio::test]
async fn effect_worker_restores_pending_effect_after_restart() { /* ... */ }

#[tokio::test]
async fn failed_effect_does_not_roll_back_committed_snapshot() { /* ... */ }
```

并把旧测试从“直接执行 transition.effects”改成“提交后由 worker 执行”。

- [x] **Step 2: 运行定向测试确认失败**

Run:

```bash
cargo test -p poise-server runtime::tests::effect_worker_executes_persisted_submit_order_and_marks_success
cargo test -p poise-server runtime::tests::effect_worker_restores_pending_effect_after_restart
cargo test -p poise-server runtime::tests::failed_effect_does_not_roll_back_committed_snapshot
```

Expected: 当前没有 effect worker，测试失败。

- [x] **Step 3: 最小实现 effect worker**

完成这些变更：

- 新建 `server/src/effect_worker.rs`
  - 轮询 `list_pending_effects()`
  - `SubmitOrder` 先把 `pending_order=Submitting` 写回快照，作为恢复锚点
  - 成功后标记 `Succeeded`
  - 失败后标记 `Failed` 并记录错误
- `server/src/runtime.rs`
  - 不再消费 `transition.effects`
  - 只负责市场/用户流接入和 observation 回流
- `server/src/assembly.rs` / `server/src/main.rs`
  - 装配并启动 effect worker

- [x] **Step 4: 跑 server 全量测试**

Run:

```bash
cargo test -p poise-server
```

Expected: `poise-server` 全绿。

- [x] **Step 5: 跑工作区全量验收**

Run:

```bash
cargo test
```

Expected: 工作区全绿。

- [x] **Step 6: 同步 spec 与任务清单**

如果实现中对字段名、接口名或执行顺序有调整：

- 更新 `docs/superpowers/specs/2026-03-25-grid-write-boundary-convergence-design.md`
- 把本计划已完成项改成 `- [x]`

- [x] **Step 7: 提交**

```bash
git add server/src/effect_worker.rs server/src/runtime.rs server/src/assembly.rs server/src/main.rs docs/superpowers/specs/2026-03-25-grid-write-boundary-convergence-design.md docs/superpowers/plans/2026-03-25-grid-write-boundary-convergence.md
git commit -m "refactor: execute persisted grid effects from write outbox"
```

---

### Review Follow-up: 收紧 effect 执行语义

**Files:**
- Modify: `engine/src/ports.rs`
- Modify: `storage/src/schema.rs`
- Modify: `storage/src/sqlite.rs`
- Modify: `server/src/effect_worker.rs`
- Modify: `server/src/runtime.rs`
- Test: `storage/src/sqlite.rs`
- Test: `server/src/runtime.rs`

- [x] `PersistedGridEffect` 增加 `batch_id` / `sequence`，让同一 transition 的 effect 有显式顺序身份
- [x] `list_pending_effects()` 只返回当前可执行的 batch head，前序 effect 未成功时不放出后续 effect
- [x] `SubmitOrder` 先把 `pending_order` 持久化成 `Submitting`，收到回执并写回状态后才标记 effect 成功
- [x] receipt 写回失败时，把 submit effect 标成 `Failed`，并保留 `Submitting` pending order 作为恢复锚点
- [x] 在没有 lease / timeout 恢复策略前，worker 不再把 effect 提前标成 `Executing`
- [x] 定向验收通过：`cargo test -p poise-storage sqlite::tests::list_pending_effects_only_returns_batch_head_until_prior_effect_succeeds`
- [x] 定向验收通过：`cargo test -p poise-server runtime::tests::effect_worker_leaves_submitting_pending_order_when_receipt_persistence_fails`
- [x] 定向验收通过：`cargo test -p poise-server runtime::tests::effect_worker_does_not_submit_follow_up_effect_after_failed_cancel_in_same_batch`
- [x] 定向验收通过：`cargo test -p poise-server runtime::tests::effect_worker_keeps_effect_pending_while_submit_is_inflight`
- [x] 全量验收通过：`cargo test -p poise-storage`
- [x] 全量验收通过：`cargo test -p poise-server`
- [x] 全量验收通过：`cargo test`

### Review Follow-up: 重启恢复与旧库兼容加固

**Files:**
- Modify: `storage/src/schema.rs`
- Modify: `server/src/effect_worker.rs`
- Modify: `server/src/runtime.rs`
- Test: `storage/src/schema.rs`
- Test: `server/src/runtime.rs`

- [x] 为 `grid_effects` 增加显式列校验，不再依赖索引创建副作用拦截旧 schema
- [x] `startup_sync()` 遇到 `Submitting` pending order 时保留恢复锚点，不在交易所状态对齐前提前清空
- [x] worker 在 submit 前检查当前快照；若同一 `client_order_id` 已被恢复，则不再重复发单
- [x] worker 在 `Submitting` 锚点仍待交易所回补时保持 effect `Pending`，避免不安全重放
- [x] submit 失败且清理 `pending_order` 再失败时，effect 仍会被标记为 `Failed`
- [x] 定向验收通过：`cargo test -p poise-storage schema::tests::initialize_rejects_legacy_grid_effects_table_without_batch_sequence -- --exact`
- [x] 定向验收通过：`cargo test -p poise-server runtime::tests::startup_sync_preserves_submitting_pending_order_until_exchange_catches_up -- --exact`
- [x] 定向验收通过：`cargo test -p poise-server runtime::tests::effect_worker_does_not_resubmit_when_matching_pending_order_is_already_restored -- --exact`
- [x] 定向验收通过：`cargo test -p poise-server runtime::tests::effect_worker_marks_effect_failed_even_if_submit_cleanup_persistence_fails -- --exact`
- [x] 全量验收通过：`cargo test -p poise-storage`
- [x] 全量验收通过：`cargo test -p poise-server`
- [x] 全量验收通过：`cargo test`
