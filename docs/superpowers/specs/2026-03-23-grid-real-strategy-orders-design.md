# Grid 页真实策略挂单设计

## 背景

当前 Grid 页左侧的 `Strategy Orders` 表格来自 `strategy.levels`，再通过 `exchange_open_orders` 计算 `Placement`。  
这会把两种语义混在一起：

- 当前真实存在于执行层或交易所的挂单
- 已经成交后转化为库存占用的网格层

当实例存在净仓时，`strategy.levels` 会把部分档位标记为 `occupied`，即使这些档位在交易所已经没有挂单。  
这会让页面看起来像“策略单”和“交易所真实挂单”不一致。

## 目标

让 Grid 页的 `Strategy Orders` 成为真实策略挂单表。

- 页面只显示当前真实存在的策略挂单
- 表中每一行都必须能在当前实时交易所订单源中找到对应订单
- 已成交转化出的库存不再显示在这张表里
- 库存和策略生命周期继续通过 `Position`、`strategy.status`、`Grid Summary` 表达

## 非目标

- 不修改服务端策略状态机
- 不移除 `strategy.status = occupied` 的库存语义
- 不在本次引入“每个格子单独建仓账本”

## 设计决策

### 1. `Strategy Orders` 只取真实订单源

Grid 页订单表不再遍历 `strategy.levels`。

订单来源规则：

1. 只有在 `execution.exchange_open_orders_source == exchange_live` 时，才使用 `execution.exchange_open_orders`
2. 否则显示空表

这样 Grid 页只在客户端确实拿到实时交易所挂单时展示策略订单，不会把执行层镜像单重新当成真实订单。

### 2. 只显示策略管理的挂单

订单表只显示策略管理下的当前挂单，不显示手工单或危险命令产生的临时单。

本次使用以下规则识别策略挂单：

- `client_order_id` 以 `grid_` 开头，或
- 能匹配当前 `strategy.levels` 中任一层的 `client_order_id / order_id`

### 3. 表格语义改为真实订单表

Grid 页左侧表格列改为：

- `Side`
- `Price`
- `Qty`
- `Status`

其中 `Status` 显示真实订单状态，例如 `NEW`。  
`Strategy` 和 `Placement` 两列移除，因为这两列属于“目标状态”和“证明状态”语义，不适合真实订单表。

### 4. 库存语义保留在右侧摘要

`Grid Summary` 继续保留：

- `status`
- `occupied_levels`
- `inventory_bias`

这部分仍然是策略/库存视图，不与左侧真实订单表混用。

## 验收标准

1. Grid 页 `Strategy Orders` 中的每一行都来自当前实时交易所订单源
2. 已成交但已无挂单的网格层，不再出现在 `Strategy Orders`
3. 当前存在库存但没有挂单时，`Strategy Orders` 为空，`Grid Summary` 仍可显示 `occupied`
4. 当交易所实时订单源不可用时，Grid 页不回退到执行层镜像单
