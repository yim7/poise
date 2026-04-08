# 单服务单交易所 Exchange 边界实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: 使用 `superpowers:subagent-driven-development`（推荐）或 `superpowers:executing-plans` 按 task 执行本计划。步骤使用 checkbox (`- [ ]`) 语法追踪。

**Goal:** 把单服务单交易所边界落到代码里：`Venue` 只表示交易所身份，`Exchange` 只表示 `server` 装配出的运行时对象；`engine` 只依赖窄 `Port`；读侧保留窄 `AccountSummaryPort`；`server` 不再解释 Binance endpoint；`poise-binance` 成为第一个按新边界实现的交易所接入。

**Architecture:** 实现按 owner 迁移顺序推进，不在中途保留过渡结构。先把服务级 `exchange` 配置和 `track.symbol` 边界固定下来；随后一次完成 `engine` 窄 `Port` 拆分和 `server` 装配层 `Exchange` 引入，避免同一批模块发生两次重布线；接着把 Binance 的 deployment / endpoint 决策和 `connected` 装配入口一起下沉到 `poise-binance`，同时停止依赖旧 `adapter.rs`；最后再把 Binance crate 收敛到 `config` / `rest` / `ws` / `mapper` / `connected` 结构，并清掉 runtime / projector 里残留的 Binance 假设。当前计划只落 Binance 路径，不提前加入 Bybit / OKX / Hyperliquid 占位代码。

**Tech Stack:** Rust workspace, Cargo, Tokio, Axum, Reqwest, Tokio Tungstenite, Serde, anyhow

**Spec:** [`../specs/2026-04-08-single-service-exchange-boundary-design.md`](../specs/2026-04-08-single-service-exchange-boundary-design.md)

---

## File Structure

### 新增文件

- `server/src/exchange.rs`
  - `server` owner 的 `Exchange` 装配对象
- `exchanges/binance/src/config.rs`
  - Binance 配置与 deployment 定义
- `exchanges/binance/src/connected.rs`
  - Binance 已连接组件集合
- `exchanges/binance/src/mapper.rs`
  - Binance 原始模型到标准化模型的映射
- `exchanges/binance/src/rest/auth.rs`
  - Binance REST 签名与鉴权
- `exchanges/binance/src/rest/client.rs`
  - Binance REST client
- `exchanges/binance/src/rest/models.rs`
  - Binance REST 原始响应模型
- `exchanges/binance/src/ws/market.rs`
  - Binance 公有行情流
- `exchanges/binance/src/ws/account.rs`
  - Binance 私有账户流
- `exchanges/binance/src/ws/models.rs`
  - Binance WebSocket 原始消息模型

### 重点修改文件

- `engine/src/ports.rs`
  - 删除过宽 `ExchangePort`
  - 保留窄 `AccountSummaryPort`
  - 引入 `ExecutionPort`、`AccountPort`、`MetadataPort`
- `engine/src/track.rs`
  - 保留 `Venue` 作为身份枚举
- `server/src/config.rs`
  - 改成服务级 `exchange`
  - 删除 `track.venue`
- `server/src/assembly.rs`
  - 通过 `build_exchange(...)` 构造装配层 `Exchange`
- `server/src/runtime/mod.rs`
  - 改为只消费装配层提取出的最小 `Port` 和 `RuntimeState`
- `server/src/runtime/startup_sync.rs`
  - 改用拆分后的执行 / 账户 / 元数据能力
- `server/src/runtime/reconcile.rs`
  - 改用执行能力
- `server/src/runtime/guards.rs`
  - 单服务单交易所下删除按 `venue` 分桶的保护状态
- `server/src/effect_worker/mod.rs`
  - 改为只依赖执行能力
- `server/src/http.rs`
  - 账户摘要依赖从旧 `ExchangePort` 改到 `AccountSummaryPort`
- `server/src/websocket.rs`
  - 测试桩与依赖改到新 `Port`
- `server/src/projector.rs`
  - 删除 Binance 专属 `binance -> binance_futures` 映射
- `server/src/main.rs`
  - 模块声明、测试桩和装配入口同步调整
- `exchanges/binance/src/lib.rs`
  - 改导出 `Config`、`connect(...)` 和已连接组件

### 迁移后删除的旧文件

- `exchanges/binance/src/adapter.rs`
- `exchanges/binance/src/rest.rs`
- `exchanges/binance/src/websocket.rs`
- `exchanges/binance/src/types.rs`

### 实施约束

- 每个 task 先写失败测试，再写最小实现
- 每个 task 验收通过后必须立即提交，并把 commit SHA 回写到本计划
- 未完成 `git add`、`git commit` 和计划回写，不得开始下一个 task
- 当前计划只实现 Binance；不要提前引入其他交易所 crate 占位实现
- 不允许保留 `track.venue` 兼容层，也不允许保留共享 `environment -> endpoint` 映射
- 顶层 `environment` 不在本计划的 owner 范围；这里只验证交易所路径不消费它，不修改实例目录设计里的 `environment` 语义
- `Exchange` 只允许存在于 `server/src/exchange.rs`、`server/src/assembly.rs` 及装配时局部变量；不得进入 `RuntimeState`、`EffectWorkerState`、`HttpState`、`WebSocketState`
- 最终验收至少包含 `cargo test --workspace --quiet` 和 `cargo build -p poise-server`

---

### Task 1: 固定服务级 `exchange` 配置边界，删除 `track.venue`

**Files:**
- Create: `exchanges/binance/src/config.rs`
- Modify: `exchanges/binance/src/lib.rs`
- Modify: `server/src/config.rs`
- Modify: `server/src/assembly.rs`
- Test: `server/src/config.rs`
- Test: `server/src/assembly.rs`

- [x] **Step 1: 先写失败测试，固定新的配置形状**

在 `server/src/config.rs` 增加配置测试：

```rust
#[test]
fn parses_service_level_exchange_config_and_track_symbols() {
    let config = parse_config(
        r#"
environment = "testnet"

[exchange]
venue = "binance"
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
notional_per_unit = 3000.0
"#,
    )
    .unwrap();

    assert_eq!(config.exchange.venue(), Venue::Binance);
    assert_eq!(config.tracks[0].symbol, "BTCUSDT");
}
```

再增加旧配置拒绝测试：

```rust
#[test]
fn rejects_legacy_track_level_venue_field() {
    let error = parse_config(
        r#"
environment = "testnet"

[exchange]
venue = "binance"
deployment = "testnet"

[[tracks]]
track_id = "btc-core"
venue = "binance"
symbol = "BTCUSDT"
lower_price = 90.0
upper_price = 110.0
long_exposure_units = 8.0
short_exposure_units = 6.0
notional_per_unit = 3000.0
"#,
    )
    .unwrap_err();

    assert!(error.to_string().contains("venue"));
}
```

在 `server/src/assembly.rs` 增加测试，固定 `Instrument` 由服务级 `exchange` 和 `track.symbol` 组合得到：

```rust
#[test]
fn track_instrument_uses_service_exchange_venue() {
    let config = test_config_from_str(/* 同上新配置形状 */);
    let instrument = config.tracks[0].instrument(config.exchange.venue());

    assert_eq!(instrument.venue, Venue::Binance);
    assert_eq!(instrument.symbol, "BTCUSDT");
}
```

- [x] **Step 2: 运行定向测试，确认当前配置边界还没有切到服务级**

Run:
`cargo test -p poise-server parses_service_level_exchange_config_and_track_symbols -- --exact`

Expected:
- FAIL，原因是当前 `TrackDefinition` 仍要求 `venue`，`ExchangeConfig` 也还不是按 `venue` 标记的配置

Run:
`cargo test -p poise-server rejects_legacy_track_level_venue_field -- --exact`

Expected:
- FAIL，原因是当前配置仍接受 `track.venue`

Run:
`cargo test -p poise-server track_instrument_uses_service_exchange_venue -- --exact`

Expected:
- FAIL，原因是当前 `TrackDefinition::instrument()` 仍直接读取 `track.venue`

- [x] **Step 3: 实现最小配置边界**

要求：
- `server/src/config.rs` 改成服务级 `exchange`
- `TrackDefinition` 删除 `venue`
- `TrackDefinition::instrument(...)` 改为接收服务级 `Venue`
- `ExchangeConfig` 改为按 `venue` 标记的枚举，但当前只实现 `Binance(poise_binance::Config)` 一个分支
- `poise_binance::Config` 先承接 Binance 所需配置字段，包括 `deployment`
- `TrackDefinition`、`ExchangeConfig` 加 `deny_unknown_fields`，明确拒绝旧字段和脏配置
- `server/src/assembly.rs` 中的唯一性校验和 `TrackManager::add_track...` 改用服务级 `exchange.venue()`

- [x] **Step 4: 运行配置与装配回归**

Run:
`cargo test -p poise-server parses_service_level_exchange_config_and_track_symbols -- --exact`

Expected:
- PASS

Run:
`cargo test -p poise-server rejects_legacy_track_level_venue_field -- --exact`

Expected:
- PASS

Run:
`cargo test -p poise-server track_instrument_uses_service_exchange_venue -- --exact`

Expected:
- PASS

- [x] **Step 5: 提交并回写 SHA**

```bash
git add exchanges/binance/src/config.rs exchanges/binance/src/lib.rs server/src/config.rs server/src/assembly.rs
git commit -m "refactor: move exchange identity to service config"
```

Task 1 code commit:
`510a3a9`

---

### Task 2: 一次完成窄 `Port` 拆分和 `server` 装配层 `Exchange` 引入

**Files:**
- Create: `server/src/exchange.rs`
- Modify: `engine/src/ports.rs`
- Modify: `server/src/assembly.rs`
- Modify: `server/src/runtime/mod.rs`
- Modify: `server/src/runtime/startup_sync.rs`
- Modify: `server/src/runtime/reconcile.rs`
- Modify: `server/src/effect_worker/mod.rs`
- Modify: `server/src/http.rs`
- Modify: `server/src/websocket.rs`
- Modify: `server/src/main.rs`
- Modify: `server/src/server_context.rs`
- Modify: `server/src/runtime/tests/support.rs`
- Modify: `server/src/runtime/tests/user_data.rs`
- Modify: `server/src/effect_worker/tests/support.rs`
- Modify: `exchanges/binance/src/adapter.rs`
- Test: `server/src/assembly.rs`
- Test: `server/src/runtime/tests/user_data.rs`
- Test: `server/src/exchange.rs`
- Verify: `application/src/account_monitor.rs`

- [x] **Step 1: 先写失败测试，固定一次性装配边界**

在 `server/src/assembly.rs` 增加装配测试，明确运行时可以接收独立的执行 / 账户摘要 / 账户私有流 / 元数据 / 行情实现：

```rust
#[tokio::test]
async fn assemble_accepts_distinct_execution_account_summary_account_metadata_and_market_ports() {
    let execution = Arc::new(FakeExecutionPort::default());
    let account_summary = Arc::new(FakeAccountSummaryPort::default());
    let account = Arc::new(FakeAccountPort::default());
    let metadata = Arc::new(FakeMetadataPort::default());
    let market_data = Arc::new(FakeMarketDataPort::default());

    let platform = assemble_with_exchange_ports(
        &test_config(),
        execution,
        market_data,
        account_summary,
        account,
        metadata,
        test_repositories(),
        Arc::new(TestClock::default()),
    )
    .await
    .unwrap();

    assert!(platform.runtime.is_running());
}
```

在 `server/src/runtime/tests/user_data.rs` 增加测试，固定私有流 owner 留在 `AccountPort` 而不是 `MarketDataPort`：

```rust
#[tokio::test]
async fn runtime_subscribes_user_data_from_account_port() {
    let execution = Arc::new(FakeExecutionPort::default());
    let account_summary = Arc::new(FakeAccountSummaryPort::default());
    let account = Arc::new(FakeAccountPort::with_user_events(vec![sample_order_update()]));
    let metadata = Arc::new(FakeMetadataPort::default());
    let market_data = Arc::new(FakeMarketDataPort::default());

    let runtime = build_test_runtime_with_ports(
        execution,
        market_data,
        account_summary,
        account,
        metadata,
    )
    .await;

    let event = runtime.next_user_data_event().await.unwrap();

    assert!(matches!(event.payload, UserDataPayload::OrderUpdate(_)));
}
```

在 `server/src/exchange.rs` 增加测试，固定 `Exchange` 只属于装配层，且同时暴露窄读侧和账户能力：

```rust
#[test]
fn exchange_retains_venue_and_exposes_stable_ports() {
    let exchange = Exchange::new(
        Venue::Binance,
        Arc::new(FakeExecutionPort::default()),
        Arc::new(FakeMarketDataPort::default()),
        Arc::new(FakeAccountSummaryPort::default()),
        Arc::new(FakeAccountPort::default()),
        Arc::new(FakeMetadataPort::default()),
    );

    assert_eq!(exchange.venue(), Venue::Binance);
    let _execution: &dyn ExecutionPort = exchange.execution();
    let _market_data: &dyn MarketDataPort = exchange.market_data();
    let _account_summary: &dyn AccountSummaryPort = exchange.account_summary();
    let _account: &dyn AccountPort = exchange.account();
    let _metadata: &dyn MetadataPort = exchange.metadata();
}
```

- [x] **Step 2: 运行定向测试，确认当前边界还没有一次迁移到位**

Run:
`cargo test -p poise-server assemble_accepts_distinct_execution_account_summary_account_metadata_and_market_ports -- --exact`

Expected:
- FAIL，原因是当前 `assemble_with_components(...)` 仍依赖 `ExchangePort + MarketDataPort`

Run:
`cargo test -p poise-server runtime_subscribes_user_data_from_account_port -- --exact`

Expected:
- FAIL，原因是当前私有流仍挂在 `MarketDataPort`

Run:
`cargo test -p poise-server exchange_retains_venue_and_exposes_stable_ports -- --exact`

Expected:
- FAIL，原因是 `server/src/exchange.rs` 尚不存在

- [x] **Step 3: 实现一次性装配边界迁移**

要求：
- 保留 `AccountSummaryPort` 作为窄读侧接口
- 删除 `ExchangePort`
- 在 `engine/src/ports.rs` 中固定为：
  - `ExecutionPort`
  - `MarketDataPort`
  - `AccountSummaryPort`
  - `AccountPort`
  - `MetadataPort`
- `AccountPort` 只负责：
  - `get_account_capacity_snapshot(...)`
  - `subscribe_user_data(...)`
- `AccountSummaryPort` 继续只负责 `get_account_summary(...)`
- 新建 `server/src/exchange.rs`
- `Exchange` 只属于 `server` 装配层，并保存：
  - `venue`
  - `execution`
  - `market_data`
  - `account_summary`
  - `account`
  - `metadata`
- `server`、`runtime`、`effect_worker`、`http`、`websocket`、测试桩在这一 task 内一次改到位
- `server/src/assembly.rs` 必须在装配时把 `Exchange` 拆成最小 `Port` 和专用 state
- `runtime`、`effect_worker`、`http`、`websocket`、`server_context` 不得持有 `Exchange`
- `AccountMonitor` 和只读摘要路径继续依赖 `AccountSummaryPort`
- `BinanceAdapter` 在旧文件结构下先实现新接口，但不承担新的装配 owner

- [x] **Step 4: 运行装配边界回归**

Run:
`cargo test -p poise-server exchange_retains_venue_and_exposes_stable_ports -- --exact`

Expected:
- PASS

Run:
`cargo test -p poise-server assemble_accepts_distinct_execution_account_summary_account_metadata_and_market_ports -- --exact`

Expected:
- PASS

Run:
`cargo test -p poise-server runtime_subscribes_user_data_from_account_port -- --exact`

Expected:
- PASS

Run:
`cargo test -p poise-application account_monitor_can_be_built_from_summary_only_source -- --exact`

Expected:
- PASS，说明读侧仍可只依赖窄 `AccountSummaryPort`

Run:
`cargo test -p poise-binance --lib`

Expected:
- PASS，说明 Binance 现有 REST / WebSocket 行为在新接口下仍成立

- [x] **Step 5: 提交并回写 SHA**

```bash
git add server/src/exchange.rs engine/src/ports.rs server/src/assembly.rs server/src/runtime/mod.rs server/src/runtime/startup_sync.rs server/src/runtime/reconcile.rs server/src/effect_worker/mod.rs server/src/http.rs server/src/websocket.rs server/src/main.rs server/src/server_context.rs server/src/runtime/tests/support.rs server/src/runtime/tests/user_data.rs server/src/effect_worker/tests/support.rs exchanges/binance/src/adapter.rs
git commit -m "refactor: introduce exchange assembly and split stable ports"
```

Task 2 code commit:
`65ee751`

---

### Task 3: 把 Binance deployment / endpoint owner 下沉到 `poise-binance`，并切到 `connected` 唯一入口

**Files:**
- Create: `exchanges/binance/src/connected.rs`
- Modify: `exchanges/binance/src/config.rs`
- Modify: `exchanges/binance/src/lib.rs`
- Modify: `exchanges/binance/src/rest.rs`
- Modify: `exchanges/binance/src/websocket.rs`
- Modify: `server/src/assembly.rs`
- Delete: `exchanges/binance/src/adapter.rs`
- Test: `exchanges/binance/src/config.rs`
- Test: `exchanges/binance/src/connected.rs`
- Test: `server/src/assembly.rs`

- [x] **Step 1: 先写失败测试，固定 endpoint 选择不再经过 `server`**

在 `exchanges/binance/src/config.rs` 增加测试：

```rust
#[test]
fn deployment_resolves_mainnet_testnet_and_custom_endpoints() {
    assert_eq!(
        Deployment::Mainnet.endpoints(),
        Endpoints::new("https://fapi.binance.com", "wss://fstream.binance.com")
    );
    assert_eq!(
        Deployment::Testnet.endpoints(),
        Endpoints::new("https://demo-fapi.binance.com", "wss://fstream.binancefuture.com")
    );
    assert_eq!(
        Deployment::Custom {
            rest_base_url: "http://127.0.0.1:8080".to_string(),
            ws_base_url: "ws://127.0.0.1:9000".to_string(),
        }
        .endpoints(),
        Endpoints::new("http://127.0.0.1:8080", "ws://127.0.0.1:9000")
    );
}
```

在 `server/src/assembly.rs` 增加测试：

```rust
#[tokio::test]
async fn build_exchange_ignores_top_level_environment_for_binance_endpoint_selection() {
    let config = parse_config(/* environment = "test"; exchange.deployment = "mainnet" */).unwrap();

    let exchange = build_exchange(&config.exchange).await.unwrap();

    assert_eq!(exchange.venue(), Venue::Binance);
}
```

在 `exchanges/binance/src/connected.rs` 增加测试，固定 Binance crate 的唯一装配入口是 `connected.rs`：

```rust
#[test]
fn connected_exposes_all_required_ports() {
    let connected = build_test_connected();

    let _execution: Arc<dyn ExecutionPort> = connected.execution();
    let _market_data: Arc<dyn MarketDataPort> = connected.market_data();
    let _account_summary: Arc<dyn AccountSummaryPort> = connected.account_summary();
    let _account: Arc<dyn AccountPort> = connected.account();
    let _metadata: Arc<dyn MetadataPort> = connected.metadata();
}
```

这个测试的重点不是网络访问，而是固定 `build_exchange(...)` 的输入只来自 `exchange` 配置，Binance 自己的装配入口只来自 `connected.rs`，并且不再消费顶层 `environment`。

- [x] **Step 2: 运行定向测试，确认当前 Binance endpoint 仍在 `server`**

Run:
`cargo test -p poise-binance deployment_resolves_mainnet_testnet_and_custom_endpoints -- --exact`

Expected:
- FAIL，原因是当前 `poise-binance` 还没有 deployment / endpoint owner

Run:
`cargo test -p poise-server build_exchange_ignores_top_level_environment_for_binance_endpoint_selection -- --exact`

Expected:
- FAIL，原因是当前 `server/src/assembly.rs` 仍有 `resolve_binance_endpoints(...)`

Run:
`cargo test -p poise-binance connected_exposes_all_required_ports -- --exact`

Expected:
- FAIL，原因是当前 Binance crate 还没有 `connected.rs` 作为唯一装配入口

- [x] **Step 3: 实现最小 owner 下沉**

要求：
- `poise_binance::Config` 明确拥有 `deployment`、`api_key`、`api_secret`
- `Deployment` 至少包含：
  - `Mainnet`
  - `Testnet`
  - `Custom { rest_base_url, ws_base_url }`
- `poise_binance::connect(config)` 返回 Binance 已连接组件
- `connected.rs` 成为 Binance crate 的唯一装配入口
- `server/src/assembly.rs` 增加 `build_exchange(&ExchangeConfig) -> Result<Exchange>`
- 删除：
  - `ValidatedExchangeRuntimeConfig`
  - `required_exchange_field(...)`
  - `resolve_binance_endpoints(...)`
  - `resolve_binance_endpoints_with_lookup(...)`
- 删除 `exchanges/binance/src/adapter.rs`
- `build_exchange(...)` 不接收也不读取顶层 `environment`
- 不把新的 owner 逻辑临时落回 `adapter.rs`

- [x] **Step 4: 运行 owner 下沉回归**

Run:
`cargo test -p poise-binance deployment_resolves_mainnet_testnet_and_custom_endpoints -- --exact`

Expected:
- PASS

Run:
`cargo test -p poise-server build_exchange_ignores_top_level_environment_for_binance_endpoint_selection -- --exact`

Expected:
- PASS

Run:
`cargo test -p poise-binance connected_exposes_all_required_ports -- --exact`

Expected:
- PASS

Run:
`cargo test -p poise-server --quiet`

Expected:
- PASS，说明 `server` 装配已完全不持有 Binance endpoint 规则

- [x] **Step 5: 提交并回写 SHA**

```bash
git add exchanges/binance/src/config.rs exchanges/binance/src/connected.rs exchanges/binance/src/lib.rs exchanges/binance/src/rest.rs exchanges/binance/src/websocket.rs server/src/assembly.rs
git rm exchanges/binance/src/adapter.rs
git commit -m "refactor: move binance wiring into connected entrypoint"
```

Task 3 code commit:
`9717fac`

---

### Task 4: 收敛 `poise-binance` 模块结构，并清理残留的 Binance 假设

**Files:**
- Create: `exchanges/binance/src/mapper.rs`
- Create: `exchanges/binance/src/rest/auth.rs`
- Create: `exchanges/binance/src/rest/client.rs`
- Create: `exchanges/binance/src/rest/models.rs`
- Create: `exchanges/binance/src/ws/market.rs`
- Create: `exchanges/binance/src/ws/account.rs`
- Create: `exchanges/binance/src/ws/models.rs`
- Modify: `exchanges/binance/src/connected.rs`
- Modify: `exchanges/binance/src/lib.rs`
- Modify: `server/src/runtime/guards.rs`
- Modify: `server/src/projector.rs`
- Modify: `server/src/runtime/tests/support.rs`
- Modify: `server/src/runtime/tests/execution.rs`
- Modify: `server/src/runtime/tests/user_data.rs`
- Delete: `exchanges/binance/src/rest.rs`
- Delete: `exchanges/binance/src/websocket.rs`
- Delete: `exchanges/binance/src/types.rs`
- Test: `server/src/runtime/guards.rs`
- Test: `server/src/projector.rs`
- Test: `exchanges/binance/src/rest/client.rs`
- Test: `exchanges/binance/src/ws/account.rs`

- [ ] **Step 1: 先写失败测试，固定最终模块边界和残留清理**

在 `server/src/runtime/guards.rs` 增加测试：

```rust
#[test]
fn account_margin_guard_tracks_single_exchange_block_without_venue_bucket() {
    let store = AccountMarginGuardStore::default();
    let instrument = Instrument::new(Venue::Binance, "BTCUSDT");

    store.activate_insufficient_margin(&instrument, "insufficient margin", Utc::now());
    let constraint = store.constraint_for(&instrument);

    assert!(constraint.increase_blocked);
}
```

在 `server/src/projector.rs` 增加测试：

```rust
#[test]
fn project_instrument_preserves_exchange_name() {
    let view = project_instrument("binance", "BTCUSDT");

    assert_eq!(view.venue, "binance");
    assert_eq!(view.symbol, "BTCUSDT");
}
```

同时把 `exchanges/binance/src/rest.rs`、`websocket.rs`、`types.rs` 里的现有测试迁移目标写到新文件中，先移动测试壳子再移动实现，确保文件结构重组不是无测试搬家。

- [ ] **Step 2: 运行定向测试，确认当前仍有遗留假设**

Run:
`cargo test -p poise-server account_margin_guard_tracks_single_exchange_block_without_venue_bucket -- --exact`

Expected:
- FAIL，原因是当前 `AccountMarginGuardStore` 仍按 `venue` 分桶

Run:
`cargo test -p poise-server project_instrument_preserves_exchange_name -- --exact`

Expected:
- FAIL，原因是当前仍把 `"binance"` 映射成 `"binance_futures"`

- [ ] **Step 3: 实现最终结构整理**

要求：
- `poise-binance` 按 spec 收敛为：
  - `config`
  - `connected`
  - `mapper`
  - `rest/*`
  - `ws/*`
- 原 `rest.rs` / `websocket.rs` / `types.rs` 的测试一起迁移，不允许无测试搬迁
- `server/src/runtime/guards.rs` 删除 `blocks_by_venue`
- `AccountMarginGuardStore` 改成单服务单交易所下的单账户保护状态
- `server/src/projector.rs` 删除 `binance -> binance_futures` 特判
- 除了上面两点清理，不改对外 HTTP / WebSocket 协议语义

- [ ] **Step 4: 运行最终回归**

Run:
`cargo test -p poise-server account_margin_guard_tracks_single_exchange_block_without_venue_bucket -- --exact`

Expected:
- PASS

Run:
`cargo test -p poise-server project_instrument_preserves_exchange_name -- --exact`

Expected:
- PASS

Run:
`cargo test -p poise-binance --lib`

Expected:
- PASS，说明 Binance crate 结构迁移后行为不变

Run:
`cargo test --workspace --quiet`

Expected:
- PASS

Run:
`cargo build -p poise-server`

Expected:
- PASS

- [ ] **Step 5: 提交并回写 SHA**

```bash
git add exchanges/binance/src exchanges/binance/Cargo.toml server/src/runtime/guards.rs server/src/projector.rs server/src/runtime/tests/support.rs server/src/runtime/tests/execution.rs server/src/runtime/tests/user_data.rs
git commit -m "refactor: align binance integration with exchange boundary"
```

Task 4 code commit:
`<fill-me>`
