# 网格订单替换门槛设计

**日期：** 2026-03-28

**目标：** 保持 `LIMIT` 挂单策略不变，但避免价格窄幅波动时频繁撤旧单、重挂新单；只有当新的挂单方案相对旧挂单的经济改善足以覆盖交易所固定双边手续费和 `5 bps` 安全垫时，才允许替换。

## 背景

当前系统会在每个价格 tick 上重算。虽然存在 `pending_order target` 匹配时的 `NoOp` 抑制，但带内 `desired_exposure` 是连续函数，轻微价格波动就可能让目标发生细小变化，进而触发撤旧换新。

这会带来两个问题：

1. 订单管理层面出现高频改挂，增加系统噪声。
2. 即使挂单本身不收费，未来若成交，过小的价格改善也未必能覆盖双边手续费和策略安全垫。

## 决策

采用“挂单替换门槛”，不引入时间防抖，不改 `LIMIT` 语义，也不把手续费和安全垫暴露成配置项。

### 固定参数

- 交易所：Binance
- 费率口径：固定双边手续费率
- 安全垫：`5 bps`

## 行为定义

### 1. 没有 `pending_order`

维持现状，直接根据当前重算结果生成 `SubmitOrder`。

### 2. 有 `pending_order`，且候选订单与现有挂单等价

直接 `NoOp`，不撤单、不重挂。

“等价”的定义按交易所规则比较：

- `instrument` 相同
- `side` 相同
- `price` 按 `price_tick` 取整后相同
- `quantity` 按 `quantity_step` 取整后相同

这里不再依赖 `desired_exposure` 的浮点完全相等。

### 3. 有 `pending_order`，但新旧方向相反

立即替换，保留现有 `CancelAll + SubmitOrder` 语义。

原因：方向反转代表策略观点改变，不能继续保留旧挂单。

### 4. 有 `pending_order`，新旧方向相同

仅当新单相对旧单的价格改善足够大时，才允许替换。

价格改善定义：

- `BUY`：`old_price - new_price`
- `SELL`：`new_price - old_price`

门槛定义：

`price_improvement / reference_price >= replacement_threshold_rate`

其中：

`replacement_threshold_rate = binance_fixed_round_trip_fee_rate + 5 bps`

若未达到门槛，则 `NoOp`，继续保留旧挂单。

### 5. 新候选订单低于交易所最小门槛

维持现有语义：

- 若已有旧挂单，则执行 `CancelAll`
- 若没有旧挂单，则 `NoOp`

## 架构落点

主要修改 `engine/src/reconciler.rs`。

原因：

- 这里同时持有当前 `pending_order`、重算后的候选 `price/quantity`、以及交易所 `ExchangeRules`
- 这是决定是否生成 `CancelAll + SubmitOrder` 的唯一合理边界

支持性辅助函数可以放在 `reconciler.rs` 内部，保持变更集中。

Binance 固定手续费率常量放在 engine 可消费的位置，避免让 reconciler 依赖 exchange adapter 细节。当前只有 `binance` 一个 venue，可先以 `Venue::Binance` 分支实现。

## 非目标

- 不引入时间窗口防抖
- 不引入新的配置项
- 不修改 submit recovery 流程
- 不改为 `IOC` / `FOK` / `MARKET`

## 测试策略

至少覆盖以下行为：

1. 候选订单与现有挂单按交易所步长等价时，返回 `NoOp`
2. 同方向但价格改善不足 `双边手续费 + 5 bps` 时，返回 `NoOp`
3. 同方向且价格改善超过门槛时，返回 `CancelAll + SubmitOrder`
4. 方向反转时，返回 `CancelAll + SubmitOrder`
5. 没有 `pending_order` 时，维持现有提交行为

