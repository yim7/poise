# 统一交易所抽象调研

日期：2026-08-27

复核修订：2026-08-31

## 结论

可以统一，但不应新建一个覆盖所有行为的巨型 `Exchange` trait，也不应先做通用 REST/WebSocket 框架。

项目已经在 [`engine/src/ports.rs`](../engine/src/ports.rs) 中形成了面向业务语义的统一抽象：

- `ExecutionPort`
- `AccountPort`
- `AccountSummaryPort`
- `MetadataPort`
- `MarketDataPort`

Binance、Bybit、Hyperliquid、OKX 的适配器都实现了这组能力端口。当前真正缺少的是：

1. **一致且可验证的端口语义**：部分实现虽然满足同一个 Rust 类型，但对“完整”“已取消”“最近”等概念的解释不同。
2. **统一的标的和计算单位模型**：当前 `QuantityKind`、`native_qty`、`quote_asset` 和账户汇总金额混合了仓位单位、wire 单位、计价资产、名义价值资产与结算资产。
3. **统一的连接与启动生命周期**：连接创建和启动准备分别在两处按交易所分派，增加交易所时容易漏改。
4. **跨适配器契约测试**：目前没有一套测试证明各交易所满足端口承诺。

因此，推荐保留现有五个能力端口。实施时先用独立的端到端任务修正开放订单完整性、取消终态、最近 PNL 覆盖范围和账户容量杠杆等现有 P0 语义风险；再按标的切片引入 `InstrumentSpec`，统一不可变的合约事实和数量/资产换算；最后在 server 装配层合并连接与启动控制。不要让大范围抽象迁移阻塞当前正确性修复，也不要把交易所差异隐藏在一个越来越大的接口里。

## 范围与现状

仓库中有四个交易所适配器：Binance、Bybit、Hyperliquid、OKX。当前可通过启动审计的完整运行时是 Binance、Hyperliquid、OKX；Bybit 已有配置、连接和交易适配，但 `recent_track_pnl_records` 返回 `Unsupported`，会被 [`server/src/runtime/pnl_startup_audit.rs`](../server/src/runtime/pnl_startup_audit.rs) 拒绝。

这也解释了“支持三个交易所”和仓库中“四个适配器”看似矛盾的原因。

[`README.md`](../README.md) 和 [`docs/system-overview.md`](system-overview.md) 的能力矩阵仍把 Binance 最近 PNL 标为不支持，但当前 Binance 实现已经返回 `Available`。这两份文档已经落后于代码，应随本次工作更新。

当前结构如下：

| 层次 | 当前职责 | 统一程度 |
| --- | --- | --- |
| `core` | `Venue`、`Instrument` 等领域类型 | 基本统一，但仍有交易所规则泄漏 |
| `engine::ports` | 交易执行、账户、元数据、行情等业务能力 | 已统一，是应保留的主边界 |
| `exchanges/*` | 协议、签名、字段映射、精度和数量换算 | 应保持交易所独立 |
| `server::assembly` | 根据配置连接适配器并组装 `ExchangePorts` | 有统一返回值，但仍逐交易所分派 |
| `server::exchange_startup` | 账户模式、持仓模式、杠杆等启动准备 | 已有统一控制接口，但再次逐交易所分派 |

## 交易所差异不应被抹平

以下差异是交易所协议本身的一部分，应由各适配器在能力端口之后消化：

| 维度 | Binance | Bybit | Hyperliquid | OKX |
| --- | --- | --- | --- | --- |
| 数量语义 | 线性合约，基础资产数量 | 线性合约，基础资产数量 | 基础资产数量 | 线性或反向合约，可能需要张数与币数量换算 |
| 价格精度 | 固定 tick | 固定 tick | 有效数字规则 | 固定 tick |
| 客户端订单 ID | 最长 36，特定字符集 | 最长 36，要求唯一 | 128 位十六进制 `cloid` | 最长 32，仅字母数字 |
| 下单响应 | 可返回实际订单状态 | 仅表示异步请求已接受 | 可返回 `resting` 或 `filled` | 仅表示请求已接受 |
| 仓位模式 | 账户级单向模式 | 交易对级单向模式 | 逐资产杠杆/保证金设置 | 账户级 `net` 模式 |
| 最近 PNL | 成交与资金费接口 | 当前未实现 | 明确时间窗并分页 | 成交与账单接口 |

这些差异支持继续使用“小能力端口 + 适配器内部映射”的方向，不支持创建一个包含所有可选参数的统一请求结构。

## 标的与单位语义专项结论

### 当前 `Instrument` 只标识路由，不能描述合约

当前 `Instrument { venue, symbol }` 足以把请求路由到适配器，但不足以回答数量、名义价值和结算问题：

- Binance、Bybit 使用类似 `BTCUSDT` 的交易所 symbol；产品类型由适配器 crate 和固定 API category 隐含。
- Hyperliquid 默认永续使用 `BTC` 等 coin 名称，HIP-3 使用 `dex:coin`；下单 wire identity 则是依赖环境和 meta 顺序的整数 asset id。
- OKX 使用 `BTC-USDT-SWAP`、`BTC-USD-SWAP` 等完整 `instId`，但不能仅靠拆字符串确定 `ctVal`、`ctValCcy` 和 `settleCcy`。

因此，`symbol` 应保留为交易所内的逻辑标识，wire asset id 继续由适配器解析；合约语义必须来自交易所元数据，不能从字符串猜测。

### 必须区分四类资产

当前代码经常把 quote asset 当作 settlement asset。线性 USDT 合约中两者碰巧相同，但这不是一般规律：

| 概念 | 含义 | 例子 |
| --- | --- | --- |
| base asset | 仓位所跟踪的标的资产 | BTC |
| quote asset | 价格的计价单位 | USDT、USDC 或 USD 指数单位 |
| notional asset | 风险限额、敞口和最小名义价值使用的估值单位 | USDT 或 USD |
| settlement asset | 保证金、PNL、资金费实际记账的资产 | USDT、USDC 或 BTC |

典型差异：

- Binance `BTCUSDT`：base=BTC，quote/notional/settlement=USDT。
- Bybit `BTCUSDT`：base=BTC，quote/notional/settlement=USDT；`BTCUSDC` 则为 USDC。
- Hyperliquid 多数永续按 USDT 价格标度，但以 USDC 抵押和结算，官方明确说明不做 USDC/USDT 汇率换算；HYPE、PURR 等还可能使用 USDC oracle。这里不能用一个 `quote_asset=USDC` 同时表示价格和结算。
- OKX `BTC-USDT-SWAP` 线性合约：base=BTC，quote/notional/settlement=USDT。
- OKX `BTC-USD-SWAP` 反向合约：base=BTC，quote/notional=USD，settlement=BTC。

[`core/src/track.rs`](../core/src/track.rs) 的 `Instrument::quote_asset` 对 Hyperliquid 固定返回 USDC、对 OKX 拆 symbol 中间段，已经混淆了 quote 与 settlement。OKX 反向合约尤其能直接证明二者不等价。

### `QuantityKind` 混合了两项独立事实

现有 `QuantityKind::{BaseAsset, InverseContract}` 同时表达“Poise 内部仓位数量单位”和“合约 payoff 是线性还是反向”，但两项并不总是一起变化：

| 场景 | 交易所 wire qty | Poise port 中的 qty | payoff |
| --- | --- | --- | --- |
| Binance USDⓈ-M | base amount | base amount | linear |
| Bybit linear | base amount | base amount | linear |
| Hyperliquid perp | base amount | base amount | linear/quanto 结算 |
| OKX linear SWAP | contracts | base amount，已乘 `ctVal` | linear |
| OKX inverse SWAP | contracts | contracts | inverse |

所以当前代码中的 `native_qty` 实际是 **Poise canonical position qty**，不是普遍意义上的 exchange-native qty。OKX linear 已经在 port 边界把 contracts 换成 base amount。

建议拆成：

- `PositionUnit::BaseAsset` 或 `PositionUnit::Contracts`：规定 `OrderRequest.quantity`、`Position.qty`、`ExchangeOrder.qty`、成交 qty 在 Poise 内部的共同单位。
- `ContractKind::Linear`、`Inverse` 或显式的合约公式：规定 position quantity、base exposure 和 notional 如何换算。
- wire quantity 类型只存在于各适配器内部，不进入 `engine::ports`。

### 当前下单与回读换算

代码核验结果如下：

- Binance、Bybit、Hyperliquid 的下单、订单、持仓和成交数量目前都按 base amount 原样通过，关键路径一致。
- OKX linear 在下单前执行 `base_qty / ctVal -> contracts`，在 REST/WS 的持仓、开放订单、累计成交和单次成交映射中执行 `contracts * ctVal -> base_qty`；当前已检查的生产路径方向一致。
- OKX inverse 让 Poise qty 保持 contract count，名义价值使用 `contracts * ctVal(USD)`，避免把价格再次乘入反向合约名义价值。
- OKX 的 `quantity_step`、`min_qty` 对 linear 已从 `lotSz`/`minSz` contracts 转成 base amount；inverse 则保持 contracts。这与 port qty 一致。

当前没有发现已发生的 OKX 双重换算，但类型系统无法阻止它：wire qty 和 canonical qty 都是裸 `f64`，同一个 helper 既可接原始 contracts，也可能被误用于已归一值。另有三处具体风险：

1. OKX linear 元数据缺少 `ctVal` 时静默使用 `1.0`；官方对 SWAP/FUTURES 提供 contract value，这里应缺失即失败，否则数量可能成倍错误。
2. OKX `settleCcy` 缺失时回退到 symbol 推断；反向合约或未来产品会得到错误 PNL 资产，应缺失即失败。
3. Hyperliquid 引擎已按 `PricePrecision` 和 `quantity_step` 归一，REST client 在序列化前又用 nearest 规则归一价格和 size。合法输入通常幂等，但若第二次改变值，绑定中记录的订单与 wire 订单就会不同。

建议让归一化只有一个 owner：先由合约规格生成不可变的 `NormalizedOrder`，再交给适配器做无损 wire 编码。适配器只验证和编码，不应静默改变 price/qty；如必须改变，receipt 必须返回实际提交的 canonical price/qty。

### 精度与最小订单

- Binance 从 `PRICE_FILTER.tickSize`、`LOT_SIZE.stepSize/minQty`、`MIN_NOTIONAL.notional` 取规则；官方特别说明不能用 `pricePrecision`/`quantityPrecision` 代替 tick/step，当前选择正确。
- Bybit linear 从 `tickSize`、`qtyStep`、`minOrderQty`、`minNotionalValue` 取规则，当前数值来源正确；但 model 丢弃了同一响应中的 `baseCoin`、`quoteCoin`、`settleCoin` 和 `contractType`。
- Hyperliquid size 精度由 `szDecimals` 决定；价格同时受最多五位有效数字和 `6 - szDecimals` 小数位约束。当前 `PricePrecision::SignificantFigures` 能表达该规则。`min_notional=10` 和费率是适配器常量，不是 meta 返回值，应明确其版本和来源。
- OKX `tickSz` 是价格 tick，`lotSz`/`minSz` 对衍生品的单位是 contracts；`ctVal`/`ctValCcy` 决定一张合约的含义。当前 linear 换算方向正确，但必须把 `ctVal` 当作必需元数据。

所有适配器的 maker/taker fee rate 当前都是硬编码默认费率，不能代表账户实际等级。它们可以作为带来源和版本的保守估算输入，但不属于 `InstrumentSpec` 的不可变标的事实，也不能与成交回报中的实际 fee 混淆。

### PNL、fee 和 funding 的单位及符号

`TrackPnlRecord` 目前用一个 `pnl_asset` 约束 `realized_pnl`、`trading_fee`、`funding_fee`，这是有价值的防线，但三个裸 `f64` 的符号约定并不直观：

- `trading_fee` 是正数表示支出、负数表示 rebate，汇总时相减。
- `funding_fee` 是正数表示收入、负数表示支出，汇总时相加。
- `realized_pnl` 是正收益、负亏损。

Binance 和 Bybit 遇到 fee asset 与推断 settlement asset 不同时会把 fee 记为 `0`，可能低估成本；Hyperliquid、OKX 对不一致资产选择失败。两种行为都不适合作为统一契约：静默记零会丢失已观察到的费用，整条失败又会连同有效的 realized PNL 一起阻断恢复。

统一契约应保留交易所返回的原始 cashflow：realized PNL、trading fee、funding 各自携带 amount 和 asset。聚合到 settlement/valuation unit 时，如果缺少可靠汇率，应继续持久化 realized PNL 和原始费用，同时把聚合结果标记为 `Incomplete` 并暴露未估值费用，不能伪装为完整统计。汇率换算属于单独的估值步骤，必须携带汇率来源和时间；它不是 `InstrumentSpec` 的职责。

OKX `fillPnl` 官方定义为 settlement currency 中、不含 fee 的平仓 PNL，wire fee 为负支出/正 rebate；当前把 fee 取反后再由 ledger 相减，方向正确。其他适配器也应通过同一组“开仓只有 fee、平仓 PNL+fee、maker rebate、正负 funding”数值测试证明没有重复计费。

### 账户估值金额与资产数量不能共用裸 `f64`

`AccountSummarySnapshot` 的 `equity`、`available`、`unrealized_pnl` 都没有 valuation asset，`AccountMonitor` 直接计算 `available/equity` 和 `unrealized_pnl/equity`。但来源并不统一：

- Bybit 官方定义三个 account-wide total 为 USD 估值。
- Binance 在 single-asset mode 中以 USDT 表示，在 multi-assets mode 中是 USD 估值。
- OKX `totalEq` 是 USD；`details[].availEq`、`details[].upl` 是逐 currency 的值。
- Hyperliquid 当前返回 USDC 账户值语义，并且 unified/portfolio 模式还会改变余额来源。

当前 OKX mapper 把不同 currency 的 `availEq` 相加写入 `available`，同时保留 `available_by_asset`；这可能把 BTC 数量和 USDT 数量相加，再与 USD `totalEq` 做比例。逐 currency `upl` 也存在同类风险。这是确定需要修正的 P0 单位问题。

建议把 account-wide 估值金额与资产原生数量分开：

```rust
struct ValuedAmount {
    value: f64,
    unit: ValuationUnit, // 例如 USD
}

struct AssetAmount {
    value: f64,
    asset: AssetId,
}
```

`AccountSummarySnapshot` 的 equity/available/unrealized PNL 必须带同一 valuation unit；`available_by_asset` 则使用 `AssetAmount`。`AccountMonitor` 在单位不一致时拒绝计算和更新 baseline，持久化 baseline 时也保存 valuation unit。账户容量的 `max_increase_notional` 同样必须携带 notional unit。

### 共享层仍有一条旧的 base-qty 换算路径

运行时主路径已经通过 `ExchangeRules::native_qty_per_exposure_unit` 处理 inverse contract，但 `TrackConfig::base_qty_per_unit`、`TrackDefinition::exposure_from_position_qty` 和 `required_additional_notional` 仍固定使用 `notional / price`。目前这些旧方法主要由测试使用；如果新调用方用于 OKX inverse，会把 contract count 当 base amount 再换算。

应删除或封闭这条重复换算路径，让 exposure、notional 与 position qty 的互转全部经过同一个合约规格对象。

### 持久化数量必须保存当时的含义

`TrackPnlRecord.qty` 当前只持久化裸数值。即使以后增加 `PositionUnit::Contracts`，历史记录仍不能仅靠“contracts”解释经济含义，因为 contract value 可能因标的而异，交易所元数据也可能发生变化。

当前 PNL 聚合以交易所返回的 realized PNL、fee 和 funding 金额为权威事实，并不需要用历史 qty 重算 PNL。因此采用最小持久化契约：

- `qty` 只用于审计和展示，禁止用当前 `InstrumentSpec` 对旧记录重算历史 PNL。
- 每条有 qty 的记录同时保存 `position_unit`；base quantity 保存 `base_asset`，contract quantity 额外保存当时的 `contract_value` 和 `contract_value_asset`。
- 如果未来确实需要从历史成交重算风险或 PNL，再单独引入不可变的 instrument-spec 版本或快照；当前不提前持久化整份 `InstrumentSpec`。

### 推荐的 `InstrumentSpec` 深模块

不要把上述字段继续平铺到 `ExchangeRules` 并让各调用方自行组合。建议由一个 `InstrumentSpec` 深模块持有标的事实和全部纯换算：

```rust
struct InstrumentSpec {
    id: Instrument,
    assets: InstrumentAssets {
        base: AssetId,
        quote: PriceUnit,
        notional: ValuationUnit,
        settlement: AssetId,
    },
    position_unit: PositionUnit,
    contract_kind: ContractKind,
    price_rule: PriceRule,
    quantity_rule: QuantityRule,
    minimum_order: MinimumOrder,
}

impl InstrumentSpec {
    fn normalize_order(&self, order: DesiredOrder) -> Result<NormalizedOrder>;
    fn position_qty_per_exposure_unit(&self, notional: NotionalAmount, price: Price)
        -> Result<PositionQty>;
    fn notional_from_position_qty(&self, qty: PositionQty, price: Price)
        -> Result<NotionalAmount>;
    fn base_exposure_from_position_qty(&self, qty: PositionQty, price: Price)
        -> Result<BaseAmount>;
}
```

接口原则：

- `InstrumentSpec` 只拥有不可变的标的事实：base/quote/notional/settlement、合约类型、数量单位、精度和最小订单规则。
- `NormalizedOrder` 的 price/qty 已满足交易所规则，执行端口和订单绑定记录同一个值。
- `PositionQty` 携带 `PositionUnit`；不能把任意 `f64` 直接传给 contract/base 换算函数。
- `NotionalAmount`、`BaseAmount`、`AssetAmount`、`ValuedAmount` 都携带单位；只有显式转换才能相加或比较。
- 实际 realized PNL、fee 和 funding 以交易所 cashflow 为权威事实，不通过一个通用的 `notional -> settlement` 方法推导。费率估算作为调用方显式传入的策略输入，并携带来源。
- OKX 的 wire contract qty、Hyperliquid 的整数 asset id、各交易所的 wire symbol 和 decimal 字符串都留在 adapter 私有类型中。
- adapter 的 codec 只做 `NormalizedOrder -> WireOrder` 和 `WirePosition/WireOrder/WireFill -> canonical fact`；转换必须在 port 边界恰好一次。
- Hyperliquid 的 USDT 价格标度到 USDC 结算的 1:1 规则必须显式记录为 venue contract，不能沿用“linear 默认 notional asset 等于 settlement asset”的隐含假设。

### 数值契约测试矩阵

以下用固定样例验收换算方向和单位；实际测试还要加入边界值、round-trip 和随机性质测试。

| 场景 | 输入规格 | Poise canonical qty | wire qty | notional / settlement 断言 |
| --- | --- | --- | --- | --- |
| Binance/Bybit linear | BTC 价格 50,000 USDT，qty step 0.001 BTC | 0.020 BTC | 0.020 | notional=1,000 USDT；持仓、订单、成交回读仍为 0.020 BTC |
| Hyperliquid linear | BTC `szDecimals=5`，价格 65,000 | 0.020 BTC | 0.020 | notional=1,300 USD 标度；PNL/fee/funding 明确为 USDC；序列化不得再次改变 canonical qty |
| Hyperliquid 有效数字 | `szDecimals=4`，原始价格 1,234.56 | 按下单方向归一 | 与 normalized price 完全相同 | passive buy/sell 分别断言方向安全的 1,234.5/1,234.6，adapter 不再 nearest 改价 |
| OKX linear | `ctVal=0.01 BTC/contract`，价格 50,000 USDT | 0.020 BTC | 2 contracts | 回读 2 contracts 必须只乘一次 `ctVal` 得 0.020 BTC；notional=1,000 USDT |
| OKX inverse | `ctVal=100 USD/contract`，价格 50,000 USD | 10 contracts | 10 contracts | notional=1,000 USD，与价格无关；等价 base exposure/settlement 换算为 0.020 BTC 时只能除一次价格 |
| OKX 多资产账户 | `totalEq=20,000 USD`，available 有 0.1 BTC 和 5,000 USDT | 不适用 | 不适用 | 禁止得到 `available=5,000.1`；account-wide available 必须是 USD 估值，逐资产数量分别保留 |
| fee/rebate/funding | fee=2、rebate=1、funding received=3、funding paid=4 | 不适用 | 不适用 | 标准化后净现金流必须唯一且可解释，不能因交易所 wire 符号不同而重复取反 |
| 非结算币费用 | realized PNL=10 USDT，fee=0.001 BNB，缺少可靠 BNB/USDT 汇率 | 不适用 | 不适用 | 两个原始 cashflow 都被持久化；USDT 聚合标记为 `Incomplete`；恢复继续且不得把 fee 记为 0 USDT |
| 历史 contract qty | 成交时 `ctVal=100 USD/contract`，记录 10 contracts；当前元数据后来变化 | 10 contracts | 10 contracts | 历史记录保留当时的 100 USD contract value；禁止使用当前元数据重算历史 PNL |

每个交易所至少覆盖：

1. `DesiredOrder -> NormalizedOrder -> WireOrder` 的精度、最小数量和最小名义价值。
2. `canonical qty -> wire qty -> canonical qty` round-trip；误差不超过该 instrument 的 quantity step 容差。
3. REST position、REST open orders、WS order、WS position、REST/WS fills 使用同一转换，并得到相同 canonical unit。
4. linear、inverse、linear-with-contract-size 三种合约分别验证 notional 公式。
5. realized PNL、unrealized PNL、fee、rebate、funding 的资产和符号。
6. 非 settlement fee 缺少可靠汇率时保留原始 cashflow、标记不完整并继续恢复。
7. 历史 contract qty 使用记录时的 contract value，不读取当前元数据重算。
8. 元数据缺 `base/quote/settlement/ctVal`、单位不匹配或换算后无法精确落在 wire step 时 fail closed。

### 单位模型迁移原则

单位模型是独立工作流，不阻塞下一节的现有 P0 语义修复，也不一次横向修改所有类型和适配器。按可运行的端到端切片迁移：

1. 先用 OKX linear 贯通 `metadata -> InstrumentSpec -> engine order -> wire contracts -> REST/WS canonical fact -> 持久化/读模型`，同一任务内迁移所有直接消费者，不保留第二个 `ExchangeRules` 事实源。
2. 再迁移 OKX inverse，验证 contract notional、base exposure、settlement asset 和历史 qty 含义。
3. 最后依次迁移 Binance、Bybit、Hyperliquid；每迁移一个 adapter，就运行同一套数值和 round-trip 契约测试，并删除该 adapter 的旧换算入口。
4. 账户估值和 PNL cashflow 分别作为独立切片处理，因为它们包含账户状态、汇率和持久化语义，不属于 `InstrumentSpec`。

### 标的与单位验收

- 给定任一 `Instrument`，启动后都能得到完整且经过验证的 `InstrumentSpec`，不能靠 symbol 猜结算资产。
- 同一个 canonical qty 在提交、持仓、开放订单和成交映射中含义一致；adapter wire qty 不越过 port 边界。
- OKX linear 和 inverse 的 round-trip 数值矩阵全部通过，缺失 `ctVal` 或 `settleCcy` 时启动失败。
- Hyperliquid 下单前后 canonical price/qty 完全一致，不存在 adapter 的第二次静默调整。
- notional、exposure、PNL、fee、funding 和账户估值均携带可比较的单位；不同资产不能直接相加。
- `AccountMonitor` 只对同 valuation unit 的 equity、available、unrealized PNL 和 baseline 计算比例。
- fee asset 不匹配时保留原始 `AssetAmount`；可以可靠换算则聚合，否则保留 realized PNL 并把统计标记为 `Incomplete`，不静默记零，也不丢弃整条记录。
- 共享层不再含 venue-specific symbol/settlement 解析，也不存在不经过 `InstrumentSpec` 的 base-qty 换算入口。

## 必须先修正的语义风险

### P0：开放订单快照并不总是完整

`ExchangeOpenOrderSnapshot::from_complete_exchange_query` 明确承诺结果来自完整查询。恢复逻辑会把快照中缺失的订单视为已经不存在。

但当前实现中：

- Binance 的查询可以返回指定交易对的全部开放订单。
- Bybit `/v5/order/realtime` 默认每页 20、最多 50，当前实现没有读取 `nextPageCursor`。
- OKX pending orders 默认及最多每页 100，当前实现没有分页。

当实际订单数超过单页上限时，Bybit 和 OKX 会把截断结果伪装成完整快照，可能导致恢复过程错误地终结仍在交易所活动的订单。

建议：

- Bybit、OKX 必须分页到结束后才能构造 `from_complete_exchange_query`。
- 无法证明完整时返回明确错误，不能降级为部分快照。
- 增加每个适配器共享的契约测试：多页数据、重复项、空末页、游标不前进和中途失败。

### P0：取消请求已接受不等于订单已取消

Bybit 和 OKX 的官方接口都说明：取消响应只表示请求已接受，最终状态需要通过订单流或查询确认。当前适配器却把成功响应直接映射为 `OrderStatus::Canceled`。

这会让引擎提前清除订单绑定，丢失后续真实状态。

建议将取消结果区分为：

- `Accepted`：交易所接受了取消请求，订单仍需确认。
- `Confirmed`：查询或订单流已经确认终态。

也可以让 `ExecutionPort::cancel_order` 只返回 `CancelAccepted`，所有终态统一由订单流或完整查询产生。无论采用哪种类型，都不能再由 HTTP 接受响应直接宣布终态。

### P0：`RecentTrackPnlRecords::Available` 没有证明数据覆盖完整

启动审计把 `Available(Vec<_>)` 当作足够恢复，但各适配器实际语义不同：

- Binance 固定请求最多 1000 条成交和资金费记录，未声明时间窗，也不分页。
- OKX 固定请求最多 100 条成交和资金费记录，未分页。
- Hyperliquid 按明确的 24 小时时间窗分页，并在触及平台只保留最近 10000 条成交的边界、无法证明完整时失败。
- Bybit 返回 `Unsupported`。

所以 `Available` 目前只证明“接口返回过数据”，没有证明覆盖了恢复需要的时间段。

建议让调用方传入所需时间窗，并返回带证据的快照，例如：

```rust
struct RecentPnlSnapshot {
    records: Vec<TrackPnlRecord>,
    requested_window: TimeRange,
    covered_window: TimeRange,
    observed_at: DateTime<Utc>,
    completeness: SnapshotCompleteness,
}
```

只有覆盖所需时间窗且 `completeness` 为完整时，启动审计才能通过。各适配器必须分页、去重，并在交易所保留期或页数限制导致无法证明完整时失败。

### P0：账户容量使用的杠杆语义不一致

Binance、Bybit、OKX 的 `AccountCapacitySnapshot` 使用当前交易对的实际杠杆。Hyperliquid 则在 `connected.rs` 中固定使用 `DEFAULT_CAPACITY_LEVERAGE = 10`，即使运行配置不是 10 倍也会返回按 10 倍估算的容量。

统一类型因此产生了不统一的业务含义。建议在查询时显式传入本次 track 的杠杆，或从交易所读取实际杠杆；如果无法获得准确值，应返回错误，不能使用隐含默认值。

### P1：客户端订单 ID 需要统一业务身份，而不是统一线格式

各交易所对长度和字符集的约束不同，Hyperliquid 还需要把本地 ID 映射为 128 位十六进制 `cloid`。当前 Hyperliquid 的反向映射保存在进程内存中，重启后可能只剩交易所 ID，影响恢复时的身份关联。

建议：

- 引擎使用稳定的 `ClientOrderKey` 作为业务身份。
- 每个适配器提供确定性的编码、校验和必要的持久化映射。
- 在发请求前验证约束，不要等交易所拒绝。
- 契约测试覆盖长度、字符集、碰撞、重启后恢复和交易所返回未知 ID。

### P1：结算资产规则泄漏到共享领域层

[`core/src/track.rs`](../core/src/track.rs) 中 `Instrument::quote_asset` 已经包含 Hyperliquid、OKX 的 venue 分支和符号字符串规则。这说明交易所符号/结算规则正在泄漏到共享层。

元数据端口已经返回 `ExchangeRules.settlement_asset`，运行时应优先依赖这份交易所权威元数据。共享层只保留不依赖 venue 的领域概念，避免今后每增加一个交易所就修改 `core`。

## 推荐的目标结构

目标结构包含两个相互独立的统一方向：

1. 计算语义由前述 `InstrumentSpec` 统一。它是核心领域中的纯计算深模块，不是新的外部端口；不可变标的事实、精度、数量、base exposure 和名义价值换算从这里进入引擎，实际 PNL/fee/funding cashflow 不从这里推导。
2. 连接生命周期由 server 装配层统一。`ConnectedExchange` 只是 `server::assembly` 的私有装配记录，不是 engine 的领域类型，也不由 adapter 返回；它不承载合约计算规则。

保留现有五个能力端口，把连接和启动控制合并为一个运行时对象：

```rust
struct ConnectedExchange {
    venue: Venue,
    ports: ExchangePorts,
    startup: Arc<dyn ExchangeStartupControl>,
}
```

`ExchangeConfig` 仍然可以是穷举枚举；这能保证新增交易所时编译器提示所有配置分支。区别在于只做一次分派：每个配置分支连接自己的适配器，并一次性返回完整的 `ConnectedExchange`。`server::assembly` 不再组装端口后，又由 `server::exchange_startup` 对同一组交易所做第二次 match。

`ExchangeStartupControl` 继续作为 server 启动流程的私有接口，由 server 为各 adapter 暴露的具体 control 类型实现。交易所 crate 不依赖 server，也不需要认识 `ConnectedExchange`。这个记录只用于保证同一配置分支产生匹配的 venue、ports 和 startup control；保持无业务方法、无额外公共接口。如果一次分派可以直接返回同样的私有元组，也不必为了名称单独公开一个类型。

如果后续确实需要展示能力，可以增加只读的 `ExchangeCapabilities`，例如是否支持最近 PNL、是否有服务端时间、支持哪些数量类型。但它只能用于启动前校验和界面展示，不能替代运行时的完整性证明。例如“支持最近 PNL”不能证明本次查询覆盖完整。

不建议：

- 把五个端口重新合并为单个 `Exchange` trait。
- 为所有交易所设计同一组 REST 请求、WebSocket 事件或认证类型。
- 用大量 `Option` 表达交易所专属参数。
- 为消除少量重复而先抽取通用 HTTP、签名或分页框架。
- 在共享层继续增加 venue 判断来解释符号、数量或结算规则。

## 建议工作顺序（尚未确认实现计划）

以下是调研给出的实施顺序，不是已经确认执行的 plan。每一项都应在同一个任务中完成失败验收测试、实现和最小验证，避免先按类型或 crate 铺出不可运行的半迁移状态。

### 工作流 1：立即修正现有 P0 端口语义

每个问题都是一个独立的端到端闭环，不等待 `InstrumentSpec`：

1. **开放订单完整性**：为 Bybit、OKX 写多页、重复游标、空末页和中途失败测试；实现分页；只有完成分页后才能构造完整快照。
2. **取消接受与终态**：先固定 `cancel accepted` 契约及调用方行为，再修改 Bybit、OKX 映射；只有订单流或权威查询可以产生 `Canceled` 终态。
3. **最近 PNL 覆盖证据**：让调用方传入时间窗，返回 coverage/completeness；为 Binance、OKX 补分页、去重和截断检查。Bybit 可以继续显式 `Unsupported`，但不能被宣称为完整运行时。
4. **账户容量杠杆**：让容量查询使用指定或交易所实际杠杆，删除 Hyperliquid 固定 10 倍语义，并用非 10 倍样例验收。

### 工作流 2：按标的切片迁移 `InstrumentSpec`

1. 用 OKX linear 建立最小 `InstrumentSpec` 接口，贯通元数据、引擎、wire 下单、REST/WS 回读、持久化和读模型；同一任务删除被替代的 `ExchangeRules` 事实源。
2. 迁移 OKX inverse，覆盖 contract notional、base exposure、settlement asset 和历史 qty 记录。
3. 依次迁移 Binance、Bybit、Hyperliquid；每完成一个 adapter，就运行同一套数值与 round-trip 契约测试并删除该 adapter 的旧换算入口。
4. 所有 adapter 迁移完成后，删除 `TrackConfig::base_qty_per_unit`、`Instrument::quote_asset` venue 特判及其他绕过 `InstrumentSpec` 的共享入口。

### 工作流 3：分别修正账户估值和 PNL cashflow

1. `AccountSummarySnapshot` 的 account-wide 字段携带同一个 valuation unit；逐资产余额使用 `AssetAmount`。OKX 读取账户级 USD 字段，不再累加 `details[].availEq/upl` 原生币数量。
2. `AccountMonitor` 校验单位后再计算比例，持久化 baseline 时保存 valuation unit。
3. `TrackPnlRecord` 分别保存 realized PNL、fee、funding 的 amount 和 asset；无法估值的 fee 保留原始值，并让聚合统计显式 `Incomplete`。
4. fee estimate 保持为带来源的调用方策略输入，不放入 `InstrumentSpec`；实际成交 fee 始终以交易所回报为准。

### 工作流 4：稳定客户端订单身份

定义稳定的 `ClientOrderKey`，让各 adapter 负责确定性编码和校验；确实无法确定性恢复时才持久化映射。验收长度、字符集、碰撞、重启恢复和未知交易所 ID。

### 工作流 5：最后统一 server 连接与启动装配

1. 在 `server::assembly` 内定义私有 `ConnectedExchange`（或等价私有元组），携带 `Venue`、`ExchangePorts` 和 `Arc<dyn ExchangeStartupControl>`。
2. 在 server 的一处 `ExchangeConfig` 穷举分支中构造匹配的 ports 和具体 startup control；adapter 不依赖 server，也不返回 `ConnectedExchange`。
3. 合并 [`server/src/assembly.rs`](../server/src/assembly.rs) 与 [`server/src/exchange_startup.rs`](../server/src/exchange_startup.rs) 的重复 venue 分派，并保留各交易所独立配置类型。
4. 增加装配测试，证明任一配置分支不会组合出 venue、ports 和 startup control 不匹配的对象。

### 工作流 6：更新文档并按证据去重

1. 更新 [`README.md`](../README.md) 和 [`docs/system-overview.md`](system-overview.md) 的当前支持状态。
2. 只有在多个 adapter 已经出现相同且稳定的实现后，才抽取分页、时间范围或重试等私有工具。
3. 不把协议字段、签名、WebSocket 事件或交易所错误码搬入统一领域接口。

## 验收标准

完成统一抽象工作的最低标准是：

- 每个已启用标的都有完整且经过验证的 `InstrumentSpec`；它只拥有不可变合约事实，不包含账户费率、汇率或实际 cashflow。
- base、quote、notional、settlement 四类资产以及 position/wire quantity 的边界明确。
- 下单、持仓、开放订单、成交和 PNL 记录使用同一个 canonical position unit；适配器私有 wire quantity 只在端口边界转换一次。
- 持久化成交 qty 同时保存 `position_unit`；contract qty 保存当时的 contract value 及其资产，旧记录不使用当前元数据重算 PNL。
- OKX linear/inverse、Hyperliquid 有效数字与结算单位、各交易所最小订单规则都通过固定数值和 round-trip 契约测试。
- 账户总额携带 valuation unit，逐资产余额携带 asset；不同单位不能直接求和、比较或计算比例。
- realized PNL、fee 和 funding 分别保存 amount 与 asset；未估值费用使聚合结果显式 `Incomplete`，但不阻断有效 realized PNL 的持久化。
- 引擎和运行时除配置/工厂外，不直接依赖具体交易所 crate。
- server 只在一处穷举连接分支组合能力端口和启动控制；`ConnectedExchange` 不进入 engine 或交易所 crate 的公共接口。
- 四个适配器都运行同一套端口契约测试；不支持的能力必须显式声明并在启动前失败。
- 所有“完整快照”都有分页完成或等价证据；无法证明完整时失败。
- 取消请求的接受状态不再被当成终态。
- 最近 PNL 明确请求时间窗，并返回覆盖范围和完整性；启动审计只接受完整结果。
- 账户容量使用指定或实际杠杆，不使用未声明的默认杠杆。
- 结算资产从交易所元数据获得，不在共享领域层继续增加 venue 特判。
- README、系统概览和代码能力状态一致。

## 本次本地验证

本次调研执行了与数量、结算和反向合约直接相关的最小测试，共 20 个测试通过：

- `cargo test -p poise-core settlement_fee`
- `cargo test -p poise-engine inverse_contract`
- `cargo test -p poise-okx inverse`
- `cargo test -p poise-application inverse_contract`
- `cargo test -p poise-server projects_inverse_contract_units_notional_and_pnl_asset`

这些结果证明当前已覆盖的 inverse/settlement 路径没有回归，不代表本文建议的单位模型和跨适配器契约已经实现；数值矩阵中的 linear-with-contract-size、账户多资产和 REST/WS round-trip 仍需新增验收测试。

2026-08-31 的复核修订只调整本文的设计职责、工作顺序和验收标准，没有修改业务代码，也没有重新运行上述测试。

## 官方资料

- [Binance USDⓈ-M Futures General Info](https://developers.binance.com/en/docs/products/derivatives-trading-usds-futures/general-info)：HTTP 503 的不同结果语义，包括执行结果未知时必须查询确认。
- [Binance USDⓈ-M Exchange Information](https://developers.binance.com/en/docs/catalog/core-trading-derivatives-trading-usd-s-m-futures/api/rest-api/market-data)：base/quote/margin asset，以及 tick、step、min qty、min notional；官方说明不能用 precision 字段替代 filter。
- [Binance USDⓈ-M Futures Trade API](https://developers.binance.com/en/docs/catalog/core-trading-derivatives-trading-usd-s-m-futures/api/rest-api/trade)：客户端订单 ID 和开放订单接口约束。
- [Binance USDⓈ-M Account API](https://developers.binance.com/en/docs/catalog/core-trading-derivatives-trading-usd-s-m-futures/api/rest-api/account)：single-asset 与 multi-assets mode 的账户总额估值语义。
- [Bybit Create Order](https://bybit-exchange.github.io/docs/v5/order/create-order)：下单响应是异步接受，需通过 WebSocket 确认。
- [Bybit Cancel Order](https://bybit-exchange.github.io/docs/v5/order/cancel-order)：取消响应是异步接受，需确认最终状态。
- [Bybit Get Open & Closed Orders](https://bybit-exchange.github.io/docs/v5/order/open-order)：开放订单分页和游标约束。
- [Bybit Get Trade History](https://bybit-exchange.github.io/docs/v5/order/execution)：成交历史的分页能力。
- [Bybit Get Instruments Info](https://bybit-exchange.github.io/docs/v5/market/instrument)：base/quote/settle coin、contract type、tick、qty step 和最小名义价值。
- [Bybit Wallet Balance](https://bybit-exchange.github.io/docs/v5/account/wallet-balance)：account-wide totals 的 USD 估值单位与逐币余额。
- [Bybit Execution Stream](https://bybit-exchange.github.io/docs/v5/websocket/private/execution)：exec qty/value/PNL/fee 及 fee currency。
- [OKX API Guide](https://www.okx.com/docs-v5/en/)：`ctType`、`ctVal`、`ctValCcy`、`settleCcy`、contracts 数量、linear/inverse notional/PNL 公式、账户估值、下单、取消和分页约束。
- [Hyperliquid Contract Specifications](https://hyperliquid.gitbook.io/hyperliquid-docs/trading/contract-specifications)：线性永续、USDT 价格标度、USDC 抵押/结算及 1:1 规则。
- [Hyperliquid Tick and Lot Size](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/tick-and-lot-size)：size decimals 与价格有效数字规则。
- [Hyperliquid Asset IDs](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/asset-ids)：默认永续、HIP-3 和环境相关的 wire asset id。
- [Hyperliquid Exchange Endpoint](https://hyperliquid.gitbook.io/Hyperliquid-docs/for-developers/api/exchange-endpoint)：`cloid`、下单参数及响应状态。
- [Hyperliquid Info Endpoint](https://hyperliquid.gitbook.io/Hyperliquid-docs/for-developers/api/info-endpoint)：开放订单、按时间查询成交和最近 10000 条成交的保留限制。
- [Hyperliquid Perpetuals Info](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/info-endpoint/perpetuals)：永续合约元数据、账户状态和资金费历史。
