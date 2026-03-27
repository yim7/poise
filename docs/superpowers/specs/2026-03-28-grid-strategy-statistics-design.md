# 网格策略统计信息设计

**日期：** 2026-03-28

**目标：** 在网格策略详情页新增稳定可解释的统计区块，第一版只展示 `总收益` 和 `累计已实现收益`，并为后续继续扩展统计项保留清晰边界。

## 背景

当前详情页已经能展示这些信息：

- 策略基础信息
- 市场价格
- 当前仓位和目标仓位
- 挂单状态
- 最近活动和可用命令

但“策略统计”仍然缺位。现状存在两个具体问题：

1. 运行时只有 `realized_pnl_today` 和 `unrealized_pnl`，没有“策略启动以来累计已实现收益”。
2. 详情页没有独立统计区块，收益信息即使补出来，也容易和状态、仓位信息混在一起。

同时，本轮讨论里已经明确排除这些方向：

- 不做年化收益
- 不做收益基准或统计本金
- 不做保证金、杠杆、仓位大小展示
- 不把交易所真实保证金或真实杠杆接入当前范围

因此这次设计目标很聚焦：先把收益统计做对，再决定是否继续扩展其他指标。

## 设计目标

### 主目标

- 新增“累计已实现收益”状态，口径稳定且可持久化
- 详情协议中新增独立 `Statistics` 区块
- 详情页新增统计展示，第一版只显示两项：
  - `Total PnL`
  - `Realized PnL`
- `Total PnL` 的定义明确为：

`累计已实现收益 + 当前未实现收益`

### 非目标

- 不改 dashboard 列表展示
- 不把收益统计接入风控逻辑
- 不修改交易所接口能力边界
- 不在这轮引入年化、收益率、保证金、杠杆、仓位大小

## 设计问题

这次真正要解决的是两个问题：

1. “累计已实现收益”应该放在哪里维护，才能在重启后保持稳定？
2. 详情页统计信息应该以什么结构暴露，才能既满足当前展示，又不把将来扩展堵死？

## 备选方案

### 方案 A：只在查询层临时拼收益

做法：

- 继续保留现有运行时状态不变
- 查询详情时从最近订单事件或最近 effect 里临时回算累计已实现收益

优点：

- 状态结构改动少

问题：

- 当前查询窗口只拿最近若干条事件，不保证覆盖策略全生命周期
- effect 和事件语义不是为“累计收益账本”设计的
- 服务重启后或窗口裁剪后，统计结果可能不完整

结论：不采用。

### 方案 B：从交易所历史成交临时回算

做法：

- 每次详情查询时调用交易所历史成交接口
- 以交易所返回结果计算累计已实现收益

优点：

- 更接近交易所账本

问题：

- 当前系统还没有这条读取链路
- 会把本轮改动扩展到外部接口、鉴权、分页、时间窗口
- 查询延迟和失败模式都会显著变复杂

结论：不采用。

### 方案 C：在运行时快照中新增累计字段，并投影到独立统计区块

做法：

- 在运行时 `RiskState` 中新增累计已实现收益字段
- 在订单收益增量回写时同步累计
- 将该字段持久化到快照和存储
- 服务端投影时组装 `statistics`
- TUI 详情页增加独立 `Statistics` 区块

优点：

- 口径稳定
- 查询简单
- 服务重启后可恢复
- 后续继续增加统计项时边界最清楚

问题：

- 需要同时修改 runtime、snapshot、storage、protocol、projector、TUI

结论：采用。

## 采用方案

采用方案 C。

核心判断：

- 收益统计是运行时事实，不是查询时的临时推导
- 详情页统计是读模型，不应继续塞进现有 `Overview`

因此这轮分成两层：

1. 写侧和状态层补齐“累计已实现收益”事实
2. 读侧和 TUI 层把这两个收益指标解释成独立统计区块

## 数据口径

### 1. 累计已实现收益

新增字段，建议命名为：

- `realized_pnl_cumulative`

定义：

- 策略启动以来，所有订单更新里 `realized_pnl` 增量的累计值

行为约束：

- 不按 UTC 日切重置
- 不由启动同步、持仓同步、挂单同步修改
- 只由订单成交收益增量累加

### 2. 当日已实现收益

继续保留现有：

- `realized_pnl_today`

它仍然只服务日内风险逻辑，不承担详情累计统计职责。

### 3. 总收益

详情投影时计算：

`total_pnl = realized_pnl_cumulative + unrealized_pnl`

这里的 `unrealized_pnl` 继续来自当前持仓观察值。

## 架构落点

### 1. 运行时和快照

主要修改：

- `engine/src/runtime.rs`
- `engine/src/snapshot.rs`

调整：

- 在 `RiskState` 中新增 `realized_pnl_cumulative`
- `snapshot()` 和 `restore_from_snapshot()` 把该字段纳入持久化边界

### 2. 收益累计入口

主要修改：

- `engine/src/manager.rs`

调整：

- 保持当前订单更新里对 `realized_pnl_today` 的日切和累加逻辑
- 同一入口新增对 `realized_pnl_cumulative` 的累加
- 启动同步和仓位同步不修改累计字段

### 3. 存储

主要修改：

- `storage/src/schema.rs`
- `storage/src/sqlite.rs`

调整：

- `grid_snapshots` 增加累计已实现收益列
- SQLite 读写逻辑同步支持新字段
- 对既有“只检查旧列存在”的初始化逻辑保持兼容，不在本轮强行升级旧表

### 4. 协议和投影

主要修改：

- `protocol/src/lib.rs`
- `server/src/projector.rs`

调整：

- 在 `GridDetailView` 中新增 `statistics`
- 第一版字段只包含：
  - `total_pnl`
  - `realized_pnl`
- `projector` 负责把内部状态解释为对外统计语义

### 5. TUI 展示

主要修改：

- `tui/src/views/instance.rs`
- `tui/tests/fixtures/grid_detail_view.json`
- 相关 WebSocket fixture

调整：

- 在 `Overview` 后新增独立 `Statistics` 区块
- 第一版先采用视觉方案 C：双列强调
- 结构上仍然保持“独立 statistics 区块”，如果后续觉得强调过强，只需要改 TUI 排版，不需要改协议和后端字段

## 详情页展示方案

第一版展示内容固定为两项：

- `Total PnL`
- `Realized PnL`

采用独立 `Statistics` 区块，不并入 `Overview`。

原因：

- `Overview` 更适合身份、状态、参考价格、仓位这类事实
- 收益统计是另一组语义，独立出来更清楚
- 后续若增加未实现收益拆分、手续费、收益率等字段，继续扩展 `Statistics` 更自然

这轮虽然先采用视觉方案 C，但结构上保留回退空间：

- 若实际 TUI 效果过于跳脱，只改 `tui/src/views/instance.rs` 的渲染方式
- `protocol` 和 `server` 层不需要改回

## 非目标边界

这轮明确不做：

- `dashboard` 列表上的收益摘要
- 基于交易所真实历史成交的账本校对
- 账户级收益汇总
- 多策略聚合统计
- 杠杆、占用保证金、仓位大小
- 颜色规则、闪烁动画、复杂视觉强调

## 测试策略

至少覆盖以下行为：

1. 订单更新带有 `realized_pnl` 增量时，会累计到 `realized_pnl_cumulative`
2. UTC 日切时，只重置 `realized_pnl_today`，不重置 `realized_pnl_cumulative`
3. 持仓同步和启动同步不会错误修改累计字段
4. 快照持久化后重新读取，累计字段保持不变
5. 详情投影里：
   - `statistics.realized_pnl = realized_pnl_cumulative`
   - `statistics.total_pnl = realized_pnl_cumulative + unrealized_pnl`
6. TUI 详情页渲染出 `Statistics` 区块和两个收益指标

## 相关文件

- `engine/src/runtime.rs`
- `engine/src/manager.rs`
- `engine/src/snapshot.rs`
- `storage/src/schema.rs`
- `storage/src/sqlite.rs`
- `protocol/src/lib.rs`
- `server/src/projector.rs`
- `tui/src/views/instance.rs`
- `tui/tests/fixtures/grid_detail_view.json`

## 后续衔接

这个设计确认后，下一步 implementation plan 应聚焦三个任务：

1. 先补 runtime / storage / projector 的失败测试和实现，打通累计收益链路
2. 再补协议 fixture 和 TUI 渲染测试，落地 `Statistics` 区块
3. 每个 task 验收通过后立即单独提交，避免和当前工作区其他改动互相污染
