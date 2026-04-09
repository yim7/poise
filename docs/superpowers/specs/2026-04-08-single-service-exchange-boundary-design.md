# 单服务单交易所 Exchange 边界设计

## 范围

本设计只覆盖当前明确范围：

- 单服务单交易所
- 单账户
- 只考虑加密交易所
- 当前对外 HTTP / WebSocket 协议保持不变

本设计不覆盖：

- 一个服务同时接多个交易所
- A 股、券商柜台或其他非加密交易所市场
- 跨交易所统一 symbol 规范

## 目标

- 明确区分交易所身份和交易所运行时对象
- 让 `server` 只负责装配，不负责交易所协议细节
- 让 `engine` 只依赖稳定的标准化能力
- 让新增第二、第三家交易所时，改动主要集中在 `exchanges/<name>/`
- 让配置边界天然支持交易所特有字段

## 命名

- `Venue`
  - 交易所身份
  - 回答“这是哪家交易所”
  - 用在 `Instrument`、快照、日志和配置分支中
- `Exchange`
  - 运行时总对象
  - 回答“系统当前如何与这家交易所交互”
  - 由多个窄接口组成

这里固定两条规则：

- `Venue` 只表示身份，不表示运行时对象
- `Exchange` 只表示运行时对象，不表示身份枚举

## 总体结构

`Exchange` 作为系统对交易所的唯一总对象，由稳定能力组成。

`Exchange` 的 owner 是装配层，不是 `engine`。

建议形状：

```rust
// server/src/exchange.rs
pub struct Exchange {
    venue: Venue,
    execution: Arc<dyn ExecutionPort>,
    market_data: Arc<dyn MarketDataPort>,
    account_summary: Arc<dyn AccountSummaryPort>,
    account: Arc<dyn AccountPort>,
    metadata: Arc<dyn MetadataPort>,
}
```

`Exchange` 的职责：

- 表达“当前服务接入的交易所”
- 聚合稳定能力

`Exchange` 不负责：

- 暴露原始交易所协议
- 承载交易所特有配置字段
- 替代内部各能力接口的 owner

`Exchange` 不进入 `engine` 的原因：

- `engine` 只需要稳定能力，不需要一个装配层聚合对象
- 把 `Exchange` 放进 `engine` 会形成浅包装边界
- 能力分组变化时，装配层对象不应扩大 `engine` 的改动面

`Exchange` 也不进入 `runtime`、`effect_worker`、`http`、`websocket` 的长期状态。

正式约束：

- `Exchange` 只存在于 `server/src/exchange.rs` 和 `server/src/assembly.rs`
- 装配完成后，其他模块只接收最小 `Port` 或专用 state
- `RuntimeState`、`EffectWorkerState`、`HttpState`、`WebSocketState` 不持有 `Exchange`

## 接口边界

### `ExecutionPort`

负责执行面：

```rust
#[async_trait]
pub trait ExecutionPort: Send + Sync {
    async fn submit_order(&self, req: OrderRequest) -> Result<OrderReceipt>;
    async fn cancel_order(&self, instrument: &Instrument, order_id: &str) -> Result<()>;
    async fn cancel_all(&self, instrument: &Instrument) -> Result<()>;
    async fn get_position(&self, instrument: &Instrument) -> Result<Position>;
    async fn get_open_orders(&self, instrument: &Instrument) -> Result<Vec<ExchangeOrder>>;
}
```

### `MarketDataPort`

负责行情面：

```rust
#[async_trait]
pub trait MarketDataPort: Send + Sync {
    async fn subscribe_prices(&self, instrument: &Instrument) -> Result<mpsc::Receiver<PriceTick>>;
}
```

### `AccountSummaryPort`

负责窄读侧账户摘要：

```rust
#[async_trait]
pub trait AccountSummaryPort: Send + Sync {
    async fn get_account_summary(&self) -> Result<AccountSummarySnapshot>;
}
```

### `AccountPort`

负责账户容量与私有流：

```rust
#[async_trait]
pub trait AccountPort: Send + Sync {
    async fn get_account_capacity_snapshot(
        &self,
        instrument: &Instrument,
    ) -> Result<AccountCapacitySnapshot>;
    async fn subscribe_user_data(&self) -> Result<mpsc::Receiver<UserDataEvent>>;
}
```

### `MetadataPort`

负责规则与时间：

```rust
#[async_trait]
pub trait MetadataPort: Send + Sync {
    async fn get_exchange_info(&self, instrument: &Instrument) -> Result<ExchangeInfo>;
    async fn get_server_time(&self) -> Result<DateTime<Utc>>;
}
```

## 所有权边界

### `poise-core`

负责：

- 纯领域类型和规则

不负责：

- 交易所接入
- 运行时装配

### `poise-engine`

负责：

- `Venue`
- `Instrument`
- 标准化后的订单、账户、行情、用户数据模型
- `ExecutionPort`、`MarketDataPort`、`AccountSummaryPort`、`AccountPort`、`MetadataPort`
- 运行态推进

不负责：

- `Exchange`
- 各家交易所 endpoint、签名、心跳、错误码、重连逻辑

### `poise-server`

负责：

- 读取配置
- 选择当前使用的交易所实现
- 装配 `Exchange`
- 启动 runtime、HTTP、WebSocket

不负责：

- 解释交易所特有凭证结构
- 解释交易所 endpoint 规则
- 持有交易所原始协议知识

### `exchanges/<name>`

负责该交易所的全部接入知识：

- 配置字段
- 鉴权和签名
- REST endpoint
- 公有 / 私有 WebSocket
- 心跳和重连
- 原始 JSON 模型
- 错误码映射
- 到标准化内部模型的映射

交易所 crate 对外暴露的是具体接入实现和已连接组件，不是通用 `Exchange` 类型。

`server` 使用这些组件组装本地 `Exchange` 对象。

## 配置模型

单服务单交易所前提下，交易所配置收敛到服务级。

配置原则：

- 顶层 `exchange` 决定当前服务接入哪家交易所
- `track` 只声明该交易所下的 `symbol`
- `track` 不再重复声明 `venue`
- 交易所部署选择由各交易所配置自己拥有

目标形状：

```toml
[exchange]
venue = "binance"
deployment = "testnet"
api_key = ""
api_secret = ""

[[tracks]]
track_id = "btc-core"
symbol = "BTCUSDT"
```

运行时由顶层 `exchange.venue` 和每条 `track.symbol` 组合出完整 `Instrument`。

这里的 `deployment` 不是共享抽象字段，而是当前交易所配置的一部分。上面的示例是 Binance 形态，不代表所有交易所都使用同一个字段名或同一组取值。

### `environment` 的边界

顶层 `environment` 不属于本设计的 owner。

它的 owner 继续是实例边界设计，见
[`2026-04-07-instance-dir-isolation-design.md`](2026-04-07-instance-dir-isolation-design.md)。

本设计对 `environment` 只施加一条约束：

- 交易所配置不消费它
- `build_exchange(...)` 不接收它
- 交易所 endpoint 和部署选择只能由各交易所自己的配置决定

也就是说：

- `environment` 继续服务于实例本地状态、运行隔离和相关启动语义
- 交易所接入边界不再复用它表达主网 / 测试网 / 沙盒 endpoint 选择

### 交易所特有字段

顶层配置不再维护所有交易所字段的并集。

正式边界：

- 交易所特有字段由对应 `exchanges/<name>` crate 拥有
- `server` 只负责按 `venue` 选择分支

建议形状：

```rust
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "venue", rename_all = "snake_case")]
pub enum ExchangeConfig {
    Binance(poise_binance::Config),
    Bybit(poise_bybit::Config),
    Okx(poise_okx::Config),
    Hyperliquid(poise_hyperliquid::Config),
}
```

每个交易所 crate 自己定义部署与 endpoint 规则。例如：

```rust
// exchanges/binance/src/config.rs
pub struct Config {
    pub deployment: Deployment,
    pub api_key: String,
    pub api_secret: String,
}

pub enum Deployment {
    Mainnet,
    Testnet,
    Custom {
        rest_base_url: String,
        ws_base_url: String,
    },
}
```

endpoint 的决定方式固定为：

- `server` 不解释 endpoint
- `server` 不持有共享 `environment -> endpoint` 映射
- 各交易所 crate 由自己的 `Config` 内部决定 endpoint
- 如需测试或私有部署覆盖，使用该交易所自己的 `Custom` 部署变体

## 装配方式

`server` 的目标是按配置构造 `Exchange`，不直接持有交易所协议知识。

建议入口：

```rust
pub async fn build_exchange(config: &ExchangeConfig) -> Result<Exchange> {
    match config {
        ExchangeConfig::Binance(config) => {
            let connected = poise_binance::connect(config).await?;
            Ok(Exchange::new(
                Venue::Binance,
                connected.execution(),
                connected.market_data(),
                connected.account_summary(),
                connected.account(),
                connected.metadata(),
            ))
        }
        ExchangeConfig::Bybit(config) => {
            let connected = poise_bybit::connect(config).await?;
            Ok(Exchange::new(
                Venue::Bybit,
                connected.execution(),
                connected.market_data(),
                connected.account_summary(),
                connected.account(),
                connected.metadata(),
            ))
        }
        ExchangeConfig::Okx(config) => {
            let connected = poise_okx::connect(config).await?;
            Ok(Exchange::new(
                Venue::Okx,
                connected.execution(),
                connected.market_data(),
                connected.account_summary(),
                connected.account(),
                connected.metadata(),
            ))
        }
        ExchangeConfig::Hyperliquid(config) => {
            let connected = poise_hyperliquid::connect(config).await?;
            Ok(Exchange::new(
                Venue::Hyperliquid,
                connected.execution(),
                connected.market_data(),
                connected.account_summary(),
                connected.account(),
                connected.metadata(),
            ))
        }
    }
}
```

`server` 只知道：

- 当前交易所配置
- 如何把结果装配进 runtime

`server` 不知道：

- 各家交易所的 endpoint
- 各家交易所的凭证字段含义
- 各家交易所的签名和私有流细节

交易所部署选择的 owner 是各交易所配置，而不是共享 `environment` 字符串。

更具体地说：

- `poise_binance::connect(config)` 内部根据 Binance 自己的 `deployment` 决定 REST / WS endpoint
- `poise_okx::connect(config)` 内部根据 OKX 自己的部署配置决定 endpoint
- `poise_hyperliquid::connect(config)` 内部根据 Hyperliquid 自己的部署配置决定 endpoint

`server` 看到的只是“拿到某家交易所配置，获得这家交易所的已连接组件，再组装本地 `Exchange`”，不会参与 endpoint 推导。

## 交易所 crate 结构

每个交易所 crate 使用同一职责结构：

```text
exchanges/binance/src/
  lib.rs
  config.rs
  connected.rs
  mapper.rs
  rest/
    auth.rs
    client.rs
    models.rs
  ws/
    market.rs
    account.rs
    models.rs
```

职责如下：

- `config.rs`
  - 交易所配置模型
- `connected.rs`
  - 组装本交易所的已连接组件
- `mapper.rs`
  - 原始类型到标准化内部模型的映射
- `rest/*`
  - REST 调用、签名、错误处理
- `ws/market.rs`
  - 公有行情流
- `ws/account.rs`
  - 私有账户流

## 当前代码的直接收敛点

- [`engine/src/ports.rs`](../../../engine/src/ports.rs)
  - 拆分当前 `ExchangePort`
- [`engine/src/track.rs`](../../../engine/src/track.rs)
  - 保留 `Venue` 作为身份枚举
- [`server/src/config.rs`](../../../server/src/config.rs)
  - 删除 `track.venue`
  - 引入服务级 `exchange`
- [`server/src/assembly.rs`](../../../server/src/assembly.rs)
  - 从直接构造 Binance 改成按配置 `build_exchange(...)`
- `server/src/exchange.rs`
  - 新建装配层 `Exchange` 对象
- `exchanges/<name>/src/connected.rs`
  - 只组装本交易所的已连接组件，不创建通用 `Exchange`
- [`server/src/runtime/guards.rs`](../../../server/src/runtime/guards.rs)
  - 单服务单交易所前提下，不再按 `venue` 分桶维护账户保护状态

## 迁移顺序

1. 固定命名和 owner：`Venue` / `Exchange`
2. 拆分 `ExchangePort`
3. 调整配置为服务级 `exchange`
4. 把 Binance endpoint 和凭证解释从 `server` 下移到 `exchanges/binance`
5. 将 `exchanges/binance` 收敛为 `config` / `rest` / `ws` / `mapper` / `connected` 结构
6. 在 Binance 路径稳定后，再接第二家交易所

## 设计结论

本设计的最终边界如下：

- `Venue` 表示交易所身份
- `Exchange` 表示系统当前接入的交易所对象
- `Exchange` 由窄接口组成，不再使用一个过宽的总 trait
- 配置按服务级声明交易所，`track` 不再重复声明 `venue`
- 各交易所特有配置字段由对应交易所 crate 拥有
- `server` 负责把交易所已连接组件装配成 `Exchange`
- 交易所原始协议知识只存在于 `exchanges/<name>`
