# K5 网格策略与风控设计

## 背景

当前 `service` 已有命令闭环、持久化恢复、Binance 行情与用户态同步，以及 `tui` 基础页面。
但 `Grid` 页面仍然是从 `execution.open_orders` 反推的展示，服务端没有独立的策略状态机，风险态也只有摘要字段，没有统一触发与广播路径。

K5 的目标是让 `service` 真正承载网格策略与风控模型，并让 `tui` 显示真实策略状态。

## 设计目标

- 在 `RuntimeSnapshot` 中新增独立 `strategy` 子状态，明确网格配置、层级状态与重建状态。
- 服务端以内核单写者路径维护策略与风控，不让客户端或外部适配层直接拼策略状态。
- 策略输出先形成执行计划，再同步到执行视图，避免把订单镜像当成策略真相。
- 风险阈值、breaker 与风险事件走统一评估入口，可持久化、可查询、可通过 WebSocket 广播。
- `tui` 的 `Grid`、`Dashboard`、`Events` 页面改为消费策略与风险模型，而不是从订单列表推断。

## 协议模型

`RuntimeSnapshot` 新增 `strategy` 字段，包含：

- `config`
  - `spacing_bps`
  - `levels_per_side`
  - `quantity_per_level`
  - `max_position_qty`
  - `rebuild_threshold_bps`
- `status`
  - `active`
  - `occupied`
  - `pending_rebuild`
- `center_price`
- `lower_bound`
- `upper_bound`
- `rebuild_reference_price`
- `pending_rebuild_reason`
- `levels`

单个 `level` 包含：

- `level_id`
- `side`
- `price`
- `quantity`
- `state`
  - `active`
  - `occupied`
  - `pending_rebuild`
- `client_order_id`
- `order_id`

现有 `risk` 摘要字段保留，用于兼容旧客户端；风险事件继续通过 `/risk/events` 与 `risk_alert` 事件暴露。

## 服务端设计

### 策略状态机

内核新增 `strategy` 模块，负责：

- 校验网格配置
- 生成对称买卖层级
- 根据当前仓位把部分层级标记为 `occupied`
- 在价格偏离 `rebuild_reference_price` 超过阈值时进入 `pending_rebuild`
- 在满足条件时重建层级并更新中心价

状态规则如下：

- 无仓位、无重建需求、无 breaker 时为 `active`
- 有仓位但仍可继续运行时为 `occupied`
- 触发重建但因仓位未归零或 breaker 打开而不能立即重建时为 `pending_rebuild`

### 执行联动

策略模块不直接改 `execution.open_orders`。
它先产出一个执行计划，只包含当前应挂出的 `active` 层级；内核再把执行计划同步到 `execution` 读模型。

联动规则如下：

- `active` 层级映射为挂单视图
- `occupied` 层级不再出现在挂单视图中
- `pending_rebuild` 或 breaker 打开时，不再生成新的网格挂单

### 风控模型

新增 `risk` 模块，统一评估以下条件：

- 最大仓位
- 止损阈值
- 单日亏损限制
- breaker 触发与解除

评估入口放在价格同步、仓位同步和执行结果落地之后。

风险评估结果包括：

- 更新 `risk` 摘要字段
- 写入新的 `RiskEvent`
- 在新事件产生时广播 `risk_alert`
- 按 breaker 状态影响策略执行计划

为避免事件重复刷屏，只在风险状态发生变化或首次命中某个规则时追加事件。

## 持久化

当前实现不额外新增 `grid_levels` 表。
SQLite 继续以 `runtime_snapshots.snapshot_json` 持久化完整 `strategy` 状态，用于恢复时直接重建策略视图。

恢复顺序保持不变：

1. 先加载 `RuntimeSnapshot`
2. 恢复 `risk_events` 与 `system_events`
3. 启动后由内核继续评估策略与风控

## TUI 设计

### Grid 页面

- `Active Grid Levels` 改为展示真实策略层级
- `Grid Summary` 展示策略状态、中心价、区间、层级数量、待重建原因
- `Operator Notes` 展示当前会话、breaker、重建建议

### Dashboard

- 风险面板展示核心阈值、当前占用、breaker 状态
- 策略摘要展示 `active / occupied / pending_rebuild`

### Events

- `Alerts` 面板显示风险事件代码、说明和建议动作
- 风险事件仍与命令、系统事件分开展示

## 测试策略

按 TDD 分三轮实现：

1. 协议与内核测试
   - `strategy` 字段合同测试
   - 配置校验测试
   - 状态流转与重建测试
   - 风险阈值与 breaker 测试
   - 风险事件广播测试
2. 持久化与恢复测试
   - `strategy` 的 snapshot roundtrip
   - breaker 与风险事件恢复
3. TUI 测试
   - selector 状态映射测试
   - `Grid`、`Dashboard`、`Events` 快照回归

## 非目标

- 本次不拆新的 workspace crate
- 本次不引入真实策略下单到 Binance
- 本次不做在线参数编辑
