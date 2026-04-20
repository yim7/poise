# Track Protection State Model Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把 track 运行模型从平铺的 `status / manual_target_override / flatten_reentry_boundary` 字段，重构成以 `TrackState` 为唯一主状态、以 `BandProtectionPolicy` 和 `RiskOutcome` 为核心契约、并由完整 `runtime_state` 持久化的实现。

**Architecture:** 按共享边界拆分，而不是按时间顺序局部修改。Task 1 只新增不破坏现有消费者的 core 契约；Task 2 一次性迁移 `BandProtectionPolicy` 配置边界和所有直接消费者，但 engine 在该 task 只按 policy 外层变体兼容旧行为，不提前引入 `ReentryGuard` / `target_anchor` 运行时语义；Task 3 再把 `TrackState/runtime_state`、持久化、application 私有 read adapter 和真正的 recover / anchor 状态机一起落地，并把 server 直接 snapshot/runtime-read-state 依赖改成公开 `TrackReadModel` / application API；Task 4 做最终语义校验和跨 crate 验收。这个计划不考虑 legacy 数据迁移，开发和验收环境直接重建状态。

**Tech Stack:** Rust workspace, Cargo, Serde, SQLite, Markdown

---

## Design Constraints

- Task 1 只能新增 core 类型、新 helper 和 core 单测，不能替换已经被 engine 使用的共享函数签名。
- `TrackConfig.out_of_band_policy` 的字段类型切换必须和所有直接消费者放在同一个 task。
- `evaluate_risk` / `flatten_reentry_confirmed` 这种已被 engine 调用的旧函数，只有在所有消费者同 task 迁移时才能替换或删除。
- Task 2 只允许把 engine 消费方切到新的 `BandProtectionPolicy` 形状；任何依赖 `ReentryGuard`、`target_anchor` 或新恢复状态机的运行时语义都必须留在 Task 3。
- `TrackRuntimeSnapshot` 的根接口切换必须和 engine、storage、application 内所有直接持有、构造、持久化或传递 snapshot 的消费者放在同一个 task。
- `TrackRuntimeSnapshot` 不能作为 server read-side / projector 生产代码或测试夹具的输入抽象；server runtime/orchestration 生产路径只能通过 application service API 间接恢复或查询运行态，不能直接解析 snapshot 私有状态。
- `TrackState` 只允许出现在 engine runtime、engine snapshot、持久化恢复、application 单一适配层和对应 application 适配测试里。
- `TrackRuntimeReadState` 是 application 内部适配器，不对 server 或其他 crate re-export。
- `TrackReadModel`、server/projector 以及 server 测试夹具不能暴露、接收或构造完整 `TrackState`。
- server read-side 测试如果需要 durable seed，必须通过 application test-support 或 application service 构造公开 `TrackReadModel`；server runtime/effect-worker 集成测试允许用 snapshot seed manager，但不能把 `TrackState` / `TrackRuntimeReadState` 暴露到 read-side 接口。
- `TrackStatus` 只允许在单一 read adapter 中从 `TrackState` 派生一次。
- `Frozen` / `Holding` 必须使用 `target_anchor` 字段；它表示进入保护状态前最后一个 risk-approved target，不是当前仓位，也不是 executor active-round anchor。
- `ExecutionGateReason` 只能定义一份，由事件可见的共享契约拥有；engine execution gate 直接使用它，不能再定义同形 reason 后转换。

## Files And Responsibilities

- Modify: `core/src/strategy.rs`
  定义 `BandProtectionPolicy`、`BandRecoverPolicy` 和新的 re-entry 价格确认 helper；Task 1 不替换旧 `OutOfBandPolicy` / `flatten_reentry_confirmed`。
- Modify: `core/src/risk.rs`
  定义 `RiskOutcome`、`RiskTerminationCause` 和新的 `evaluate_risk_outcome`；Task 1 不替换旧 `RiskDecision` / `evaluate_risk`。
- Modify: `core/src/events.rs`
  定义唯一的 `ExecutionGateReason` 事件可见词汇；engine execution gate 直接复用，不能再复制一份同形 reason。
- Modify: `protocol/src/lib.rs`
- Modify: `application/src/track_definition.rs`
- Modify: `application/src/read_model.rs`
- Modify: `application/src/query_service.rs`
- Modify: `application/src/debug_query_service.rs`
- Modify: `application/src/mutation_executor.rs`
- Modify: `application/src/track_mutation_store.rs`
- Modify: `application/src/track_persistence.rs`
- Modify: `application/src/lib.rs`
- Modify: `application/src/track_read_source.rs`
  把 `TrackRuntimeReadState` / `TrackReadSource` 限制为 application 内部适配器，不再作为跨 crate 接口 re-export。
- Create: `application/src/test_support.rs`
  只对 `server-test-support` 暴露公开 `TrackReadModel` fixture；内部如需 snapshot/runtime helper，留在 application 私有实现里。
- Create: `engine/src/execution_gate.rs`
- Modify: `engine/src/price_gate.rs`
- Modify: `engine/src/lib.rs`
- Modify: `engine/src/transition.rs`
- Modify: `engine/src/reconciler.rs`
- Modify: `engine/src/runtime.rs`
- Modify: `engine/src/manager.rs`
- Modify: `engine/src/snapshot.rs`
- Modify: `engine/src/persisted_runtime.rs`
- Modify: `storage/src/sqlite.rs`
- Modify: `server/src/config.rs`
- Modify: `server/src/projector.rs`
- Modify: `server/src/http.rs`
- Modify: `server/src/websocket.rs`
- Modify: `server/src/assembly.rs`
- Modify: `server/src/exchange_startup.rs`
- Modify: `server/src/state_bootstrap.rs`
- Modify: `server/src/runtime/reconcile.rs`
- Modify: `server/src/main.rs`
- Modify: `server/src/test_support.rs`
- Modify: `server/src/effect_worker/tests/mod.rs`
- Modify: `server/src/effect_worker/tests/retry.rs`
- Modify: `server/src/effect_worker/tests/support.rs`
- Modify: `server/src/runtime/tests/mod.rs`
- Modify: `server/src/runtime/tests/support.rs`
- Modify: `server/src/runtime/tests/user_data.rs`
- Modify: `configs/bybit-testnet.demo.toml`
- Modify: `configs/binance-testnet.demo.toml`
- Modify: `configs/test.demo.toml`
- Modify: `README.md`
- Modify: `docs/protocol-contract.md`
- Modify: `tui/src/main.rs`
- Modify: `tui/src/views/instance.rs`
- Modify: `tui/tests/fixtures/track_detail_view.json`
- Modify: `tui/tests/fixtures/ws_track_detail_changed.json`
- Modify: `storage/src/schema.rs`
- Modify: `docs/superpowers/specs/2026-04-20-track-protection-state-model-design.md`
- Modify: `docs/superpowers/plans/2026-04-20-track-protection-state-model.md`

这些文件覆盖本计划的两个共享边界：

- Task 2 迁移 public policy/config 边界，包括后端配置、协议、公开文档、TUI、调参工具和 demo 配置；engine 同步切到 `BandProtectionPolicy`，但只保留按外层变体兼容旧行为的消费方式。
- Task 3 迁移 `TrackRuntimeSnapshot/runtime_state` 边界，并把 `BandRecoverPolicy + ReentryGuard`、`target_anchor` 和新的恢复状态机一起落地；包括 engine、storage、application 内所有直接持有、构造、持久化或传递 `TrackRuntimeSnapshot` 的模块；server 侧同步改为通过 application 公开 read model / service API 获取数据。
- Task 4 只做最终语义校验和任务清单同步；公共配置和协议文档已经在 Task 2 跟随边界迁移。

## Non-Goals

- 不新增对外 `SetTarget` 命令；`ManualState::TargetOverride` 只替换现有内部 generic override 表达。
- 不实现 legacy `track_snapshots` 迁移；旧库通过删表或重建数据库处理。
- 不把 `TrackState` 暴露到协议层；对外继续暴露现有 `TrackStatus` 枚举值。

### Task 1: 新增 core 契约，不替换现有共享函数

**Files:**
- Modify: `core/src/strategy.rs`
- Modify: `core/src/risk.rs`
- Test: `core/src/strategy.rs`
- Test: `core/src/risk.rs`

- [x] **Step 1: 先写失败测试，锁住新增契约**

```rust
#[test]
fn band_protection_policy_parses_flatten_with_price_confirm() {
    let policy: BandProtectionPolicy = serde_json::from_value(serde_json::json!({
        "flatten": {
            "recover": {
                "price_confirm": { "bps": 500 }
            }
        }
    }))
    .unwrap();

    assert!(matches!(
        policy,
        BandProtectionPolicy::Flatten {
            recover: BandRecoverPolicy::PriceConfirm { bps: 500 }
        }
    ));
}

#[test]
fn band_reentry_price_confirmation_is_boundary_specific() {
    assert!(band_reentry_price_confirmed(
        75_290.0,
        &BandRecoverPolicy::PriceConfirm { bps: 500 },
        75_000.0,
        80_800.0,
        BandBoundary::Below,
    ));
    assert!(!band_reentry_price_confirmed(
        80_700.0,
        &BandRecoverPolicy::PriceConfirm { bps: 500 },
        75_000.0,
        80_800.0,
        BandBoundary::Above,
    ));
}

#[test]
fn evaluate_risk_outcome_terminates_when_daily_loss_limit_is_breached() {
    let decision = evaluate_risk_outcome(
        &ExposureIntent {
            current: Exposure(4.0),
            target: Exposure(8.0),
            unit_notional: 375.0,
            loss_guard: LossGuardSnapshot {
                net_realized_pnl_today: -90.0,
                net_realized_pnl_cumulative: -90.0,
                unrealized_pnl: -35.0,
            },
        },
        &CapacityBudget {
            max_notional: 3_000.0,
            daily_loss_limit: 120.0,
            total_loss_limit: 500.0,
        },
    );

    assert_eq!(
        decision,
        RiskOutcome::Terminate(RiskTerminationCause::DailyLossLimit)
    );
}
```

- [x] **Step 2: 运行定向测试，确认当前实现失败**

Run:

- `cargo test -p poise-core strategy::tests::band_protection_policy_parses_flatten_with_price_confirm -- --exact`
- `cargo test -p poise-core strategy::tests::band_reentry_price_confirmation_is_boundary_specific -- --exact`
- `cargo test -p poise-core risk::tests::evaluate_risk_outcome_terminates_when_daily_loss_limit_is_breached -- --exact`

Expected:

- `BandProtectionPolicy`、`BandRecoverPolicy`、`RiskOutcome` 这些类型尚不存在
- `band_reentry_price_confirmed` 和 `evaluate_risk_outcome` 尚不存在
- 旧 `TrackConfig.out_of_band_policy`、`flatten_reentry_confirmed`、`evaluate_risk` 不变，因此不会触发跨 crate 半迁移

- [x] **Step 3: 新增 core 类型和新 helper，不修改旧共享函数**

`core/src/strategy.rs`：

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BandProtectionPolicy {
    Freeze { recover: BandRecoverPolicy },
    Hold,
    Flatten { recover: BandRecoverPolicy },
    Terminate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BandRecoverPolicy {
    BackInBand,
    PriceConfirm { bps: u32 },
}

pub fn band_reentry_price_confirmed(
    price: f64,
    recover: &BandRecoverPolicy,
    lower_price: f64,
    upper_price: f64,
    boundary: BandBoundary,
) -> bool {
    match recover {
        BandRecoverPolicy::BackInBand => price >= lower_price && price <= upper_price,
        BandRecoverPolicy::PriceConfirm { bps } => {
            let confirmation_distance = (upper_price - lower_price) * f64::from(*bps) / 10_000.0;
            match boundary {
                BandBoundary::Below => price >= lower_price + confirmation_distance,
                BandBoundary::Above => price <= upper_price - confirmation_distance,
            }
        }
    }
}
```

`core/src/risk.rs`：

```rust
#[derive(Debug, Clone, PartialEq)]
pub enum RiskOutcome {
    Allow { target: Exposure },
    Cap { target: Exposure },
    Terminate(RiskTerminationCause),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RiskTerminationCause {
    DailyLossLimit,
    TotalLossLimit,
}

pub fn evaluate_risk_outcome(intent: &ExposureIntent, budget: &CapacityBudget) -> RiskOutcome {
    let daily_loss_amount =
        (-(intent.loss_guard.net_realized_pnl_today + intent.loss_guard.unrealized_pnl)).max(0.0);
    let total_loss_amount =
        (-(intent.loss_guard.net_realized_pnl_cumulative + intent.loss_guard.unrealized_pnl)).max(0.0);

    if daily_loss_amount >= budget.daily_loss_limit {
        return RiskOutcome::Terminate(RiskTerminationCause::DailyLossLimit);
    }
    if total_loss_amount >= budget.total_loss_limit {
        return RiskOutcome::Terminate(RiskTerminationCause::TotalLossLimit);
    }

    let max_abs_exposure = budget.max_notional / intent.unit_notional;
    if intent.target.0.abs() > max_abs_exposure {
        return RiskOutcome::Cap {
            target: Exposure(intent.target.0.signum() * max_abs_exposure),
        };
    }

    RiskOutcome::Allow {
        target: intent.target,
    }
}
```

要求：

- 不修改 `TrackConfig.out_of_band_policy` 的字段类型
- 不修改旧 `flatten_reentry_confirmed` 签名
- 不修改旧 `evaluate_risk` 签名或返回类型
- 不修改 engine 消费方
- 不在第一版 `RiskOutcome` 增加账户容量不足分支；账户容量不足的 owner 在 Task 3 明确迁移到 `ExecutionGate / AccountCapacityGate`

- [x] **Step 4: 运行 Task 1 回归**

Run:

- `cargo test -p poise-core strategy::tests::band_protection_policy_parses_flatten_with_price_confirm -- --exact`
- `cargo test -p poise-core strategy::tests::band_reentry_price_confirmation_is_boundary_specific -- --exact`
- `cargo test -p poise-core risk::tests::evaluate_risk_outcome_terminates_when_daily_loss_limit_is_breached -- --exact`

Expected:

- 新 core 契约可编译
- 旧共享函数仍可被现有 engine 调用
- Task 1 结束时没有半迁移共享边界

- [x] **Step 5: Commit**

```bash
git add core/src/strategy.rs core/src/risk.rs
git commit -m "refactor: introduce band policy and risk outcome contracts"
```

Recorded commit: `7caa270`

### Task 2: 迁移 `BandProtectionPolicy` 配置边界和全部消费者

**Files:**
- Modify: `core/src/strategy.rs`
- Modify: `core/src/events.rs`
- Modify: `protocol/src/lib.rs`
- Modify: `application/src/track_definition.rs`
- Modify: `application/src/read_model.rs`
- Modify: `application/src/query_service.rs`
- Modify: `application/src/debug_query_service.rs`
- Modify: `application/src/mutation_executor.rs`
- Modify: `engine/src/reconciler.rs`
- Modify: `engine/src/runtime.rs`
- Modify: `engine/src/manager.rs`
- Modify: `engine/src/snapshot.rs`
- Modify: `engine/src/persisted_runtime.rs`
- Modify: `storage/src/sqlite.rs`
- Modify: `server/src/config.rs`
- Modify: `server/src/projector.rs`
- Modify: `server/src/http.rs`
- Modify: `server/src/websocket.rs`
- Modify: `server/src/assembly.rs`
- Modify: `server/src/exchange_startup.rs`
- Modify: `server/src/state_bootstrap.rs`
- Modify: `server/src/main.rs`
- Modify: `server/src/test_support.rs`
- Modify: `server/src/effect_worker/tests/mod.rs`
- Modify: `server/src/effect_worker/tests/retry.rs`
- Modify: `server/src/effect_worker/tests/support.rs`
- Modify: `server/src/runtime/tests/mod.rs`
- Modify: `server/src/runtime/tests/support.rs`
- Modify: `server/src/runtime/tests/user_data.rs`
- Modify: `configs/bybit-testnet.demo.toml`
- Modify: `configs/binance-testnet.demo.toml`
- Modify: `configs/test.demo.toml`
- Modify: `README.md`
- Modify: `docs/protocol-contract.md`
- Modify: `tui/src/main.rs`
- Modify: `tui/src/views/instance.rs`
- Modify: `tui/tests/fixtures/track_detail_view.json`
- Modify: `tui/tests/fixtures/ws_track_detail_changed.json`
- Modify: `tools/track-tuning-workbench/src/domain/trackDraft.ts`
- Modify: `tools/track-tuning-workbench/src/domain/trackCurve.ts`
- Modify: `tools/track-tuning-workbench/src/domain/trackValidation.ts`
- Modify: `tools/track-tuning-workbench/src/domain/trackCurvePath.test.ts`
- Modify: `tools/track-tuning-workbench/src/domain/trackFixtures.test.ts`
- Modify: `tools/track-tuning-workbench/src/app/AppShell.tsx`
- Modify: `tools/track-tuning-workbench/src/app/workbenchBridge.ts`
- Modify: `tools/track-tuning-workbench/src/app/workbenchBridge.test.ts`
- Modify: `tools/track-tuning-workbench/src/state/workbenchStore.test.ts`
- Modify: `tools/track-tuning-workbench/src/ui/app/AppShell.test.tsx`
- Modify: `tools/track-tuning-workbench/src/ui/app/useSelectedTrackWorkbench.test.tsx`
- Modify: `tools/track-tuning-workbench/src/ui/editor/TrackEditor.tsx`
- Modify: `tools/track-tuning-workbench/src/ui/editor/sections/RiskSection.tsx`
- Modify: `tools/track-tuning-workbench/src-tauri/src/config_document.rs`
- Modify: `tools/track-tuning-workbench/src-tauri/src/config_projection.rs`
- Modify: `tools/track-tuning-workbench/src-tauri/src/commands.rs`
- Test: `core/src/strategy.rs`
- Test: `application/src/track_definition.rs`
- Test: `server/src/config.rs`
- Test: `server/src/projector.rs`
- Test: `engine/src/reconciler.rs`
- Test: `tui/src/views/instance.rs`
- Test: `tools/track-tuning-workbench/src-tauri/src/config_document.rs`
- Test: `tools/track-tuning-workbench/src-tauri/src/commands.rs`
- Test: `tools/track-tuning-workbench/src/app/workbenchBridge.test.ts`

- [x] **Step 1: 写失败测试，锁住配置边界一次性迁移**

```rust
#[test]
fn track_config_accepts_flatten_price_confirm_policy() {
    let config = TrackConfig {
        lower_price: 75_000.0,
        upper_price: 80_800.0,
        long_exposure_units: 8.0,
        short_exposure_units: 8.0,
        notional_per_unit: 375.0,
        min_rebalance_units: 0.5,
        shape_family: ShapeFamily::Linear,
        out_of_band_policy: BandProtectionPolicy::Flatten {
            recover: BandRecoverPolicy::PriceConfirm { bps: 500 },
        },
    };

    assert!(matches!(
        config.out_of_band_policy,
        BandProtectionPolicy::Flatten {
            recover: BandRecoverPolicy::PriceConfirm { bps: 500 }
        }
    ));
}

#[test]
fn config_toml_parses_flatten_price_confirm_policy() {
    let raw = r#"
[[tracks]]
id = "btc-core"
symbol = "BTC-USDT-SWAP"
lower_price = 75000
upper_price = 80800
long_exposure_units = 8
short_exposure_units = 8
notional_per_unit = 375
out_of_band_policy = { flatten = { recover = { price_confirm = { bps = 500 } } } }
"#;

    let config: AppConfig = toml::from_str(raw).unwrap();

    assert!(matches!(
        config.tracks[0].out_of_band_policy,
        Some(BandProtectionPolicy::Flatten {
            recover: BandRecoverPolicy::PriceConfirm { bps: 500 }
        })
    ));
}

#[test]
fn projector_shows_flatten_price_confirm_policy_without_engine_state() {
    let mut source = source_with_failed_effect_and_recent_event();
    source.status = TrackStatus::Active;
    source.out_of_band_policy = BandProtectionPolicy::Flatten {
        recover: BandRecoverPolicy::PriceConfirm { bps: 500 },
    };

    let detail = TrackProjector::new().project_detail(&source);

    assert_eq!(
        serde_json::to_value(detail.strategy.out_of_band_policy).unwrap(),
        serde_json::json!({ "flatten": { "recover": { "price_confirm": { "bps": 500 } } } })
    );
}
```

- [x] **Step 2: 运行定向测试，确认当前实现失败**

Run:

- `cargo test -p poise-core strategy::tests::track_config_accepts_flatten_price_confirm_policy -- --exact`
- `cargo test -p poise-server config::tests::config_toml_parses_flatten_price_confirm_policy -- --exact`
- `cargo test -p poise-server projector::tests::projector_shows_flatten_price_confirm_policy_without_engine_state -- --exact`
- `cargo test -p poise-tui views::instance::tests::renders_flatten_out_of_band_policy_name -- --exact`
- `cargo test -p poise-track-tuning-workbench config_document::tests::export_explicitly_writes_supported_defaults -- --exact`
- `pnpm --dir tools/track-tuning-workbench test -- workbenchBridge.test.ts`

Expected:

- `TrackConfig.out_of_band_policy` 仍是旧 `OutOfBandPolicy`
- protocol / config / projector / TUI / workbench 仍只支持 `"freeze" | "hold" | "flatten" | "terminate"`
- engine 还没有切到新的 `BandProtectionPolicy` 形状

- [x] **Step 3: 一次性迁移 `OutOfBandPolicy` 直接消费者**

`core/src/strategy.rs`：

```rust
pub struct TrackConfig {
    pub lower_price: f64,
    pub upper_price: f64,
    pub long_exposure_units: f64,
    pub short_exposure_units: f64,
    pub notional_per_unit: f64,
    pub min_rebalance_units: f64,
    pub shape_family: ShapeFamily,
    pub out_of_band_policy: BandProtectionPolicy,
}

pub enum BandStatus {
    InBand { target: Exposure },
    OutOfBand {
        policy: BandProtectionPolicy,
        boundary: BandBoundary,
    },
}
```

`engine/src/reconciler.rs`：

```rust
fn apply_out_of_band(
    track: &TrackRuntime,
    policy: BandProtectionPolicy,
    boundary: BandBoundary,
) -> BandTargetDecision {
    let frozen_target = track
        .desired_exposure
        .clone()
        .unwrap_or_else(|| track.current_exposure.clone());

    match policy {
        BandProtectionPolicy::Freeze { .. } => BandTargetDecision {
            target: frozen_target,
            flatten_reentry_boundary: None,
            new_status: Some(TrackStatus::Frozen),
        },
        BandProtectionPolicy::Hold => BandTargetDecision {
            target: frozen_target,
            flatten_reentry_boundary: None,
            new_status: Some(TrackStatus::Holding),
        },
        BandProtectionPolicy::Flatten { .. } => BandTargetDecision {
            target: Exposure(0.0),
            flatten_reentry_boundary: Some(boundary),
            new_status: Some(TrackStatus::Flattening),
        },
        BandProtectionPolicy::Terminate => terminate_for_band_breach(track),
    }
}
```

要求：

- 删除旧 `OutOfBandPolicy`，或只在 protocol compatibility 测试中通过 serde alias 读取旧字符串，不保留运行时双类型
- `freeze` 的旧字符串配置需要规范化为 `BandProtectionPolicy::Freeze { recover: BandRecoverPolicy::BackInBand }`
- `flatten` 的旧字符串配置需要规范化为 `BandProtectionPolicy::Flatten { recover: BandRecoverPolicy::PriceConfirm { bps: 500 } }`
- protocol / application / server / storage / engine / TUI / workbench 中不再导入旧 `OutOfBandPolicy` 或只处理旧字符串形态
- Task 2 的 engine 只允许按 policy 外层变体兼容到旧 `Frozen` / `Holding` / `Flattening` / `Terminated` 行为，不得提前引入 `freeze_with_reentry_guard`、`hold_with_target_anchor`、`flatten_with_reentry_guard` 这类新状态机 helper
- demo 配置和 fixture 使用新的嵌套 policy 形状，不再写 `out_of_band_policy = "freeze"` 这种运行时配置形态
- README 中的配置示例改为 `out_of_band_policy = { flatten = { recover = { price_confirm = { bps = 500 } } } }`
- `docs/protocol-contract.md` 中 `strategy.out_of_band_policy` 的稳定形状改为嵌套 policy object，不再写成 `"freeze" | "hold" | "flatten" | "terminate"` 字符串枚举；Task 2 只描述公开字段形状，不在这里提前承诺 `target_anchor` / `ReentryGuard` 运行时语义
- 用 `rg -n 'OutOfBandPolicy|TrackOutOfBandPolicy|out_of_band_policy = "|outOfBandPolicy.*freeze|outOfBandPolicy.*flatten' core protocol application engine storage server tui tools configs` 确认旧类型、旧字符串配置和旧字符串 union 已迁移；不要把仍然合法的字段名 `out_of_band_policy` 当成失败

- [x] **Step 4: 运行 Task 2 回归**

Run:

- `cargo test -p poise-core strategy::tests::track_config_accepts_flatten_price_confirm_policy -- --exact`
- `cargo test -p poise-server config::tests::config_toml_parses_flatten_price_confirm_policy -- --exact`
- `cargo test -p poise-server projector::tests::projector_shows_flatten_price_confirm_policy_without_engine_state -- --exact`
- `cargo test -p poise-engine flattening -- --nocapture`
- `cargo test -p poise-tui views::instance::tests::renders_flatten_out_of_band_policy_name -- --exact`
- `cargo test -p poise-track-tuning-workbench config_document::tests::export_explicitly_writes_supported_defaults -- --exact`
- `pnpm --dir tools/track-tuning-workbench test -- workbenchBridge.test.ts`

Expected:

- `TrackConfig.out_of_band_policy` 和所有直接消费者已经同 task 迁移完成
- 没有旧 `OutOfBandPolicy` 运行时消费者残留
- TUI、workbench、demo configs 和 fixtures 不再依赖旧字符串 policy 形状
- README 和 protocol contract 与新的 public policy 形状一致
- engine 已经能消费新的 `BandProtectionPolicy`，但仍只按外层变体兼容旧带外行为
- `recover` 配置、`ReentryGuard` 和记忆化 `target_anchor` 的运行时语义尚未在 Task 2 落地，它们由 Task 3 一次性接手

- [x] **Step 5: Commit**

```bash
git add core/src/strategy.rs core/src/events.rs protocol/src/lib.rs application/src/track_definition.rs application/src/read_model.rs application/src/query_service.rs application/src/debug_query_service.rs application/src/mutation_executor.rs engine/src/reconciler.rs engine/src/runtime.rs engine/src/manager.rs engine/src/snapshot.rs engine/src/persisted_runtime.rs storage/src/sqlite.rs server/src/config.rs server/src/projector.rs server/src/http.rs server/src/websocket.rs server/src/assembly.rs server/src/exchange_startup.rs server/src/state_bootstrap.rs server/src/main.rs server/src/test_support.rs server/src/effect_worker/tests/mod.rs server/src/effect_worker/tests/retry.rs server/src/effect_worker/tests/support.rs server/src/runtime/tests/mod.rs server/src/runtime/tests/support.rs server/src/runtime/tests/user_data.rs configs/bybit-testnet.demo.toml configs/binance-testnet.demo.toml configs/test.demo.toml README.md docs/protocol-contract.md tui/src/main.rs tui/src/views/instance.rs tui/tests/fixtures/track_detail_view.json tui/tests/fixtures/ws_track_detail_changed.json tools/track-tuning-workbench/src/domain/trackDraft.ts tools/track-tuning-workbench/src/domain/trackCurve.ts tools/track-tuning-workbench/src/domain/trackValidation.ts tools/track-tuning-workbench/src/domain/trackCurvePath.test.ts tools/track-tuning-workbench/src/domain/trackFixtures.test.ts tools/track-tuning-workbench/src/app/AppShell.tsx tools/track-tuning-workbench/src/app/workbenchBridge.ts tools/track-tuning-workbench/src/app/workbenchBridge.test.ts tools/track-tuning-workbench/src/state/workbenchStore.test.ts tools/track-tuning-workbench/src/ui/app/AppShell.test.tsx tools/track-tuning-workbench/src/ui/app/useSelectedTrackWorkbench.test.tsx tools/track-tuning-workbench/src/ui/editor/TrackEditor.tsx tools/track-tuning-workbench/src/ui/editor/sections/RiskSection.tsx tools/track-tuning-workbench/src-tauri/src/config_document.rs tools/track-tuning-workbench/src-tauri/src/config_projection.rs tools/track-tuning-workbench/src-tauri/src/commands.rs
git commit -m "refactor: migrate band protection policy boundary"
```

Recorded commit: `924c1dd`

### Task 3: 迁移 `TrackState/runtime_state` 和完整 snapshot 边界

**Files:**
- Modify: `core/src/events.rs`
- Create: `engine/src/execution_gate.rs`
- Modify: `engine/src/price_gate.rs`
- Modify: `engine/src/lib.rs`
- Modify: `engine/src/runtime.rs`
- Modify: `engine/src/reconciler.rs`
- Modify: `engine/src/manager.rs`
- Modify: `engine/src/snapshot.rs`
- Modify: `engine/src/persisted_runtime.rs`
- Modify: `engine/src/transition.rs`
- Modify: `storage/src/schema.rs`
- Modify: `storage/src/sqlite.rs`
- Modify: `application/src/lib.rs`
- Create: `application/src/test_support.rs`
- Modify: `application/src/track_read_source.rs`
- Modify: `application/src/read_model.rs`
- Modify: `application/src/query_service.rs`
- Modify: `application/src/debug_query_service.rs`
- Modify: `application/src/track_mutation_store.rs`
- Modify: `application/src/track_persistence.rs`
- Modify: `application/src/mutation_executor.rs`
- Modify: `server/src/http.rs`
- Modify: `server/src/websocket.rs`
- Modify: `server/src/assembly.rs`
- Modify: `server/src/state_bootstrap.rs`
- Modify: `server/src/projector.rs`
- Modify: `server/src/runtime/reconcile.rs`
- Modify: `server/src/effect_worker/tests/mod.rs`
- Modify: `server/src/effect_worker/tests/support.rs`
- Modify: `server/src/runtime/tests/mod.rs`
- Modify: `server/src/runtime/tests/support.rs`
- Modify: `server/src/runtime/tests/user_data.rs`
- Test: `engine/src/execution_gate.rs`
- Test: `engine/src/runtime.rs`
- Test: `engine/src/reconciler.rs`
- Test: `engine/src/manager.rs`
- Test: `engine/src/persisted_runtime.rs`
- Test: `storage/src/sqlite.rs`
- Test: `application/src/test_support.rs`
- Test: `application/src/track_read_source.rs`
- Test: `application/src/query_service.rs`
- Test: `application/src/mutation_executor.rs`
- Test: `server/src/http.rs`
- Test: `server/src/websocket.rs`
- Test: `server/src/assembly.rs`
- Test: `server/src/state_bootstrap.rs`
- Test: `server/src/projector.rs`

- [ ] **Step 1: 写失败测试，锁住主状态、持久化根对象和单点投影**

```rust
#[test]
fn snapshot_round_trips_runtime_track_state() {
    let mut runtime = test_runtime();
    runtime.track_state = TrackState::Running(ControlState::Automatic(AutoState::Flattening {
        guard: ReentryGuard {
            boundary: BandBoundary::Below,
        },
    }));

    let snapshot = runtime.snapshot();
    let restored = PersistedRuntimeCodec::decode(
        PersistedRuntimeCodec::encode_snapshot(&snapshot).unwrap(),
    )
    .unwrap();

    assert_eq!(restored.runtime_state, snapshot.runtime_state);
}

#[test]
fn reconcile_target_terminates_when_risk_requests_termination() {
    let mut track = test_runtime();
    track.current_exposure = Exposure(4.0);
    track.risk_state.unrealized_pnl = -35.0;
    track.ledger_state.gross_realized_pnl_cumulative = -90.0;
    track.ledger_state.trading_fee_cumulative = 0.0;

    let result = reconcile_target(&track, 95.0);

    assert_eq!(result.desired_exposure, Exposure(0.0));
    assert_eq!(
        result.new_runtime_state,
        Some(TrackState::Terminated {
            cause: TerminationCause::Risk(RiskTerminationCause::DailyLossLimit),
        }),
    );
}

#[test]
fn freeze_samples_target_anchor_from_last_risk_approved_target() {
    let mut track = test_runtime_with_strategy_target(Exposure(4.0));
    track.current_exposure = Exposure(1.0);
    track.config.out_of_band_policy = BandProtectionPolicy::Freeze {
        recover: BandRecoverPolicy::PriceConfirm { bps: 500 },
    };

    let result = reconcile_target(&track, 74_900.0);

    assert_eq!(
        result.new_runtime_state,
        Some(TrackState::Running(ControlState::Automatic(AutoState::Frozen {
            target_anchor: Exposure(4.0),
            guard: ReentryGuard {
                boundary: BandBoundary::Below,
            },
        }))),
    );
    assert_eq!(result.desired_exposure, Exposure(4.0));
}

#[test]
fn frozen_reentry_clears_target_anchor_and_follows_current_strategy_target() {
    let mut track = test_runtime();
    track.track_state = TrackState::Running(ControlState::Automatic(AutoState::Frozen {
        target_anchor: Exposure(4.0),
        guard: ReentryGuard {
            boundary: BandBoundary::Below,
        },
    }));
    track.config.out_of_band_policy = BandProtectionPolicy::Freeze {
        recover: BandRecoverPolicy::PriceConfirm { bps: 500 },
    };

    let result = reconcile_target(&track, 75_400.0);

    assert_eq!(
        result.new_runtime_state,
        Some(TrackState::Running(ControlState::Automatic(
            AutoState::FollowingBand,
        ))),
    );
    assert_eq!(result.desired_exposure, strategy_target_at(75_400.0));
}

#[test]
fn hold_samples_target_anchor_from_last_risk_approved_target() {
    let mut track = test_runtime_with_strategy_target(Exposure(4.0));
    track.current_exposure = Exposure(1.0);
    track.config.out_of_band_policy = BandProtectionPolicy::Hold;

    let result = reconcile_target(&track, 74_900.0);

    assert_eq!(
        result.new_runtime_state,
        Some(TrackState::Running(ControlState::Automatic(AutoState::Holding {
            target_anchor: Exposure(4.0),
        }))),
    );
    assert_eq!(result.desired_exposure, Exposure(4.0));
}

#[test]
fn holding_keeps_target_anchor_when_price_reenters_band() {
    let mut track = test_runtime_with_strategy_target(Exposure(2.0));
    track.track_state = TrackState::Running(ControlState::Automatic(AutoState::Holding {
        target_anchor: Exposure(4.0),
    }));
    track.config.out_of_band_policy = BandProtectionPolicy::Hold;

    let result = reconcile_target(&track, 75_400.0);

    assert_eq!(result.new_runtime_state, None);
    assert_eq!(result.desired_exposure, Exposure(4.0));
}

#[test]
fn resume_from_holding_clears_target_anchor_and_recomputes_following_band() {
    let mut manager = test_manager_with_cached_strategy_price(95.0);
    let track_id = TrackId::new("btc-core");

    let track = manager.tracks.get_mut(&track_id).unwrap();
    track.track_state = TrackState::Running(ControlState::Automatic(AutoState::Holding {
        target_anchor: Exposure(4.0),
    }));
    track.current_exposure = Exposure(4.0);

    manager.resume_track("btc-core").unwrap();

    let track = manager.get_track("btc-core").unwrap();
    assert_eq!(
        track.track_state,
        TrackState::Running(ControlState::Automatic(AutoState::FollowingBand)),
    );
    assert_eq!(track.desired_exposure, Some(strategy_target_at(95.0)));
}

#[test]
fn account_capacity_gate_blocks_increase_without_risk_outcome() {
    let decision = AccountCapacityGate::evaluate(AccountCapacityGateInput {
        current: Exposure(2.0),
        approved_target: Exposure(6.0),
        unit_notional: 375.0,
        available_notional: Some(1_000.0),
    });

    assert_eq!(
        decision,
        ExecutionGateDecision::NoSubmit {
            reason: ExecutionGateReason::AccountCapacityInsufficient {
                required_notional: 1_500.0,
                available_notional: 1_000.0,
            },
        },
    );
}

#[test]
fn reconcile_reports_account_capacity_as_execution_gate_not_risk() {
    let mut track = test_runtime();
    track.current_exposure = Exposure(2.0);
    track.execution_gate_state.account_capacity.available_notional = Some(1_000.0);

    let result = reconcile_target(&track, 95.0);

    assert!(result.applied_risk_cap.is_none());
    assert_eq!(
        result.execution_gate_decision,
        ExecutionGateDecision::NoSubmit {
            reason: ExecutionGateReason::AccountCapacityInsufficient {
                required_notional: 1_500.0,
                available_notional: 1_000.0,
            },
        },
    );
    assert_eq!(
        result.events,
        vec![DomainEvent::ExecutionGateApplied {
            reason: ExecutionGateReason::AccountCapacityInsufficient {
                required_notional: 1_500.0,
                available_notional: 1_000.0,
            },
        }],
    );
}

#[test]
fn application_test_support_projects_private_runtime_seed_to_public_read_model() {
    let read_model = TrackReadModelFixture::manual_flattening("fixture").build();

    assert_eq!(read_model.status, TrackStatus::ManualFlattening);
}

#[test]
fn read_source_derives_manual_flattening_status_from_runtime_state() {
    let snapshot = test_snapshot_with_runtime_state(
        TrackState::Running(ControlState::Manual(ManualState::Flattened)),
    );

    let source = TrackRuntimeReadState::from_snapshot(snapshot, false);

    assert_eq!(source.status, TrackStatus::ManualFlattening);
}

#[test]
fn projector_available_commands_follow_public_status_only() {
    let read_model = test_read_model_with_status(TrackStatus::Paused);
    let detail = TrackProjector::new().project_detail(&read_model);

    assert!(detail
        .available_commands
        .iter()
        .any(|command| command.command == TrackCommandType::Resume && command.enabled));
}
```

- [ ] **Step 2: 运行定向测试，确认当前实现失败**

Run:

- `cargo test -p poise-engine snapshot_round_trips_runtime_track_state -- --exact`
- `cargo test -p poise-engine freeze_samples_target_anchor_from_last_risk_approved_target -- --exact`
- `cargo test -p poise-engine frozen_reentry_clears_target_anchor_and_follows_current_strategy_target -- --exact`
- `cargo test -p poise-engine hold_samples_target_anchor_from_last_risk_approved_target -- --exact`
- `cargo test -p poise-engine holding_keeps_target_anchor_when_price_reenters_band -- --exact`
- `cargo test -p poise-engine resume_from_holding_clears_target_anchor_and_recomputes_following_band -- --exact`
- `cargo test -p poise-engine account_capacity_gate_blocks_increase_without_risk_outcome -- --exact`
- `cargo test -p poise-engine reconcile_reports_account_capacity_as_execution_gate_not_risk -- --exact`
- `cargo test -p poise-engine reconcile_target_terminates_when_risk_requests_termination -- --exact`
- `cargo test -p poise-application read_model::tests::read_model_from_snapshot_flattens_runtime_state -- --exact`
- `cargo test -p poise-application track_read_source::tests::read_source_derives_manual_flattening_status_from_runtime_state -- --exact`
- `cargo test -p poise-application query_service::tests::load_track_recovery_view_projects_runtime_recovery_summary -- --exact`
- `cargo test -p poise-application track_command_service::tests::restore_persisted_track_state_rehydrates_manager_from_store -- --exact`
- `cargo test -p poise-server projector_available_commands_follow_public_status_only -- --exact`
- `cargo test -p poise-server runtime::tests::execution::insufficient_margin_guard_blocks_follow_up_submit_after_market_tick -- --exact`
- `cargo test -p poise-server runtime::tests::startup::startup_bootstrap_restores_claimed_live_order_before_first_tick -- --exact`
- `cargo test -p poise-server runtime::tests::startup::recovery_task_resyncs_recovery_anomaly_automatically_without_user_data -- --exact`
- `cargo test -p poise-server runtime::tests::reconcile::apply_user_data_event_preserves_write_service_mutation_error_kind -- --exact`

Expected:

- `TrackState`、`ControlState`、`ManualState`、`ReentryGuard` 这些类型尚不存在
- `target_anchor` 语义尚未实现，`Frozen` / `Holding` 仍缺少明确采样、保留和清除规则
- application test-support 还缺少不泄漏私有 snapshot 的 read model seed 构造边界
- `runtime.snapshot()` 仍在输出旧的 `status / manual_target_override / flatten_reentry_boundary`
- read source 还没有单一适配层把 `runtime_state` 投影成公开状态
- account capacity 仍通过 risk 命名的 state/event 表达，还没有迁移到 execution gate owner
- projector 测试夹具还没有完全和公开 `TrackStatus` 输入对齐
- server 的 HTTP、websocket、assembly、bootstrap 边界还没有从直接 snapshot 依赖迁移到 application 公开 read model / service API

- [ ] **Step 3: 建立 engine 主状态并迁移 target / command 逻辑**

`engine/src/runtime.rs`：

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TrackState {
    WaitingMarketData,
    Running(ControlState),
    Paused { suspended: ControlState },
    Terminated { cause: TerminationCause },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ControlState {
    Automatic(AutoState),
    Manual(ManualState),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AutoState {
    FollowingBand,
    Frozen {
        target_anchor: Exposure,
        guard: ReentryGuard,
    },
    Holding {
        target_anchor: Exposure,
    },
    Flattening { guard: ReentryGuard },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ManualState {
    Flattened,
    TargetOverride { target: Exposure },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReentryGuard {
    pub boundary: BandBoundary,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TerminationCause {
    ManualCommand,
    Band(BandTerminationCause),
    Risk(RiskTerminationCause),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum BandTerminationCause {
    OutOfRange,
}
```

`engine/src/reconciler.rs`：

```rust
fn apply_out_of_band(
    track: &TrackRuntime,
    policy: BandProtectionPolicy,
    boundary: BandBoundary,
) -> BandTargetDecision {
    match policy {
        BandProtectionPolicy::Freeze { recover } => {
            freeze_with_reentry_guard(track, recover, boundary)
        }
        BandProtectionPolicy::Hold => hold_with_target_anchor(track),
        BandProtectionPolicy::Flatten { recover } => {
            flatten_with_reentry_guard(track, recover, boundary)
        }
        BandProtectionPolicy::Terminate => terminate_for_band_breach(track),
    }
}
```

要求：

- `TrackState` 取代旧的顶层 `status`
- 删除顶层 `manual_target_override`
- 删除顶层 `flatten_reentry_boundary`
- `desired_exposure` 只作为派生缓存
- `reconciler` 改用 `evaluate_risk_outcome`
- risk terminate 映射成 `TrackState::Terminated { cause: TerminationCause::Risk(...) }`
- `ManualState::TargetOverride` 先只作为内部状态形状，不新增公开命令
- 用新的 `TrackState` / `ReentryGuard` / `target_anchor` 替换 Task 2 中按 policy 外层变体兼容旧行为的 engine 分支
- `Freeze { recover }` / `Flatten { recover }` 在本 task 才真正开始消费 `recover`，不再依赖旧 `flatten_reentry_boundary` 或固定确认常量
- `Hold` 在本 task 才切换到 `target_anchor` 语义，不再复用旧的 `desired_exposure.unwrap_or(current_exposure)` 兼容逻辑
- `Frozen` / `Holding` 的 `target_anchor` 在进入保护状态的同一次 reconcile 中，从 risk-approved target 采样；恢复或人工切换时清除
- `target_anchor` 只作为保护期间的派生目标来源，不作为 risk cap 结果回写点
- account capacity 从 `RiskState.account_capacity_constraint` 迁移到独立 `ExecutionGateState.account_capacity`
- account capacity 不再产生 `DomainEvent::RiskDenied`；如果需要事件，使用 execution gate 语义事件

- [ ] **Step 4: 同 task 切换 snapshot、storage、read adapter 和 projector 输入**

`core/src/events.rs`：

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionGateReason {
    AccountCapacityInsufficient {
        required_notional: f64,
        available_notional: f64,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DomainEvent {
    ExposureTargetChanged { from: Exposure, to: Exposure },
    BandBreached { boundary: BandBoundary, price: f64 },
    BandReentered { price: f64 },
    PolicyTriggered { policy: BandProtectionPolicy },
    RiskCapApplied { intended: Exposure, capped: Exposure },
    ExecutionGateApplied { reason: ExecutionGateReason },
    ReplacementGateApplied { reason: ReplacementGateReason },
}
```

`engine/src/execution_gate.rs`：

```rust
use poise_core::events::ExecutionGateReason;
use poise_core::types::Exposure;

use crate::price_gate::PriceExecutionBlockReason;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecutionGateState {
    pub account_capacity: AccountCapacityGateState,
    pub price_execution_block_reason: Option<PriceExecutionBlockReason>,
    pub last_decision: ExecutionGateDecision,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountCapacityGateState {
    pub available_notional: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountCapacityGateInput {
    pub current: Exposure,
    pub approved_target: Exposure,
    pub unit_notional: f64,
    pub available_notional: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ExecutionGateDecision {
    Open,
    NoSubmit { reason: ExecutionGateReason },
}

pub struct AccountCapacityGate;

impl ExecutionGateState {
    pub fn open() -> Self {
        Self {
            account_capacity: AccountCapacityGateState {
                available_notional: None,
            },
            price_execution_block_reason: None,
            last_decision: ExecutionGateDecision::Open,
        }
    }
}

impl AccountCapacityGate {
    pub fn evaluate(input: AccountCapacityGateInput) -> ExecutionGateDecision {
        let Some(available_notional) = input.available_notional else {
            return ExecutionGateDecision::Open;
        };
        let increase_units = (input.approved_target.0.abs() - input.current.0.abs()).max(0.0);
        let required_notional = increase_units * input.unit_notional;

        if required_notional > available_notional {
            return ExecutionGateDecision::NoSubmit {
                reason: ExecutionGateReason::AccountCapacityInsufficient {
                    required_notional,
                    available_notional,
                },
            };
        }

        ExecutionGateDecision::Open
    }
}
```

`engine/src/snapshot.rs`：

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackRuntimeSnapshot {
    pub track_id: TrackId,
    pub restore_revision: TrackRestoreRevision,
    pub runtime_state: TrackState,
    pub current_exposure: Exposure,
    pub desired_exposure: Option<Exposure>,
    pub executor_state: ExecutorState,
    pub replacement_gate_reason: Option<ReplacementGateReason>,
    pub execution_gate_state: ExecutionGateState,
    pub ledger_state: TrackLedgerState,
    pub risk: RiskState,
    pub observed: ObservedState,
}
```

`application/src/lib.rs`：

```rust
mod track_read_source;

#[cfg(any(test, feature = "server-test-support"))]
#[doc(hidden)]
pub mod test_support;

#[cfg(feature = "server-test-support")]
#[doc(hidden)]
pub use test_support::TrackReadModelFixture;
```

`application/src/test_support.rs`：

```rust
use chrono::Utc;
use poise_core::risk::CapacityBudget;
use poise_core::strategy::{
    BandBoundary, BandProtectionPolicy, BandRecoverPolicy, ShapeFamily, TrackConfig,
};
use poise_core::types::Exposure;
use poise_engine::execution_gate::ExecutionGateState;
use poise_engine::ledger::TrackLedgerState;
use poise_engine::persisted_runtime::TrackRestoreRevision;
use poise_engine::runtime::{
    AutoState, ControlState, ExecutorState, ManualState, ReentryGuard, RiskState, TrackState,
};
use poise_engine::snapshot::{ObservedState, TrackRuntimeSnapshot};
use poise_engine::track::{Instrument, TrackId, Venue};

use crate::track_read_source::{TrackReadSource, TrackRuntimeReadState};
use crate::{TrackReadDefinition, TrackReadModel};

#[doc(hidden)]
pub struct TrackReadModelFixture {
    read_model: TrackReadModel,
}

impl TrackReadModelFixture {
    pub fn paused(track_id: &str) -> Self {
        Self::from_runtime_state(
            track_id,
            TrackState::Paused {
                suspended: ControlState::Automatic(AutoState::FollowingBand),
            },
        )
    }

    pub fn manual_flattening(track_id: &str) -> Self {
        Self::from_runtime_state(
            track_id,
            TrackState::Running(ControlState::Manual(ManualState::Flattened)),
        )
    }

    pub fn automatic_flattening_below(track_id: &str) -> Self {
        Self::from_runtime_state(
            track_id,
            TrackState::Running(ControlState::Automatic(AutoState::Flattening {
                guard: ReentryGuard {
                    boundary: BandBoundary::Below,
                },
            })),
        )
    }

    pub fn with_ledger_state(mut self, ledger_state: TrackLedgerState) -> Self {
        self.read_model.ledger_state = ledger_state;
        self
    }

    pub fn build(self) -> TrackReadModel {
        self.read_model
    }

    fn from_runtime_state(track_id: &str, runtime_state: TrackState) -> Self {
        Self {
            read_model: project_read_model(track_id, runtime_state),
        }
    }
}

fn project_read_model(track_id: &str, runtime_state: TrackState) -> TrackReadModel {
    let snapshot = runtime_snapshot(track_id, runtime_state);
    let definition = test_read_definition(snapshot.track_id.clone());

    TrackReadModel::from_source(TrackReadSource {
        definition,
        runtime: TrackRuntimeReadState::from_snapshot(snapshot, false),
        updated_at: Utc::now(),
        recent_track_events: Vec::new(),
        recent_effects: Vec::new(),
    })
}

fn runtime_snapshot(track_id: &str, runtime_state: TrackState) -> TrackRuntimeSnapshot {
    let now = Utc::now();
    TrackRuntimeSnapshot {
        track_id: TrackId::new(track_id),
        restore_revision: TrackRestoreRevision::from_stored("fixture"),
        runtime_state,
        current_exposure: Exposure(0.0),
        desired_exposure: None,
        executor_state: ExecutorState::empty(now),
        replacement_gate_reason: None,
        execution_gate_state: ExecutionGateState::open(),
        ledger_state: TrackLedgerState::default(),
        risk: RiskState::default(),
        observed: ObservedState::default(),
    }
}

fn test_read_definition(track_id: TrackId) -> TrackReadDefinition {
    TrackReadDefinition {
        track_id,
        instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
        track_config: test_track_config(),
        budget: test_budget(),
    }
}

fn test_track_config() -> TrackConfig {
    TrackConfig {
        lower_price: 75_000.0,
        upper_price: 80_800.0,
        long_exposure_units: 8.0,
        short_exposure_units: 8.0,
        notional_per_unit: 375.0,
        min_rebalance_units: 0.5,
        shape_family: ShapeFamily::Linear,
        out_of_band_policy: BandProtectionPolicy::Flatten {
            recover: BandRecoverPolicy::PriceConfirm { bps: 500 },
        },
    }
}

fn test_budget() -> CapacityBudget {
    CapacityBudget {
        max_notional: 3_000.0,
        daily_loss_limit: 100.0,
        total_loss_limit: 300.0,
    }
}
```

`storage/src/schema.rs`：

```rust
const TRACK_SNAPSHOTS_CREATE_SQL: &str = "CREATE TABLE track_snapshots (
    track_id TEXT PRIMARY KEY,
    restore_revision TEXT,
    runtime_state_json TEXT NOT NULL,
    current_exposure REAL NOT NULL,
    desired_exposure REAL,
    executor_state_json TEXT,
    replacement_gate_reason_json TEXT,
    execution_gate_state_json TEXT,
    ledger_state_json TEXT,
    unrealized_pnl REAL NOT NULL DEFAULT 0,
    strategy_price REAL,
    strategy_price_status TEXT NOT NULL,
    mark_price REAL,
    best_bid REAL,
    best_ask REAL,
    out_of_band_since TEXT,
    last_tick_at TEXT,
    market_data_stale_since TEXT,
    updated_at TEXT NOT NULL
);";
```

`application/src/track_read_source.rs`：

```rust
pub(crate) struct TrackRuntimeReadState {
    pub status: TrackStatus,
    pub current_exposure: Exposure,
    pub desired_exposure: Option<Exposure>,
    pub executor_state: ExecutorState,
    pub replacement_gate_reason: Option<ReplacementGateReason>,
    pub ledger_state: TrackLedgerState,
    pub unrealized_pnl: f64,
    pub has_account_margin_guard: bool,
    pub price_execution_block_reason: Option<PriceExecutionBlockReason>,
    pub strategy_price: Option<f64>,
    pub strategy_price_status: StrategyPriceStatus,
    pub mark_price: Option<f64>,
    pub best_bid: Option<f64>,
    pub best_ask: Option<f64>,
    pub market_data_stale_since: Option<DateTime<Utc>>,
}

impl TrackRuntimeReadState {
    pub(crate) fn from_snapshot(
        snapshot: TrackRuntimeSnapshot,
        has_account_margin_guard: bool,
    ) -> Self {
        Self {
            status: derive_track_status(&snapshot.runtime_state),
            current_exposure: snapshot.current_exposure,
            desired_exposure: snapshot.desired_exposure,
            executor_state: snapshot.executor_state,
            replacement_gate_reason: snapshot.replacement_gate_reason,
            ledger_state: snapshot.ledger_state,
            unrealized_pnl: snapshot.risk.unrealized_pnl,
            has_account_margin_guard,
            price_execution_block_reason: snapshot
                .execution_gate_state
                .price_execution_block_reason,
            strategy_price: snapshot.observed.strategy_price,
            strategy_price_status: snapshot.observed.strategy_price_status,
            mark_price: snapshot.observed.mark_price,
            best_bid: snapshot.observed.best_bid,
            best_ask: snapshot.observed.best_ask,
            market_data_stale_since: snapshot.observed.market_data_stale_since,
        }
    }
}

pub(crate) struct TrackReadSource {
    pub definition: TrackReadDefinition,
    pub runtime: TrackRuntimeReadState,
    pub updated_at: DateTime<Utc>,
    pub recent_track_events: Vec<StoredTrackEvent>,
    pub recent_effects: Vec<PersistedTrackEffect>,
}
```

要求：

- `TrackRuntimeSnapshot` 根接口和 engine、storage、application 内部直接消费者在本 task 一起切换
- `application::test_support` 对 read-side `server-test-support` 只暴露 `TrackReadModelFixture` 这类公开 projection fixture，不暴露 `TrackRuntimeSnapshot`、`TrackRuntimeReadState` 或 `TrackReadSource`
- server read-side / projector 测试中的 durable seed 改为 application test-support 的公开 `TrackReadModel` fixture；runtime/effect-worker 集成测试保留 snapshot seed manager 能力
- `TrackRuntimeReadState` / `TrackReadSource` 只能作为 application 内部适配器，不再从 `application/src/lib.rs` re-export
- `TrackStatus` 只在 `TrackRuntimeReadState::from_snapshot` 或同级 helper 中派生一次
- account capacity 由 `ExecutionGateState.account_capacity` 持有，不再作为 `RiskState` 字段
- account capacity 阻止加仓时输出 `ExecutionGateDecision`，不输出 `RiskOutcome`，不产生 `DomainEvent::RiskDenied`
- server read-side / projector 和它们的测试夹具只消费公开 `TrackReadModel` / `TrackStatus`
- server runtime/orchestration 生产路径通过 application service 恢复持久化状态和读取 recovery summary，不直接解析 `TrackRuntimeSnapshot` / `TrackRuntimeReadState`
- 不保留 legacy 解码逻辑
- 用 `rg -l "TrackRuntimeSnapshot|TrackRuntimeReadState::from_snapshot|TrackRuntimeReadState::from_parts|from_snapshot\\(" application/src engine/src storage/src` 核对内部直接消费者；匹配到的文件必须已经在本 task 文件列表中迁移或确认不受根接口形状影响
- 用 `! rg -n "TrackRuntimeSnapshot|TrackRuntimeReadState|TrackState::|ControlState::|AutoState::|ManualState::|ReentryGuard" server/src/runtime/reconcile.rs server/src/server_context.rs server/src/state_bootstrap.rs` 确认 server runtime/orchestration 生产模块不接触 snapshot 私有状态
- 用 `! rg -n "TrackRuntimeReadState|TrackState::|ControlState::|AutoState::|ManualState::|ReentryGuard" server/src/http.rs server/src/websocket.rs server/src/projector.rs` 确认 server read-side / projector 只消费公开投影

- [x] **Step 5: 运行 Task 3 回归**

Run:

- `cargo test -p poise-engine snapshot::tests::snapshot_round_trips_runtime_track_state -- --exact`
- `cargo test -p poise-engine reconciler::tests::freeze_samples_target_anchor_from_last_risk_approved_target -- --exact`
- `cargo test -p poise-engine reconciler::tests::frozen_reentry_clears_target_anchor_and_follows_current_strategy_target -- --exact`
- `cargo test -p poise-engine reconciler::tests::hold_samples_target_anchor_from_last_risk_approved_target -- --exact`
- `cargo test -p poise-engine reconciler::tests::holding_keeps_target_anchor_when_price_reenters_band -- --exact`
- `cargo test -p poise-engine manager::tests::resume_from_holding_clears_target_anchor_and_recomputes_following_band -- --exact`
- `cargo test -p poise-engine reconciler::tests::reconcile_target_terminates_when_risk_requests_termination -- --exact`
- `cargo test -p poise-application track_read_source::tests::read_source_derives_manual_flattening_status_from_runtime_state -- --exact`
- `cargo test -p poise-application read_model::tests::read_model_from_snapshot_flattens_runtime_state -- --exact`
- `cargo test -p poise-application query_service::tests::load_track_recovery_view_projects_runtime_recovery_summary -- --exact`
- `cargo test -p poise-application track_command_service::tests::restore_persisted_track_state_rehydrates_manager_from_store -- --exact`
- `cargo test -p poise-storage sqlite::tests::load_track_state_from_runtime_state_snapshot_schema -- --exact`
- `cargo test -p poise-storage sqlite::tests::save_transition_persists_snapshot_and_events_atomically -- --exact`
- `cargo test -p poise-server projector::tests::projector_available_commands_follow_public_status_only -- --exact`
- `cargo test -p poise-server runtime::tests::execution::insufficient_margin_guard_blocks_follow_up_submit_after_market_tick -- --exact`
- `cargo test -p poise-server runtime::tests::startup::startup_bootstrap_restores_claimed_live_order_before_first_tick -- --exact`
- `cargo test -p poise-server runtime::tests::startup::recovery_task_resyncs_recovery_anomaly_automatically_without_user_data -- --exact`
- `cargo test -p poise-server runtime::tests::reconcile::apply_user_data_event_preserves_write_service_mutation_error_kind -- --exact`
- `! rg -n "pub mod track_read_source|pub use .*TrackRuntimeReadState|pub use .*TrackReadSource" application/src/lib.rs`
- `! rg -n "TrackRuntimeSnapshot|TrackRuntimeReadState|TrackState::|ControlState::|AutoState::|ManualState::|ReentryGuard" server/src/runtime/reconcile.rs server/src/server_context.rs server/src/state_bootstrap.rs`
- `! rg -n "TrackRuntimeReadState|TrackState::|ControlState::|AutoState::|ManualState::|ReentryGuard" server/src/http.rs server/src/websocket.rs server/src/projector.rs`

Expected:

- `TrackState` 是 engine 唯一主状态
- storage 以完整 `runtime_state_json` 为根对象保存和恢复
- storage 以 `execution_gate_state_json` 保存账户容量 gate 状态，不再通过 `risk.account_capacity_constraint` 恢复
- `TrackRuntimeReadState` 保持为 application 私有适配器，server read-side 只看到 `TrackReadModel`
- `application/src/lib.rs` 不再 re-export `TrackRuntimeReadState` / `TrackReadSource`
- `Frozen` / `Holding` 的 `target_anchor` 采样、保留和清除语义有测试覆盖
- account capacity 不再通过 risk 命名的 state/event 表达
- engine、storage、application 内部直接 `TrackRuntimeSnapshot` 消费者已经随根接口迁移
- server runtime/orchestration 生产路径不再直接解析 `TrackRuntimeSnapshot` 或 `TrackRuntimeReadState`
- server read-side / projector 生产代码和测试夹具都不 import `TrackRuntimeReadState`，也不构造 engine 私有 `TrackState`

- [x] **Step 6: Commit**

```bash
git add core/src/events.rs engine/src/execution_gate.rs engine/src/price_gate.rs engine/src/lib.rs engine/src/runtime.rs engine/src/reconciler.rs engine/src/manager.rs engine/src/snapshot.rs engine/src/persisted_runtime.rs engine/src/transition.rs storage/src/schema.rs storage/src/sqlite.rs application/src/lib.rs application/src/test_support.rs application/src/track_read_source.rs application/src/read_model.rs application/src/query_service.rs application/src/debug_query_service.rs application/src/track_mutation_store.rs application/src/track_persistence.rs application/src/mutation_executor.rs server/src/http.rs server/src/websocket.rs server/src/assembly.rs server/src/state_bootstrap.rs server/src/projector.rs server/src/runtime/reconcile.rs server/src/effect_worker/tests/mod.rs server/src/effect_worker/tests/support.rs server/src/runtime/tests/mod.rs server/src/runtime/tests/support.rs server/src/runtime/tests/user_data.rs
git commit -m "refactor: adopt track state and runtime state persistence root"
```

Recorded commits: `8a60d88`, `6058c2a`

### Task 4: 最终跨 crate 验收并同步任务清单

**Files:**
- Modify: `docs/superpowers/specs/2026-04-20-track-protection-state-model-design.md`
- Modify: `docs/superpowers/plans/2026-04-20-track-protection-state-model.md`

- [ ] **Step 1: 核对设计稿和计划不再描述未实现的时间确认**

Run:

- `! rg -n "PriceAndTime|dwell|entered_confirmation_zone_at" docs/superpowers/specs/2026-04-20-track-protection-state-model-design.md`

Expected:

- 没有匹配结果
- 第一版只公开 `BandRecoverPolicy::BackInBand` 和 `BandRecoverPolicy::PriceConfirm`
- 时间确认作为未来扩展，不出现在可反序列化 policy 或 runtime guard 中

- [ ] **Step 2: 运行最小跨 crate 验收**

Run:

- `cargo test -p poise-core band_protection_policy_parses_flatten_with_price_confirm -- --exact`
- `cargo test -p poise-core evaluate_risk_outcome_terminates_when_daily_loss_limit_is_breached -- --exact`
- `! rg -n "DenyIncrease" docs/superpowers/specs/2026-04-20-track-protection-state-model-design.md`
- `cargo test -p poise-server config_toml_parses_flatten_price_confirm_policy -- --exact`
- `cargo test -p poise-server projector_shows_flatten_price_confirm_policy_without_engine_state -- --exact`
- `cargo test -p poise-engine reconcile_target_terminates_when_risk_requests_termination -- --exact`
- `cargo test -p poise-engine account_capacity_gate_blocks_increase_without_risk_outcome -- --exact`
- `cargo test -p poise-engine reconcile_reports_account_capacity_as_execution_gate_not_risk -- --exact`
- `cargo test -p poise-engine snapshot_round_trips_runtime_track_state -- --exact`
- `cargo test -p poise-engine freeze_samples_target_anchor_from_last_risk_approved_target -- --exact`
- `cargo test -p poise-engine frozen_reentry_clears_target_anchor_and_follows_current_strategy_target -- --exact`
- `cargo test -p poise-engine hold_samples_target_anchor_from_last_risk_approved_target -- --exact`
- `cargo test -p poise-engine holding_keeps_target_anchor_when_price_reenters_band -- --exact`
- `cargo test -p poise-engine resume_from_holding_clears_target_anchor_and_recomputes_following_band -- --exact`
- `cargo test -p poise-application read_model::tests::read_model_from_snapshot_flattens_runtime_state -- --exact`
- `cargo test -p poise-application track_read_source::tests::read_source_derives_manual_flattening_status_from_runtime_state -- --exact`
- `cargo test -p poise-application query_service::tests::load_track_recovery_view_projects_runtime_recovery_summary -- --exact`
- `cargo test -p poise-application track_command_service::tests::restore_persisted_track_state_rehydrates_manager_from_store -- --exact`
- `cargo test -p poise-server projector_available_commands_follow_public_status_only -- --exact`
- `cargo test -p poise-server runtime::tests::execution::insufficient_margin_guard_blocks_follow_up_submit_after_market_tick -- --exact`
- `cargo test -p poise-server runtime::tests::startup::startup_bootstrap_restores_claimed_live_order_before_first_tick -- --exact`
- `cargo test -p poise-server runtime::tests::startup::recovery_task_resyncs_recovery_anomaly_automatically_without_user_data -- --exact`
- `cargo test -p poise-server runtime::tests::reconcile::apply_user_data_event_preserves_write_service_mutation_error_kind -- --exact`
- `cargo test -p poise-tui renders_flatten_out_of_band_policy_name -- --exact`
- `cargo test -p poise-track-tuning-workbench export_explicitly_writes_supported_defaults -- --exact`
- `pnpm --dir tools/track-tuning-workbench test -- workbenchBridge.test.ts`
- `! rg -n "pub mod track_read_source|pub use .*TrackRuntimeReadState|pub use .*TrackReadSource" application/src/lib.rs`
- `! rg -n "TrackRuntimeSnapshot|TrackRuntimeReadState|TrackState::|ControlState::|AutoState::|ManualState::|ReentryGuard" server/src/runtime/reconcile.rs server/src/server_context.rs server/src/state_bootstrap.rs`
- `! rg -n "TrackRuntimeReadState|TrackState::|ControlState::|AutoState::|ManualState::|ReentryGuard" server/src/http.rs server/src/websocket.rs server/src/projector.rs`
- `! rg -n "ExecutionGateEventReason|to_event_reason" core/src engine/src application/src storage/src server/src`
- `! rg -n "Frozen \\{ anchor|Holding \\{ anchor" docs/superpowers/specs/2026-04-20-track-protection-state-model-design.md docs/superpowers/plans/2026-04-20-track-protection-state-model.md`
- `! rg -n "account_capacity_constraint|RiskDenied" engine/src application/src storage/src server/src`

Expected:

- Task 1 没有替换被 engine 消费的旧共享函数
- 第一版 `RiskOutcome` 没有账户容量不足分支
- `TrackConfig.out_of_band_policy` 和所有直接消费者已经同 task 迁移
- 没有旧 `OutOfBandPolicy` 运行时消费者残留
- TUI、workbench、demo configs 和 fixtures 已跟随配置边界迁移
- README 和 protocol contract 已在 Task 2 跟随 public policy 形状迁移
- storage 保存完整 `runtime_state_json`
- 账户容量 gate 状态通过 `execution_gate_state_json` 持久化，不再通过 risk 命名字段或事件表达
- `TrackRuntimeReadState` 不对 server 或其他 crate 公开，server read-side 只消费 `TrackReadModel`
- `application/src/lib.rs` 不再 re-export `TrackRuntimeReadState` / `TrackReadSource`
- `ExecutionGateReason` 只有一份共享事件可见类型，没有 core/engine 双定义或转换层
- `Frozen` / `Holding` 使用 `target_anchor`，并且 `Holding` 的采样、保留和 resume 清除语义都有锁定测试
- server runtime/orchestration 生产路径通过 application service 恢复和查询运行态，不直接解析 `TrackRuntimeSnapshot` 或 `TrackRuntimeReadState`
- server read-side / projector 生产代码和测试夹具都不 import `TrackRuntimeReadState`，也不构造 `AutoState / ManualState / ReentryGuard`

- [ ] **Step 3: Commit**

```bash
git add docs/superpowers/specs/2026-04-20-track-protection-state-model-design.md docs/superpowers/plans/2026-04-20-track-protection-state-model.md
git commit -m "docs: sync track state model semantics and rollout plan"
```
