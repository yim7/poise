# 网格替换门槛可观测性设计

**日期：** 2026-03-28

**目标：** 在不改变当前挂单替换门槛行为的前提下，让用户在 detail/TUI 中直接看到“为什么这次没有换单”，同时在活动流中保留原因变化记录，便于测试网联调和线上排查。

## 背景

当前 engine 已经有挂单替换门槛：

- 候选单与现有挂单按交易所步长取整后等价时，不重挂
- 同方向挂单只有在价格改善超过双边 taker 手续费加 `5 bps` 安全垫时才重挂
- 方向反转时立即换单

但这些判断只存在于 engine 内部，detail/TUI 只能看到当前 `pending_order`，看不到“为什么它被保留”。用户需要靠读代码或猜测来判断系统状态，这对联调不够友好。

## 决策

增加一个“当前替换门槛状态”到 snapshot，并在必要时发出新的 `DomainEvent`。UI 两侧都展示：

1. `Execution` 面板显示当前最新原因
2. `Activity` 保留原因发生变化时的日志

## 行为定义

### 1. 当前状态

在 runtime snapshot 中维护一个轻量字段，表示当前 `pending_order` 为什么被保留。

先支持两类原因：

- `rounded_match`
- `improvement_below_threshold { improvement_bps, threshold_bps }`

该字段只表达“当前状态”，不表达历史。

### 2. Activity 日志

在原因首次出现或原因发生变化时，追加新的 `DomainEvent`。

不在每个 tick 上重复写入同一条日志，避免活动流被同一原因刷屏。

对应消息：

- `kept pending order: candidate matches pending order after rounding`
- `kept pending order: improvement 9.0 bps < threshold 13.0 bps`

### 3. 清理时机

下列情况清空 snapshot 中的当前原因：

- 没有 `pending_order`
- 本次重算决定实际替换挂单
- grid 被 startup sync 或 order update 清掉 pending order

### 4. submit recovery anchor

submit recovery anchor 不使用这套原因字段，不产出这类门槛日志。

原因：

- recovery 是恢复链路，不是普通 live 挂单替换决策
- 混进同一可观测面板会让语义变乱

## 架构落点

### engine

修改 `engine/src/reconciler.rs` 和相关 runtime/snapshot 结构：

- `reconcile()` 返回当前替换门槛状态
- snapshot 持久化这个状态
- manager 在状态变化时产出新的 `DomainEvent`

### core

扩展 `core/src/events.rs`，加入新的 `DomainEvent` 变体用于 activity 投影。

### protocol/server

扩展 `GridExecutionView`，加入当前替换门槛说明字段。projector 负责：

- detail.execution 中投影当前状态
- activity 中把新事件投成可读消息

### tui

在 instance view 的 `Execution` 区域新增一行显示当前原因。

## 展示格式

### Execution

- `replacement gate: rounded match`
- `replacement gate: 9.0 bps < 13.0 bps`
- 无原因时：`replacement gate: -`

### Activity

- `kept pending order: candidate matches pending order after rounding`
- `kept pending order: improvement 9.0 bps < threshold 13.0 bps`

## 非目标

- 不在 dashboard 列表页增加新列
- 不把门槛参数做成配置项
- 不展示 submit recovery 的内部恢复细节
- 不增加新的 HTTP 接口

## 测试策略

至少覆盖：

1. engine 在 `rounded_match` 时写入当前原因
2. engine 在改善不足时写入包含 bps 数值的当前原因
3. engine 在挂单被替换或清空时清除当前原因
4. 相同原因重复 tick 不重复产生 activity 事件
5. projector 正确投影 execution reason 和 activity message
6. TUI instance view 正确显示 replacement gate 行

