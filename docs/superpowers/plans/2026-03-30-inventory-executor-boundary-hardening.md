# Inventory Executor Boundary Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在保留 `slot` 模型和现有库存执行器主语义的前提下，把槽位生命周期、`submit recovery` 和写侧串行边界收紧到旧 spec 定义的所有权边界内。

**Architecture:** 这次不是重做库存执行器，也不引入 actor。实现只做三件事：把 `slot` 的状态推进收回 `engine::executor`，把 `submit recovery` 从 `manager / write_service / effect_worker / effect_service` 的旁路链路并回执行器，以及把 `write_service` 的全局串行锁缩到按 `grid` 串行。协议、存储和 TUI 只有在被这些边界调整直接影响时才修改，优先保持外部语义稳定。

**Tech Stack:** Rust workspace, cargo test, tokio, anyhow, chrono

---

## File Structure

### 重点修改文件

- `engine/src/executor.rs`：新增执行器拥有的槽位状态推进与 submit recovery 决策入口
- `engine/src/manager.rs`：删除直接改写槽位的辅助函数，改成把事实交给执行器
- `engine/src/runtime.rs`：收窄执行器运行态与恢复输入输出定义
- `server/src/effect_service.rs`：退回 effect 查询与状态更新，不再生成 recovery 判断
- `server/src/effect_worker.rs`：只执行 effect、收集事实、回写结果
- `server/src/write_service.rs`：改为按 `grid` 串行持久化 mutation
- `server/src/runtime.rs`：去掉 startup sync 对 recovery anchor 旁路语义的依赖
- `docs/superpowers/specs/2026-03-29-inventory-executor-architecture-design.md`：实现完成后回写最终边界
- `docs/superpowers/plans/2026-03-30-inventory-executor-boundary-hardening.md`：执行时勾选并记录 commit SHA

### 重点测试文件

- `engine/src/executor.rs`
- `engine/src/manager.rs`
- `server/src/effect_worker.rs`
- `server/src/runtime.rs`
- `server/src/write_service.rs`

---

### Task 1: 把槽位生命周期推进收回执行器

**Files:**
- Modify: `engine/src/executor.rs`
- Modify: `engine/src/manager.rs`
- Modify: `engine/src/runtime.rs`
- Modify: `server/src/effect_worker.rs`
- Test: `engine/src/executor.rs`
- Test: `engine/src/manager.rs`
- Test: `server/src/effect_worker.rs`

- [x] **Step 1: 在 `engine/src/executor.rs` 写失败测试，锁住执行器拥有槽位状态推进**

测试至少覆盖：
- `submit request` 会由执行器创建 `SubmitPending` 槽位
- `submit receipt` 会由执行器把同一槽位推进到 `Working`
- 终态订单事实会由执行器清理对应槽位
- 无匹配槽位时不会由外层偷偷补一个新槽位

- [x] **Step 2: 运行定向测试确认失败**

Run:
`cargo test -p grid-engine executor::tests::submit_receipt_promotes_submit_pending_slot_to_working -- --exact`
`cargo test -p grid-engine executor::tests::terminal_order_clears_matching_slot -- --exact`

Expected:
测试失败或编译失败，因为当前槽位推进仍有一部分散在 `manager` 辅助函数里。

- [x] **Step 3: 在 `engine/src/manager.rs` 和 `server/src/effect_worker.rs` 写失败测试，锁住外层只传事实不改槽位**

测试至少覆盖：
- `manager.observe_order()` 通过执行器 transition 清理终态槽位
- `effect_worker` 在提交成功后只回写回执事实，不直接决定槽位状态
- 现有主路径行为保持不变：成功提交后仍能看到 `working_order`

- [x] **Step 4: 运行定向测试确认失败**

Run:
`cargo test -p grid-engine manager::tests::observe_order_clears_matching_inventory_core_slot_on_terminal_status -- --exact`
`cargo test -p grid-server effect_worker::tests::submit_success_updates_working_order_without_pending_anchor -- --exact`

Expected:
测试失败，因为当前 `manager` / `effect_worker` 还在调用直接改槽位的入口。

- [x] **Step 5: 做最小实现，把槽位 transition 收回 `engine::executor`**

要求：
- 在 `engine/src/executor.rs` 增加执行器拥有的槽位事实吸收入口，覆盖 `submit request`、`submit receipt`、live order 认领和终态清理
- `engine/src/manager.rs` 删除直接 `upsert / clear slot` 的辅助函数与对外入口
- `manager` 改成把订单事实转给执行器，而不是直接写 `executor_state.slots`
- `server/src/effect_worker.rs` 改成只提交事实并触发持久化，不再隐含槽位状态机

- [x] **Step 6: 运行 Task 1 的定向测试**

Run:
`cargo test -p grid-engine executor::tests:: -- --nocapture`
`cargo test -p grid-engine manager::tests::observe_order_clears_matching_inventory_core_slot_on_terminal_status -- --exact`
`cargo test -p grid-server effect_worker::tests::submit_success_updates_working_order_without_pending_anchor -- --exact`

Expected:
槽位推进相关测试通过，且外层不再直接改写槽位。

- [x] **Step 7: 提交**

Task 1 code commit:
`48196ae618d043023f78fe9a4818545778ddc14e`

```bash
git add engine/src/executor.rs engine/src/manager.rs server/src/effect_worker.rs
git commit -m "refactor(engine): internalize slot lifecycle transitions"
```

---

### Task 2: 把 `submit recovery` 并回执行器

**Files:**
- Modify: `engine/src/executor.rs`
- Modify: `engine/src/manager.rs`
- Modify: `engine/src/runtime.rs`
- Modify: `server/src/effect_service.rs`
- Modify: `server/src/effect_worker.rs`
- Modify: `server/src/write_service.rs`
- Modify: `server/src/runtime.rs`
- Test: `engine/src/executor.rs`
- Test: `engine/src/manager.rs`
- Test: `server/src/effect_worker.rs`
- Test: `server/src/runtime.rs`
- Test: `server/src/effect_service.rs`

- [x] **Step 1: 在 `engine/src/executor.rs` 写失败测试，锁住 `submit recovery` 的执行器决策**

测试至少覆盖：
- receipt-backed 槽位存在 live order 时由执行器认领并恢复 `Working`
- 当前计划已变化且 effect 失效时由执行器返回 supersede
- 缺少充分事实时由执行器返回继续等待 exchange state

- [x] **Step 2: 运行定向测试确认失败**

Run:
`cargo test -p grid-engine executor::tests::submit_recovery_restores_live_order_from_receipt_backed_slot -- --exact`
`cargo test -p grid-engine executor::tests::submit_recovery_supersedes_stale_effect_when_current_plan_changed -- --exact`

Expected:
测试失败，因为当前 `submit recovery` 仍在 `manager` 里单独分类。

- [x] **Step 3: 在 `server/src/effect_worker.rs`、`server/src/runtime.rs` 和 `server/src/effect_service.rs` 写失败测试，锁住 server 侧只传递事实**

测试至少覆盖：
- `effect_service` 不再产出 `submit_recovery_anchor`
- `effect_worker` 只根据执行器恢复结果决定 effect 后续动作
- startup sync 不再依赖 `effect_service` 拼 recovery 旁路语义

- [x] **Step 4: 运行定向测试确认失败**

Run:
`cargo test -p grid-server effect_service::tests::submit_recovery_anchor_only_exists_for_matching_pending_submit_effect -- --exact`
`cargo test -p grid-server effect_worker::tests::submit_recovery_waits_while_recovery_anomaly_is_active -- --exact`
`cargo test -p grid-server runtime::tests::startup_sync_replans_even_when_submit_recovery_anchor_is_present -- --exact`

Expected:
现有测试需要改写或新增，因为当前 server 侧仍持有 recovery 旁路判断。

- [x] **Step 5: 做最小实现，把 `submit recovery` 彻底收回执行器**

要求：
- `engine/src/executor.rs` 增加 submit recovery 输入输出，统一判断 `Proceed / AwaitExchangeState / Recovered / Superseded`
- `engine/src/manager.rs` 删除 `SubmitRecoveryPlan / Resolution / Action` 这套旁路状态机
- `server/src/effect_service.rs` 退回纯 effect 查询与状态更新服务
- `server/src/effect_worker.rs` 只收集回执、live order 和 effect 事实，再调用写侧持久化执行器结果
- `server/src/runtime.rs` 不再从 `effect_service` 取 recovery anchor 后再驱动恢复

- [x] **Step 6: 运行 Task 2 的定向测试**

Run:
`cargo test -p grid-engine executor::tests:: -- --nocapture`
`cargo test -p grid-server effect_service::tests:: -- --nocapture`
`cargo test -p grid-server effect_worker::tests:: -- --nocapture`
`cargo test -p grid-server runtime::tests::startup_sync_replans_even_when_submit_recovery_anchor_is_present -- --exact`

Expected:
`submit recovery` 的判断与恢复入口都统一回到执行器，server 侧测试通过。

- [x] **Step 7: 提交**

Task 2 code commit:
`629d7372d55a171c7b0651ff7eb660c6ab5a3b72`

```bash
git add engine/src/executor.rs engine/src/manager.rs engine/src/runtime.rs server/src/effect_service.rs server/src/effect_worker.rs server/src/write_service.rs server/src/runtime.rs
git commit -m "refactor(server): move submit recovery into executor"
```

---

### Task 3: 把写侧串行从全局锁缩到按 `grid` 串行

**Files:**
- Modify: `server/src/write_service.rs`
- Modify: `server/src/runtime.rs`
- Test: `server/src/write_service.rs`
- Test: `server/src/runtime.rs`

- [ ] **Step 1: 在 `server/src/write_service.rs` 写失败测试，锁住同 `grid` 串行、不同 `grid` 可并行**

测试至少覆盖：
- 同一个 `grid` 的两次 mutation 仍按顺序提交
- 两个不同 `grid` 的 mutation 不共享同一把全局锁
- `recover_submit_effect()` 和常规 `mutate_grid()` 使用同一套按 `grid` 串行规则

- [ ] **Step 2: 运行定向测试确认失败**

Run:
`cargo test -p grid-server write_service::tests::mutations_for_same_grid_remain_serialized -- --exact`
`cargo test -p grid-server write_service::tests::mutations_for_different_grids_do_not_share_global_lock -- --exact`

Expected:
测试失败，因为当前 `write_service` 仍只有全局 `mutation_lock`。

- [ ] **Step 3: 做最小实现，把 `write_service` 改为按 `grid` 串行**

要求：
- 用按 `GridId` 索引的锁表替换单一 `mutation_lock`
- `mutate_grid()`、`recover_submit_effect()` 和所有写侧入口都走同一套按 `grid` guard
- 保持持久化事务与通知顺序不变
- 不引入 actor；这一步只收紧现有 `write_service` 的串行粒度

- [ ] **Step 4: 运行 Task 3 的定向测试**

Run:
`cargo test -p grid-server write_service::tests:: -- --nocapture`
`cargo test -p grid-server runtime::tests:: -- --nocapture`

Expected:
写侧并发测试通过，现有 runtime 集成测试无回归。

- [ ] **Step 5: 提交**

```bash
git add server/src/write_service.rs server/src/runtime.rs
git commit -m "refactor(server): serialize grid mutations per grid"
```

---

### Task 4: 全量回归并同步文档

**Files:**
- Modify: `docs/superpowers/specs/2026-03-29-inventory-executor-architecture-design.md`
- Modify: `docs/superpowers/plans/2026-03-30-inventory-executor-boundary-hardening.md`

- [ ] **Step 1: 运行 crate 级回归**

Run:
`cargo test -p grid-engine`
`cargo test -p grid-server`
`cargo test -p grid-storage`
`cargo test -p grid-tui`

Expected:
相关 crate 全绿。

- [ ] **Step 2: 运行工作区全量测试与格式检查**

Run:
`cargo test --workspace`
`cargo fmt --all --check`

Expected:
工作区测试通过，格式检查通过。

- [ ] **Step 3: 同步 spec 与 plan 的最终落地结果**

要求：
- 只保留这轮边界收紧实际落地的命名
- 在本 plan 每个已完成 task 后记录 commit SHA
- 若实现和 spec 出现偏差，先改 spec，不保留口头约定

- [ ] **Step 4: 提交收尾文档同步**

```bash
git add docs/superpowers/specs/2026-03-29-inventory-executor-architecture-design.md docs/superpowers/plans/2026-03-30-inventory-executor-boundary-hardening.md
git commit -m "docs: sync inventory executor boundary hardening plan"
```
