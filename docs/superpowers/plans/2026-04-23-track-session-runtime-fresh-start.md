# Track Session Runtime Fresh-Start Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把重启后的启动语义改成真正的 fresh-session：旧会话本地执行状态全部作废，只保留 definition 和持久业务状态，再基于交易所真值与当前市场数据重建新的 session runtime。

**Architecture:** 这次实现分成四层推进。先拆 `TrackDefinition / TrackControlState / TrackLedgerState` 的 owner 和直接消费者，再把 fresh-session 构造规则收进现有 `TrackRuntime::fresh_start(...)`，然后在同一任务里同时完成 startup 三阶段和旧会话 work 作废，最后做 focused 验收。整个过程不做旧 session 兼容恢复，不再从 live order 反推本地 binding。

**Tech Stack:** Rust workspace, Cargo, Serde, chrono, Markdown

---

## 设计约束

- 重启后不延续任何旧会话本地执行工作：
  - `Pending` effect
  - `Executing` effect
  - `follow_up_retirements`
  - bindings
  - recovery anomaly
  - boundary progress
- inherited orders 只属于 cleanup 阶段，不参与新 session runtime 语义构建。
- 新 session runtime 只能由以下输入构造：
  - `TrackDefinition`
  - `TrackControlState`
  - `TrackLedgerState`
  - `FreshSessionExternalInputs`
- `FreshSessionExternalInputs` 第一阶段包含：
  - 当前真实仓位
  - 当前有效市场数据
  - 当前标的 `ExchangeRules`
- `TrackDefinition` 不再使用 `CapacityBudget`。定义层改成：
  - `config: TrackConfig`
  - `max_notional`
  - `loss_limits: LossLimits`
- `max_notional` 与曲线天然上限共同决定：
  - `effective_max_notional = min(curve_max_notional, max_notional)`
- startup cleanup 规则只能存在于 startup phase 内部，steady-state user task 不得持有 cleanup filter。
- startup replay 与 steady-state 交接不能有事件空窗。
- `TrackLedgerState` 的日边界固定按 UTC 解释，不允许由 startup、risk guard 或 projector 各自定义“today”。
- startup 不允许从旧 `TrackState`、旧 runtime snapshot 或旧 session transient state 推导 `TrackControlState`。

## 相关文件与责任

### 定义与持久业务状态

- Modify: `application/src/track_definition.rs`
  - 去掉 `CapacityBudget`
  - 引入 `LossLimits`
  - 明确 `TrackDefinition` 的定义层边界
- Create: `application/src/track_control_state.rs`
  - owner `TrackControlState`
  - owner `PersistedControlMode`
  - owner 产品控制命令如何写入持久控制状态
- Modify: `engine/src/ledger.rs`
  - owner `TrackLedgerState`
  - owner `ledger_utc_day` 的 rollover 规则
  - owner gross / fee / funding / unresolved gap 的记账语义
- Modify: `server/src/config.rs`
  - 保持配置文件扁平
  - 但加载后映射到新的定义结构
- Modify: `core/src/risk.rs`
  - 拆掉 `CapacityBudget`
  - 让风险评估直接消费 `LossLimits`、显式 `max_notional` 和 `TrackLedgerState` 的派生 net realized pnl
- Modify: `engine/src/track.rs`
  - 同步新的 `TrackDefinition` 结构
- Modify: `engine/src/persisted_runtime.rs`
  - 不再作为 fresh-session 恢复输入
  - 删除或降级旧 `PersistedRuntimeCodec` 的启动恢复职责
- Modify: `engine/src/snapshot.rs`
  - 不再把 snapshot 作为跨重启执行语义边界
- Modify: `application/src/runtime_lifecycle_service.rs`
  - 从持久状态 owner 读取 `TrackControlState / TrackLedgerState`
  - 在 fresh-session 前先请求 UTC 日边界标准化后的 `TrackLedgerState`
  - 组装 `FreshSessionExternalInputs`，包括当前仓位、当前有效市场数据和 `ExchangeRules`
- Modify: `storage/src/sqlite.rs`
  - 提供 `TrackControlState / TrackLedgerState` 的持久化读写入口
- Modify: `storage/src/schema.rs`
  - 新增或重写 track 级持久真值 schema
  - 停止把旧 `track_snapshots` 作为 startup 输入
- Modify: `application/src/read_model.rs`
  - 直接消费 `TrackLedgerState` 的 gross / fee / funding / gap 真值
- Modify: `server/src/projector.rs`
  - 把 `TrackLedgerState` 投影到对外 ledger 视图
- Modify: `protocol/src/lib.rs`
  - 对外不再暴露混合的 `budget`
  - 显式暴露 `max_notional` 与 `loss_limits`
- Modify: `tui/src/protocol.rs`
- Modify: `tui/src/api_client.rs`
- Modify: `tui/src/views/instance.rs`
  - 同步新的定义层展示

### 会话 runtime 构造

- Modify: `engine/src/runtime.rs`
  - 新增 `TrackRuntime::fresh_start(...)`
  - 定义 `FreshSessionExternalInputs` 输入值对象
- Modify: `engine/src/manager.rs`
  - 去掉“在旧 runtime 上局部 reset 后继续运行”的入口
  - 改为显式接受 fresh-session 初始 runtime
- Modify: `application/src/runtime_lifecycle_service.rs`
  - 只保留 startup 期需要的生命周期动作

### startup 三阶段与旧会话 work 清理

- Modify: `server/src/runtime/startup_bootstrap.rs`
  - 用私有函数表达 `InheritedOrderCleanup` 阶段
  - 引入 startup 私有 `CleanupTracker`
  - 用私有函数表达 `SteadyStateHandoff` 阶段
  - 只在最终 cutoff drain 后 `CleanupTracker` 仍 quiesced 时允许 handoff
- Modify: `server/src/runtime/user_data.rs`
  - steady-state 只接受交接边界后的事件
  - 不再保留 startup cleanup 特殊规则
- Modify: `server/src/runtime/mod.rs`
  - 用新的 startup 阶段结果启动 steady-state task
- Modify: `application/src/mutation_executor.rs`
  - fresh-session 清理 `Pending + Executing`
  - 清空 `follow_up_retirements`
- Modify: `application/src/track_effect_store.rs`
- Modify: `storage/src/sqlite.rs`
  - 新增按 track 查询并 supersede 可作废的旧会话 effect

### 文档

- Modify: `docs/superpowers/specs/2026-04-22-curve-boundary-ledger-execution-design.md`
  - 如果最终实现影响执行器内核边界，回写 cross-reference
- Modify: `docs/superpowers/specs/2026-04-23-track-session-runtime-fresh-start-design.md`
- Modify: `docs/superpowers/plans/2026-04-23-track-session-runtime-fresh-start.md`

## 非目标

- 不恢复旧 session 的 binding / boundary progress
- 不让交易所 live order 成为本地 binding 的恢复真值
- 不在本轮引入新的多账户预算系统

## Task 1: 拆分 definition、持久控制状态、持久账本真值与会话执行态

**Files:**

- Modify: `application/src/track_definition.rs`
- Create: `application/src/track_control_state.rs`
- Modify: `server/src/config.rs`
- Modify: `engine/src/ledger.rs`
- Modify: `core/src/risk.rs`
- Modify: `engine/src/track.rs`
- Modify: `engine/src/persisted_runtime.rs`
- Modify: `engine/src/snapshot.rs`
- Modify: `application/src/runtime_lifecycle_service.rs`
- Modify: `storage/src/sqlite.rs`
- Modify: `storage/src/schema.rs`
- Modify: `application/src/read_model.rs`
- Modify: `server/src/projector.rs`
- Modify: `protocol/src/lib.rs`
- Modify: `tui/src/protocol.rs`
- Modify: `tui/src/api_client.rs`
- Modify: `tui/src/views/instance.rs`
- Test: `application/src/track_definition.rs`
- Test: `application/src/track_control_state.rs`
- Test: `engine/src/ledger.rs`
- Test: `server/src/config.rs`
- Test: `core/src/risk.rs`
- Test: `protocol/src/lib.rs`

- [x] **Step 1: 先写失败测试，锁住新的定义边界**

覆盖点：

- `TrackDefinition` 不再暴露 `CapacityBudget`
- `max_notional` 与 `LossLimits` 分开
- `TrackControlState` 是封闭集合，只允许 `Enabled / Paused / Terminated`
- `TrackControlState` 只由产品控制命令或持久业务事件写入
- `WaitingMarketData / Frozen / FlattenPending / Flattening` 不允许跨重启持久化
- `TrackControlState / TrackLedgerState` 不再从旧 runtime snapshot 间接取得
- startup 不从旧 `TrackState` 映射 `TrackControlState`
- `TrackLedgerState` 明确带有 `ledger_utc_day`
- `TrackLedgerState` 明确保存 gross / fee / funding / unresolved gap 真值
- UTC 跨日 rollover 只有单一 owner
- `net_realized_pnl_*` 只作为派生值，不单独持久化
- `unrealized_pnl` 不进入 `TrackLedgerState`
- 配置文件仍保持扁平输入
- `curve_max_notional` 与 `effective_max_notional` 的派生语义明确
- `ExchangeRules` 不从旧 runtime 恢复，而是作为 `FreshSessionExternalInputs` 显式传入
- public protocol 不再暴露混合的 `budget`

- [x] **Step 2: 运行定向测试，确认旧结构失败**

Run:

- `cargo test -p poise-application track_definition::tests:: -- --nocapture`
- `cargo test -p poise-application track_control_state::tests:: -- --nocapture`
- `cargo test -p poise-engine ledger::tests:: -- --nocapture`
- `cargo test -p poise-server config::tests:: -- --nocapture`
- `cargo test -p poise-core risk::tests:: -- --nocapture`
- `cargo test -p poise-server projector::tests:: -- --nocapture`
- `cargo test -p poise-protocol -- --nocapture`

Expected:

- 现有实现仍依赖 `CapacityBudget`，`TrackControlState / TrackLedgerState` 还没有独立 owner，并且旧 runtime snapshot 仍是启动恢复边界，新测试失败。

- [x] **Step 3: 做最小实现，完成定义层拆分**

要求：

- 引入 `LossLimits`
- `TrackDefinition` 改成 `config + max_notional + loss_limits`
- 引入 `TrackControlState` owner，封闭表达 `Enabled / Paused / Terminated`
- 明确产品控制命令到 `TrackControlState` 的写入规则，并丢弃会话瞬时状态
- 如果存在一次性旧数据迁移，只允许在迁移脚本或迁移测试里定义旧状态转换，不允许 startup runtime 调用
- 明确 `TrackLedgerState` 的 `ledger_utc_day` 与 UTC rollover 入口
- 风险模块直接消费 `LossLimits` 与 `TrackLedgerState` 的派生 net realized pnl
- 明确 `effective_max_notional` 的派生入口
- runtime lifecycle 直接消费持久状态 owner，而不是从旧 snapshot 取值
- fresh-session bootstrap 只接收已经按 UTC 标准化后的 `TrackLedgerState`
- projector / read model 直接消费 `TrackLedgerState`，不再临时拼装另一套账本真值
- `PersistedRuntimeCodec / TrackRuntimeSnapshot / track_snapshots` 不再作为 startup 输入
- protocol / TUI 同步 `max_notional + loss_limits`，不保留 `budget` 作为公开边界

- [x] **Step 4: 运行 Task 1 回归**

Run:

- `cargo test -p poise-application track_definition::tests:: -- --nocapture`
- `cargo test -p poise-application track_control_state::tests:: -- --nocapture`
- `cargo test -p poise-engine ledger::tests:: -- --nocapture`
- `cargo test -p poise-server config::tests:: -- --nocapture`
- `cargo test -p poise-core risk::tests:: -- --nocapture`
- `cargo test -p poise-protocol -- --nocapture`
- `cargo test -p poise-server projector::tests:: -- --nocapture`

Expected:

- 定义层、持久控制状态、持久账本真值和公开协议边界完成，旧 `CapacityBudget` 与旧 runtime snapshot 不再作为共享边界。
- Task 1 implementation commit: `cd6c874`

## Task 2: 定义 `TrackRuntime::fresh_start(...)`

**Files:**

- Modify: `engine/src/runtime.rs`
- Modify: `engine/src/manager.rs`
- Modify: `application/src/runtime_lifecycle_service.rs`
- Test: `engine/src/runtime.rs`
- Test: `engine/src/manager.rs`

- [ ] **Step 1: 先写失败测试，锁住 fresh-session 初始状态**

覆盖点：

- fresh-session 不继承旧 bindings、旧 boundary progress、旧 anomaly
- ledger anchor 取当前真实仓位
- 无有效市场数据时保持 `WaitingMarketData`
- 不沿用旧 `desired_exposure` 与旧 `strategy_price`
- `TrackRuntime::fresh_start(...)` 必须显式接收 `FreshSessionExternalInputs`
- `ExchangeRules` 只能来自 `FreshSessionExternalInputs`，不能从旧 runtime 或旧 snapshot 读取

- [ ] **Step 2: 运行定向测试，确认当前 reset 语义失败**

Run:

- `cargo test -p poise-engine runtime::tests:: -- --nocapture`
- `cargo test -p poise-engine manager::tests:: -- --nocapture`

Expected:

- 当前实现仍是“在旧 runtime 上 reset 一部分字段”，新测试失败。

- [ ] **Step 3: 做最小实现，建立 `TrackRuntime::fresh_start(...)`**

要求：

- 新 session runtime 的构造规则由现有 `TrackRuntime` 自己拥有
- 引入 `FreshSessionExternalInputs`，包含当前仓位、当前有效市场数据和当前标的 `ExchangeRules`
- 不新增 factory / bootstrapper 类型，也不新增 `TrackSessionRuntime` 包装类型；startup 和 manager 只能调用 `TrackRuntime::fresh_start(...)`
- 旧 runtime snapshot 不再作为 startup 执行语义恢复输入
- manager 不再负责猜测需要保留哪些旧 session 字段

- [ ] **Step 4: 运行 Task 2 回归**

Run:

- `cargo test -p poise-engine runtime::tests:: -- --nocapture`
- `cargo test -p poise-engine manager::tests:: -- --nocapture`

Expected:

- fresh-session 的初始 runtime 由 `TrackRuntime::fresh_start(...)` 根据定义、持久状态和外部真值构造。

## Task 3: 重写 startup 三阶段，并同时让旧会话 work 全部失效

**Files:**

- Modify: `server/src/runtime/startup_bootstrap.rs`
- Modify: `server/src/runtime/user_data.rs`
- Modify: `server/src/runtime/mod.rs`
- Modify: `application/src/mutation_executor.rs`
- Modify: `application/src/track_effect_store.rs`
- Modify: `storage/src/sqlite.rs`
- Test: `server/src/runtime/startup_bootstrap.rs`
- Test: `application/src/runtime_lifecycle_service.rs`
- Test: `storage/src/sqlite.rs`

- [ ] **Step 1: 先写失败测试，锁住 startup 三阶段边界**

覆盖点：

- inherited orders 只参与 cleanup
- handoff 前 `CleanupTracker` 必须 quiesced
- startup phase 独占 user-data receiver，handoff 前完成 cleanup quiesce、最终外部真值查询、session 构建和 cutoff drain
- 最终外部真值查询必须包含当前仓位、当前标的 `ExchangeRules` 和当前有效市场数据
- 若 cutoff drain 后 cleanup 状态变化，必须回到 cleanup barrier 重新建立 handoff
- startup replay 与 steady-state handoff 没有丢事件窗口
- cleanup filter 不会泄漏到 steady-state user task
- steady-state 只处理交接边界之后的事件
- fresh-session 会同时作废 `Pending + Executing + follow_up_retirements`
- 旧会话 effect 不会再阻塞新会话批次

- [ ] **Step 2: 运行定向测试，确认当前实现失败**

Run:

- `cargo test -p poise-server runtime::startup_bootstrap::tests:: -- --nocapture`
- `cargo test -p poise-application runtime_lifecycle_service::tests:: -- --nocapture`
- `cargo test -p poise-storage sqlite::tests:: -- --nocapture`

Expected:

- 当前实现的 replay 与最终 cutoff 交接仍有空窗，且旧会话 work 还没有和 startup 边界一起作废，新测试失败。

- [ ] **Step 3: 做最小实现，建立显式 startup 阶段**

要求：

- `InheritedOrderCleanup` 是 `startup_bootstrap.rs` 内部流程边界，优先实现为私有函数，不要求新增 public type
- `FreshSessionBootstrap` 是调用 `TrackRuntime::fresh_start(...)` 的流程步骤，不要求新增 type
- `SteadyStateHandoff` 是 `startup_bootstrap.rs` 内部流程边界，优先实现为私有函数，不要求新增 public type
- `CleanupTracker` 是 startup 私有状态对象，用来持有 cleanup identity、终态解析和 quiesce 判定

三阶段边界和旧会话 work 作废必须在同一 task 里一起落地，不允许先切 startup 语义、后补旧会话清理。

唯一允许的 startup 时序是：

1. startup 独占 user-data receiver
2. cleanup 当前标的 inherited orders
3. 消费 user-data 直到 `CleanupTracker.quiesced`
4. 查询最终外部真值
5. 调用 `TrackRuntime::fresh_start(...)` 构建 fresh session runtime
6. 取得 steady-state cutoff
7. drain / replay `event_time <= cutoff`
8. 若 drain 后 cleanup 状态变化，回到第 3 步
9. 若 drain 后仍 quiesced，把 receiver 移交 steady-state

- [ ] **Step 4: 运行 Task 3 回归**

Run:

- `cargo test -p poise-server runtime::startup_bootstrap::tests:: -- --nocapture`
- `cargo test -p poise-application runtime_lifecycle_service::tests:: -- --nocapture`
- `cargo test -p poise-storage sqlite::tests:: -- --nocapture`

Expected:

- startup 清理、fresh-session 构建、cleanup quiesce 与 steady-state handoff 边界清楚，且旧会话本地 work 全部失效。

## Task 4: 统一回写文档与 focused 验收

**Files:**

- Modify: `docs/superpowers/specs/2026-04-23-track-session-runtime-fresh-start-design.md`
- Modify: `docs/superpowers/plans/2026-04-23-track-session-runtime-fresh-start.md`
- Modify: `docs/superpowers/specs/2026-04-22-curve-boundary-ledger-execution-design.md`

- [ ] **Step 1: 复核 spec 与实现是否仍一致**

检查：

- `TrackDefinition` 的形状
- fresh-session 语义
- startup 三阶段
- inherited order cleanup 与 session bootstrap 的 owner 分离

- [ ] **Step 2: 运行 focused 验收**

Run:

- `cargo test -p poise-core risk::tests:: -- --nocapture`
- `cargo test -p poise-engine runtime::tests:: -- --nocapture`
- `cargo test -p poise-engine manager::tests:: -- --nocapture`
- `cargo test -p poise-application runtime_lifecycle_service::tests:: -- --nocapture`
- `cargo test -p poise-protocol -- --nocapture`
- `cargo test -p poise-server projector::tests:: -- --nocapture`
- `cargo test -p poise-server runtime::startup_bootstrap::tests:: -- --nocapture`
- `cargo test -p poise-storage sqlite::tests:: -- --nocapture`
- `git diff --check`

Expected:

- 新设计的不变量都被 focused 测试覆盖，文档与代码一致。

## 计划自检

### spec 覆盖

- 已覆盖定义层拆分：`TrackDefinition / max_notional / LossLimits`
- 已覆盖持久控制状态、持久账本真值与 session runtime 分离
- 已覆盖 startup 三阶段与 handoff 空窗问题
- 已覆盖旧会话 effect 与 follow-up retirement 的清理规则
- 已覆盖公开协议不再暴露混合 `budget`

### 占位词检查

- 本计划没有 `TODO / TBD / implement later`
- 所有任务都指向明确文件和定向验证入口

### 一致性检查

- `LossLimits` 作为定义层结构
- `max_notional` 保持平铺字段
- `TrackControlState` 是封闭集合，不复用旧 `TrackState`
- `TrackLedgerState` 是 track 级账本真值，不保存 `unrealized_pnl`
- track session runtime 对应现有 `TrackRuntime`，通过 `TrackRuntime::fresh_start(...)` 构造，不引入 factory / bootstrapper 或 `TrackSessionRuntime` 包装类型
- startup 采用显式三阶段，而不是 runtime 常驻特例

## 评审重点

建议评审时优先看这四点：

1. `TrackDefinition / TrackControlState / TrackLedgerState / TrackRuntime` 的边界是否清楚
2. inherited order cleanup 是否已经和 session bootstrap 分离
3. startup replay 与 steady-state handoff 是否没有空窗
4. `CapacityBudget` 被拆掉后，`max_notional` 与 `LossLimits` 的 owner 是否合理
