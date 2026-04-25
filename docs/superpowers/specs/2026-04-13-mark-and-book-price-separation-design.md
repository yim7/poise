# 策略价、执行价与标记价分离设计

## 背景

当前系统把单一 `reference_price` 同时用于三件事：

- 计算 `band_status(price, config)`
- 计算 `desired_exposure(price, config)`
- 生成实际挂单价格

这在平稳场景下不明显，但在 Bybit 某些标的上会出现明显问题：

- `mark price` 仍停留在较高位置
- 实际盘口已经落到更低位置
- 如果目标仓位仍按 `mark price` 解释，策略目标本身就落在错误的价格轴上
- 如果挂单价格也继续用 `mark price`，订单会长期不成交

这次要解决的不是 `unit` 抽象，也不是市价退出链路，而是把三种价格职责分开：

- 策略价
- 执行价
- 标记价

## 最终设计

### 命名原则

- 删除泛名字 `reference_price`
- 凡是表达策略当前依据价格的字段，一律使用 `strategy_price`
- 凡是表达标记价的字段，一律使用 `mark_price`
- 凡是表达执行盘口的字段，一律使用 `best_bid / best_ask` 或 `execution_quote`

这条规则同时适用于：

- engine 运行时
- 对外协议
- read model / 投影
- 测试夹具与测试名

### 价格职责

- `strategy_price`：`book_mid = (best_bid + best_ask) / 2`
- `execution_price`：`Buy -> best_ask`，`Sell -> best_bid`
- `mark_price`：只用于风控与保护，不再决定目标仓位

本次不把下列价格引入运行时核心模型：

- `index_price`
- `last_trade_price`

### 市场数据模型

```rust
pub struct ExecutionQuote {
    pub best_bid: f64,
    pub best_ask: f64,
}

pub struct ExecutionQuoteTick {
    pub instrument: Instrument,
    pub execution_quote: ExecutionQuote,
    pub timestamp: DateTime<Utc>,
}

pub struct MarkPriceTick {
    pub instrument: Instrument,
    pub mark_price: f64,
    pub timestamp: DateTime<Utc>,
}

pub enum MarketDataTick {
    ExecutionQuote(ExecutionQuoteTick),
    MarkPrice(MarkPriceTick),
}

pub enum MarketObservation {
    ExecutionQuote { execution_quote: ExecutionQuote },
    MarkPrice { mark_price: f64 },
}
```

约束：

- `mark_price > 0`
- `best_bid > 0`
- `best_ask > 0`
- `best_bid <= best_ask`

`strategy_price` 不单独从交易所上送，而是在 engine 内部由 `execution_quote` 推导：

```rust
strategy_price = (best_bid + best_ask) / 2.0
```

这样可以把“策略如何解释盘口”这条知识放在 engine，而不是分散到每个交易所适配器里。

### 模块职责

#### exchange adapter

- 只负责提取 `mark_price`、`best_bid`、`best_ask`
- 不负责计算 `strategy_price`
- 如果拿不到有效盘口，不输出 `ExecutionQuoteTick`
- `mark_price` 和 `execution_quote` 是两类独立市场事件，不在 adapter 内部强行合并成一条 tick

#### engine runtime

- 持有最近一次 `mark_price`
- 持有最近一次 `execution_quote`
- 从 `execution_quote` 推导 `strategy_price`
- 计算内部价格执行门禁结果
- 持有最近一次成功计算出的策略快照价格
- 不把 stale 策略快照复用成当前执行目标

#### core strategy

- 只消费 `strategy_price`
- 不知道 `mark_price`
- 不知道 `best_bid / best_ask`

#### risk / protection

- 只消费 `mark_price`
- 不知道具体盘口一档
- 额外负责 `mark_price` 与 `strategy_price` 的偏离保护

#### projector / read model

- 不参与价格门禁决策
- 只把内部价格异常原因投影成 `attention_required`
- 必须让用户看得出当前展示的是正常值还是 last-known 快照

#### executor

- 只消费 `ExecutionQuote`
- `Buy` 只用 `best_ask`
- `Sell` 只用 `best_bid`

### 策略规则

下面这些逻辑全部改为围绕 `strategy_price` 解释：

- `band_status(strategy_price, config)`
- `desired_exposure(strategy_price, config)`
- 带内 / 带外判断
- `freeze / hold / flatten / terminate` 的带外触发

也就是说，价格区间和目标仓位都不再围绕 `mark_price`，而是围绕当前真实盘口中间价。

### 执行规则

所有执行单统一使用盘口一档：

- `Buy` 使用 `best_ask`
- `Sell` 使用 `best_bid`

这条规则对全部执行动作生效：

- 普通调仓
- 自动带外 `flatten`
- 手动 `Flatten`
- `Terminate`

本次不为退出动作单独发明另一套市价链路或另一套价格来源。

### 风控与保护规则

`mark_price` 不再参与目标仓位计算，但继续承担两类职责：

- 现有风险计算继续使用 `mark_price`
- 新增 `mark-book divergence guard`

偏离保护规则：

- `book_mid = strategy_price`
- 偏离度定义为 `abs(mark_price - book_mid) / mark_price`
- guard 需要显式的进入阈值和恢复阈值
- 恢复阈值必须小于进入阈值，避免来回抖动

只要 `mark_price` 与 `book_mid` 偏离过大，就认为当前盘口不适合继续自动执行。

第一版 ownership 固定为：

- `mark-book divergence guard` 归 engine 内部价格门禁模块所有
- 阈值先使用 engine 内部固定安全常量
- 不进入用户配置
- 不进入 track config
- 不进入对外协议

如果后续确认需要按 venue 或策略族区分，再单独出 spec 调整 owner 和配置边界。

### 内部价格执行门禁

价格相关的执行约束不直接写成 `attention_required`，而是先收敛成一个内部 gate：

```rust
enum SubmitPurpose {
    AutoReconcile,
    ManualRiskReduction,
}

enum PriceExecutionGate {
    Open,
    ManualRiskReductionOnly { reason: PriceExecutionBlockReason },
    NoSubmit { reason: PriceExecutionBlockReason },
}

enum PriceExecutionBlockReason {
    MissingExecutionQuote,
    MarkBookDivergence,
}
```

语义：

- `Open`
  - 允许普通自动执行
  - 允许自动改价
  - 允许 `SubmitPurpose::AutoReconcile`
  - 允许 `SubmitPurpose::ManualRiskReduction`
- `ManualRiskReductionOnly`
  - 禁止普通自动执行
  - 禁止自动改价
  - 拒绝 `SubmitPurpose::AutoReconcile`
  - 允许 `SubmitPurpose::ManualRiskReduction` 继续发送新的减风险单
- `NoSubmit`
  - 禁止任何新的 submit
  - 禁止自动改价
  - 拒绝所有 `SubmitPurpose`

第一版映射规则固定为：

- `MissingExecutionQuote -> NoSubmit`
- `MarkBookDivergence -> ManualRiskReductionOnly`

### 价格门禁下的订单处理

当 `PriceExecutionGate != Open` 时：

- 停止普通自动执行
- 不生成新的策略单
- 不做自动改价
- 撤掉现有加风险单
- 保留现有减风险单

这里的“加风险单 / 减风险单”沿用现有执行器角色语义：

- `IncreaseInventory` 视为加风险单
- `DecreaseInventory` 视为减风险单
- 对已有 working order，风险角色以订单请求的 `reduce_only` 为准：`reduce_only = true` 是减风险单，否则是加风险单。不要从 boundary `Up / Down` 方向重新推断，因为空头减仓也是 `Up` 方向买单。

对于本地仍处于 `SubmitPending`、还没有真正发到交易所的请求：

- 不继续发送新的加风险 submit
- `ManualRiskReductionOnly` 下，只有 `SubmitPurpose::ManualRiskReduction` 允许新的减风险 submit
- `NoSubmit` 下，不继续发送任何新的减风险 submit
- 被 gate 挡住的 pending submit 由 submit recovery 单点处理
- 第一版统一语义是：被挡住的 pending submit 直接 supersede，等待后续恢复时重新 reconcile 生成新 effect
- effect worker 不直接决定 blocked pending submit 的后果

### 手动 `Flatten` / `Terminate` 例外

`PriceExecutionGate = ManualRiskReductionOnly` 时，`SubmitPurpose::ManualRiskReduction` 保留一个例外：

- 手动 `Flatten`
- 手动 `Terminate`

允许规则：

- 只能发新的减风险单
- 必须存在有效 `execution_quote`
- 仍然按盘口一档定价

禁止规则：

- 如果 `PriceExecutionGate = NoSubmit`，任何新单都不发
- 不允许借 `Flatten` / `Terminate` 发送加风险单

命令语义本身不变：

- `Flatten` 仍然把目标锁到 `0`
- `Terminate` 仍然进入终态

只是它们的实际下单仍然依赖可用盘口。

`PriceExecutionGate` 的权限解释也固定为单点规则：

- submit 是否允许，只能由 gate 模块统一判断
- 自动改价 / 自动 replacement 是否允许，也只能由 gate 模块统一判断
- executor、recovery、effect worker 不重新解释 `Open / ManualRiskReductionOnly / NoSubmit` 的权限矩阵
- pending submit 被 gate 挡住后的 effect 生命周期，由 submit recovery 单点拥有

### 缺少盘口时的策略行为

如果 `execution_quote = None`，则当前没有可用 `strategy_price`。

此时：

- 不再用 `mark_price` 代替 `strategy_price`
- 不再重算新的 `band_status`
- 不再重算新的 `desired_exposure`
- 保留最近一次成功计算出的 `strategy_price`
- 这个保留值只表示 last-known 策略快照价格，不表示当前仍然有效的策略输入
- 当前执行目标仍然维持原有语义：`desired_exposure` 只表示最后一次成功 reconcile 产生的执行目标

这样可以避免在“策略主价格不可用”时偷偷回退到另一条价格轴，也避免把 stale 策略快照和当前执行目标混成同一个字段。

### 历史快照迁移

已有 `reference_price` 快照不具备新模型下的明确语义，因此迁移时：

- 不把旧 `reference_price` 直接伪装成 live `strategy_price`
- 不把旧 `reference_price` 直接伪装成 live `mark_price`
- 迁移后的 `strategy_price` 视为不可用并标记 `stale`
- 迁移后的 `mark_price / best_bid / best_ask` 置空
- 如果历史表里已经有 `price_execution_block_reason`，迁移时原样保留，不重新推导新的 gate reason

直到新的市场观测进入后，轨道才重新获得 live 价格语义。

对运行时快照还需要额外兼容一条旧格式恢复规则：

- 如果旧快照缺少 `price_execution_block_reason`，恢复时根据 `mark_price / best_bid / best_ask` 做一次 gate 推导
- 如果连盘口都没有，则恢复成 `MissingExecutionQuote`
- 这条兼容只存在于 restore 边界，不重新引入 `reference_price`

### 恢复规则

只有同时满足下面条件，轨道才能退出这次价格相关的 gate：

- 有效盘口恢复
- `mark-book divergence guard` 回到恢复阈值以内

恢复后：

- 重新根据最新 `strategy_price` 计算 `band_status`
- 重新根据最新 `strategy_price` 计算 `desired_exposure`
- 再恢复普通自动执行
- 如果恢复前有因为 price gate 被 supersede 的 submit，需要通过这次新的 reconcile 重新生成 effect，而不是复用旧 pending effect

## 交易所数据来源

### Binance

- `mark_price`：`<symbol>@markPrice` 的 `p`
- `best_bid`：`<symbol>@bookTicker` 的 `b`
- `best_ask`：`<symbol>@bookTicker` 的 `a`

本次不接：

- `index price`
- `last trade price`

### Bybit

- `mark_price`：`tickers.{symbol}` 的 `markPrice`
- `best_bid`：优先使用 `tickers.{symbol}` 的 `bid1Price`
- `best_ask`：优先使用 `tickers.{symbol}` 的 `ask1Price`

如果 Bybit ticker 流在实现中无法稳定提供一档盘口，再退到：

- `orderbook.1.{symbol}` 的买一 / 卖一

本次不接：

- `indexPrice`
- `lastPrice`
- `publicTrade`

## 对外语义

运行时和投影的价格语义改成：

- `status.strategy_price`：最近一次成功计算出的策略价
- `status.strategy_price_status`：`live | stale`
- `market.mark_price`：当前标记价
- `market.index_price`：删除

详情视图应补充可观测的一档盘口：

- `market.best_bid`
- `market.best_ask`

这样用户能直接区分：

- 策略当前依据的价格
- 风控当前依据的价格
- 执行当前依据的价格

价格相关 gate 的对外投影规则：

- `PriceExecutionGate != Open` 时，`execution.execution_status = attention_required`
- detail 必须暴露明确 attention reason
- 第一版 attention reason 文案固定为：
  - `MissingExecutionQuote -> "missing execution quote"`
  - `MarkBookDivergence -> "mark/book divergence"`
- 当 `status.strategy_price_status = stale` 时，reason 必须能说明当前策略价不可重新计算

## 非目标

- 不改成市价单链路
- 不引入 `index_price`
- 不引入 `last_trade_price`
- 不重写 `Exposure` / `unit` 抽象
- 不把 `notional_per_unit` 改成动态口径

## 验收

至少补齐下面几类验收测试：

1. 市场数据适配
   - Binance 能正确解析 `mark_price`
   - Binance 能正确解析 `best_bid / best_ask`
   - Bybit 能正确解析 `mark_price`
   - Bybit 能正确解析 `best_bid / best_ask`
   - 无效盘口会被映射成 `execution_quote = None`

2. 策略价格
   - `strategy_price` 由 `(best_bid + best_ask) / 2` 推导
   - `band_status` 使用 `strategy_price`
   - `desired_exposure` 使用 `strategy_price`
   - 缺少盘口时不回退到 `mark_price`
   - 缺少盘口时，`strategy_price` 保留 last-known 值并标记 `stale`
   - 缺少盘口时，不把 stale `strategy_price` 重新写回新的 `desired_exposure`

3. 执行价格
   - `Buy` 取 `best_ask`
   - `Sell` 取 `best_bid`
   - 普通调仓、`Flatten`、`Terminate` 使用同一规则

4. 标记价保护
   - 风控继续使用 `mark_price`
   - `mark-book divergence guard` 会触发价格 gate
   - 偏离回到恢复阈值内后能恢复

5. 内部价格门禁
   - `MissingExecutionQuote -> NoSubmit`
   - `MarkBookDivergence -> ManualRiskReductionOnly`
   - `Open / ManualRiskReductionOnly / NoSubmit` 对 `SubmitPurpose` 的权限矩阵正确

6. 价格异常期间的执行
   - 停止普通自动执行
   - 不做自动改价
   - 加风险单会被撤掉
   - 减风险单会被保留
   - `SubmitPending` 会按 gate 规则处理

7. 人工减风险例外
   - `ManualRiskReductionOnly` 下，手动 `Flatten` 可以继续发减风险单
   - `ManualRiskReductionOnly` 下，手动 `Terminate` 可以继续发减风险单
   - `NoSubmit` 下，手动 `Flatten` / `Terminate` 不会发出任何新单

8. 投影
   - `status.strategy_price` 表达最近一次成功计算出的策略价
   - `status.strategy_price_status` 明确标记 `live | stale`
   - `market.mark_price` 表达真实 `mark_price`
   - `market.index_price` 被移除
   - detail 暴露 `best_bid / best_ask`
   - 价格 gate 会投影成 `attention_required` 与明确原因
