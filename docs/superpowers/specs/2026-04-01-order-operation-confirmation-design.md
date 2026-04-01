# Order Operation Confirmation Design

**背景**

当前运行时对订单操作成功的判断偏弱：

1. `submit_order` 返回 receipt 后，本地往往把它当成“订单已成功进入受控工作集”。
2. `cancel_order` 返回 `Ok(())` 后，本地直接清理 slot，并把 effect 标成 `succeeded`。
3. 正常运行时没有对全部 track 做低频 `openOrders + position` 对账，未追踪挂单常常要等到重启后的 `startup_sync` 才会暴露。

这会带来三类实际问题：

1. `submit receipt did not match executor slot` 时，交易所真实单可能已经存在，但本地没有稳定接住它。
2. `Unknown order sent` 时，真实含义可能是“订单已先成交或已先取消”，而不是普通撤单失败。
3. 本地 `slot` 工作集和交易所真实 open orders 工作集分叉后，系统可能仍显示 `normal`，直到重启或异常恢复路径才发现。

当前 executor 设计见 [2026-03-29-inventory-executor-architecture-design.md](2026-03-29-inventory-executor-architecture-design.md)。本次方案不推翻该设计，不引入独立订单台账，也不把 slot 模型替换成完整 per-order 状态机。

## 目标

- 明确“订单操作成功”的确认语义，不再把单次 REST 成功返回当成最终确认。
- 在正常运行时补上低频交易所对账，避免未知挂单只能靠重启发现。
- 统一处理 `submit receipt did not match executor slot` 和 `Unknown order sent` 这类“结果未知”的情况。
- 保持现有 executor / manager / write_service 边界，不重写执行模型。

## 非目标

- 不在本次引入完整 per-order 确认状态机。
- 不新增独立订单台账或多 slot 执行器。
- 不把 Binance 原始事件和错误语义直接泄漏到 executor。
- 不改变对外 TUI/HTTP 协议主模型，只在必要时补充更清晰的异常来源。

## 问题定义

这里真正要判断的不是“REST 调用成功没成功”，而是：

- 本地是否已经发出订单操作 intent
- 交易所是否接受了这次请求
- 交易所后续事实是否已经与本地预期一致

当前系统把前两步混成了一步，从而出现：

- 接口成功，但 slot 未稳定接住
- 接口失败，但真实订单状态已终结
- 本地 `slot` 和交易所真实 open orders 工作集不一致

## 候选方向

### 方向 A：维持当前模型，只继续修个别错误分支

做法：

- 针对 `submit receipt unmatched`、`Unknown order sent` 继续补局部恢复逻辑。

优点：

- 改动最小。

缺点：

- 仍然依赖“出问题时碰巧进入异常分支”才能发现分叉。
- 正常运行中的未知挂单仍可能长期隐藏。

### 方向 B：保留现有 slot 模型，增加低频对账巡检

做法：

- REST 返回只表示“请求被接受”，不再等价于最终确认。
- 对所有正常 track 增加低频 `position + openOrders` 对账。
- `submit receipt unmatched`、`Unknown order sent`、无法吸收的 order update 都统一触发一次立即对账。
- 对账仍由现有 executor 产出 `Rebuilt / Anomaly`。

优点：

- 能补上“重启才发现未知挂单”的主要缺口。
- 不需要重写 executor 抽象。
- 复杂度可控，知识边界清晰。

缺点：

- 仍然不是逐单强一致状态机。
- 会增加交易所查询频率，需要控制巡检节奏。

### 方向 C：引入完整 per-order 确认状态机

做法：

- 为每笔订单显式维护 `local_intent / rest_ack / exchange_confirmed` 等确认级别。
- slot 不再独自承担全部订单归属知识。

优点：

- 语义最完整。
- 对竞态的表达最清晰。

缺点：

- 改动面大。
- 会显著扩展 engine/server 复杂度。
- 与当前单 slot executor 设计不匹配，风险高。

## 设计结论

选择方向 B。

原因：

- 现阶段主要问题不是“缺少完整订单状态机”，而是“没有运行中持续对账能力”。
- 低频对账巡检已经能覆盖当前最痛的缺口：未知挂单不必等到重启才能发现。
- 该方案把新增复杂度主要放在 runtime 的对账队列与巡检调度，以及 write_service 的 stale follow-up 退休规则，不扩散到 executor 接口层。

## 核心设计

### 0. 统一对账触发抽象

本方案不允许由不同时间源各自长出独立的“去对账”逻辑。所有触发源统一先收敛为一个请求抽象：

```rust
pub struct ReconcileRequest {
    pub track_id: TrackId,
    pub reason: ReconcileReason,
}

pub enum ReconcileReason {
    PeriodicAudit,
    SubmitOutcomeUnknown,
    CancelOutcomeUnknown,
    UnabsorbedOrderUpdate,
    ManualRecovery,
}
```

规则：

- REST 错误、receipt 回写异常、user data 兜底、低频巡检都只能产出 `ReconcileRequest`
- 是否立即执行、是否防抖、是否合并到现有 per-track 串行写路径，都由统一对账入口决定
- 任何新增触发源都必须先映射到 `ReconcileReason`，而不是直接新增一条旁路逻辑

这样可以避免按“时间源”分解行为，把复杂度压回一个统一入口。

### 1. 订单操作确认分三层理解

本次不把这三层完整持久化成新状态机，但要统一按这三层理解代码路径：

1. `local_intent`
   - 本地已生成 submit/cancel effect
2. `rest_ack`
   - 交易所 REST 请求返回成功
3. `exchange_confirmed`
   - 后续通过 user data 或 exchange 对账确认真实状态与本地预期一致

实现约束：

- `rest_ack` 不等于最终确认。
- 一旦本地无法证明 `exchange_confirmed`，就必须进入对账。

### 2. 定义“结果未知”而不是一律普通失败

以下情况统一视为 `outcome unknown`，不是普通业务失败：

- `submit receipt did not match executor slot`
- `cancel_order` 返回 Binance `-2011 Unknown order sent`

这类情况不由 runtime 直接做错误语义判断，而是先交给一个独立的语义归类层：

```rust
pub enum OutcomeClass {
    FinalFailure,
    OutcomeUnknown(ReconcileReason),
}
```

建议模块边界：

- `order_outcome`（或等价命名）负责：
  - 输入：REST 错误、receipt 回写结果
  - 输出：`FinalFailure` 或 `OutcomeUnknown(ReconcileReason)`
- `runtime` 只接收 `ReconcileRequest`，不长期承担 Binance 错误码语义知识
- `order update` 的“是否无法吸收”不经过 `order_outcome`
- 它仍由当前订单事实吸收路径的拥有者判定，再直接映射成 `ReconcileReason::UnabsorbedOrderUpdate`

对于 `OutcomeUnknown`，统一触发：

- 构造 `ReconcileRequest { track_id, reason }`
- 交给统一对账入口
- 对账入口拉取 `position`
- 对账入口拉取 `openOrders`
- 对账入口调用 `write_service.sync_exchange_state(...)`

对账后再由 executor 决定：

- `Rebuilt`
- `Anomaly`

这样可以把“未知结果”的解释责任从 worker 移回 exchange facts。

### 3. 对所有正常 track 增加低频巡检

当前 recovery task 只会轮询已经进入 `recovery_anomaly` 的 track。方案改为：

- 对所有 `active / reducing_only / frozen / holding` 且未终止的 track 做低频巡检
- 第一版巡检频率建议 `5s`
- 巡检本身不直接做对账写回，只负责发：
  - `ReconcileRequest { reason: PeriodicAudit, .. }`

巡检目标不是每次都触发异常，而是尽早发现：

- 交易所 open orders 与本地 slot 工作集不一致
- 交易所仓位与本地 current exposure 不一致

### 3.1 当前实现的执行语义

当前落地版本分成两条执行路径：

1. `SubmitOutcomeUnknown`、`CancelOutcomeUnknown`、`UnabsorbedOrderUpdate`
   - 通过 `enqueue_reconcile_request(...)` 立即执行一次对账
   - 不额外排入 background 队列
2. `PeriodicAudit`
   - 由 recovery task 的 per-track 定时器触发
   - 与 anomaly track 的自动重试共享同一条 background 调度循环

当前还没有实现“紧急 reason 与 `PeriodicAudit` 在同一个显式队列里合并”的完整版本。  
现阶段依赖的是：

- write-side 的 per-track mutation lock 保证同一 track 的写入串行
- recovery task 的 per-track audit 计时器避免重复巡检
- 紧急对账通过即时入口缩短发现和恢复时间

这意味着：

- 当前实现已经避免了“正常轨道完全不巡检”的问题
- 但 `ReconcileRequest` 的真正合并队列仍是后续可继续增强的点

### 3.2 紧急对账的可观测语义

“多个紧急 reason 可合并为一次执行”不能只体现在少打一轮 REST。统一入口必须给每次实际执行的对账保留一个稳定、可测试的语义载体。

当前代码仍保留这组类型，用来稳定表达“这次执行是巡检还是紧急对账”：

```rust
pub enum ReconcileTriggerClass {
    Periodic,
    Emergency,
}

pub struct ReconcileExecution {
    pub track_id: TrackId,
    pub trigger_class: ReconcileTriggerClass,
    pub merged_reasons: SmallVec<[ReconcileReason; 4]>,
}
```

当前约束：

- `merged_reasons` 当前实现里通常只有一个 reason；background 巡检和即时对账还没有共享同一个合并队列。
- `trigger_class` 只表达“这轮执行是巡检还是紧急对账”，不承担多个紧急 reason 之间的排序语义。
- 当且仅当 `merged_reasons` 全部为 `PeriodicAudit` 时，`trigger_class = Periodic`。
- 只要 `merged_reasons` 中包含任一非 `PeriodicAudit` 的 reason，这轮执行就必须取 `trigger_class = Emergency`。
- 被紧急对账覆盖的 `PeriodicAudit` 不得额外补跑一次；是否已被覆盖必须能从 `ReconcileExecution` 观察到。

这样至少能明确区分：

- 这是一次单纯 `PeriodicAudit`
- 这是一次由 `CancelOutcomeUnknown` 升级出来的紧急对账
- 这是一次单个紧急来源触发的立即对账

### 4. user data 继续做快速路径，对账负责兜底

本次不把 user data 改成全新确认状态机。

保留现有语义：

- `PositionUpdate` 继续快速更新 `current_exposure`
- `OrderUpdate` 继续快速更新/清理 slot

新增约束：

- 如果 user data 无法被当前 slot 工作集吸收，不得只做静默 no-op
- 必须转成 `ReconcileRequest { reason: UnabsorbedOrderUpdate, .. }`

这样可以保持 user data 的低延迟优势，同时避免“漏吸收就永远沉默”。

### 4.1 `order update` 吸收结果抽象

为了避免 `order_outcome` 复制 slot 认领规则，`order update` 路径使用独立结果抽象：

```rust
pub enum OrderUpdateAbsorbResult {
    Applied,
    DuplicateReplay,
    Unabsorbed,
}
```

语义：

- `Applied`
  - 该事件已被当前 slot 工作集吸收
- `DuplicateReplay`
  - 该事件与当前 slot 中的订单事实等价，只是重复重放
- `Unabsorbed`
  - 该事件无法由当前 slot 工作集解释，必须发 `ReconcileRequest::UnabsorbedOrderUpdate`

这个结果由订单事实吸收路径拥有；`runtime` 只消费结果，不重做 slot 匹配判断。

唯一实现位置约束：

- §5 中列出的“无法吸收”判定条目只能在一个实现点落地
- 第一版固定在 `engine/src/executor/recording.rs` 的 order update 吸收路径
- 当前实现通过 `apply_order_observation_with_result(...)` 这个紧邻 helper 暴露该判定
- user data 入口、runtime 与测试都只调用这一个判定实现，不得在 `order_outcome` 或 runtime 里各自维护半套规则

### 5. 定义“无法吸收”的可检验边界

本次不写完整状态机，但必须把“无法吸收”写成可枚举规则。

以下情形视为 `UnabsorbedOrderUpdate`：

1. 收到 `keeps_working_order()` 的订单更新，但当前没有任何 slot 能按
   `client_order_id + order_id`
   唯一匹配该订单。
2. 收到订单更新时，存在多个 slot 都能匹配同一条订单事实。
3. 收到 terminal order update 时，当前 slot 工作集无法证明这条终态属于自己已知生命周期，但这条事件会影响本地对真实工作集的理解。

以下情形不视为 `UnabsorbedOrderUpdate`：

1. 同一 `client_order_id / order_id` 的重复重放，且更新内容与当前 slot 中的订单事实等价。
2. 乱序但仍能被当前 slot 唯一吸收的更新。
3. 与当前 track 无关的 instrument 事件。

测试必须直接覆盖这些条目，不允许只写“无法吸收时会触发对账”的描述性测试。

### 6. 不把巡检逻辑下沉到 executor

职责边界保持：

- `runtime`
  - 接收 `ReconcileRequest`
  - 执行即时对账入口
  - 调度 `PeriodicAudit` 与 anomaly track 的 background 重试
- `write_service`
  - 负责拉 pending submit hints、持久化 per-track 写事务
- `executor`
  - 只负责根据 live facts 判定 `Rebuilt / Anomaly`

这样能避免把交易所拉取节奏、错误分类、巡检调度泄漏进 executor。

### 7. 先不引入完整 per-order 状态机

原因：

- 当前主要缺口是“发现分叉”，不是“缺少更细的展示字段”。
- per-order 状态机会显著增加 engine/server 的认知负担。
- 低频巡检已经能把“正常运行中未知挂单长期隐藏”的问题大幅缩小。

如果后续仍然出现：

- 巡检频率足够但仍难以解释生命周期
- 多个竞态都需要在单笔订单层面稳定还原

再评估引入完整确认状态机。

## 边界与职责

### runtime

- 负责全部正常 track 的低频巡检调度
- 负责消费 `ReconcileRequest`
- 负责统一的 per-track 对账排队与节流
- 不直接承担交易所错误语义归类

### outcome classifier

- 只负责把 REST 错误、receipt 回写异常转换成 `OutcomeClass`
- 不做对账调度，不直接写状态

### order update absorb path

- 负责吸收 user data `OrderUpdate`
- 负责产生 `OrderUpdateAbsorbResult`
- 负责实现 §5 中“无法吸收”的唯一判定规则
- `Unabsorbed` 时由调用方映射成 `ReconcileReason::UnabsorbedOrderUpdate`

### effect worker

- 继续负责 effect 执行
- 不再把 `Unknown order sent` 简单当成普通失败终点
- 只负责把未知结果交给 outcome classifier，再发 `ReconcileRequest`

### write service

- 继续是 per-track 串行提交点
- 保持 `sync_exchange_state(...)` 为唯一对账写入口

### executor

- 保持现有恢复认领决策表
- 继续输出 `Rebuilt / Anomaly`
- 不感知巡检频率与 REST 错误分类细节

### stale follow-up submit 的归属

旧 lifecycle 的 follow-up submit 清理不放在 runtime。

原因：

- 它依赖 batch / effect 状态与 per-track 写事务边界
- 这类知识已经在 write-side 持久化路径里
- runtime 只应表达“何时需要对账”，不应拥有旧 batch 退休规则

因此本次明确：

- stale follow-up submit 的退休规则由 `write_service` 统一拥有
- 触发条件不由 `write_service` 自己推断，而是来自一个显式输入
- runtime 只负责把相关事实送到对账写入口，不直接清理旧 batch

最终输入抽象：

```rust
pub struct FollowUpRetirementRequest {
    pub batch_id: String,
    pub blocked_sequence: u32,
    pub closed_order_id: String,
}
```

语义：

- 它表达“同一 batch 中，某个旧 effect 序号之前的 lifecycle 已经结束，后续被它阻塞的 submit 可以被退休”
- `write_service` 负责用 `batch_id + blocked_sequence` 找到同 batch 的 replacement submit
- `write_service` 用 `closed_order_id` 判断当前 slot 是否还属于旧生命周期；如果不属于，就 supersede 对应 follow-up effect，并清理可能已经恢复出来的 `SubmitPending` slot

这样可以把“旧生命周期终结”这个设计知识收成一个窄接口，而不是让 write-side 长出第二套生命周期推理规则。

## 测试策略

第一阶段至少补四类验收：

1. 正常运行中的 track 存在未知 live order，不重启，也能靠 `PeriodicAudit` 进入恢复路径。
2. `submit receipt did not match executor slot` 会被 outcome classifier 转成 `SubmitOutcomeUnknown`，而不是只记普通失败。
3. `Unknown order sent` 会被 outcome classifier 转成 `CancelOutcomeUnknown`，而不是只记普通失败。
4. 可枚举的 `UnabsorbedOrderUpdate` 情形会发出 `ReconcileRequest`，重复重放和可吸收乱序不会误触发。
5. `Unknown order sent` 后，旧 lifecycle 的 follow-up submit 会被 write-side 退休，不会长期以 `Pending` 残留。

另外保留现有回归：

- receipt-backed working order 不允许被后续小 submit recovery 抢 slot
- `unknown_live_order` 时自动取消未知 live orders 后可恢复

## 风险与权衡

### 成本

- 增加交易所查询次数
- runtime 调度逻辑更复杂

### 收益

- 运行中能更早发现未追踪挂单
- 把“未知结果”统一交给 exchange facts 解释
- 不需要立即重做 executor 模型

### 控制手段

- 当前默认巡检频率是 `5s`
- 单个 track 的巡检必须串行，不与已有 per-track 写事务并发冲突
- 如果连续巡检失败，只告警，不立即扩大异常语义

## 为什么不是 TTL 方案

本次问题不是“老单挂太久没人撤”，而是“本地和交易所工作集会分叉，且运行中缺少持续对账”。  
直接加 TTL 只能改变订单寿命，不能解决“本地是否真的知道交易所现在有什么单”。

因此本次优先顺序应该是：

1. 先补确认和对账
2. 再讨论是否需要 working order 的超时重报价
