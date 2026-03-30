# Grid Engine Runtime Internalization Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把预算归属、startup exchange state 合并规则和 observation-driven reconcile 语义继续收回 `engine`，让 `server/runtime` 退化成外部事件翻译层。

**Architecture:** 这次按四段推进：先把 `CapacityBudget` 收进 `GridRuntime`，再把 startup sync 改成专用 `sync_exchange_state()` 入口，然后拆出 `effect_service` 并修掉 submit 恢复分支，最后把 `PendingOrder` 构建和 observation-driven reconcile 语义收回 `engine`。每段都先写失败测试，再做最小实现，最后跑定向回归。

**Tech Stack:** Rust workspace, tokio, axum, serde, anyhow, chrono

---

## File Structure

### 新建文件

- `server/src/effect_service.rs`：承接 effect/outbox 查询、effect 完成和 startup submit anchor 判定

### 修改文件

- `engine/src/runtime.rs`：把 `CapacityBudget` 收进 `GridRuntime`
- `engine/src/observation.rs`：删掉 startup sync 混入的通用 observation
- `engine/src/reconciler.rs`：改成直接从 `GridRuntime` 读取预算
- `engine/src/manager.rs`：删除 budget map，新增 `sync_exchange_state()`，收回 observation-driven reconcile 语义
- `server/src/write_service.rs`：增加 `sync_exchange_state()` 写侧入口，收窄到 mutation 边界
- `server/src/runtime.rs`：删除 user data 补丁重算逻辑，startup sync 改走专用入口
- `server/src/effect_worker.rs`：改走 `effect_service` 并修正 submit 恢复语义
- `server/src/assembly.rs`：组装 `effect_service`
- `docs/superpowers/specs/2026-03-27-poise-engine-runtime-internalization-design.md`：实现中如果接口命名微调，同步 spec
- `docs/superpowers/plans/2026-03-27-poise-engine-runtime-internalization.md`：执行过程中同步任务清单

---

### Task 1: 把预算归属收回 `GridRuntime`

**Files:**
- Modify: `engine/src/runtime.rs`
- Modify: `engine/src/reconciler.rs`
- Modify: `engine/src/manager.rs`
- Modify: `server/src/assembly.rs`
- Test: `engine/src/reconciler.rs`
- Test: `engine/src/manager.rs`

- [x] **Step 1: 先写失败测试，锁住预算不再依赖外置 map**

在 `engine/src/reconciler.rs` 增加一个测试，直接构造带预算的 `GridRuntime`，验证风险裁剪仍然生效：

```rust
#[test]
fn reconcile_reads_budget_from_runtime() {
    let mut grid = test_runtime();
    grid.status = GridStatus::Active;
    grid.current_exposure = Exposure(0.0);
    grid.reference_price = Some(100.0);
    grid.budget = CapacityBudget {
        max_notional: 750.0,
        daily_loss_limit: -120.0,
        stop_loss_pct: 4.0,
    };

    let result = reconcile(&grid, 90.0);

    assert!(matches!(
        result.events.as_slice(),
        [DomainEvent::RiskCapApplied { .. }, DomainEvent::ExposureTargetChanged { .. }]
    ));
}
```

再在 `engine/src/manager.rs` 增加一个测试，确认 `resume_grid()` 在只有 runtime 自带预算时仍能重算状态：

```rust
#[test]
fn resume_grid_recomputes_status_without_external_budget_store() {
    let mut manager = test_manager();
    manager.add_grid(
        GridId::new("btc"),
        test_instrument(),
        test_config(),
        test_budget(),
        test_exchange_rules(),
    ).unwrap();
    manager.observe(
        &GridId::new("btc"),
        GridObservation::Market(MarketObservation { reference_price: 95.0 }),
    ).unwrap();
    manager.pause_grid("btc").unwrap();

    manager.resume_grid("btc").unwrap();

    assert_eq!(manager.get_grid("btc").unwrap().status, GridStatus::Active);
}
```

- [x] **Step 2: 运行定向测试确认失败**

Run:

```bash
cargo test -p poise-engine reconciler::tests::reconcile_reads_budget_from_runtime -- --exact
cargo test -p poise-engine manager::tests::resume_grid_recomputes_status_without_external_budget_store -- --exact
```

Expected: 编译失败或测试失败，因为当前 `reconcile()` 和 `GridManager` 仍然依赖外置 budget map。

- [x] **Step 3: 做最小实现，把预算收回 runtime**

要求：

- `GridRuntime` 增加 `budget: CapacityBudget`
- `GridRuntime::new()` 接收 `budget`
- `GridManager` 删除 `budgets: HashMap<GridId, CapacityBudget>`
- `reconciler::reconcile()` 改成 `reconcile(grid: &GridRuntime, price: f64)`
- `resume_grid()` / `reconcile_grid()` 直接从 runtime 读预算
- `server/src/assembly.rs` 按新签名装配

- [x] **Step 4: 运行定向测试**

Run:

```bash
cargo test -p poise-engine reconciler::tests::
cargo test -p poise-engine manager::tests::
cargo test -p poise-server assembly::tests::
```

Expected: `poise-engine` 相关测试通过，`poise-server` 组装测试保持全绿。

- [ ] **Step 5: 提交**

```bash
git add engine/src/runtime.rs engine/src/reconciler.rs engine/src/manager.rs server/src/assembly.rs
git commit -m "refactor(engine): move budget ownership into grid runtime"
```

---

### Task 2: 引入 `sync_exchange_state()`，收回 startup sync 合并规则

**Files:**
- Modify: `engine/src/observation.rs`
- Modify: `engine/src/manager.rs`
- Modify: `server/src/write_service.rs`
- Modify: `server/src/runtime.rs`
- Test: `engine/src/manager.rs`
- Test: `server/src/runtime.rs`
- Test: `server/src/write_service.rs`

- [x] **Step 1: 先写失败测试，锁住 startup sync 的专用入口语义**

在 `engine/src/manager.rs` 增加测试：

```rust
#[test]
fn exchange_state_observation_clears_stale_pending_order_when_anchor_is_not_preserved() {
    let mut manager = manager_with_pending_order("order-stale", OrderStatus::New);

    let transition = manager.observe(
        &GridId::new("btc"),
        PositionObservation {
            qty: 0.0,
            unrealized_pnl: 0.0,
        },
        vec![],
        false,
    ).unwrap();

    assert_eq!(transition.snapshot.pending_order, None);
}

#[test]
fn exchange_state_observation_preserves_submit_anchor_when_requested() {
    let mut manager = manager_with_pending_order("order-restored", OrderStatus::Submitting);

    let transition = manager.observe(
        &GridId::new("btc"),
        PositionObservation {
            qty: 0.0,
            unrealized_pnl: 0.0,
        },
        vec![],
        true,
    ).unwrap();

    assert_eq!(
        transition.snapshot.pending_order.unwrap().order_id.as_deref(),
        Some("order-restored")
    );
}
```

在 `server/src/runtime.rs` 增加测试，要求 startup sync 仍能通过现有恢复锚点场景，但内部走单一 observation：

```rust
#[tokio::test]
async fn startup_sync_uses_exchange_state_observation_to_restore_live_state() {
    let fixture = runtime_fixture(Some(test_snapshot()), btc_position(0.2, 15.0), vec![open_order()], test_budget()).await;

    let handles = fixture.runtime.start().await.unwrap();

    wait_until_instance(&fixture.state, |instance| {
        instance.current_exposure == Exposure(0.8) && instance.pending_order.is_some()
    }).await;

    shutdown(handles).await;
}
```

- [x] **Step 2: 运行定向测试确认失败**

Run:

```bash
cargo test -p poise-engine manager::tests::exchange_state_observation_clears_stale_pending_order_when_anchor_is_not_preserved -- --exact
cargo test -p poise-engine manager::tests::exchange_state_observation_preserves_submit_anchor_when_requested -- --exact
cargo test -p poise-server startup_sync_uses_exchange_state_observation_to_restore_live_state -- --exact
```

Expected: 编译失败或测试失败，因为当前还没有 `sync_exchange_state()` 专用入口和对应写侧入口。

- [x] **Step 3: 做最小实现，把 startup sync 合并规则收回 engine**

要求：

- `GridObservation` 保持只有 `Market / Position / Order`
- `GridManager` 新增 `sync_exchange_state()`
- 处理顺序固定为：更新 position -> 根据 flag 决定是否清 `pending_order` -> 按确定性顺序检查并重放 open orders
- 如果同一 grid 存在多于一笔 live open order，直接返回错误，不做隐式覆盖
- 不自动触发 `reconcile`
- `GridWriteService` 增加 `sync_exchange_state()`
- `ServerRuntime::startup_sync()` 改成直接传 `PositionObservation + Vec<OrderObservation>`

- [x] **Step 4: 运行定向测试**

Run:

```bash
cargo test -p poise-engine manager::tests::exchange_state_observation_ -- --nocapture
cargo test -p poise-server startup_sync_ -- --nocapture
cargo test -p poise-server write_service::tests::
```

Expected: startup sync 相关回归仍然通过，新的 observation 测试通过。

- [ ] **Step 5: 提交**

```bash
git add engine/src/observation.rs engine/src/manager.rs server/src/write_service.rs server/src/runtime.rs
git commit -m "refactor(engine): absorb startup exchange sync into observation model"
```

---

### Task 3: 把 observation-driven reconcile 收回 `engine.observe()`

**Files:**
- Modify: `engine/src/manager.rs`
- Modify: `server/src/runtime.rs`
- Test: `engine/src/manager.rs`
- Test: `server/src/runtime.rs`

- [x] **Step 1: 先写失败测试，锁住 order / position observation 的重算语义**

在 `engine/src/manager.rs` 增加测试：

```rust
#[test]
fn observe_position_with_cached_reference_price_reconciles_immediately() {
    let mut manager = active_manager_with_reference_price(95.0);

    let transition = manager.observe(
        &GridId::new("btc"),
        GridObservation::Position(PositionObservation {
            qty: 0.0,
            unrealized_pnl: 0.0,
        }),
    ).unwrap();

    assert!(transition.effects.iter().any(|effect| matches!(effect, GridEffect::SubmitOrder { .. })));
}

#[test]
fn observe_canceled_order_with_cached_reference_price_reconciles_immediately() {
    let mut manager = active_manager_with_reference_price_and_pending_order(95.0);

    let transition = manager.observe(
        &GridId::new("btc"),
        GridObservation::Order(OrderObservation {
            order_id: "order-1".into(),
            client_order_id: "btc-reconcile".into(),
            side: Side::Buy,
            price: 95.0,
            quantity: 0.2,
            realized_pnl: 0.0,
            status: OrderStatus::Canceled,
        }),
    ).unwrap();

    assert!(transition.effects.iter().any(|effect| matches!(effect, GridEffect::SubmitOrder { .. })));
}

#[test]
fn observe_filled_order_does_not_reconcile_before_position_update() {
    let mut manager = active_manager_with_reference_price_and_pending_order(95.0);

    let transition = manager.observe(
        &GridId::new("btc"),
        GridObservation::Order(filled_order_observation()),
    ).unwrap();

    assert!(!transition.effects.iter().any(|effect| !matches!(effect, GridEffect::NoOp)));
}
```

在 `server/src/runtime.rs` 额外加一条验收测试，证明不再通过 `command_reconcile()` 补命令；原有“无需等待新 tick 就继续提交”的端到端测试继续保留：

```rust
#[tokio::test]
async fn position_update_reconciles_without_runtime_follow_up_command() {
    // 断言 server/runtime 不再显式调用 command_reconcile()，
    // 且 position user-data 只走一次业务写路径。
}
```

- [x] **Step 2: 运行定向测试确认失败**

Run:

```bash
cargo test -p poise-engine manager::tests::observe_position_with_cached_reference_price_reconciles_immediately -- --exact
cargo test -p poise-engine manager::tests::observe_canceled_order_with_cached_reference_price_reconciles_immediately -- --exact
cargo test -p poise-engine manager::tests::observe_filled_order_does_not_reconcile_before_position_update -- --exact
cargo test -p poise-server runtime::tests::position_update_reconciles_without_runtime_follow_up_command -- --exact
```

Expected: 失败，因为当前 `engine.observe()` 还不会自己触发后续重算。

- [x] **Step 3: 做最小实现，把 user data 重算语义收回 engine**

要求：

- `GridManager.observe(Position)` 在已有参考价时直接走 `reconcile_grid()`
- `GridManager.observe(Order)` 更新状态后，仅在 `Canceled / Rejected / Expired` 且已有参考价时直接走 `reconcile_grid()`
- `Filled / PartiallyFilled` 继续等待后续 `PositionObservation`
- `server/runtime.rs` 删除：
  - `should_reconcile_after_user_data()`
  - `command_reconcile()`
  - user data replay / live task 中的补丁命令分支

- [x] **Step 4: 运行定向测试**

Run:

```bash
cargo test -p poise-engine manager::tests::
cargo test -p poise-server runtime::tests::position_update_
cargo test -p poise-server runtime::tests::terminal_order_update_
```

Expected: 现有“position/order update 会立即重算”的验收测试继续通过，新增的 `position_update_reconciles_without_runtime_follow_up_command` 也通过，但重算语义已经由 `engine.observe()` 提供。

- [ ] **Step 5: 提交**

```bash
git add engine/src/manager.rs server/src/runtime.rs
git commit -m "refactor(engine): internalize observation-driven reconcile semantics"
```

---

### Task 4: 拆出 `effect_service`，集中 `PendingOrder` 构建并修复 submit 恢复语义

**Files:**
- Create: `server/src/effect_service.rs`
- Modify: `engine/src/runtime.rs`
- Modify: `engine/src/reconciler.rs`
- Modify: `server/src/assembly.rs`
- Modify: `server/src/effect_worker.rs`
- Modify: `server/src/runtime.rs`
- Test: `engine/src/runtime.rs`
- Test: `server/src/runtime.rs`
- Test: `server/src/effect_service.rs`

- [x] **Step 1: 先写失败测试**

覆盖：

- `PendingOrder` builder 统一由 `engine/runtime` 提供
- receipt 已落 snapshot、effect pending、live open order 缺失且 target 未达时保持 pending，等待 live exchange state
- 只有 receipt-backed 恢复证据仍在时，live open order 缺失但 target 已达才直接完成 effect
- 如果已经没有 receipt-backed 恢复证据，即便 target 已达，旧 submit effect 也必须进入 `Superseded`
- 当前 runtime 已经 `Pause` 或当前仓位/目标已经让旧 submit 失效时，过时 effect 不再继续发单，并进入 `Superseded`
- 旧 submit effect 进入 `Superseded` 后，要立刻补出当前 runtime 对应的替代计划
- 只有存在匹配 pending `SubmitOrder` effect 时，`Submitting` 锚点才继续抑制 startup 后的重复 submit
- receipt-backed 恢复证据在 startup sync 后仍能保留给 effect recovery 使用
- 没有匹配 pending effect 的孤儿 `Submitting` 锚点会在 startup sync 被清掉
- submit 被交易所拒绝且本地清理 `Submitting` 再失败时，effect 保持 pending，不提前终结
- rounding / quantity step 不会把仍然有效的 submit effect 误判成 stale

- [x] **Step 2: 做最小实现**

要求：

- 新增 `server/src/effect_service.rs`
- `runtime` / `effect_worker` 改走 `effect_service`
- `PendingOrder` 新增 submit request / receipt / live order / order observation builder
- `PendingOrder::is_submit_recovery_anchor()` 统一定义真正恢复锚点
- `PendingOrder::target_reached()` 统一 live state 恢复判定
- `SubmitRecoveryAnchor` 代替裸 `bool` 穿过 startup sync 边界，并同时承载 submit anchor / receipt-backed 恢复证据
- `sync_exchange_state()` 自己排序并验证 live open orders
- `effect_worker` 在执行 submit 前先按当前 runtime 计划校验 effect 是否仍然对齐，不再反推 exposure
- 过时 submit effect 进入 `Superseded`，并保证投影不会显示成 `submit order succeeded`
- `Proceed/AwaitExchangeState` 的恢复判定不产生多余持久化写入
- `GridManager` 在 submit recovery 期 suppress 二次 effect；`reconciler` 保持通用规划逻辑

- [x] **Step 3: 跑定向回归**

Run:

```bash
cargo test -p poise-engine runtime::tests::pending_order_ -- --nocapture
cargo test -p poise-server runtime::tests::effect_worker_ -- --nocapture
cargo test -p poise-server effect_service::tests:: -- --nocapture
```

Expected: 恢复分支按 live state 选择“恢复 / 完成 / 重试”，且 `PendingOrder` 形状不回归。

- [ ] **Step 4: 提交**

```bash
git add engine/src/runtime.rs engine/src/reconciler.rs server/src/effect_service.rs server/src/effect_worker.rs server/src/runtime.rs server/src/assembly.rs
git commit -m "refactor(server): separate effect service and fix submit recovery"
```

---

### Task 5: 同步文档并跑模块验收

**Files:**
- Modify: `docs/superpowers/specs/2026-03-27-poise-engine-runtime-internalization-design.md`
- Modify: `docs/superpowers/plans/2026-03-27-poise-engine-runtime-internalization.md`

- [x] **Step 1: 对照实现同步 spec**

要求：

- 如果 `ExchangeStateObservation`、辅助函数或测试命名有实际调整，回写 spec
- 不保留“实现如此但文档还写旧方案”的状态

- [x] **Step 2: 跑模块级验收**

Run:

```bash
cargo test -p poise-engine
cargo test -p poise-server
```

Expected: `poise-engine`、`poise-server` 全绿；`cargo test --workspace` 由人工最终统一执行。

- [x] **Step 3: 同步任务清单**

要求：

- 把本计划已完成项改成 `- [x]`
- 如果执行过程中出现必要的新任务，直接补进本计划，不另开临时 TODO

- [ ] **Step 4: 提交**

```bash
git add docs/superpowers/specs/2026-03-27-poise-engine-runtime-internalization-design.md docs/superpowers/plans/2026-03-27-poise-engine-runtime-internalization.md
git commit -m "docs: record engine runtime internalization plan"
```
