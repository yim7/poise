# TUI 订单视图拆分设计

## 背景

当前 TUI 的 `Dashboard` 直接把 `execution.open_orders` 渲染为 `Open Orders`。在默认运行态和当前 Binance 行情接入模式下，这批订单并不等同于“交易所真实仍在挂着的订单”：

- `strategy.levels` 会根据当前价格生成网格层级
- 内核会把缺失层级交给执行适配层同步
- 当前 engine 默认仍挂在 `FakeExecutionAdapter`
- `FakeExecutionAdapter` 会直接构造本地 `OpenOrder` 并写回快照

这会让页面把“策略计划中的挂单镜像”显示成“真实交易所挂单”，用户很难判断：

- 策略想挂哪些单
- 当前是否真的已经挂单成功
- 当前模式是否根本没有真实交易所挂单数据

## 问题定义

当前订单展示同时混合了两层语义：

1. 策略层语义：网格层级当前希望存在什么订单
2. 执行层语义：交易所当前实际仍然挂着什么订单

只保留一张 `Open Orders` 表，会导致以下问题：

- 首页把本地镜像误读成交易所事实
- 无法区分“策略希望挂单”与“已经真实挂单”
- 在没有真实执行适配器的模式下，界面没有显式告知“当前不能证明是否已真实挂单”

## 目标

- 在 TUI 中明确拆分 `Strategy Orders` 和 `Exchange Open Orders`
- 让首页优先回答“交易所真实挂单是什么”
- 让 `Grid` 页回答“策略希望挂什么，以及当前是否能证明已挂上”
- 在没有真实交易所挂单能力时，界面必须明确说明“当前不可用”，不能继续借用策略镜像冒充真实挂单

## 非目标

- 本次不实现真实 Binance 下单、撤单和查单适配器
- 本次不把 `FakeExecutionAdapter` 替换成真实交易所执行适配器
- 本次不扩展新的命令类型
- 本次不做 Web UI 适配
- 本次不实现 level 级 `failed` 放单状态；当前系统还没有稳定的 level 级失败事实来源

## 设计摘要

本次设计采用“双视图 + 双轴状态”的方式：

- `Dashboard`
  - 主表展示 `Exchange Open Orders`
  - 只在服务端明确声明“这批订单是交易所真实挂单”时展示表格
  - 否则展示不可用说明
- `Grid`
  - 新增 `Strategy Orders` 主表
  - 展示策略层级和挂单证明状态
- `Help`
  - 明确解释 `Strategy Orders` 与 `Exchange Open Orders` 的区别

其中：

- `Strategy State` 描述策略层语义
- `Placement State` 描述是否能证明订单已经真实挂上

## 信息架构

### Dashboard

- 保留现有运行态摘要：仓位、PnL、健康度
- 原 `Open Orders` 面板改为 `Exchange Open Orders`
- 原摘要里的 `Orders` 改为 `Exchange Orders`
- 当真实挂单数据不可用时，表格不再显示策略镜像，改为说明文案

### Grid

- 保留现有网格状态、上下界、层级摘要
- 新增 `Strategy Orders` 表，作为策略层订单主视图
- 该表用于解释“策略当前想在什么价位放什么单”

### Help

- 增加术语说明：
  - `Strategy Orders`：策略层希望存在的订单目标
  - `Exchange Open Orders`：执行层确认仍在交易所挂着的真实订单
  - 若当前模式没有真实交易所挂单能力，`Exchange Open Orders` 会显示不可用说明

## 数据语义

### 1. 服务端新增订单来源标识

为 `ExecutionState` 增加字段：

```text
open_orders_source
```

枚举值：

- `exchange_live`
  - `execution.open_orders` 可被视为交易所真实挂单
- `strategy_mirror`
  - `execution.open_orders` 只是本地策略镜像，不能当作交易所事实
- `unavailable`
  - 当前模式没有可用挂单数据

设计理由：

- 这比只改前端文案更可靠
- TUI 不需要猜测当前 `open_orders` 的语义
- 后续如果接入真实执行适配器，TUI 可以无缝切到真实挂单视图

### 2. 当前模式的语义约束

本次范围内，当前几个运行模式按下面处理：

- 默认本地 bootstrap：`strategy_mirror`
- 当前 Binance 行情接入但仍使用 `FakeExecutionAdapter`：`strategy_mirror`
- 没有任何挂单镜像来源的模式：`unavailable`
- 未来真实执行适配器接入后：`exchange_live`

### 3. Strategy Orders 的数据来源

`Strategy Orders` 不新增服务端查询接口，先由 TUI 基于现有快照本地推导：

- 主来源：`strategy.levels`
- 关联依据：`level.client_order_id / level.order_id`
- 参考补充：`execution.open_orders`

这样能保证：

- 先把策略语义讲清楚
- 不把一次页面重构扩成新查询模型工程

## 双轴状态定义

### Strategy State

直接复用现有网格层级状态：

- `active`
- `occupied`
- `pending_rebuild`

### Placement State

本次实现 4 个值：

- `live`
  - 能证明存在匹配的真实交易所挂单
- `not_placed`
  - 策略希望存在挂单，但已知当前没有匹配真实挂单
- `not_expected`
  - 当前层级本来就不应该有挂单
- `unknown`
  - 当前模式无法证明是否已真实挂单

本次不实现 `failed`，原因：

- 当前系统只有整体策略同步失败日志，没有稳定的 level 级失败事实
- 强行在本次实现里加入 `failed`，会让状态来源不可靠

## Placement State 判定规则

按下面顺序判断：

1. 如果 `Strategy State = occupied`
   - `Placement State = not_expected`
2. 如果 `Strategy State = pending_rebuild`
   - `Placement State = not_expected`
3. 如果 `Strategy State = active` 且 `open_orders_source = exchange_live`
   - 若存在匹配 `client_order_id` 或 `order_id` 的真实挂单：`live`
   - 否则：`not_placed`
4. 如果 `Strategy State = active` 且 `open_orders_source != exchange_live`
   - `Placement State = unknown`

## 页面行为

### Dashboard: Exchange Open Orders

#### 当 `open_orders_source = exchange_live`

- 正常渲染表格
- 标题：`Exchange Open Orders`
- 摘要计数：`Exchange Orders`
- 摘要计数值：真实交易所挂单数量

#### 当 `open_orders_source = strategy_mirror`

- 不渲染订单表格内容
- 显示说明文案：
  - `当前模式只提供策略挂单镜像`
  - `尚未接入真实交易所挂单查询`
- 摘要计数字段仍保留 `Exchange Orders`
- 摘要计数值显示 `N/A`，不能显示 `0`

#### 当 `open_orders_source = unavailable`

- 不渲染订单表格内容
- 显示说明文案：
  - `当前模式未提供交易所挂单数据`
- 摘要计数字段仍保留 `Exchange Orders`
- 摘要计数值显示 `N/A`，不能显示 `0`

### Grid: Strategy Orders

新增表格列：

- `Side`
- `Price`
- `Qty`
- `Strategy State`
- `Placement State`

展示规则：

- 按价格排序，买单在下方价位，卖单在上方价位
- `Strategy State` 保持策略语义
- `Placement State` 按前述规则计算

### Help

新增术语说明：

- `Strategy Orders` 看的是策略目标，不是交易所事实
- `Exchange Open Orders` 只在有真实挂单数据时才展示
- 若首页显示不可用说明，表示当前运行模式不能证明策略单已真实挂出

## 文案规范

本次新增或替换文案全部改为中文，避免英文长句在窄屏下断行难读。

建议文案：

- `Exchange Open Orders`
  - 中文标题：`交易所挂单`
- `Strategy Orders`
  - 中文标题：`策略订单`
- `当前模式只提供策略挂单镜像`
- `尚未接入真实交易所挂单查询`
- `当前模式未提供交易所挂单数据`

若项目决定保留英文标题，则说明文案仍使用中文，避免整页术语中英混杂。

## 服务端改动范围

本次服务端只做最小语义补齐：

- `ExecutionState` 增加 `open_orders_source`
- bootstrap、Binance 行情 supervisor、fake 执行路径按真实语义写入来源
- 不改现有 `/runtime/snapshot` 主体结构
- 不新增订单查询 endpoint

## TUI 改动范围

- `protocol`、`state`、`selectors` 增加 `open_orders_source` 读取与派生
- `Dashboard` 订单面板改成真实挂单视图
- `Grid` 新增策略订单表
- `Help` 增加术语说明
- 替换旧的误导性订单文案

## 测试策略

### 服务端

- 协议测试：
  - `ExecutionState.open_orders_source` 编解码
- 启动路径测试：
  - 默认 bootstrap 输出 `strategy_mirror`
  - 当前 Binance 行情模式输出 `strategy_mirror`
  - `unavailable` 场景按约定输出

### TUI

- selector 测试：
  - `Placement State` 判定
  - `exchange_live / strategy_mirror / unavailable` 三种来源下的 Dashboard 视图分支
- 渲染快照：
  - `Dashboard` 真实挂单可用
  - `Dashboard` 真实挂单不可用
  - `Grid` 新增策略订单表
  - `Help` 术语说明

## 风险与取舍

### 优点

- 先把当前语义说真，立刻消除“本地镜像冒充真实挂单”的误导
- 不阻塞后续真实执行适配器接入
- 页面职责更清楚，排障顺序更自然

### 代价

- 本次完成后，`Exchange Open Orders` 在当前模式下大概率经常显示“不可用”
- 这会暴露当前执行层尚未打通真实交易所挂单查询的事实

这是预期行为。比起继续展示一个看起来很完整但语义错误的订单表，明确不可用更准确。

## 实现边界结论

本次设计聚焦一件事：把“策略订单”和“交易所真实挂单”拆开，并让页面明确表达当前能证明到哪一步。

实现完成后的用户判断应当变成：

- `Grid`：策略当前打算挂哪些单
- `Dashboard`：交易所实际上有哪些真实挂单
- 若 `Dashboard` 不可用：当前模式还不能证明策略单已真实挂出
