# Architecture Review Remediation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 按 `docs/2026-03-30-architecture-review.md` 的最终结论消除 6 条正式 finding，并把 `Exposure` 设计债和 `EffectService` 清理项明确落位。

**Architecture:** 分三段推进。先收紧 engine 内部边界（executor 拆分、定价参数化、运行态封装）；再收紧 server 应用边界（恢复同步收敛、读模型边界）；最后验收回写。不引入独立 projection store，不重写运行方式，优先在当前 crate 边界内把复杂度压回单一 owner。

**Tech Stack:** Rust workspace, cargo test, tokio, axum, serde, rusqlite

**Task 依赖关系：**

```
Task 1 (executor 拆分) ──→ Task 2 (replacement gate 参数化)
                                │
                                ↓
                          Task 3 (runtime 封装)
                                │
                                ↓
                          Task 4 (sync 收敛)
                                │
                                ↓
                          Task 5 (read model)
                                │
                                ↓
                          Task 6 (验收)
```

---

## File Structure

### 新增文件

- `engine/src/executor/mod.rs`：re-export 对外 API
- `engine/src/executor/planning.rs`：plan、replacement gate、desired order
- `engine/src/executor/recovery.rs`：recover_working_orders、recover_submit_effect
- `engine/src/executor/recording.rs`：record_submit_*、apply_order_observation、clear_*
- `engine/src/executor/slots.rs`：slot 辅助函数（模块内 pub，对外不暴露）
- `server/src/read_model.rs`：server 自有的读模型类型与转换

### 修改文件

- `engine/src/lib.rs`：executor 切为目录模块
- `core/src/types.rs`：ExchangeRules 扩展 taker_fee_rate
- `exchanges/binance/src/types.rs`：填充 taker_fee_rate
- `engine/src/executor/planning.rs`：replacement gate 读取 ExchangeRules.taker_fee_rate
- `engine/src/runtime.rs`：字段 pub→pub(crate)、restore 完整性保护
- `engine/src/manager.rs`：适配 runtime 新 accessor
- `server/src/runtime.rs`：删除 ExchangeStateSyncMode，使用 engine 定义
- `server/src/write_service.rs`：删除 StartupSyncMode，使用 engine 定义
- `server/src/query_service.rs`：组装 read model 而非直传 snapshot
- `server/src/projector.rs`：消费 read model
- `server/src/effect_service.rs`：根据 Task 4 结果决定保留或删除

---

### Task 1: 拆分执行器模块并缩小公开面

**Depends on:** 无
**对应 finding:** #3（executor.rs 过大且公开面过宽）

**Files:**
- Delete: `engine/src/executor.rs`
- Create: `engine/src/executor/mod.rs`
- Create: `engine/src/executor/planning.rs`
- Create: `engine/src/executor/recovery.rs`
- Create: `engine/src/executor/recording.rs`
- Create: `engine/src/executor/slots.rs`
- Modify: `engine/src/lib.rs`

- [x] **Step 1: 运行现有 executor 测试，记录基线**

Run: `cargo test -p grid-engine executor::tests:: -- --nocapture 2>&1 | tail -5`
Result: 25 passed; 0 failed; 76 filtered out。

- [x] **Step 2: 创建 `engine/src/executor/` 目录，把 `executor.rs` 原样移入 `mod.rs`**

```bash
mkdir engine/src/executor
mv engine/src/executor.rs engine/src/executor/mod.rs
```

Run: `cargo test -p grid-engine executor::tests:: -- --nocapture 2>&1 | tail -5`
Result: 25 passed; 0 failed; 76 filtered out。

- [x] **Step 3: 提取 slots.rs —— 把 slot 辅助函数移出 mod.rs**

把以下函数移到 `engine/src/executor/slots.rs`（保持 `pub(super)` 可见性）：
- `split_inventory_core_slot`
- `split_inventory_core_slot_from_slots`
- `with_inventory_core_slot`
- `replace_first_matching_slot`
- `clear_matching_slots`
- `slot_matches_order`
- `empty_inventory_core_slot`
- `empty_slot`
- `role_for_side`
- `rebuild_slot_from_live_order`

mod.rs 加 `mod slots;` 并把调用点改为 `slots::xxx`。

Run: `cargo test -p grid-engine executor::tests:: -- --nocapture 2>&1 | tail -5`
Result: 25 passed; 0 failed; 76 filtered out。

- [x] **Step 4: 提取 recording.rs —— 把状态录入函数移出 mod.rs**

把以下 pub 函数移到 `engine/src/executor/recording.rs`：
- `record_submit_request`
- `record_submit_receipt`（+ `SubmitReceiptResolution`）
- `record_submit_failure`
- `apply_order_observation`
- `clear_pending_submit`
- `clear_working_order_by_order_id`
- `clear_all_working_orders`
- `submit_pending_slot`（降为 `pub(super)`）

连同辅助函数 `target_exposure_reached`。

mod.rs 加 `mod recording;` 并 re-export 对外需要的符号。

Run: `cargo test -p grid-engine executor::tests:: -- --nocapture 2>&1 | tail -5`
Result: 25 passed; 0 failed; 76 filtered out。

- [x] **Step 5: 提取 recovery.rs —— 把恢复逻辑移出 mod.rs**

把以下 pub 函数移到 `engine/src/executor/recovery.rs`：
- `recover_working_orders`（+ `RecoveryInput`、`RecoveryResolution`、`RecoveryAnomaly`）
- `recover_submit_effect`（+ `SubmitRecoveryInput`、`SubmitRecoveryPlan`、`SubmitRecoveryResolution`、`SubmitRecoveryPlanContext`）
- `submit_requests_match`（+ `submit_recovery_matches_current_plan`、`values_match_with_step`）

连同辅助函数 `recovery_anomaly`。

mod.rs 加 `mod recovery;` 并 re-export。

Run: `cargo test -p grid-engine executor::tests:: -- --nocapture 2>&1 | tail -20`
Result: 25 passed; 0 failed; 76 filtered out。

- [x] **Step 6: 提取 planning.rs —— 把规划逻辑移出 mod.rs**

把以下函数移到 `engine/src/executor/planning.rs`：
- `plan`（+ `ExecutorInput`、`ExecutorPlan`）
- `current_submit_hint`（+ `PendingSubmitHint`）
- `refresh_state`
- `plan_desired_orders`
- `desired_inventory_order`（+ `DesiredOrder`、`OrderRole`、`OrderSlot`）
- `diff_desired_orders`
- `desired_order_to_request`
- `desired_matches_working_order`
- `replacement_gate_reason_for_working_order`
- `replacement_improvement_ratio`
- `rounded_values_match`、`ratio_to_bps`
- 所有常量（`REBALANCE_GAP_THRESHOLD` 等）
- `resolve_gap_started_at`、`resolve_mode`、`resolve_reason`、`update_stats`

mod.rs 加 `mod planning;` 并 re-export。mod.rs 中只保留类型定义（`ExecutionMode`、`ExecutionReason`、`INVENTORY_CORE_SLOT`）和 re-export 声明。

Run: `cargo test -p grid-engine executor::tests:: -- --nocapture 2>&1 | tail -20`
Result: 25 passed; 0 failed; 76 filtered out。

- [x] **Step 7: 缩小 pub 面——把不需要对 crate 外暴露的符号降级**

审查 mod.rs 的 re-export 列表。只保留 `manager.rs` 和 `server/src/write_service.rs` 实际 import 的符号为 `pub`。其余降为 `pub(crate)` 或模块内可见。

具体检查方法：临时注释掉 mod.rs 中某个 re-export，跑 `cargo check --workspace`，如果只有 executor 内部测试报错则可以降级。

Run: `cargo test -p grid-engine && cargo test -p grid-server`
Result: `grid-engine` 101 passed; `grid-server` 119 passed。

Review:
- `engine/src/executor/mod.rs` 已收敛为模块声明、re-export、`ExecutionMode` / `ExecutionReason` / `INVENTORY_CORE_SLOT` 和测试。
- `planning.rs` / `recovery.rs` / `recording.rs` / `slots.rs` 的职责边界已分开，`manager.rs` 只保留一处与 `RecoveryInput` 收敛相关的无行为改动。
- Step 7 实际采用“收紧 re-export”而不是继续暴露子模块，保留了 `runtime` / `write_service` 需要的公共类型，其余 helper 和输入结构降为 `pub(crate)`。

- [x] **Step 8: 提交**

```bash
git add engine/src/lib.rs engine/src/executor/ docs/superpowers/plans/2026-03-30-architecture-review-remediation.md
git commit -m "refactor(engine): split executor into planning/recovery/recording/slots submodules"
```

Commit: `ad4248db896186e9843b7dcf7da3244e7e449710`

---

### Task 2: 把 replacement gate 定价知识从 engine 硬编码改成显式输入

**Depends on:** Task 1（replacement gate 代码现在在 `executor/planning.rs`）
**对应 finding:** #1（Binance 费率常量泄漏到 engine）

**Files:**
- Modify: `core/src/types.rs`
- Modify: `engine/src/executor/planning.rs`
- Modify: `exchanges/binance/src/types.rs`

- [x] **Step 1: 给 ExchangeRules 加 maker_fee_rate 和 taker_fee_rate 字段**

在 `core/src/types.rs` 的 `ExchangeRules` 中加：

```rust
pub struct ExchangeRules {
    pub price_tick: f64,
    pub quantity_step: f64,
    pub min_qty: f64,
    pub min_notional: f64,
    pub maker_fee_rate: f64,
    pub taker_fee_rate: f64,
}
```

Run: `cargo check --workspace`
Result: 编译失败，新测试先报 `maker_fee_rate` 字段不存在；随后 `cargo check --workspace` 也暴露出所有 `ExchangeRules` 构造点都需要补新字段。

- [x] **Step 2: 在所有构造点补上 fee rate 字段**

- `exchanges/binance/src/types.rs`（约 line 130）：填 `maker_fee_rate: 0.0002, taker_fee_rate: 0.0004`（Binance USDⓈ-M VIP0 费率）。注：exchangeInfo API 不返回 fee rate，此处用硬编码默认值；后续可从 `/fapi/v1/commissionRate` 接口动态获取。
- engine 和 server 测试中的 `test_exchange_rules()` 辅助函数：填 `maker_fee_rate: 0.0, taker_fee_rate: 0.0`（测试不依赖费率时用 0）
- `server/src/assembly.rs` 测试中的 `test_exchange_rules()`：同上

Run: `cargo check --workspace`
Result: 编译通过。

- [x] **Step 3: 改写 replacement gate 公式，使用 maker + taker 费率**

在 `engine/src/executor/planning.rs` 中：

```rust
fn replacement_gate_reason_for_working_order(
    current_order: &WorkingOrder,
    desired_order: &DesiredOrder,
    reference_price: f64,
    rules: &ExchangeRules,
) -> Option<ReplacementGateReason> {
    // ... existing guards ...

    let improvement_ratio =
        replacement_improvement_ratio(current_order, desired_order, reference_price);
    let threshold_rate =
        (rules.maker_fee_rate + rules.taker_fee_rate)
            + (REPLACEMENT_SAFETY_BUFFER_BPS / BPS_DENOMINATOR);
    // ... rest unchanged ...
}
```

公式含义：取消旧 maker 单的机会成本（maker_fee）+ 新单可能 cross spread 的成本（taker_fee）+ 安全缓冲。比原来的 `taker * 2` 更准确。

删除 `const BINANCE_TAKER_FEE_RATE: f64 = 0.0004;`。

Run: `cargo check -p grid-engine`
Expected: 编译通过。

- [x] **Step 4: 补测试——验证不同 fee rate 组合产生不同 gate 结果**

在 executor 测试中加测试：
- 同样的价格改善幅度，用 `maker_fee_rate: 0.0005, taker_fee_rate: 0.001`（高费率）应该触发 gate
- 用 `maker_fee_rate: 0.0, taker_fee_rate: 0.0`（零费率）不应该触发

Run: `cargo test -p grid-engine executor::tests::replacement_gate_threshold_uses_exchange_maker_and_taker_fee_rate -- --exact --nocapture`
Result:
- 改实现前先跑到红灯，确认测试捕获的是 `maker_fee_rate` 缺失和旧 gate 公式。
- 改实现后重新运行，新测试通过。

- [x] **Step 5: 全量验证**

Run: `cargo test`
Result: workspace 全部通过。

Review:
- 这次改动仍然停留在计划主线内：新增 `ExchangeRules` 的 fee 字段、更新 Binance 默认值、改写 `replacement_gate` 公式，并补齐所有 `ExchangeRules` 构造点。
- Binance 适配层只保留了一个明确注释的 VIP0 默认值，没有把 `commissionRate` 拉进本次 task，避免提前引入新的运行时依赖。
- `manager::tests::observe_market_replacement_gate_emits_event_when_reason_changes` 只同步了阈值断言，从 13 bps 改为 11 bps，保留了“reason 发生变化时要发事件”这个原始测试意图。

- [x] **Step 6: 提交**

```bash
git add core/src/types.rs engine/src/executor/planning.rs exchanges/binance/src/types.rs docs/superpowers/plans/2026-03-30-architecture-review-remediation.md
git commit -m "refactor(engine): parameterize replacement gate fees via ExchangeRules"
```

Commit: `67e7c5125024d56b739d1beab23b3158cefabc87`

---

### Task 3: 收紧 GridRuntime 状态边界并补 restore 完整性保护

**Depends on:** Task 2（ExchangeRules 结构已稳定）
**对应 finding:** #4（restore 缺少完整性保护）、#5（运行态封装过浅）

**Files:**
- Modify: `engine/src/runtime.rs`
- Modify: `engine/src/manager.rs`

- [ ] **Step 1: 给 restore_from_snapshot 加 debug_assert round-trip 保护**

在 `engine/src/runtime.rs` 的 `restore_from_snapshot()` 末尾 `Ok(())` 之前加：

```rust
debug_assert_eq!(
    self.snapshot(),
    *snapshot,
    "restore_from_snapshot left persisted fields unsynced"
);
```

这需要 `GridRuntimeSnapshot` 实现 `PartialEq`（已有）。

Run: `cargo test -p grid-engine -- runtime::tests --nocapture`
Expected: 全部通过。debug_assert 验证当前所有字段都被正确 restore。

- [ ] **Step 2: 写一个 regression 测试，证明 debug_assert 能抓住遗漏**

```rust
#[test]
fn restore_from_snapshot_detects_missing_field_via_round_trip() {
    let mut runtime = test_runtime();
    runtime.status = GridStatus::Active;
    runtime.current_exposure = Exposure(4.0);
    runtime.target_exposure = Some(Exposure(6.0));
    runtime.reference_price = Some(96.0);

    let snapshot = runtime.snapshot();
    let mut fresh = test_runtime();
    fresh.restore_from_snapshot(&snapshot).unwrap();
    assert_eq!(fresh.snapshot(), snapshot);
}
```

Run: `cargo test -p grid-engine -- runtime::tests::restore_from_snapshot_detects --exact --nocapture`
Expected: 通过——证明当前 restore 是完整的，且保护装置就位。

- [ ] **Step 3: 把 GridRuntime 字段从 pub 改为 pub(crate)**

把 `engine/src/runtime.rs` 中 `GridRuntime` 的所有字段从 `pub` 改为 `pub(crate)`。

```rust
#[derive(Debug, Clone)]
pub struct GridRuntime {
    pub(crate) id: GridId,
    pub(crate) instrument: Instrument,
    pub(crate) config: GridConfig,
    pub(crate) budget: CapacityBudget,
    pub(crate) exchange_rules: ExchangeRules,
    pub(crate) status: GridStatus,
    pub(crate) current_exposure: Exposure,
    pub(crate) target_exposure: Option<Exposure>,
    pub(crate) manual_target_override: Option<Exposure>,
    pub(crate) executor_state: ExecutorState,
    pub(crate) replacement_gate_reason: Option<ReplacementGateReason>,
    pub(crate) risk_state: RiskState,
    pub(crate) reference_price: Option<f64>,
    pub(crate) out_of_band_since: Option<DateTime<Utc>>,
    pub(crate) last_tick_at: Option<DateTime<Utc>>,
    pub(crate) market_data_stale_since: Option<DateTime<Utc>>,
    pub(crate) tick_timeout_secs: u64,
}
```

Run: `cargo check --workspace`
Expected: engine 内部（manager.rs 等）编译通过；server 中如果有直接字段访问会报错。

- [ ] **Step 4: 给 server 层需要的只读访问加 pub accessor**

检查 server crate 中对 GridRuntime 字段的直接访问（通过 `manager.get_grid()` / `manager.list_grids()` 返回的 `&GridRuntime`）。为每个被访问的字段加 pub accessor。

预期需要的 accessor（基于 `server/src/assembly.rs` 和 `write_service.rs`）：

```rust
impl GridRuntime {
    pub fn id(&self) -> &GridId { &self.id }
    pub fn instrument(&self) -> &Instrument { &self.instrument }
    pub fn status(&self) -> &GridStatus { &self.status }
    pub fn budget(&self) -> &CapacityBudget { &self.budget }
}
```

逐个加，每加一个跑 `cargo check -p grid-server` 确认编译错误减少。

Run: `cargo check --workspace`
Expected: workspace 编译通过。

- [ ] **Step 5: 全量测试验证**

Run: `cargo test`
Expected: workspace 全部通过。

- [ ] **Step 6: 提交**

```bash
git add engine/src/runtime.rs engine/src/manager.rs docs/superpowers/plans/2026-03-30-architecture-review-remediation.md
git commit -m "refactor(engine): encapsulate GridRuntime fields and add restore round-trip guard"
```

---

### Task 4: 收敛 exchange state sync 的单一 owner，消除三处重复语义

**Depends on:** Task 3（runtime accessor 已就位）
**对应 finding:** #2（恢复策略三处重复）
**附带:** EffectService 清理

**Files:**
- Modify: `engine/src/manager.rs`
- Modify: `server/src/runtime.rs`
- Modify: `server/src/write_service.rs`
- Modify: `server/src/effect_service.rs`（可能删除）

- [ ] **Step 1: 运行 server 测试基线**

Run: `cargo test -p grid-server -- --nocapture 2>&1 | tail -5`
Expected: 全部通过，记录通过数量。

- [ ] **Step 2: 把 engine/manager.rs 中的 StartupSyncMode 重命名为 ExchangeSyncMode 并改为 pub**

当前名称 `StartupSyncMode` 语义偏窄——实际上 shutdown 和 recovery retry 也复用同一模式，不只是 startup。重命名为 `ExchangeSyncMode` 更准确地描述"从交易所同步状态时是否追加 reconcile"。

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExchangeSyncMode {
    RecoverOnly,
    RecoverAndReconcile,
}

impl ExchangeSyncMode {
    pub fn allows_follow_up_reconcile(self) -> bool {
        matches!(self, Self::RecoverAndReconcile)
    }
}
```

同步更新 `manager.rs` 内部所有引用点。

Run: `cargo check -p grid-engine`
Expected: 编译通过。

- [ ] **Step 3: 删除 write_service.rs 中的 StartupSyncMode 副本**

删除 `server/src/write_service.rs` 中 `StartupSyncMode` 枚举和 `impl` 块（约 line 23-33），改为 `use grid_engine::manager::ExchangeSyncMode;`。

把 `sync_exchange_state_inner` 的参数类型和内部调用适配到 `ExchangeSyncMode`。

Run: `cargo check -p grid-server`
Expected: 编译通过。

- [ ] **Step 4: 删除 runtime.rs 中的 ExchangeStateSyncMode 副本**

删除 `server/src/runtime.rs` 中 `ExchangeStateSyncMode` 枚举（约 line 45-49），改为 `use grid_engine::manager::ExchangeSyncMode;`。把所有 `ExchangeStateSyncMode::RecoverOnly` / `RecoverAndReconcile` 替换为 `ExchangeSyncMode::RecoverOnly` / `RecoverAndReconcile`。

Run: `cargo check -p grid-server`
Expected: 编译通过。

- [ ] **Step 5: 评估并处理 EffectService**

检查 `server/src/effect_service.rs` 的调用者。如果只有 `effect_worker.rs` 在用，且 EffectService 仍然只是对 `StateRepositoryPort` 的转发，则：
- 删除 `effect_service.rs`
- 让 `EffectWorker` 直接持有 `Arc<dyn StateRepositoryPort>`
- 调整 `assembly.rs` 中的 `ServerState` 和组装逻辑

如果 EffectService 承担了其他职责，保留并记录原因。

Run: `cargo check -p grid-server`
Expected: 编译通过。

- [ ] **Step 6: 全量测试验证**

Run: `cargo test`
Expected: workspace 全部通过。

- [ ] **Step 7: 提交**

```bash
git add engine/src/manager.rs server/src/runtime.rs server/src/write_service.rs server/src/effect_service.rs server/src/effect_worker.rs server/src/assembly.rs docs/superpowers/plans/2026-03-30-architecture-review-remediation.md
git commit -m "refactor: converge ExchangeSyncMode to single engine definition"
```

---

### Task 5: 引入 server-owned read model（phase 1：单一接触点 + 结构拍平）

**Depends on:** Task 4
**对应 finding:** #6（读模型边界贴着 engine snapshot）

**Scope:** Phase 1 只做单一接触点和结构拍平——projector 不再穿透 engine 内部结构（如 `snapshot.executor_state.stats.max_inventory_gap_abs.0`），改为读取拍平后的标量字段。read model 仍复用 engine/core 的稳定枚举类型（GridStatus、ExecutionMode 等），不创建第三份副本。完全类型解耦留作后续 phase 2，当 engine 类型变更频率证明有必要时再推进。

**Files:**
- Create: `server/src/read_model.rs`
- Modify: `server/src/main.rs`
- Modify: `server/src/query_service.rs`
- Modify: `server/src/projector.rs`

- [ ] **Step 1: 运行 projector 和 HTTP 测试基线**

Run: `cargo test -p grid-server -- projector::tests --nocapture && cargo test -p grid-server -- http::tests --nocapture`
Expected: 全部通过。

- [ ] **Step 2: 创建 `server/src/read_model.rs`，定义 read model 结构体**

```rust
use chrono::{DateTime, Utc};
use grid_engine::grid::GridId;
use grid_engine::ports::{PersistedGridEffect, StoredDomainEvent};

pub struct GridReadModel {
    // Identity
    pub grid_id: String,
    pub venue: String,
    pub symbol: String,

    // Lifecycle
    pub status: grid_engine::runtime::GridStatus,
    pub updated_at: DateTime<Utc>,

    // Strategy
    pub lower_price: f64,
    pub upper_price: f64,
    pub shape_family: grid_core::strategy::ShapeFamily,
    pub out_of_band_policy: grid_core::strategy::OutOfBandPolicy,

    // Market
    pub reference_price: Option<f64>,

    // Position
    pub current_exposure: f64,
    pub target_exposure: Option<f64>,

    // Risk / Statistics
    pub realized_pnl_cumulative: f64,
    pub unrealized_pnl: f64,

    // Execution
    pub executor_mode: grid_engine::executor::ExecutionMode,
    pub inventory_gap: f64,
    pub gap_started_at: Option<DateTime<Utc>>,
    pub max_inventory_gap_abs: f64,
    pub max_gap_age_ms: i64,
    pub stats_started_at: DateTime<Utc>,
    pub has_recovery_anomaly: bool,
    pub has_stale_market_data: bool,
    pub replacement_gate_reason: Option<grid_core::events::ReplacementGateReason>,
    pub slots: Vec<ReadModelSlot>,

    // Commands
    pub manual_target_override: Option<f64>,

    // Activity
    pub recent_domain_events: Vec<StoredDomainEvent>,
    pub recent_effects: Vec<PersistedGridEffect>,
}

pub struct ReadModelSlot {
    pub label: String,
    pub is_submit_pending: bool,
    pub side: grid_core::types::Side,
    pub price: f64,
    pub quantity: f64,
    pub role: grid_engine::executor::OrderRole,
}
```

加 `from_snapshot` 构造函数，把 `GridRuntimeSnapshot` + `DateTime<Utc>` + events + effects 转成 `GridReadModel`。这个函数是 snapshot → read model 的唯一接触点。注意：read model 复用 engine/core 的稳定枚举类型（如 `GridStatus`、`ShapeFamily`），但所有结构性访问（嵌套字段遍历）都在 `from_snapshot` 内完成，projector 只看到拍平后的标量和枚举。

在 `server/src/main.rs` 中加 `mod read_model;`。

Run: `cargo check -p grid-server`
Expected: 编译通过（新代码无调用者）。

- [ ] **Step 3: 让 query_service 返回 GridReadModel 而非 GridReadModelSource**

修改 `query_service.rs`：
- `list_grid_sources()` 返回 `Vec<GridReadModel>`
- `load_detail_source()` 返回 `Option<GridReadModel>`
- 内部调用 `GridReadModel::from_snapshot(...)` 做转换

暂时保留 `GridReadModelSource` 结构体（可能有测试依赖），但 pub API 全部切到 `GridReadModel`。

Run: `cargo check -p grid-server`
Expected: projector.rs 和 http.rs 中使用 `GridReadModelSource` 的地方报错。

- [ ] **Step 4: 改写 projector 消费 GridReadModel**

修改 `projector.rs` 中 `project_list_item` 和 `project_detail` 的参数类型，从 `&GridReadModelSource` 改为 `&GridReadModel`。逐字段替换属性访问：

- `source.snapshot.grid_id.as_str()` → `source.grid_id.as_str()`
- `source.snapshot.current_exposure.0` → `source.current_exposure`
- `source.snapshot.executor_state.stats.max_inventory_gap_abs.0` → `source.max_inventory_gap_abs`
- 等等

projector 不再 import 任何 `grid_engine::snapshot::*` 或 `grid_engine::runtime::ExecutorState`。

Run: `cargo check -p grid-server`
Expected: 编译通过。

- [ ] **Step 5: 更新 projector 和 HTTP 测试**

测试中原来构造 `GridReadModelSource` 的地方改为构造 `GridReadModel`。验证 JSON 输出不变。

Run: `cargo test -p grid-server -- projector::tests --nocapture && cargo test -p grid-server -- http::tests --nocapture`
Expected: 全部通过。

- [ ] **Step 6: 删除 GridReadModelSource（如果不再被使用）**

Run: `cargo test -p grid-server && cargo test -p grid-tui`
Expected: 全部通过。

- [ ] **Step 7: 提交**

```bash
git add server/src/main.rs server/src/read_model.rs server/src/query_service.rs server/src/projector.rs docs/superpowers/plans/2026-03-30-architecture-review-remediation.md
git commit -m "refactor(server): introduce server-owned read model boundary"
```

---

### Task 6: 全量验收并回写文档

**Depends on:** Task 5

**Files:**
- Modify: `docs/2026-03-30-architecture-review.md`
- Modify: `docs/superpowers/plans/2026-03-30-architecture-review-remediation.md`

- [ ] **Step 1: 全量验证**

Run: `cargo test`
Expected: workspace 全部通过。

- [ ] **Step 2: 验证 finding 逐条消除**

逐条检查：
1. `rg 'BINANCE_TAKER_FEE' engine/` → 无结果
2. `rg 'StartupSyncMode|ExchangeStateSyncMode' server/src/write_service.rs server/src/runtime.rs` → 只有 import，无本地定义
3. `wc -l engine/src/executor/mod.rs` → 远小于原 2360 行
4. `rg 'debug_assert_eq.*snapshot' engine/src/runtime.rs` → 有结果
5. `rg 'pub [a-z]' engine/src/runtime.rs | grep 'GridRuntime' -A 20` → 字段为 pub(crate)
6. `rg 'GridRuntimeSnapshot' server/src/projector.rs` → 无结果

- [ ] **Step 3: 回写评审文档和本计划**

在 `docs/2026-03-30-architecture-review.md` 末尾标记每条 finding 的完成状态。
在本文件中勾选已完成步骤并记录 commit SHA。

- [ ] **Step 4: 提交**

```bash
git add docs/2026-03-30-architecture-review.md docs/superpowers/plans/2026-03-30-architecture-review-remediation.md
git commit -m "docs: close architecture review remediation"
```

---

## Deferred Follow-up

### A. `Exposure(pub f64)` 设计债

不并入主线。主线完成后单独决策：
- 方向 1：字段私有化 + 领域方法（`abs()`、`signum()`、`value()`）
- 方向 2：明确承认轻量语义标签，删掉不必要包装

另起 follow-up plan。

### B. `EffectService` 清理

Task 4 Step 5 中处理。如果最终保留了，记录原因。

### C. Read Model Phase 2：完全类型解耦

Task 5 是 phase 1（单一接触点 + 结构拍平）。如果后续 engine 枚举类型变更频繁导致 read_model.rs 频繁跟改，再推进 phase 2：把边界敏感字段（GridStatus、ExecutionMode 等）改成 server 自有类型。当前不做——避免在 grid-protocol 已有 wire types 的情况下创建第三份枚举副本。
