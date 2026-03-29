# Inventory Executor Architecture Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把当前“库存目标直接翻成单笔挂单”的执行模型改造成独立库存执行器，让系统能持续管理库存偏差，并补齐可观测性与量化评价指标。

**Architecture:** 这次按五段推进：先引入 `executor_state`、`working_orders` 和执行统计的运行态与持久化，再把执行规划从 `reconciler` 下移到新的执行模块，然后重写 `manager` / `write_service` / `runtime` 的恢复与观测吸收路径，接着在不新增接口的前提下把当前诊断和累计统计投影到 detail / TUI，最后做全量回归和收尾清理。每个 task 都先写失败测试，再做最小实现，再跑定向验证并提交。

**Tech Stack:** Rust workspace, cargo test, tokio, rusqlite, serde, anyhow, chrono

---

## File Structure

### 新建文件

- `engine/src/executor.rs`：库存执行器主逻辑，负责 `ExecutionMode`、`DesiredOrders` 规划和 `working_orders` diff
- `server/tests/inventory_executor_benchmark.rs`：固定回放场景下的库存执行器 vs 传统网格 benchmark
- `server/tests/support/replay_benchmark.rs`：benchmark 共享回放 harness、对照组和报告聚合

### 修改文件

- `engine/src/lib.rs`：导出新的执行器模块
- `engine/src/runtime.rs`：引入 `ExecutorState`、`WorkingOrder`、执行原因与槽位语义，收窄 `PendingOrder`
- `engine/src/snapshot.rs`：把快照从 `pending_order` 扩展到 `executor_state`
- `engine/src/reconciler.rs`：收窄为高层库存收敛，不再直接规划单笔下单 effect
- `engine/src/manager.rs`：改成“目标库存 -> 执行器 -> 工作集 diff”的主编排路径
- `protocol/src/lib.rs`：扩展 detail 里的执行诊断和统计字段
- `storage/src/schema.rs`：为 `executor_state` / `working_orders` 持久化调整 schema
- `storage/src/sqlite.rs`：读写新的快照结构，并保留恢复所需字段
- `server/src/effect_service.rs`：删除或收窄围绕单个 `pending_order` 的恢复锚点逻辑
- `server/src/write_service.rs`：按新的快照与恢复语义改写写侧入口
- `server/src/runtime.rs`：startup sync 改成“重建工作集 -> 重新规划”
- `server/src/effect_worker.rs`：只负责逐笔 effect 执行与结果回写，不再承担执行策略判断
- `server/src/projector.rs`：在不改对外 contract 的前提下，从 `working_orders` 投影执行视图
- `server/src/query_service.rs`：测试夹具与读模型源适配新的快照结构
- `server/src/http.rs`：更新 HTTP 测试夹具
- `server/src/websocket.rs`：更新 WS 测试夹具
- `tui/src/views/instance.rs`：渲染当前执行诊断和累计统计
- `tui/src/api_client.rs`：适配扩展后的 detail 结构
- `tui/src/protocol.rs`：补齐协议反序列化测试
- `tui/tests/fixtures/grid_detail_view.json`：更新 detail 夹具
- `docs/superpowers/specs/2026-03-29-inventory-executor-architecture-design.md`：实现中若命名或边界有小调整，同步 spec
- `docs/superpowers/plans/2026-03-29-inventory-executor-architecture.md`：执行过程中同步勾选、记录提交 SHA

---

### Task 1: 引入执行器运行态与快照持久化

**Files:**
- Create: `engine/src/executor.rs`
- Modify: `engine/src/lib.rs`
- Modify: `engine/src/runtime.rs`
- Modify: `engine/src/snapshot.rs`
- Modify: `storage/src/schema.rs`
- Modify: `storage/src/sqlite.rs`
- Test: `engine/src/runtime.rs`
- Test: `engine/src/snapshot.rs`
- Test: `storage/src/sqlite.rs`

- [ ] **Step 1: 在 `engine/src/runtime.rs` 写失败测试，锁住 `ExecutorState` 和 `WorkingOrder` 的最小形状**

测试要覆盖：
- `GridRuntime::snapshot()` 会带出 `executor_state`
- `restore_from_snapshot()` 能恢复 `mode`、`inventory_gap`、`working_orders`
- `ExecutionStats` 会跟随 snapshot 一起持久化
- `DesiredOrders` 不在 snapshot 中持久化

- [ ] **Step 2: 运行定向测试确认失败**

Run:
`cargo test -p grid-engine runtime::tests::snapshot_round_trips_executor_state -- --exact`

Expected:
编译失败或测试失败，因为当前 `GridRuntimeSnapshot` 仍然只有 `pending_order`。

- [ ] **Step 3: 在 `storage/src/sqlite.rs` 写失败测试，锁住 `executor_state` 持久化**

测试要覆盖：
- 保存带 `working_orders` 的 snapshot 后能正确读回
- 旧 `pending_order_json` 不能再作为运行态唯一来源

- [ ] **Step 4: 运行定向测试确认失败**

Run:
`cargo test -p grid-storage sqlite::tests::saves_and_loads_executor_state_with_working_orders -- --exact`

Expected:
编译失败或测试失败，因为当前 schema 和 sqlite 读写逻辑还没有 `executor_state`。

- [ ] **Step 5: 做最小实现，建立执行器运行态骨架**

要求：
- `engine/src/executor.rs` 增加 `ExecutionMode`、`ExecutionReason`、`OrderRole`、`OrderSlot`、`DesiredOrder`
- `engine/src/runtime.rs` 增加 `ExecutorState`、`WorkingOrder` 和 `ExecutionStats`
- `engine/src/snapshot.rs` 改成持久化 `executor_state`
- `storage/src/schema.rs` / `storage/src/sqlite.rs` 改成读写新的快照结构
- 先不删除旧字段使用点之外的全部旧代码，优先让新结构可存可读

- [ ] **Step 6: 运行 Task 1 的定向测试**

Run:
`cargo test -p grid-engine runtime::tests::snapshot_round_trips_executor_state -- --exact`
`cargo test -p grid-storage sqlite::tests::saves_and_loads_executor_state_with_working_orders -- --exact`

Expected:
两组测试通过。

- [ ] **Step 7: 提交**

```bash
git add engine/src/lib.rs engine/src/executor.rs engine/src/runtime.rs engine/src/snapshot.rs storage/src/schema.rs storage/src/sqlite.rs
git commit -m "refactor(engine): add inventory executor runtime state"
```

---

### Task 2: 把执行规划从 `reconciler` 下移到执行器

**Files:**
- Modify: `engine/src/executor.rs`
- Modify: `engine/src/reconciler.rs`
- Modify: `engine/src/manager.rs`
- Test: `engine/src/executor.rs`
- Test: `engine/src/reconciler.rs`
- Test: `engine/src/manager.rs`

- [ ] **Step 1: 在 `engine/src/executor.rs` 写失败测试，锁住 `Passive / Rebalance / CatchUp` 的模式切换**

测试至少覆盖：
- 小偏差进入 `Passive`
- 偏差扩大或超时进入 `Rebalance`
- 再扩大或再超时进入 `CatchUp`
- 规划过程中会更新 `last_execution_reason`
- 发生重报价、进入 `CatchUp` 时会累加对应统计

- [ ] **Step 2: 运行定向测试确认失败**

Run:
`cargo test -p grid-engine executor::tests::plans_execution_mode_from_gap_and_age -- --exact`

Expected:
编译失败，因为执行器规划逻辑还不存在。

- [ ] **Step 3: 在 `engine/src/manager.rs` 写失败测试，锁住“先算 `DesiredOrders`，再 diff 工作集”的主路径**

测试至少覆盖：
- 市场观察后不会直接写单笔 `SubmitOrder`，而是通过执行器决定 effect
- `DesiredOrders` 与 `working_orders` 等价时返回 `NoOp`
- 常规改挂不生成 `CancelAll`

- [ ] **Step 4: 运行定向测试确认失败**

Run:
`cargo test -p grid-engine manager::tests::observe_market_plans_through_inventory_executor -- --exact`
`cargo test -p grid-engine manager::tests::executor_noop_when_working_orders_match_desired_orders -- --exact`

Expected:
测试失败，因为当前 `manager` / `reconciler` 仍然是单订单路径。

- [ ] **Step 5: 做最小实现，把规划收回执行器**

要求：
- `reconciler` 只返回高层 `target_exposure` 和事件
- 执行器根据 `target_exposure`、`current_exposure`、`reference_price`、`executor_state` 生成 `DesiredOrders`
- 执行器对 `DesiredOrders` 与 `working_orders` 做 diff
- `manager` 改为调用执行器并把 diff 结果翻成 effect
- 正常路径不再默认使用 `CancelAll + SubmitOrder`
- 执行器同步更新当前诊断与累计统计

- [ ] **Step 6: 运行 Task 2 的定向测试**

Run:
`cargo test -p grid-engine executor::tests:: -- --nocapture`
`cargo test -p grid-engine manager::tests::observe_market_plans_through_inventory_executor -- --exact`
`cargo test -p grid-engine manager::tests::executor_noop_when_working_orders_match_desired_orders -- --exact`

Expected:
执行器模式与 diff 相关测试通过。

- [ ] **Step 7: 提交**

```bash
git add engine/src/executor.rs engine/src/reconciler.rs engine/src/manager.rs
git commit -m "refactor(engine): move execution planning into inventory executor"
```

---

### Task 3: 重写观测吸收、恢复和 worker 边界

**Files:**
- Modify: `engine/src/manager.rs`
- Modify: `server/src/effect_service.rs`
- Modify: `server/src/write_service.rs`
- Modify: `server/src/runtime.rs`
- Modify: `server/src/effect_worker.rs`
- Test: `engine/src/manager.rs`
- Test: `server/src/runtime.rs`
- Test: `server/src/effect_worker.rs`
- Test: `server/src/write_service.rs`

- [ ] **Step 1: 在 `server/src/runtime.rs` 写失败测试，锁住 startup sync 会先重建 `working_orders` 再重新规划**

测试要覆盖：
- live position 和 live open orders 会被吸收到新的 `executor_state`
- 恢复后是“工作集重建 + 重算”，不是延续旧 `pending_order` 锚点补丁

- [ ] **Step 2: 运行定向测试确认失败**

Run:
`cargo test -p grid-server runtime::tests::startup_sync_rebuilds_working_orders_before_replanning -- --exact`

Expected:
测试失败，因为当前恢复仍围绕 `pending_order`。

- [ ] **Step 3: 在 `server/src/effect_worker.rs` 和 `server/src/write_service.rs` 写失败测试，锁住 worker 只做逐笔执行与回写**

测试至少覆盖：
- 提交成功后更新对应 `working_order`
- 取消成功后清理对应 `working_order`
- worker 不再依赖单个 `pending_order` 的 submit anchor

- [ ] **Step 4: 运行定向测试确认失败**

Run:
`cargo test -p grid-server effect_worker::tests::submit_success_updates_working_order_without_pending_anchor -- --exact`
`cargo test -p grid-server write_service::tests::recovers_working_orders_from_live_exchange_state -- --exact`

Expected:
测试失败，因为当前 write service / worker 仍围绕 `pending_order`。

- [ ] **Step 5: 做最小实现，重写恢复与观测吸收路径**

要求：
- `manager.observe()` / `manager.sync_exchange_state()` 改成更新 `working_orders`
- `write_service` 改为保存新的快照结构
- `runtime.startup_sync()` 改为“重建工作集 -> 重算 `DesiredOrders` -> diff”
- `effect_worker` 只做 effect 执行与结果回写
- `effect_service` 删除或收窄所有围绕单个 `pending_order` 的中心语义

- [ ] **Step 6: 运行 Task 3 的定向测试**

Run:
`cargo test -p grid-server runtime::tests::startup_sync_rebuilds_working_orders_before_replanning -- --exact`
`cargo test -p grid-server effect_worker::tests::submit_success_updates_working_order_without_pending_anchor -- --exact`
`cargo test -p grid-server write_service::tests::recovers_working_orders_from_live_exchange_state -- --exact`

Expected:
恢复链路与 worker 边界测试通过。

- [ ] **Step 7: 提交**

```bash
git add engine/src/manager.rs server/src/effect_service.rs server/src/write_service.rs server/src/runtime.rs server/src/effect_worker.rs
git commit -m "refactor(server): recover inventory executor working orders"
```

---

### Task 4: 补执行可观测性并保持对外 contract 不变

**Files:**
- Modify: `protocol/src/lib.rs`
- Modify: `server/src/projector.rs`
- Modify: `server/src/query_service.rs`
- Modify: `server/src/http.rs`
- Modify: `server/src/websocket.rs`
- Modify: `tui/src/views/instance.rs`
- Modify: `tui/src/api_client.rs`
- Modify: `tui/src/protocol.rs`
- Modify: `tui/tests/fixtures/grid_detail_view.json`
- Test: `server/src/projector.rs`
- Test: `server/src/http.rs`
- Test: `server/src/websocket.rs`
- Test: `tui/src/views/instance.rs`
- Test: `tui/src/api_client.rs`
- Test: `tui/src/protocol.rs`

- [ ] **Step 1: 在 `server/src/projector.rs` 写失败测试，锁住从 `working_orders` 投影当前执行诊断和累计统计**

测试至少覆盖：
- `pending_order_count` 改由 `working_orders` 与 pending effects 推导
- 详情页里的 `execution.pending_order` 继续返回一个稳定的代表订单视图
- `execution` 中增加当前 `mode`、`inventory_gap`、`gap_age_ms`、`working_order_count`、`last_execution_reason`
- `statistics` 中增加 `requote_count`、`catch_up_count`、提交/撤单/成交计数、`max_inventory_gap_abs`、`max_gap_age_ms`

- [ ] **Step 2: 运行定向测试确认失败**

Run:
`cargo test -p grid-server projector::tests::projects_execution_badge_from_working_orders -- --exact`
`cargo test -p grid-server projector::tests::projects_detail_pending_order_from_primary_working_order -- --exact`
`cargo test -p grid-server projector::tests::projects_execution_observability_statistics -- --exact`

Expected:
测试失败，因为当前 projector 还没有新的执行诊断和统计字段。

- [ ] **Step 3: 更新 protocol、HTTP / WS、TUI 夹具，锁住可观测性字段**

测试要覆盖：
- `/grids` 继续返回 `pending_order_count`
- `/grids/:id` 继续返回 `execution.pending_order`
- `/grids/:id` 新增当前执行诊断和累计统计字段
- WebSocket 详情推送同步带出这些字段
- TUI detail 视图能显示当前模式、偏差、偏差持续时间、工作订单数量和累计统计

- [ ] **Step 4: 运行定向测试确认失败**

Run:
`cargo test -p grid-server http::tests:: -- --nocapture`
`cargo test -p grid-server websocket::tests:: -- --nocapture`
`cargo test -p grid-tui renders_grid_detail_execution_activity_and_commands -- --exact`
`cargo test -p grid-tui api_client::tests:: -- --nocapture`
`cargo test -p grid-tui protocol::tests:: -- --nocapture`

Expected:
协议与 TUI 夹具需要更新，新增字段测试失败。

- [ ] **Step 5: 做最小实现，把观测字段投影到现有 detail / TUI**

要求：
- `protocol/src/lib.rs` 为现有 detail 结构补充默认可选观测字段
- `projector` 从 `working_orders` 推导现有执行徽标与详情视图
- `projector` 同时投影当前诊断与累计统计
- 优先选一个稳定“主工作单”映射到现有 `pending_order` 字段
- `query_service` / `http` / `websocket` / `tui` 夹具全部切到新的快照结构
- `tui/src/views/instance.rs` 把新增字段渲染到 Execution / Statistics 区块

- [ ] **Step 6: 运行 Task 4 的定向测试**

Run:
`cargo test -p grid-server projector::tests:: -- --nocapture`
`cargo test -p grid-server http::tests:: -- --nocapture`
`cargo test -p grid-server websocket::tests:: -- --nocapture`
`cargo test -p grid-tui renders_grid_detail_execution_activity_and_commands -- --exact`
`cargo test -p grid-tui`

Expected:
投影和协议相关测试通过，detail / TUI 能直接看到当前诊断和累计统计。

- [ ] **Step 7: 提交**

```bash
git add protocol/src/lib.rs server/src/projector.rs server/src/query_service.rs server/src/http.rs server/src/websocket.rs tui/src/views/instance.rs tui/src/api_client.rs tui/src/protocol.rs tui/tests/fixtures/grid_detail_view.json
git commit -m "feat(observability): project inventory executor diagnostics and stats"
```

---

### Task 5: 建立回放 benchmark，对照传统网格基线

**Files:**
- Create: `server/tests/inventory_executor_benchmark.rs`
- Create: `server/tests/support/replay_benchmark.rs`
- Modify: `server/src/runtime.rs`
- Modify: `docs/superpowers/specs/2026-03-29-inventory-executor-architecture-design.md`
- Modify: `docs/superpowers/plans/2026-03-29-inventory-executor-architecture.md`

- [ ] **Step 1: 在 `server/tests/inventory_executor_benchmark.rs` 写失败测试，锁住固定回放场景和 benchmark 报告结构**

测试至少覆盖：
- 同一段价格路径、同一套成交规则、同一套交易所约束
- 同时跑：
  - 当前库存执行器
  - 传统网格基线执行器
- 输出稳定报告字段：
  - `mean_abs_inventory_gap`
  - `max_inventory_gap_abs`
  - `max_gap_age_ms`
  - `submit_count`
  - `cancel_count`
  - `fill_count`
  - `requote_count`
  - `catch_up_count`
  - `realized_pnl`
  - `net_realized_pnl`

- [ ] **Step 2: 运行定向测试确认失败**

Run:
`cargo test -p grid-server inventory_executor_benchmark::tests::replay_benchmark_compares_inventory_executor_against_traditional_grid -- --exact`

Expected:
编译失败，因为 benchmark harness 和传统网格基线还不存在。

- [ ] **Step 3: 在 `server/tests/support/replay_benchmark.rs` 实现最小回放 harness**

要求：
- 尽量复用现有 `runtime_fixture` / 测试工具的输入形状
- 用固定价格路径和固定成交判定，避免把 benchmark 变成不稳定随机测试
- 主运行时只提供原始执行事实，不把“传统网格对照逻辑”塞进生产代码

- [ ] **Step 4: 写失败测试，锁住 benchmark 使用运行时原始事实而不是额外业务分支**

测试至少覆盖：
- benchmark 报告能从执行器现有原始事实聚合得到
- 不要求生产代码同时维护两套执行路径

- [ ] **Step 5: 运行定向测试确认失败**

Run:
`cargo test -p grid-server inventory_executor_benchmark::tests::benchmark_report_uses_runtime_observability_facts -- --exact`

Expected:
测试失败，因为当前还没有聚合逻辑。

- [ ] **Step 6: 做最小实现，产出稳定 benchmark 报告**

要求：
- 报告从运行时原始事实和累计统计聚合
- 传统网格基线只存在于回放 harness
- benchmark 输出保持纯测试能力，不新增生产接口

- [ ] **Step 7: 运行 Task 5 的定向测试**

Run:
`cargo test -p grid-server inventory_executor_benchmark::tests:: -- --nocapture`

Expected:
固定回放场景 benchmark 通过，并能稳定输出两套执行逻辑的对比指标。

- [ ] **Step 8: 提交**

```bash
git add server/tests/inventory_executor_benchmark.rs server/tests/support/replay_benchmark.rs server/src/runtime.rs docs/superpowers/specs/2026-03-29-inventory-executor-architecture-design.md docs/superpowers/plans/2026-03-29-inventory-executor-architecture.md
git commit -m "test(benchmark): add replay comparison against traditional grid"
```

---

### Task 6: 全量回归、文档同步和旧语义清理

**Files:**
- Modify: `engine/src/runtime.rs`
- Modify: `engine/src/manager.rs`
- Modify: `server/src/runtime.rs`
- Modify: `docs/superpowers/specs/2026-03-29-inventory-executor-architecture-design.md`
- Modify: `docs/superpowers/plans/2026-03-29-inventory-executor-architecture.md`

- [ ] **Step 1: 清理未再使用的 `pending_order` 中心语义和遗留辅助函数**

重点检查：
- 提交恢复分支是否仍以单个 `pending_order` 为中心
- `CancelAll` 是否仍在常规路径出现
- 旧 `replacement_gate_reason` 判断是否被错误保留在主执行路径

- [ ] **Step 2: 运行 crate 级回归**

Run:
`cargo test -p grid-engine`
`cargo test -p grid-storage`
`cargo test -p grid-server`
`cargo test -p grid-tui`

Expected:
四个 crate 全绿。

- [ ] **Step 3: 运行工作区全量测试与格式检查**

Run:
`cargo test --workspace`
`cargo fmt --all --check`

Expected:
全量测试通过；如果 `fmt --check` 因既有未格式化文件失败，至少格式化本次修改文件并记录结果。

- [ ] **Step 4: 同步 spec 与 plan 里的最终命名、任务勾选和提交 SHA**

要求：
- 只记录这次实际落地的命名
- 删除执行过程中不再适用的步骤
- 在每个已完成 task 后写入对应提交 SHA

- [ ] **Step 5: 提交收尾变更**

```bash
git add engine/src/runtime.rs engine/src/manager.rs server/src/runtime.rs docs/superpowers/specs/2026-03-29-inventory-executor-architecture-design.md docs/superpowers/plans/2026-03-29-inventory-executor-architecture.md
git commit -m "refactor(engine): finalize inventory executor migration"
```
