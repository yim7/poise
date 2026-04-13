# Flatten Lifecycle Semantics Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把策略层 `reduce_only` 正式替换为 `flatten`，拆分自动带外平仓与手动 `Flatten` 的生命周期状态，并把 `hold` 的人工恢复语义一起修正到最终版本。

**Architecture:** 先端到端完成“带外策略与生命周期状态”的核心抽象变更，让 `core -> engine -> protocol -> projector -> TUI` 在同一个 task 里一起切到 `flatten / flattening / manual_flattening`，避免中间任务引入临时兼容。然后用第二个 task 专门修正 `freeze / hold / resume` 的恢复规则，最后统一稳定文档和任务追踪。

**Tech Stack:** Rust workspace, Cargo tests, Markdown

---

## Files And Responsibilities

- Modify: `core/src/strategy.rs`
  定义新的带外策略稳定值。
- Modify: `engine/src/runtime.rs`
  定义新的运行时状态值。
- Modify: `engine/src/snapshot.rs`
- Modify: `engine/src/persisted_runtime.rs`
  对齐新的状态枚举序列化和恢复测试。
- Modify: `engine/src/reconciler.rs`
  实现自动带外 `flatten` 与 `freeze / hold` 的恢复规则。
- Modify: `engine/src/manager.rs`
  实现手动 `Flatten` 与 `Resume` 的生命周期语义。
- Modify: `protocol/src/lib.rs`
  同步对外枚举值和字符串。
- Modify: `application/src/read_model.rs`
- Modify: `application/src/track_read_source.rs`
  传递新的状态值和 `manual_target_override`。
- Modify: `server/src/projector.rs`
  投影状态值和命令可用性。
- Modify: `tui/src/views/dashboard.rs`
- Modify: `tui/src/theme.rs`
- Modify: `tui/src/views/instance.rs`
  展示新的生命周期名字。
- Modify: `README.md`
- Modify: `docs/protocol-contract.md`
- Modify: `docs/superpowers/specs/2026-04-13-flatten-lifecycle-semantics-design.md`
- Modify: `docs/superpowers/plans/2026-04-13-flatten-lifecycle-semantics.md`
  回写最终实现结果与 commit SHA。

### Task 1: 端到端替换 `reduce_only / reducing_only` 为最终 flatten 语义

**Files:**
- Modify: `core/src/strategy.rs`
- Modify: `engine/src/runtime.rs`
- Modify: `engine/src/snapshot.rs`
- Modify: `engine/src/persisted_runtime.rs`
- Modify: `engine/src/reconciler.rs`
- Modify: `engine/src/manager.rs`
- Modify: `protocol/src/lib.rs`
- Modify: `application/src/read_model.rs`
- Modify: `application/src/track_read_source.rs`
- Modify: `server/src/projector.rs`
- Modify: `tui/src/views/dashboard.rs`
- Modify: `tui/src/theme.rs`
- Modify: `tui/src/views/instance.rs`
- Test: `core/src/strategy.rs`
- Test: `engine/src/reconciler.rs`
- Test: `engine/src/manager.rs`
- Test: `engine/src/snapshot.rs`
- Test: `engine/src/persisted_runtime.rs`
- Test: `protocol/src/lib.rs`
- Test: `server/src/projector.rs`
- Test: `tui/src/views/dashboard.rs`
- Test: `tui/src/views/instance.rs`

- [x] **Step 1: 先写失败测试，锁住端到端的新语义**

至少补这些测试：

```rust
#[test]
fn out_of_band_policy_serializes_flatten() {
    let json = serde_json::to_string(&OutOfBandPolicy::Flatten).unwrap();
    assert_eq!(json, "\"flatten\"");
}

#[test]
fn track_status_displays_manual_flattening() {
    assert_eq!(
        TrackStatus::ManualFlattening.to_string(),
        "manual_flattening"
    );
}

#[test]
fn reconcile_target_flatten_policy_enters_flattening() {}

#[test]
fn manual_flatten_sets_manual_flattening_and_override() {}

#[test]
fn snapshot_round_trips_manual_flattening_status() {}

#[test]
fn persisted_runtime_restores_flattening_status() {}

#[test]
fn project_out_of_band_policy_uses_flatten() {}

#[test]
fn project_track_status_uses_manual_flattening() {}
```

覆盖点：

- 策略层不再输出 `"reduce_only"`
- 协议层不再输出 `"reducing_only"`
- runtime 不再写入 `ReducingOnly`
- projector / TUI 不再投影旧名字
- 订单层 `OrderRequest.reduce_only` 不在这个任务里改动

- [x] **Step 2: 运行定向测试，确认当前实现失败**

Run:

- `cargo test -p poise-core out_of_band_policy_serializes_flatten -- --exact`
- `cargo test -p poise-engine reconcile_target_flatten_policy_enters_flattening -- --exact`
- `cargo test -p poise-engine manual_flatten_sets_manual_flattening_and_override -- --exact`
- `cargo test -p poise-engine snapshot_round_trips_manual_flattening_status -- --exact`
- `cargo test -p poise-protocol track_status_displays_manual_flattening -- --exact`
- `cargo test -p poise-server project_out_of_band_policy_uses_flatten -- --exact`

Expected:

- 当前实现失败，因为 `OutOfBandPolicy::ReduceOnly` 和 `TrackStatus::ReducingOnly` 仍然存在
- projector 和 UI 仍在输出旧名字

- [x] **Step 3: 做最小实现，端到端切换到新名字**

`core/src/strategy.rs`：

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutOfBandPolicy {
    Freeze,
    Hold,
    Flatten,
    Terminate,
}
```

`engine/src/runtime.rs`：

```rust
pub enum TrackStatus {
    WaitingMarketData,
    Active,
    Frozen,
    Holding,
    Flattening,
    ManualFlattening,
    Terminated,
    Paused,
}
```

并同步修改 `engine/src/snapshot.rs`、`engine/src/persisted_runtime.rs` 的状态序列化与恢复测试，使 `Flattening` 和 `ManualFlattening` 作为稳定状态值参与 round-trip。

`engine/src/reconciler.rs`：

```rust
if let Some(target_override) = track.manual_target_override.clone() {
    return TargetReconcileResult {
        desired_exposure: target_override,
        new_status: Some(TrackStatus::ManualFlattening),
        // ...
    };
}

OutOfBandPolicy::Flatten => (Exposure(0.0), Some(TrackStatus::Flattening)),
```

`engine/src/manager.rs`：

```rust
track.manual_target_override = Some(Exposure(0.0));
track.status = TrackStatus::ManualFlattening;
```

`protocol/src/lib.rs`：

```rust
Self::Flattening => "flattening",
Self::ManualFlattening => "manual_flattening",
Self::Flatten => "flatten",
```

并同步修改 `application/src/read_model.rs`、`application/src/track_read_source.rs`、`server/src/projector.rs`、`tui/src/views/dashboard.rs`、`tui/src/views/instance.rs` 的状态映射和展示名字。

- [x] **Step 4: 跑 Task 1 回归**

Run:

- `cargo test -p poise-core`
- `cargo test -p poise-engine`
- `cargo test -p poise-protocol`
- `cargo test -p poise-application`
- `cargo test -p poise-server`
- `cargo test -p poise-tui`

Expected:

- `core -> engine -> protocol -> projector -> TUI` 都只输出新的策略语义
- 订单层 `reduce_only` 测试不受影响

- [x] **Step 5: Commit**

```bash
git add core/src/strategy.rs engine/src/runtime.rs engine/src/snapshot.rs engine/src/persisted_runtime.rs engine/src/reconciler.rs engine/src/manager.rs protocol/src/lib.rs application/src/read_model.rs application/src/track_read_source.rs server/src/projector.rs tui/src/views/dashboard.rs tui/src/theme.rs tui/src/views/instance.rs
git commit -m "refactor: align flatten lifecycle semantics across runtime and UI"
```

Task 1 implementation commit: `a056cbb`

### Task 2: 修正 `freeze / hold / resume` 的恢复语义

**Files:**
- Modify: `engine/src/reconciler.rs`
- Modify: `engine/src/manager.rs`
- Modify: `server/src/projector.rs`
- Modify: `tui/src/views/dashboard.rs`
- Modify: `tui/src/views/instance.rs`
- Test: `engine/src/reconciler.rs`
- Test: `engine/src/manager.rs`
- Test: `server/src/projector.rs`

- [x] **Step 1: 先写失败测试，锁住 `hold` 的人工恢复语义**

至少补这些测试：

```rust
#[test]
fn frozen_recovers_to_active_when_price_returns_in_band() {}

#[test]
fn holding_stays_holding_when_price_returns_in_band() {}

#[test]
fn resume_clears_holding_and_reconciles_normally() {}

#[test]
fn resume_is_enabled_when_holding_is_active() {}

#[test]
fn resume_availability_depends_on_status_not_override() {}
```

如果当前 `Resume` 在缺少 live `strategy_price` 时已经有测试，补一个 `Holding` 分支共用同样的等待市场数据结果断言。

- [x] **Step 2: 运行定向测试，确认当前实现失败**

Run:

- `cargo test -p poise-engine holding_stays_holding_when_price_returns_in_band -- --exact`
- `cargo test -p poise-engine resume_clears_holding_and_reconciles_normally -- --exact`
- `cargo test -p poise-server resume_is_enabled_when_holding_is_active -- --exact`
- `cargo test -p poise-server resume_availability_depends_on_status_not_override -- --exact`

Expected:

- 当前实现失败，因为 `Holding` 仍会在回带内自动恢复
- `Resume` 也还没有把 `Holding` 当作人工恢复状态

- [x] **Step 3: 做最小实现，修正 `hold` 的恢复规则**

`engine/src/reconciler.rs`：

```rust
match track.status {
    TrackStatus::WaitingMarketData => Some(TrackStatus::Active),
    TrackStatus::Frozen | TrackStatus::Flattening => Some(TrackStatus::Active),
    TrackStatus::Holding => None,
    _ => None,
}
```

`engine/src/manager.rs`：

```rust
if matches!(track.status, TrackStatus::Holding) {
    track.status = TrackStatus::WaitingMarketData;
    track.desired_exposure = None;
    track.replacement_gate_reason = None;

    return match Self::live_strategy_price_for(track) {
        Some(strategy_price) => self.reconcile_track(id, strategy_price),
        None => {
            Ok((vec![], vec![]))
        }
    };
}
```

`server/src/projector.rs` 里把 `Resume` 的可用性改成：

```rust
enabled: matches!(
    status,
    EngineTrackStatus::Paused
        | EngineTrackStatus::Holding
        | EngineTrackStatus::ManualFlattening
)
```

并补一个反向测试，明确即使测试夹具里存在异常的 `manual_target_override`，只要状态不是 `Paused / Holding / ManualFlattening`，`Resume` 也不能被启用。

`tui` 只同步展示和按钮语义，不增加额外兼容文案。

- [x] **Step 4: 跑 Task 2 回归**

Run:

- `cargo test -p poise-engine reconciler::tests:: -- --nocapture`
- `cargo test -p poise-engine manager::tests::resume -- --nocapture`
- `cargo test -p poise-server projector::tests:: -- --nocapture`

Expected:

- `Frozen` 和 `Flattening` 自动恢复
- `Holding` 不自动恢复
- `Resume` 可以从 `Holding` 恢复
- UI 命令语义与状态来源一致

- [x] **Step 5: Commit**

```bash
git add engine/src/reconciler.rs engine/src/manager.rs server/src/projector.rs tui/src/views/dashboard.rs tui/src/views/instance.rs
git commit -m "fix: preserve hold as manual recovery lifecycle"
```

Task 2 implementation commit: `bb3f9f8`

### Task 3: 更新稳定文档并删除旧策略语义残留

**Files:**
- Modify: `README.md`
- Modify: `docs/protocol-contract.md`
- Modify: `docs/superpowers/specs/2026-04-13-flatten-lifecycle-semantics-design.md`
- Modify: `docs/superpowers/plans/2026-04-13-flatten-lifecycle-semantics.md`

- [x] **Step 1: 写一个文档回归清单**

把下面这些稳定文档语义逐条检查并改成最终版本：

- `README.md` 中提到的带外策略名字
- `docs/protocol-contract.md` 中对 `flatten`、`terminate`、`resume` 的说明
- 本 spec 和本 plan 中的实际实现结果与 commit SHA

- [x] **Step 2: 更新稳定文档**

至少写清楚：

- 自动带外策略是 `flatten`
- `freeze` 自动恢复，`hold` 人工恢复
- 手动命令是 `Flatten`
- 自动状态是 `flattening`
- 手动状态是 `manual_flattening`
- 订单层 `reduce_only` 仍保留
- 不支持旧 `reduce_only` 策略别名

- [x] **Step 3: 跑最终回归**

Run:

- `cargo test -p poise-core`
- `cargo test -p poise-engine`
- `cargo test -p poise-application`
- `cargo test -p poise-protocol`
- `cargo test -p poise-server`
- `cargo test -p poise-tui`

Expected:

- 所有工作区测试通过
- 不再有策略层 `reduce_only / reducing_only` 对外残留
- `hold` 的恢复语义和文档一致
- 订单层 `reduce_only` 相关测试继续通过

- [x] **Step 4: Commit**

```bash
git add README.md docs/protocol-contract.md docs/superpowers/specs/2026-04-13-flatten-lifecycle-semantics-design.md docs/superpowers/plans/2026-04-13-flatten-lifecycle-semantics.md
git commit -m "docs: align flatten and hold lifecycle semantics"
```

Task 3 implementation commit: `324467a`

### Post-Review Fixes

在严格代码评审后，补了 3 个实现偏差：

- `Holding` 回带内后继续保持冻结 target，不再切回带内曲线目标
- 手动 `Flatten` 在没有 live `strategy_price` 时，也会立即把 `desired_exposure` 置为 `0` 并清掉 `replacement_gate_reason`
- restore 路径新增不变量校验：`ManualFlattening` 必须配对 `manual_target_override = Some(Exposure(0.0))`

验证命令：

- `cargo test -p poise-engine -p poise-server`

Post-review fix commit: `a7a8e4e`

## Self-Review

- spec 覆盖检查：
  - 自动带外 `flatten` 进入 `Flattening`：Task 1
  - 手动 `Flatten` 进入 `ManualFlattening`：Task 1
  - `Resume` 清 override 恢复正常 reconcile：Task 1
  - `Holding` 不自动恢复，且只能人工恢复：Task 2
  - 协议和 projector 不再输出 `reduce_only / reducing_only`：Task 1
  - TUI 与稳定文档同步：Task 1、Task 3
  - 订单层 `reduce_only` 保留：Task 1、Task 3 说明和全量回归
- placeholder 检查：
  - 没有 `TODO` / `TBD`
  - 每个 task 都有文件、测试、命令和 commit
- 类型一致性：
  - `OutOfBandPolicy::Flatten`
  - `TrackStatus::Flattening`
  - `TrackStatus::ManualFlattening`
  - `manual_target_override: Option<Exposure>`
