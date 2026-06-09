# OKX 币本位反向合约支持 Spec

## 背景

Poise 是一个用价格曲线管理库存的系统。用户可以把这个库存管理能力用于持币对冲、攒币或其他目的，但系统本身不建模用户持有多少现货，也不引入 hedge ratio、现货仓位或对冲目标。

当前 OKX 适配器只按 `SWAP` 接口接入了永续合约，但内部数量语义仍偏向 U 本位线性合约：engine 多处假设 `Position.qty` 可以直接按标的币数量解释，并用 `notional_per_unit / band_center` 得到每个 exposure unit 的数量。OKX 币本位反向合约不满足这个假设。

以 OKX `BTC-USD-SWAP` 为例，公共 instruments 接口返回：

```text
ctType = inverse
ctVal = 100
ctValCcy = USD
settleCcy = BTC
lotSz = 0.1
minSz = 0.1
```

含义是：

```text
1 张合约 = 固定 100 USD 面值
下单、挂单、成交、持仓数量 = 合约张数
PNL、手续费、资金费结算资产 = BTC
```

因此币本位支持不是简单允许 `symbol = "BTC-USD-SWAP"`，而是要让系统清楚区分：

- 策略库存单位 `Exposure`
- 交易所原生库存数量 `native quantity`
- USD 名义价值
- BTC 结算损益

参考：

- OKX 公共接口：`https://www.okx.com/api/v5/public/instruments?instType=SWAP&instId=BTC-USD-SWAP`

## 目标

- 支持 OKX 币本位反向永续合约，例如 `BTC-USD-SWAP`。
- 保持系统核心定位：价格曲线管理库存，不内置对冲现货的用户目标。
- 保持 `notional_per_unit` 和 `max_notional` 的 USD 面值口径，用来决定每个 exposure unit 分配多少合约规模。
- 让币本位反向合约的 `Exposure` 基于稳定的合约张数计算，价格变化不会导致当前 exposure 无交易漂移。
- OKX adapter 根据 instrument metadata 处理 `ctType`、`ctVal`、`ctValCcy` 和 `settleCcy`。
- 币本位 inverse 下单、持仓、挂单、成交回报使用合约张数。
- PNL、手续费、资金费保留结算资产语义；`BTC-USD-SWAP` 的原始结算资产是 BTC。
- 币本位的损益展示和每日止损默认使用结算币种，例如 BTC；不能把 BTC 结算损益和 USD 面值配置作为同一类无单位数字直接相加。

## 非目标

- 不新增 hedge ratio、现货持仓、目标持币数量等用户目的层配置。
- 不改变现有区间外行为；带外仍由 `out_of_band_policy` 控制。
- 不把币本位仓位按当前价格折算成 BTC 数量后再计算 `current_exposure`。
- 不新增独立数量模型 service；数量语义放在现有 `ExchangeRules` 边界上。
- 不新增 loss limit 资产配置；loss limit 自动跟随 instrument 的结算资产。
- 不在第一版中接入 OKX `max-size`；账户容量先基于账户可用资产本地估算。
- 不把 OKX U 本位 SWAP 的张数语义暴露到 core / engine；如果 OKX API 返回张数，由 adapter 内部转成标的币数量。
- 不接入 OKX spot、交割合约、期权、划转、提现或资金账户操作。
- 不在第一版中支持多交易所币本位通用化；本 spec 聚焦 OKX `SWAP`。

## 主导复杂度信号

主导复杂度是 `cognitive load` 和 `change amplification`。如果数量单位继续以裸 `f64` 在 core、engine、exchange adapter、projector 和 storage 间传递，后续读者必须记住每个字段在不同交易所下到底是 BTC、合约张数、USD 还是结算币损益。

设计选择是把数量语义下沉到现有 `ExchangeRules` 边界。交易所 adapter 拥有 OKX 字段知识；engine 只通过 `ExchangeRules` 的少量方法把 `native quantity`、`Exposure` 和名义价值互相换算。第一版避免新增独立 service 或多层 DTO。

## 术语

### Exposure

`Exposure` 仍然是策略库存单位，不是 BTC 数量，也不是交易所张数。

统一公式：

```text
current_exposure = native_position_qty / native_qty_per_exposure_unit
target_native_qty = target_exposure * native_qty_per_exposure_unit
```

### Native quantity

`native quantity` 是交易所持仓和下单使用的稳定库存单位。

不同合约的 native quantity 不同：

```text
Binance / Bybit U 本位线性合约:
native quantity = 标的币数量，例如 BTC

OKX 币本位反向合约:
native quantity = 合约张数
```

如果某个交易所的 U 本位 API 原始返回合约张数，但张数与标的币数量是固定比例关系，adapter 应在内部转换成标的币数量再交给 engine。U 本位在 core / engine 层不暴露张数概念。

### USD notional

`notional_per_unit` 和 `max_notional` 继续表示 USD 名义价值。

这是配置层的规范尺寸，不是要求用户只能用 USD 理解币本位。它回答的是“每个 exposure unit 分配多少价值”，系统再用区间中点和合约 metadata 换算成交易所原生数量。

对 OKX `BTC-USD-SWAP`：

```text
contracts_per_exposure_unit = notional_per_unit / ctVal
notional = abs(contracts) * ctVal
```

这个口径只描述合约规模。它不表示 PNL 资产，也不表示币本位账户最终赚亏多少 BTC。展示层可以同时显示中点等价 BTC 数量和合约张数，例如 `1000 USD @ 100000 = 0.01 BTC = 10 张`。

### Settlement PNL

币本位反向合约的 PNL、手续费、资金费原始结算资产由 `settleCcy` 决定。

对 `BTC-USD-SWAP`：

```text
settlement_pnl_asset = BTC
```

币本位 loss guard 默认也使用这个资产。USD 等值可以作为展示或跨资产风控投影，但不是币本位第一版的默认事实源。

## Exposure 语义

### U 本位现有语义

当前 U 本位线性合约语义保持不变：

```text
native_position_qty = BTC 数量
native_qty_per_exposure_unit = notional_per_unit / band_center
```

例子：

```text
band_center = 100000
notional_per_unit = 1000
native_qty_per_exposure_unit = 0.01 BTC

position_qty = 0.03 BTC
current_exposure = 3
```

价格变化不会改变 `position_qty`，因此也不会改变 `current_exposure`。

这个语义的代价是：每个 exposure unit 在不同价格上的实际 USD 价值并不相同。

```text
actual_notional_at_price = notional_per_unit * current_price / band_center
```

例如 `notional_per_unit = 1000`、`band_center = 100000` 时：

```text
price = 50000  => 1 unit = 0.01 BTC = 500 USD
price = 100000 => 1 unit = 0.01 BTC = 1000 USD
price = 120000 => 1 unit = 0.01 BTC = 1200 USD
```

因此 U 本位现有语义是“中点 BTC 数量等额”，不是“每个价位 USD 价值等额”。如果未来希望 U 本位也按每个价位固定 USD 价值下单，需要引入按价格计算 order quantity 的 sizing 模型，并重新定义 position qty 到 exposure 的换算方式；这会改变现有策略行为，不属于本次币本位支持的默认改动。

### OKX 币本位反向合约语义

OKX 币本位反向合约使用张数作为 native quantity：

```text
native_position_qty = 合约张数
native_qty_per_exposure_unit = notional_per_unit / ctVal
```

例子：

```text
symbol = BTC-USD-SWAP
ctVal = 100 USD / 张
notional_per_unit = 1000 USD
native_qty_per_exposure_unit = 10 张

position_qty = -30 张
current_exposure = -3
```

BTC 价格从 `100000` 跌到 `50000` 时，`-30 张` 仍然是 `-30 张`，`current_exposure` 仍然是 `-3`。当前价格只用于展示 BTC 折算量、保证金压力估算，或在明确需要 USD 视角时计算 USD 等值。

错误做法：

```text
contracts * ctVal / current_price = 当前折算 BTC 数量
current_exposure = 当前折算 BTC 数量 / 某个 BTC-per-unit
```

这种做法会在没有任何交易发生时，让价格变化改变 `current_exposure`。

## ExchangeRules 数量语义

需要在现有 `ExchangeRules` 中表达数量语义，避免让 engine 到处判断“这个 qty 是 BTC 还是张数”。第一版不新增独立 service；只在 `ExchangeRules` 里增加少量字段，并提供两个聚焦方法。

```text
quantity_kind = "base_asset" | "inverse_contract"
contract_notional = inverse contract 每张 USD 面值，base_asset 下为空
settlement_asset = PNL、手续费、资金费和 loss limit 的资产
```

`contract_notional` 不是固定 `100`，而是来自交易所 instrument metadata。对 OKX `BTC-USD-SWAP`，它来自 `ctVal`。

`ExchangeRules` 第一版只提供两个核心方法：

```text
native_qty_per_exposure_unit(notional_per_unit, band_center)
notional_from_native_qty(native_qty, price)
```

这些方法内部处理差异：

```text
base_asset:
  native_qty_per_exposure_unit = notional_per_unit / band_center
  notional_from_native_qty = abs(base_qty) * price

inverse_contract:
  native_qty_per_exposure_unit = notional_per_unit / contract_notional
  notional_from_native_qty = abs(contracts) * contract_notional
```

如果未来需要展示“张数折算成多少 BTC”，可以在展示层按价格派生，不作为第一版核心方法。

OKX `BTC-USDT-SWAP` 这类 U 本位 SWAP 底层也可能有张数和 `ctVal`，但张数与 BTC 数量是固定比例关系。第一版不把它建成第三种 core 模型；adapter 内部转换后，core / engine 仍然只看到 base asset 数量。

## 配置语义

现有 track 配置字段保持含义：

```text
notional_per_unit: 每个 exposure unit 对应的 USD 名义价值
max_notional: 最大 USD 名义价值
daily_loss_limit: 当日最大净损失，资产自动跟随该 instrument 的结算资产
total_loss_limit: 累计最大净损失，资产自动跟随该 instrument 的结算资产
```

币本位下，`notional_per_unit` 和 `max_notional` 仍然按 USD 面值配置，因为 OKX inverse 合约本身就是固定 USD 面值。用户配置“每格子分配多少钱”时，用 USD 口径更稳定，也可以直接换算成张数。

这个选择也符合分批买入和卖出的成本直觉：按价值分配每个格子或 exposure unit，而不是在不同价格固定同样 BTC 数量。价格低时，同样 USD 价值对应更多 BTC；价格高时，同样 USD 价值对应更少 BTC。用户界面可以把同一个配置展示为中点等价 BTC 数量，降低币本位用户的理解成本，但内部仍以 USD 价值为规范输入。

`daily_loss_limit` 和 `total_loss_limit` 不应该沿用 USD 假设，也不需要新增用户配置。系统应根据合约类型和 instrument metadata 自动切换：

```text
U 本位线性合约: 使用 settlement asset，例如 USDT / USDC
币本位反向合约: 使用 settlement asset，例如 BTC
```

对 `BTC-USD-SWAP`，这表示：

```text
daily_loss_limit = 0.01 => 当日最多净亏 0.01 BTC
total_loss_limit = 0.03 => 累计最多净亏 0.03 BTC
```

如果未来需要跨币种统一风控，可以另加明确命名的 USD 投影视图；第一版不把它做成用户配置。

示例：

```toml
[exchange]
venue = "okx"
deployment = "demo"
api_key = "..."
api_secret = "..."
passphrase = "..."

[[tracks]]
track_id = "btc-coin-margin"
symbol = "BTC-USD-SWAP"
lower_price = 80000.0
upper_price = 120000.0
long_exposure_units = 0.0
short_exposure_units = 120.0
notional_per_unit = 1000.0
max_notional = 120000.0
daily_loss_limit = 0.01
total_loss_limit = 0.03
leverage = 2
```

在 `ctVal = 100` 时：

```text
1 exposure unit = 10 张
1 exposure unit 中点等价 = 1000 / 100000 = 0.01 BTC
short_exposure_units = 120 => 最大 1200 张
最大 USD 面值 = 1200 * 100 = 120000 USD
当日最大净损失 = 0.01 BTC
累计最大净损失 = 0.03 BTC
```

这只是库存曲线配置。用户可以把它用于对冲 `1 BTC` 的价值，但系统不保存这个用户目的。

## OKX adapter 需求

### Instrument metadata

OKX REST model 需要解析并保存：

```text
instId
tickSz
lotSz
minSz
ctType
ctVal
ctValCcy
settleCcy
```

字段 owner 是 `exchanges/okx`。共享层只看到 `ExchangeRules` 上的数量语义和资产标签。

### Exchange rules

`quantity_step` 和 `min_qty` 表示 native quantity 的交易所步进。

对 `BTC-USD-SWAP`：

```text
quantity_step = lotSz = 0.1 张
min_qty = minSz = 0.1 张
```

当前 engine 的最小名义判断如果使用：

```text
price * quantity
```

则只适用于 native quantity 是标的币数量的线性场景。币本位反向合约应使用：

```text
abs(contracts) * ctVal
```

来判断 USD 名义价值。实现时需要把最小交易量判断也改成调用 `ExchangeRules` 的名义价值方法，而不是固定用 `price * quantity`。

对 OKX U 本位 SWAP，如果交易所 API 原始 `sz/pos` 是张数，adapter 需要用 `ctVal` 转成 base asset 数量后再进入 engine。这个转换是 OKX adapter 内部细节，不需要进入 `ExchangeRules` 的公开数量分支。

### REST 下单

对 OKX inverse：

```text
OrderRequest.quantity = 合约张数
PlaceOrderBody.sz = OrderRequest.quantity
```

adapter 不需要用当前价格把张数换成 BTC 数量。engine 已经根据 `ExchangeRules` 给出 native quantity。

### REST / WS 持仓和挂单

OKX 返回：

```text
positions.pos = 合约张数
orders.sz = 合约张数
orders.accFillSz = 合约张数
orders.fillSz = 合约张数
```

这些字段映射到 engine 时仍保持 native quantity。

```text
Position.qty = 合约张数
ExchangeOrder.qty = 合约张数
ExchangeOrder.filled_qty = 合约张数
TrackPnlRecord.qty = 合约张数
```

以上要求针对 OKX inverse。OKX U 本位 SWAP 如果返回张数，adapter 应先转成 base asset 数量。

## Engine 需求

engine 不应该继续把 `base_qty_per_unit` 作为唯一数量换算入口。

需要把执行规划输入从：

```text
base_qty_per_unit
```

升级为：

```text
native_qty_per_exposure_unit
ExchangeRules notional helpers
```

核心行为：

```text
desired_exposure -> target_native_qty
current_native_qty -> current_exposure
order native qty -> binding and fill absorption
```

币本位下，binding、open order recovery、partial fill、cancel receipt、position sync 都以合约张数为数量单位。

## PNL 与风控

### 原始结算 PNL

OKX 币本位反向合约的成交 PNL、手续费和资金费应按 `settleCcy` 标记。

对 `BTC-USD-SWAP`：

```text
pnl_asset = BTC
```

现有 `TrackPnlRecord`、`TrackPnlStats` 和 SQLite schema 只有无单位的 `realized_pnl`、`trading_fee`、`funding_fee`。币本位支持需要避免让这些字段同时代表 BTC 和 USD。

最低要求：

- 记录或投影时能明确 PNL 的结算资产。
- `TrackPnlRecord` 持久化 `pnl_asset`；这是历史事实，不靠 symbol 临时推导。
- `TrackPnlStats` 只能聚合同一 `pnl_asset` 的记录；第一版不做跨资产净额。
- `BTC-USD-SWAP` 的原始 PNL / fee / funding 不能展示成 USD。
- 币本位 loss guard 默认使用结算资产损益，例如 BTC，而不是强制折算成 USD。
- 如果未来增加 USD loss guard，则必须使用明确的 USD 投影字段，不能直接把 BTC 数字和 USD limit 比较。

可接受的数据模型方向：

```text
settlement pnl:
  asset = BTC
  realized_pnl / trading_fee / funding_fee = BTC 数量

loss guard pnl:
  asset = BTC
  realized_pnl / trading_fee / funding_fee / unrealized_pnl = BTC 数量

```

第一版币本位不依赖 USD 投影来做 loss guard。如果未来增加 USD loss guard，trade PNL 可以用成交价折算；unrealized PNL 可以用当前 mark price 折算；资金费如果没有明确成交价，应使用事件发生时可获得的 mark price 或交易所提供的 USD 等值字段。如果无法可靠换算，应 fail closed，不进入 USD loss guard。

### Loss guard

`daily_loss_limit` 和 `total_loss_limit` 必须和该 instrument 的结算资产使用同一资产口径。

币本位默认输入应是：

```text
asset = BTC
net_realized_pnl_today = BTC 数量
net_realized_pnl_cumulative = BTC 数量
unrealized_pnl = BTC 数量
```

U 本位可以继续使用 USDT / USD-like 资产作为 loss guard 口径。核心要求是 `LossGuardSnapshot` 不能再隐含“数字一定是 USD”；第一版保持 `LossGuardSnapshot` 为纯数字，并在 engine 构建 snapshot 前验证 `TrackPnlStats.pnl_asset` 和 instrument 的 `settlement_asset` 一致。

## Account capacity 与保证金压力

币本位反向合约的保证金压力和 U 本位不同。

U 本位线性合约近似：

```text
initial_margin_usd = base_qty * price / leverage
```

OKX 币本位反向合约近似：

```text
initial_margin_btc = contracts * ctVal / (mark_price * leverage)
```

价格下跌时，同样张数的 USD 面值不变，但折算成 BTC 的保证金需求变高。

因此 OKX 币本位账户容量不能按 symbol 的 quote asset 查余额。`BTC-USD-SWAP` 的 quote-like 字段是 USD，但保证金和结算资产是 BTC。

第一版不接 OKX `GET /api/v5/account/max-size`。账户容量使用账户可用资产本地估算，语义是：

```text
account available asset
+ current instrument price
+ leverage
+ instrument quantity rules
=> estimated capacity for this instrument
```

这不是交易所最终承诺值；下单被交易所拒绝时仍然要处理错误并刷新账户状态。

需要的数据：

```text
available_by_asset[settlement_asset]
mark_price
leverage
quantity_kind
contract_notional
```

U 本位线性合约：

```text
estimated_max_base_qty = available_usdt * leverage / mark_price
```

币本位 inverse：

```text
estimated_max_contracts = available_btc * mark_price * leverage / contract_notional
```

挂单冻结如果已经体现在交易所的 available 字段里，会自然进入估算。实施时需要用 OKX demo 或 fixture 确认可用余额字段选择。当前 `BalanceDetail` 解析了 `availEq`，但 `AccountSummarySnapshot` 只有无资产标签的 `available`，不足以表达 `available_by_asset`。

## Protocol / 展示需求

现有公开读模型中的 `quantity`、`notional`、`notional_asset`、`pnl_asset` 对币本位需要更清楚。

建议补充或调整：

```text
position.quantity = native quantity
position.quantity_unit = "contracts" 或 "base"
position.notional = USD 名义价值
position.notional_asset = "USD"
pnl.pnl_asset = settleCcy，例如 "BTC"
```

按价格折算的 base 数量可以作为后续展示增强，不作为第一版核心字段。loss guard 资产第一版不单独暴露；它和 `pnl_asset` 使用同一个结算资产。

如果第一版不扩展 protocol 字段，至少不能把 `BTC-USD-SWAP` 的 PNL asset 投影为 `USD`。当前 `pnl_asset = instrument.quote_asset()` 对币本位会错，应改为来自 `ExchangeRules` 或 instrument metadata 的 settlement asset。

## 验收标准

### 纯函数 / mapper

- OKX `InstrumentInfo` 能解析 `ctType`、`ctVal`、`ctValCcy`、`settleCcy`。
- `BTC-USD-SWAP` 的 `ExchangeRules` 使用 `quantity_kind="inverse_contract"`，并带有 `contract_notional=ctVal` 和 `settlement_asset=settleCcy`。
- `ctVal=100`、`notional_per_unit=1000` 时，`native_qty_per_exposure_unit = 10`。
- `position.qty = -30 张` 时，`current_exposure = -3`。
- price 从 `100000` 变为 `50000` 时，同一 `-30 张` 的 `current_exposure` 不变。
- `notional = abs(contracts) * ctVal`。

### Engine

- 币本位 inverse 的 submit quantity、binding quantity、fill quantity 和 open order quantity 都以张数处理。
- `quantity_step=0.1` 时，订单按 0.1 张步进取整。
- 最小交易量判断不使用 `price * contracts`。
- recovery 能用交易所 open orders 的张数重建 binding。
- position sync 不会把张数按当前价格折算成 BTC 后再计算 exposure。

### PNL / 风控

- OKX inverse trade PNL 原始资产为 BTC。
- OKX inverse fee / funding 原始资产为 BTC。
- `TrackPnlRecord.pnl_asset` 持久化为 BTC。
- `pnl_asset` 显示为 BTC。
- 币本位 inverse 自动使用 BTC PNL 与 BTC loss limit 比较。
- U 本位自动使用 USDT / USDC 等结算资产 PNL 与同资产 loss limit 比较。
- 如果无法把某条 BTC PNL 明细可靠换算成 USD 等值，该明细不得静默进入 USD loss guard。

### Account capacity

- `BTC-USD-SWAP` 使用 `settleCcy=BTC` 确定保证金资产。
- 第一版不接 OKX `max-size`。
- 使用 `available_by_asset[BTC]`、mark price、leverage 和 `contract_notional` 估算可开张数。
- 价格降低时，同样 BTC 可用保证金对应的可开张数降低。

### 回归

- Binance / Bybit U 本位现有行为不变。
- OKX `BTC-USDT-SWAP` 不应因为新增 inverse 支持而产生数量单位回归；如果 OKX API 原始返回张数，adapter 内部转换后，engine 仍然看到 BTC 数量。

## 事实确认与第一版范围

- OKX 账户余额中，用于 inverse 可用保证金的最佳字段是 `availBal`、`availEq`，还是另一个账户接口字段；挂单冻结后该字段是否符合本地 capacity 估算预期。
- OKX position `upl`、orders `fillPnl`、fee 字段在币本位 demo / mainnet 中是否始终以 `settleCcy` 计价。
- Funding fee 当前 OKX adapter 是否已经有完整来源；如果没有，币本位 funding PNL 可以作为后续能力，但不能错误标成 USD。
- Protocol 第一版加入 `quantity_unit`，并修正 `pnl_asset`，不加入按价格折算的 base 数量或 USD projection。
