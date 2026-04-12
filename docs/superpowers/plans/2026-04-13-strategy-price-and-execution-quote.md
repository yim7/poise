# Strategy Price And Execution Quote Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把当前单一 `reference_price` 语义拆成 `strategy_price + mark_price + execution_quote`，让策略目标围绕盘口中间价计算、执行围绕盘口一档定价、风控围绕标记价判断，并在价格异常时通过单一 price gate 统一控制自动执行和人工减风险例外。

**Architecture:** 先把市场数据入口统一升级成 `mark_price + execution_quote`，并让 engine 持有 `strategy_price` 与 `strategy_price_status`。随后在 engine 内引入唯一的 `PriceExecutionGate` owner，同时让 executor 和 effect worker 显式消费 `SubmitPurpose`，避免“自动执行”和“人工减风险”在不同调用路径里各自补条件。最后再统一 read model、protocol、projector、TUI 和持久化命名，彻底移除 `reference_price` 的旧语义。

**Tech Stack:** Rust workspace, Tokio, Cargo tests, SQLite, Markdown

---

## Files And Responsibilities

- Create: `engine/src/price_gate.rs`
  单点拥有 `PriceExecutionGate`、`PriceExecutionBlockReason`、`SubmitPurpose`、`mark-book divergence` 常量，以及 submit 权限矩阵、自动改价权限和 working order 处置规则。
- Modify: `engine/src/ports.rs`
  定义共享 `ExecutionQuote` 和新的 `PriceTick`。
- Modify: `engine/src/observation.rs`
  定义新的 `MarketObservation` 输入结构。
- Modify: `application/src/track_observation_service.rs`
  把 `observe_market(...)` 从 `f64` 升级成 `MarketObservation`。
- Modify: `application/src/mutation_executor.rs`
  把 observation、submit recovery 和 effect preparation 边界切到新的价格模型与 `SubmitPurpose`。
- Modify: `server/src/runtime/market_data.rs`
  市场任务把完整 `PriceTick` 映射成新的 `MarketObservation`。
- Modify: `exchanges/binance/src/ws/mod.rs`
  订阅 Binance 的 mark + book 组合市场流。
- Modify: `exchanges/binance/src/ws/market.rs`
  合并 mark / book 消息，产出完整 `PriceTick`。
- Modify: `exchanges/binance/src/ws/models.rs`
  定义 Binance mark / book stream payload 模型。
- Modify: `exchanges/bybit/src/ws/market.rs`
  从 Bybit ticker 流中同时提取 `markPrice / bid1Price / ask1Price`。
- Modify: `exchanges/bybit/src/ws/models.rs`
  扩展 Bybit ticker payload 模型。
- Modify: `engine/src/runtime.rs`
  把 runtime 观测字段从 `reference_price` 改成 `strategy_price / strategy_price_status / mark_price / best_bid / best_ask / price_execution_gate`。
- Modify: `engine/src/manager.rs`
  统一市场观测、策略价更新、price gate 更新、自动执行抑制和人工减风险例外。
- Modify: `engine/src/reconciler.rs`
  把 `desired_exposure(...)` 和 `band_status(...)` 切到 `strategy_price`。
- Modify: `engine/src/execution_plan.rs`
  给 `ExecutionAction::SubmitOrder` 增加 `submit_purpose`。
- Modify: `engine/src/executor/planning.rs`
  用 `best_ask / best_bid` 定价，并显式消费 `SubmitPurpose`。
- Modify: `engine/src/executor/slots.rs`
  在 gate 关闭时按统一规则处理已存在的 working order，撤加风险单、保减风险单。
- Modify: `engine/src/executor/recovery.rs`
  让 submit recovery 在 gate 关闭时尊重 `SubmitPurpose`。
- Modify: `engine/src/executor/mod.rs`
  补 executor 级回归测试，锁住 bid/ask 定价和 gate 行为。
- Modify: `engine/src/snapshot.rs`
  扩展 runtime snapshot 的 observed state。
- Modify: `engine/src/persisted_runtime.rs`
  对齐新的 snapshot 编解码。
- Modify: `storage/src/schema.rs`
  重建 `track_snapshots` 价格列，删除旧 `reference_price` 列。
- Modify: `storage/src/sqlite.rs`
  迁移 `track_snapshots` 数据并读写新字段。
- Modify: `application/src/track_read_source.rs`
  把 runtime 观测态映射成 read source。
- Modify: `application/src/read_model.rs`
  定义 `strategy_price / strategy_price_status / mark_price / best_bid / best_ask / price gate reason`。
- Modify: `protocol/src/lib.rs`
  对外协议改成 `strategy_price`，新增 `strategy_price_status / best_bid / best_ask`，删除 `index_price`。
- Modify: `server/src/projector.rs`
  投影新的价格字段、stale 语义和 price gate attention reason。
- Modify: `server/src/effect_worker/dispatch.rs`
  分发时保留 `submit_purpose`。
- Modify: `server/src/effect_worker/execute.rs`
  只消费 `prepare_submit_execution(...)` 的结果，不直接解释价格 gate 或 pending submit 生命周期。
- Modify: `tui/src/views/instance.rs`
  展示 `strategy_price / strategy_price_status / mark_price / best_bid / best_ask`，移除 `index`。
- Modify: `tui/tests/fixtures/track_detail_view.json`
- Modify: `tui/tests/fixtures/track_list_response.json`
- Modify: `tui/tests/fixtures/ws_track_detail_changed.json`
- Modify: `tui/tests/fixtures/ws_track_list_item_changed.json`
  更新 fixtures 里的价格字段。
- Modify: `README.md`
  更新用户可见价格语义。
- Modify: `docs/superpowers/specs/2026-04-13-mark-and-book-price-separation-design.md`
  若实现中的最小边界和字段名有细化，回写 spec。
- Modify: `docs/superpowers/plans/2026-04-13-strategy-price-and-execution-quote.md`
  执行时勾选任务并记录 commit SHA。

### Task 1: 升级共享价格模型，并打通市场数据入口

**Files:**
- Modify: `engine/src/ports.rs`
- Modify: `engine/src/observation.rs`
- Modify: `application/src/track_observation_service.rs`
- Modify: `application/src/mutation_executor.rs`
- Modify: `server/src/runtime/market_data.rs`
- Modify: `exchanges/binance/src/ws/mod.rs`
- Modify: `exchanges/binance/src/ws/market.rs`
- Modify: `exchanges/binance/src/ws/models.rs`
- Modify: `exchanges/bybit/src/ws/market.rs`
- Modify: `exchanges/bybit/src/ws/models.rs`
- Test: `exchanges/binance/src/ws/market.rs`
- Test: `exchanges/bybit/src/ws/market.rs`
- Test: `application/src/track_observation_service.rs`

- [x] **Step 1: 先写失败测试，锁住新的 `PriceTick` 和 `MarketObservation` 入口**

在 `exchanges/binance/src/ws/market.rs`、`exchanges/bybit/src/ws/market.rs`、`application/src/track_observation_service.rs` 增加至少这些测试：

```rust
#[test]
fn parses_binance_mark_and_book_into_price_tick() {}

#[test]
fn ignores_binance_book_update_until_bid_and_ask_are_both_present() {}

#[test]
fn parses_bybit_ticker_mark_and_top_of_book_into_price_tick() {}

#[test]
fn emits_bybit_tick_with_none_quote_when_bid_or_ask_is_missing() {}

#[tokio::test]
async fn observation_service_persists_market_observation_with_mark_and_quote() {}
```

覆盖点：

- Binance tick 必须同时支持 `mark_price` 和 `execution_quote`
- Bybit tick 必须同时支持 `markPrice / bid1Price / ask1Price`
- `MarketObservation` 不再是 `reference_price: f64`
- `TrackObservationService.observe_market(...)` 改为接收完整 `MarketObservation`

- [x] **Step 2: 运行定向测试，确认当前实现失败**

Run:

- `cargo test -p poise-binance ws::market::tests::parses_binance_mark_and_book_into_price_tick -- --exact`
- `cargo test -p poise-bybit ws::market::tests::parses_bybit_ticker_mark_and_top_of_book_into_price_tick -- --exact`
- `cargo test -p poise-application track_observation_service::tests::observation_service_persists_market_observation_with_mark_and_quote -- --exact`

Expected:

- 当前实现失败，因为 `PriceTick` 还没有 `ExecutionQuote`
- 当前 `MarketObservation` 仍只有 `reference_price`
- observation service 仍然只接受 `f64`

- [x] **Step 3: 做最小实现，统一 `PriceTick` / `MarketObservation` 结构**

先在 `engine/src/ports.rs` 和 `engine/src/observation.rs` 引入共享价格模型：

```rust
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ExecutionQuote {
    pub best_bid: f64,
    pub best_ask: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PriceTick {
    pub instrument: Instrument,
    pub mark_price: f64,
    pub execution_quote: Option<ExecutionQuote>,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MarketObservation {
    pub mark_price: f64,
    pub execution_quote: Option<ExecutionQuote>,
}
```

然后把 `application/src/track_observation_service.rs`、`application/src/mutation_executor.rs`、`server/src/runtime/market_data.rs` 改成直接传 `MarketObservation`，例如：

```rust
pub async fn observe_market(
    &self,
    id: &str,
    observation: MarketObservation,
) -> Result<TrackTransition> {
    self.executor.observe_market(id, observation).await
}
```

`server/src/runtime/market_data.rs` 要改成：

```rust
.observe_market(
    &track.id,
    poise_engine::observation::MarketObservation {
        mark_price: tick.mark_price,
        execution_quote: tick.execution_quote,
    },
)
```

- [x] **Step 4: 实现 Binance / Bybit 适配层的完整价格输出**

Binance：

- `exchanges/binance/src/ws/mod.rs` 把单一 `@markPrice` 订阅改成 mark + book 组合流
- `exchanges/binance/src/ws/models.rs` 增加 `BookTickerMessage`
- `exchanges/binance/src/ws/market.rs` 增加组合状态，直到有有效 bid/ask 才填充 `ExecutionQuote`

最小结构：

```rust
struct BinanceMarketState {
    last_mark_price: Option<f64>,
    last_quote: Option<ExecutionQuote>,
}
```

Bybit：

- `exchanges/bybit/src/ws/models.rs` 扩展：

```rust
pub(crate) struct PublicTickerData {
    pub mark_price: Option<f64>,
    pub bid1_price: Option<f64>,
    pub ask1_price: Option<f64>,
}
```

- `exchanges/bybit/src/ws/market.rs` 在 ticker state 里同时缓存 `mark_price` 和 `ExecutionQuote`

- [x] **Step 5: 跑 Task 1 回归**

Run:

- `cargo test -p poise-binance ws::market::tests:: -- --nocapture`
- `cargo test -p poise-bybit ws::market::tests:: -- --nocapture`
- `cargo test -p poise-application track_observation_service::tests::observation_service_persists_market_observation_with_mark_and_quote -- --exact --nocapture`

Expected:

- 两个交易所都能产出 `mark_price + execution_quote`
- observation service 已切到新 `MarketObservation`
- 当前还没有引入 `strategy_price` 和 gate，但市场数据入口已经统一

- [ ] **Step 6: Commit**

```bash
git add engine/src/ports.rs engine/src/observation.rs application/src/track_observation_service.rs application/src/mutation_executor.rs server/src/runtime/market_data.rs exchanges/binance/src/ws/mod.rs exchanges/binance/src/ws/market.rs exchanges/binance/src/ws/models.rs exchanges/bybit/src/ws/market.rs exchanges/bybit/src/ws/models.rs
git commit -m "feat(market): carry mark price and execution quote through observation pipeline"
```

### Task 2: 把 runtime / snapshot / storage / read model 改成 `strategy_price` 语义

**Files:**
- Modify: `engine/src/runtime.rs`
- Modify: `engine/src/snapshot.rs`
- Modify: `engine/src/persisted_runtime.rs`
- Modify: `storage/src/schema.rs`
- Modify: `storage/src/sqlite.rs`
- Modify: `application/src/track_read_source.rs`
- Modify: `application/src/read_model.rs`
- Test: `engine/src/snapshot.rs`
- Test: `storage/src/sqlite.rs`
- Test: `application/src/read_model.rs`

- [ ] **Step 1: 先写失败测试，锁住 observed state 和持久化字段**

增加至少这些测试：

```rust
#[test]
fn snapshot_round_trips_strategy_price_mark_price_and_quote() {}

#[test]
fn sqlite_migrates_legacy_reference_price_into_stale_price_state() {}

#[test]
fn read_model_exposes_strategy_price_status_and_best_bid_ask() {}
```

覆盖点：

- runtime snapshot 不再使用 `observed.reference_price`
- storage 会持久化 `strategy_price / strategy_price_status / mark_price / best_bid / best_ask`
- read model 不再暴露 `reference_price`

- [ ] **Step 2: 运行定向测试，确认当前实现失败**

Run:

- `cargo test -p poise-engine snapshot::tests::snapshot_round_trips_strategy_price_mark_price_and_quote -- --exact`
- `cargo test -p poise-storage sqlite::tests::sqlite_migrates_legacy_reference_price_into_stale_price_state -- --exact`
- `cargo test -p poise-application read_model::tests::read_model_exposes_strategy_price_status_and_best_bid_ask -- --exact`

Expected:

- 当前 snapshot / sqlite / read model 仍只有 `reference_price`

- [ ] **Step 3: 做最小实现，替换 runtime 观测字段**

在 `engine/src/runtime.rs`、`engine/src/snapshot.rs` 中把 `reference_price` 改成下面这组字段。`TrackRuntime` 至少要新增或替换：

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StrategyPriceStatus {
    Live,
    Stale,
}

pub(crate) strategy_price: Option<f64>,
pub(crate) strategy_price_status: StrategyPriceStatus,
pub(crate) mark_price: Option<f64>,
pub(crate) best_bid: Option<f64>,
pub(crate) best_ask: Option<f64>,
```

`ObservedState` 对齐成：

```rust
pub struct ObservedState {
    pub strategy_price: Option<f64>,
    pub strategy_price_status: StrategyPriceStatus,
    pub mark_price: Option<f64>,
    pub best_bid: Option<f64>,
    pub best_ask: Option<f64>,
    pub out_of_band_since: Option<DateTime<Utc>>,
    pub last_tick_at: Option<DateTime<Utc>>,
    pub market_data_stale_since: Option<DateTime<Utc>>,
}
```

- [ ] **Step 4: 重建 `track_snapshots` 价格列，并迁移旧数据**

在 `storage/src/schema.rs` 和 `storage/src/sqlite.rs` 中重建 `track_snapshots` 表，删除旧 `reference_price` 列，引入新列：

```sql
strategy_price REAL,
strategy_price_status TEXT NOT NULL,
mark_price REAL,
best_bid REAL,
best_ask REAL
```

迁移时不要伪造新语义里的 live 价格字段，而是明确写成 stale 空值：

```sql
strategy_price = NULL
strategy_price_status = 'stale'
mark_price = NULL
best_bid = NULL
best_ask = NULL
```

要求：

- 迁移后 schema 里不再保留 `reference_price`
- 历史 snapshot 不伪装成 live `strategy_price` 或 live `mark_price`
- SQLite 读写层与 `PersistedRuntimeCodec` 都只认新字段

- [ ] **Step 5: 更新 read source / read model 命名**

`application/src/track_read_source.rs` 和 `application/src/read_model.rs` 至少改成。`TrackRuntimeReadState` 需要包含：

```rust
pub strategy_price: Option<f64>,
pub strategy_price_status: StrategyPriceStatus,
pub mark_price: Option<f64>,
pub best_bid: Option<f64>,
pub best_ask: Option<f64>,
```

并删除：

```rust
pub reference_price: Option<f64>,
```

- [ ] **Step 6: 跑 Task 2 回归**

Run:

- `cargo test -p poise-engine snapshot::tests:: -- --nocapture`
- `cargo test -p poise-storage sqlite::tests:: -- --nocapture`
- `cargo test -p poise-application read_model::tests:: -- --nocapture`

Expected:

- runtime / snapshot / storage / read model 已全部切到 `strategy_price`
- 历史 `reference_price` 已迁移成显式 `stale/null` 价格状态
- 旧 `reference_price` 已从这些层删除

- [ ] **Step 7: Commit**

```bash
git add engine/src/runtime.rs engine/src/snapshot.rs engine/src/persisted_runtime.rs storage/src/schema.rs storage/src/sqlite.rs application/src/track_read_source.rs application/src/read_model.rs
git commit -m "refactor(runtime): replace reference price with strategy and market fields"
```

### Task 3: 在 engine 内实现 `strategy_price` 推导、stale 语义和 `PriceExecutionGate`

**Files:**
- Create: `engine/src/price_gate.rs`
- Modify: `engine/src/manager.rs`
- Modify: `engine/src/reconciler.rs`
- Modify: `engine/src/runtime.rs`
- Modify: `engine/src/observation.rs`
- Test: `engine/src/price_gate.rs`
- Test: `engine/src/manager.rs`

- [ ] **Step 1: 先写失败测试，锁住策略价推导和 price gate**

增加至少这些测试：

```rust
#[test]
fn price_gate_returns_no_submit_when_quote_is_missing() {}

#[test]
fn price_gate_returns_manual_risk_reduction_only_when_mark_book_diverges() {}

#[test]
fn price_gate_reopens_after_divergence_recovers() {}

#[test]
fn observe_market_derives_strategy_price_from_book_mid() {}

#[test]
fn observe_market_keeps_last_strategy_price_and_marks_stale_when_quote_disappears() {}

#[test]
fn reconcile_target_uses_strategy_price_instead_of_mark_price() {}

#[test]
fn reconcile_target_keeps_existing_desired_exposure_when_strategy_price_is_stale() {}

#[test]
fn observe_market_recomputes_desired_exposure_after_quote_recovers() {}
```

覆盖点：

- `strategy_price = (best_bid + best_ask) / 2`
- 缺少 quote 时，`strategy_price` 保持 last-known 且 `strategy_price_status = Stale`
- 缺少 quote 时，不会把 stale `strategy_price` 重新写回新的 `desired_exposure`
- `mark-book divergence` 用固定常量驱动 gate
- gate 必须区分进入阈值和恢复阈值，不能退化成单阈值
- `desired_exposure(...)` 和 `band_status(...)` 已切到 `strategy_price`
- 报价恢复后，manager 会重新计算新的 `desired_exposure`，而不是只把 gate 打开

- [ ] **Step 2: 运行定向测试，确认当前实现失败**

Run:

- `cargo test -p poise-engine price_gate::tests::price_gate_returns_no_submit_when_quote_is_missing -- --exact`
- `cargo test -p poise-engine price_gate::tests::price_gate_reopens_after_divergence_recovers -- --exact`
- `cargo test -p poise-engine manager::tests::observe_market_derives_strategy_price_from_book_mid -- --exact`
- `cargo test -p poise-engine manager::tests::observe_market_keeps_last_strategy_price_and_marks_stale_when_quote_disappears -- --exact`
- `cargo test -p poise-engine manager::tests::observe_market_recomputes_desired_exposure_after_quote_recovers -- --exact`
- `cargo test -p poise-engine manager::tests::reconcile_target_uses_strategy_price_instead_of_mark_price -- --exact`

Expected:

- 当前 engine 还没有 `PriceExecutionGate`
- 当前 reconcile 仍围绕旧 `reference_price`

- [ ] **Step 3: 做最小实现，新增单一 price gate owner**

在 `engine/src/price_gate.rs` 定义：

```rust
pub const MAX_MARK_BOOK_DIVERGENCE_BPS: u32 = 300;
pub const RECOVER_MARK_BOOK_DIVERGENCE_BPS: u32 = 150;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SubmitPurpose {
    AutoReconcile,
    ManualRiskReduction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PriceExecutionBlockReason {
    MissingExecutionQuote,
    MarkBookDivergence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PriceExecutionGate {
    Open,
    ManualRiskReductionOnly { reason: PriceExecutionBlockReason },
    NoSubmit { reason: PriceExecutionBlockReason },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkingOrderGateAction {
    Keep,
    Cancel,
}
```

并提供唯一入口：

```rust
pub fn evaluate_price_execution_gate(
    previous: PriceExecutionGate,
    mark_price: Option<f64>,
    quote: Option<ExecutionQuote>,
) -> PriceExecutionGate

pub fn allows_submit(gate: PriceExecutionGate, purpose: SubmitPurpose) -> bool

pub fn allows_auto_replace(gate: PriceExecutionGate) -> bool

pub fn working_order_gate_action(
    gate: PriceExecutionGate,
    role: OrderRole,
) -> WorkingOrderGateAction
```

- [ ] **Step 4: 在 manager 中统一更新 `strategy_price` 和 gate**

`engine/src/manager.rs` 的 `observe_market(...)` 要改成：

```rust
let strategy_price = observation
    .execution_quote
    .map(|quote| (quote.best_bid + quote.best_ask) / 2.0);

track.mark_price = Some(observation.mark_price);
track.best_bid = observation.execution_quote.map(|quote| quote.best_bid);
track.best_ask = observation.execution_quote.map(|quote| quote.best_ask);

if let Some(strategy_price) = strategy_price {
    track.strategy_price = Some(strategy_price);
    track.strategy_price_status = StrategyPriceStatus::Live;
} else {
    track.strategy_price_status = StrategyPriceStatus::Stale;
}

track.price_execution_gate = evaluate_price_execution_gate(
    track.price_execution_gate,
    track.mark_price,
    observation.execution_quote,
);
```

并在 `engine/src/reconciler.rs` 把：

```rust
strategy::band_status(reference_price, &track.config)
strategy::desired_exposure(reference_price, &track.config)
```

全部切成 `strategy_price`。

另外要补一条显式规则：

- `strategy_price_status = Stale` 时，不再重算新的 `band_status / desired_exposure`
- 此时保留当前 `desired_exposure`，直到有效盘口恢复后再重新 reconcile
- 当新的有效 quote 恢复后，`observe_market(...)` 必须在同一条 manager 路径里重新计算 `band_status / desired_exposure`，恢复普通自动执行入口

- [ ] **Step 5: 跑 Task 3 回归**

Run:

- `cargo test -p poise-engine price_gate::tests:: -- --nocapture`
- `cargo test -p poise-engine manager::tests::observe_market_ -- --nocapture`
- `cargo test -p poise-engine manager::tests::reconcile_target_uses_strategy_price_instead_of_mark_price -- --exact --nocapture`
- `cargo test -p poise-engine manager::tests::reconcile_target_keeps_existing_desired_exposure_when_strategy_price_is_stale -- --exact --nocapture`
- `cargo test -p poise-engine manager::tests::observe_market_recomputes_desired_exposure_after_quote_recovers -- --exact --nocapture`

Expected:

- gate 只在 `engine/src/price_gate.rs` 有唯一 owner
- `strategy_price` 的推导和 stale 语义已经落在 manager
- gate 的恢复逻辑已经锁住进入阈值 / 恢复阈值分离
- stale `strategy_price` 不会被重新解释成新的执行目标
- 报价恢复后会重新计算 `desired_exposure`，而不是保留旧目标到下一次偶然 reconcile
- reconcile 已不再使用 `mark_price`

- [ ] **Step 6: Commit**

```bash
git add engine/src/price_gate.rs engine/src/manager.rs engine/src/reconciler.rs engine/src/runtime.rs engine/src/observation.rs
git commit -m "feat(engine): derive strategy price from book mid and gate price execution"
```

### Task 4: 切换执行定价，并让 `SubmitPurpose` 贯穿 submit / recovery / effect worker

**Files:**
- Modify: `engine/src/execution_plan.rs`
- Modify: `engine/src/executor/planning.rs`
- Modify: `engine/src/executor/slots.rs`
- Modify: `engine/src/executor/recovery.rs`
- Modify: `engine/src/executor/mod.rs`
- Modify: `engine/src/manager.rs`
- Modify: `application/src/mutation_executor.rs`
- Modify: `application/src/track_effect_service.rs`
- Modify: `server/src/effect_worker/dispatch.rs`
- Modify: `server/src/effect_worker/execute.rs`
- Test: `engine/src/executor/mod.rs`
- Test: `server/src/effect_worker/tests/execute.rs`
- Test: `server/src/runtime/tests/execution.rs`

- [ ] **Step 1: 先写失败测试，锁住 bid/ask 定价和 gate 权限矩阵**

增加至少这些测试：

```rust
#[test]
fn buy_order_uses_best_ask() {}

#[test]
fn sell_order_uses_best_bid() {}

#[test]
fn auto_reconcile_submit_is_blocked_when_gate_is_no_submit() {}

#[test]
fn manual_risk_reduction_submit_is_allowed_when_gate_is_manual_risk_reduction_only() {}

#[test]
fn gate_cancels_existing_increase_inventory_working_order() {}

#[test]
fn gate_keeps_existing_decrease_inventory_working_order() {}

#[test]
fn gate_closed_stops_automatic_replacement_for_kept_working_order() {}

#[test]
fn gate_blocked_pending_submit_is_superseded_by_recovery() {}

#[tokio::test]
async fn effect_worker_does_not_dispatch_pending_auto_submit_when_price_gate_is_closed() {}
```

覆盖点：

- planner 不再从 `strategy_price` 直接定价
- `ExecutionAction::SubmitOrder` 必须显式带 `submit_purpose`
- 已存在的 working order 也会按 gate 统一处理
- `gate != Open` 时不会继续自动改价或自动 replacement
- 被 gate 挡住的 pending submit 由 recovery 单点判定为 supersede，并等待恢复后的新 reconcile
- effect worker 不再自己判断 gate 下的 pending submit 生命周期

- [ ] **Step 2: 运行定向测试，确认当前实现失败**

Run:

- `cargo test -p poise-engine executor::tests::buy_order_uses_best_ask -- --exact`
- `cargo test -p poise-engine executor::tests::manual_risk_reduction_submit_is_allowed_when_gate_is_manual_risk_reduction_only -- --exact`
- `cargo test -p poise-engine executor::tests::gate_cancels_existing_increase_inventory_working_order -- --exact`
- `cargo test -p poise-engine executor::tests::gate_closed_stops_automatic_replacement_for_kept_working_order -- --exact`
- `cargo test -p poise-engine executor::tests::gate_blocked_pending_submit_is_superseded_by_recovery -- --exact`
- `cargo test -p poise-server effect_worker::tests::execute::effect_worker_does_not_dispatch_pending_auto_submit_when_price_gate_is_closed -- --exact`

Expected:

- 当前 `SubmitOrder` 还没有 `submit_purpose`
- 当前 planner 仍用单一价格字段定价
- 当前自动 replacement 还不会受价格 gate 控制
- 当前 recovery 还没有统一拥有 gate-blocked pending submit 的 lifecycle

- [ ] **Step 3: 做最小实现，给 submit action 加 `submit_purpose`**

在 `engine/src/execution_plan.rs` 把 submit action 改成：

```rust
pub enum ExecutionAction {
    SubmitOrder {
        request: OrderRequest,
        desired_exposure: Exposure,
        submit_purpose: SubmitPurpose,
    },
}
```

要求：

- 普通 reconcile 一律写 `SubmitPurpose::AutoReconcile`
- 手动 `Flatten` / `Terminate` 走 `SubmitPurpose::ManualRiskReduction`

- [ ] **Step 4: 切换 executor 定价和 recovery / dispatch gate**

`engine/src/executor/planning.rs` 至少改成：

```rust
let price = match side {
    Side::Buy => round_to_step(input.execution_quote.best_ask, input.exchange_rules.price_tick),
    Side::Sell => round_to_step(input.execution_quote.best_bid, input.exchange_rules.price_tick),
};
```

同时：

- `SubmitIntentInput` 不再接收单一 `reference_price`
- 改成接收 `execution_quote` 和 `submit_purpose`
- `engine/src/executor/planning.rs` 的 replacement / cancel-replace 路径只调用 `price_gate::allows_auto_replace(...)`
  - `gate == Open ->` 允许按现有 replacement 规则继续
  - `gate != Open ->` 不生成自动 replacement effect，即使 working order 被判定为 `Keep`
- `engine/src/executor/slots.rs` 和相关 planning 路径只调用 `price_gate::working_order_gate_action(...)`
  - `IncreaseInventory + gate != Open -> Cancel`
  - `DecreaseInventory + gate != Open -> Keep`
- `engine/src/executor/recovery.rs` 在 gate 关闭时：
  - 不再手写权限矩阵，只调用 `price_gate::allows_submit(...)`
  - 被 gate 挡住的 pending submit 统一返回 `SubmitRecoveryResolution::Superseded`
  - 后续依赖恢复后的新 reconcile 重新生成 submit effect
- `application/src/mutation_executor.rs` / `application/src/track_effect_service.rs` 保持当前边界：
  - `prepare_submit_execution(...)` 只消费 recovery resolution
  - `Superseded` 时直接更新 effect 状态，不把 gate 语义再往 server 层传播
- `server/src/effect_worker/execute.rs` 不直接查询 gate
  - 它只调用 `prepare_submit_execution(...)`
  - 返回 `None` 时直接停止本次执行
  - pending submit 的 supersede 语义全部由 recovery 侧完成

- [ ] **Step 5: 跑 Task 4 回归**

Run:

- `cargo test -p poise-engine executor::tests:: -- --nocapture`
- `cargo test -p poise-server runtime::tests::execution -- --nocapture`
- `cargo test -p poise-server effect_worker::tests::execute -- --nocapture`

Expected:

- 买单走 `best_ask`，卖单走 `best_bid`
- 自动 submit 在 `NoSubmit / ManualRiskReductionOnly` 下都不会继续发出
- `gate != Open` 时不会继续自动改价
- 已存在的加风险单会被撤掉，减风险单会被保留
- 被 gate 挡住的 pending submit 会被 supersede，等待恢复后的新 reconcile
- 手动 `Flatten` / `Terminate` 只在 `ManualRiskReductionOnly` 下继续发减风险单

- [ ] **Step 6: Commit**

```bash
git add engine/src/execution_plan.rs engine/src/executor/planning.rs engine/src/executor/slots.rs engine/src/executor/recovery.rs engine/src/executor/mod.rs engine/src/manager.rs application/src/mutation_executor.rs application/src/track_effect_service.rs server/src/effect_worker/dispatch.rs server/src/effect_worker/execute.rs
git commit -m "feat(execution): route submit purpose through price gate and top-of-book pricing"
```

### Task 5: 更新 protocol / projector / TUI / README，并完成全链路回归

**Files:**
- Modify: `protocol/src/lib.rs`
- Modify: `server/src/projector.rs`
- Modify: `tui/src/views/instance.rs`
- Modify: `tui/tests/fixtures/track_detail_view.json`
- Modify: `tui/tests/fixtures/track_list_response.json`
- Modify: `tui/tests/fixtures/ws_track_detail_changed.json`
- Modify: `tui/tests/fixtures/ws_track_list_item_changed.json`
- Modify: `README.md`
- Modify: `docs/superpowers/specs/2026-04-13-mark-and-book-price-separation-design.md`
- Test: `protocol/src/lib.rs`
- Test: `server/src/projector.rs`
- Test: `tui/src/views/instance.rs`

- [ ] **Step 1: 先写失败测试，锁住对外字段和 TUI 展示**

增加至少这些测试：

```rust
#[test]
fn detail_view_serializes_strategy_price_and_quote_fields() {}

#[test]
fn projector_maps_price_gate_to_attention_required_reason() {}

#[test]
fn projector_marks_strategy_price_status_stale_when_quote_is_missing() {}

#[test]
fn renders_market_block_with_strategy_mark_and_top_of_book_prices() {}
```

覆盖点：

- `reference_price` 从 protocol 删除
- `index_price` 从 protocol 删除
- detail / list 改成 `strategy_price`
- detail 展示 `best_bid / best_ask`
- stale 策略价和 price gate reason 对外可见

- [ ] **Step 2: 运行定向测试，确认当前实现失败**

Run:

- `cargo test -p poise-protocol detail_view_serializes_strategy_price_and_quote_fields -- --exact`
- `cargo test -p poise-server projector::tests::projector_maps_price_gate_to_attention_required_reason -- --exact`
- `cargo test -p poise-tui views::instance::tests::renders_market_block_with_strategy_mark_and_top_of_book_prices -- --exact`

Expected:

- 当前协议和 projector 仍然输出 `reference_price` / `index_price`
- TUI 仍然展示 `ref / mark / index`

- [ ] **Step 3: 做最小实现，统一对外命名和展示**

`protocol/src/lib.rs` 至少改成：

```rust
pub struct TrackListItemView {
    pub strategy_price: Option<f64>,
    pub strategy_price_status: StrategyPriceStatusView,
}

pub struct TrackStatusPanelView {
    pub lifecycle: TrackLifecycleView,
    pub strategy_price: Option<f64>,
    pub strategy_price_status: StrategyPriceStatusView,
}

pub struct TrackMarketView {
    pub mark_price: Option<f64>,
    pub best_bid: Option<f64>,
    pub best_ask: Option<f64>,
}
```

`server/src/projector.rs` 负责：

- `status.strategy_price <- source.strategy_price`
- `status.strategy_price_status <- source.strategy_price_status`
- `market.mark_price <- source.mark_price`
- `market.best_bid / best_ask <- source.best_bid / best_ask`
- `execution.attention_reasons` 增加价格 gate reason

`tui/src/views/instance.rs` 要把：

```rust
"prices: ref {} | mark {} | index {}"
```

改成：

```rust
"prices: strategy {} ({}) | mark {} | bid {} | ask {}"
```

- [ ] **Step 4: 更新 README 和 spec**

README 至少明确：

- `strategy_price = book_mid`
- `mark_price` 只用于风控与保护
- `Buy -> best_ask`
- `Sell -> best_bid`
- 缺少盘口或价格偏离过大时，自动执行会进入 `attention_required`

spec 只回写实现中真正落地的字段名和 gate 常量 owner，不再保留实现前表述。

- [ ] **Step 5: 跑最终回归**

Run:

- `cargo test -p poise-engine`
- `cargo test -p poise-application`
- `cargo test -p poise-storage`
- `cargo test -p poise-protocol`
- `cargo test -p poise-binance`
- `cargo test -p poise-bybit`
- `cargo test -p poise-server`
- `cargo test -p poise-tui`

Expected:

- 全 workspace 相关 crate 通过
- `reference_price` 不再出现在核心运行时、协议和 TUI
- `strategy_price / mark_price / best_bid / best_ask / strategy_price_status` 全链路打通

- [ ] **Step 6: Commit**

```bash
git add protocol/src/lib.rs server/src/projector.rs tui/src/views/instance.rs tui/tests/fixtures/track_detail_view.json tui/tests/fixtures/track_list_response.json tui/tests/fixtures/ws_track_detail_changed.json tui/tests/fixtures/ws_track_list_item_changed.json README.md docs/superpowers/specs/2026-04-13-mark-and-book-price-separation-design.md docs/superpowers/plans/2026-04-13-strategy-price-and-execution-quote.md
git commit -m "feat(ui): expose strategy price and execution quote semantics"
```
