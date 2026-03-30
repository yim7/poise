# Production Safety Hardening Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 修复三轮架构评审中发现的 P0/P1 生产安全问题，使系统能够安全地进行实盘交易。

**Architecture:** 全部改动沿现有六角架构的边界进行。reduce_only 和 client_order_id 从 engine 端口层贯穿到 Binance 适配层；风控预算从配置层贯穿到 core；flatten 通过 executor 已有的 ReducingOnly 语义实现；超时在适配层内部完成；优雅停机在 server 入口层实现；tick 新鲜度在 engine runtime 层实现。

**Tech Stack:** Rust, tokio, axum, reqwest, Binance USDⓈ-M Futures API

---

## Task 1: reduce_only 支持

减仓订单必须携带 `reduceOnly=true`，防止在仓位已平的情况下反向开仓。同时 `submit_requests_match` 必须比较 `reduce_only`，否则 recovery 会把 `reduce_only=true` 和 `reduce_only=false` 的订单当成等价请求。

**Files:**
- Modify: `engine/src/ports.rs` — `OrderRequest` 加 `reduce_only` 字段
- Modify: `engine/src/executor.rs` — `desired_order_to_request` 设置 `reduce_only`；`submit_requests_match` 加 `reduce_only` 比较
- Modify: `exchanges/binance/src/rest.rs` — `new_order` 发送 `reduceOnly` 参数
- Test: 各文件内 `#[cfg(test)] mod tests`

- [x] **Step 1: 在 OrderRequest 加 reduce_only 字段**

`engine/src/ports.rs`：

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrderRequest {
    pub instrument: Instrument,
    pub side: Side,
    pub price: f64,
    pub quantity: f64,
    pub client_order_id: String,
    #[serde(default)]
    pub reduce_only: bool,
}
```

- [x] **Step 2: 修复所有构造 OrderRequest 的地方的编译错误**

全局搜索 `OrderRequest {`，补上 `reduce_only: false`（或 `reduce_only: true`）。这些位置包括：
- `engine/src/executor.rs` 的 `desired_order_to_request` — 根据 `desired_order.role` 设置
- 所有测试中手工构造的 `OrderRequest`

`engine/src/executor.rs` 中 `desired_order_to_request`：

```rust
fn desired_order_to_request(
    input: &ExecutorInput<'_>,
    desired_order: &DesiredOrder,
) -> OrderRequest {
    OrderRequest {
        instrument: input.instrument.clone(),
        side: desired_order.side,
        price: desired_order.price,
        quantity: desired_order.quantity,
        client_order_id: format!("{}-reconcile", input.track_id.as_str()),
        reduce_only: desired_order.role == OrderRole::DecreaseInventory,
    }
}
```

- [x] **Step 3: 在 submit_requests_match 加 reduce_only 比较**

`engine/src/executor.rs`：

```rust
pub fn submit_requests_match(
    left: &OrderRequest,
    right: &OrderRequest,
    exchange_rules: &ExchangeRules,
) -> bool {
    left.instrument == right.instrument
        && left.side == right.side
        && left.client_order_id == right.client_order_id
        && left.reduce_only == right.reduce_only
        && values_match_with_step(left.price, right.price, exchange_rules.price_tick)
        && values_match_with_step(left.quantity, right.quantity, exchange_rules.quantity_step)
}
```

**为什么：** recovery 通过 `submit_requests_match` 判断 pending submit 是否仍等价于当前计划。如果不比较 `reduce_only`，一个 SELL reduce_only=true 和 SELL reduce_only=false 会被当成同一请求，recovery 会保留错误的 pending submit。

- [x] **Step 4: Binance REST 发送 reduceOnly 参数**

`exchanges/binance/src/rest.rs` 的 `new_order`：

```rust
pub async fn new_order(&self, req: &OrderRequest) -> Result<OrderReceipt> {
    let mut params = vec![
        ("symbol", req.instrument.symbol.clone()),
        ("side", side_to_binance(req.side).to_string()),
        ("type", "LIMIT".to_string()),
        ("timeInForce", "GTC".to_string()),
        ("quantity", format_decimal(req.quantity)),
        ("price", format_decimal(req.price)),
        ("newClientOrderId", req.client_order_id.clone()),
    ];
    if req.reduce_only {
        params.push(("reduceOnly", "true".to_string()));
    }
    let response: BinanceOrderResponse = self
        .send_request(Method::POST, "/fapi/v1/order", params, AuthMode::Signed)
        .await?;
    response.try_into()
}
```

- [x] **Step 5: 写验收测试 — executor 对减仓单设置 reduce_only**

`engine/src/executor.rs` tests：

```rust
#[test]
fn plan_sets_reduce_only_for_decrease_inventory_order() {
    let instrument = test_instrument();
    let rules = test_exchange_rules();
    let track_id = test_track_id();
    let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();

    let plan = plan(ExecutorInput {
        track_id: &track_id,
        instrument: &instrument,
        exchange_rules: &rules,
        base_qty_per_unit: 3.75,
        current_exposure: Exposure(6.0),
        target_exposure: Exposure(2.0),
        reference_price: 95.0,
        executor_state: None,
        observed_at: now,
    });

    let submit = plan.effects.iter().find_map(|e| match e {
        ExecutionAction::SubmitOrder { request, .. } => Some(request),
        _ => None,
    });
    assert!(submit.is_some());
    assert!(submit.unwrap().reduce_only);
}

#[test]
fn plan_does_not_set_reduce_only_for_increase_inventory_order() {
    let instrument = test_instrument();
    let rules = test_exchange_rules();
    let track_id = test_track_id();
    let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();

    let plan = plan(ExecutorInput {
        track_id: &track_id,
        instrument: &instrument,
        exchange_rules: &rules,
        base_qty_per_unit: 3.75,
        current_exposure: Exposure(0.0),
        target_exposure: Exposure(4.0),
        reference_price: 95.0,
        executor_state: None,
        observed_at: now,
    });

    let submit = plan.effects.iter().find_map(|e| match e {
        ExecutionAction::SubmitOrder { request, .. } => Some(request),
        _ => None,
    });
    assert!(submit.is_some());
    assert!(!submit.unwrap().reduce_only);
}
```

- [x] **Step 6: 写验收测试 — reduce_only 变化导致 recovery supersede**

`engine/src/executor.rs` tests。构造一个 pending submit (reduce_only=true)，当前计划产出的请求 reduce_only=false（其余字段不变），验证 recovery 判定为 stale 并 supersede：

```rust
#[test]
fn submit_recovery_supersedes_when_reduce_only_changes() {
    let instrument = test_instrument();
    let rules = test_exchange_rules();
    let track_id = test_track_id();
    let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();

    let old_request = OrderRequest {
        instrument: instrument.clone(),
        side: Side::Sell,
        price: 100.0,
        quantity: 3.75,
        client_order_id: format!("{}-reconcile", track_id.as_str()),
        reduce_only: true,
    };

    let previous_state = record_submit_request(
        &ExecutorState::empty(now),
        &old_request,
        Exposure(2.0),
    );

    let recovery = recover_submit_effect(SubmitRecoveryInput {
        request: &old_request,
        previous_state: &previous_state,
        live_order: None,
        current_exposure: &Exposure(6.0),
        target_exposure: &Exposure(10.0),
        exchange_rules: &rules,
        current_plan: Some(SubmitRecoveryPlanContext {
            track_id: &track_id,
            instrument: &instrument,
            base_qty_per_unit: 3.75,
            target_exposure: Exposure(10.0),
            reference_price: 100.0,
            observed_at: now,
        }),
    });

    assert!(matches!(
        recovery.resolution,
        SubmitRecoveryResolution::Superseded { .. }
    ));
}
```

- [x] **Step 7: 写验收测试 — Binance REST 发送 reduceOnly 参数**

`exchanges/binance/src/adapter.rs` tests：

```rust
#[tokio::test]
async fn submit_reduce_only_order_includes_reduce_only_param() {
    let server = MockHttpServer::spawn(vec![MockResponse::json(
        200,
        r#"{"orderId": 123, "clientOrderId": "test-1", "status": "NEW"}"#,
    )])
    .await;
    let adapter = BinanceAdapter::new("key", "secret", server.base_url(), "ws://127.0.0.1:1");

    adapter
        .submit_order(OrderRequest {
            instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
            side: Side::Sell,
            price: 95000.0,
            quantity: 0.01,
            client_order_id: "test-1".to_string(),
            reduce_only: true,
        })
        .await
        .unwrap();

    let requests = server.requests().await;
    assert!(requests[0].path.contains("reduceOnly=true"));
}

#[tokio::test]
async fn submit_non_reduce_only_order_omits_reduce_only_param() {
    let server = MockHttpServer::spawn(vec![MockResponse::json(
        200,
        r#"{"orderId": 124, "clientOrderId": "test-2", "status": "NEW"}"#,
    )])
    .await;
    let adapter = BinanceAdapter::new("key", "secret", server.base_url(), "ws://127.0.0.1:1");

    adapter
        .submit_order(OrderRequest {
            instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
            side: Side::Buy,
            price: 95000.0,
            quantity: 0.01,
            client_order_id: "test-2".to_string(),
            reduce_only: false,
        })
        .await
        .unwrap();

    let requests = server.requests().await;
    assert!(!requests[0].path.contains("reduceOnly"));
}
```

- [x] **Step 8: 运行全部测试**

Run: `cargo test`
Expected: PASS

- [x] **Step 9: Commit**

```bash
git add engine/src/ports.rs engine/src/executor.rs exchanges/binance/src/rest.rs exchanges/binance/src/adapter.rs
git commit -m "feat: add reduce_only to OrderRequest, recovery, and Binance adapter

DecreaseInventory orders carry reduce_only=true. submit_requests_match
now compares reduce_only so recovery correctly supersedes when the
reduce_only semantic changes."
```

Task 1 code commit:
`88224c04ba6691b3e5fde070e26e4f5fdc3aaa5d`

---

## Task 2: 风控预算可配置

当前 `daily_loss_limit` = -∞, `stop_loss_pct` = 100%，风控完全不生效。改为配置文件显式声明，缺省值仍然宽松但有限。

**Files:**
- Modify: `server/src/config.rs` — `GridDefinition` 加风控字段，`budget()` 读取
- Modify: `configs/binance-testnet.toml` — 示例配置
- Test: `server/src/config.rs` 内 tests

- [x] **Step 1: 在 GridDefinition 加可选风控字段**

`server/src/config.rs`：

```rust
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct GridDefinition {
    pub track_id: String,
    pub venue: Venue,
    pub symbol: String,
    pub lower_price: f64,
    pub upper_price: f64,
    pub long_exposure_units: f64,
    pub short_exposure_units: f64,
    pub notional_per_unit: f64,
    #[serde(default = "default_shape_family")]
    pub shape_family: ShapeFamily,
    #[serde(default = "default_out_of_band_policy")]
    pub out_of_band_policy: OutOfBandPolicy,
    pub max_notional: Option<f64>,
    pub daily_loss_limit: Option<f64>,
    pub stop_loss_pct: Option<f64>,
}
```

- [x] **Step 2: 修改 budget() 读取显式配置，保留合理缺省**

```rust
impl GridDefinition {
    pub fn budget(&self) -> CapacityBudget {
        let implied_max = self.long_exposure_units.max(self.short_exposure_units)
            * self.notional_per_unit;
        CapacityBudget {
            max_notional: self.max_notional.unwrap_or(implied_max),
            daily_loss_limit: self.daily_loss_limit.unwrap_or(-implied_max * 0.1),
            stop_loss_pct: self.stop_loss_pct.unwrap_or(10.0),
        }
    }
}
```

缺省逻辑：`daily_loss_limit` 默认 -10% max_notional，`stop_loss_pct` 默认 10%。不再是 -∞/100%。

- [x] **Step 3: 更新示例配置**

`configs/binance-testnet.toml` 追加：

```toml
max_notional = 10000
daily_loss_limit = -500
stop_loss_pct = 5.0
```

- [x] **Step 4: 写验收测试**

`server/src/config.rs` tests：

```rust
#[test]
fn budget_uses_explicit_risk_limits_when_configured() {
    let config = parse_config(
        r#"
environment = "test"

[[tracks]]
track_id = "btc-core"
venue = "binance"
symbol = "BTCUSDT"
lower_price = 90.0
upper_price = 110.0
long_exposure_units = 8.0
short_exposure_units = 8.0
notional_per_unit = 375.0
max_notional = 5000.0
daily_loss_limit = -200.0
stop_loss_pct = 5.0
"#,
    )
    .unwrap();

    let budget = config.grids[0].budget();
    assert!((budget.max_notional - 5000.0).abs() < f64::EPSILON);
    assert!((budget.daily_loss_limit - (-200.0)).abs() < f64::EPSILON);
    assert!((budget.stop_loss_pct - 5.0).abs() < f64::EPSILON);
}

#[test]
fn budget_uses_safe_defaults_when_risk_limits_omitted() {
    let config = parse_config(
        r#"
environment = "test"

[[tracks]]
track_id = "btc-core"
venue = "binance"
symbol = "BTCUSDT"
lower_price = 90.0
upper_price = 110.0
long_exposure_units = 8.0
short_exposure_units = 8.0
notional_per_unit = 375.0
"#,
    )
    .unwrap();

    let budget = config.grids[0].budget();
    let implied_max = 8.0 * 375.0; // 3000.0
    assert!((budget.max_notional - implied_max).abs() < f64::EPSILON);
    assert!((budget.daily_loss_limit - (-implied_max * 0.1)).abs() < f64::EPSILON);
    assert!((budget.stop_loss_pct - 10.0).abs() < f64::EPSILON);
}
```

- [x] **Step 5: 运行测试**

Run: `cargo test -p poise-server`
Expected: PASS

- [x] **Step 6: Commit**

```bash
git add server/src/config.rs configs/binance-testnet.toml
git commit -m "feat: make risk budget configurable with safe defaults

daily_loss_limit defaults to -10% of max_notional, stop_loss_pct
defaults to 10%. Previously both were effectively disabled."
```

Task 2 code commit:
`44fac5908838d601f4858d2a32b6c084383024a6`

---

## Task 3: client_order_id 唯一化

当前所有 reconcile 订单使用固定的 `{track_id}-reconcile`，存在 Binance 去重碰撞风险。

**关键设计决策：** 把 `client_order_id` 从 `submit_requests_match` 的等价比较中移除。`client_order_id` 仅用于交易所去重，不参与 recovery 等价判断。Recovery 等价性只看 (instrument, side, price, quantity, reduce_only)。这样 client_order_id 可以自由使用时间戳保证唯一，不会破坏 recovery 流程。

**Files:**
- Modify: `engine/src/executor.rs` — `desired_order_to_request` 生成唯一 id；`submit_requests_match` 移除 `client_order_id` 比较
- Test: `engine/src/executor.rs` tests

- [x] **Step 1: 从 submit_requests_match 移除 client_order_id 比较**

`engine/src/executor.rs`：

```rust
pub fn submit_requests_match(
    left: &OrderRequest,
    right: &OrderRequest,
    exchange_rules: &ExchangeRules,
) -> bool {
    left.instrument == right.instrument
        && left.side == right.side
        && left.reduce_only == right.reduce_only
        && values_match_with_step(left.price, right.price, exchange_rules.price_tick)
        && values_match_with_step(left.quantity, right.quantity, exchange_rules.quantity_step)
}
```

**为什么移除而非保留：** recovery 等价性的语义是"这个 pending submit 是否仍然是当前计划想做的事"。决定这件事的是 instrument/side/price/quantity/reduce_only，不是交易所去重 ID。保留 `client_order_id` 比较会导致：换了 ID 生成策略后，语义相同的订单被判为 stale，触发不必要的 supersede。

- [x] **Step 2: 在 desired_order_to_request 生成唯一 client_order_id**

```rust
fn desired_order_to_request(
    input: &ExecutorInput<'_>,
    desired_order: &DesiredOrder,
) -> OrderRequest {
    let timestamp_suffix = input.observed_at.timestamp_millis();
    OrderRequest {
        instrument: input.instrument.clone(),
        side: desired_order.side,
        price: desired_order.price,
        quantity: desired_order.quantity,
        client_order_id: format!("{}-{}", input.track_id.as_str(), timestamp_suffix),
        reduce_only: desired_order.role == OrderRole::DecreaseInventory,
    }
}
```

- [x] **Step 3: 修复引用 client_order_id 固定字符串的测试**

搜索测试中 `"btc-core-reconcile"` 的硬编码断言，改为检查前缀：

```rust
assert!(hint.request.client_order_id.starts_with("btc-core-"));
```

同样修复 `server/src/runtime.rs`、`server/src/effect_worker.rs` 等测试中硬编码的 `client_order_id`。

- [x] **Step 4: 写验收测试 — 不同时间戳产生不同 id**

```rust
#[test]
fn plan_generates_unique_client_order_ids_across_calls() {
    let instrument = test_instrument();
    let rules = test_exchange_rules();
    let track_id = test_track_id();
    let t1 = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();
    let t2 = t1 + Duration::milliseconds(1);

    let plan1 = plan(ExecutorInput {
        track_id: &track_id,
        instrument: &instrument,
        exchange_rules: &rules,
        base_qty_per_unit: 3.75,
        current_exposure: Exposure(0.0),
        target_exposure: Exposure(4.0),
        reference_price: 95.0,
        executor_state: None,
        observed_at: t1,
    });
    let plan2 = plan(ExecutorInput {
        track_id: &track_id,
        instrument: &instrument,
        exchange_rules: &rules,
        base_qty_per_unit: 3.75,
        current_exposure: Exposure(0.0),
        target_exposure: Exposure(4.0),
        reference_price: 95.0,
        executor_state: None,
        observed_at: t2,
    });

    let id1 = plan1.effects.iter().find_map(|e| match e {
        ExecutionAction::SubmitOrder { request, .. } => Some(&request.client_order_id),
        _ => None,
    });
    let id2 = plan2.effects.iter().find_map(|e| match e {
        ExecutionAction::SubmitOrder { request, .. } => Some(&request.client_order_id),
        _ => None,
    });

    assert!(id1.is_some());
    assert!(id2.is_some());
    assert_ne!(id1, id2);
    assert!(id1.unwrap().starts_with("btc-core-"));
}
```

- [x] **Step 5: 写验收测试 — recovery 不因 client_order_id 不同而 supersede**

验证语义相同（instrument/side/price/quantity/reduce_only 相同）但 client_order_id 不同的请求，recovery 仍判定为 match：

```rust
#[test]
fn submit_requests_match_ignores_client_order_id() {
    let rules = test_exchange_rules();
    let left = OrderRequest {
        instrument: test_instrument().clone(),
        side: Side::Buy,
        price: 95.0,
        quantity: 3.75,
        client_order_id: "btc-core-1711699500000".to_string(),
        reduce_only: false,
    };
    let right = OrderRequest {
        instrument: test_instrument().clone(),
        side: Side::Buy,
        price: 95.0,
        quantity: 3.75,
        client_order_id: "btc-core-1711699500050".to_string(),
        reduce_only: false,
    };
    assert!(submit_requests_match(&left, &right, &rules));
}
```

- [x] **Step 6: 运行测试**

Run: `cargo test`
Expected: PASS

- [x] **Step 7: Commit**

```bash
git add engine/src/executor.rs
git commit -m "fix: unique client_order_id without breaking recovery

client_order_id uses timestamp suffix for Binance dedup uniqueness.
submit_requests_match no longer compares client_order_id — recovery
equivalence is based on (instrument, side, price, quantity, reduce_only)."
```

Task 3 code commit:
`825c75f1a68f7939f87d23025cad9e1e180aa99b`

---

## Task 4: HTTP 请求超时

reqwest client 没有设置任何超时，卡住的请求会阻塞 effect worker。

**Files:**
- Modify: `exchanges/binance/src/rest.rs` — `build_http_client` 加超时
- Test: `exchanges/binance/src/rest.rs` tests

- [x] **Step 1: 给 reqwest client 加超时**

`exchanges/binance/src/rest.rs` 的 `build_http_client`：

```rust
fn build_http_client(base_url: &str) -> reqwest::Client {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(15));
    if should_bypass_proxy(base_url) {
        builder = builder.no_proxy();
    }
    builder.build().expect("failed to build reqwest client")
}
```

- [x] **Step 2: 写验收测试 — 连接超时**

```rust
#[tokio::test]
async fn request_times_out_when_server_does_not_respond() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();

    let client = BinanceRestClient::new(
        format!("http://{}", address),
        "key",
        "secret",
    );

    let result = tokio::time::timeout(
        Duration::from_secs(20),
        client.get_server_time(),
    )
    .await;

    assert!(result.is_ok(), "should not need external timeout");
    assert!(result.unwrap().is_err(), "request should fail with timeout");
}
```

- [x] **Step 3: 运行测试**

Run: `cargo test -p poise-binance`
Expected: PASS

- [x] **Step 4: Commit**

```bash
git add exchanges/binance/src/rest.rs
git commit -m "fix: add connect and request timeouts to Binance REST client

connect_timeout=5s, request_timeout=15s. Prevents effect worker from
hanging indefinitely on unresponsive exchange API."
```

Task 4 code commit:
`e213079eefd5cf8b614d1c2b01d1dec09755bea8`

---

## Task 5: 优雅停机

进程退出时必须先停止接收新工作，再做一轮有界 cleanup，并把 cleanup 后的交易所状态回写本地快照，避免“远端已撤单、本地仍显示有挂单”的停机假象。

**关键设计决策：**
- 不能先 `abort` 再 cleanup；否则 cleanup 结果无法回写。
- 本 task 的目标是 `best-effort exchange cleanup + final sync`，不是承诺“远端绝对空单才退出”。
- `effect worker` 收到停机信号后只完成当前 in-flight effect，不再拉取新的 persisted effect。

**Files:**
- Modify: `Cargo.toml` — 为 tokio 打开 `signal` feature
- Modify: `engine/src/manager.rs` — 增加 shutdown 专用的 exchange state sync 路径
- Modify: `server/src/main.rs` — SIGINT / SIGTERM 监听
- Modify: `server/src/runtime.rs` — shutdown signal、ordered shutdown、final sync
- Modify: `server/src/effect_worker.rs` — 支持 shutdown 后停止拉取新 effect
- Modify: `server/src/write_service.rs` — 暴露不触发 follow-up reconcile 的 sync 方法
- Test: `server/src/runtime.rs`, `server/src/effect_worker.rs`

- [x] **Step 1: 给 runtime / effect worker 增加 shutdown signal**

在 `server/src/runtime.rs` 中为 `ServerRuntime` 增加 `watch::Sender<bool>`，`start()` 时把 `watch::Receiver<bool>` 传给市场任务、用户流任务、recovery 任务和 effect worker。

`server/src/effect_worker.rs`：

```rust
pub struct EffectWorker {
    state: ServerState,
    exchange: Arc<dyn ExchangePort>,
    poll_interval: Duration,
    shutdown_rx: watch::Receiver<bool>,
}

pub async fn run_until_shutdown(&self) -> Result<()> {
    loop {
        if *self.shutdown_rx.borrow() {
            return Ok(());
        }
        tokio::select! {
            _ = self.shutdown_rx.changed() => {
                if *self.shutdown_rx.borrow() {
                    return Ok(());
                }
            }
            _ = sleep(self.poll_interval) => {
                self.run_once().await?;
            }
        }
    }
}
```

- [x] **Step 2: runtime 实现有序 shutdown，并在 cleanup 后做 final sync**

`server/src/runtime.rs`：

```rust
impl ServerRuntime {
    pub async fn shutdown(&self, mut handles: RuntimeHandles) {
        let _ = self.shutdown_tx.send(true);
        tracing::info!("shutdown signal sent");

        let drain_timeout = Duration::from_secs(30);
        if tokio::time::timeout(drain_timeout, &mut handles.effect_task)
            .await
            .is_err()
        {
            tracing::warn!("effect worker drain timed out after {drain_timeout:?}");
            handles.effect_task.abort();
        }

        let grids = self.state.write_service.grid_instruments().await;
        for grid in &grids {
            if let Err(error) = self.exchange.cancel_all(&grid.instrument).await {
                tracing::warn!(
                    "failed to cancel all orders for {} during shutdown: {error}",
                    grid.instrument.symbol
                );
                continue;
            }

            if let Err(error) = sync_exchange_state_from_exchange(
                &self.state,
                &self.exchange,
                &grid.id,
                &grid.instrument,
            )
            .await
            {
                tracing::warn!(
                    "failed to persist final exchange state for {} during shutdown: {}",
                    grid.instrument.symbol,
                    error.message()
                );
            }
        }

        handles.market_task.abort();
        handles.user_task.abort();
        handles.recovery_task.abort();
        let _ = handles.market_task.await;
        let _ = handles.user_task.await;
        let _ = handles.recovery_task.await;

        tracing::info!("shutdown complete");
    }
}
```

- [x] **Step 3: main 中监听 SIGINT + SIGTERM**

`server/src/main.rs`：

```rust
let shutdown_signal = async {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("failed to install SIGTERM handler");
        tokio::select! {
            _ = ctrl_c => {}
            _ = sigterm.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        ctrl_c.await.expect("failed to install ctrl+c handler");
    }
};
```

- [x] **Step 4: 写验收测试**

`server/src/effect_worker.rs` tests：

```rust
#[tokio::test]
async fn effect_worker_stops_polling_new_effects_after_shutdown_signal() {
    // 先处理一个 in-flight effect，再发送 shutdown，验证不会再拉第二个 effect
}
```

`server/src/runtime.rs` tests：

```rust
#[tokio::test]
async fn shutdown_cancels_orders_and_persists_final_exchange_state() {
    // 启动 runtime，seed open order，触发 shutdown
    // 断言 fake exchange 收到 cancel_all
    // 断言 write_service 最终 snapshot 对应 open_orders 已清空
}
```

- [x] **Step 5: 运行测试**

Run: `cargo test -p poise-server`
Expected: PASS

- [x] **Step 6: Commit**

```bash
git add server/src/main.rs server/src/runtime.rs server/src/effect_worker.rs
git commit -m "feat: graceful shutdown with ordered drain and final sync

Shutdown sequence is: stop intake -> drain in-flight effects (30s) ->
cancel all open orders -> persist final exchange state -> abort
remaining tasks. Supports both SIGINT and SIGTERM."
```

Task 5 code commit:
`ccca393`

---

## Task 6: flatten 命令

实盘需要一个紧急退出命令，但它不能复用当前“带外 ReduceOnly”状态机语义。`flatten` 必须把“人工目标覆盖为 0”持久化下来，否则下一次带内重算会把 target 又拉回策略曲线。

**关键设计决策：**
- 不把 `flatten` 建立在现有 `ReducingOnly` 状态本身上；`ReducingOnly` 继续表示策略/带外结果。
- 新增持久化控制位 `manual_target_override: Option<Exposure>`。`flatten` 把它设为 `Some(Exposure(0.0))`。
- `reconciler` 发现 override 时直接使用 override target，不再走带内曲线 target 计算。
- `Resume` 在“人工 flatten 生效中”时清除 override，恢复正常策略控制。

**Files:**
- Modify: `engine/src/runtime.rs` — `GridRuntime` 增加 `manual_target_override`
- Modify: `engine/src/snapshot.rs` — `GridRuntimeSnapshot` 持久化 `manual_target_override`
- Modify: `engine/src/command.rs` — 加 `Flatten`
- Modify: `engine/src/reconciler.rs` — override 优先于曲线 target
- Modify: `engine/src/manager.rs` — `flatten_grid` / `resume_grid` 收敛控制语义
- Modify: `server/src/http.rs` — 放行 `Flatten`
- Modify: `server/src/projector.rs` — `available_commands` 反映 flatten 已实现
- Modify: `storage/src/schema.rs` / `storage/src/sqlite.rs` — 持久化 `manual_target_override`，保证重启后 override 不丢失
- Modify: `docs/protocol-contract.md` — 同步命令语义
- Test: `engine/src/manager.rs`, `engine/src/runtime.rs`, `server/src/http.rs`, `server/src/projector.rs`

- [x] **Step 1: 在 runtime / snapshot 中增加持久化的 manual_target_override**

`engine/src/runtime.rs`：

```rust
pub struct GridRuntime {
    // ... existing fields ...
    pub manual_target_override: Option<Exposure>,
}
```

`engine/src/snapshot.rs`：

```rust
pub struct GridRuntimeSnapshot {
    // ... existing fields ...
    #[serde(default)]
    pub manual_target_override: Option<Exposure>,
}
```

同时更新 `snapshot()` / `restore_from_snapshot()` 和相关 round-trip 测试。

- [x] **Step 2: 在 reconciler 中让 override 优先于曲线 target**

`engine/src/reconciler.rs`：

```rust
pub fn reconcile_target(grid: &GridRuntime, reference_price: f64) -> TargetReconcileResult {
    if let Some(target_override) = grid.manual_target_override.clone() {
        let delta = grid.current_exposure.delta(&target_override);
        return TargetReconcileResult {
            events: (!delta.is_zero()).then(|| DomainEvent::ExposureTargetChanged {
                from: grid.current_exposure.clone(),
                to: target_override.clone(),
            }).into_iter().collect(),
            target_exposure: target_override,
            new_status: Some(GridStatus::ReducingOnly),
            suppress_execution: delta.is_zero(),
        };
    }

    // existing band / risk logic
}
```

**关键点：** override 生效时不再看 band status，也不把 `Frozen/Holding/ReduceOnly` 的策略语义和人工退出意图混在一起。

- [x] **Step 3: manager 实现 flatten / resume 的控制语义**

`engine/src/manager.rs`：

```rust
fn flatten_grid(&mut self, id: &GridId) -> Result<(Vec<DomainEvent>, Vec<GridEffect>)> {
    let grid = self
        .grids
        .get_mut(id)
        .ok_or_else(|| anyhow::anyhow!("grid `{}` not found", id.as_str()))?;

    if matches!(grid.status, GridStatus::Terminated) {
        bail!("cannot flatten terminated grid `{}`", id.as_str());
    }

    grid.manual_target_override = Some(Exposure(0.0));
    grid.status = GridStatus::ReducingOnly;

    match grid.reference_price {
        Some(price) => self.reconcile_grid(id, price),
        None => Ok((vec![], vec![])),
    }
}
```

`resume_grid()` 需要改成：
- `Paused` 时保留现有语义
- `manual_target_override.is_some()` 且当前为人工 flatten 状态时，清掉 override 后再按当前价格恢复正常 target

- [x] **Step 4: HTTP / projector / 协议文档同步 flatten 已实现**

`server/src/http.rs` 放行 `Flatten`；`server/src/projector.rs` 里的 `available_commands` 不再把 flatten 固定标成 disabled；`docs/protocol-contract.md` 去掉“flatten 未实现”的描述。

- [x] **Step 5: 写验收测试**

`engine/src/manager.rs` tests：

```rust
#[test]
fn flatten_persists_manual_target_override_and_targets_zero() {
    let mut manager = test_manager_with_active_grid();
    let track_id = GridId::new("btc1");

    let transition = manager.command(&track_id, GridCommand::Flatten).unwrap();

    let grid = manager.get_grid("btc1").unwrap();
    assert_eq!(grid.manual_target_override, Some(Exposure(0.0)));
    assert_eq!(grid.status, GridStatus::ReducingOnly);
    assert_eq!(transition.snapshot.manual_target_override, Some(Exposure(0.0)));
}

#[test]
fn flatten_keeps_zero_target_even_when_price_is_in_band() {
    let mut manager = test_manager_with_active_grid();
    let track_id = GridId::new("btc1");

    manager.command(&track_id, GridCommand::Flatten).unwrap();
    let transition = manager
        .observe(&track_id, GridObservation::Market(MarketObservation { reference_price: 100.0 }))
        .unwrap();

    assert_eq!(transition.snapshot.target_exposure, Some(Exposure(0.0)));
}

#[test]
fn resume_clears_manual_target_override_after_flatten() {
    let mut manager = test_manager_with_active_grid();
    let track_id = GridId::new("btc1");

    manager.command(&track_id, GridCommand::Flatten).unwrap();
    manager.resume_grid("btc1").unwrap();

    let grid = manager.get_grid("btc1").unwrap();
    assert!(grid.manual_target_override.is_none());
}
```

`server/src/http.rs` / `server/src/projector.rs` tests：
- `flatten` 返回 `200`
- detail 中 `available_commands` 不再显示“flatten command is not implemented”

- [x] **Step 6: 运行测试**

Run: `cargo test`
Expected: PASS

- [x] **Step 7: Commit**

```bash
git add engine/src/runtime.rs engine/src/snapshot.rs engine/src/command.rs engine/src/reconciler.rs engine/src/manager.rs server/src/http.rs server/src/projector.rs docs/protocol-contract.md
git commit -m "feat: implement flatten via persisted manual target override

Flatten now sets manual_target_override=0 and keeps that override
across reprice and restart. Resume clears the override and returns
the grid to strategy-owned targeting."
```

Task 6 code commit:
`6d64cf4`

---

## Task 7: Tick 新鲜度监控

当前价格流断掉后，grid 会继续使用最后一次参考价交易。这个问题不能复用现有 `Frozen` 状态解决，因为 `Frozen` 已经表示带外 `Freeze` 策略。行情健康度必须作为独立观测状态建模。

**关键设计决策：**
- 不复用 `GridStatus::Frozen`。
- 在观测态中新增 `last_tick_at` 和 `market_data_stale_since`。
- 行情过期时“暂停执行并标记 attention required”，而不是改写策略生命周期状态。
- 新 tick 到达后清除 stale 标记，恢复正常执行。

**Files:**
- Modify: `engine/src/runtime.rs` — `GridRuntime` 增加 `last_tick_at` / `market_data_stale_since` / `tick_timeout_secs`
- Modify: `engine/src/snapshot.rs` — `ObservedState` 持久化 `last_tick_at` / `market_data_stale_since`
- Modify: `engine/src/manager.rs` — market tick 更新健康度；reconcile 前做 freshness guard
- Modify: `server/src/config.rs` — `GridDefinition` 加 `tick_timeout_secs`
- Modify: `server/src/assembly.rs` — 组装 `tick_timeout_secs`
- Modify: `server/src/projector.rs` — stale data 也投影为 `AttentionRequired`
- Modify: `storage/src/schema.rs` / `storage/src/sqlite.rs` — 持久化 stale 观测字段，保证重启后健康状态不丢失
- Test: `engine/src/manager.rs`, `server/src/config.rs`, `server/src/projector.rs`

- [x] **Step 1: 在 runtime / snapshot 里增加行情健康字段**

`engine/src/runtime.rs`：

```rust
pub struct GridRuntime {
    // ... existing fields ...
    pub last_tick_at: Option<DateTime<Utc>>,
    pub market_data_stale_since: Option<DateTime<Utc>>,
    pub tick_timeout_secs: u64,
}
```

`engine/src/snapshot.rs`：

```rust
pub struct ObservedState {
    pub reference_price: Option<f64>,
    pub out_of_band_since: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_tick_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub market_data_stale_since: Option<DateTime<Utc>>,
}
```

- [x] **Step 2: config / assembly 把 tick_timeout_secs 传到 GridRuntime**

`server/src/config.rs` 的 `GridDefinition` 增加：

```rust
pub tick_timeout_secs: Option<u64>,
```

`server/src/assembly.rs` 在 `manager.add_grid(...)` 附近把默认值 `30` 秒传进去；相应扩展 `GridManager::add_grid(...)` / `GridRuntime::new(...)` 的参数列表。

- [x] **Step 3: manager 在 market tick 更新健康度，并在 reconcile 前做 freshness guard**

`engine/src/manager.rs`：

```rust
fn observe_market(
    &mut self,
    id: &GridId,
    observation: MarketObservation,
) -> Result<(Vec<DomainEvent>, Vec<GridEffect>)> {
    let now = self.clock.now();
    let grid = self
        .grids
        .get_mut(id)
        .ok_or_else(|| anyhow::anyhow!("grid `{}` not found", id.as_str()))?;
    grid.last_tick_at = Some(now);
    grid.market_data_stale_since = None;
    self.reconcile_grid(id, observation.reference_price)
}

fn guard_market_data_freshness(&mut self, id: &GridId) -> Result<bool> {
    let now = self.clock.now();
    let grid = self
        .grids
        .get_mut(id)
        .ok_or_else(|| anyhow::anyhow!("grid `{}` not found", id.as_str()))?;

    let Some(last_tick_at) = grid.last_tick_at else {
        return Ok(false);
    };

    let age_ms = (now - last_tick_at).num_milliseconds().max(0);
    if age_ms <= i64::try_from(grid.tick_timeout_secs).unwrap_or(30) * 1000 {
        return Ok(false);
    }

    if grid.market_data_stale_since.is_none() {
        grid.market_data_stale_since = Some(now);
    }

    Ok(true)
}
```

在 `reconcile_grid()`、`command(Reconcile)`、以及会触发 follow-up reconcile 的 position / order 路径里，若 `guard_market_data_freshness()` 返回 `true`，则返回空 effect，不更新 target。

- [x] **Step 4: projector 把 stale data 也投影为 AttentionRequired**

`server/src/projector.rs`：

```rust
fn project_execution_status(source: &GridReadModelSource) -> ExecutionStatusView {
    if source.snapshot.executor_state.recovery_anomaly.is_some()
        || source.snapshot.observed.market_data_stale_since.is_some()
    {
        ExecutionStatusView::AttentionRequired
    } else {
        ExecutionStatusView::Normal
    }
}
```

- [x] **Step 5: 写验收测试**

`engine/src/manager.rs` tests。引入一个可变测试时钟：

```rust
#[derive(Clone)]
struct MutableClock(Arc<std::sync::Mutex<DateTime<Utc>>>);

impl ClockPort for MutableClock {
    fn now(&self) -> DateTime<Utc> {
        *self.0.lock().unwrap()
    }
}

impl MutableClock {
    fn set(&self, value: DateTime<Utc>) {
        *self.0.lock().unwrap() = value;
    }
}
```

测试：

```rust
#[test]
fn stale_market_data_suspends_follow_up_reconcile_without_overwriting_status() {
    let started_at = Utc.with_ymd_and_hms(2026, 3, 29, 8, 0, 0).unwrap();
    let clock = MutableClock(Arc::new(std::sync::Mutex::new(started_at)));
    let mut manager = test_manager_with_clock(Arc::new(clock.clone()));
    let track_id = GridId::new("btc1");

    manager.observe(
        &track_id,
        GridObservation::Market(MarketObservation { reference_price: 95.0 }),
    ).unwrap();

    clock.set(Utc.with_ymd_and_hms(2026, 3, 29, 8, 1, 0).unwrap());
    let transition = manager.observe(
        &track_id,
        GridObservation::Position(PositionObservation { qty: 0.0, unrealized_pnl: 0.0 }),
    ).unwrap();

    assert!(transition.effects.is_empty());
    assert!(transition.snapshot.observed.market_data_stale_since.is_some());
    assert_eq!(transition.snapshot.status, GridStatus::Active);
}

#[test]
fn fresh_tick_clears_market_data_stale_flag() {
    let started_at = Utc.with_ymd_and_hms(2026, 3, 29, 8, 0, 0).unwrap();
    let clock = MutableClock(Arc::new(std::sync::Mutex::new(started_at)));
    let mut manager = test_manager_with_clock(Arc::new(clock.clone()));
    let track_id = GridId::new("btc1");

    manager.observe(
        &track_id,
        GridObservation::Market(MarketObservation { reference_price: 95.0 }),
    ).unwrap();

    clock.set(Utc.with_ymd_and_hms(2026, 3, 29, 8, 1, 0).unwrap());
    let _ = manager.observe(
        &track_id,
        GridObservation::Position(PositionObservation { qty: 0.0, unrealized_pnl: 0.0 }),
    ).unwrap();

    let transition = manager.observe(
        &track_id,
        GridObservation::Market(MarketObservation { reference_price: 96.0 }),
    ).unwrap();

    assert!(transition.snapshot.observed.market_data_stale_since.is_none());
}
```

`server/src/projector.rs` tests：

```rust
#[test]
fn stale_market_data_projects_attention_required() {
    let mut source = test_read_model_source();
    source.snapshot.observed.market_data_stale_since =
        Some(Utc.with_ymd_and_hms(2026, 3, 29, 8, 1, 0).unwrap());

    let detail = GridProjector::new().project_detail(&source);
    assert_eq!(detail.execution.execution_status, ExecutionStatusView::AttentionRequired);
}
```

- [x] **Step 6: 运行测试**

Run: `cargo test`
Expected: PASS

- [x] **Step 7: Commit**

```bash
git add engine/src/runtime.rs engine/src/snapshot.rs engine/src/manager.rs server/src/config.rs server/src/assembly.rs server/src/projector.rs
git commit -m "feat: suspend execution on stale market data

Stale market data is modeled as observed health, not as GridStatus::Frozen.
Execution is suspended while stale and projector surfaces
attention_required until a fresh tick clears the condition."
```

Task 7 code commit:
`41865ce`

---

## 验收标准

全部 7 个 task 完成后，运行：

```bash
cargo test
```

预期全部通过。以下行为可验证：

1. **reduce_only**: DecreaseInventory 方向的订单发到 Binance 时携带 `reduceOnly=true`；`submit_requests_match` 比较 `reduce_only`，recovery 不会混淆减仓和增仓 submit
2. **风控预算**: 配置文件中可以显式设置 `daily_loss_limit`、`stop_loss_pct`；不设置时默认值有限而非 `-∞ / 100%`
3. **client_order_id**: 同一网格连续两次 plan 产生的 `client_order_id` 不同；recovery 等价性不依赖 `client_order_id`，语义相同的请求仍被判为等价
4. **HTTP 超时**: reqwest client 的 `connect_timeout=5s`、`timeout=15s`
5. **优雅停机**: SIGINT / SIGTERM 触发有序停机：停止 intake -> drain in-flight effect -> `cancel_all` -> final sync -> abort 剩余任务
6. **flatten**: `POST /tracks/:id/commands {"command":"flatten"}` 返回 `200`，grid 持久化 `manual_target_override=0`，后续带内价格更新也不会把 target 拉回策略曲线；`Resume` 清掉 override 后恢复正常策略控制
7. **tick 新鲜度**: 行情中断超过 `tick_timeout_secs` 后执行被暂停，detail / list 投影为 `attention_required`；新 tick 到达后 stale 标记清除并恢复正常执行
