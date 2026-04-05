# Exchange State Freshness Gate 设计

## 背景

当前执行链路里，`engine` 会根据本地 `current_exposure`、`desired_exposure` 和最新 `reference_price` 规划调仓动作。

这套模型默认假设：

- 本地仓位视图足够新
- 订单事件、仓位事件、价格事件的到达顺序不会造成明显偏差

但实际运行时并不满足这个假设。交易所真实状态可能已经变化，而本地仍停留在旧状态。例如：

1. 订单已经在交易所成交，但本地还没收到 `Filled`
2. `Filled` 已到，但对应的 `PositionUpdate` 还没到
3. `Price` 先到，触发了新一轮 reconcile
4. reconcile 仍基于旧 `current_exposure` 计算 side effect

结果是：

- 会把已经成交的仓位再次算进调仓差值
- 可能多发一轮 `SubmitOrder` / `CancelOrder`
- 仓位会在一段时间内偏离目标
- 即使最终可能重新收敛，也会额外付出手续费、滑点和瞬时风险敞口

这不是单个事件类型处理不全的问题，而是“本地状态在某些窗口内不可信”。

## 目标

- 当本地交易所状态可能过时时，阻止基于旧状态直接产生新的执行 side effect
- 保留现有 `engine` 的职责边界，不把交易所同步职责推入 `engine`
- 把“何时必须先同步交易所状态”收敛成单点规则
- 让 `Price` 先到、`Filled` 先到、`CancelUnknown` 先到这些时序都走统一机制

## 非目标

- 不引入完整“双状态仓位模型”
- 不要求事件强一致或严格顺序处理
- 不重写 `engine` 的 round / planning 状态机
- 不修改交易所 adapter 协议
- 不消除所有瞬时偏差，只减少错误 side effect 的产生

## 当前问题

### 1. 执行安全性依赖事件到达顺序

当前是否继续调仓，隐含依赖以下条件同时成立：

- 本地 working order 状态已吸收
- 本地仓位已同步到成交后的值
- 价格事件在上述状态更新之后才到

这会把系统正确性建立在事件顺序上，而不是建立在显式的状态可信度判断上。

### 2. “为什么需要同步” 的知识正在扩散

如果继续沿事件分支补规则，后续很容易出现：

- `Filled` 要 sync
- `UnabsorbedOrderUpdate` 要 sync
- 某些 `Cancel` 结果要 sync
- 某些 `Price` 驱动也要 sync

这会把“状态何时不可信”的知识散到多个入口，形成 change amplification。

### 3. `engine` 本身并没有错

`engine` 的职责是：

- 给定当前状态，生成计划

这里的问题不在规划逻辑，而在于输入状态已经失真。因此修复应该落在 server 写侧边界，而不是继续把交易所时序知识灌进 `engine`。

## 备选方案

### 方案 A：继续补事件触发点

- 遇到 `Filled` 就立即 sync
- 以后再按需要补更多事件类型

优点：

- 改动最小
- 能快速止血

缺点：

- 规则按事件类型散开
- 无法覆盖 `Price` 先到但 `Filled` 未到的窗口
- 维护者需要记住多个分支里的特殊处理

### 方案 B：在产生 side effect 前做状态可信度门控

- 允许 `Price` 更新继续刷新目标和本地 snapshot
- 但当本次 reconcile 将要产生真实 side effect 时，先检查本地交易所状态是否可信
- 如果不可信，则先从交易所同步状态，再重跑 reconcile

优点：

- 不依赖具体事件顺序
- 把复杂度收敛到“执行前是否可信”这一条规则
- `engine` 边界保持不变

缺点：

- 比方案 A 多一个门控模块或门控步骤
- 需要补几组时序测试

### 方案 C：完整双状态模型

- 区分“本地观察状态”和“交易所确认状态”
- 所有执行仅基于确认态

优点：

- 语义最强

缺点：

- 改动明显过大
- 当前阶段投入产出比不高

## 结论

采用 **方案 B：在产生 side effect 前做状态可信度门控**。

设计要求是：

- 保留异常类即时 sync
- 去掉 `Filled` 这类普通时序事件的专用即时 sync
- 让“是否需要先同步”由统一门控决定，而不是由事件分支零散决定

## 设计

### 核心原则

系统不再问：

- “哪个事件先到了？”

而改为问：

- “当前本地交易所状态是否足够可信，可以直接发执行 side effect？”

只有当答案是“可以”时，才允许发出新的 `SubmitOrder` / `CancelOrder`。

### 模块边界

#### `engine`

职责保持不变：

- 根据给定状态生成执行计划

不负责：

- 判断交易所状态是否过时
- 决定何时调用交易所同步

#### `server/src/exchange_freshness.rs`

新增单点模块，显式拥有每个 `track` 的交易所状态新鲜度事实。

职责：

- 维护 `Fresh / Stale` 状态
- 记录为什么进入 `Stale`
- 维护每个 `track` 的 freshness generation
- 判断某个 effect 在当前 freshness 下是否必须先同步

它拥有的知识是：

- 哪些可观察事实会把状态置脏
- 哪些 effect 属于真实 side effect
- 什么情况下可以直接执行，什么情况下必须先 sync
- 一次 sync 最多只能清除哪一代 stale

它不负责：

- 访问交易所
- 直接写回 `TrackRuntime`
- 规划订单

#### `server/src/runtime.rs`

职责：

- 驱动 market / user / effect 等输入源
- 写入自己首先观察到的 freshness 事实
- 调用写侧执行入口

不再承担：

- 按不同事件类型零散解释“是否需要补一次交易所同步”

#### `server/src/effect_worker.rs`

职责：

- 在真实 submit / cancel 前读取 freshness 状态
- 在执行结果不确定时写入 freshness 事实
- 若 freshness 要求先同步，则发起 reconcile 并结束本轮执行

不再承担：

- 从 `executor_state` 或订单槽位反推 stale

## freshness 接口

建议把接口收敛成一个更深的模块，而不是只做一个无状态 helper。

例如：

- `mark_stale(track_id, reason)`
- `prepare_sync(track_id) -> ExchangeFreshnessSyncToken`
- `clear_if_current(token)`
- `is_stale(track_id) -> bool`
- `requires_sync_before_effect(track_id, effect) -> bool`

其中：

- `exchange_freshness` 是 freshness 语义的唯一 owner
- `runtime` 和 `effect_worker` 都可以调用 `mark_stale`
- 谁先观察到“本地状态可能落后于交易所真实状态”的事实，谁就写入该事实
- 任何发起 sync 的路径都先调用 `prepare_sync(...)`
- sync 成功后只调用 `clear_if_current(...)`
- `effect_worker` 继续负责调用 `requires_sync_before_effect(...)`

这样“stale 是什么、何时置脏、哪些 effect 要被拦住”都留在一个 owner 里。

`ExchangeFreshnessSyncToken` 是一个不透明 token。它捕获“这次 sync 开始前，调用方看到的是哪一代 freshness 状态”，但不把 revision 细节暴露给外层。

这样外层不需要理解并发细节，只需要遵守两条规则：

- sync 开始前拿 token
- sync 成功后按 token 条件清脏

## 命名边界

需要明确区分两类 reason：

- `ExchangeFreshnessReason`
  - 表达“为什么这个 track 现在处于 stale”
  - 只能使用可观察事实命名
  - 例如：`FilledAwaitingSync`、`UnabsorbedOrderUpdate`、`SubmitOutcomeUnknown`、`CancelOutcomeUnknown`

- `ReconcileReason`
  - 表达“为什么现在要入队一次 reconcile”
  - 可以使用调度意图命名
  - 第一版建议使用 `SyncBeforeSideEffect`

这两层不要求一一对应，也不应该复用同一个名字。

这样可以避免把“状态事实”和“调度动作”混成同一抽象。

## 门控规则

第一版只做最小必要规则。

### 1. 没有真实 side effect 时直接通过

如果计划结果只有：

- `NoOp`

则直接通过，不触发交易所同步。

因为此时即使本地状态略旧，也没有执行风险，只是 snapshot 可能暂时不够新。

### 2. 计划包含真实 side effect 时，检查 freshness 状态

如果计划包含：

- `SubmitOrder`
- `CancelOrder`

则需要进一步判断当前 `track` 是否处于 `Stale`。

### 3. `Stale` 的 owner 和置脏规则

第一版不再从 `executor_state` 反推 stale。

原因是：

- `Working` / `SubmitPending` 是正常执行生命周期，不应直接等价于“状态过时”
- 如果把活动订单直接当 stale，会把正常执行和异常恢复混在一起
- 这会迫使后续继续补例外规则，复杂度重新扩散

第一版建议只在明确存在“本地状态可能落后于交易所真实状态”的事实时置脏，例如：

- 已吸收 `Filled`，但尚未做一次交易所同步确认
- `UnabsorbedOrderUpdate`
- `SubmitOutcomeUnknown`
- `CancelOutcomeUnknown`

这里的关键是：

- `stale` 是一个显式事实
- 不是由 worker 或 gate 临时从 slot 状态猜出来的结论

### 4. 门控行为

当“计划包含真实 side effect”且 `track` 处于 `Stale` 时：

1. 先执行 `sync_exchange_state_from_exchange(...)`
2. 用同步后的状态重跑一次 reconcile / plan
3. 仅执行重跑后的 effects

如果同步失败或同步后仍处于异常状态：

- 返回 `NoOp`
- 等待下一次输入源重试

这样可以把“错误 side effect”设计成“不执行”。

### 5. `Fresh` 的恢复规则

第一版建议只在成功完成一次 `sync_exchange_state_from_exchange(...)` 后清除 stale。

不在普通 `PositionUpdate` 上直接清除，原因是：

- `PositionUpdate` 只能说明仓位视图变了
- 它不保证挂单视图也已经和交易所一致
- 如果在多个事件分支里各自决定何时清除 stale，知识会再次扩散

同时，清脏不能是无条件的 `clear(track_id)`。

原因是：

- stale 事实和交易所同步是异步发生的
- sync 执行过程中，可能又有新的 `Filled` 或 `OutcomeUnknown`
- 如果 sync 结束后无条件清脏，会把“sync 开始后才出现的新 stale”一并抹掉

因此第一版需要把清脏语义收敛成：

1. sync 开始前，调用 `prepare_sync(track_id)` 拿到 token
2. sync 成功并完成 writeback 后，调用 `clear_if_current(token)`
3. 只有当这次 sync 观察到的 freshness generation 仍然是当前代时，才真正恢复 `Fresh`

这意味着第一版会偏保守，但边界更清楚，也把“晚到 stale 被误清除”的竞态设计掉了。

## 触发来源的处理

### `Price`

- 仍然允许更新目标
- 但如果这次会落到真实 side effect，先走门控

这正是解决 “`Price` 先到但真实仓位已变” 的关键。

### `OrderUpdate`

- `Filled`：标记 `track` 为 stale，不立即 sync
- `UnabsorbedOrderUpdate`：标记 stale，并继续立即 sync

### `PositionUpdate`

- 正常更新本地仓位
- 更新后再走现有 reconcile 流程
- 第一版不直接清除 stale

## 为什么不直接做完整双状态模型

完整双状态模型会引入新的长期概念：

- 本地估计仓位
- 交易所确认仓位
- 两者的切换与展示语义

这当然更强，但当前问题的最小有效解并不需要这一层复杂度。

本次更重要的是：

- 不让不可信状态直接驱动 side effect

只要这点成立，就能明显降低错误调仓。

## 测试策略

验收测试至少覆盖以下时序：

### 1. `Price` 先到，真实成交已发生，但本地 `Filled` 还没到

- 旧行为：直接按旧仓位继续发单
- 新行为：先同步交易所状态，再重跑计划，不应重复加仓或减仓

### 2. `Filled` 已到，但 `PositionUpdate` 还没到

- 新行为：立即或通过门控完成一次同步
- 本地 snapshot 应更新到交易所真实仓位

### 3. `UnabsorbedOrderUpdate`

- 继续立即同步
- 不改变现有恢复路径

### 4. 状态可能过时，但本次计划只有 `NoOp`

- 不应触发额外 sync

### 5. 同步失败

- 不应继续发新的 side effect
- 应保留后续重试机会

## 实施范围

预计改动主要集中在：

- `server/src/assembly.rs`
- `server/src/exchange_freshness.rs`
- `server/src/runtime.rs`
- `server/src/effect_worker.rs`
- `server/src/order_outcome.rs`
- 相关 assembly / runtime / effect_worker 测试

第一版明确不改：

- `server/src/write_service.rs`
  原因：freshness 语义不属于写侧状态持久化边界，第一版不把这份知识再往 `write_service` 下沉。
- `server/src/execution_guard.rs`
  原因：第一版由 `server/src/exchange_freshness.rs` 直接承担门控语义，不引入第二个名字相近的浅模块。

第一阶段不要求改动：

- `engine`
- `exchange adapter`
- `protocol`
- `tui`

## 风险与取舍

### 风险

- 价格频繁波动时，可能增加 `sync_exchange_state` 次数
- 如果门控条件过宽，可能带来保守执行

### 取舍

本设计明确偏向：

- 少发错误单
- 接受少量额外同步成本

原因是当前主要风险不是“少同步一次”，而是“基于旧状态继续执行”。

## 后续演进

如果未来仍持续遇到以下问题：

- 部分成交与仓位更新严重乱序
- 多交易所行为不一致
- 需要区分本地估计态与确认态做展示或风控

再考虑升级到双状态模型。当前阶段不提前承担这部分结构成本。

## 边界说明

这套设计明确区分两类情况：

### 普通时序延迟

例如：

- `Filled` 先到，`PositionUpdate` 后到
- 真实成交已发生，但 `Price` 先触发了下一轮 reconcile

这类情况不再靠“事件类型专用 sync”处理，而是交给 freshness state + freshness gate：

- 可以继续更新目标
- 但不允许基于可能过时的状态直接发 side effect

### 明确状态失配或执行结果不确定

例如：

- `UnabsorbedOrderUpdate`
- `SubmitOutcomeUnknown`
- `CancelOutcomeUnknown`

这类情况说明本地状态已经无法解释交易所当前状态，属于异常恢复路径，因此继续保留即时 sync。

其中：

- `UnabsorbedOrderUpdate` 由 `runtime` 首先观察到并写入 stale
- `SubmitOutcomeUnknown` / `CancelOutcomeUnknown` 由 `effect_worker` 首先观察到并写入 stale

这不会破坏边界，因为 freshness 语义仍只存在于 `exchange_freshness` 一个 owner 中；变化的只是事实生产者，而不是语义 owner。

## 持久化边界

第一版建议 `exchange_freshness` 只保存在 `ServerState` 的进程内共享状态里，不持久化到 snapshot。

原因：

- 它表达的是“当前进程对交易所状态可信度的判断”
- 服务重启后，现有 startup sync 本就会重新从交易所建立真实状态
- 如果现在就把 freshness 写进快照，会把运行时协调状态泄露到持久化模型，增加不必要的耦合
