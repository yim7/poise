# Live Quote Persistence Decoupling Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 保留 `bookTicker` 作为执行层实时输入，同时停止“每个盘口 tick 都持久化 snapshot 并广播 `TrackChanged`”，并让 UI 通过单独的低频 live 刷新看到较新的市场信息。

**Architecture:** `engine/application` 内部把状态拆成三层：live quote、live strategy target、durable execution state / snapshot。现有顶层 `desired_exposure` 被重新限定为 track 级 canonical durable target；raw live target 跟 tick 走，单独放到 live state。现有 `executor_state.active_round.desired_exposure` 继续保留，但只作为 executor-local round anchor；`Start/Switch` 时从顶层 `desired_exposure` 同步，`Continue` 时允许与顶层 `desired_exposure` 暂时分离。`observe_market(...)` 更新 live state 后，只在顶层 `desired_exposure` 或其他 durable 后果变化时返回 `Durable(...)`；否则返回 `LiveOnly`。application query 层把 durable snapshot 与 `TrackLiveView` 拼成现有 HTTP / durable websocket 读模型，而 server websocket 另外增加一条按连接、按 `track_id` 合并的 `TrackLiveViewChanged` 低频刷新路径。

**Tech Stack:** Rust workspace, Tokio, Cargo tests, chrono, serde, Markdown

---

## Files And Responsibilities

- Modify: `engine/src/runtime.rs`
  拆出 `LiveQuoteState`、`QuoteHealthView`、`StrategyTargetView`、`TrackLiveView`，并把现有顶层 `desired_exposure` 收窄成 track 级 canonical durable target。
- Modify: `engine/src/snapshot.rs`
  从 durable snapshot 删除 `strategy_price`、`strategy_price_status`、`mark_price`、`best_bid`、`best_ask`、`last_tick_at`、raw `desired_exposure`、`price_execution_block_reason`，并显式保留现有顶层 `desired_exposure` 作为 durable target。
- Modify: `engine/src/manager.rs`
  让 `observe_market(...)` 返回 `MarketMutationOutcome`，并把 durable 边界收窄到顶层 `desired_exposure` / effects / 其他 durable 后果。
- Modify: `engine/src/executor/recovery.rs`
  把 recovery 输入改成明确区分：无 active round 时读顶层 `desired_exposure`，有 active round 时读 `active_round.desired_exposure`。
- Modify: `engine/src/executor/round_policy.rs`
  固定顶层 `desired_exposure` 与 `active_round.desired_exposure` 的同步规则，避免两个 durable 语义重新混在一起。
- Modify: `application/src/mutation_executor.rs`
  只对 `Durable(...)` 走 `commit_track_mutation(...)`，并暴露 `TrackLiveView` / `QuoteHealthView` / `StrategyTargetView` 查询。
- Modify: `application/src/track_observation_service.rs`
  暴露上述 live 查询给 query/runtime。
- Modify: `application/src/track_read_source.rs`
  把 runtime read state 改成由 durable snapshot + `TrackLiveView` 组合构造。
- Modify: `application/src/query_service.rs`
  在 `load_track_detail_source(...)` 里拼接 durable snapshot、recent events/effects 与 `TrackLiveView`。
- Modify: `application/src/read_model.rs`
  维持现有 `TrackReadModel` 对外字段，但改成读取 query-time live 视图而不是 snapshot 原始字段。
- Modify: `protocol/src/lib.rs`
  新增窄事件 `StreamEvent::TrackLiveViewChanged` 与对应的 `TrackLiveView` 协议结构。
- Modify: `server/src/websocket.rs`
  新增 live dirty 合并、`250ms` 低频 flush、`TrackLiveViewChanged` 推送与诊断统计。
- Modify: `server/src/server_context.rs`
  挂入 websocket live refresh 需要的 query 依赖或 test-only support。
- Modify: `server/src/assembly.rs`
  组装 websocket 所需的 live query 依赖。
- Modify: `server/src/projector.rs`
  只保留 durable 读模型投影职责，不复制 live quote 规则。
- Modify: `server/src/runtime/startup_sync.rs`
  明确启动后在第一条 tick 前没有有效 live quote，但 recovery / startup 仍使用恢复出来的顶层 `desired_exposure`。
- Modify: `server/src/runtime/tests/startup_sync.rs`
  锁住“启动后无 live quote，但仍有 durable `desired_exposure`；首个 tick 后恢复 live view”的行为。
- Modify: `server/src/runtime/tests/execution.rs`
  锁住执行规划读取 live quote，而不是 persisted snapshot 旧值。
- Modify: `server/src/runtime/market_data.rs`
  market tick 成功写入后标记 websocket live dirty。
- Modify: `tui/src/main.rs`
  处理 `TrackLiveViewChanged`，把 market/strategy 相关字段合并到当前 UI 状态。
- Modify: `tui/src/app.rs`
  增加应用 live market patch 的入口。
- Modify: `docs/superpowers/specs/2026-04-15-live-quote-persistence-decoupling-design.md`
  执行完成后同步最终接口名与 owner。
- Modify: `docs/superpowers/plans/2026-04-15-live-quote-persistence-decoupling.md`
  执行时勾选步骤，并在每个 task 验收通过后回写 commit SHA。

### Task 1: 先收窄顶层 `desired_exposure` owner，再拆 live/durable 边界

**Files:**
- Modify: `engine/src/runtime.rs`
- Modify: `engine/src/snapshot.rs`
- Modify: `engine/src/manager.rs`
- Modify: `engine/src/executor/recovery.rs`
- Modify: `engine/src/executor/round_policy.rs`
- Modify: `server/src/runtime/startup_sync.rs`
- Test: `engine/src/runtime.rs`
- Test: `engine/src/snapshot.rs`
- Test: `engine/src/manager.rs`
- Test: `server/src/runtime/tests/startup_sync.rs`
- Test: `server/src/runtime/tests/execution.rs`

- [x] **Step 1: 先写失败测试，锁住顶层 `desired_exposure` 与 live target 的新 owner**

新增至少这些测试：

```rust
#[test]
fn persisted_snapshot_preserves_durable_desired_exposure_but_omits_raw_live_target() {}

#[test]
fn restore_from_snapshot_restores_durable_desired_exposure_but_not_live_quote_or_live_target() {}

#[test]
fn quote_health_view_returns_missing_quote_baseline_without_tick() {}

#[test]
fn market_data_health_deadline_uses_live_last_tick_only() {}

#[tokio::test]
async fn startup_uses_restored_desired_exposure_before_first_tick() {}

#[test]
fn active_round_anchor_syncs_from_durable_desired_exposure_on_start_and_switch() {}

#[test]
fn active_round_anchor_may_differ_from_durable_desired_exposure_during_continue() {}
```

覆盖点：

- snapshot 不再 round-trip quote-derived 与 raw live target 字段
- restore 后 live quote / live target 为空，但顶层 durable `desired_exposure` 仍存在
- `QuoteHealthView` 在无 tick 时自然返回缺失 quote 基线
- deadline 只依赖 live `last_tick_at`
- startup / recovery 不等待下一条 tick 才拿到稳定执行目标
- `active_round.desired_exposure` 的同步规则被锁住，不再作为第二个通用 durable owner

- [x] **Step 2: 运行定向测试，确认当前实现失败**

Run:

- `cargo test -p poise-engine persisted_snapshot_preserves_durable_desired_exposure_but_omits_raw_live_target -- --exact`
- `cargo test -p poise-engine restore_from_snapshot_restores_durable_desired_exposure_but_not_live_quote_or_live_target -- --exact`
- `cargo test -p poise-engine quote_health_view_returns_missing_quote_baseline_without_tick -- --exact`
- `cargo test -p poise-engine market_data_health_deadline_uses_live_last_tick_only -- --exact`
- `cargo test -p poise-server startup_uses_restored_desired_exposure_before_first_tick -- --exact`
- `cargo test -p poise-engine active_round_anchor_syncs_from_durable_desired_exposure_on_start_and_switch -- --exact`
- `cargo test -p poise-engine active_round_anchor_may_differ_from_durable_desired_exposure_during_continue -- --exact`

Expected:

- snapshot 还在混用 persisted `desired_exposure` 与 raw target 语义
- startup / recovery 还没有明确依赖顶层 durable `desired_exposure`
- `active_round.desired_exposure` 和 track 级稳定目标的同步规则还没有被明确定义

- [x] **Step 3: 先做最小 owner 改造，再拆字段**

在 `engine/src/runtime.rs` 增加：

```rust
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LiveQuoteState {
    pub strategy_price: Option<f64>,
    pub mark_price: Option<f64>,
    pub execution_quote: Option<ExecutionQuote>,
    pub last_tick_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct StrategyTargetView {
    pub desired_exposure: Option<Exposure>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct QuoteHealthView {
    pub strategy_price_status: StrategyPriceStatus,
    pub price_execution_gate: PriceExecutionGate,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct TrackLiveView {
    pub strategy_price: Option<f64>,
    pub strategy_price_status: StrategyPriceStatus,
    pub mark_price: Option<f64>,
    pub best_bid: Option<f64>,
    pub best_ask: Option<f64>,
    pub desired_exposure: Option<f64>,
    pub price_execution_block_reason: Option<PriceExecutionBlockReason>,
}
```

并让 `TrackRuntime` 拥有：

```rust
pub(crate) live_quote: LiveQuoteState,
pub(crate) live_target: StrategyTargetView,
// 继续复用现有顶层 desired_exposure 作为 canonical durable target
```

同时把 `engine/src/snapshot.rs` 的 `ObservedState` 和 `TrackRuntimeSnapshot` 改成 durable-only：

```rust
pub struct ObservedState {
    pub out_of_band_since: Option<DateTime<Utc>>,
    pub market_data_stale_since: Option<DateTime<Utc>>,
}

pub struct TrackRuntimeSnapshot {
    // ...
    pub current_exposure: Exposure,
    pub desired_exposure: Option<Exposure>,
    // 不再持有 price_execution_block_reason
    pub observed: ObservedState,
}
```

Task 1 实际实现说明：

- 当前先保留了 `ObservedState` / `TrackRuntimeSnapshot` 的兼容字段形状，避免一次性扩散到所有 query / test support。
- 但 runtime `snapshot()` 现在统一把 quote-derived 字段写成 `None` / `Stale`，序列化时也会省略这些值。
- `restore_from_snapshot(...)` 不再恢复这些字段，新的 durable 语义已经生效。
- 彻底删除结构字段留到后续 query / live view 迁移完成后再做。

- [x] **Step 4: 更新 restore / snapshot / recovery / startup 语义**

在 `engine/src/runtime.rs` 实现：

```rust
impl TrackRuntime {
    pub fn quote_health_view(&self) -> QuoteHealthView { ... }
    pub fn strategy_target_view(&self) -> StrategyTargetView { ... }
    pub fn live_view(&self) -> TrackLiveView { ... }
}
```

`restore_from_snapshot(...)` 改成：

```rust
self.live_quote = LiveQuoteState::default();
self.live_target = StrategyTargetView::default();
self.desired_exposure = snapshot.desired_exposure.clone();
```

要求：

- 不从 snapshot 恢复 quote-derived 状态
- 不把 raw live target 恢复进 runtime
- recovery / round policy / startup 改成按固定规则读取：
  有 active round 时读 `active_round.desired_exposure`；
  无 active round 时读顶层 `desired_exposure`
- `Start/Switch` 时把 `active_round.desired_exposure` 从顶层 `desired_exposure` 同步进去

- [x] **Step 5: 让 `observe_market(...)` 与 `market_data_health_deadline(...)` 先改为读写 live state**

在 `engine/src/manager.rs` 至少完成：

```rust
track.live_quote.last_tick_at = Some(now);
track.live_quote.mark_price = Some(observation.mark_price);
track.live_quote.execution_quote = observation.execution_quote;
track.live_quote.strategy_price = observation
    .execution_quote
    .map(|quote| (quote.best_bid + quote.best_ask) / 2.0);
```

并让 `market_data_health_deadline(...)` 只读取 `track.live_quote.last_tick_at`。

- [x] **Step 6: 跑 Task 1 回归**

Run:

- `cargo test -p poise-engine persisted_snapshot_preserves_durable_desired_exposure_but_omits_raw_live_target -- --exact --nocapture`
- `cargo test -p poise-engine restore_from_snapshot_restores_durable_desired_exposure_but_not_live_quote_or_live_target -- --exact --nocapture`
- `cargo test -p poise-engine quote_health_view_returns_missing_quote_baseline_without_tick -- --exact --nocapture`
- `cargo test -p poise-engine market_data_health_deadline_uses_live_last_tick_only -- --exact --nocapture`
- `cargo test -p poise-server startup_uses_restored_desired_exposure_before_first_tick -- --exact --nocapture`

Expected:

- snapshot / restore 已经分清 live target 与 durable `desired_exposure`
- 启动时 live quote 为空，但顶层 `desired_exposure` 可用于 recovery / startup

实际通过的验证命令：

- `cargo fmt --all`
- `cargo test -p poise-engine runtime::tests::margin_guard_snapshot_round_trips_executor_state -- --exact --nocapture`
- `cargo test -p poise-engine manager::tests::sync_exchange_state_preserves_submit_pending_slot_without_live_orders_when_pending_effect_exists -- --exact --nocapture`
- `cargo test -p poise-server runtime::tests::startup_sync:: -- --nocapture`
- `cargo test -p poise-server runtime::tests::execution::effect_worker_leaves_submitting_working_order_when_receipt_persistence_fails -- --exact --nocapture`
- `cargo test -p poise-server runtime::tests::execution:: -- --nocapture`
- `cargo test -p poise-engine -- --nocapture`
- `cargo test -p poise-application -- --nocapture`

- [x] **Step 7: Commit**

```bash
git add engine/src/runtime.rs engine/src/snapshot.rs engine/src/manager.rs engine/src/executor/recovery.rs engine/src/executor/round_policy.rs server/src/runtime/startup_sync.rs docs/superpowers/plans/2026-04-15-live-quote-persistence-decoupling.md
git commit -m "refactor(engine): narrow durable desired exposure owner"
```

对应 commit SHA：

- `5667e5b` `refactor(engine): narrow durable desired exposure owner`

### Task 2: 把 durable 边界下沉到“顶层 `desired_exposure` 变化”

**Files:**
- Modify: `engine/src/manager.rs`
- Modify: `application/src/mutation_executor.rs`
- Modify: `application/src/track_observation_service.rs`
- Test: `engine/src/manager.rs`
- Test: `application/src/mutation_executor.rs`

- [ ] **Step 1: 先写失败测试，锁住 raw target 不再直接触发 durable**

新增至少这些测试：

```rust
#[test]
fn observe_market_returns_live_only_when_raw_target_moves_but_execution_intent_is_unchanged() {}

#[test]
fn observe_market_returns_durable_when_planned_effects_change() {}

#[tokio::test]
async fn observe_market_live_only_tick_does_not_emit_track_changed() {}
```

覆盖点：

- raw `desired_exposure` 小幅变化但未改变顶层 `desired_exposure` 时，返回 `LiveOnly`
- 只有 effect plan / durable 后果变化时，才返回 `Durable(...)`
- application 只对 `Durable(...)` 发 `TrackChanged`

- [ ] **Step 2: 运行定向测试，确认当前实现失败**

Run:

- `cargo test -p poise-engine observe_market_returns_live_only_when_raw_target_moves_but_execution_intent_is_unchanged -- --exact`
- `cargo test -p poise-engine observe_market_returns_durable_when_planned_effects_change -- --exact`
- `cargo test -p poise-application observe_market_live_only_tick_does_not_emit_track_changed -- --exact`

Expected:

- `observe_market(...)` 现在仍然会因为 raw target 改变而落到 durable 写路径

- [ ] **Step 3: 引入窄结果类型并按执行意图判定 durable**

在 `engine/src/manager.rs` 引入：

```rust
pub enum MarketMutationOutcome {
    LiveOnly,
    Durable(TrackTransition),
}
```

并把：

```rust
pub fn observe_market(
    &mut self,
    id: &TrackId,
    observation: MarketObservation,
) -> Result<MarketMutationOutcome>
```

内部规则改成：

- 先更新 live quote 和 live target
- 再运行现有 reconcile / effect planning
- 如果量化后的执行意图需要改变顶层 durable `desired_exposure`，就把它写进 `TrackTransition`
- 如果没有新的 domain events、effects、durable executor/risk changes，也没有 stale 边界变化，则返回 `LiveOnly`
- 只有这些 durable 后果真正变化时，才返回 `Durable(...)`

- [ ] **Step 4: application 只对 `Durable(...)` 走持久化与广播**

在 `application/src/mutation_executor.rs` 改成：

```rust
pub(crate) async fn observe_market(
    &self,
    id: &str,
    observation: MarketObservation,
) -> Result<Option<TrackTransition>> {
    match self
        .mutate_track(id, |manager| manager.observe_market(&TrackId::new(id), observation.clone()))
        .await?
    {
        MarketMutationOutcome::LiveOnly => Ok(None),
        MarketMutationOutcome::Durable(transition) => Ok(Some(transition)),
    }
}
```

`TrackObservationService::observe_market(...)` 同步改成返回 `Result<Option<TrackTransition>>`。

- [ ] **Step 5: 跑 Task 2 回归**

Run:

- `cargo test -p poise-engine observe_market_returns_live_only_when_raw_target_moves_but_execution_intent_is_unchanged -- --exact --nocapture`
- `cargo test -p poise-engine observe_market_returns_durable_when_planned_effects_change -- --exact --nocapture`
- `cargo test -p poise-application observe_market_live_only_tick_does_not_emit_track_changed -- --exact --nocapture`

Expected:

- raw target 抖动不再直接写库
- 真正 durable 变化仍会持久化

- [ ] **Step 6: Commit**

```bash
git add engine/src/manager.rs application/src/mutation_executor.rs application/src/track_observation_service.rs docs/superpowers/plans/2026-04-15-live-quote-persistence-decoupling.md
git commit -m "feat(application): persist market updates only on durable intent changes"
```

### Task 3: 用 `TrackLiveView` 重建 query/read-model，但保持 HTTP / durable 读模型形状稳定

**Files:**
- Modify: `application/src/mutation_executor.rs`
- Modify: `application/src/track_observation_service.rs`
- Modify: `application/src/track_read_source.rs`
- Modify: `application/src/query_service.rs`
- Modify: `application/src/read_model.rs`
- Modify: `server/src/projector.rs`
- Test: `application/src/query_service.rs`
- Test: `application/src/read_model.rs`
- Test: `server/src/projector.rs`

- [ ] **Step 1: 先写失败测试，锁住“字段形状不变，但来源改成 durable + live 拼装”**

新增至少这些测试：

```rust
#[tokio::test]
async fn load_track_detail_source_merges_durable_snapshot_and_live_view() {}

#[test]
fn read_model_uses_track_live_view_for_market_and_target_fields() {}

#[test]
fn projector_preserves_existing_detail_and_list_shapes() {}
```

覆盖点：

- `TrackReadModel` 继续包含 `strategy_price_status`、`mark_price`、`best_bid`、`best_ask`、`desired_exposure`
- 这些字段不再直接来自 snapshot
- projector 仍然消费同样的 `TrackReadModel` 字段

- [ ] **Step 2: 运行定向测试，确认当前实现失败**

Run:

- `cargo test -p poise-application load_track_detail_source_merges_durable_snapshot_and_live_view -- --exact`
- `cargo test -p poise-application read_model_uses_track_live_view_for_market_and_target_fields -- --exact`
- `cargo test -p poise-server projector_preserves_existing_detail_and_list_shapes -- --exact`

Expected:

- 当前 query/read-model 仍然直接依赖 snapshot 中的 quote / target 字段

- [ ] **Step 3: 给 application query 层暴露窄的 live 查询**

在 `application/src/mutation_executor.rs` / `application/src/track_observation_service.rs` 增加：

```rust
pub(crate) async fn track_live_view(&self, id: &str) -> Result<TrackLiveView> {
    let manager = self.manager.read().await;
    manager.track_live_view(&TrackId::new(id))
}

pub(crate) async fn quote_health_view(&self, id: &str) -> Result<QuoteHealthView> { ... }
pub(crate) async fn strategy_target_view(&self, id: &str) -> Result<StrategyTargetView> { ... }
```

- [ ] **Step 4: 让 `TrackReadSource` / `TrackReadModel` 改为显式从 parts 构造**

在 `application/src/track_read_source.rs` 改成：

```rust
impl TrackRuntimeReadState {
    pub fn from_parts(snapshot: TrackRuntimeSnapshot, live: TrackLiveView) -> Self {
        // 从 durable snapshot 取 lifecycle / ledger / executor / risk
        // 从 live 视图取 strategy_price / status / quotes / desired_exposure / block_reason
    }
}
```

然后在 `application/src/query_service.rs` 的 `load_track_detail_source(...)` 中改成：

```rust
let snapshot = self.track_store.load_track_snapshot(track_id).await?;
let live = self.observation.track_live_view(track_id).await?;

Ok(TrackReadSource {
    definition,
    runtime: TrackRuntimeReadState::from_parts(snapshot, live),
    updated_at,
    recent_track_events,
    recent_effects,
})
```

- [ ] **Step 5: 跑 Task 3 回归**

Run:

- `cargo test -p poise-application load_track_detail_source_merges_durable_snapshot_and_live_view -- --exact --nocapture`
- `cargo test -p poise-application read_model_uses_track_live_view_for_market_and_target_fields -- --exact --nocapture`
- `cargo test -p poise-server projector_preserves_existing_detail_and_list_shapes -- --exact --nocapture`

Expected:

- HTTP / durable websocket 读模型形状稳定
- quote/target 字段改为 query-time live 拼装

- [ ] **Step 6: Commit**

```bash
git add application/src/mutation_executor.rs application/src/track_observation_service.rs application/src/track_read_source.rs application/src/query_service.rs application/src/read_model.rs server/src/projector.rs docs/superpowers/plans/2026-04-15-live-quote-persistence-decoupling.md
git commit -m "refactor(application): compose read models from durable snapshot and live views"
```

### Task 4: 为 UI 增加低频 `TrackLiveViewChanged` 路径

**Files:**
- Modify: `protocol/src/lib.rs`
- Modify: `server/src/websocket.rs`
- Modify: `server/src/server_context.rs`
- Modify: `server/src/assembly.rs`
- Modify: `server/src/runtime/market_data.rs`
- Modify: `tui/src/protocol.rs`
- Modify: `tui/src/app.rs`
- Modify: `tui/src/main.rs`
- Test: `server/src/websocket.rs`
- Test: `tui/src/main.rs`

- [ ] **Step 1: 先写失败测试，锁住“市场字段单独低频刷新”**

新增至少这些测试：

```rust
#[tokio::test]
async fn websocket_coalesces_live_view_updates_per_track_at_250ms_windows() {}

#[tokio::test]
async fn websocket_live_view_updates_do_not_trigger_full_detail_projection() {}

#[tokio::test]
async fn tui_applies_track_live_view_patch_without_reloading_detail() {}
```

覆盖点：

- 同一 `track_id` 高频 live 更新被按 `250ms` 时间窗合并
- `TrackLiveViewChanged` 不会触发 full detail 重投影
- TUI 收到 live patch 后会更新市场/目标字段

- [ ] **Step 2: 运行定向测试，确认当前实现失败**

Run:

- `cargo test -p poise-server websocket_coalesces_live_view_updates_per_track_at_250ms_windows -- --exact`
- `cargo test -p poise-server websocket_live_view_updates_do_not_trigger_full_detail_projection -- --exact`
- `cargo test -p poise-tui tui_applies_track_live_view_patch_without_reloading_detail -- --exact`

Expected:

- protocol 里还没有 `TrackLiveViewChanged`
- websocket 还没有 live dirty / 250ms flush

- [ ] **Step 3: 先扩协议，再实现 server 低频 live 刷新**

在 `protocol/src/lib.rs` 增加：

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackLiveView {
    pub strategy_price: Option<f64>,
    pub strategy_price_status: StrategyPriceStatusView,
    pub mark_price: Option<f64>,
    pub best_bid: Option<f64>,
    pub best_ask: Option<f64>,
    pub desired_exposure: Option<f64>,
    pub price_execution_block_reason: Option<PriceExecutionBlockReasonView>,
}

pub enum StreamEvent {
    // existing...
    TrackLiveViewChanged {
        track_id: String,
        live: TrackLiveView,
    },
}
```

然后在 `server/src/websocket.rs`：

- 新增 per-connection live dirty set
- 每 `250ms` flush 一次 dirty `track_id`
- flush 时只查询 `TrackLiveView`
- 推送 `TrackLiveViewChanged`

要求：

- 继续保留 durable `TrackChanged` 的现有批内去重
- diagnostics 分开统计 durable pushes 和 live pushes

- [ ] **Step 4: 让 TUI 合并 live patch**

在 `tui/src/main.rs` / `tui/src/app.rs`：

```rust
match event {
    StreamEvent::TrackLiveViewChanged { track_id, live } => {
        app.apply_track_live_view(&track_id, live);
    }
    // existing...
}
```

`App::apply_track_live_view(...)` 只更新：

- 当前列表项需要的 strategy/market 摘要
- 当前 detail 页里的 market / target / gate/status 字段

不触发额外 HTTP reload。

- [ ] **Step 5: 跑 Task 4 回归**

Run:

- `cargo test -p poise-server websocket_ -- --nocapture`
- `cargo test -p poise-tui tui_applies_track_live_view_patch_without_reloading_detail -- --exact --nocapture`

Expected:

- 高频 live 更新会被低频合并
- full-detail durable 重投影不再由市场字段刷新驱动

- [ ] **Step 6: Commit**

```bash
git add protocol/src/lib.rs server/src/websocket.rs server/src/server_context.rs server/src/assembly.rs server/src/runtime/market_data.rs tui/src/protocol.rs tui/src/app.rs tui/src/main.rs docs/superpowers/plans/2026-04-15-live-quote-persistence-decoupling.md
git commit -m "feat(ws): add low-frequency live market refresh stream"
```

### Task 5: 明确启动基线与全量验收

**Files:**
- Modify: `server/src/runtime/startup_sync.rs`
- Modify: `server/src/runtime/tests/startup_sync.rs`
- Modify: `server/src/runtime/tests/execution.rs`
- Modify: `server/src/effect_worker/tests/support.rs`
- Modify: `server/src/test_support.rs`
- Modify: `docs/superpowers/specs/2026-04-15-live-quote-persistence-decoupling-design.md`
- Modify: `docs/superpowers/plans/2026-04-15-live-quote-persistence-decoupling.md`

- [ ] **Step 1: 先写失败测试，锁住启动后无 live quote 的基线**

新增至少这些测试：

```rust
#[tokio::test]
async fn startup_without_new_tick_exposes_missing_live_quote_baseline() {}

#[tokio::test]
async fn first_tick_after_startup_rehydrates_live_view_and_execution_inputs() {}
```

覆盖点：

- 从 persisted snapshot 恢复后，在第一条 tick 前没有有效 live quote
- 首个 tick 到来后，`TrackLiveView` 与执行输入恢复

- [ ] **Step 2: 运行定向测试，确认当前实现失败**

Run:

- `cargo test -p poise-server startup_without_new_tick_exposes_missing_live_quote_baseline -- --exact`
- `cargo test -p poise-server first_tick_after_startup_rehydrates_live_view_and_execution_inputs -- --exact`

Expected:

- 当前启动路径和夹具仍默认带着旧 quote

- [ ] **Step 3: 做最小实现与夹具更新**

在 `server/src/effect_worker/tests/support.rs`、`server/src/test_support.rs`、`server/src/runtime/startup_sync.rs` 里统一改成：

```rust
// 只恢复 durable snapshot，不注入任何 live quote / raw target
```

要求：

- 启动阶段没有 live quote
- 首个新 tick 后自然恢复

- [ ] **Step 4: 跑完整回归**

Run:

- `cargo test -p poise-engine --no-run`
- `./target/debug/deps/poise_engine-237eca218a85b938 --nocapture`
- `cargo test -p poise-application --no-run`
- `./target/debug/deps/poise_application-7b2594c1d9b16a6d --nocapture`
- `cargo test -p poise-protocol -- --nocapture`
- `cargo test -p poise-server --no-run`
- `./target/debug/deps/poise_server-cded5ca7f4175f44 --nocapture`
- `cargo test -p poise-tui -- --nocapture`

Expected:

- `poise-engine` 全绿
- `poise-application` 全绿
- `poise-server` 全绿
- `poise-tui` 全绿
- websocket diagnostics 相关测试继续通过

- [ ] **Step 5: 同步 spec / plan 与回写 commit SHA**

把 spec 里接口名同步到最终实现版本，并把本 plan 中每个 task 的：

- checkbox
- 对应 commit SHA

回写成真实状态。

- [ ] **Step 6: Commit**

```bash
git add server/src/runtime/startup_sync.rs server/src/runtime/tests/startup_sync.rs server/src/runtime/tests/execution.rs server/src/effect_worker/tests/support.rs server/src/test_support.rs docs/superpowers/specs/2026-04-15-live-quote-persistence-decoupling-design.md docs/superpowers/plans/2026-04-15-live-quote-persistence-decoupling.md
git commit -m "docs(runtime): sync live quote persistence decoupling plan and design"
```

## Plan Self-Review

- Spec coverage:
  - live quote / live target / durable snapshot 拆分、`LiveOnly / Durable(...)`、去掉 raw target durable 边界、low-frequency UI live refresh、启动基线、全量验收，都有对应 task
- Placeholder scan:
  - 无 `TODO` / `TBD`
  - 每个 task 都给了测试、实现接口、命令和 commit
- Type consistency:
  - 统一使用 `MarketMutationOutcome`
  - 统一使用 `TrackLiveView` / `QuoteHealthView` / `StrategyTargetView`
  - `TrackRuntimeSnapshot` 始终被视为 durable-only
