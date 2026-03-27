# 网格运行态知识继续下推到 `engine` 设计

## 1. 背景

第一阶段已经把写侧提交边界固定下来：

- `snapshot + events + effects` 原子提交
- effect 执行改成持久化 outbox
- `pending_order=Submitting` 成为 submit effect 的恢复锚点

第二阶段又把对外协议和读模型边界固定下来：

- HTTP / WebSocket / TUI 读取 projector 输出
- 内部 snapshot 和 effect 细节不再直接暴露给协议层

这两步做完以后，当前最明显还留在 `server` 的运行时知识有三类：

1. `server/runtime` 仍然决定订单/仓位观察后要不要补一次 `Reconcile`
2. 启动同步时，`server/runtime` 仍然自己拼“清旧挂单 -> 应用 live position -> 应用 live open orders”这套状态合并顺序
3. `GridManager` 仍然在 runtime 外层单独维护 `budgets: HashMap<GridId, CapacityBudget>`

这些问题不会立刻打碎对外 contract，但会继续放大：

- `change amplification`：只要改观察语义、重算时机或预算模型，就要同时改 `engine`、`server/runtime`、测试夹具
- `cognitive load`：读代码的人要同时记住 observation 语义、启动同步顺序、恢复锚点例外和预算外置约定
- `unknown unknowns`：很难判断一次运行态修改会不会影响 startup sync、user data replay、effect 恢复或协议投影

现在 projector 已经提供了稳定缓冲层，正适合把这些内部知识继续收回 `engine`。

## 2. 设计目标

### 2.1 主目标

- `engine` 拿回“观察后是否立即重算”的决策权
- `engine` 拿回“启动时如何吸收交易所 live state”的状态合并规则
- 预算知识从 `GridManager` 外层 map 收回 `GridRuntime`
- `server/runtime` 退化成外部事件翻译层，不再持有运行态时序规则

### 2.2 非目标

- 这次不改 effect outbox 调度模型
- 这次不把 submit effect 的交易所恢复语义整个搬回 `engine`
- 这次不改 HTTP / WebSocket / TUI 对外 contract
- 这次不实现 `terminate` / `flatten`
- 这次不做多交易所 adapter registry

## 3. 当前残留问题

### 3.1 用户流后的重算语义还在 `server/runtime`

当前 `server/src/runtime.rs` 仍然维护：

- `should_reconcile_after_user_data()`
- `command_reconcile()`
- “PositionUpdate 总是触发重算”
- “OrderUpdate 只有 `Canceled / Rejected / Expired` 触发重算”

这说明 observation 本身还不是完整业务语义，`engine.observe()` 只负责更新局部状态，是否继续推进重算仍靠外层补一个命令。

### 3.2 启动同步的状态合并顺序还在 `server/runtime`

启动时当前链路是：

1. `server/runtime` 拉 live position
2. `server/runtime` 拉 live open orders
3. 根据 outbox 恢复锚点决定要不要先清 `pending_order`
4. 再分别调用 `observe_position()` 和 `observe_order()`

这里真正属于 `engine` 的知识是：

- live position 如何覆盖观察仓位
- live open orders 如何重建 `pending_order`
- 没有 live open orders 时何时清旧挂单

只有“是否保留 submit 恢复锚点”这一项与 outbox 持久化状态有关，不适合直接收进 `engine`。

### 3.3 预算还挂在 `GridManager` 外层

当前 `GridManager` 自己维护：

```rust
pub struct GridManager {
    grids: HashMap<GridId, GridRuntime>,
    budgets: HashMap<GridId, CapacityBudget>,
    instruments: HashMap<Instrument, GridId>,
    clock: Arc<dyn ClockPort>,
}
```

这让预算成为“manager 的额外并行状态”，而不是 runtime 的一部分。对 `reconciler` 来说，`GridRuntime` 还不是自足的运行态对象。

## 4. 核心设计决策

### 4.1 把预算收回 `GridRuntime`

`GridRuntime` 增加：

```rust
pub struct GridRuntime {
    pub id: GridId,
    pub instrument: Instrument,
    pub config: GridConfig,
    pub budget: CapacityBudget,
    pub exchange_rules: ExchangeRules,
    ...
}
```

对应调整：

- `GridRuntime::new()` 接收 `budget`
- `GridManager` 删除 `budgets: HashMap<GridId, CapacityBudget>`
- `reconciler::reconcile()` 改成只接收 `&GridRuntime`

这样 `engine` 内部会更一致：

- 配置、预算、观察状态、挂单状态、风控运行态都挂在同一个聚合上
- `GridManager` 不再维护第二份与 runtime 强耦合的并行 map

### 4.2 startup sync 改成专用入口 `sync_exchange_state()`

steady-state observation 继续只保留：

```rust
pub enum GridObservation {
    Market(MarketObservation),
    Position(PositionObservation),
    Order(OrderObservation),
}
```

startup / recovery 不再挂在通用 `observe()` 上，而是新增专用入口：

```rust
pub fn sync_exchange_state(
    &mut self,
    id: &GridId,
    position: PositionObservation,
    open_orders: Vec<OrderObservation>,
    submit_recovery_anchor: Option<SubmitRecoveryAnchor>,
) -> Result<GridTransition>
```

语义约束：

- `sync_exchange_state()` 只吸收 startup 时刻的交易所 live facts
- 它不表达 steady-state 事件，也不参与用户流 replay
- `submit_recovery_anchor` 只承载 effect 恢复层确认过的恢复锚点
- 只有 snapshot 里存在 `Submitting` 恢复锚点，且 outbox 里仍存在匹配的 pending `SubmitOrder` effect，才允许传入这个锚点

### 4.3 `engine` 拿回启动同步的状态合并规则

`GridManager.sync_exchange_state()` 的处理顺序固定为：

1. 更新观察仓位和未实现盈亏
2. 如果没有匹配的 `submit_recovery_anchor`，先清当前 `pending_order`
3. 按确定性顺序检查和重放 `open_orders`
   - 如果存在多于一笔 live open order，直接返回错误
   - 否则由 `engine` 自己排序并重建 `pending_order`
4. 不自动触发 `Reconcile`

这样划分的原因：

- startup sync 的职责是“把 live exchange facts 吸收到 runtime”
- 它不是一条新的控制命令
- effect 恢复是否继续执行，仍由 outbox / effect worker 负责

`server/runtime` 只需要把交易所数据翻译成 `PositionObservation + Vec<OrderObservation>`，不再自己写状态合并逻辑。

### 4.4 `engine` 拿回 observation-driven reconcile 语义

`GridManager.observe()` 改成对不同 observation 直接决定是否继续重算：

- `MarketObservation`
  - 与现在一致：更新参考价并执行 `reconcile`
- `PositionObservation`
  - 更新 `current_exposure` 和 `risk.unrealized_pnl`
  - 如果已有参考价，则直接执行 `reconcile`
- `OrderObservation`
  - 更新 `pending_order` / `risk.realized_pnl_today`
  - 如果状态是 `Canceled / Rejected / Expired` 且已有参考价，则直接执行 `reconcile`
  - `Filled / PartiallyFilled` 不立即重算，仍然等待随后仓位更新提供真实 exposure
- `sync_exchange_state()`
  - 只吸收 live state，不自动触发 `reconcile`

另外保留一个边界约束：

- `reconciler::reconcile()` 保持通用规划逻辑，不再理解 submit recovery 的瞬时恢复态
- submit recovery 期的“继续吸收 target / price 变化，但暂不发第二次计划”由 `GridManager` 在拿到 `reconcile()` 结果后统一 suppress effect

结果：

- `server/runtime` 删除 `should_reconcile_after_user_data()`
- `server/runtime` 删除 `command_reconcile()` 辅助路径
- 用户流 replay 和 live user task 都只做 observation 翻译

### 4.5 `GridCommand::Reconcile` 继续保留

这次不删除 `GridCommand::Reconcile`。

保留原因：

- 启动回放之外，系统仍然需要显式“基于当前参考价重算一次”的控制入口
- 后续 `terminate` / `flatten`、手动修复、诊断工具都可能复用这条命令

但它不再是 user data 正常链路里的补丁动作，而是显式控制命令。

## 5. 模块边界调整

### 5.1 `grid-engine`

新增拥有：

- observation-driven reconcile 规则
- startup exchange state merge 规则
- 预算归属

仍然不拥有：

- effect outbox 查询
- HTTP / WebSocket DTO
- 交易所 I/O

### 5.2 `grid-server`

`server/runtime.rs` 只保留：

- 订阅市场流和用户流
- 把 `ExchangeOrder` / `Position` / startup live state 翻译成 observation 参数
- 调用 `GridWriteService`

不再拥有：

- 观察后要不要重算的业务判断
- startup sync 的状态合并顺序
- 预算外置 map 的读取路径

### 5.3 `server/write_service.rs`

新增一个写侧入口：

```rust
pub async fn sync_exchange_state(
    &self,
    id: &str,
    position: PositionObservation,
    open_orders: Vec<OrderObservation>,
    submit_recovery_anchor: Option<SubmitRecoveryAnchor>,
) -> Result<GridTransition>
```

这样 startup sync 仍走同一条写侧提交边界，但不再污染通用 observation 模型。

### 5.4 `server/effect_service.rs`

新增独立 effect/outbox 服务，负责：

- `list_pending_effects`
- `load_grid_state`
- `complete_effect_succeeded() / complete_effect_failed()`
- `submit_recovery_anchor()`

`GridWriteService` 只保留 grid mutation + transition 提交边界。

## 6. 数据流

### 6.1 启动同步

1. `server/runtime` 获取 live position / live open orders
2. `effect_service` 基于 snapshot 恢复锚点和 pending `SubmitOrder` effect，产出 `submit_recovery_anchor`
3. `write_service.sync_exchange_state()` 调用 `engine.sync_exchange_state()`
5. 写侧原子保存新快照和 transition 产物

### 6.2 实时用户流

1. 交易所适配器产出 `UserDataEvent`
2. `server/runtime` 按 `Instrument` 找到 `GridId`
3. `server/runtime` 把事件翻译成 `GridObservation::Position` 或 `GridObservation::Order`
4. `write_service.observe_*()` 调用 `engine.observe()`
5. `engine` 自己决定是否追加 `reconcile` 产物

### 6.3 effect 执行与恢复

本阶段改成：

- `EffectWorker` 继续负责交易所 I/O
- effect/outbox 仓储访问统一经 `effect_service`
- `PendingOrder` 构建统一收敛到 `engine/runtime`
- 已经过时的 submit effect 会在执行前按当前 runtime 计划失效，不再继续发单
- 这类 effect 以 `Superseded` 终态结束，不再伪装成 `Succeeded`
- 旧 submit effect 一旦被判定为 `Superseded`，会立刻按当前 runtime 状态补出替代计划，不再等待下一次外部 observation
- submit 恢复语义按 live state 决策：
  - live open order 仍在：恢复 pending 并完成 effect
  - 只有 receipt-backed 恢复证据仍在时，live open order 不在但 `current_exposure` 已达 target：完成 effect，不重提
  - receipt 已落 snapshot 但 live state 还没对齐：保持 effect pending，等待交易所事实而不是重提
  - 如果已经没有 receipt-backed 恢复证据，即便 `current_exposure` 已达 target，也按 `Superseded` 结束，不伪装成成功提交
  - submit 被交易所拒绝且本地清理 `Submitting` 失败：保持 effect pending，避免留下非重启不可恢复的孤儿锚点

这里的“按当前 runtime 计划失效”有两个约束：

- 不再通过 `request.quantity / base_qty_per_unit()` 反推旧 exposure
- 必须复用当前 `GridRuntime + ExchangeRules` 生成的计划语义，避免 rounding / step 造成误判
- startup sync 保留的不只是 `Submitting` 锚点，也包括与 pending submit effect 对齐的 receipt-backed 恢复证据

## 7. 实现顺序建议

按风险从低到高拆成三段：

1. 先把预算收回 `GridRuntime`
2. 再把 startup sync 改成专用 `sync_exchange_state()` 入口
3. 再拆出 `effect_service` 并修正 submit 恢复语义
4. 最后把 observation-driven reconcile 规则收进 `engine`

原因：

- 预算归属调整对外部行为影响最小
- startup sync 有独立测试面，适合单独锁定
- user data 重算语义最容易引入行为回归，应该最后处理

## 8. 测试策略

### 8.1 `grid-engine`

新增并先写失败测试，覆盖：

- `GridRuntime` 自带预算时，`reconcile` 仍然正确裁剪目标
- `sync_exchange_state()` 在没有匹配 `submit_recovery_anchor` 时清旧挂单
- `sync_exchange_state()` 在存在匹配 `submit_recovery_anchor` 时保留 submit 恢复锚点
- 只有“锚点 + 匹配 pending submit effect”同时存在时才允许保留；孤儿 `Submitting` 不得跨启动继续存在
- submit recovery 期 target 变化时，`reconciler` 仍能产出新计划，但 `GridManager` 会 suppress effect，避免重复发单
- `sync_exchange_state()` 在同一 grid 出现多于一笔 live open order 时直接报错
- `PendingOrder` builder 保持 submit request / receipt / live order / order observation 现有形状不回归
- `PositionObservation` 在已有参考价时直接产出新的 `SubmitOrder` / `CancelAll`
- `OrderObservation::Canceled/Rejected/Expired` 在已有参考价时直接产出重算 effect
- `OrderObservation::Filled/PartiallyFilled` 不提前重算

### 8.2 `grid-server`

新增并先写失败测试，覆盖：

- startup sync 通过 `sync_exchange_state()` 更新状态，不再手工调用 `observe_position()` + `observe_order()`
- user data update 不再额外调用 `command_reconcile()`
- `position_update_reconciles_without_runtime_follow_up_command` 证明 position user-data 只走一次业务写路径
- receipt 已落 snapshot 的恢复分支按 live state 正确选择“恢复 / 完成 / 重试”
- 已失效 submit effect 以 `Superseded` 终态进入投影，并允许同批后续 effect 继续解锁

### 8.3 回归重点

- `effect_worker_retries_submit_when_receipt_snapshot_has_no_live_order_and_target_not_reached`
- `effect_worker_completes_submit_without_retry_when_receipt_snapshot_has_no_live_order_and_target_reached`
- `position_update_reconciles_without_runtime_follow_up_command`
- `position_update_submits_reconcile_without_waiting_for_new_tick`
- `terminal_order_update_reconciles_without_waiting_for_new_tick`

这些测试要么保留，要么按新设计改写，但不能丢。

## 9. 验收标准

完成后应满足：

- `GridRuntime` 自身拥有预算，不再依赖 `GridManager` 外置 budget map
- `server/runtime` 不再保留 `should_reconcile_after_user_data()` 这类业务判断
- startup sync 通过单一 observation 把 live exchange state 吸收到 `engine`
- 单 `pending_order` 模型下，startup sync 遇到同一 grid 的多笔 live open order 会直接报错
- position / order observation 的重算语义由 `engine.observe()` 决定
- submit recovery 防重仍然存在，但 owner 变成 `GridManager`，不再写进通用 `reconciler`
- 当前 projector / HTTP / WS / TUI contract 不变
- 全量测试通过，并新增覆盖 startup sync 与 observation-driven reconcile 的验收测试
