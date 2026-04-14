# Recovery 协调与读侧广播分离设计

> 更新：market data stale 的后台调度问题已在 [Market Data Health 调度设计](2026-04-15-market-data-health-scheduling-design.md) 单独处理；本文继续只覆盖 recovery 与读侧广播的边界，不再同时承载 `refresh_market_data_health()` 的调度设计。

## 背景

当前线上仍然持续出现：

- `recovery notification stream lagged by N messages; reseeding recovery tracking`

这说明 `recovery` 任务即使已经做过 batch 合并，仍然跟不上内部通知流。

根因不是 batch 大小，而是抽象本身不对：

- `application` 写边界在每次持久化后发送 `ApplicationNotification::TrackChanged`
- `websocket`、测试和部分应用服务都订阅这条广播
- `recovery` 也订阅同一条广播，再自己回库判断 `recovery_anomaly` 是否存在

这让 `recovery` 被迫消费它并不关心的大量写通知。只要市场数据写入频率高，`recovery` 就会和 websocket/UI 一起竞争同一条广播流，最终触发 `Lagged`。

本次设计的目标，是把 `recovery` 从通用读侧广播里拿出来，让它只消费“恢复跟踪状态发生边沿变化”这一类真正相关的事实。

## 已确认事实

### `recovery` 真正关心的输入很少

`recovery` 任务只需要两类输入：

- 某个 `track_id` 的 `recovery_anomaly` 是否从无变有，或从有变无
- 是否需要做一次启动或异常后的全量 reseed

它不需要知道：

- 这个 track 最近一共写了多少次库
- 最近是不是有普通价格更新、仓位更新、UI 需要的投影更新

### 给 `ApplicationNotification` 加新枚举没有解决根因

即使把现有通知扩展成：

- `TrackChanged`
- `RecoveryTrackingChanged`

也没有从模型上解决问题，因为 `recovery` 仍然必须先从同一个 `broadcast::Receiver` 把每条消息读出来。只要同一根广播里还有大量 `TrackChanged`，`recovery` 依然会 lag。

### 这个信号不应该暴露给 UI

`recovery_anomaly` 是否存在，本来就会通过 read model 投影成 UI 可见状态，例如：

- `has_recovery_anomaly`
- `recovery_anomaly`

但“恢复跟踪需要更新”是后台运行时协调知识，不应该进入 websocket 协议，也不应该塞进 `ApplicationNotification` 这种读侧广播接口里。

## 问题定义

本次真正的设计决策是：

> `recovery` worker 应该通过什么接口得知“哪些 track 的恢复跟踪状态发生了变化”？

这个接口需要满足四个要求：

1. 不再依赖高频 `TrackChanged` 广播
2. 不把 runtime 协调知识暴露给 UI
3. 不让 `application` 直接知道 `server` 里存在某个 recovery worker
4. 让“`recovery_anomaly` 边沿变化”这份知识只保留在尽可能少的地方

## 目标

- `recovery` 不再订阅 `ApplicationNotification`
- websocket / UI 继续沿用现有 `ApplicationNotification`
- `recovery` 普通路径不再回库读取 snapshot 来判断 anomaly 是否变化
- `recovery` 的输入从“消息流”改成“最新事实”
- 保持 `submit preflight` 与这次改造解耦，不再把两个问题混在同一轮重构里

## 非目标

- 不修改 websocket 对外协议
- 不重做 `SubmitPreflight` 的现有边界
- 不在本次设计里解决 `recovery` 的 50ms 全量 `refresh_market_data_health()` 轮询
- 不改变 `engine` 内部如何计算 `recovery_anomaly`

## 现状中的设计问题

### 1. `ApplicationNotification` 同时承担了读侧失效和运行时协调

现在的 `ApplicationNotification` 更适合表达：

- 读侧数据可能变了
- websocket 可以重新投影

但 `recovery` 使用它时，实际上是在拿“读侧失效信号”做“运行时调度输入”。

这会带来两个复杂度问题：

- `recovery` 必须理解大量与自己无关的写入来源
- 任何 `TrackChanged` 语义调整，都可能意外影响 `recovery`

### 2. `recovery` 在消费端重新推导写边界已经知道的事实

`MutationExecutor::commit_track_mutation(...)` 同时拿到：

- `previous_snapshot`
- `next_snapshot`

所以“`recovery_anomaly` 是否从无变有，或从有变无”这件事，在写边界最容易、也最准确判断。

当前实现却把这个事实丢掉，然后让 `recovery` 在广播消费者一侧：

1. 收到 `TrackChanged`
2. 再回库读取 snapshot
3. 再自己判断 anomaly 是否存在

这是明显的重复知识和重复工作。

### 3. 现有问题不是通知条数优化问题，而是边界问题

继续优化以下策略都只是战术补丁：

- 扩大广播容量
- 延长 drain 时间
- 增加批大小
- 放慢 `recovery` 处理频率

它们都没有改变最核心的问题：

> `recovery` 仍然在消费错误的抽象。

## 备选方案

### 方案 A：继续优化通用广播消费

做法：

- 保留 `recovery` 对 `ApplicationNotification` 的订阅
- 继续调 batch、容量、间隔

优点：

- 改动最小

缺点：

- 只是缓解，不是修正边界
- `recovery` 仍然依赖高频读侧广播
- 后续每次通知量上涨，都可能再次出现 lag

结论：

- 不采用

### 方案 B：给 `ApplicationNotification` 增加 recovery 专用枚举

做法：

- 在现有广播里增加类似 `RecoveryTrackingChanged`

优点：

- 比纯 `TrackChanged` 更有语义

缺点：

- `recovery` 仍然必须消费同一个高频广播
- runtime 协调知识被混入 UI/读侧通知接口
- 读侧和后台运行时职责继续共用一套消息模型

结论：

- 不采用

### 方案 C：新增 server 内部 recovery 通道，直接发送 `RecoveryTrackingChanged`

做法：

- `application` 写边界直接往一条新通道发送：
  - `track_id`
  - `active`

优点：

- `recovery` 不再依赖通用广播

缺点：

- `application` 需要直接知道 server 里的 recovery 概念
- `RecoveryTrackingChanged` 是 server 运行时命名，不是写边界中性事实
- 这会把 `server` 术语反灌进 `application`

结论：

- 不采用

### 方案 D：`application` 输出中性写事实，`server` 自己落到 `RecoveryDirtyState`

做法：

- `application` 在 commit 成功后判断 `recovery_anomaly.is_some()` 是否发生边沿变化
- 如果发生变化，就通过专用 observer 回调：
  - `track_id`
  - `active`
- `server` 提供内部 observer 实现，把这次变化写进自己的 `RecoveryDirtyState`
- `recovery` worker 只消费 `RecoveryDirtyState`

优点：

- `application` 只表达“这次写入后哪些事实变化了”，不表达 server 调度动作
- `server` 只表达“我如何利用这些事实驱动 recovery”
- 没有 runtime 消息队列，因此不会因为无关高频流产生 lag
- `recovery` 普通路径不需要回库重读 snapshot

缺点：

- 需要新增一条 application-owned 的写边界 observer 接口

结论：

- 采用

## 最终设计

采用 **方案 D：中性写事实 + server 内部脏状态**。

### 核心原则

- `ApplicationNotification` 继续只做读侧广播
- `application` 只输出中性写事实，不输出 recovery 命令
- `server` 自己把写事实转换成 recovery 脏状态
- `recovery` 只消费最新脏状态，不消费历史消息流

## 模块边界

### `application`

`application` 负责：

- 在 commit 成功后判断 `recovery_anomaly` 是否发生边沿变化
- 输出最小的中性写事实

`application` 不负责：

- 决定是否要重试 recovery
- 维护 recovery 跟踪集合
- 暴露 recovery 专用通知给 UI 或 websocket

### `server`

`server` 负责：

- 持有 `RecoveryDirtyState`
- 把 application 的写事实落成内部脏状态
- 由 `recovery` worker 消费这份脏状态并更新跟踪集合

`server` 不再负责：

- 从通用 `TrackChanged` 广播里重新猜测 recovery 状态

### `websocket` / UI

继续负责：

- 订阅 `ApplicationNotification`
- 收到 `TrackChanged` 后重新读取投影

它们不需要知道：

- `RecoveryDirtyState`
- recovery anomaly 边沿变化回调
- recovery worker 的调度策略

## 接口形状

### `application` 的专用 observer 接口

建议新增一个只表达当前唯一事实的 observer trait，由 `TrackServiceSet` 提供安装点，默认实现为 no-op：

```rust
pub trait RecoveryAnomalyObserver: Send + Sync {
    fn observe_recovery_anomaly_change(&self, track_id: &TrackId, active: bool);
}
```

语义约束：

- 只有在 `recovery_anomaly.is_some()` 的布尔值发生变化时才调用
- `active == true` 表示 `recovery_anomaly` 从无变有
- `active == false` 表示 `recovery_anomaly` 从有变无

要求：

- 这是 recovery anomaly 的专用回调，不是通用事实总线
- 默认实现为 no-op，不把少见情况推给所有调用方
- 同步接口即可，因为正常实现只是更新内存脏状态并 `notify_one`

### 为什么不做 `TrackMutationFacts`

本次刻意不引入类似：

- `TrackMutationFacts`
- `observe_track_mutation(...)`

这种通用事实容器。

原因是当前只有一个明确事实：

- `recovery_anomaly` 边沿变化

如果现在就把接口做宽，后续很容易变成“再加一个可选字段”的事实袋子，把多个运行时消费者重新混进同一个扩展点。更稳的做法是：

- 当前只为当前事实定义专用接口
- 将来真的出现第二个明确、稳定、同 owner 的写边界事实，再重新判断是否值得抽象

### `server` 内部的 `RecoveryDirtyState`

`server` 侧新增内部协调对象：

```rust
struct RecoveryDirtyState {
    updates: Mutex<HashMap<TrackId, bool>>,
    reseed_required: AtomicBool or Mutex<bool>,
    notify: Notify,
}
```

最小接口：

- `mark_recovery_anomaly(track_id, active)`
- `mark_reseed_required()`
- `take() -> RecoveryWorkset`
- `wait().await`

其中 `RecoveryWorkset` 建议是：

```rust
struct RecoveryWorkset {
    anomaly_updates: HashMap<TrackId, bool>,
    reseed_required: bool,
}
```

关键语义：

- 同一 track 连续多次变化，只保留最后一次状态
- 不积累历史条数
- 没有 lag 概念，因为它不是消息队列

## 数据流

### 正常写入路径

1. `MutationExecutor::commit_track_mutation(...)` 比较 `previous_snapshot` 和 `next_snapshot`
2. 只要 `recovery_anomaly.is_some()` 的布尔值发生变化，就调用 `observe_recovery_anomaly_change(track_id, active)`
3. server 的 observer 实现调用 `RecoveryDirtyState::mark_recovery_anomaly(track_id, active)`
5. `recovery` worker 被唤醒，消费最新 `RecoveryWorkset`
6. `recovery` worker 直接更新 `tracked` 集合，不再回库重读 snapshot

### 全量重建路径

以下情况仍允许 reseed：

- runtime 启动
- 内部状态明确丢失
- 将来如果出现 recovery 跟踪数据结构损坏

reseed 仍然通过 `mutation_store.load_track_state(...)` 全量读取 snapshot 完成，但它不再是正常高频路径。

## 为什么这个方案更合理

### 它把知识放回了正确 owner

“`recovery_anomaly` 是否发生边沿变化”这份知识，只应该存在于：

- 写边界比较前后 snapshot 的地方

而不应该分散在：

- 通知生产者
- 广播消费者
- recovery worker 的回库读取逻辑

### 它没有把 runtime 术语泄漏到 UI

UI 仍然只消费：

- `TrackChanged`
- `AccountChanged`

后台协调事实只在：

- `application` 的 recovery anomaly observer
- `server` 的 `RecoveryDirtyState`

这两层之间流动。

### 它把问题从“消息处理性能”改成“状态同步正确性”

当前 lag 的本质是：

- 错误的消费者在追错误的消息流

新设计下，`recovery` 不再面对无关消息洪峰，系统行为会更接近它真实职责：

- 收到相关事实
- 更新跟踪集合
- 在 retry / audit 定时点执行恢复动作

## 与现有改动的关系

### `SubmitPreflight`

本次设计不再把 `SubmitPreflight` 和 `recovery` 混成一个问题。

当前 `SubmitPreflight` 的独立 worker、`SubmitCoordinator`、`SubmitFlight` / `SubmitCompletion` 设计保持不变。这次只处理：

- `recovery` 如何获得自己的输入

### 50ms `refresh_market_data_health()` 轮询

这仍然是独立问题。

它可能继续带来额外负载，但不是这次 `lagged by 92` 的根因。当前日志里的 lag 是因为 `recovery` 仍在消费高频广播，而不是因为它只做了 20Hz 自己的本地轮询。

因此这次设计先不把调度模型和通知模型一起改。

## 验收标准

实现完成后需要满足：

1. `server/src/runtime/reconcile.rs` 不再订阅 `state.notifications`
2. `ApplicationNotification` 不新增 recovery 专用枚举
3. 普通 `TrackChanged` 风暴下，如果 `recovery_anomaly` 没有边沿变化，就不会给 `recovery` 增加工作
4. `recovery_anomaly` 从无到有、从有到无时，`recovery` 能正确更新跟踪集合
5. websocket 和现有 UI 更新行为不变
6. 启动和显式 reseed 路径仍然可用

## 测试建议

至少补这几类测试：

### application 层

- `commit_track_mutation` 在 `recovery_anomaly` 未变化时不调用 observer
- `commit_track_mutation` 在 `None -> Some` 时调用 `observe_recovery_anomaly_change(..., true)`
- `commit_track_mutation` 在 `Some -> None` 时调用 `observe_recovery_anomaly_change(..., false)`

### server 层

- `recovery` worker 不再因为高频 `TrackChanged` 广播触发 `Lagged`
- `RecoveryDirtyState` 对同一 `track_id` 的连续更新只保留最后状态
- startup reseed 仍能正确建立初始 `tracked` 集合

## 实施顺序

1. 在 `application` 新增 `RecoveryAnomalyObserver` 及其默认 no-op 实现
2. 给 `TrackServiceSet` 增加安装 `RecoveryAnomalyObserver` 的能力，并提供默认 no-op 实现
3. 在 `commit_track_mutation(...)` 里判断 `recovery_anomaly.is_some()` 是否发生边沿变化
4. 在 `server` 新增 `RecoveryDirtyState` 和 observer 实现
5. 改写 `recovery` worker，让它消费 `RecoveryDirtyState`
6. 删除 `recovery` 对 `state.notifications.subscribe()` 的依赖
7. 补齐回归测试

## 决策总结

这次不再沿着“优化 `recovery` 如何消费 `ApplicationNotification`”继续修补。

新的设计结论是：

- 读侧广播和 runtime 协调是两种不同抽象
- `recovery` 不该订阅通用广播
- `application` 应该输出中性写事实
- `server` 应该把这些事实落成自己的内部脏状态

这样才能从设计上消除这类 lag，而不是继续在同一条拥堵通道上做局部优化。
