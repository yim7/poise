# Submit Preflight Lookup 优化 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让新鲜的正常 submit 默认不再调用 `openOrders` 预检查，同时保留启动恢复、同进程重复 submit，以及 write side 要求先看交易所事实时的 live order 保护。

**Architecture:** 新增一个 `submit_preflight` 协调模块，集中决定某条 pending submit effect 是走 `Direct` 还是 `NeedsLiveOrderLookup`。启动恢复的判断不再依赖时间比较，而是由 runtime 在启动阶段显式采样所有 pending submit；同进程内已尝试 submit 的跟踪也集中到同一模块，避免在 `effect_worker` 里散落条件判断。

**Tech Stack:** Rust, tokio, poise-server, poise-storage, SQLite-backed effect repository tests

---

### Task 1: 引入 submit preflight 协调状态与接口

**Files:**
- Create: `server/src/submit_preflight.rs`
- Modify: `server/src/assembly.rs`
- Modify: `server/src/effect_worker.rs`
- Modify: `server/src/runtime.rs`
- Modify: `server/src/write_service.rs`
- Modify: `docs/superpowers/plans/2026-04-02-submit-preflight-lookup-optimization.md`

- [ ] **Step 1: 写 failing tests**

补五条最小红测：

- `fresh_submit_uses_direct_preflight_without_open_orders_lookup`
- `mark_submit_started_happens_only_after_prepare_returns_some`
- `submit_preflight_decides_direct_for_fresh_effect`
- `submit_preflight_decides_lookup_for_started_effect`
- `submit_preflight_assumes_single_effect_worker_execution_order`

测试位置：
- `server/src/effect_worker.rs`
- `server/src/submit_preflight.rs`

测试要求：
- 新鲜 submit 第一次执行时，`get_open_orders_calls == 0`
- `prepare_submit_execution(...)` 返回 `None` 时，不能把该 effect 记成已尝试 submit
- `submit_preflight` 自己的单元测试要直接锁住：
  - 新鲜 effect -> `Direct`
  - 已标记 started 的 effect -> `NeedsLiveOrderLookup`

- [ ] **Step 2: 运行定向测试确认红灯**

Run:
- `cargo test -p poise-server effect_worker::tests::fresh_submit_uses_direct_preflight_without_open_orders_lookup -- --nocapture`
- `cargo test -p poise-server effect_worker::tests::mark_submit_started_happens_only_after_prepare_returns_some -- --nocapture`
- `cargo test -p poise-server submit_preflight::tests::submit_preflight_decides_direct_for_fresh_effect -- --nocapture`
- `cargo test -p poise-server submit_preflight::tests::submit_preflight_decides_lookup_for_started_effect -- --nocapture`
- `cargo test -p poise-server effect_worker::tests::submit_preflight_assumes_single_effect_worker_execution_order -- --nocapture`

Expected:
- 这些测试失败
- 失败原因分别指向当前每次 submit 都会查 `openOrders`、当前缺少显式的 submit started 协调，以及当前没有独立的 preflight 决策模块

- [ ] **Step 3: 写最小实现**

实现范围：

- 在 `server/src/submit_preflight.rs` 新增：
  - `SubmitPreflightDecision`
  - 共享状态结构，至少包含：
    - `startup_pending_submit_effects`
    - `attempted_submit_effects`
  - 协调接口，至少包含：
    - `decide(effect_id, client_order_id, hint) -> SubmitPreflightDecision`
    - `reconcile_pending_submit_effects(current_pending_submit_effect_ids)`
- 在 write side 增加一个窄提示接口，例如：
  - `submit_preflight_hint(effect_id, request, target_exposure) -> SubmitPreflightHint`
  - 例如：
    - `DirectSafe`
    - `NeedsExchangeStateLookup`
  - 只表达“当前 executor 状态是否已经要求先看交易所事实”
  - 这个最小实现必须在本 task 打通，不延后到后续 task
- 在 `ServerState` 挂入这份共享状态
- 明确保留当前单 worker 顺序执行不变量，不在本 task 引入并发 worker 语义
- 在 `effect_worker.execute_submit(...)` 中改为：
  1. 先通过 write side 的只读路径获取 `SubmitPreflightHint`
  2. 把 hint 传给 `submit_preflight.decide(effect_id, client_order_id, hint)`
  3. `Direct` 时直接走 `prepare_submit_execution(..., None)`
  4. `NeedsLiveOrderLookup` 时才调用 `exchange.get_open_orders(...)`
  5. 只有 `prepare_submit_execution(...)` 返回 `Some(prepared_submit)` 后，才 `mark_submit_started(effect_id)`
  6. 再执行 `submit_order(...)`
  7. 这里新增的是本地读，不是额外的交易所调用；本 task 不把 hint 读取和 `prepare_submit_execution(...)` 合并

- [ ] **Step 4: 运行定向测试确认通过**

Run:
- `cargo test -p poise-server effect_worker::tests::fresh_submit_uses_direct_preflight_without_open_orders_lookup -- --nocapture`
- `cargo test -p poise-server effect_worker::tests::mark_submit_started_happens_only_after_prepare_returns_some -- --nocapture`
- `cargo test -p poise-server submit_preflight::tests::submit_preflight_decides_direct_for_fresh_effect -- --nocapture`
- `cargo test -p poise-server submit_preflight::tests::submit_preflight_decides_lookup_for_started_effect -- --nocapture`
- `cargo test -p poise-server effect_worker::tests::submit_preflight_assumes_single_effect_worker_execution_order -- --nocapture`

Expected: PASS

- [ ] **Step 5: 提交 Task 1**

```bash
git add server/src/submit_preflight.rs server/src/assembly.rs server/src/effect_worker.rs server/src/runtime.rs server/src/write_service.rs docs/superpowers/plans/2026-04-02-submit-preflight-lookup-optimization.md
git commit -m "feat(server): add submit preflight coordination"
```

### Task 2: 用启动阶段显式采样替代时间代理

**Files:**
- Modify: `engine/src/ports.rs`
- Modify: `storage/src/sqlite.rs`
- Modify: `server/src/runtime.rs`
- Modify: `server/src/assembly.rs`
- Modify: `server/src/submit_preflight.rs`
- Modify: 所有实现 `StateRepositoryPort` 的测试 fake/mock repository
- Modify: `docs/superpowers/plans/2026-04-02-submit-preflight-lookup-optimization.md`

- [ ] **Step 1: 写 failing tests**

补四条红测（含 storage 一条）：

- `startup_preflight_marks_all_pending_submit_effects_not_only_dispatchable_ones`
- `startup_sampling_happens_after_startup_replay_before_effect_worker_runs`
- `submit_preflight_decides_lookup_for_startup_pending_effect`

测试位置：
- `server/src/runtime.rs`
- `storage/src/sqlite.rs`
- `server/src/submit_preflight.rs`

测试要求：
- 启动时尚不可调度、但已经是 pending submit 的旧 effect，也会进入 `startup_pending_submit_effects`
- 采样时机在 `startup_sync` 和 `replay_startup_user_data` 之后、任何长生命周期后台任务启动之前
- 启动采样到的 `effect_id` 送进 `submit_preflight.decide(...)` 时，会直接返回 `NeedsLiveOrderLookup`

- [ ] **Step 2: 运行定向测试确认红灯**

Run:
- `cargo test -p poise-storage sqlite::tests::list_all_pending_submit_effects_returns_non_dispatchable_pending_submits -- --nocapture`
- `cargo test -p poise-server runtime::tests::startup_preflight_marks_all_pending_submit_effects_not_only_dispatchable_ones -- --nocapture`
- `cargo test -p poise-server runtime::tests::startup_sampling_happens_after_startup_replay_before_effect_worker_runs -- --nocapture`
- `cargo test -p poise-server submit_preflight::tests::submit_preflight_decides_lookup_for_startup_pending_effect -- --nocapture`

Expected:
- 至少一条因缺少新仓储接口失败
- 至少一条因启动阶段没有显式采样失败

- [ ] **Step 3: 写最小实现**

实现范围：

- 在 `StateRepositoryPort` 增加：
  - `list_all_pending_submit_effects()`
- 补齐所有 `StateRepositoryPort` 实现，包括测试 fake/mock repository，避免 trait 变更只落在正式实现里
- 在 SQLite 实现中新增对应查询：
  - 条件只要求 `status == Pending`
  - 并过滤 `TrackEffect::SubmitOrder`
  - 不再要求“当前可调度”
- 在 `ServerRuntime::start()` 中：
  1. 完成 `startup_sync`
  2. 完成 `replay_startup_user_data`
  3. 调 `list_all_pending_submit_effects()`
  4. 用结果初始化 `startup_pending_submit_effects`
  5. 再启动 `recovery task`、`effect worker`、`user task`、`market task`

- [ ] **Step 4: 运行定向测试确认通过**

Run:
- `cargo test -p poise-storage sqlite::tests::list_all_pending_submit_effects_returns_non_dispatchable_pending_submits -- --nocapture`
- `cargo test -p poise-server runtime::tests::startup_preflight_marks_all_pending_submit_effects_not_only_dispatchable_ones -- --nocapture`
- `cargo test -p poise-server runtime::tests::startup_sampling_happens_after_startup_replay_before_effect_worker_runs -- --nocapture`
- `cargo test -p poise-server submit_preflight::tests::submit_preflight_decides_lookup_for_startup_pending_effect -- --nocapture`

Expected: PASS

- [ ] **Step 5: 提交 Task 2**

```bash
git add engine/src/ports.rs storage/src/sqlite.rs server/src/runtime.rs server/src/assembly.rs server/src/submit_preflight.rs server/src/http.rs server/src/websocket.rs server/src/effect_worker.rs server/src/write_service.rs docs/superpowers/plans/2026-04-02-submit-preflight-lookup-optimization.md
git commit -m "feat(server): seed startup submit preflight snapshot"
```

### Task 3: 补齐清理路径与恢复行为验收

**Files:**
- Modify: `server/src/submit_preflight.rs`
- Modify: `server/src/write_service.rs`
- Modify: `server/src/effect_worker.rs`
- Modify: `server/src/runtime.rs`
- Modify: `docs/superpowers/plans/2026-04-02-submit-preflight-lookup-optimization.md`

- [ ] **Step 1: 写 failing tests**

补四条红测：

- `restarted_pending_submit_with_matching_live_order_is_recovered_without_duplicate_submit`
- `attempted_submit_tracking_is_cleared_after_submit_success`
- `attempted_submit_tracking_is_cleared_after_submit_failure_or_supersede`
- `startup_pending_tracking_is_cleared_on_track_effect_state_changed_notification`

测试位置：
- `server/src/runtime.rs`
- `server/src/effect_worker.rs`

测试要求：
- 启动恢复场景下，如果交易所已有 matching live order，不会再次 `submit_order(...)`
- submit 成功后集合清掉
- submit 失败或 superseded 后集合也清掉
- `TrackEffectStateChanged` 到来后，runtime 会重算并清掉不再属于 pending submit 集合的 `startup_pending_submit_effects`
- `TrackEffectStateChanged` 到来后，runtime 会通过同一个重算入口清理 `attempted_submit_effects`

- [ ] **Step 2: 运行定向测试确认红灯**

Run:
- `cargo test -p poise-server runtime::tests::restarted_pending_submit_with_matching_live_order_is_recovered_without_duplicate_submit -- --nocapture`
- `cargo test -p poise-server effect_worker::tests::attempted_submit_tracking_is_cleared_after_submit_success -- --nocapture`
- `cargo test -p poise-server effect_worker::tests::attempted_submit_tracking_is_cleared_after_submit_failure_or_supersede -- --nocapture`
- `cargo test -p poise-server runtime::tests::startup_pending_tracking_is_cleared_on_track_effect_state_changed_notification -- --nocapture`

Expected: FAIL

- [ ] **Step 3: 写最小实现**

实现范围：

- 把 submit preflight 清理收敛到单一 write-side seam：
  - `write_service` 不直接删除 preflight 缓存
  - 它继续通过现有 `TrackEffectStateChanged` 通知暴露“effect 状态变了”这一事实
- 补一条 runtime 侧的明确入口：
  - runtime 在处理 `TrackEffectStateChanged` 通知时，重新读取当前 pending submit 集合
  - 调用 `submit_preflight.reconcile_pending_submit_effects(current_pending_submit_effect_ids)`
  - 由 `submit_preflight` 自己统一清理：
    - `startup_pending_submit_effects`
    - `attempted_submit_effects`
- `reconcile_pending_submit_effects(...)` 使用和启动采样相同的 pending submit 定义：
  - `status == Pending`
  - `effect == TrackEffect::SubmitOrder`
- 保持“真实 submit 已开始但 effect 仍 Pending”的场景不清理
- 启动恢复命中 matching live order 时，走恢复分支，不再重复 submit

- [ ] **Step 4: 运行定向测试确认通过**

Run:
- `cargo test -p poise-server runtime::tests::restarted_pending_submit_with_matching_live_order_is_recovered_without_duplicate_submit -- --nocapture`
- `cargo test -p poise-server effect_worker::tests::attempted_submit_tracking_is_cleared_after_submit_success -- --nocapture`
- `cargo test -p poise-server effect_worker::tests::attempted_submit_tracking_is_cleared_after_submit_failure_or_supersede -- --nocapture`
- `cargo test -p poise-server runtime::tests::startup_pending_tracking_is_cleared_on_track_effect_state_changed_notification -- --nocapture`

Expected: PASS

- [ ] **Step 5: 运行完整验证**

Run:
- `cargo test -p poise-server effect_worker::tests:: -- --nocapture`
- `cargo test -p poise-server runtime::tests:: -- --nocapture`
- `cargo test -p poise-storage -- --nocapture`

Expected:
- submit preflight 相关回归通过
- runtime 启动恢复与 effect worker 回归不受影响

- [ ] **Step 6: 更新任务清单并提交 Task 3**

在本文件里回写：
- 每个 task 的完成状态
- 对应 commit SHA

```bash
git add server/src/submit_preflight.rs server/src/write_service.rs server/src/effect_worker.rs server/src/runtime.rs docs/superpowers/plans/2026-04-02-submit-preflight-lookup-optimization.md
git commit -m "fix(server): avoid redundant open orders lookup for fresh submits"
```
