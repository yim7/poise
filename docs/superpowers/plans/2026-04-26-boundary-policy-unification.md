# Boundary Policy 统一重构任务清单

> 执行规则：每个任务先补或确认验收测试，再实现；验收通过后立即提交，并把 commit SHA 回写到对应任务。

**目标：** 在不改变生产执行行为的前提下，把 Normal 模式下 CatchUp / CurveMaker 的跨 policy 协议合并进统一 `BoundaryPolicy`，降低 executor planning 的概念数量。

**设计文档：** [../specs/2026-04-26-boundary-policy-unification-design.md](../specs/2026-04-26-boundary-policy-unification-design.md)

## 非目标

- 不删除 boundary progress ledger。
- 不改变 Binance order fill / position update 的事实模型。
- 不改变 external effect boundary。
- 不把 ManualOverride / Flatten 合并进 Normal boundary policy。

## Task 1：确定性清理和测试补齐

**目的：** 先消除不会改变设计的噪音，并补齐后续重构需要依赖的行为测试。

**文件：**

- `engine/src/executor/planning.rs`
- `engine/src/executor/recording.rs`
- `engine/src/executor/mod.rs`
- `engine/src/manager.rs`
- `application/src/mutation_executor.rs`
- `server/src/runtime/startup_bootstrap.rs`
- `engine/src/executor/ledger.rs`
- `engine/src/executor/recording.rs`
- `engine/src/executor/planning.rs`

**步骤：**

- [x] 补 terminal binding 清理测试，验证 `plan()` 返回前不会保留 `Terminal` binding。
- [x] 补 `anchor_crossed_qty` middle branch 测试。
- [x] 补 sell-side CurveMaker 测试，覆盖 `current_exposure > desired_exposure` 时的 trigger price 和 reduce-only。
- [x] 补 partial fill 后后续 binding 继续覆盖剩余量的端到端测试。
- [x] 补 active binding drift budget 容差测试。
- [x] 删除 `record_submit_request` 空操作及生产调用点；如果发现它仍有语义，改名并补语义测试。
- [x] 清理测试里的 `Box::leak`，改成测试 fixture 持有 `LazyLock` 值。
- [x] 给当前 policy 优先级和 cancel-pending 时序加注释，作为 Task 2 重构前的行为说明。

**最小验收命令：**

- `cargo test -p poise-engine executor::ledger::tests:: -- --nocapture`
- `cargo test -p poise-engine executor::recording::tests:: -- --nocapture`
- `cargo test -p poise-engine executor::planning::tests:: -- --nocapture`

**Commit SHA：** `93914ea`

## Task 2：锁住统一 BoundaryPolicy 前的 Normal 行为

**目的：** 在改结构前，把 CatchUp / CurveMaker 当前交互行为固定成验收测试。

**文件：**

- `engine/src/executor/planning.rs`
- `engine/src/executor/policy.rs`

**步骤：**

- [x] 测试 due maker grace 后，同轮可以 cancel maker 并 submit aggressive replacement。
- [x] 测试上一轮遗留的 cancel-pending owner 会阻止同 operation 重复 submit。
- [x] 测试 active maker 在未过 grace 时仍可覆盖 future operation。
- [x] 测试 aggressive due binding 保持聚合多个 due operations。
- [x] 测试 passive maker 仍保持每 operation 一张单。

**最小验收命令：**

- `cargo test -p poise-engine executor::planning::tests:: -- --nocapture`
- `cargo test -p poise-engine executor::policy::tests:: -- --nocapture`

**Commit SHA：** `90ab247`

## Task 3：引入统一 BoundaryPolicy

**目的：** 让 Normal 模式的 due/passive/aggressive/grace/cancel-pending 决策由一个 policy 模块拥有。

**文件：**

- `engine/src/executor/policy.rs`
- `engine/src/executor/planning.rs`
- `engine/src/executor/binding.rs`

**步骤：**

- [x] 引入 `BoundaryPolicyInput` / `BoundaryPolicyOutput`，把 Normal 模式 operation selection 移入 `policy.rs`。
- [x] 将 due aggressive 聚合和 passive maker per-operation 两种执行形态保留为 policy 输出。
- [x] 将 maker grace 解释移动到统一 policy/reconciliation 内部。
- [x] 删除 `CoverageReservation` 对调用层的暴露。
- [x] 删除 `preexisting_cancel_pending_operations` 快照。
- [x] 删除 `effective_maker_owner_indexes` 和 `replaceable_active_owner_indexes`，或将等价逻辑内聚到新的 policy/reconciliation 私有函数中。
- [x] 保持 ManualOverride / Flatten 路径不变。

**最小验收命令：**

- `cargo test -p poise-engine executor::planning::tests:: -- --nocapture`
- `cargo test -p poise-engine executor::policy::tests:: -- --nocapture`

**Commit SHA：** `af71238`

## Task 4：文档同步和最终验收

**目的：** 删除过时概念描述，避免后续按旧双 policy 协议继续开发。

**文件：**

- `docs/superpowers/specs/2026-04-22-curve-boundary-ledger-execution-design.md`
- `docs/superpowers/specs/2026-04-26-boundary-policy-unification-design.md`
- `docs/superpowers/plans/2026-04-26-boundary-policy-unification.md`

**步骤：**

- [x] 更新旧 boundary ledger spec 中关于 `CoverageReservation` / 串行 policy runner 的描述。
- [x] 确认新 spec 与实现命名一致。
- [x] 回写 Task 1-3 的 commit SHA。
- [x] 运行必要的最终验收测试。

**建议最终验收命令：**

- `cargo test -p poise-engine executor::planning::tests:: -- --nocapture`
- `cargo test -p poise-engine executor::policy::tests:: -- --nocapture`
- `cargo test -p poise-engine executor::recording::tests:: -- --nocapture`
- `cargo test -p poise-engine executor::ledger::tests:: -- --nocapture`

**Commit SHA：** `5f07fce`
