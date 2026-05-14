# Hyperliquid HIP-3 接入设计 Spec

## 背景

当前 Hyperliquid 适配器只支持默认 perpetual dex。启动 `cbrs` track 时，server 会按 track 下发 startup leverage，Hyperliquid REST client 先用 `/info {"type":"meta"}` 查 `meta.universe[].name == symbol`，再用数组下标作为 `updateLeverage.asset`。HIP-3 市场不在默认 universe 中，因此 `symbol = "CBRS"` 会失败为：

```text
missing Hyperliquid asset `CBRS`
```

Hyperliquid HIP-3 builder-deployed perps 的差异点：

- info endpoint 的 perps 请求支持 `dex` 字段；空字符串或省略表示默认 perp dex。
- HIP-3 coin 使用 `{dex}:{coin}` wire name，例如主网 `xyz:CBRS`。
- HIP-3 exchange action 的 `asset` id 不是 `index_in_meta`，而是 `100000 + perp_dex_index * 10000 + index_in_meta`。
- HIP-3 市场可能是 `onlyIsolated` / `strictIsolated`，不能假设所有资产都可以 cross margin。

参考文档：

- [Hyperliquid API: info perpetuals endpoint](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/info-endpoint/perpetuals) 支持 `dex`。
- [Hyperliquid API: asset ids](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/asset-ids) 记录 builder-deployed perps asset id 公式。
- [HIP-3: builder-deployed perpetuals](https://hyperliquid.gitbook.io/hyperliquid-docs/hyperliquid-improvement-proposals-hips/hip-3-builder-deployed-perpetuals) 说明 HIP-3 交易沿用 HyperCore action，但 asset id 需要按 HIP-3 schema 设置。

## 目标

- 支持单个 Poise Hyperliquid 实例同时配置默认 perps 和 HIP-3 perps。
- 允许配置并交易 HIP-3 symbol，例如 `xyz:CBRS`。
- 保持 `core`、`engine`、`application` 和 `server` 的交易所无关边界不扩大。
- 启动期杠杆、交易规则、持仓、挂单、下单、撤单、cancel all 和行情订阅按 instrument symbol 使用正确的 dex 上下文。
- 账户可用资产按 Hyperliquid 账户级共享余额处理，不按默认 perps / HIP-3 dex 拆成多套 Poise 账户。
- 对默认 Hyperliquid perps 保持现有配置兼容。

## 非目标

- 不在 exchange config 里声明单一 `perp_dex`。
- 不新增 spot、vault 运维、HIP-3 deployer action、oracle 更新、资金划转或提现能力。
- 不在 `core::Instrument` 中加入 dex 字段。
- 不解决非 USDC collateral 的完整容量估算；先按当前账户余额口径保守延续，必要时在实现中显式标注限制。

## 主导复杂度信号

主导复杂度是 `change amplification`。HIP-3 引入的 dex 选择、coin wire name、asset id 公式和保证金模式如果散落到 server 装配、启动杠杆、REST 每个方法和 WS 调用方中，后续每个交易动作都会重复携带同一组特殊规则。

设计选择是让 `exchanges/hyperliquid` 拥有全部 HIP-3 协议知识。调用方仍只传 `Instrument { venue: Hyperliquid, symbol }`，Hyperliquid adapter 在内部从 symbol 解析 dex 上下文，并结合 meta 把 symbol 解析成交易所 wire action 所需的 asset descriptor。

## 配置设计

不新增 Hyperliquid exchange 级 `perp_dex` 字段。Hyperliquid 的 `symbol` 继续使用交易所 wire name：

- 默认 perps 使用 `BTC`、`ETH` 这类 coin 名称。
- HIP-3 perps 使用 `{dex}:{coin}`，例如 `xyz:CBRS`。

同一个 Hyperliquid 实例可以同时包含两类 track：

```toml
[exchange]
venue = "hyperliquid"
deployment = "mainnet"
private_key = "0x..."
wallet_address = "0x..."

[[tracks]]
track_id = "btc-core"
symbol = "BTC"
leverage = 10
lower_price = 90000.0
upper_price = 110000.0
long_exposure_units = 1.0
short_exposure_units = 1.0
notional_per_unit = 100.0
daily_loss_limit = 100.0
total_loss_limit = 200.0

[[tracks]]
track_id = "cbrs"
symbol = "xyz:CBRS"
leverage = 3
lower_price = 90.0
upper_price = 110.0
long_exposure_units = 1.0
short_exposure_units = 1.0
notional_per_unit = 100.0
daily_loss_limit = 100.0
total_loss_limit = 200.0
```

字段语义：

- `symbol` 不含 `:`：按默认 Hyperliquid perp dex 查询和交易。
- `symbol` 含一个 `:`：按 HIP-3 wire name 解析，冒号前为 dex name，完整 symbol 原样作为 coin name 传给交易所。
- `symbol` 含多个 `:`：视为无效 Hyperliquid HIP-3 symbol。Poise 当前只接受 `{dex}:{coin}`，不尝试猜测或修复更复杂的 wire name。

校验：

- `server::config::parse_config` 不理解 HIP-3 dex 规则，只继续做通用 track 校验。
- Hyperliquid adapter 在首次解析 symbol 时校验 HIP-3 wire name：冒号前后都不能为空，dex name 只允许 ASCII 可见字符，且 symbol 只能包含一个冒号。
- 错误信息应直接包含 symbol，例如：

```text
invalid Hyperliquid HIP-3 symbol `xyz:`: expected `{dex}:{coin}`
```

## Ownership 边界

### Hyperliquid adapter 拥有的知识

- 如何从 symbol 解析默认 dex 或 HIP-3 dex 上下文。
- 如何把当前 symbol 对应的 dex 注入 info endpoint body。
- 如何按 dex name 查询 `perpDexs` 并映射为 dex index。
- 默认 perps 和 HIP-3 perps 的 asset id 公式。
- `MetaResponse` 中 `onlyIsolated`、`marginMode`、`maxLeverage` 对 leverage action 的影响。
- WS market subscription 使用 `{dex}:{coin}` wire name；账户级 user stream subscription 不带 dex。

### Server 拥有的知识

- TOML schema 到 `TrackDefinition` 的投影。
- 单实例单 exchange 的配置校验。
- 启动期按 track 调用 `SymbolLeverageSetter`。

### Core / Engine 不新增知识

`Instrument.symbol` 继续表示交易所原生 symbol。对于 HIP-3，它就是 `xyz:CBRS`。策略、风险、执行规划不需要理解 dex。

## REST 设计

### 新增内部概念

在 `exchanges/hyperliquid/src/rest/client.rs` 内部引入私有结构：

```rust
enum PerpDexRef<'a> {
    Default,
    Hip3 { dex: &'a str },
}

struct AssetDescriptor {
    id: u32,
    sz_decimals: u32,
    max_leverage: Option<u32>,
    leverage_is_cross: bool,
}
```

`PerpDexRef` 不是持久对象，也不进入公共 API；它只是模块内部的解析结果，用于避免每个 REST 方法重复判断 symbol 是否为 HIP-3。

可选的模块函数：

```rust
fn parse_perp_dex(symbol: &str) -> Result<PerpDexRef<'_>>
fn with_perp_dex(body: serde_json::Value, dex: PerpDexRef<'_>) -> serde_json::Value
fn hip3_asset_id(perp_dex_index: u32, index_in_meta: u32) -> u32
```

这些函数不需要暴露到公共 API。

### 协议模型扩展

当前 `PerpAssetMeta` 只反序列化 `name` 和 `szDecimals`。HIP-3 接入需要在同一个 wire model 上补充可选字段：

```rust
struct PerpAssetMeta {
    name: String,
    #[serde(rename = "szDecimals")]
    sz_decimals: u32,
    #[serde(rename = "maxLeverage", default)]
    max_leverage: Option<u32>,
    #[serde(rename = "onlyIsolated", default)]
    only_isolated: Option<bool>,
    #[serde(rename = "marginMode", default)]
    margin_mode: Option<String>,
}
```

字段语义：

- `maxLeverage` 用于启动期 leverage 前置校验。字段存在且配置 leverage 超过它时，本地直接返回清晰错误，不再发交易所 action。
- `onlyIsolated` 和 `marginMode` 用于决定 `updateLeverage.isCross`。
- `marginMode` 先保留为 `Option<String>`，不要过早做成封闭 enum。原因是交易所可能增加新字符串；对未知非空值应 fail closed 并报告原始值。
- 这些字段都保持 optional，以兼容旧测试 fixture 和默认 perps 中没有限制字段的资产。

### Info 请求

以下请求要按当前 symbol 的 dex 上下文决定是否带 `dex`：

- `meta`
- `clearinghouseState`
- `openOrders`
- 其他 instrument-scoped perps 状态请求

`spotClearinghouseState`、`userAbstraction` 和账户容量查询不带 dex，因为它们表达账户级共享资产或账户模式，不是某个 perp dex 的市场查询。

实现时不要让一个 `user_state()` helper 同时服务两种语义。建议拆成：

- `user_state_for_symbol(symbol)`：按 symbol 解析 dex，body 带对应 `dex`，只用于持仓、per-dex PnL 明细等 instrument-scoped 查询。
- `account_state()` 或 `account_balance_state()`：不带 dex，只用于账户级摘要或容量口径。
- `open_orders_for_symbol(symbol)`：按 symbol 解析 dex，body 带对应 `dex`，再按 `order.coin == symbol` 过滤。

### Meta 和 dex index 缓存

当前 REST client 的 `meta_cache: OnceCell<MetaResponse>` 只能缓存默认 dex 的 universe。支持默认 perps 和 HIP-3 混配后，meta 必须按 dex 上下文分桶缓存，否则先查询 `BTC` 会把默认 universe 缓住，后续 `xyz:CBRS` 会继续查默认 universe；反过来也会失败。

推荐替换为可按 key 增量填充的缓存：

```rust
meta_cache: tokio::sync::Mutex<HashMap<PerpDexKey, MetaResponse>>
perp_dex_index_cache: tokio::sync::Mutex<HashMap<String, u32>>
```

`PerpDexKey` 保持 crate 私有，可表示默认 dex 和 HIP-3 dex name：

```rust
enum PerpDexKey {
    Default,
    Hip3(String),
}
```

缓存语义：

- 默认 dex 的 meta 请求不带 `dex`，缓存到 `PerpDexKey::Default`。
- HIP-3 dex 的 meta 请求带 `dex`，按 dex name 缓存到 `PerpDexKey::Hip3(dex)`。
- 同一个 REST client 内查询 `BTC`、`xyz:CBRS`、`abc:FOO` 时，三份 meta 互不覆盖。
- 不要在持有 `Mutex` 锁时执行 HTTP 请求。实现应先短锁读取缓存；未命中时释放锁、请求远端，再短锁写入。并发下重复请求可以接受，缓存 key 错误不可接受。
- 不使用 `OnceCell<HashMap<...>>` 作为最终实现形态，因为它容易变成“初始化一次后不能按新 dex 追加”的浅缓存。

Dex index 缓存：

- 默认 dex 不查询 dex index，asset id 继续使用 `index_in_meta`。
- HIP-3 dex 通过 `/info {"type":"perpDexs"}` 查找 `value.name == dex` 的数组下标。
- 缓存按 dex name 存储，允许同一个实例内同时出现 `BTC` 和 `xyz:CBRS`，也允许未来多个 HIP-3 dex 共存。
- 找不到时返回 `missing Hyperliquid perp dex `xyz``。

### Asset descriptor

`asset_descriptor(symbol)` 改为：

1. 从 symbol 解析 dex 上下文：`BTC` -> default，`xyz:CBRS` -> HIP-3 dex `xyz`。
2. 获取当前 dex 的 `meta`。
3. 在 `meta.universe` 里精确匹配 `asset.name == symbol`。
4. 默认 dex：`id = index_in_meta`。
5. HIP-3 dex：`id = 100000 + perp_dex_index * 10000 + index_in_meta`。
6. 从 meta 派生 `leverage_is_cross`：
   - 默认 perps 保持 `true`。
   - HIP-3 如果 `onlyIsolated == true`，使用 `false`。
   - HIP-3 如果 `marginMode == "noCross"` 或 `marginMode == "strictIsolated"`，使用 `false`。
   - HIP-3 如果 `marginMode` 缺失且 `onlyIsolated` 缺失或为 `false`，视为没有显式 isolated 限制，使用 `true`。主网 `xyz` dex 中存在这种形态的 HIP-3 标的。
   - HIP-3 如果出现未知的非空 `marginMode`，fail closed，不发送不确定的 cross leverage action。
7. 如果 meta 提供 `maxLeverage`，校验配置 leverage 不超过 `maxLeverage`。

错误信息应包含 dex 上下文，例如：

```text
missing Hyperliquid asset `xyz:CBRS` in perp dex `xyz`
```

### Leverage action

当前 `set_leverage` 固定 `is_cross: true`。改为使用 `AssetDescriptor`：

```rust
UpdateLeverageAction {
    asset: descriptor.id,
    is_cross: descriptor.leverage_is_cross,
    leverage,
}
```

发送 action 前先校验 leverage：

```rust
if let Some(max_leverage) = descriptor.max_leverage {
    ensure!(leverage <= max_leverage, "...");
}
```

如果未来需要手动选择 isolated/cross，可再加配置字段；本次先让 adapter 根据 meta 自动选择，避免调用方理解交易所细节。

### 下单、撤单和 cancel all

下单、撤单、cancel all 都复用 `asset_descriptor` / `asset_id`，因此 HIP-3 asset id 只在一个位置实现。

`get_open_orders(symbol)` 查询 body 按 symbol 带 dex，然后仍按 `order.coin == symbol` 过滤。对于 HIP-3，symbol 是 `xyz:CBRS`。

`get_position(symbol)` 查询 body 按 symbol 带 dex，然后仍按 `position.coin == symbol` 匹配。

## WebSocket 设计

市场流当前订阅：

```json
{"type":"bbo","coin": symbol}
{"type":"activeAssetCtx","coin": symbol}
```

设计选择：

- 对 HIP-3，使用 `coin = "xyz:CBRS"`，不额外带 `dex`，不暴露新 port。
- `orderUpdates`、`userEvents`、`userFills`、`userFundings` 是账户级 user stream subscription，不带 dex；解析层保留现有事件，runtime 按 instrument 过滤。
- 如果未来改用 `clearinghouseState` / `openOrders` 这类需要 dex 的 WS subscription，需要单独设计订阅输入边界；不要在当前 `HyperliquidWsClient` 构造时固定单一 dex。

构造方式：

- 不把 dex 放进 `HyperliquidWsClient` 构造参数。
- market stream 的 dex 上下文来自每次 `subscribe_prices(instrument)` 的 `instrument.symbol`。
- user stream 保持账户级订阅，不需要当前实例的 dex 集合。

## 账户资产和容量

现有逻辑：

- standard mode 使用 perps `clearinghouseState.withdrawable`。
- unified / portfolio margin 使用 `spotClearinghouseState` 的 USDC 可用余额。

HIP-3 设计：

- Hyperliquid 账户资产是账户级共享余额。默认 perps 和 HIP-3 perps 都消耗同一账户资产池；例如页面上开 `xyz:CBRS` 也使用同一个 Hyperliquid 账户保证金。
- `get_position` 等 perps 状态查询仍按 symbol 带 dex，因为持仓和挂单属于具体 perp dex。
- `AccountPort::get_account_capacity_snapshot(instrument)` 可以继续使用账户级可用余额口径，不应把 capacity 拆成每个 dex 一套余额。
- HIP-3 的 `onlyIsolated` / `marginMode = "noCross"` 影响的是杠杆 action 的 `isCross` 和仓位保证金模式，不代表资产余额独立。
- 如果未来某个 HIP-3 dex 明确暴露并要求非共享 collateral，本次实现应 fail closed；在没有明确证据前，不因为 symbol 属于 HIP-3 就假定它有独立 collateral。

`AccountSummaryPort::get_account_summary()` 继续表示 Hyperliquid 账户级摘要。它没有 instrument 参数，也不应隐式选择某个 dex。

风险约束：

- 实现前要确认 standard mode 下不带 dex 的 `clearinghouseState.withdrawable` 是账户级可用余额，能够覆盖 HIP-3 占用后的可用资产。如果它只是默认 dex 口径，不能继续把它当作混合实例容量事实源。
- unified account / portfolio margin 继续优先使用 `spotClearinghouseState` 的 USDC available after maintenance，因为官方文档建议这类账户用 spot balances endpoint 作为跨 spot/perps 的交易账户余额。
- 如果无法确认 standard mode 的共享可用余额口径，HIP-3 混合实例的容量估算应 fail closed 或要求账户处于 unified / portfolio margin，而不是静默沿用默认 dex `withdrawable`。

## 测试策略

遵循项目约定，先补最小验收测试，再实现。

### `poise-hyperliquid`

优先测试：

- `parse_perp_dex("BTC")` 返回默认 dex。
- `parse_perp_dex("xyz:CBRS")` 返回 HIP-3 dex `xyz`。
- `parse_perp_dex("xyz:")`、`parse_perp_dex(":CBRS")`、`parse_perp_dex("a:b:c")` 返回清晰错误。
- `meta` 请求对 `BTC` 不带 `dex`，对 `xyz:CBRS` 带 `dex = "xyz"`。
- meta cache 按 dex 分桶：同一个 REST client 内先查 `BTC` 再查 `xyz:CBRS`，以及先查 `xyz:CBRS` 再查 `BTC`，都使用各自正确 universe。
- `clearinghouseState` / `openOrders` 等持仓和挂单查询对 `xyz:CBRS` 带 `dex`，对 `BTC` 不带 `dex`。
- `get_position("xyz:CBRS")` 使用带 dex 的 `user_state_for_symbol`，账户容量使用不带 dex 的账户级 helper，二者不能复用同一个隐式 `user_state()`。
- 账户容量查询不因 `xyz:CBRS` 带 `dex`，而是继续读取账户级共享可用余额。
- standard mode 下如果无法确认 `withdrawable` 是共享账户余额，容量查询 fail closed；unified / portfolio margin 继续用 `spotClearinghouseState`。
- `asset_descriptor` 对默认 dex 返回普通下标。
- `asset_descriptor` 对 HIP-3 返回 `100000 + dex_index * 10000 + index_in_meta`。
- 同一个 REST client 内先后查询 `BTC` 和 `xyz:CBRS` 时，各自使用正确 meta 和 asset id。
- `set_leverage` 对 `onlyIsolated`、`marginMode = "noCross"` 或 `marginMode = "strictIsolated"` 资产发送 `isCross: false`。
- `set_leverage` 对 HIP-3 且缺失 `marginMode/onlyIsolated` 的资产发送 `isCross: true`。
- `set_leverage` 对 HIP-3 未知非空 margin mode fail closed。
- `set_leverage` 对超过 `maxLeverage` 的配置 fail before exchange action。
- submit / cancel / cancel all 使用 HIP-3 asset id。
- WS market subscription 对 `xyz:CBRS` 保持 coin 原样，且不带 `dex`。
- user stream subscription JSON 不带单一 dex。

最小测试命令：

```bash
cargo test -p poise-hyperliquid
```

如果实现只动 REST client，可先跑更窄过滤：

```bash
cargo test -p poise-hyperliquid rest::client::tests::
```

### `poise-server`

优先测试：

- `parse_config` 支持同一个 Hyperliquid 配置内同时出现 `symbol = "BTC"` 和 `symbol = "xyz:CBRS"`。
- `parse_config` 不尝试校验 HIP-3 dex 前缀，只保持现有通用配置校验。

最小测试命令：

```bash
cargo test -p poise-server config::tests::
```

## 实施切片

1. Symbol 解析和文档切片：在 Hyperliquid adapter 内增加私有 `parse_perp_dex`，补 README / system overview 说明 `BTC` 与 `xyz:CBRS` 可以混配。
2. REST metadata 和 asset id 切片：实现按 symbol 注入 dex、按 dex 分桶 meta cache、`perpDexs` 查询、HIP-3 asset id 计算，并让 `get_exchange_info` 能同时找到 `BTC` 和 `xyz:CBRS`。
3. 交易动作切片：让 leverage、submit、cancel、cancel all 统一使用新的 `AssetDescriptor`，并处理 isolated/cross。
4. 状态查询切片：让 position、open orders 的 perps 查询按 instrument symbol 带 dex；账户容量继续使用共享账户余额。
5. WS 切片：market stream 保持 `xyz:CBRS` wire name且不带 dex；user stream 保持账户级不带 dex；补 subscription 测试。
6. 文档和验收切片：更新 README / system overview，跑 `poise-hyperliquid` 和直接相关的 `poise-server` config 测试。

## 备选方案

### 方案 A：只把 `symbol` 改成 `xyz:CBRS`

优点是改动最少。缺点是 `meta` 仍查默认 dex，asset id 仍是默认下标，启动杠杆、下单和撤单都会继续失败或使用错误 asset id。不可接受。

### 方案 B：在 track 级别配置 `dex`

例如：

```toml
[[tracks]]
symbol = "CBRS"
hyperliquid_dex = "xyz"
```

优点是显式。缺点是把 Hyperliquid 私有协议知识上移到通用 track schema，并迫使 server / registry / validation 理解交易所特例。由于 Hyperliquid wire symbol 本身已经携带 `dex`，这个字段会制造双重事实源：`symbol = "xyz:CBRS"` 和 `hyperliquid_dex = "xyz"` 可能不一致。不可取。

### 方案 C：在 Hyperliquid exchange config 配置单个 `perp_dex`

这个方案能把 HIP-3 知识限制在 Hyperliquid adapter 内部，但错误地假设单实例只能属于一个 perp dex。实际 hype 实例可能同时交易 `BTC` 这类默认 perps 和 `xyz:CBRS` 这类 HIP-3 perps，因此不可取。

### 方案 D：由 Hyperliquid adapter 从 symbol 解析 dex 上下文

这是推荐方案。它利用 Hyperliquid 已有 wire name：不含冒号表示默认 perp dex，`{dex}:{coin}` 表示 HIP-3。调用方仍只传 `Instrument.symbol`，adapter 内部按 symbol 选择 meta、asset id、状态查询和行情订阅的 dex 上下文。它支持默认 perps 与 HIP-3 混配，也避免新增 track 字段或 exchange 级单 dex 限制。

## 概念预算复核

- `dex` 是 Hyperliquid symbol 的一部分，不新增 exchange config 字段或 track 字段。
- `PerpDexRef` 是 Hyperliquid adapter 内部解析结果，只为集中 dex 注入和默认/HIP-3 分支，不进入公共 port。
- `AssetDescriptor` 已存在，扩展它比新增一层 resolver service 更简单。
- `dex index` 是交易所协议细节，只按 dex name 缓存于 REST client，不成为持久化事实源。
- `xyz:CBRS` 是 `Instrument.symbol` 的交易所原生表示，不新增 `Instrument` 字段。

## 待确认

- 是否存在已上线且 Poise 需要支持的 HIP-3 dex 使用非共享 collateral。如果存在，容量估算必须先返回明确限制，不能误报可用容量。
- standard mode 下不带 dex 的 `clearinghouseState.withdrawable` 是否能作为默认 perps + HIP-3 混合实例的共享可用余额。如果不能确认，首版仅支持 unified / portfolio margin 下的 HIP-3 容量估算，或容量 fail closed。
- `updateLeverage` 对 `onlyIsolated` / `noCross` / `strictIsolated` 资产使用 `isCross: false` 是否完全符合 Hyperliquid 当前接口语义，需要用测试账号或官方示例确认。
