# OKX 合约接入设计

目标：新增 OKX 交易所适配，只覆盖 Poise 当前运行需要的永续合约能力，不引入 OKX SDK 作为长期核心依赖。

## 范围

- 只支持 OKX `SWAP` 永续合约。
- `symbol` 使用 OKX 原生合约 ID，例如 `BTC-USDT-SWAP`。
- 默认交易模式为 `cross`，持仓模式按 `net` 处理。
- 配置字段为 `venue = "okx"`、`deployment`、`api_key`、`api_secret`、`passphrase`。
- `deployment = "mainnet"` 使用生产 REST 和 WebSocket；`deployment = "demo"` 使用 OKX demo WebSocket，并在 REST 私有请求加入 `x-simulated-trading: 1`。
- 不支持 spot、margin、futures 交割合约、option、提现、划转、资金账户操作、策略单、WebSocket 下单。

## 架构

新增 `exchanges/okx` crate，沿用现有交易所适配器边界。OKX 协议细节全部封装在该 crate 内，`core`、`engine`、`server` 只通过现有 port 访问。

模块划分：

- `config.rs`：部署环境、端点、凭证校验。
- `rest/auth.rs`：OKX REST 和 WebSocket 登录签名。签名输入为 `timestamp + method + requestPath + body`，算法为 HMAC-SHA256 后 Base64。
- `rest/models.rs`：OKX envelope、instrument、balance、position、open order、order ack、server time 等响应模型。
- `rest/client.rs`：封装 REST 请求、认证 header、demo header、错误 envelope 处理和端口需要的查询/写操作。
- `mapper.rs`：把 OKX REST / WebSocket 数据映射到 Poise `ExchangeInfo`、`AccountSummarySnapshot`、`Position`、`ExchangeOrder`、`UserDataEvent` 等类型。
- `ws/mod.rs`、`ws/market.rs`、`ws/account.rs`、`ws/models.rs`：WebSocket 连接、登录、订阅、解析和断线重连。
- `connected.rs`：实现 `ExecutionPort`、`MarketDataPort`、`AccountSummaryPort`、`AccountPort`、`MetadataPort`。
- `startup_control.rs`：实现启动阶段 `SymbolLeverageSetter`。

`server` 侧只增加：

- `ExchangeConfig::Okx`
- `build_exchange` 的 OKX 分支
- `build_symbol_leverage_setter` 的 OKX 分支
- `Venue::Okx` 和 `as_str() = "okx"`

## REST 能力

`OkxRestClient` 提供以下能力：

- `get_exchange_info(symbol)`：调用 `GET /api/v5/public/instruments?instType=SWAP&instId=...`。
- `get_account_summary()`：调用 `GET /api/v5/account/balance`。
- `get_account_capacity_snapshot(symbol)`：使用 OKX 可用权益和当前 leverage 估算可增仓 notional；如果 OKX 在当前账户模式下无法返回足够字段，则返回明确错误。
- `get_position(symbol)`：调用 `GET /api/v5/account/positions?instType=SWAP&instId=...`，按 `net` 持仓映射正负数量。
- `get_open_orders(symbol)`：调用 `GET /api/v5/trade/orders-pending?instType=SWAP&instId=...`。
- `submit_order(req)`：调用 `POST /api/v5/trade/order`，固定 `tdMode = "cross"`、`ordType = "limit"`。
- `cancel_order(symbol, order_id)`：调用 `POST /api/v5/trade/cancel-order`。
- `cancel_all(symbol)`：先查当前 symbol open orders，再使用批量撤单接口撤销。
- `set_leverage(symbol, leverage)`：调用 `POST /api/v5/account/set-leverage`，固定 `mgnMode = "cross"`。
- `get_server_time()`：优先调用 OKX public time endpoint，避免本地时钟漂移影响私有请求。

REST 错误处理以 OKX envelope 为准：`code == "0"` 才视为成功，否则错误信息包含 path、code、msg 和请求上下文。

## WebSocket 能力

行情连接使用 public WebSocket：

- 订阅 `tickers`，映射 best bid / best ask 到 `ExecutionQuoteTick`。
- 订阅 mark price channel，映射到 `MarkPriceTick`。

用户连接使用 private WebSocket：

- 建连后发送 `login`，使用 OKX WebSocket 认证签名。
- 登录成功后订阅 `orders`，映射订单状态和成交增量。
- 订阅 `positions` 或 account/balance-position 相关频道，补齐持仓更新。
- 对可从 `orders` channel 取得的 fill 信息写入 `TrackPnlRecord::trade`。
- 资金费如果能从稳定的 account/bill 推送归属到具体 `instId`，则写入 `TrackPnlRecord::funding`；否则不进入 track PNL，避免错误归属。
- 连接断开、close frame、reset without close handshake 后，后台任务退避重连、重新登录并重新订阅。

## 映射规则

- `Side::Buy -> "buy"`，`Side::Sell -> "sell"`。
- OKX `pos` 在 `net` 模式下直接作为 signed quantity；正数为多，负数为空。
- `avgPx` 为空且仓位为零时映射为 `0.0`；非零仓位缺字段时报错。
- 订单状态：
  - `live`、`partially_filled` -> active order
  - `filled` -> `OrderStatus::Filled`
  - `canceled`、`mmp_canceled` -> `OrderStatus::Canceled`
  - 其他状态显式报错，不静默吞掉。
- `ExchangeRules` 使用 OKX instrument 字段：
  - `tickSz -> price_tick`
  - `lotSz -> quantity_step`
  - `minSz -> min_qty`
  - `min_notional` 如果 OKX 对 SWAP 未提供稳定字段，先按 `0.0` 或本地保守默认处理，并在 mapper 测试中固定该选择。

## 测试策略

先按 TDD 加测试，再实现。

最小测试入口：

- `cargo test -p poise-core track::tests::venue_as_str_supports_okx`
- `cargo test -p poise-okx config::tests::`
- `cargo test -p poise-okx rest::auth::tests::`
- `cargo test -p poise-okx mapper::tests::`
- `cargo test -p poise-okx rest::client::tests::`
- `cargo test -p poise-okx ws::tests::`
- `cargo test -p poise-okx connected::tests::`
- `cargo test -p poise-server config::tests::parses_okx_exchange_config`
- `cargo test -p poise-server assembly::tests::`
- `cargo test -p poise-server exchange_startup::tests::`

最终验收至少运行：

- `cargo test -p poise-okx`
- `cargo test -p poise-core track::tests::venue_as_str_supports_okx`
- `cargo test -p poise-server config::tests::`
- `cargo test -p poise-server assembly::tests::`
- `cargo test -p poise-server exchange_startup::tests::`

## 待确认风险

- OKX 账户模式差异会影响 `cross` / `net` 行为。适配器只支持 `net` 模式；如果账户是 long/short mode，应在启动或持仓解析时给出明确错误。
- OKX demo REST 与生产 REST 共用域名，私有请求必须带 `x-simulated-trading: 1`。这个行为必须有请求形状测试。
- WebSocket fills channel 有账户等级限制，交易 PNL 优先从 `orders` channel 的成交字段派生，避免依赖受限频道。
- 资金费归属如果缺少稳定实时推送，不应伪造；可以后续通过账单轮询补齐。
