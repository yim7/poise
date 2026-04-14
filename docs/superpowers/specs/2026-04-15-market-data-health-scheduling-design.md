# Market Data Health 调度设计

## 背景

在 [Recovery 协调与读侧广播分离设计](2026-04-14-recovery-submit-preflight-decoupling-design.md) 落地之后，`recovery notification stream lagged` 的主要根因已经处理掉了，但当前 runtime 里还有一条独立的问题：

- `server/src/runtime/reconcile.rs` 每 `50ms` 遍历所有 track
- 对每个 track 调一次 `refresh_market_data_health()`
- 这条逻辑和 recovery anomaly retry / audit 共用同一个后台任务

现状可以工作，但有两个明显问题：

1. `market data stale` 是时间驱动的健康维护，不是 recovery 协调职责
2. `50ms` 全量 sweep 在语义上过粗，在 track 数量增加后会引入持续的无效负载

当前还有一条已有测试锁住的业务要求：

- 即使后续没有新的 tick，也必须在超过 `tick_timeout_secs` 后把 `market_data_stale_since` 标出来

这说明“后台持续检查 market data 新鲜度”这个能力本身需要保留；需要调整的是它的归属和调度方式。

## 问题定义

本次设计要解决的是：

> runtime 应该怎样在不依赖 `50ms` 全量轮询的前提下，按时触发 `refresh_market_data_health()`？

这里有三个约束：

1. `market_data_stale` 的判定规则应继续由 `engine/application` 拥有
2. `recovery` 不应继续承担 market data health 的后台调度
3. 现有 `ClockPort` 只有 `now()`，没有可等待的逻辑时钟接口，测试里也依赖可跳变的 `MutableClock`

## 已确认事实

### `market data stale` 是时间驱动，不是消息驱动

当前 stale 判定逻辑在 `TrackManager::refresh_market_data_health()` 内部：

- 读取 `last_tick_at`
- 读取 `tick_timeout_secs`
- 如果 `now - last_tick_at` 超过超时阈值，则写入 `market_data_stale_since`

因此这类状态变化不是由某条新消息直接触发，而是由“时间流逝到某个截止点”触发。

### 这条逻辑不应该继续挂在 recovery worker 里

recovery worker 当前真正负责的是：

- recovery anomaly 跟踪
- anomaly retry
- 周期性 audit 对账

`refresh_market_data_health()` 不属于这三类职责。把它继续留在 recovery 主循环里，会让 future reader 误以为 market data stale 是 recovery 的一部分。

### 不能直接把逻辑时钟 deadline 交给 Tokio `sleep_until`

现有 runtime 和测试都通过 `ClockPort::now()` 取时间；测试里常用 `MutableClock` 直接把逻辑时间往前跳。

如果调度器直接把某个 `DateTime<Utc>` 转成 Tokio 实时时钟上的 `sleep_until`：

- 生产环境通常没问题
- 但测试里仅修改 `MutableClock` 不会唤醒 Tokio timer

所以设计不能假设“逻辑时间 deadline 等于 Tokio wall-clock deadline”。

### 当前 runtime 里的 track 集合与 tick timeout 视为静态

本设计默认以下约束成立：

- runtime 生命周期内不会热新增或热删除 track
- `tick_timeout_secs` 不会在 runtime 运行中动态修改

这两个假设与当前代码现状一致。若未来支持：

- 热增删 track
- 动态调整 tick timeout

则需要把对应变化源也接入 `market_data_health_task` 的 dirty 重算入口。

## 目标

- 把 market data health 调度从 `recovery` 任务中拆出
- 去掉 `50ms` 的全量 track sweep
- 保持“无后续 tick 也会在超时后标 stale”这个行为
- 让 stale 判定规则继续隐藏在 `engine/application` 内部
- 让 runtime 在 track 数量增长时，后台成本主要与“脏 track 数量 + 到期 track 数量”相关，而不是与“全部 track × 固定频率”相关

## 非目标

- 不改变 `market_data_stale_since` / `strategy_price_status` 的业务语义
- 不改变 websocket / read model 的对外协议
- 不重做 market data 订阅模型
- 不在本次设计里引入新的逻辑时钟异步接口

## 现状中的设计问题

### 1. recovery 任务承担了错误的职责

当前 `recovery` 主循环除了处理 anomaly retry / audit，还负责：

- 定时刷新 market data health

这让一个本应围绕“交易所状态恢复”的后台任务，同时承担了“行情新鲜度维护”的职责。后续任何一侧调度需求变化，都容易误伤另一侧。

### 2. 固定频率全量 sweep 的成本和业务需求不匹配

`market_data_stale` 的超时阈值是秒级，而当前 sweep 频率是 `20Hz`。这意味着系统在空闲时也会持续做：

- 遍历全部 track
- 逐个进入写路径
- 逐个判断是否还没到 stale 时间

这条路径的绝大多数调用都会返回 no-op。

### 3. stale 判定知识不能上浮到 server

如果 server 直接根据：

- `last_tick_at`
- `tick_timeout_secs`

自行决定什么时候一定 stale，就把 stale 规则复制到了调度层。

今天的规则虽然只是简单超时，但未来如果 stale 语义增加其他门控，server 调度层就会和 engine 规则分叉。

## 备选方案

### 方案 A：保留 recovery 内的 sweep，只把间隔放慢

做法：

- 继续在 recovery 主循环里调 `refresh_market_data_health()`
- 把 `50ms` 改成 `500ms` 或 `1s`

优点：

- 改动最小

缺点：

- 错误的职责边界保留不变
- 仍然是固定频率全量 sweep
- 只是降低成本，不是改变模型

结论：

- 不采用

### 方案 B：拆成独立任务，但仍做固定频率全量 sweep

做法：

- 新增 `market_data_health_task`
- 周期性遍历全部 track，调用 `refresh_market_data_health()`

优点：

- 至少把职责从 recovery 里拆出来了
- 实现直接

缺点：

- 调度模型仍然粗糙
- track 数量增长时，后台成本仍然和全量 sweep 绑定
- “明明只有少数 track 接近超时，却不断扫所有 track”的问题依旧存在

结论：

- 只比现状好一层，不采用

### 方案 C：deadline-driven 调度器，server 只调度，application 继续拥有规则

做法：

- `application` 暴露一个窄查询接口：
  - `market_data_health_deadline(track_id) -> Option<DateTime<Utc>>`
- server 继续直接持有 `ClockPort`
- server 新增独立 `market_data_health_task`
- task 在启动时为所有 track seed 初始 deadline
- market data worker 在成功写入新 tick 后，只标记该 `track_id` 为 dirty
- health task 收到 dirty 后，只为 dirty track 重新查询 deadline
- health task 到达最近 deadline 时，只对到期 track 调用 `refresh_market_data_health()`
- 由于 `ClockPort` 仅提供 `now()`，task 的等待策略分两种：
  - 没有任何 deadline 时，只等待 dirty notify / shutdown
  - 有最近 deadline 时，使用 `min(最近 deadline 与当前逻辑时间的差值, max_sleep_interval)` 做 bounded sleep
  - 醒来后再次用逻辑时钟判断是否到期

优点：

- recovery 和 market data health 职责分离
- server 不再全量扫全部 track
- stale 规则仍然由 `engine/application` 拥有
- `ClockPort` 仍停留在 runtime 基础设施边界，不被抬进 application
- 即使 market subscription 失败，只要 startup seed 里有有效 deadline，health task 仍能按时把 track 标 stale
- `ClockPort` 的测试约束被显式吸收进调度器，不需要重做整个时钟抽象

缺点：

- 需要新增一个 application-owned 的 deadline 查询接口
- server 需要维护一份内部 deadline 调度状态

结论：

- 采用

## 最终设计

采用 **方案 C：独立的 deadline-driven market data health 调度器**。

### 核心原则

- stale 判定规则继续隐藏在 `engine/application`
- server 只拥有“何时重新询问下一次 deadline”和“何时触发 refresh”这层调度知识
- market data health 调度与 recovery 调度彻底分离
- 不再有 `50ms` 的全量 track sweep

## 模块边界

### `engine` / `application`

负责：

- 根据当前 track runtime 状态给出“下一次可能需要做 market data health 检查的逻辑 deadline”
- 在 `refresh_market_data_health()` 内继续判断是否真的要把 track 标 stale

不负责：

- 维护后台任务
- 管理 deadline 队列

建议新增的窄接口形状：

```rust
pub async fn market_data_health_deadline(
    &self,
    id: &str,
) -> Result<Option<DateTime<Utc>>>;
```

语义：

- `Some(deadline)` 表示该 track 在这个逻辑时间点之后需要重新检查 health
- `None` 表示当前没有待检查 deadline

返回 `None` 的典型场景：

- 从未收到 tick
- 已经处于 stale，直到下一笔 tick 清掉 stale 之前都不需要再次检查

### `server::runtime::market_data`

继续负责：

- 订阅价格流
- 把 tick 写入 `observe_market(...)`

新增负责：

- tick 成功写入后，标记该 `track_id` 的 market data health deadline 需要重算

它不负责：

- 自己判断 stale
- 自己管理 deadline heap

### `server::runtime::market_data_health`

新增独立任务，负责：

- 启动时 seed 所有 track 的 deadline
- 消费内部 `dirty_tracks`
- 在 task 局部维护 deadline 调度状态
- 在 track 到期时调用 `refresh_market_data_health()`
- refresh 后重新查询该 track 的下一次 deadline
- 持有 runtime 注入的 `ClockPort`、`MarketDataHealthState` 和 `max_sleep_interval`

它不负责：

- 消费 websocket/UI 广播
- 管理 recovery anomaly

### `server::runtime::reconcile`

不再负责：

- `refresh_market_data_health()` 的后台调度

只保留：

- recovery dirty state
- anomaly retry
- audit 对账

## 调度模型

### 内部状态

新增 `MarketDataHealthState`，建议最小接口：

- `mark_dirty(track_id)`
- `take_dirty() -> HashSet<TrackId>`
- `wait().await`

内部可以用：

- `HashSet<TrackId>` 保存 dirty tracks 和通知状态

deadline 索引是 `market_data_health_task` 的私有实现细节，不放进共享状态对象。初版可以在 task 内部用：

- `HashMap<TrackId, DateTime<Utc>>` 保存当前 deadline

本次设计不要求一开始就上最复杂的 heap；如果用 `HashMap` 扫最早 deadline 已经足够简单且只在 task 私有状态内使用，也可以接受。关键是：

- 不再按固定频率扫描全部 track 并写路径
- 只在 dirty 变化或 deadline 到达时工作

### task 主循环

1. 启动时：
   - 通过 `track_instruments()` 拿到全部 `track_id`
   - 逐个调用 `market_data_health_deadline(track_id)` seed 当前 deadline

2. 正常循环：
   - 先处理 dirty tracks，重算其 deadline
   - 通过 runtime 持有的 `ClockPort::now()` 读取当前逻辑时间
   - 读取当前最近 deadline
   - 如果没有任何 deadline，则只等待：
     - shutdown
     - dirty notify
   - 如果存在最近 deadline，则等待：
     - shutdown
     - dirty notify
     - bounded sleep

3. bounded sleep 规则：
   - 如果有最近 deadline，则令：
     - `remaining = deadline - clock.now()`
     - `sleep_for = min(max(remaining, 0), max_sleep_interval)`
   - sleep 结束后重新读取 `clock.now()`，判断哪些 track 已到期

4. 对到期 track：
   - 调 `refresh_market_data_health(track_id)`
   - 再重新查询该 `track_id` 的 deadline

### 为什么需要 `max_sleep_interval`

这是当前 `ClockPort` 抽象下必须显式保留的一层保护。

原因是：

- 逻辑时间可能在测试里瞬间跳变
- Tokio timer 不会因为 `MutableClock` 改变而提前醒来

因此 scheduler 在“已有至少一个 deadline”时，不能无限期地只睡到某个 wall-clock 时间点，而要保留一个有上限的重新检查周期。

这个上限不再是“每 `50ms` 全扫全部 track”，而只是：

- 一个独立 task 的轻量级唤醒周期
- 每次醒来只处理 dirty 或到期 track

默认建议：

- 生产默认 `max_sleep_interval = 1s`
- 测试可通过 runtime fixture 降低到 `50ms` 或更小

## 数据流

### 新 tick 到来

1. market task 收到 tick
2. 调 `observe_market(track_id, tick)`
3. 如果写入成功：
   - `market_data_health_state.mark_dirty(track_id)`
4. health task 被唤醒
5. 重新查询该 track 的 deadline

### 长时间无 tick

1. health task 持有某个 track 的 deadline
2. deadline 到达后，调用 `refresh_market_data_health(track_id)`
3. engine/application 决定是否真的写入 stale
4. 刷新后再次查询 deadline
5. 如果已 stale，则通常返回 `None`

### 重启恢复

1. runtime 启动
2. health task 从已有 snapshot seed deadline
3. 即使后续 market subscription 失败，只要 snapshot 里已有 `last_tick_at`，也仍能在 deadline 到达后把 track 标 stale

## 错误处理

- 查询 deadline 失败：
  - 打 warning
  - 把该 track 的下一次检查时间设为 `clock.now() + max_sleep_interval`
  - 让 task 在下一次 bounded wake 时重试 deadline 查询

- `refresh_market_data_health()` 失败：
  - 打 warning
  - 把该 track 的下一次检查时间设为 `clock.now() + max_sleep_interval`
  - 让 task 在下一次 bounded wake 时继续重试 refresh

- market tick 写入失败：
  - 不标 dirty
  - 因为 track 状态没有可靠改变

## 测试策略

至少补齐以下验收测试：

1. 从 recovery 任务中删除 health check 后，原有 recovery 测试不再依赖 `refresh_market_data_health()`
2. `background_health_check_marks_market_data_stale_without_follow_up_events` 仍然通过
3. 有 deadline 的 track 在无后续 tick 时会被按时标 stale
4. 新 tick 到来后会重置 deadline，不会沿用旧截止时间
5. market subscription 失败但 snapshot 已有 `last_tick_at` 时，health task 仍能按时标 stale
6. health task 空闲时不会对全部 track 做固定频率写路径调用

## 迁移步骤

1. 新增 `market_data_health_deadline()` 查询接口
2. 新增 `MarketDataHealthState` 与独立 task
3. 让 market task 在 `observe_market()` 成功后标 dirty
4. 将 `refresh_market_data_health()` 从 recovery 主循环中删除
5. 更新 runtime tests，覆盖 startup seed、deadline 重算和无 tick stale 场景

## 与既有设计的关系

- 本文是对 [Recovery 协调与读侧广播分离设计](2026-04-14-recovery-submit-preflight-decoupling-design.md) 的后续补充
- 前一份设计解决的是“recovery 不应消费通用读侧广播”
- 本文解决的是“market data health 不应继续挂在 recovery 任务里，也不应依赖 50ms 全量轮询”
