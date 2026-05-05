# Exchange Design Simplification Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 降低 4 个交易所接入后的样板代码、venue 分支和维护成本，同时保留交易所私有协议差异，不为了抽象而抽象。

**Architecture:** 保留现有 `ExecutionPort` / `MarketDataPort` / `AccountPort` / `AccountSummaryPort` / `MetadataPort` 这组有效边界；`ExchangePorts` 只作为 `connect()` 的短生命周期连接结果，删除重复的本地 `Connected` 容器，但不作为 runtime 领域对象继续向下传递。REST/WS 请求签名、字段映射、账户模式等交易所细节继续留在各 exchange crate 内；启动资金探测保留为 server 启动策略，不进入 engine ports。

**Tech Stack:** Rust workspace, async_trait ports, Tokio mpsc streams, cargo test per crate.

---

## 设计约束

- 不引入通用 `ExchangeAdapter` 框架，不把 REST/WS 协议细节搬到共享层。
- 共享层只拥有“系统怎么消费交易所能力”的知识，不拥有“某个交易所 HTTP/WS 怎么请求”的知识。
- 任何新增抽象必须删除已有重复或隐藏真实差异：主要目标是 `Connected` 容器重复、server 按 venue 判断启动资金模型。
- 不引入 `Connected` trait。connector 返回具体连接结果类型，动态分发只发生在具体 port trait 上。
- `ExchangePorts` 只停留在连接和 assembly 边界；runtime、read model、domain 不持有它。
- 每个任务完成后必须运行任务内测试、提交 commit，并把 commit SHA 回写到本文件。

## 文件职责

- `engine/src/ports.rs`：定义系统消费交易所能力的 port traits；`ExchangePorts` 只负责承载 connector 返回的现有 ports。
- `server/src/exchange.rs`：过渡包装；Task 3 删除，避免把 `ExchangePorts` 变成长生命周期对象。
- `exchanges/*/src/connected.rs`：只负责把交易所私有 REST/WS client 组装成 `ExchangePorts`；逐步删除重复的 `Connected` 容器和纯透传 wrapper。
- `server/src/assembly.rs`：只按配置选择连接哪个交易所；拿到 `ExchangePorts` 后立刻拆给启动准备、account monitor 和 runtime。
- `server/src/runtime/startup_bootstrap.rs`：启动时使用 server 侧启动资金探测策略，不把该策略建模为交易所通用 port。
- `docs/system-overview.md`：记录新的交易所接入边界和新增交易所时的最小步骤。

---

### Task 1: 新增 ExchangePorts bundle，先不改变 exchange crates API

**Files:**
- Modify: `engine/src/ports.rs`
- Modify: `server/src/exchange.rs`
- Test: `server/src/exchange.rs`

- [x] **Step 1: 写失败测试**

在 `server/src/exchange.rs` 的测试里新增断言：`Exchange::new` 接收一个 `ExchangePorts`，并且 `execution_port()` / `market_data_port()` / `account_summary_port()` / `account_port()` / `metadata_port()` 都来自同一个 bundle。

预期新增测试名：

```rust
#[test]
fn exchange_wraps_exchange_ports_bundle() {
    // 构造 ExchangePorts::new(...)
    // 构造 Exchange::new(Venue::Binance, ports.clone())
    // assert_eq!(exchange.venue(), Venue::Binance)
    // assert Arc::ptr_eq(...) 或至少能取出每个 port
}
```

- [x] **Step 2: 验证失败**

Run:

```bash
cargo test -p poise-server exchange::tests::exchange_wraps_exchange_ports_bundle
```

Expected: FAIL，原因是 `ExchangePorts` 或新的 `Exchange::new` 签名不存在。

- [x] **Step 3: 最小实现**

在 `engine/src/ports.rs` 增加：

```rust
#[derive(Clone)]
pub struct ExchangePorts {
    execution: Arc<dyn ExecutionPort>,
    market_data: Arc<dyn MarketDataPort>,
    account_summary: Arc<dyn AccountSummaryPort>,
    account: Arc<dyn AccountPort>,
    metadata: Arc<dyn MetadataPort>,
}
```

并提供 `new(...)` 和 clone getter。`server/src/exchange.rs` 改为持有 `ports: ExchangePorts`，保留现有 `execution_port()` 等方法，避免 runtime 大面积改动。

- [x] **Step 4: 验证通过**

Run:

```bash
cargo test -p poise-server exchange::tests::
cargo check -p poise-server
```

- [x] **Step 5: 提交并回写 SHA**

```bash
git add engine/src/ports.rs server/src/exchange.rs docs/superpowers/plans/2026-05-05-exchange-design-simplification.md
git commit -m "refactor: add exchange ports bundle"
```

Commit: `56449f39658f5fa7d1e9a5fb81eae830f31c556a`

---

### Task 2: 让四个 exchange connect 直接返回 ExchangePorts

**Files:**
- Modify: `exchanges/binance/src/connected.rs`
- Modify: `exchanges/bybit/src/connected.rs`
- Modify: `exchanges/hyperliquid/src/connected.rs`
- Modify: `exchanges/okx/src/connected.rs`
- Modify: `exchanges/*/src/lib.rs`
- Modify: `server/src/assembly.rs`
- Test: each exchange `connected::tests::`
- Test: `server/src/assembly.rs`

- [x] **Step 1: 写失败测试**

把每个 exchange 的 `connected_exposes_all_required_ports` 测试改成直接接收 `ExchangePorts`：

```rust
let ports = connect(&config).await.unwrap();
let _execution = ports.execution();
let _market_data = ports.market_data();
let _account_summary = ports.account_summary();
let _account = ports.account();
let _metadata = ports.metadata();
```

`server/src/assembly.rs` 增加或更新测试，要求 `build_exchange` 不再调用 `connected.execution()` 这类 getter，而是把 `connect_*` 返回的 bundle 传给 `Exchange::new`。

- [x] **Step 2: 验证失败**

Run:

```bash
cargo test -p poise-binance connected::tests::connected_exposes_all_required_ports
cargo test -p poise-bybit connected::tests::connected_exposes_all_required_ports
cargo test -p poise-hyperliquid connected::tests::connected_exposes_all_required_ports
cargo test -p poise-okx connected::tests::connected_exposes_all_required_ports
```

Expected: FAIL，原因是 `connect()` 仍返回交易所本地 `Connected`。

- [x] **Step 3: 最小实现**

每个 `connect(config)` 改为返回 `Result<ExchangePorts>`。删除本地 `Connected` struct、`from_parts` 和五个 getter。保留目前需要的 port wrapper structs。

`server/src/assembly.rs` 从：

```rust
let connected = connect_binance(binance_config).await?;
Exchange::new(Venue::Binance, connected.execution(), ...)
```

改为：

```rust
let ports = connect_binance(binance_config).await?;
Exchange::new(Venue::Binance, ports)
```

- [x] **Step 4: 验证通过**

Run:

```bash
cargo test -p poise-binance connected::tests::
cargo test -p poise-bybit connected::tests::
cargo test -p poise-hyperliquid connected::tests::
cargo test -p poise-okx connected::tests::
cargo test -p poise-server assembly::tests::
cargo check -p poise-server
```

- [x] **Step 5: 提交并回写 SHA**

```bash
git add exchanges server docs/superpowers/plans/2026-05-05-exchange-design-simplification.md
git commit -m "refactor: return exchange ports from connectors"
```

Commit: `87c8ef978525db7c2c6887fcaad2fad2210dd50a`

---

### Task 3: 让 ExchangePorts 只停留在 assembly 层

**Files:**
- Modify: `server/src/assembly.rs`
- Modify: `server/src/main.rs`
- Modify: `engine/src/ports.rs`
- Delete: `server/src/exchange.rs`
- Test: `server/src/assembly.rs`

- [x] **Step 1: 写失败测试**

更新 `server/src/assembly.rs` 的 build exchange 测试：`build_exchange()` 返回 `(Venue, ExchangePorts)`，测试直接断言第一个元素是对应 venue，并能从第二个元素取出所需 ports。`assemble_with_exchange_ports()` 不再构造 `server::Exchange`。

移除 `server/src/exchange.rs` 的测试入口；该文件不再作为 server 层抽象存在。

- [x] **Step 2: 验证失败**

Run:

```bash
cargo test -p poise-server assembly::tests::build_exchange_uses_exchange_deployment_for_binance_endpoint_selection
```

Expected: FAIL 或 compile error，原因是 `build_exchange()` 仍返回 `Exchange`。

- [x] **Step 3: 最小实现**

`build_exchange()` 改为返回 `Result<(Venue, ExchangePorts)>`。`build_exchange_and_prepare_startup()` 和 `assemble_with_state_store()` 传递 `Venue` 与 `ExchangePorts`，但不再保留 `Exchange` wrapper。

在 `assemble_with_state_store()` 内只在需要时从 `ExchangePorts` clone 具体 port：

- `metadata` 用于启动加载 exchange info。
- `account_summary` 用于 account monitor。
- 五个 port 拆给 `RuntimePorts`。

删除 `server/src/exchange.rs` 和 `mod exchange;`。同时删除 `ExchangePorts` 的 `*_ref` getter，避免为了已删除的 wrapper 暴露双套访问方式。

- [x] **Step 4: 验证通过**

Run:

```bash
cargo test -p poise-server assembly::tests::
cargo check -p poise-server
cargo fmt --check
git diff --check
```

- [x] **Step 5: 提交并回写 SHA**

```bash
git add engine server docs/superpowers/plans/2026-05-05-exchange-design-simplification.md
git commit -m "refactor: keep exchange ports at assembly boundary"
```

Commit: `e5920302398cfe14a1c7c5e757aa91d05ed86cc4`

---

### Task 4: 删除纯透传 port wrappers，保留有转换价值的 wrappers

**Files:**
- Modify: `exchanges/binance/src/connected.rs`
- Modify: `exchanges/bybit/src/connected.rs`
- Modify: `exchanges/hyperliquid/src/connected.rs`
- Modify: `exchanges/okx/src/connected.rs`
- Modify as needed: `exchanges/*/src/rest/client.rs`, `exchanges/*/src/ws/mod.rs`
- Test: each exchange `connected::tests::`

- [x] **Step 1: 补 characterization 验收**

不写“wrapper 不存在”的脆弱结构性断言。使用现有 connected tests 保证 `connect()` 仍能构造全部 ports；实现阶段只删除确认纯透传、没有错误语义或组合职责的 wrapper。

期望保留的 wrappers：

- `Execution`：如果需要错误语义转换、symbol 转换或下单特殊处理。
- `Account`：如果需要组合 REST + WS。

期望删除的 wrappers：

- 只调用 `ws.subscribe_prices(instrument)` 的 `MarketData`。
- 只调用 `rest.get_account_summary()` 的 `AccountSummary`。
- 只调用 `rest.get_exchange_info(&instrument.symbol)` / `rest.get_server_time()` 的 `Metadata`，除非该交易所有特殊实现。

- [x] **Step 2: 验证重构基线**

Run:

```bash
cargo test -p poise-binance connected::tests::
```

Expected: PASS，作为删除 wrapper 前的行为基线。

- [x] **Step 3: 最小实现**

为 REST/WS client 直接实现对应 trait。例如：

```rust
#[async_trait]
impl MarketDataPort for BinanceWsClient {
    async fn subscribe_prices(&self, instrument: &Instrument) -> Result<mpsc::Receiver<MarketDataTick>> {
        self.subscribe_prices(instrument).await
    }
}
```

删除纯透传 wrapper struct 及其 `new`。不要把所有 wrapper 一次性强行删除；如果 wrapper 有错误转换或组合职责，保留。

- [x] **Step 4: 验证通过**

Run:

```bash
cargo test -p poise-binance connected::tests::
cargo test -p poise-bybit connected::tests::
cargo test -p poise-hyperliquid connected::tests::
cargo test -p poise-okx connected::tests::
RUSTFLAGS="-Dwarnings" cargo check -p poise-server
```

- [x] **Step 5: 提交并回写 SHA**

```bash
git add exchanges docs/superpowers/plans/2026-05-05-exchange-design-simplification.md
git commit -m "refactor: remove pass-through exchange port wrappers"
```

Commit: `01f36eb5d45b4011e03758e596929a849ce35b82`

---

### Task 5: 把启动资金探测改成 server 私有策略，删除 runtime mode 分支

**Files:**
- Modify: `server/src/runtime/mod.rs`
- Modify: `server/src/runtime/startup_bootstrap.rs`
- Modify: `server/src/assembly.rs`
- Modify as needed: `server/src/exchange_startup.rs`
- Test: `server/src/runtime/startup_bootstrap.rs`
- Test: `server/src/assembly.rs`

- [ ] **Step 1: 写失败测试**

在 `server/src/assembly.rs` 修改启动资金测试：不再断言 `RuntimeStartupCapacityMode`，改为验证 startup bootstrap 通过 server 私有 `StartupCapacityProbe` 得到 capacity。

在 `server/src/runtime/startup_bootstrap.rs` 增加测试：

```rust
#[tokio::test]
async fn startup_capacity_is_probed_through_runtime_strategy_with_track_leverage() {
    // FakeStartupCapacityProbe 记录 instrument 和 startup_leverage
    // bootstrap 后断言收到 BTCUSDT + 配置 leverage
}
```

- [ ] **Step 2: 验证失败**

Run:

```bash
cargo test -p poise-server assembly::tests::startup_capacity
cargo test -p poise-server startup_bootstrap::tests::startup_capacity_is_probed_through_runtime_strategy_with_track_leverage
```

Expected: FAIL，原因是当前 runtime 仍使用 `RuntimeStartupCapacityMode`。

- [ ] **Step 3: 最小实现**

新增 server 私有启动策略，放在 `server/src/runtime` 或 `server/src/exchange_startup.rs`，不要放进 `engine/src/ports.rs`：

```rust
#[async_trait]
pub(crate) trait StartupCapacityProbe: Send + Sync {
    async fn probe_startup_capacity(
        &self,
        instrument: &Instrument,
        startup_leverage: u32,
    ) -> Result<AccountCapacitySnapshot>;
}
```

删除 `RuntimeStartupCapacityMode` 和 `RuntimeStartupDefinition.capacity_mode`，`startup_bootstrap` 统一调用 runtime 持有的 `StartupCapacityProbe`。

实现策略：

- Binance：调用现有 `AccountPort::get_account_capacity_snapshot(instrument)`。
- Bybit / Hyperliquid / OKX：调用 `AccountSummaryPort::get_account_summary()`，返回 `available * startup_leverage`。

venue 到策略的选择只保留在 server 组装层或 `exchange_startup` helper 中，不进入 exchange crates，不进入 engine ports。

- [ ] **Step 4: 验证通过**

Run:

```bash
cargo test -p poise-server startup_bootstrap::tests::
cargo test -p poise-server assembly::tests::
RUSTFLAGS="-Dwarnings" cargo check -p poise-server
```

- [ ] **Step 5: 提交并回写 SHA**

```bash
git add server docs/superpowers/plans/2026-05-05-exchange-design-simplification.md
git commit -m "refactor: move startup capacity probing to runtime strategy"
```

Commit: _pending_

---

### Task 6: 更新交易所接入文档

**Files:**
- Modify: `docs/system-overview.md`
- Optional Modify: `README.md`

- [ ] **Step 1: 写文档验收清单**

在 `docs/system-overview.md` 增加“新增交易所接入步骤”：

- 实现 REST/WS 私有 client。
- 将 client 组装为 `ExchangePorts`。
- 只在需要转换语义时添加 wrapper。
- 在 server 组装层选择启动资金探测策略。
- 补 connected / mapper / startup capacity 最小测试。

- [ ] **Step 2: 最小验证**

Run:

```bash
rg -n "ExchangePorts|StartupCapacityProbe|新增交易所" docs README.md
git diff --check
```

- [ ] **Step 3: 提交并回写 SHA**

```bash
git add docs/system-overview.md README.md docs/superpowers/plans/2026-05-05-exchange-design-simplification.md
git commit -m "docs: document exchange integration boundaries"
```

Commit: _pending_

---

## 最终验收

Run:

```bash
cargo fmt --check
git diff --check
RUSTFLAGS="-Dwarnings" cargo check -p poise-server
cargo test -p poise-server assembly::tests::
cargo test -p poise-server startup_bootstrap::tests::
cargo test -p poise-binance connected::tests::
cargo test -p poise-bybit connected::tests::
cargo test -p poise-hyperliquid connected::tests::
cargo test -p poise-okx connected::tests::
```

如果最终验收发现跨 crate 影响，再补跑对应 crate 的局部测试；不要默认跑 workspace 全量测试。
