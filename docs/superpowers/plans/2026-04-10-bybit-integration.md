# Bybit 接入实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: 使用 `superpowers:subagent-driven-development`（推荐）或 `superpowers:executing-plans` 按 task 执行本计划。步骤使用 checkbox (`- [ ]`) 语法追踪。

**Goal:** 在不改变当前单服务单交易所边界和价格模型的前提下，为 `poise-server` 新增 Bybit V5 USDT 永续接入，支持主网/测试网、全量交易闭环，以及 one-way / `UNIFIED` 账户路径。

**Architecture:** 先固定共享层最小边界，只新增 `Venue::Bybit`、`ExchangeConfig::Bybit` 和 workspace 依赖；随后搭建 `poise-bybit` crate 的 `config` / `connected` / `rest` / `ws` / `mapper` 结构，并把 Bybit 协议知识全部下沉到这个 crate；最后补齐 Bybit 的 REST / WebSocket 映射、one-way / `UNIFIED` 约束失败路径、README 与示例配置。当前计划不拆 `reference_price`、`mark_price`、`index_price`，Bybit 首版继续对齐 Binance 的 `reference_price = mark_price` 语义。

**Tech Stack:** Rust workspace, Cargo, Tokio, Reqwest, Tokio Tungstenite, Serde, anyhow, hmac, sha2, chrono

**Spec:** [`../specs/2026-04-10-bybit-integration-design.md`](../specs/2026-04-10-bybit-integration-design.md)

---

## File Structure

### 新增文件

- `exchanges/bybit/Cargo.toml`
  - Bybit crate 依赖定义
- `exchanges/bybit/src/lib.rs`
  - 当前导出 `Config`、`Deployment`；后续接入 `Connected`、`connect(...)`
- `exchanges/bybit/src/config.rs`
  - Bybit 配置、主网/测试网 endpoint、凭证校验
- `exchanges/bybit/src/connected.rs`
  - Bybit 已连接组件集合，以及各个 `Port` 的 owner
- `exchanges/bybit/src/mapper.rs`
  - Bybit 原始 DTO 到 `poise-engine` 稳定模型的映射
- `exchanges/bybit/src/rest/mod.rs`
  - REST 模块入口
- `exchanges/bybit/src/rest/auth.rs`
  - Bybit V5 HMAC 签名
- `exchanges/bybit/src/rest/client.rs`
  - Bybit REST client
- `exchanges/bybit/src/rest/models.rs`
  - Bybit REST 原始响应模型
- `exchanges/bybit/src/ws/mod.rs`
  - WebSocket 模块入口
- `exchanges/bybit/src/ws/market.rs`
  - public ticker 流
- `exchanges/bybit/src/ws/account.rs`
  - private order / position 流
- `exchanges/bybit/src/ws/models.rs`
  - Bybit WebSocket 原始消息模型
- `configs/bybit-testnet.demo.toml`
  - Bybit 测试网实例示例配置

### 重点修改文件

- `Cargo.toml`
  - workspace members 增加 `exchanges/bybit`
- `engine/src/track.rs`
  - 新增 `Venue::Bybit`
- `server/Cargo.toml`
  - 引入 `poise-bybit`
- `server/src/config.rs`
  - `ExchangeConfig` 增加 `Bybit`
- `server/src/assembly.rs`
  - `build_exchange(...)` 新增 Bybit 分支
- `README.md`
  - 说明 Bybit 可用配置和示例启动方式

### 实施约束

- 每个 task 先写失败测试，再写最小实现
- 每个 task 验收通过后必须立即提交
- 不做共享 CEX 底座
- 不做 symbol 归一化层
- 不修改 `engine/src/ports.rs` 的接口形状
- 不拆价格模型；Bybit 首版继续 `reference_price = mark_price`
- Bybit 首版只走 `linear`、one-way、`UNIFIED` 账户摘要路径

---

### Task 1: 固定共享层 Bybit 边界与 workspace 入口

验收记录：
- 状态：已完成
- 实现提交：`f298c63 feat: add bybit config boundary`
- 修正提交：`ac7b7b2 fix: tighten bybit config surface`
- 验证：
  - `cargo test -p poise-engine track::tests::venue_as_str_supports_bybit -- --exact`
  - `cargo test -p poise-server config::tests::parses_service_level_exchange_config_and_track_symbols -- --exact`
  - `cargo test -p poise-server config::tests::parses_bybit_exchange_config_and_tracks -- --exact`
  - `cargo test -p poise-bybit config::tests::credentials_handle_missing_fields_whitespace_and_success -- --exact`

**Files:**
- Create: `exchanges/bybit/Cargo.toml`
- Create: `exchanges/bybit/src/lib.rs`
- Create: `exchanges/bybit/src/config.rs`
- Modify: `Cargo.toml`
- Modify: `engine/src/track.rs`
- Modify: `server/Cargo.toml`
- Modify: `server/src/config.rs`
- Test: `exchanges/bybit/src/config.rs`
- Test: `server/src/config.rs`

- [x] **Step 1: 先写失败测试，固定 workspace / venue / 配置边界**

在 `engine/src/track.rs` 增加 `Venue::Bybit` 测试：

```rust
#[test]
fn venue_as_str_supports_bybit() {
    assert_eq!(Venue::Bybit.as_str(), "bybit");
}
```

在 `exchanges/bybit/src/config.rs` 增加 endpoint 和凭证测试：

```rust
#[test]
fn deployment_resolves_mainnet_and_testnet_endpoints() {
    assert_eq!(
        Deployment::Mainnet.endpoints(),
        Endpoints::new(
            "https://api.bybit.com",
            "wss://stream.bybit.com/v5/public/linear",
            "wss://stream.bybit.com/v5/private",
        )
    );
    assert_eq!(
        Deployment::Testnet.endpoints(),
        Endpoints::new(
            "https://api-testnet.bybit.com",
            "wss://stream-testnet.bybit.com/v5/public/linear",
            "wss://stream-testnet.bybit.com/v5/private",
        )
    );
}

#[test]
fn credentials_require_api_key_and_api_secret() {
    let error = Config::default().credentials().unwrap_err();
    assert!(error.to_string().contains("missing required exchange.api_key"));
}
```

在 `server/src/config.rs` 增加 Bybit 配置解析测试：

```rust
#[test]
fn parses_bybit_exchange_config_and_tracks() {
    let config = parse_config(
        r#"
[exchange]
venue = "bybit"
deployment = "testnet"
api_key = "demo-key"
api_secret = "demo-secret"

[[tracks]]
track_id = "btc-core"
symbol = "BTCUSDT"
lower_price = 90.0
upper_price = 110.0
long_exposure_units = 8.0
short_exposure_units = 6.0
notional_per_unit = 375.0
daily_loss_limit = 300.0
total_loss_limit = 600.0
"#,
    )
    .unwrap();

    assert_eq!(config.exchange.venue(), Venue::Bybit);
    assert_eq!(config.tracks[0].symbol, "BTCUSDT");
}
```

- [x] **Step 2: 运行定向测试，确认当前代码还没有 Bybit 边界**

Run:
`cargo test -p poise-server parses_bybit_exchange_config_and_tracks -- --exact`

Expected:
- FAIL，原因是当前 `ExchangeConfig` 还没有 `Bybit` 分支

Run:
`cargo test -p poise-engine venue_as_str_supports_bybit -- --exact`

Expected:
- FAIL，原因是当前 `Venue` 还没有 `Bybit`

- [x] **Step 3: 写最小实现，接通 workspace / venue / 配置**

在 `engine/src/track.rs` 增加枚举分支：

```rust
pub enum Venue {
    Binance,
    Bybit,
}

impl Venue {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Binance => "binance",
            Self::Bybit => "bybit",
        }
    }
}
```

创建 `exchanges/bybit/src/config.rs`：

```rust
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub deployment: Deployment,
    pub api_key: Option<String>,
    pub api_secret: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Deployment {
    Mainnet,
    #[default]
    Testnet,
}
```

在 `server/src/config.rs` 增加 Bybit 分支：

```rust
#[serde(tag = "venue", rename_all = "snake_case")]
pub enum ExchangeConfig {
    Binance(binance::Config),
    Bybit(bybit::Config),
}

impl ExchangeConfig {
    pub fn venue(&self) -> Venue {
        match self {
            Self::Binance(_) => Venue::Binance,
            Self::Bybit(_) => Venue::Bybit,
        }
    }
}
```

更新 workspace 依赖：

```toml
[workspace]
members = [
    "application",
    "core",
    "engine",
    "storage",
    "protocol",
    "exchanges/binance",
    "exchanges/bybit",
    "server",
    "tui",
]
```

- [ ] **Step 4: 运行配置与 workspace 回归**

Run:
`cargo test -p poise-engine venue_as_str_supports_bybit -- --exact`

Expected:
- PASS

Run:
`cargo test -p poise-bybit deployment_resolves_mainnet_and_testnet_endpoints -- --exact`

Expected:
- PASS

Run:
`cargo test -p poise-server parses_bybit_exchange_config_and_tracks -- --exact`

Expected:
- PASS

- [ ] **Step 5: 提交**

```bash
git add Cargo.toml engine/src/track.rs server/Cargo.toml server/src/config.rs exchanges/bybit/Cargo.toml exchanges/bybit/src/lib.rs exchanges/bybit/src/config.rs
git commit -m "feat: add bybit config boundary"
```

---

### Task 2: 建立 Bybit crate 骨架并接入 `build_exchange(...)`

验收记录：
- 状态：已完成
- 提交：`0cb0fa8 feat: wire bybit exchange assembly`
- 验证：
  - `cargo test -p poise-bybit connected::tests::connected_exposes_all_required_ports -- --exact`
  - `cargo test -p poise-server assembly::tests::build_exchange_uses_exchange_deployment_for_bybit_endpoint_selection -- --exact`
  - `cargo fmt --all --check`

**Files:**
- Create: `exchanges/bybit/src/connected.rs`
- Create: `exchanges/bybit/src/rest/mod.rs`
- Create: `exchanges/bybit/src/ws/mod.rs`
- Modify: `exchanges/bybit/src/lib.rs`
- Modify: `server/src/assembly.rs`
- Test: `exchanges/bybit/src/connected.rs`
- Test: `server/src/assembly.rs`

- [x] **Step 1: 先写失败测试，固定已连接入口和装配分支**

在 `exchanges/bybit/src/connected.rs` 增加端口暴露测试：

```rust
#[test]
fn connected_exposes_all_required_ports() {
    let connected = Connected::from_parts(
        Arc::new(FakeExecutionPort),
        Arc::new(FakeMarketDataPort),
        Arc::new(FakeAccountSummaryPort),
        Arc::new(FakeAccountPort),
        Arc::new(FakeMetadataPort),
    );

    let _execution: Arc<dyn ExecutionPort> = connected.execution();
    let _market_data: Arc<dyn MarketDataPort> = connected.market_data();
    let _account_summary: Arc<dyn AccountSummaryPort> = connected.account_summary();
    let _account: Arc<dyn AccountPort> = connected.account();
    let _metadata: Arc<dyn MetadataPort> = connected.metadata();
}
```

在 `server/src/assembly.rs` 增加装配测试：

```rust
#[tokio::test]
async fn build_exchange_uses_exchange_deployment_for_bybit_endpoint_selection() {
    let config = parse_config(
        r#"
[exchange]
venue = "bybit"
deployment = "mainnet"
api_key = "demo-key"
api_secret = "demo-secret"

[[tracks]]
track_id = "btc-core"
symbol = "BTCUSDT"
lower_price = 90.0
upper_price = 110.0
long_exposure_units = 8.0
short_exposure_units = 6.0
notional_per_unit = 375.0
daily_loss_limit = 300.0
total_loss_limit = 600.0
"#,
    )
    .unwrap();

    let exchange = build_exchange(&config.exchange).await.unwrap();
    assert_eq!(exchange.venue(), Venue::Bybit);
}
```

- [x] **Step 2: 运行定向测试，确认当前还没有 Bybit 已连接入口**

Run:
`cargo test -p poise-bybit connected_exposes_all_required_ports -- --exact`

Expected:
- FAIL，原因是当前还没有 `Connected`

Run:
`cargo test -p poise-server build_exchange_uses_exchange_deployment_for_bybit_endpoint_selection -- --exact`

Expected:
- FAIL，原因是当前 `build_exchange(...)` 还没有 Bybit 分支

- [x] **Step 3: 写最小实现，建立 crate 骨架和装配分支**

创建 `exchanges/bybit/src/connected.rs`：

```rust
pub async fn connect(config: &Config) -> Result<Connected> {
    let endpoints = config.endpoints();
    let (api_key, api_secret) = config.credentials()?;
    let rest = Arc::new(BybitRestClient::new(endpoints.rest_base_url(), api_key, api_secret));
    let ws = Arc::new(BybitWsClient::new(
        endpoints.public_ws_base_url(),
        endpoints.private_ws_base_url(),
        rest.clone(),
    ));

    Ok(Connected::from_clients(rest, ws))
}

#[derive(Clone)]
pub struct Connected {
    execution: Arc<dyn ExecutionPort>,
    market_data: Arc<dyn MarketDataPort>,
    account_summary: Arc<dyn AccountSummaryPort>,
    account: Arc<dyn AccountPort>,
    metadata: Arc<dyn MetadataPort>,
}
```

在 `server/src/assembly.rs` 增加 Bybit 分支：

```rust
match config {
    ExchangeConfig::Binance(binance_config) => {
        let connected = connect_binance(binance_config).await?;
        Ok(Exchange::new(
            Venue::Binance,
            connected.execution(),
            connected.market_data(),
            connected.account_summary(),
            connected.account(),
            connected.metadata(),
        ))
    }
    ExchangeConfig::Bybit(bybit_config) => {
        let connected = connect_bybit(bybit_config).await?;
        Ok(Exchange::new(
            Venue::Bybit,
            connected.execution(),
            connected.market_data(),
            connected.account_summary(),
            connected.account(),
            connected.metadata(),
        ))
    }
}
```

- [x] **Step 4: 运行骨架与装配回归**

Run:
`cargo test -p poise-bybit connected_exposes_all_required_ports -- --exact`

Expected:
- PASS

Run:
`cargo test -p poise-server build_exchange_uses_exchange_deployment_for_bybit_endpoint_selection -- --exact`

Expected:
- PASS

- [x] **Step 5: 提交**

```bash
git add exchanges/bybit/src/lib.rs exchanges/bybit/src/connected.rs exchanges/bybit/src/rest/mod.rs exchanges/bybit/src/ws/mod.rs server/src/assembly.rs
git commit -m "feat: wire bybit exchange assembly"
```

---

### Task 3: 实现 Bybit REST、映射和保守容量快照

验收记录：
- 状态：已完成
- 实现提交：`2cf2ab0 feat: add bybit rest integration`
- 修正提交：`d9e1f19 fix: wire bybit rest-backed ports`
- 清理提交：`0f52ae9 refactor: enforce bybit unified wallet summary`
- 验证：
  - `cargo test -p poise-bybit --lib -- --nocapture`
  - `cargo test -p poise-server assembly::tests::build_exchange_uses_exchange_deployment_for_bybit_endpoint_selection -- --exact`
  - `cargo fmt --all --check`

**Files:**
- Create: `exchanges/bybit/src/mapper.rs`
- Create: `exchanges/bybit/src/rest/auth.rs`
- Create: `exchanges/bybit/src/rest/client.rs`
- Create: `exchanges/bybit/src/rest/models.rs`
- Modify: `exchanges/bybit/src/connected.rs`
- Test: `exchanges/bybit/src/mapper.rs`
- Test: `exchanges/bybit/src/rest/auth.rs`
- Test: `exchanges/bybit/src/rest/client.rs`

- [x] **Step 1: 先写失败测试，固定 REST 映射和容量语义**

在 `exchanges/bybit/src/mapper.rs` 增加规则映射测试：

```rust
#[test]
fn converts_linear_instrument_info_into_exchange_info() {
    let response: BybitInstrumentsInfoResponse = serde_json::from_str(r#"
    {
      "result": {
        "list": [{
          "symbol": "BTCUSDT",
          "priceFilter": { "tickSize": "0.10" },
          "lotSizeFilter": {
            "qtyStep": "0.001",
            "minOrderQty": "0.001",
            "minNotionalValue": "5"
          }
        }]
      }
    }
    "#).unwrap();

    let info = ExchangeInfo::try_from(response.result.list.into_iter().next().unwrap()).unwrap();
    assert_eq!(info.instrument, Instrument::new(Venue::Bybit, "BTCUSDT"));
    assert_eq!(info.rules.price_tick, 0.10);
    assert_eq!(info.rules.quantity_step, 0.001);
    assert_eq!(info.rules.min_qty, 0.001);
    assert_eq!(info.rules.min_notional, 5.0);
}
```

增加账户摘要与保守容量测试：

```rust
#[test]
fn converts_unified_wallet_balance_into_account_summary_snapshot() {
    let response: BybitWalletBalanceResponse = serde_json::from_str(r#"
    {
      "result": {
        "list": [{
          "accountType": "UNIFIED",
          "totalEquity": "12500.5",
          "totalAvailableBalance": "9800.25",
          "totalPerpUPL": "-120.75"
        }]
      }
    }
    "#).unwrap();

    let snapshot = response.into_account_summary_snapshot().unwrap();
    assert_eq!(snapshot.equity, 12500.5);
    assert_eq!(snapshot.available, 9800.25);
    assert_eq!(snapshot.unrealized_pnl, -120.75);
}

#[test]
fn missing_unified_wallet_balance_fields_fail_stably() {
    let response: BybitWalletBalanceResponse = serde_json::from_str(r#"
    {
      "result": { "list": [{ "accountType": "UNIFIED" }] }
    }
    "#).unwrap();

    let error = response.into_account_summary_snapshot().unwrap_err();
    assert!(error.to_string().contains("missing totalAvailableBalance"));
}

#[test]
fn builds_account_capacity_snapshot_from_available_balance_only() {
    let response: BybitWalletBalanceResponse = serde_json::from_str(r#"
    {
      "result": {
        "list": [{
          "accountType": "UNIFIED",
          "totalEquity": "12500.5",
          "totalAvailableBalance": "9800.25",
          "totalPerpUPL": "-120.75"
        }]
      }
    }
    "#).unwrap();

    let snapshot = response.into_account_capacity_snapshot().unwrap();
    assert_eq!(snapshot.max_increase_notional, 9800.25);
}
```

在 `rest/auth.rs` 增加签名测试：

```rust
#[test]
fn signs_v5_payload_with_timestamp_recv_window_and_body() {
    let signature = sign(
        "secret",
        "demo-key",
        "1710000000000",
        "5000",
        "{\"category\":\"linear\",\"symbol\":\"BTCUSDT\"}",
    );

    assert_eq!(signature.len(), 64);
}
```

- [x] **Step 2: 运行定向测试，确认当前 REST 路径还没实现**

Run:
`cargo test -p poise-bybit converts_linear_instrument_info_into_exchange_info -- --exact`

Expected:
- FAIL，原因是当前还没有 Bybit REST DTO 和映射

Run:
`cargo test -p poise-bybit builds_account_capacity_snapshot_from_available_balance_only -- --exact`

Expected:
- FAIL，原因是当前还没有 Bybit 账户余额 DTO 和容量快照映射

- [x] **Step 3: 写最小实现，完成 REST client、签名和 mapper**

在 `exchanges/bybit/src/rest/auth.rs` 中实现签名：

```rust
pub fn sign(
    secret: &str,
    api_key: &str,
    timestamp: &str,
    recv_window: &str,
    payload: &str,
) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(timestamp.as_bytes());
    mac.update(api_key.as_bytes());
    mac.update(recv_window.as_bytes());
    mac.update(payload.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}
```

在 `exchanges/bybit/src/mapper.rs` 中实现映射：

```rust
impl TryFrom<BybitLinearInstrument> for ExchangeInfo {
    type Error = anyhow::Error;

    fn try_from(value: BybitLinearInstrument) -> Result<Self, Self::Error> {
        Ok(Self {
            instrument: Instrument::new(Venue::Bybit, value.symbol),
            rules: ExchangeRules {
                price_tick: parse_decimal("tickSize", &value.price_filter.tick_size)?,
                quantity_step: parse_decimal("qtyStep", &value.lot_size_filter.qty_step)?,
                min_qty: parse_decimal("minOrderQty", &value.lot_size_filter.min_order_qty)?,
                min_notional: parse_decimal(
                    "minNotionalValue",
                    &value.lot_size_filter.min_notional_value,
                )?,
                maker_fee_rate: 0.0002,
                taker_fee_rate: 0.00055,
            },
        })
    }
}
```

在 `exchanges/bybit/src/connected.rs` 中把 REST 支持的端口接通：

```rust
#[async_trait]
impl AccountPort for BybitAccount {
    async fn get_account_capacity_snapshot(
        &self,
        _instrument: &Instrument,
    ) -> Result<AccountCapacitySnapshot> {
        self.rest.get_account_capacity_snapshot().await
    }

    async fn subscribe_user_data(&self) -> Result<mpsc::Receiver<UserDataEvent>> {
        self.ws.subscribe_user_data().await
    }
}
```

- [x] **Step 4: 运行 REST 与映射回归**

Run:
`cargo test -p poise-bybit converts_linear_instrument_info_into_exchange_info -- --exact`

Expected:
- PASS

Run:
`cargo test -p poise-bybit converts_unified_wallet_balance_into_account_summary_snapshot -- --exact`

Expected:
- PASS

Run:
`cargo test -p poise-bybit missing_unified_wallet_balance_fields_fail_stably -- --exact`

Expected:
- PASS

Run:
`cargo test -p poise-bybit builds_account_capacity_snapshot_from_available_balance_only -- --exact`

Expected:
- PASS

- [x] **Step 5: 提交**

```bash
git add exchanges/bybit/src/mapper.rs exchanges/bybit/src/rest/auth.rs exchanges/bybit/src/rest/client.rs exchanges/bybit/src/rest/models.rs exchanges/bybit/src/connected.rs
git commit -m "feat: add bybit rest integration"
```

---

### Task 4: 实现 Bybit WebSocket、one-way 约束和文档示例

**Files:**
- Create: `exchanges/bybit/src/ws/models.rs`
- Create: `exchanges/bybit/src/ws/market.rs`
- Create: `exchanges/bybit/src/ws/account.rs`
- Modify: `exchanges/bybit/src/ws/mod.rs`
- Modify: `exchanges/bybit/src/connected.rs`
- Create: `configs/bybit-testnet.demo.toml`
- Modify: `README.md`
- Test: `exchanges/bybit/src/ws/market.rs`
- Test: `exchanges/bybit/src/ws/account.rs`
- Test: `README.md` 相关配置解析测试

- [ ] **Step 1: 先写失败测试，固定 public/private WS 和 one-way 失败路径**

在 `ws/market.rs` 增加 ticker 解析测试：

```rust
#[test]
fn parses_linear_ticker_message_into_price_tick() {
    let tick = parse_ticker_message(r#"
    {
      "ts": 1710000000000,
      "data": {
        "symbol": "BTCUSDT",
        "markPrice": "64000.10",
        "indexPrice": "63999.90"
      }
    }
    "#).unwrap().unwrap();

    assert_eq!(tick.instrument, Instrument::new(Venue::Bybit, "BTCUSDT"));
    assert_eq!(tick.reference_price, 64000.10);
    assert_eq!(tick.mark_price, 64000.10);
}
```

在 `ws/account.rs` 增加 private 流映射测试：

```rust
#[test]
fn parses_order_update_into_user_data_event() {
    let event = parse_private_message(r#"
    {
      "topic": "order",
      "creationTime": 1710000000000,
      "data": [{
        "symbol": "BTCUSDT",
        "orderId": "123",
        "orderLinkId": "client-1",
        "side": "Buy",
        "price": "64000.1",
        "qty": "0.01",
        "orderStatus": "New"
      }]
    }
    "#).unwrap().unwrap();

    match event.payload {
        UserDataPayload::OrderUpdate(order) => {
            assert_eq!(order.order_id, "123");
            assert_eq!(order.client_order_id, "client-1");
        }
        other => panic!("unexpected payload: {other:?}"),
    }
}

#[test]
fn rejects_non_one_way_position_update() {
    let error = parse_private_message(r#"
    {
      "topic": "position",
      "creationTime": 1710000000000,
      "data": [{
        "symbol": "BTCUSDT",
        "side": "Buy",
        "size": "0.01",
        "avgPrice": "64000",
        "unrealisedPnl": "10",
        "positionIdx": 1
      }]
    }
    "#).unwrap_err();

    assert!(error.to_string().contains("unsupported positionIdx"));
}
```

在 `server/src/config.rs` 增加 README 示例对齐测试：

```rust
#[test]
fn parses_bybit_testnet_example_config() {
    let config = parse_config(include_str!("../../configs/bybit-testnet.demo.toml")).unwrap();
    assert_eq!(config.exchange.venue(), Venue::Bybit);
    assert_eq!(config.tracks[0].symbol, "BTCUSDT");
}
```

- [ ] **Step 2: 运行定向测试，确认当前 WS 和示例配置还没实现**

Run:
`cargo test -p poise-bybit parses_linear_ticker_message_into_price_tick -- --exact`

Expected:
- FAIL，原因是当前还没有 Bybit public WS 解析

Run:
`cargo test -p poise-bybit rejects_non_one_way_position_update -- --exact`

Expected:
- FAIL，原因是当前还没有 Bybit private WS 约束处理

Run:
`cargo test -p poise-server parses_bybit_testnet_example_config -- --exact`

Expected:
- FAIL，原因是当前还没有示例配置文件

- [ ] **Step 3: 写最小实现，补全 WS 和 README 示例**

在 `ws/market.rs` 中保持价格口径与 Binance 对齐：

```rust
Ok(Some(PriceTick {
    instrument: Instrument::new(Venue::Bybit, ticker.symbol),
    reference_price: parse_decimal("markPrice", &ticker.mark_price)?,
    mark_price: parse_decimal("markPrice", &ticker.mark_price)?,
    timestamp,
}))
```

在 `ws/account.rs` 中对非 one-way 明确失败：

```rust
fn bybit_position_to_engine(value: BybitPosition) -> Result<Position> {
    if value.position_idx != 0 {
        return Err(anyhow!("unsupported positionIdx: {}", value.position_idx));
    }

    let qty = match value.side.as_str() {
        "Buy" => parse_decimal("size", &value.size)?,
        "Sell" => -parse_decimal("size", &value.size)?,
        _ => 0.0,
    };

    Ok(Position {
        instrument: Instrument::new(Venue::Bybit, value.symbol),
        qty,
        avg_price: parse_decimal("avgPrice", &value.avg_price)?,
        unrealized_pnl: parse_decimal("unrealisedPnl", &value.unrealised_pnl)?,
    })
}
```

创建 `configs/bybit-testnet.demo.toml`：

```toml
bind_address = "127.0.0.1:8000"

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

- [ ] **Step 4: 跑 Bybit crate、server 和 README 示例回归**

Run:
`cargo test -p poise-bybit`

Expected:
- PASS

Run:
`cargo test -p poise-server`

Expected:
- PASS

Run:
`cargo build -p poise-server`

Expected:
- PASS

- [ ] **Step 5: 提交**

```bash
git add exchanges/bybit/src/ws/mod.rs exchanges/bybit/src/ws/models.rs exchanges/bybit/src/ws/market.rs exchanges/bybit/src/ws/account.rs configs/bybit-testnet.demo.toml README.md server/src/config.rs
git commit -m "feat: add bybit websocket integration"
```

---

### Task 5: 接通 Bybit `ExecutionPort` 并完成最终闭环回归

**Files:**
- Modify: `exchanges/bybit/src/rest/client.rs`
- Modify: `exchanges/bybit/src/rest/models.rs`
- Modify: `exchanges/bybit/src/mapper.rs`
- Modify: `exchanges/bybit/src/connected.rs`
- Test: `exchanges/bybit/src/mapper.rs`
- Test: `exchanges/bybit/src/rest/client.rs`

- [ ] **Step 1: 先写失败测试，固定下单、撤单、查仓位和查挂单语义**

在 `exchanges/bybit/src/mapper.rs` 增加下单和订单状态映射测试：

```rust
#[test]
fn converts_create_order_response_into_order_receipt() {
    let response: BybitOrderResponse = serde_json::from_str(r#"
    {
      "result": {
        "orderId": "123",
        "orderLinkId": "client-1"
      }
    }
    "#).unwrap();

    let receipt = response.result.into_order_receipt().unwrap();
    assert_eq!(receipt.order_id, "123");
    assert_eq!(receipt.client_order_id, "client-1");
    assert_eq!(receipt.status, OrderStatus::Submitting);
}

#[test]
fn converts_one_way_position_snapshot_into_position() {
    let position = BybitPosition {
        symbol: "BTCUSDT".into(),
        side: "Sell".into(),
        size: "0.01".into(),
        avg_price: "64000".into(),
        unrealised_pnl: "10".into(),
        position_idx: 0,
    };

    let converted = Position::try_from(position).unwrap();
    assert_eq!(converted.qty, -0.01);
}

#[test]
fn rejects_non_one_way_position_snapshot() {
    let position = BybitPosition {
        symbol: "BTCUSDT".into(),
        side: "Buy".into(),
        size: "0.01".into(),
        avg_price: "64000".into(),
        unrealised_pnl: "10".into(),
        position_idx: 1,
    };

    let error = Position::try_from(position).unwrap_err();
    assert!(error.to_string().contains("unsupported positionIdx"));
}
```

在 `exchanges/bybit/src/rest/client.rs` 增加交易请求测试：

```rust
#[tokio::test]
async fn create_order_uses_linear_limit_gtc_and_position_idx_zero() {
    // mock server 断言 body 至少包含：
    // category=linear, orderType=Limit, timeInForce=GTC, positionIdx=0
}
```

- [ ] **Step 2: 运行定向测试，确认当前交易 REST 还没接通**

Run:
`cargo test -p poise-bybit converts_create_order_response_into_order_receipt -- --exact`

Expected:
- FAIL，原因是当前还没有 Bybit 交易 DTO / 映射

Run:
`cargo test -p poise-bybit create_order_uses_linear_limit_gtc_and_position_idx_zero -- --exact`

Expected:
- FAIL，原因是当前 `ExecutionPort` 还没有走 Bybit REST

- [ ] **Step 3: 写最小实现，接通 `ExecutionPort`**

实现范围：

- `submit_order` -> `POST /v5/order/create`
- `cancel_order` -> `POST /v5/order/cancel`
- `cancel_all` -> `POST /v5/order/cancel-all`
- `get_position` -> `GET /v5/position/list?category=linear&symbol=...`
- `get_open_orders` -> `GET /v5/order/realtime?category=linear&symbol=...`

固定参数：

- `category = linear`
- `orderType = Limit`
- `timeInForce = GTC`
- `positionIdx = 0`
- `orderLinkId <- client_order_id`
- `reduceOnly <- OrderRequest.reduce_only`

映射要求：

- create order 的 REST 回包映射为 `OrderReceipt { status: Submitting }`
- one-way `Sell` 仓位数量映射为负数
- open orders 映射到现有 `ExchangeOrder`
- 非 one-way 的 `positionIdx != 0` 明确失败

- [ ] **Step 4: 跑 Bybit 最终回归**

Run:
`cargo test -p poise-bybit`

Expected:
- PASS

Run:
`cargo test -p poise-server`

Expected:
- PASS

Run:
`cargo build -p poise-server`

Expected:
- PASS

- [ ] **Step 5: 提交**

```bash
git add exchanges/bybit/src/rest/client.rs exchanges/bybit/src/rest/models.rs exchanges/bybit/src/mapper.rs exchanges/bybit/src/connected.rs
git commit -m "feat: add bybit execution integration"
```

---

## Self-Review

### Spec 覆盖

- Bybit V5、`linear`、主网/测试网：Task 1、Task 2
- `ExchangeConfig::Bybit`、`Venue::Bybit`、装配分支：Task 1、Task 2
- `reference_price = mark_price`：Task 4
- `UNIFIED` 账户摘要：Task 3
- one-way 约束：Task 4
- 保守容量快照：Task 3
- 全量交易闭环：Task 4、Task 5
- README 与示例配置：Task 4

### Placeholder 扫描

- 没有 `TODO`、`TBD`、`similar to`
- 每个 task 都给出明确文件路径
- 每个测试步骤都有具体命令

### 类型一致性

- `Venue::Bybit`
- `ExchangeConfig::Bybit`
- `BybitRestClient`
- `BybitWsClient`
- `Connected`
- `PriceTick.reference_price`
- `AccountCapacitySnapshot.max_increase_notional`

以上命名在各 task 中保持一致。
