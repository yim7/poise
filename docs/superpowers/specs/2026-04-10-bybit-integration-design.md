# Bybit USDT 永续测试网接入设计

## 背景

当前仓库已经把交易所边界收敛到单服务单交易所：

- `server` 负责读取配置和装配运行时
- `engine` 只依赖稳定的交易所端口
- `exchanges/binance/` 拥有 Binance 的全部协议知识

现阶段要新增第二家交易所实现，但不改变“单个服务实例只接一家交易所”的运行边界。目标交易所固定为 Bybit，范围固定为 Bybit V5 的 USDT 永续测试网。

本设计建立在现有交易所边界设计之上，见 [`2026-04-08-single-service-exchange-boundary-design.md`](2026-04-08-single-service-exchange-boundary-design.md)。

## 范围

本设计只覆盖以下范围：

- 新增 `poise-bybit` crate
- 接入 Bybit V5
- 只支持 `linear`，也就是 USDT 永续
- 支持测试网和主网
- 支持全量交易闭环：
  - 行情订阅
  - 交易规则和服务器时间
  - 账户摘要
  - 仓位和挂单查询
  - 下单、撤单、按 symbol 全撤
  - 私有流订单/仓位更新

## 非目标

本设计明确不做以下事情：

- 不接入 OKX、Hyperliquid 或其他交易所
- 不支持 Bybit 现货、反向合约、期权、USDC 合约
- 不支持 demo trading
- 不抽共享 CEX 底座
- 不引入统一 symbol 翻译层
- 不改 `engine` 现有 `Port` 语义
- 不处理 hedge mode，首版只支持 one-way mode
- 不处理 portfolio margin 等需要单独风险语义的账户模式
- 不在这一版拆分 `reference_price`、`mark_price`、`index_price` 的内部状态

## 目标

- 在不破坏现有 Binance 主线的前提下，新增 Bybit 接入
- 让 Bybit 的协议知识尽量收敛在 `exchanges/bybit/`
- 保持 `server` 和 `engine` 的改动面最小
- 保持配置模型和现有单服务单交易所边界一致
- 让后续接入 OKX 时，共享层不需要再次大改

## 结论

采用以下边界：

1. 新增 `exchanges/bybit/` crate，结构对齐现有 `exchanges/binance/`
2. `engine` 只新增 `Venue::Bybit`，不改现有端口定义
3. `server` 只新增 `ExchangeConfig::Bybit` 和装配分支
4. `tracks[].symbol` 继续使用交易所原生值，Bybit 首版使用 `BTCUSDT` 这类线性永续 symbol
5. Bybit 首版策略价格口径继续对齐当前 Binance 语义：
   - `reference_price = mark_price`
   - `index_price` 不进入内部运行时
6. 价格字段真实分离作为后续独立任务处理，不和这次 Bybit 接入绑定

## 共享层边界

### `poise-engine`

`poise-engine` 只承担稳定抽象：

- `Venue` 新增 `Bybit`
- `Instrument { venue, symbol }` 形状不变
- `ExecutionPort`、`MarketDataPort`、`AccountSummaryPort`、`AccountPort`、`MetadataPort` 形状不变

不负责：

- Bybit 的 endpoint
- Bybit 的鉴权签名
- Bybit 的 category、position mode、私有流消息形状

### `poise-server`

`poise-server` 只承担装配：

- `ExchangeConfig` 新增 `Bybit`
- `build_exchange(...)` 新增 Bybit 分支
- 配置解析仍然以 `[exchange]` 顶层分支决定当前实例接哪家交易所

不负责：

- 解释 Bybit V5 的请求签名规则
- 解释 Bybit 的 websocket topic
- 持有任何 Bybit 原始协议知识

### `poise-application` / `poise-protocol` / `poise-tui`

这几个层级不新增交易所特有概念。

它们继续消费现有读模型和协议字段：

- `status.reference_price`
- `market.mark_price`
- `market.index_price`

本次不改变协议字段集合，只让 Bybit 首版先对齐当前 Binance 的价格语义。

## Bybit Owner

`exchanges/bybit/` 拥有 Bybit 的全部接入知识：

- V5 REST endpoint
- V5 公有/私有 websocket endpoint
- HMAC 鉴权签名
- `linear` category 固定值
- `symbol` 原生语义
- one-way mode 约束
- 账户和仓位接口字段映射
- 私有流订单/仓位更新映射

建议模块结构：

```text
exchanges/bybit/src/
  config.rs
  connected.rs
  mapper.rs
  lib.rs
  rest/
    auth.rs
    client.rs
    models.rs
  ws/
    account.rs
    market.rs
    models.rs
```

## 配置模型

### 服务级交易所配置

Bybit 继续使用现有服务级配置边界：

```toml
[exchange]
venue = "bybit"
deployment = "testnet"
api_key = ""
api_secret = ""

[[tracks]]
track_id = "btc-core"
symbol = "BTCUSDT"
lower_price = 90000.0
upper_price = 110000.0
long_exposure_units = 8.0
short_exposure_units = 8.0
notional_per_unit = 375.0
daily_loss_limit = 300.0
total_loss_limit = 600.0
```

### `poise-bybit::Config`

`poise-bybit::Config` 自己拥有 Bybit 的部署配置，不让 `server` 解释 endpoint。

建议形状：

```rust
pub struct Config {
    pub deployment: Deployment,
    pub api_key: Option<String>,
    pub api_secret: Option<String>,
}

pub enum Deployment {
    Mainnet,
    Testnet,
}
```

`Deployment` 负责返回：

- REST base URL
- public WS base URL
- private WS base URL

本次不增加 `custom` 部署配置，避免在第二家交易所接入阶段引入未被需求验证的变体。

## 账户与模式约束

Bybit 首版固定以下运行约束：

- 只支持 `linear`
- 只支持 one-way mode，也就是 `positionIdx = 0`
- 只支持 `UNIFIED` 账户摘要接口可覆盖的账户形态
- 只支持能返回可用余额摘要的账户模式

出现以下情况时，系统应显式失败，而不是隐式降级：

- `positionIdx != 0`
- 账户余额接口无法按 `UNIFIED` 语义返回所需字段
- 无法得到可用于账户摘要和容量快照的余额字段

这条约束的目的不是覆盖 Bybit 全部账户模型，而是避免把 one-way / hedge、普通保证金 / portfolio margin 的复杂度扩散到共享层。

## 端口映射

### `MetadataPort`

`MetadataPort` 使用 Bybit V5 的市场信息接口。

职责：

- 读取单个 `linear` symbol 的规则
- 映射 `tickSize`、`qtyStep`、`minOrderQty`、`minNotionalValue`
- 读取服务器时间

规则映射：

- `price_tick <- priceFilter.tickSize`
- `quantity_step <- lotSizeFilter.qtyStep`
- `min_qty <- lotSizeFilter.minOrderQty`
- `min_notional <- lotSizeFilter.minNotionalValue`

手续费率在首版沿用固定默认值，不在这次接入里引入额外费率查询。

### `MarketDataPort`

`MarketDataPort` 使用 Bybit V5 public websocket：

- 线性永续 public stream
- topic 使用 `tickers.{symbol}`

映射规则：

- `PriceTick.reference_price <- markPrice`
- `PriceTick.mark_price <- markPrice`
- `PriceTick.timestamp <- websocket message ts`

首版不把 `indexPrice` 写入内部运行时，因为当前共享价格模型还没有独立字段承载它。

### `ExecutionPort`

`ExecutionPort` 使用 Bybit V5 交易 REST 接口：

- `submit_order` -> create order
- `cancel_order` -> cancel order
- `cancel_all` -> cancel all orders，按 symbol 执行
- `get_position` -> get position info
- `get_open_orders` -> get open orders

首版下单固定：

- `category = linear`
- `orderType = Limit`
- `timeInForce = GTC`
- `positionIdx = 0`
- `orderLinkId <- client_order_id`
- `reduceOnly <- OrderRequest.reduce_only`

Bybit 下单确认是异步语义。REST 只表示请求已被接受，真实状态以后续 private websocket 更新为准。

### `AccountSummaryPort`

`AccountSummaryPort` 使用账户余额接口，首版固定读取 `UNIFIED` 账户维度摘要。

映射目标：

- `equity`
- `available`
- `unrealized_pnl`
- `observed_at`

### `AccountPort`

`AccountPort` 负责两件事：

1. 账户容量快照
2. 私有流订阅

账户容量快照建议按保守语义计算：

- `available <- totalAvailableBalance`
- `max_increase_notional <- available`

这条决策是有意和当前 Binance 实现分开的：

- 当前 Binance 会把 `available * leverage` 作为近似上界
- Bybit 如果复用同样语义，就会把交易所侧 symbol leverage 设置变成启动前提
- 首版先使用更保守但 owner 清晰的容量语义，避免把外部 leverage 配置引入共享启动边界

代价是：

- Bybit 首版的启动前 `max_notional` 校验会更严格
- 某些在交易所侧实际可开的仓位，首版会因为保守校验被提前拒绝

私有流首版只映射：

- 订单更新
- 仓位更新

不在这次接入里引入额外的成交流优化、快速执行流或 websocket 下单。

## 数据映射

### Symbol

- `tracks[].symbol` 继续使用交易所原生值
- Bybit 首版直接使用 `BTCUSDT`、`ETHUSDT`
- 不增加 symbol 归一化层

### Position

`Position.qty` 继续沿用现有语义，直接使用 Bybit `size` 的数量值。

在 one-way mode 下：

- `Buy` 仓位映射为正数量
- `Sell` 仓位映射为负数量
- 空仓映射为 `0`

### Order

Bybit 订单映射保持现有稳定字段：

- `order_id`
- `client_order_id`
- `side`
- `price`
- `qty`
- `status`

不把 Bybit 特有订单字段泄漏到共享层。

## 价格口径决策

当前系统内部只把一个价格口径作为策略输入，即 `reference_price`。

这次 Bybit 接入明确采用以下决策：

- 不修改现有价格模型
- Bybit 首版与 Binance 对齐，使用 `markPrice` 驱动策略
- `market.mark_price` 和 `market.index_price` 的真实分离延后处理

原因：

- 当前 Binance 也是 `reference_price = mark_price`
- 如果只在 Bybit 上先拆价格字段，会造成 Binance 和 Bybit 行为不一致
- 价格字段真实分离会穿透 `engine`、`application`、`server`、`protocol`、`tui`，应该作为单独任务统一处理

## 模块改动面

### 新增

- `exchanges/bybit/Cargo.toml`
- `exchanges/bybit/src/lib.rs`
- `exchanges/bybit/src/config.rs`
- `exchanges/bybit/src/connected.rs`
- `exchanges/bybit/src/mapper.rs`
- `exchanges/bybit/src/rest/auth.rs`
- `exchanges/bybit/src/rest/client.rs`
- `exchanges/bybit/src/rest/models.rs`
- `exchanges/bybit/src/ws/account.rs`
- `exchanges/bybit/src/ws/market.rs`
- `exchanges/bybit/src/ws/models.rs`

### 修改

- `Cargo.toml`
- `engine/src/track.rs`
- `server/Cargo.toml`
- `server/src/config.rs`
- `server/src/assembly.rs`
- `README.md`

必要时补充：

- Bybit testnet 示例 config
- 协议/投影层中关于 `venue` 展示的测试

## 验收测试

本次必须先写失败测试，再实现。

### 配置与装配

- `server/src/config.rs`
  - 能解析 `[exchange] venue = "bybit"`
  - `tracks[].symbol = "BTCUSDT"` 能进入 `ConfiguredTrackInput`
- `server/src/assembly.rs`
  - `build_exchange(...)` 能为 `ExchangeConfig::Bybit` 走到 `poise_bybit::connect(...)`
  - 组装后的 `Exchange.venue()` 为 `Venue::Bybit`

### `poise-bybit` crate

- `config.rs`
  - testnet / mainnet endpoint 解析正确
  - 缺失凭证时报错稳定
- `mapper.rs`
  - instrument info -> `ExchangeInfo`
  - position -> `Position`
  - open order / order update -> `ExchangeOrder`
  - 账户余额摘要 -> `AccountSummarySnapshot`
  - `positionIdx != 0` 的仓位更新按稳定错误处理
  - 缺失 `UNIFIED` 必需字段的账户余额摘要按稳定错误处理
- `rest/auth.rs`
  - HMAC 签名符合 Bybit V5 规则
- `ws/market.rs`
  - ticker 消息能解析出 `markPrice`
- `ws/account.rs`
  - private order / position 消息能映射成现有 `UserDataEvent`
- `connected.rs`
  - `connect(...)` 暴露全部必需端口

### 工作区验收

至少需要通过：

- `cargo test -p poise-bybit`
- `cargo test -p poise-server`

如果 Bybit 接入引发 workspace 级公共边界改动，再补跑对应 workspace 测试。

## 风险与约束

### 异步确认语义

Bybit 订单创建和取消的 REST 应答不是最终状态。系统必须继续以 private websocket 为准做订单状态确认。

### 账户模式约束

Bybit 的账户和仓位模式复杂度高于当前共享层语义。首版通过“只支持 one-way、只支持 `UNIFIED` 摘要可覆盖的账户模式”来明确裁剪范围。

### 价格字段债务

`market.index_price` 当前继续不能表达真实 index price。这个债务本次保留，但必须在后续单独任务里统一处理，不能再在单个交易所接入里局部修补。
