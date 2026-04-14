# Recovery 与 Submit Preflight 解耦设计

## 背景

最近运行时暴露出两类问题：

- `recovery notification stream lagged` 频繁出现，说明 `recovery` 任务跟不上内部通知流。
- `SubmitPreflight` 的 pending submit 重算被挂在 `recovery` 的通知批处理后面，导致两条本来不同的职责被绑在一起。

当前实现里：

- `recovery` 订阅 `ApplicationNotification` 广播流，消费 `TrackChanged` / `AccountChanged`，再自己推导“哪些 track 需要更新恢复跟踪”“是否要 reseed”“是否要顺手重算 submit preflight”。
- `SubmitPreflight` 自己只拥有决策状态，但它的 pending submit 集合删除和重算不由它自己驱动，而是通过 `recovery` 通知批处理间接触发。

这让系统出现两个结构性问题：

1. `recovery` 关心的是“当前哪些 track 的恢复状态变脏了”，但实现却建立在“我有没有漏掉某条通知”之上。
2. `SubmitPreflight` 关心的是“当前哪些 submit effect 仍然处于 pending”，但它的维护时机被混进了 `recovery` 的职责里。

## 目标

- 让 `recovery` 不再按广播消息条数工作，而是按最新脏状态工作。
- 让 `SubmitPreflight` 的 pending submit bookkeeping 脱离 `recovery`，成为独立的运行时职责。
- 保留现有 `websocket` / UI 对广播事件流的消费方式，不把所有消费者都改成同一种模型。
- 让后续优化 `recovery` 或 `submit preflight` 时只改各自 owner，不再同时理解另一侧实现。

## 非目标

- 不重写 `engine` 的恢复语义。
- 不修改 `ExchangeFreshness` 的门控规则。
- 不引入新的持久化表。
- 不在本次设计里重做 `ApplicationNotification` 的对外协议。

## 当前问题

### 1. `recovery` 被迫消费它不需要的历史

`recovery` 任务真正需要的输入只有两类：

- 哪些 `track_id` 的恢复状态需要重新判断
- 是否必须 reseed 整个恢复跟踪集合

但当前接口给它的是广播流。它只能：

- 收到一条 `TrackChanged`
- 再去读一次 snapshot 判断 `recovery_anomaly`
- 顺手再决定要不要做全局 `submit preflight` 重算

这让实现被“通知条数”牵着走，而不是被“当前状态”牵着走。

### 2. `SubmitPreflight` 的 owner 不完整

`SubmitPreflight` 已经拥有：

- `startup_pending_submit_effects`
- `attempted_submit_effects`
- `decide(...)`

但它不拥有“何时重算 pending submit 集合”这份运行时知识。当前这份知识停留在 `recovery` 任务里。

结果是：

- 修改 `recovery` 的通知策略，会连带影响 `SubmitPreflight`
- 修改 `SubmitPreflight` 的删除/保留语义，也要回头碰 `recovery`

### 3. `ApplicationNotification` 对这两个消费者都太粗

`ApplicationNotification` 当前只有：

- `TrackChanged`
- `AccountChanged`

它适合 `websocket` 这类“需要知道有事发生了”的消费者，但不适合 `recovery` 和 `submit preflight` 这种只关心最新合并状态的后台维护任务。

## 备选方案

### 方案 A：继续优化 `recovery` 的 batch drain

- 给 batch 增加条数上限或时间上限
- 继续沿用广播消费模型

优点：

- 改动最小

缺点：

- `recovery` 仍然建立在广播流之上
- `SubmitPreflight` 仍然被绑在 `recovery`
- 复杂度只是从“每条通知”变成“每批通知”

### 方案 B：只把 `SubmitPreflight` 从 `recovery` 拆出去

- `SubmitPreflight` 改成独立脏标记/消费者
- `recovery` 继续消费广播流

优点：

- 可以先切掉最明显的职责耦合

缺点：

- `recovery` 的核心问题仍在
- 后续还得继续改第二次

### 方案 C：`recovery` 与 `SubmitPreflight` 都改成脏状态消费者

- `SubmitPreflight` 独立维护自己的 dirty flag
- `recovery` 独立维护自己的 dirty tracks 与 reseed 标记
- 广播流保留给真正需要事件流语义的消费者

优点：

- 边界最清楚
- `recovery` 不再追消息
- `SubmitPreflight` 不再依附在 `recovery`

缺点：

- 需要引入两个明确的运行时协调对象

## 结论

采用 **方案 C：`recovery` 与 `SubmitPreflight` 都改成脏状态消费者**。

## 2026-04-14 实现更新

本次实现比最初设计再往前推了一步，重点是把 `SubmitPreflight` 的失效知识从
`effect_worker` 控制流里拿掉：

- `application` 新增 `submit_effect_service` 模块，专门承载 submit effect 生命周期相关接口。
- `TrackEffectService` 只保留通用 effect 写接口，不再暴露 submit-specific 协议。
- `SubmitEffectService` 现在对外只暴露一次 `recover_or_dispatch(...)`，返回 `SubmitAttempt::{Dispatch, Finished}`。
- 真正继续执行 submit 时，`application` 层仍返回 `SubmitDispatch` 这个持久化写回 handle；它本身已经是 one-shot，终态写回方法会消费 handle，不再允许同一个 dispatch 被重复结束。
- `server` 侧新增 `SubmitCoordinator` / `SubmitFlight` / `SubmitCompletion`：`SubmitCoordinator::prepare(...)` 统一负责 preflight 判定、必要的 live-order lookup、`recover_or_dispatch(...)` 以及 `mark_submit_started(...)`；`SubmitFlight` 只负责拆出 `OrderRequest` 和一次性的 `SubmitCompletion`，后者再负责 submit 结果写回和 pending-submit dirty 转发。

因此，当前边界应理解为：

- `SubmitEffectService` 拥有“单次 submit 尝试如何从恢复判断进入 dispatch，以及后续写回会不会让 pending submit 集合失效”这份知识。
- `SubmitPreflight` 拥有 pending submit 集合的运行时缓存与重算调度。
- `SubmitCoordinator` 拥有“什么时候进入 in-flight submit 语义、什么时候需要把写回结果转成 preflight bookkeeping”这份 server 运行时知识。
- `SubmitCompletion` 把“一次 started submit 只能结束一次”这条约束收进 server 接口，不再让调用方保留一个可重复调用的终态 handle。
- `effect_worker` 只负责执行 submit 流程和处理交易所结果，不再自己拼接 preflight 判定、started 标记和 dirty 转发的时序。

## 设计

### 模块边界

#### `SubmitPreflight`

`SubmitPreflight` 继续拥有：

- 哪些 effect 属于启动恢复 pending submit
- 哪些 effect 在当前进程里已经尝试过 submit
- 每次 submit 前是否需要查交易所 live order

同时，它新增对“pending submit 集合需要重算”这份事实的 owner 身份。

也就是说：

- `SubmitPreflight` 不只拥有决策状态
- 还拥有自己的 maintenance trigger

#### `Recovery`

`Recovery` 只拥有：

- 哪些 `track_id` 的恢复跟踪需要重算
- 是否需要 reseed 当前 `tracked` 集合
- 周期性 anomaly retry / audit 调度

它不再拥有：

- `SubmitPreflight` 的全局 pending submit bookkeeping

#### `ApplicationNotification`

`ApplicationNotification` 继续保留给：

- `websocket`
- 其他只需要“有事发生了”的广播消费者

它不再是 `recovery` 和 `submit preflight` 这两类后台维护任务的主抽象。

### 新的协调对象

#### `SubmitPreflightDirtyState`

建议新增一个共享协调对象，内部至少包含：

- `dirty: bool`
- `notify: Notify`

它提供的接口保持最小：

- `mark_dirty()`
- `take_dirty() -> bool`
- `notified().await`

语义是：

- 生产者只负责标记“pending submit 集合可能变化了”
- 后台 worker 醒来后做一次完整重算

这样 `SubmitPreflight` 就不需要知道是谁触发了变化，也不需要追历史消息。

#### `RecoveryDirtyState`

建议新增一个共享协调对象，内部至少包含：

- `dirty_tracks: HashSet<String>`
- `reseed_required: bool`
- `notify: Notify`

它提供的接口保持最小：

- `mark_track_dirty(track_id)`
- `mark_reseed_required()`
- `take() -> RecoveryWorkset`
- `notified().await`

其中 `RecoveryWorkset` 只表达本轮要处理的最新事实：

- 一组去重后的 `track_id`
- 是否需要 reseed

### 生产者规则

#### `SubmitPreflight` 脏标记

以下情况只需要 `mark_dirty()`：

- 启动时已经完成 `startup_pending_submit_effects` 初始采样后，后续任何可能改变 pending submit 集合的 effect 持久化
- submit 成功、失败、supersede 等 effect 状态变化

对应语义是：

- worker 不关心具体是哪条 effect 变化了
- 只关心“现在数据库里的 pending submit 集合和内存缓存可能不一致”

#### `Recovery` 脏标记

以下情况调用 `mark_track_dirty(track_id)`：

- 某个 track snapshot 已持久化，并且它的 `recovery_anomaly` 可能变化

以下情况调用 `mark_reseed_required()`：

- 恢复跟踪状态已知可能丢失，需要整表重建

这两个操作都不要求后台任务追历史条数，只要求最终能看到最新 workset。

### 后台 worker

#### `SubmitPreflight` worker

运行模型：

1. 等待 `notify`
2. `take_dirty()`
3. 若为脏，则读取 `effect_store.list_all_pending_submit_effects()`
4. 把当前 pending submit effect id 集合传给 `submit_preflight.reconcile_pending_submit_effects(...)`
5. 回到等待

这个 worker 是全局 bookkeeping worker，不属于 `recovery`。

#### `Recovery` worker

运行模型：

1. 启动时 `seed_recovery_tracking(...)`
2. 同时等待：
   - 周期 `ticker`
   - `RecoveryDirtyState::notified()`
3. 收到脏标记后，`take()` 一次性取出最新 workset
4. 若需要 reseed，则整表重建 `tracked`
5. 否则只对这批 `track_id` 重新读取 snapshot，更新 `tracked`
6. 真正访问交易所的对账动作仍只在：
   - anomaly retry 到期
   - audit 到期

这保证了：

- 普通状态变化不会直接触发交易所对账
- `recovery` 不再因为通知风暴而被迫追消息

### 对旧实现的替换关系

本设计落地后，应删除 `recovery` 任务里对 `SubmitPreflight` 的附带处理，包括：

- `needs_preflight_reconcile()`
- “收到 recovery notification batch 后顺手跑一次 `reconcile_submit_preflight_state(...)`” 这条路径

`reconcile_submit_preflight_state(...)` 可以保留为 `SubmitPreflight` worker 的内部执行函数，或迁入专用模块，但不再由 `recovery` 拥有调用时机。

`collect_recovery_notification_batch(...)` 这套围绕广播流做的批处理，也不应再是 `recovery` 的主路径。

## 实现顺序

### 第一步：拆开 `SubmitPreflight`

- 引入 `SubmitPreflightDirtyState`
- 增加独立 worker
- 移除 `recovery` 中的 preflight 重算逻辑

这是最小可独立验收的一步。完成后：

- `SubmitPreflight` 与 `recovery` 的职责边界已经分开
- 但 `recovery` 仍可能继续消费广播流

### 第二步：把 `recovery` 改成脏状态消费者

- 引入 `RecoveryDirtyState`
- 把 track 脏标记与 reseed 标记下沉成显式接口
- 去掉 `recovery` 对广播流批处理的主依赖

完成后，`recovery` 将只按：

- 脏 track 集合
- reseed 标记
- anomaly retry / audit ticker

这三类输入运行。

## 验收要求

### `SubmitPreflight`

- pending submit 集合变化后，`SubmitPreflight` 不再依赖 `recovery` 通知批处理才清理本地缓存
- 启动恢复 submit、同进程重复 submit、submit success/failure/supersede 的既有测试语义保持不变

### `Recovery`

- 普通 `TrackChanged` 高频发生时，`recovery` 不再按消息条数线性放大处理成本
- `recovery` 仍能在 anomaly retry / audit 到期时正常触发交易所同步
- reseed 行为仍然正确

## 与既有文档的关系

- 本文更新了 [`2026-04-02-submit-preflight-lookup-optimization-design.md`](2026-04-02-submit-preflight-lookup-optimization-design.md) 中“runtime 通过 `recovery` 通知回读统一做 preflight 缓存删除和重算”的运行时归属。
- 本文不改变 [`2026-04-05-exchange-state-freshness-gate-design.md`](2026-04-05-exchange-state-freshness-gate-design.md) 的 freshness gate 语义；它只调整 runtime 内部任务边界与协调模型。
