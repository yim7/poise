# 网格平台第三阶段实现计划：grid-binance

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

> **执行结果：** 已在 `codex/phase3-binance` 分支完成实现并通过验收。原计划中的分步提交改为本次开发结束后的集中提交。

**Goal:** 实现 Binance USDⓈ-M Futures 交易所适配器，完成 `ExchangePort` 和 `MarketDataPort` trait 的具体实现。

**Architecture:** 六边形架构适配器层。grid-binance 实现 grid-engine 中定义的两个端口 trait，封装 Binance 特有的 REST/WS 协议、签名、限速和重连逻辑。详见[架构设计 spec](../specs/2026-03-24-grid-platform-architecture-design.md)。

**Tech Stack:** Rust, reqwest (REST), tokio-tungstenite (WebSocket), hmac-sha256 (签名)

**前置依赖：** 第一阶段（grid-core + grid-engine）已完成。

---

## File Structure

### 新建文件

```
exchanges/binance/
├── Cargo.toml
└── src/
    ├── lib.rs
    ├── types.rs        # Binance 特有请求/响应类型 + 与 core 类型的转换
    ├── rest.rs         # REST API 客户端（签名、限速、重试）
    ├── websocket.rs    # WebSocket 客户端（市场数据流 + 用户数据流）
    └── adapter.rs      # ExchangePort + MarketDataPort 实现
```

### 修改文件

- `Cargo.toml`（workspace 根）：添加 `"exchanges/binance"` 到 members

---

### Task 1: 初始化 grid-binance crate

**Files:**
- Modify: `Cargo.toml`
- Create: `exchanges/binance/Cargo.toml`
- Create: `exchanges/binance/src/lib.rs`

- [x] **Step 1: 添加依赖到 workspace**

在 `Cargo.toml` 的 `[workspace.dependencies]` 中添加：

```toml
reqwest = { version = "0.11.27", default-features = false, features = ["json", "rustls-tls"] }
tokio-tungstenite = { version = "0.24", features = ["rustls-tls-webpki-roots"] }
hmac = "0.12"
sha2 = "0.10"
hex = "0.4"
url = "2"
```

在 `[workspace].members` 中添加 `"exchanges/binance"`。

- [x] **Step 2: 创建 exchanges/binance/Cargo.toml**

```toml
[package]
name = "grid-binance"
version.workspace = true
edition.workspace = true

[dependencies]
grid-engine = { path = "../../engine" }
grid-core = { path = "../../core" }
reqwest.workspace = true
tokio-tungstenite.workspace = true
tokio.workspace = true
serde.workspace = true
serde_json.workspace = true
anyhow.workspace = true
async-trait.workspace = true
chrono.workspace = true
hmac.workspace = true
sha2.workspace = true
hex.workspace = true
url.workspace = true
futures-util = "0.3"
tracing = "0.1"
```

- [x] **Step 3: 创建占位模块**

- [x] **Step 4: 验证编译**

Run: `cargo check -p grid-binance`

- [x] **Step 5: 提交**

```bash
git add -A && git commit -m "feat: initialize grid-binance crate"
```

---

### Task 2: Binance 类型定义与转换

**Files:**
- Create: `exchanges/binance/src/types.rs`

- [x] **Step 1: 写测试**

测试 Binance JSON 响应反序列化和到 engine 类型的转换：
- `BinanceOrderResponse` → `OrderReceipt`
- `BinancePositionRisk` → `Position`
- `BinanceOpenOrder` → `OpenOrder`
- `BinanceExchangeInfo` → `ExchangeInfo`

- [x] **Step 2: 运行测试确认失败**

Run: `cargo test -p grid-binance -- types`

- [x] **Step 3: 实现类型定义和转换**

定义 Binance 特有的 serde 结构体，实现 `From`/`Into` 转换到 engine 端口类型。

- [x] **Step 4: 运行测试确认通过**

- [x] **Step 5: 提交**

```bash
git add -A && git commit -m "feat(binance): add Binance types and conversion to engine port types"
```

---

### Task 3: REST API 客户端

**Files:**
- Create: `exchanges/binance/src/rest.rs`

- [x] **Step 1: 写测试**

测试签名生成：给定 API key、secret 和查询参数，验证 HMAC-SHA256 签名正确。

- [x] **Step 2: 运行测试确认失败**

- [x] **Step 3: 实现 REST 客户端**

- `BinanceRestClient` struct：持有 reqwest::Client、api_key、api_secret、base_url
- 签名方法：HMAC-SHA256
- 公开方法：`get_exchange_info`、`get_position`、`get_open_orders`、`new_order`、`cancel_order`、`cancel_all_orders`
- 内置重试（最多 3 次，指数退避）

- [x] **Step 4: 运行测试确认通过**

- [x] **Step 5: 提交**

```bash
git add -A && git commit -m "feat(binance): add REST API client with HMAC signing and retry"
```

---

### Task 4: WebSocket 客户端

**Files:**
- Create: `exchanges/binance/src/websocket.rs`

- [x] **Step 1: 实现 WebSocket 客户端**

- `BinanceWsClient` struct
- 市场数据流：连接 `wss://fstream.binance.com/ws/<symbol>@markPrice`，解析为 `PriceTick`
- 用户数据流：通过 listenKey 连接，解析订单更新和仓位更新为 `UserDataEvent`
- 自动重连：断线后指数退避重连
- 通过 `mpsc::Sender` 向外推送事件

- [x] **Step 2: 验证编译通过**

Run: `cargo check -p grid-binance`

- [x] **Step 3: 提交**

```bash
git add -A && git commit -m "feat(binance): add WebSocket client with auto-reconnect"
```

---

### Task 5: ExchangePort + MarketDataPort 适配器

**Files:**
- Create: `exchanges/binance/src/adapter.rs`

- [x] **Step 1: 写测试**

使用 mock HTTP server（如 wiremock）测试 adapter 的端口方法：
- `submit_order` 调用 REST `new_order` 并返回 `OrderReceipt`
- `cancel_order` 调用 REST `cancel_order`
- `get_position` 调用 REST 并转换类型
- `subscribe_prices` 返回 `mpsc::Receiver<PriceTick>`

- [x] **Step 2: 运行测试确认失败**

- [x] **Step 3: 实现 BinanceAdapter**

```rust
pub struct BinanceAdapter {
    rest: BinanceRestClient,
    ws_base_url: String,
}

#[async_trait]
impl ExchangePort for BinanceAdapter { ... }

#[async_trait]
impl MarketDataPort for BinanceAdapter { ... }
```

- [x] **Step 4: 运行测试确认通过**

- [x] **Step 5: 验证全部测试通过**

Run: `cargo test -p grid-binance`

- [x] **Step 6: 提交**

```bash
git add -A && git commit -m "feat(binance): implement ExchangePort and MarketDataPort adapter"
```

---

## 验收标准

1. `cargo test -p grid-binance` 全部通过
2. `BinanceAdapter` 实现 `ExchangePort` 和 `MarketDataPort`
3. 签名计算有独立测试覆盖
4. Binance JSON ↔ engine 类型转换有测试覆盖
5. WebSocket 支持自动重连
6. 加第二家交易所只需新建 crate 实现同样 trait，不需修改 engine
