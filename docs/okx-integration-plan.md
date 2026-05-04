# OKX 合约接入执行计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 自研 OKX `SWAP` 永续合约交易所适配，只覆盖 Poise 当前运行需要的合约能力。

**Architecture:** 新增 `exchanges/okx` crate，把 OKX 鉴权、REST/WS 协议、响应模型和映射细节全部封装在 crate 内。`core`、`server` 和运行时只新增 `Venue::Okx`、配置分支和现有 port 装配，不泄漏 OKX 协议字段。

**Tech Stack:** Rust 2024、workspace crate、`reqwest`、`tokio-tungstenite`、`serde`、`hmac`、`sha2`、`base64`、TDD。

---

## 边界

- 只支持 OKX `SWAP` 永续合约，不支持 spot、margin、交割合约、option、提现、划转、资金账户操作、策略单、WebSocket 下单。
- `symbol` 使用 OKX 原生合约 ID，例如 `BTC-USDT-SWAP`。
- 默认交易模式为 `cross`，持仓模式按 `net` 解析；如果 OKX 返回 long/short 持仓，应明确报错。
- 配置字段为 `venue = "okx"`、`deployment`、`api_key`、`api_secret`、`passphrase`。
- `deployment = "mainnet"` 使用生产 REST/WS；`deployment = "demo"` 使用 OKX demo WS，REST 私有请求加 `x-simulated-trading: 1`。
- OKX 协议、签名、demo header、WS 登录和 symbol 规则全部留在 `exchanges/okx/` 内。

## 文件结构

- Create: `exchanges/okx/Cargo.toml`
- Create: `exchanges/okx/src/lib.rs`
- Create: `exchanges/okx/src/config.rs`
- Create: `exchanges/okx/src/mapper.rs`
- Create: `exchanges/okx/src/rest/mod.rs`
- Create: `exchanges/okx/src/rest/auth.rs`
- Create: `exchanges/okx/src/rest/models.rs`
- Create: `exchanges/okx/src/rest/client.rs`
- Create: `exchanges/okx/src/ws/mod.rs`
- Create: `exchanges/okx/src/ws/market.rs`
- Create: `exchanges/okx/src/ws/account.rs`
- Create: `exchanges/okx/src/ws/models.rs`
- Create: `exchanges/okx/src/connected.rs`
- Create: `exchanges/okx/src/startup_control.rs`
- Modify: `Cargo.toml`
- Modify: `core/src/track.rs`
- Modify: `server/Cargo.toml`
- Modify: `server/src/config.rs`
- Modify: `server/src/assembly.rs`
- Modify: `server/src/exchange_startup.rs`
- Modify: `README.md`
- Modify: `SECURITY.md`
- Modify: `docs/system-overview.md`

## 任务清单

### Task 1: Workspace、Venue 和配置边界

**Files:**
- Create: `exchanges/okx/Cargo.toml`
- Create: `exchanges/okx/src/lib.rs`
- Create: `exchanges/okx/src/config.rs`
- Modify: `Cargo.toml`
- Modify: `core/src/track.rs`
- Modify: `server/Cargo.toml`
- Modify: `server/src/config.rs`
- Modify: `server/src/assembly.rs`
- Modify: `server/src/exchange_startup.rs`

- [x] **Step 1: 写失败测试**

在 `core/src/track.rs` 测试模块新增：

```rust
#[test]
fn venue_as_str_supports_okx() {
    assert_eq!(Venue::Okx.as_str(), "okx");
}
```

在 `server/src/config.rs` 测试模块新增：

```rust
#[test]
fn parses_okx_exchange_config() {
    let config = parse_config(
        r#"
[exchange]
venue = "okx"
deployment = "demo"
api_key = "demo-key"
api_secret = "demo-secret"
passphrase = "demo-passphrase"

[[tracks]]
track_id = "btc-core"
symbol = "BTC-USDT-SWAP"
lower_price = 90000.0
upper_price = 110000.0
long_exposure_units = 8.0
short_exposure_units = 6.0
notional_per_unit = 3000.0
daily_loss_limit = 300.0
total_loss_limit = 600.0
"#,
    )
    .unwrap();

    assert_eq!(config.exchange.venue(), Venue::Okx);
    let definition = config.tracks[0]
        .to_track_definition(config.exchange.venue())
        .unwrap();
    assert_eq!(definition.instrument().venue, Venue::Okx);
    assert_eq!(definition.instrument().symbol, "BTC-USDT-SWAP");
}
```

在 `server/src/assembly.rs` 测试模块新增：

```rust
#[tokio::test]
async fn build_exchange_uses_exchange_deployment_for_okx_endpoint_selection() {
    let config = parse_config(
        r#"
[exchange]
venue = "okx"
deployment = "demo"
api_key = "demo-key"
api_secret = "demo-secret"
passphrase = "demo-passphrase"

[[tracks]]
track_id = "btc-core"
symbol = "BTC-USDT-SWAP"
lower_price = 90000.0
upper_price = 110000.0
long_exposure_units = 8.0
short_exposure_units = 6.0
notional_per_unit = 3000.0
daily_loss_limit = 300.0
total_loss_limit = 600.0
"#,
    )
    .unwrap();

    let exchange = build_exchange(&config.exchange).await.unwrap();

    assert_eq!(exchange.venue(), Venue::Okx);
}
```

在 `server/src/exchange_startup.rs` 测试模块新增：

```rust
#[test]
fn build_symbol_leverage_setter_accepts_okx_credentials() {
    build_symbol_leverage_setter(&ExchangeConfig::Okx(poise_okx::Config {
        deployment: poise_okx::Deployment::Demo,
        api_key: Some("demo-key".to_string()),
        api_secret: Some("demo-secret".to_string()),
        passphrase: Some("demo-passphrase".to_string()),
    }))
    .unwrap();
}
```

- [x] **Step 2: 运行测试确认失败**

Run:

```bash
cargo test -p poise-core track::tests::venue_as_str_supports_okx
cargo test -p poise-server config::tests::parses_okx_exchange_config
cargo test -p poise-server assembly::tests::build_exchange_uses_exchange_deployment_for_okx_endpoint_selection
cargo test -p poise-server exchange_startup::tests::build_symbol_leverage_setter_accepts_okx_credentials
```

Expected: 失败，原因包括 `Venue::Okx`、`ExchangeConfig::Okx`、`poise_okx` 未定义。

- [x] **Step 3: 最小实现**

`core/src/track.rs`：

```rust
pub enum Venue {
    Binance,
    Bybit,
    Hyperliquid,
    Okx,
}

impl Venue {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Binance => "binance",
            Self::Bybit => "bybit",
            Self::Hyperliquid => "hyperliquid",
            Self::Okx => "okx",
        }
    }
}
```

`exchanges/okx/Cargo.toml`：

```toml
[package]
name = "poise-okx"
version.workspace = true
edition.workspace = true

[dependencies]
anyhow.workspace = true
async-trait.workspace = true
chrono.workspace = true
poise-core = { path = "../../core" }
poise-engine = { path = "../../engine" }
reqwest.workspace = true
serde.workspace = true
serde_json.workspace = true
tokio.workspace = true
```

`exchanges/okx/src/lib.rs`：

```rust
mod config;
mod connected;
mod startup_control;

pub use config::{Config, Deployment, Endpoints};
pub use connected::{Connected, connect};
pub use startup_control::SymbolLeverageControl;
```

`exchanges/okx/src/config.rs`：

```rust
use anyhow::{Result, anyhow};
use serde::Deserialize;

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub deployment: Deployment,
    pub api_key: Option<String>,
    pub api_secret: Option<String>,
    pub passphrase: Option<String>,
}

impl Config {
    pub fn credentials(&self) -> Result<Credentials> {
        Ok(Credentials {
            api_key: required_field(self.api_key.as_deref(), "exchange.api_key")?,
            api_secret: required_field(self.api_secret.as_deref(), "exchange.api_secret")?,
            passphrase: required_field(self.passphrase.as_deref(), "exchange.passphrase")?,
        })
    }

    pub fn endpoints(&self) -> Endpoints {
        self.deployment.endpoints()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Deployment {
    Mainnet,
    #[default]
    Demo,
}

impl Deployment {
    pub fn endpoints(&self) -> Endpoints {
        match self {
            Self::Mainnet => Endpoints::new(
                "https://www.okx.com",
                "wss://ws.okx.com:8443/ws/v5/public",
                "wss://ws.okx.com:8443/ws/v5/private",
                false,
            ),
            Self::Demo => Endpoints::new(
                "https://www.okx.com",
                "wss://wspap.okx.com:8443/ws/v5/public",
                "wss://wspap.okx.com:8443/ws/v5/private",
                true,
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoints {
    rest_base_url: String,
    public_ws_url: String,
    private_ws_url: String,
    simulated_trading: bool,
}

impl Endpoints {
    pub fn new(
        rest_base_url: impl Into<String>,
        public_ws_url: impl Into<String>,
        private_ws_url: impl Into<String>,
        simulated_trading: bool,
    ) -> Self {
        Self {
            rest_base_url: rest_base_url.into().trim_end_matches('/').to_string(),
            public_ws_url: public_ws_url.into().trim_end_matches('/').to_string(),
            private_ws_url: private_ws_url.into().trim_end_matches('/').to_string(),
            simulated_trading,
        }
    }

    pub fn rest_base_url(&self) -> &str {
        &self.rest_base_url
    }

    pub fn public_ws_url(&self) -> &str {
        &self.public_ws_url
    }

    pub fn private_ws_url(&self) -> &str {
        &self.private_ws_url
    }

    pub fn simulated_trading(&self) -> bool {
        self.simulated_trading
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credentials {
    api_key: String,
    api_secret: String,
    passphrase: String,
}

impl Credentials {
    pub fn api_key(&self) -> &str {
        &self.api_key
    }

    pub fn api_secret(&self) -> &str {
        &self.api_secret
    }

    pub fn passphrase(&self) -> &str {
        &self.passphrase
    }
}

fn required_field(value: Option<&str>, field_name: &str) -> Result<String> {
    let value = value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("missing required {field_name}"))?;
    Ok(value.to_string())
}
```

`exchanges/okx/src/connected.rs` 和 `startup_control.rs` 先提供可构造对象；运行方法返回清晰的 pending 错误只用于让 Task 1 通过，后续 task 会替换为真实实现。

`Cargo.toml` workspace members/default-members 增加 `exchanges/okx`。

`server/Cargo.toml` 增加：

```toml
poise-okx = { path = "../exchanges/okx" }
```

`server/src/config.rs` 增加 `use poise_okx as okx;`、`ExchangeConfig::Okx(okx::Config)`、`venue() -> Venue::Okx`。

`server/src/assembly.rs` 和 `server/src/exchange_startup.rs` 增加 OKX 分支。

- [x] **Step 4: 运行测试确认通过**

Run:

```bash
cargo test -p poise-core track::tests::venue_as_str_supports_okx
cargo test -p poise-server config::tests::parses_okx_exchange_config
cargo test -p poise-server assembly::tests::build_exchange_uses_exchange_deployment_for_okx_endpoint_selection
cargo test -p poise-server exchange_startup::tests::build_symbol_leverage_setter_accepts_okx_credentials
```

Expected: 全部 PASS。

- [ ] **Step 5: 提交并回写 SHA**

Commit message:

```bash
git add Cargo.toml core/src/track.rs server/Cargo.toml server/src/config.rs server/src/assembly.rs server/src/exchange_startup.rs exchanges/okx
git commit -m "Add OKX config boundary"
```

回写本 task 的 `Commit SHA`。

Commit SHA：

### Task 2: OKX 配置、端点和 REST 签名

**Files:**
- Modify: `exchanges/okx/Cargo.toml`
- Modify: `exchanges/okx/src/config.rs`
- Create: `exchanges/okx/src/rest/mod.rs`
- Create: `exchanges/okx/src/rest/auth.rs`

- [ ] **Step 1: 写失败测试**

`exchanges/okx/src/config.rs`：

```rust
#[test]
fn deployment_resolves_mainnet_and_demo_endpoints() {
    assert_eq!(
        Deployment::Mainnet.endpoints(),
        Endpoints::new(
            "https://www.okx.com",
            "wss://ws.okx.com:8443/ws/v5/public",
            "wss://ws.okx.com:8443/ws/v5/private",
            false,
        )
    );
    assert_eq!(
        Deployment::Demo.endpoints(),
        Endpoints::new(
            "https://www.okx.com",
            "wss://wspap.okx.com:8443/ws/v5/public",
            "wss://wspap.okx.com:8443/ws/v5/private",
            true,
        )
    );
}

#[test]
fn credentials_validate_required_fields_and_trim_values() {
    let config = Config {
        deployment: Deployment::Demo,
        api_key: Some("  demo-key  ".to_string()),
        api_secret: Some("\n demo-secret \t".to_string()),
        passphrase: Some(" demo-passphrase ".to_string()),
    };

    let credentials = config.credentials().unwrap();

    assert_eq!(credentials.api_key(), "demo-key");
    assert_eq!(credentials.api_secret(), "demo-secret");
    assert_eq!(credentials.passphrase(), "demo-passphrase");
}
```

`exchanges/okx/src/rest/auth.rs`：

```rust
#[test]
fn signs_okx_rest_payload_with_hmac_sha256_base64() {
    let signature = sign_okx_payload(
        "2020-12-08T09:08:57.715Z",
        "GET",
        "/api/v5/account/balance?ccy=BTC",
        "",
        "22582BD0CFF14C41EDBF1AB98506286D",
    );

    assert_eq!(signature, "HiZhvSfMtWJA3uUIVXV3a/bSXNPCWvYFXoGCVS8V4zY=");
}

#[test]
fn builds_websocket_login_signature_path() {
    let signature = sign_okx_payload(
        "1704876947",
        "GET",
        "/users/self/verify",
        "",
        "22582BD0CFF14C41EDBF1AB98506286D",
    );

    assert_eq!(signature, "5/36BgGV6m/6pmdc20zdqk0mzF5ZalmzzPD2fo3wavU=");
}
```

签名固定值由独立 HMAC-SHA256 + Base64 计算得到，测试不调用生产 signer 之外的 helper。

- [ ] **Step 2: 运行测试确认失败**

Run:

```bash
cargo test -p poise-okx config::tests::
cargo test -p poise-okx rest::auth::tests::
```

Expected: auth module and signer missing.

- [ ] **Step 3: 最小实现**

Add workspace dependency if missing:

```toml
base64 = "0.22"
```

`exchanges/okx/Cargo.toml`:

```toml
base64.workspace = true
hmac.workspace = true
sha2.workspace = true
```

`exchanges/okx/src/rest/mod.rs`:

```rust
pub(crate) mod auth;
```

`exchanges/okx/src/rest/auth.rs`:

```rust
use base64::{Engine as _, engine::general_purpose::STANDARD};
use hmac::{Hmac, Mac};
use sha2::Sha256;

pub(crate) fn sign_okx_payload(
    timestamp: &str,
    method: &str,
    request_path: &str,
    body: &str,
    secret_key: &str,
) -> String {
    let payload = format!("{timestamp}{method}{request_path}{body}");
    let mut mac = Hmac::<Sha256>::new_from_slice(secret_key.as_bytes())
        .expect("HMAC-SHA256 accepts any key length");
    mac.update(payload.as_bytes());
    STANDARD.encode(mac.finalize().into_bytes())
}
```

`exchanges/okx/src/lib.rs`:

```rust
mod rest;
```

- [ ] **Step 4: 运行测试确认通过**

Run:

```bash
cargo test -p poise-okx config::tests::
cargo test -p poise-okx rest::auth::tests::
```

Expected: 全部 PASS。

- [ ] **Step 5: 提交并回写 SHA**

Commit message:

```bash
git add Cargo.toml exchanges/okx
git commit -m "Add OKX config validation and signing"
```

Commit SHA：

### Task 3: REST 模型和 mapper

**Files:**
- Create: `exchanges/okx/src/rest/models.rs`
- Create: `exchanges/okx/src/mapper.rs`
- Modify: `exchanges/okx/src/lib.rs`
- Modify: `exchanges/okx/src/rest/mod.rs`

- [ ] **Step 1: 写失败测试**

覆盖以下行为：

- instrument response -> `ExchangeInfo`
- balance response -> `AccountSummarySnapshot`
- position response -> signed `Position`
- long/short mode position -> error
- pending order response -> `ExchangeOrder`
- OKX order state -> Poise `OrderStatus`

Test command:

```bash
cargo test -p poise-okx mapper::tests::
cargo test -p poise-okx rest::models::tests::
```

Expected: 模型和 mapper 缺失导致失败。

- [ ] **Step 2: 实现模型**

`OkxEnvelope<T>`:

```rust
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct OkxEnvelope<T> {
    pub code: String,
    pub msg: String,
    pub data: Vec<T>,
}
```

Required response models:

- `InstrumentInfo` with `inst_id`, `tick_sz`, `lot_sz`, `min_sz`, optional `ct_val`
- `BalanceSnapshot` with total equity / available equity / unrealized pnl fields needed by account summary
- `PositionSnapshot` with `inst_id`, `pos`, `avg_px`, `upl`, `pos_side`, `lever`
- `PendingOrderSnapshot` with `inst_id`, `ord_id`, `cl_ord_id`, `side`, `px`, `sz`, `acc_fill_sz`, `state`
- `OrderAck` with `ord_id`, `cl_ord_id`, `s_code`, `s_msg`
- `ServerTime` with `ts`

Use serde renames to mirror OKX fields exactly.

- [ ] **Step 3: 实现 mapper**

`mapper.rs` owns:

- `exchange_info_from_instrument`
- `account_summary_from_balance`
- `position_from_snapshot`
- `open_order_from_snapshot`
- `order_status_from_okx_state`
- `side_to_okx`

Mapping decisions:

- `Venue::Okx`
- `tickSz -> price_tick`
- `lotSz -> quantity_step`
- `minSz -> min_qty`
- `min_notional = 0.0` until OKX provides a stable per-SWAP field in the response used here
- `posSide == "net"` is required
- `pos` parses as signed quantity
- `live` / `partially_filled` -> active order
- `filled` -> `OrderStatus::Filled`
- `canceled` / `mmp_canceled` -> `OrderStatus::Canceled`

- [ ] **Step 4: 运行测试确认通过**

Run:

```bash
cargo test -p poise-okx mapper::tests::
cargo test -p poise-okx rest::models::tests::
```

Expected: 全部 PASS。

- [ ] **Step 5: 提交并回写 SHA**

Commit message:

```bash
git add exchanges/okx
git commit -m "Add OKX REST models and mappers"
```

Commit SHA：

### Task 4: REST client 查询和写操作

**Files:**
- Create: `exchanges/okx/src/rest/client.rs`
- Modify: `exchanges/okx/src/rest/mod.rs`
- Modify: `exchanges/okx/src/lib.rs`

- [ ] **Step 1: 写失败测试**

`rest::client::tests` 覆盖：

- public instruments request shape: `GET /api/v5/public/instruments?instType=SWAP&instId=BTC-USDT-SWAP`
- private balance request headers include OKX auth headers
- demo private request includes `x-simulated-trading: 1`
- submit order posts `tdMode = "cross"`、`ordType = "limit"`、`instId`、`clOrdId`、`side`、`px`、`sz`
- cancel order posts `/api/v5/trade/cancel-order`
- cancel all queries pending orders then posts `/api/v5/trade/cancel-batch-orders`
- set leverage posts `/api/v5/account/set-leverage` with `mgnMode = "cross"`
- non-zero OKX envelope code returns error including code/msg/path

Run:

```bash
cargo test -p poise-okx rest::client::tests::
```

Expected: client missing.

- [ ] **Step 2: 实现 REST client**

`OkxRestClient` fields:

```rust
pub(crate) struct OkxRestClient {
    http: reqwest::Client,
    base_url: String,
    credentials: Credentials,
    simulated_trading: bool,
    timestamp_provider: Arc<dyn Fn() -> chrono::DateTime<chrono::Utc> + Send + Sync>,
}
```

Methods:

- `new(config: &Config) -> Result<Self>`
- `with_http_client_and_timestamp_provider(...)` for tests
- `get_exchange_info`
- `get_account_summary`
- `get_account_capacity_snapshot(symbol)`
- `get_position`
- `get_open_orders`
- `submit_order`
- `cancel_order`
- `cancel_all`
- `set_leverage`
- `get_server_time`

Use `reqwest::Client::builder().no_proxy().build()?` to avoid system proxy side effects in tests.

- [ ] **Step 3: 运行测试确认通过**

Run:

```bash
cargo test -p poise-okx rest::client::tests::
```

Expected: 全部 PASS。

- [ ] **Step 4: 提交并回写 SHA**

Commit message:

```bash
git add exchanges/okx
git commit -m "Add OKX REST client"
```

Commit SHA：

### Task 5: Connected ports、startup control 和 server 装配

**Files:**
- Modify: `exchanges/okx/src/connected.rs`
- Modify: `exchanges/okx/src/startup_control.rs`
- Modify: `server/src/assembly.rs`
- Modify: `server/src/exchange_startup.rs`

- [ ] **Step 1: 写失败测试**

`exchanges/okx/src/connected.rs`:

```rust
#[tokio::test]
async fn connected_exposes_all_required_ports() {
    let config = Config {
        deployment: crate::Deployment::Demo,
        api_key: Some("demo-key".to_string()),
        api_secret: Some("demo-secret".to_string()),
        passphrase: Some("demo-passphrase".to_string()),
    };

    let connected = connect(&config).await.unwrap();

    let _execution = connected.execution();
    let _market_data = connected.market_data();
    let _account_summary = connected.account_summary();
    let _account = connected.account();
    let _metadata = connected.metadata();
}
```

Server tests from Task 1 must now pass through真实 OKX connected/startup 构造路径。

- [ ] **Step 2: 实现 connected/startup**

`connect(config)` builds:

- `Arc<OkxRestClient>`
- `Arc<OkxWsClient>` 的构造参数先在 Task 5 固定下来；Task 6 负责实现 WebSocket 行为并接入对应 port。

`ExecutionPort` routes REST methods.

`AccountSummaryPort` routes REST account summary.

`AccountPort::get_account_capacity_snapshot` routes REST capacity.

`MetadataPort` routes exchange info and server time.

`SymbolLeverageControl::set_leverage` routes REST set leverage.

- [ ] **Step 3: 运行测试确认通过**

Run:

```bash
cargo test -p poise-okx connected::tests::
cargo test -p poise-server assembly::tests::
cargo test -p poise-server exchange_startup::tests::
```

Expected: 全部 PASS。

- [ ] **Step 4: 提交并回写 SHA**

Commit message:

```bash
git add exchanges/okx server/src/assembly.rs server/src/exchange_startup.rs
git commit -m "Wire OKX exchange ports"
```

Commit SHA：

### Task 6: WebSocket 行情和用户事件

**Files:**
- Create: `exchanges/okx/src/ws/mod.rs`
- Create: `exchanges/okx/src/ws/market.rs`
- Create: `exchanges/okx/src/ws/account.rs`
- Create: `exchanges/okx/src/ws/models.rs`
- Modify: `exchanges/okx/Cargo.toml`
- Modify: `exchanges/okx/src/lib.rs`
- Modify: `exchanges/okx/src/connected.rs`

- [ ] **Step 1: 写失败测试**

`ws::tests` 覆盖：

- `tickers` message -> `MarketDataTick::ExecutionQuote`
- mark price message -> `MarketDataTick::MarkPrice`
- private login payload includes `apiKey`、`passphrase`、`timestamp`、`sign`
- `orders` message -> `UserDataPayload::OrderUpdate`
- order fill fields from `orders` -> `TrackPnlRecord::trade`
- `positions` message -> `UserDataPayload::PositionUpdate`
- public WS disconnect reconnects and resubscribes
- private WS disconnect reconnects, relogs in and resubscribes

Run:

```bash
cargo test -p poise-okx ws::tests::
```

Expected: WS module incomplete.

- [ ] **Step 2: 实现 WS client**

`OkxWsClient` fields:

```rust
pub(crate) struct OkxWsClient {
    public_ws_url: String,
    private_ws_url: String,
    credentials: Credentials,
    reconnect_delay: Duration,
    timestamp_provider: Arc<dyn Fn() -> i64 + Send + Sync>,
}
```

`subscribe_prices(instrument)`:

- connect public WS
- subscribe `tickers` for `instrument.symbol`
- subscribe mark price channel for `instrument.symbol`
- spawn read loop
- on disconnect, sleep backoff and resubscribe

`subscribe_user_data()`:

- connect private WS
- send login
- subscribe `orders` with `instType = "SWAP"`
- subscribe `positions` with `instType = "SWAP"`
- parse messages and send `UserDataEvent`
- on disconnect, sleep backoff, relogin and resubscribe

- [ ] **Step 3: 接入 connected ports**

`OkxMarketData` routes `subscribe_prices` to `OkxWsClient`.

`OkxAccount` routes `subscribe_user_data` to `OkxWsClient`.

- [ ] **Step 4: 运行测试确认通过**

Run:

```bash
cargo test -p poise-okx ws::tests::
cargo test -p poise-okx connected::tests::
```

Expected: 全部 PASS。

- [ ] **Step 5: 提交并回写 SHA**

Commit message:

```bash
git add exchanges/okx
git commit -m "Add OKX websocket streams"
```

Commit SHA：

### Task 7: 文档和最终验收

**Files:**
- Modify: `README.md`
- Modify: `SECURITY.md`
- Modify: `docs/system-overview.md`

- [ ] **Step 1: 更新文档**

README 需要说明：

- Poise 支持 Binance / Bybit / Hyperliquid / OKX 合约。
- `exchange.venue` 支持 `okx`。
- OKX 配置字段：`api_key`、`api_secret`、`passphrase`。
- OKX `deployment` 支持 `mainnet` 和 `demo`。
- OKX symbol 使用 `BTC-USDT-SWAP`。
- OKX 只支持 `SWAP` 永续、`cross`、`net`。

`SECURITY.md` 需要说明 OKX API key 应只给 Read/Trade，不开启 Withdraw，建议绑定 IP。

`docs/system-overview.md` 需要更新当前支持交易所、配置字段和 symbol 规则。

- [ ] **Step 2: 运行最终测试**

Run:

```bash
cargo test -p poise-okx
cargo test -p poise-core track::tests::venue_as_str_supports_okx
cargo test -p poise-server config::tests::
cargo test -p poise-server assembly::tests::
cargo test -p poise-server exchange_startup::tests::
```

Expected: 全部 PASS。

- [ ] **Step 3: 静态审计**

Run:

```bash
rg -n "not implemented|unimplemented|TODO|todo" exchanges/okx server/src/config.rs server/src/assembly.rs server/src/exchange_startup.rs core/src/track.rs -S
rg -n "\[ \]" docs/okx-integration-plan.md
git status --short
```

Expected:

- OKX 接入路径没有残留 `not implemented` / `TODO`
- 除当前 task SHA 回写前，没有未完成计划项
- 工作区只有计划 SHA 回写变更或为空

- [ ] **Step 4: 提交并回写 SHA**

Commit message:

```bash
git add README.md SECURITY.md docs/system-overview.md docs/okx-integration-design.md docs/okx-integration-plan.md
git commit -m "docs: document OKX swap support"
```

Commit SHA：

## 最终完成审计

关闭目标前必须重新核对：

- `venue = "okx"` 配置可解析。
- `Venue::Okx.as_str() == "okx"`。
- `exchanges/okx` 实现 REST 查询、下单、撤单、cancel all、set leverage、账户摘要、持仓、open orders。
- `exchanges/okx` 实现 WS 行情、mark、订单更新、成交 PNL、持仓更新、断线重连和重订阅。
- server 装配 OKX exchange 和 startup leverage setter。
- README、`SECURITY.md`、`docs/system-overview.md` 描述 OKX 当前边界。
- `docs/okx-integration-plan.md` 所有 task 均为 `[x]`，每个 task 有 commit SHA。
- 最终测试全部通过。
