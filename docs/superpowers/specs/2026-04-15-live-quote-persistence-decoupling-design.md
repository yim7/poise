# 实时报价、执行意图与读侧通知解耦设计

## 背景

当前 `/ws` 诊断已经说明，问题的主因不是 websocket 单条推送慢，而是上游 `TrackChanged` 太多。

一组实际运行窗口数据：

- `window_ms = 5073`
- `raw_track_notifications = 655`
- `track_pushes = 397`
- `avg_detail_query_ms = 1.42`
- `avg_send_ms = 0.02`

这说明：

1. 下游 websocket 批内去重已经有效，但只能压掉同一小批次内的重复通知
2. 真正的高频来源仍然是 `observe_market(...)` 之后持续不断产生的 `TrackChanged`
3. 如果不改上游建模，只继续在 websocket 末端补丁，收益会越来越有限

沿当前链路：

`bookTicker 实时推送 -> observe_market -> 持久化 snapshot -> TrackChanged -> websocket 重投影`

其中最关键的问题不是 Binance 推得快，而是：

- 实时报价字段被放进 durable snapshot
- raw live target 和 recovery/startup 需要的稳定执行目标目前还是同一个 `desired_exposure` owner
- UI 市场信息又完全依赖 durable `TrackChanged` 的发生频率刷新

## 问题定义

本次要解决的是：

> 如何保留 `bookTicker` 作为执行层实时输入，同时避免 raw quote / raw target 抖动直接驱动 durable 持久化与 full-detail websocket 重投影，并且不把 recovery/startup 重新做成依赖下一条 tick 的时序协议？

## 已确认事实

### 1. `bookTicker` 当前对执行层是必需输入

按当前实现：

- `strategy_price = (best_bid + best_ask) / 2`
- `Buy` 用 `best_ask`
- `Sell` 用 `best_bid`
- 没有 quote 时会进入 `MissingExecutionQuote`

所以本次不能简单移除 `bookTicker`。

### 2. raw `desired_exposure` 不是好的 durable 边界

对价格连续敏感的策略来说，盘口小幅变化就可能让 raw target 跟着波动。  
如果把 raw `desired_exposure` 本身当成 durable 事实，就算 websocket 末端做了再多去重，通知频率仍然可能接近当前量级。

真正更接近“需要 durable 写入”的边界，不是 raw target 是否变化，而是：

- 经过 `min_rebalance_units`
- 经过价格/数量步进
- 经过 gate
- 经过 effect planning

之后，**实际执行意图是否变化**。

### 3. recovery / startup 仍然需要稳定的 durable 执行目标

当前代码里 `desired_exposure` 不只是 UI 展示或临时计算值，它还是：

- recovery 重建 round policy 的输入
- startup 恢复后在没有新 tick 时的稳定目标
- executor active round 之外的一层 reconcile-owned 状态

因此本次不能简单把 `desired_exposure` 整体降成 live-only。  
真正需要拆开的，是：

- **raw live target**
  跟随 tick 连续变化
- **track 级 durable `desired_exposure`**
  只在量化执行意图真正变化时更新，并作为 recovery / startup 的稳定输入

### 4. UI 市场信息不能继续完全依赖 durable `TrackChanged`

如果把 live quote 从 durable 通知链路中拿掉，但又不单独定义 UI 市场字段如何刷新，结果就会变成：

- durable 通知确实少了
- 但 UI 的 `mark_price / best_bid / best_ask / strategy_price_status / desired_exposure` 也会长时间停在旧值

这不是实现细节，而是产品语义问题，必须在设计里先定清楚。

## 目标

- 保留 `bookTicker` 作为执行层实时输入
- 停止“每个盘口 tick 都持久化并广播 `TrackChanged`”
- 让 raw quote / raw target 抖动不再直接决定 durable 写入频率
- 让 UI 仍能以有上限的频率看到较新的市场与目标信息
- 不把 Binance 专有知识上浮到 application / server 通用层
- 保持现有执行规则、price gate 规则、submit planning 规则不变

## 非目标

- 不改变 Binance 市场数据订阅模型
- 不改变 `mark_price` / `execution_quote` 的业务含义
- 不重做 durable websocket 协议
- 不给 UI 提供 tick 级逐笔盘口流

## 备选方案

### 方案 A：只继续优化 websocket 下游

做法：

- 保留现有 `observe_market -> commit_track_mutation -> TrackChanged`
- 继续在 `/ws` 层做更强的 debounce / batching

缺点：

- 只能缓解末端压力
- 持久化、内部广播、读模型失效仍然按 tick 频率发生
- 不能真正改变通知源头

结论：

- 不采用

### 方案 B：把 raw quote 拆成 live state，但仍把 raw `desired_exposure` 当成 durable

做法：

- quote 字段不再持久化
- raw `desired_exposure` 仍然放在 snapshot，并继续参与 `TrackChanged`

缺点：

- 对价格连续敏感的策略里，通知频率仍可能很高
- 会出现“已经做了 live/durable 双层拆分，但核心问题只部分缓解”的情况

结论：

- 不采用

### 方案 C：拆出 live state，并把 durable 边界下沉到“量化后的执行意图”

做法：

- `bookTicker` 继续进入 engine/application
- quote 相关字段全部转成 live state
- raw `desired_exposure` 也不再直接作为 durable 字段保存
- 只有在量化后的执行意图、effects 或其他 durable 后果变化时，才持久化
- UI 市场信息改走一条单独的低频 live 刷新路径

优点：

- 执行层仍然拿到最新 quote
- durable 通知边界更接近真正重要的状态变化
- UI 刷新语义被单独设计，不再隐含依赖 durable 通知

缺点：

- 需要新增 live 查询和一条窄的 websocket live event
- query/read model 需要从“只读 snapshot”改成“snapshot + live view 拼装”

结论：

- 采用

## 最终设计

采用 **方案 C：把实时报价、raw target 与 durable 后果分成三层，并把 UI 市场刷新从 durable 通知里单独拆出**。

## 核心原则

- `bookTicker` 继续是执行输入，不是 durable 通知频率的直接来源
- raw quote 与 raw target 都属于 live state
- track 级稳定 `desired_exposure` 属于 durable state
- durable 层只保存跨进程保留的业务后果与稳定 `desired_exposure`
- UI 市场信息单独走低频 live 刷新，不再默认跟 durable `TrackChanged` 绑定

## 三层状态边界

### 1. live quote state

只在进程内存在，归 `engine/application` 运行时所有。

至少包含：

- `last_tick_at`
- `mark_price`
- `execution_quote`
- `strategy_price`

特点：

- 每个 tick 都可以更新
- 不进入 mutation store
- 不参与 durable snapshot 序列化
- 进程重启后自然丢失

`market_data_health_deadline(...)` 后续也只基于这层 live `last_tick_at` 计算。

### 2. live strategy target state

这一层同样不持久化，也由 `engine/application` 单点拥有。

至少包含：

- raw `desired_exposure`
- `strategy_price_status`
- `price_execution_gate`

这里的关键规则是：

- raw `desired_exposure` 变化本身不是 durable 事件
- 它只是当前 quote 下的策略目标视图
- 它可以在两次 durable 更新之间多次变化

### 3. durable execution state / snapshot

这一层保存跨进程保留的业务后果，以及 recovery / startup 需要的稳定执行目标。

至少包含：

- `desired_exposure`
- `market_data_stale_since`
- `replacement_gate_reason`
- `executor_state`
- `ledger_state`
- `risk`
- `status`
- 其他已有的 durable 诊断与生命周期字段

这里的关键规则是：

- 顶层 `desired_exposure` 不再表示 raw live target
- 它被重新限定为“当前已被 durable 执行层接受的稳定目标”
- 只有 raw target 进一步导致量化执行意图变化时，才会更新顶层 `desired_exposure`
- recovery / startup 使用的是这个 durable `desired_exposure`，不是等待下一条 tick 再重新推导 raw target

这一层还需要和现有 `executor_state.active_round.desired_exposure` 明确区分：

- 顶层 `desired_exposure` 是 **track 级 canonical durable target**
- `active_round.desired_exposure` 是 **executor-local round anchor**
- `active_round.desired_exposure` 只表达“当前进行中的 round 是围绕哪个目标开始的”
- 它不是第二个通用 durable target owner

同步规则固定为：

- `RoundLifecycleDecision::Start` 或 `Switch` 时：
  `active_round.desired_exposure = desired_exposure`
- `RoundLifecycleDecision::Continue` 时：
  继续保留原有 `active_round.desired_exposure`
- `RoundLifecycleDecision::Finish` 时：
  `active_round = None`，稳定目标只剩顶层 `desired_exposure`

因此允许出现的状态是：

- `desired_exposure != active_round.desired_exposure`

这不表示双 owner，而表示：

- track 级稳定目标已经更新
- 但当前 active round 还在围绕旧锚点执行
- 下一次 `Switch` / `Finish -> Start` 之后，active round 才会切到新的顶层 `desired_exposure`

明确不再保留：

- `strategy_price`
- `strategy_price_status`
- `mark_price`
- `best_bid`
- `best_ask`
- `last_tick_at`
- raw `desired_exposure`
- `price_execution_block_reason`

## 对外 live 查询边界

为了让执行层、query 层和 UI 都能拿到当前运行时视图，`engine/application` 暴露 3 个窄查询：

```rust
struct QuoteHealthView {
    strategy_price_status: StrategyPriceStatus,
    price_execution_gate: PriceExecutionGate,
}

struct StrategyTargetView {
    desired_exposure: Option<Exposure>,
}

struct TrackLiveView {
    strategy_price: Option<f64>,
    strategy_price_status: StrategyPriceStatus,
    mark_price: Option<f64>,
    best_bid: Option<f64>,
    best_ask: Option<f64>,
    desired_exposure: Option<f64>,
    price_execution_block_reason: Option<PriceExecutionBlockReason>,
}
```

语义：

- `QuoteHealthView`：当前如何解释 quote
- `StrategyTargetView`：当前策略 target 是什么
- `TrackLiveView`：给 query / websocket / UI 用的窄 live 投影视图

这些都不是 durable 事实，也不进入 snapshot。

同时 durable snapshot 明确保留：

```rust
struct TrackRuntimeSnapshot {
    desired_exposure: Option<Exposure>,
}
```

语义：

- 顶层 `desired_exposure`：当前已被 durable 执行层接受的稳定目标
- 它供 recovery / startup / round policy 使用
- 它不等于当前 raw live target
- 它也不等于 `active_round.desired_exposure`

如果当前存在 active round，则读取规则固定为：

1. executor 内部 round lifecycle / slot planning：
   优先使用 `active_round.desired_exposure`
2. startup 恢复、无 active round 的 recovery、query-time 稳定目标视图：
   使用顶层 `desired_exposure`

## market tick 的新返回边界

`observe_market(...)` 的结果收窄为：

```rust
enum MarketMutationOutcome {
    LiveOnly,
    Durable(TrackTransition),
}
```

语义：

- `LiveOnly`
  - tick 只改变了 live quote / live strategy / live UI 视图
  - 不需要 durable 持久化
- `Durable(TrackTransition)`
  - tick 已经引起 durable 后果变化
  - 继续走现有持久化与 `TrackChanged`

## durable 判定规则

market tick 进入后，系统不再问“raw target 变没变”，而是问：

> 这次 tick 有没有改变量化后的执行意图，或者产生新的 durable 后果？

只有下面这些情况才会返回 `Durable(...)`：

- 生成新的 domain events
- 生成新的 effects
- 执行状态、诊断状态、风险状态出现 durable 变化
- `market_data_stale_since` 边界变化
- 顶层 `desired_exposure` 变化，且这种变化会改变 effect 生成或 executor durable 状态

而下面这些情况都应返回 `LiveOnly`：

- `last_tick_at` 前进
- `mark_price` 更新
- `best_bid / best_ask` 更新
- `strategy_price` 更新
- raw `desired_exposure` 小幅变化，但没有改变顶层 `desired_exposure`

## 启动语义

因为 quote-derived 状态都不再持久化，启动阶段不再需要额外 reset 协议。

启动后在第一条新 tick 到来之前：

- live quote state 为空
- `QuoteHealthView` 自然返回“当前没有有效 quote”
- `TrackLiveView` 自然返回缺失 live 市场信息
- durable snapshot 继续表达跨进程保留的业务事实与顶层 `desired_exposure`
- recovery / startup 直接使用恢复出来的顶层 `desired_exposure`
- 如果 snapshot 里已有 `active_round`，则 executor 内部继续沿用其 `desired_exposure` 作为当前 round anchor
- 暂停 track 在没有新 tick 时恢复后，生命周期状态保持 `WaitingMarketData`，而不是假装回到 `Active`
- user-data replay、position update 等 durable 观察仍可更新仓位/账本，但依赖 live quote 的 submit / replacement 仍要等首个新 tick 才恢复

换句话说，重启恢复 durable 业务状态与稳定 `desired_exposure`，但不恢复实时盘口本身。

## UI 刷新语义

这次明确把 UI 刷新拆成两条路径。

### durable 路径

继续沿用现有协议：

- `TrackListItemChanged`
- `TrackDetailChanged`
- `AccountSummaryChanged`

它们只在 durable 业务状态变化时发送。

### live market 路径

新增一条窄 websocket 事件：

```rust
StreamEvent::TrackLiveViewChanged {
    track_id: String,
    live: TrackLiveView,
}
```

这条路径的规则固定为：

- 由 server 内部按连接、按 `track_id` 合并 dirty
- 第一版每个 `track_id` 每个连接最多 `4Hz`（`250ms` 一次）
- 只发送 `TrackLiveView`
- 不重投影整个 detail/list item

这样：

- UI 的市场与目标信息仍然足够新
- durable `TrackChanged` 不再承担 live price 刷新职责
- 高频 `bookTicker` 不会继续把 full detail 重投影频率带到 tick 级

## 模块职责

### exchange adapter

负责：

- 订阅 `markPrice + bookTicker`
- 解析 `mark_price` 与 `execution_quote`

不负责：

- durable 判定
- UI 刷新节流

### engine/application

负责：

- 持有 live quote state
- 计算 raw target、quote health 与量化执行意图
- 判断 `LiveOnly / Durable(...)`
- 暴露 `TrackLiveView` / `QuoteHealthView` / `StrategyTargetView`

不负责：

- websocket 网络发送
- UI 连接级节流

### application query

负责：

- 把 durable snapshot 与 `TrackLiveView` 拼成现有 `TrackReadSource` / `TrackReadModel`
- 让 HTTP / durable websocket 继续复用现有 projector 协议

不负责：

- 自己决定 live 刷新频率

### server/websocket

负责：

- durable `TrackChanged` 的批内合并与 full-detail 重投影
- `TrackLiveViewChanged` 的低频合并推送

不负责：

- 猜测 raw quote 变化哪些需要 durable 持久化

## 运行时行为

### 正常高频盘口更新

- Binance 持续推 `bookTicker`
- engine/application 持续更新 live quote 与 raw target
- 只有顶层 `desired_exposure` 或其他 durable 后果变化时，才持久化并发 `TrackChanged`
- UI 市场字段通过低频 `TrackLiveViewChanged` 刷新

### quote 缺失或恢复

当 quote 从：

- 有效 -> 缺失
- 缺失 -> 有效

时：

- `TrackLiveView` 与 `QuoteHealthView` 会立即变化
- UI 会经由低频 live 路径看到变化
- 只有它进一步引起 durable 后果时，才会发 `TrackChanged`

### stale timeout

`market_data_health_task` 继续在超过 `tick_timeout_secs` 后调用 `refresh_market_data_health()`。

它只负责 durable 的 stale 后果：

- 写入 `market_data_stale_since`

而不是恢复或持久化 live quote。

## 测试策略

至少补齐这些验收测试：

1. 高频 `bookTicker` 只更新 live quote，不产生 `TrackChanged`
2. raw `desired_exposure` 小幅变化但未改变顶层 `desired_exposure` 时，仍返回 `LiveOnly`
3. 量化执行意图变化并产生新 effects 时，仍走 `Durable(...)`
4. `TrackLiveViewChanged` 在高频 tick 下被按 `track_id` 和时间窗合并
5. UI 仍能看到较新的 `mark_price / best_bid / best_ask / desired_exposure`
6. stale timeout 到期时，仍会 durable 地把 track 标 stale
7. 重启后在第一条 tick 前，没有有效 live quote
8. `active_round.desired_exposure` 与顶层 `desired_exposure` 的同步规则被锁住：
   `Start/Switch` 同步、`Continue` 允许分离、`Finish` 后只剩顶层 `desired_exposure`

## 风险与取舍

### 风险：状态分成三层，理解成本上升

这是本方案新增的主要复杂度。

但这份复杂度对应的是系统里真实存在的三类不同问题：

- 实时盘口输入
- 实际执行意图
- durable 业务后果

如果继续把它们揉进一个 snapshot，复杂度只会继续以高频写库、高频通知和末端补丁的形式出现。

### 风险：新增一条 live websocket 事件

这会让 UI 协议多一条事件。

这是可接受的，因为：

- 它是窄 payload，不是第二套 full detail 协议
- 它直接表达“UI 当前市场字段如何刷新”这个产品语义
- 它比继续隐含依赖 durable `TrackChanged` 更清楚

### 风险：query 层需要拼 durable + live

这会让 query service 增加一个 live 查询步骤。

这是可接受的，因为：

- owner 仍然在 application
- server/projector 不需要复制 quote 规则
- 对外 HTTP / durable websocket 协议可以尽量保持稳定

## 实施顺序

1. 先收窄现有顶层 `desired_exposure` 的 durable 语义，把它和 raw live target 拆成两个 owner
2. 再把 live quote 与 query-time live 视图从 snapshot 中拿掉
3. 让 `observe_market(...)` 返回 `LiveOnly / Durable(...)`
4. 在 application query 层引入 `TrackLiveView`
5. 给 websocket 增加低频 `TrackLiveViewChanged`
6. 用现有 diagnostics 重新观测 `raw_track_notifications`、`track_pushes` 和 live push 频率

## 预期结果

完成后，系统行为应变成：

- Binance `bookTicker` 仍然实时进入执行层
- raw quote / raw target 抖动不再直接进入 durable 通知
- full-detail websocket 推送主要只对真正的 durable 业务状态变化发生
- UI 市场信息通过低频 `TrackLiveViewChanged` 保持新鲜
- 当前的 lag 主因会从“上游 tick 直接变 durable 通知”转成“只有真正重要的状态变化才发 durable 通知”
