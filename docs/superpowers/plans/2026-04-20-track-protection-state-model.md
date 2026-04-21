# Track Protection State Model Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` or `superpowers:executing-plans` when执行本计划。每个 task 都要单独验收、提交，并把 commit SHA 回写到本文件。

**Goal:** 让 track 带外保护语义和状态机与最新设计一致：

- `freeze`：出主带即冻结，不补仓，回主带立即恢复
- `flatten`：出主带先进入等待阶段，继续向外穿过外侧触发带才真正平到 `0`，之后按恢复规则自动恢复
- `terminate`：出主带直接终止
- `hold` 删除

**Architecture:** 按共享边界拆 task，不按时间顺序拆。Task 1 处理 `BandProtectionPolicy` 这个 public/config boundary，并同时迁移所有直接消费者与运行语义；该 task 故意保持现有 `runtime_state / TrackRuntimeSnapshot` 形状不动，也不提前改公开 lifecycle/status 枚举，避免把后两个共享边界半迁移。Task 2 再把 engine 私有运行态、`TrackRuntimeSnapshot` 边界，以及由 runtime 驱动的 public lifecycle boundary 一起改成最终设计，并把所有直接消费者一起迁移。Task 3 只做最终校验与文档回写。

**Tech Stack:** Rust workspace, Cargo, Serde, SQLite, Markdown

---

## Design Constraints

- `BandProtectionPolicy` 形状变更必须和所有直接消费者同 task 迁移：
  - `core`
  - `protocol`
  - `application` 配置 / 读模型
  - `server` 配置 / projector / 装配
  - `tui`
  - `workbench`
  - demo 配置
  - README / protocol contract
- Task 1 不允许改动 `TrackRuntimeSnapshot` 根形状；只允许在现有 engine 私有 runtime 表示上实现新语义
- Task 1 不允许改动公开 lifecycle/status 边界；`TrackStatus` / `TrackReadStatus` / 协议 `TrackStatus` / TUI lifecycle 投影必须保持现状
- Task 2 才允许切换 `AutoState` / `runtime_state_json` / `TrackRuntimeSnapshot`
- Task 2 才允许删除 `Holding`，并且必须和 runtime 删除、public lifecycle 投影删除同 task 完成
- `TrackRuntimeSnapshot` 形状变更必须和所有直接消费者同 task 迁移，包括：
  - engine snapshot / restore / persistence
  - storage sqlite
  - application `track_read_source` 及相关 read adapter
  - application mutation store / persistence
  - 直接构造 snapshot 的测试夹具
- `Holding` 的公开状态删除必须和所有直接消费者同 task 迁移，包括：
  - `application::TrackReadStatus`
  - `server::projector`
  - `protocol::TrackStatus`
  - TUI lifecycle/status 展示
- `freeze` 不再携带 `recover`
- `flatten` 才拥有 `trigger_bps` 和 `recover`
- `hold` 必须从 policy、runtime state、配置、投影、文档和 fixture 一起删除
- 第一版不实现时间确认
- 第一版不实现“多次 flatten 自动 terminate”
- risk 语义保持现状：`daily_loss_limit / total_loss_limit` 直接进入 `Terminated`

## Files And Responsibilities

### Public band-policy boundary

- Modify: `core/src/strategy.rs`
- Modify: `protocol/src/lib.rs`
- Modify: `application/src/track_definition.rs`
- Modify: `application/src/read_model.rs`
- Modify: `server/src/config.rs`
- Modify: `server/src/projector.rs`
- Modify: `server/src/http.rs`
- Modify: `server/src/websocket.rs`
- Modify: `server/src/assembly.rs`
- Modify: `server/src/main.rs`
- Modify: `tui/src/main.rs`
- Modify: `tui/src/views/instance.rs`
- Modify: `tools/track-tuning-workbench/src/app/workbenchBridge.ts`
- Modify: `tools/track-tuning-workbench/src-tauri/src/config_document.rs`
- Modify: `tools/track-tuning-workbench/src-tauri/src/config_projection.rs`
- Modify: `tools/track-tuning-workbench/src-tauri/src/commands.rs`
- Modify: `configs/bybit-testnet.demo.toml`
- Modify: `configs/binance-testnet.demo.toml`
- Modify: `configs/test.demo.toml`
- Modify: `README.md`
- Modify: `docs/protocol-contract.md`

### Engine private runtime and snapshot boundary

- Modify: `engine/src/runtime.rs`
- Modify: `engine/src/reconciler.rs`
- Modify: `engine/src/snapshot.rs`
- Modify: `engine/src/persisted_runtime.rs`
- Modify: `engine/src/manager.rs`
- Modify: `storage/src/sqlite.rs`
- Modify: `application/src/track_read_source.rs`
- Modify: `application/src/query_service.rs`
- Modify: `application/src/debug_query_service.rs`
- Modify: `application/src/runtime_lifecycle_service.rs`
- Modify: `application/src/runtime_read_state_loader.rs`
- Modify: `application/src/track_read_source_loader.rs`
- Modify: `application/src/track_mutation_store.rs`
- Modify: `application/src/track_persistence.rs`
- Modify: `application/src/mutation_executor.rs`
- Modify: `application/src/read_model.rs`
- Modify: `protocol/src/lib.rs`
- Modify: `server/src/http.rs`
- Modify: `server/src/websocket.rs`
- Modify: `server/src/assembly.rs`
- Modify: `server/src/projector.rs`
- Modify: `server/src/runtime/tests/support.rs`
- Modify: `server/src/runtime/tests/user_data.rs`
- Modify: `server/src/effect_worker/tests/support.rs`
- Modify: `server/src/effect_worker/tests/mod.rs`
- Modify: `tui/src/views/dashboard.rs`
- Modify: `tui/src/theme.rs`

## Non-Goals

- 不修改执行器下单机制
- 不新增时间确认
- 不实现历史数据迁移；开发和验收环境直接重建状态
- 不新增对外 `flatten_pending` 生命周期值

## Task 1: 迁移 `BandProtectionPolicy` public/config boundary 与语义

**Files:**

- Modify: `core/src/strategy.rs`
- Modify: `protocol/src/lib.rs`
- Modify: `application/src/track_definition.rs`
- Modify: `application/src/read_model.rs`
- Modify: `server/src/config.rs`
- Modify: `server/src/projector.rs`
- Modify: `server/src/http.rs`
- Modify: `server/src/websocket.rs`
- Modify: `server/src/assembly.rs`
- Modify: `server/src/main.rs`
- Modify: `tui/src/main.rs`
- Modify: `tui/src/views/instance.rs`
- Modify: `tools/track-tuning-workbench/src/app/workbenchBridge.ts`
- Modify: `tools/track-tuning-workbench/src-tauri/src/config_document.rs`
- Modify: `tools/track-tuning-workbench/src-tauri/src/config_projection.rs`
- Modify: `tools/track-tuning-workbench/src-tauri/src/commands.rs`
- Modify: `configs/bybit-testnet.demo.toml`
- Modify: `configs/binance-testnet.demo.toml`
- Modify: `configs/test.demo.toml`
- Modify: `README.md`
- Modify: `docs/protocol-contract.md`
- Modify: `engine/src/reconciler.rs`
- Modify: `engine/src/manager.rs`
- Test: `core/src/strategy.rs`
- Test: `server/src/config.rs`
- Test: `server/src/projector.rs`
- Test: `engine/src/reconciler.rs`
- Test: `tui/src/views/instance.rs`
- Test: `tools/track-tuning-workbench/src-tauri/src/commands.rs`

- [x] **Step 1: 先写失败测试，锁定 shared boundary 形状**

```rust
#[test]
fn band_protection_policy_parses_freeze_without_recover() {}

#[test]
fn band_protection_policy_parses_flatten_with_trigger_and_price_confirm() {}

#[test]
fn config_toml_parses_flatten_trigger_and_price_confirm_policy() {}

#[test]
fn projector_shows_flatten_trigger_and_recover_policy() {}

#[test]
fn flatten_policy_freezes_before_trigger_band_with_current_runtime_shape() {}

#[test]
fn flatten_policy_enters_flattening_after_trigger_band_breach_with_current_runtime_shape() {}
```

- [x] **Step 2: 同 task 改写所有 direct consumer**

要求：

- `BandProtectionPolicy` 改为：

```rust
enum BandProtectionPolicy {
    Freeze,
    Flatten {
        trigger_bps: u32,
        recover: BandRecoverPolicy,
    },
    Terminate,
}
```

- 删除 `Hold`
- `freeze` 配置形状改为 `{ freeze = {} }`
- `flatten` 配置形状改为：

```toml
out_of_band_policy = { flatten = {
  trigger_bps = 500,
  recover = { price_confirm = { bps = 500 } }
} }
```

- `protocol`、`server projector`、`TUI`、`workbench`、demo config、README、protocol contract 必须同 task 一起迁移
- Task 1 只允许修改 `out_of_band_policy` 的 serde / projection / 展示；不得删除或改写 `TrackStatus` / `TrackReadStatus` / `holding` 公开生命周期值

- [x] **Step 3: 在不改 snapshot 形状的前提下实现新运行语义**

要求：

- Task 1 明确保持现有 `TrackRuntimeSnapshot` / `runtime_state_json` 形状不变
- engine 可以在当前私有 runtime 表示上实现新语义，但这种兼容只允许停留在 engine 私有边界
- 对外行为必须已经正确：
  - `freeze` 出主带即冻结，回主带立即恢复
  - `flatten` 出主带先表现为 `frozen`
  - 只有穿过 `trigger_bps` 才进入 `flattening`
  - `flattening` 再按 `recover` 自动恢复

- [x] **Step 4: 运行最小回归**

Run:

- `cargo test -p poise-core strategy::tests::band_protection_policy_parses_freeze_without_recover -- --exact`
- `cargo test -p poise-core strategy::tests::band_protection_policy_parses_flatten_with_trigger_and_price_confirm -- --exact`
- `cargo test -p poise-server config::tests::config_toml_parses_flatten_trigger_and_price_confirm_policy -- --exact`
- `cargo test -p poise-server projector::tests::projector_shows_flatten_trigger_and_recover_policy -- --exact`
- `cargo test -p poise-engine reconciler::tests::flatten_policy_freezes_before_trigger_band_with_current_runtime_shape -- --exact`
- `cargo test -p poise-engine reconciler::tests::flatten_policy_enters_flattening_after_trigger_band_breach_with_current_runtime_shape -- --exact`
- `cargo test -p poise-tui views::instance::tests::renders_flatten_trigger_policy_name -- --exact`
- `cargo test -p poise-track-tuning-workbench commands::tests::export_current_track_text_only_contains_the_selected_track -- --exact`
- `cargo test -p poise-track-tuning-workbench commands::tests::export_all_tracks_text_keeps_each_track_block -- --exact`
- `cargo test -p poise-track-tuning-workbench commands::tests::export_current_track_only_returns_tracks_table -- --exact`
- `cargo test -p poise-track-tuning-workbench commands::tests::load_config_file_returns_projected_tracks -- --exact`

- [x] **Step 5: Commit**

```bash
git add core/src/strategy.rs protocol/src/lib.rs application/src/track_definition.rs application/src/read_model.rs server/src/config.rs server/src/projector.rs server/src/http.rs server/src/websocket.rs server/src/assembly.rs server/src/main.rs tui/src/main.rs tui/src/views/instance.rs tools/track-tuning-workbench/src/app/workbenchBridge.ts tools/track-tuning-workbench/src-tauri/src/config_document.rs tools/track-tuning-workbench/src-tauri/src/config_projection.rs tools/track-tuning-workbench/src-tauri/src/commands.rs configs/bybit-testnet.demo.toml configs/binance-testnet.demo.toml configs/test.demo.toml README.md docs/protocol-contract.md engine/src/reconciler.rs engine/src/manager.rs
git commit -m "refactor: migrate public band protection policy boundary"
```

Recorded commit: `7c59914`

## Task 2: 迁移 runtime、snapshot 与 public lifecycle boundary 到最终形状

**Files:**

- Modify: `engine/src/runtime.rs`
- Modify: `engine/src/reconciler.rs`
- Modify: `engine/src/snapshot.rs`
- Modify: `engine/src/persisted_runtime.rs`
- Modify: `engine/src/manager.rs`
- Modify: `storage/src/sqlite.rs`
- Modify: `application/src/track_read_source.rs`
- Modify: `application/src/query_service.rs`
- Modify: `application/src/debug_query_service.rs`
- Modify: `application/src/runtime_lifecycle_service.rs`
- Modify: `application/src/runtime_read_state_loader.rs`
- Modify: `application/src/track_read_source_loader.rs`
- Modify: `application/src/track_mutation_store.rs`
- Modify: `application/src/track_persistence.rs`
- Modify: `application/src/mutation_executor.rs`
- Modify: `application/src/read_model.rs`
- Modify: `protocol/src/lib.rs`
- Modify: `server/src/http.rs`
- Modify: `server/src/websocket.rs`
- Modify: `server/src/assembly.rs`
- Modify: `server/src/projector.rs`
- Modify: `server/src/runtime/tests/support.rs`
- Modify: `server/src/runtime/tests/user_data.rs`
- Modify: `server/src/effect_worker/tests/support.rs`
- Modify: `server/src/effect_worker/tests/mod.rs`
- Modify: `tui/src/views/dashboard.rs`
- Modify: `tui/src/theme.rs`
- Test: `engine/src/reconciler.rs`
- Test: `engine/src/snapshot.rs`
- Test: `engine/src/persisted_runtime.rs`
- Test: `application/src/track_read_source.rs`
- Test: `application/src/read_model.rs`
- Test: `server/src/projector.rs`
- Test: `tui/src/views/dashboard.rs`

- [ ] **Step 1: 先写失败测试，锁定最终 runtime state**

```rust
#[test]
fn freeze_policy_uses_frozen_without_boundary_guard() {}

#[test]
fn flatten_policy_uses_flatten_pending_before_flattening() {}

#[test]
fn flatten_pending_rearms_when_price_flips_to_opposite_out_of_band_side() {}

#[test]
fn snapshot_round_trips_flatten_pending_runtime_state() {}

#[test]
fn flatten_pending_projects_as_frozen_without_leaking_private_state() {}

#[test]
fn runtime_boundary_migration_removes_holding_from_public_status_projection() {}
```

- [ ] **Step 2: 同 task 切换 final runtime_state / snapshot 形状**

要求：

- `AutoState` 改为：

```rust
enum AutoState {
    FollowingBand,
    Frozen { target_anchor: Exposure },
    FlattenPending { target_anchor: Exposure, boundary: BandBoundary },
    Flattening { boundary: BandBoundary },
}
```

- 删除：
  - `Holding`
  - 通用 `ReentryGuard`
- `TrackRuntimeSnapshot`、`runtime_state_json`、`persisted_runtime`、`sqlite` 一起切到新形状
- 所有直接解析/构造 `TrackRuntimeSnapshot` 的 application 适配层和测试夹具同 task 一起迁移
- `Holding` 必须在本 task 内同时从以下边界删除：
  - `engine` runtime / `TrackStatus`
  - `application::TrackReadStatus`
  - `protocol::TrackStatus`
  - `server::projector` 的 lifecycle / command 逻辑
  - TUI dashboard / theme 的 lifecycle 展示

- [ ] **Step 3: 锁死 `FlattenPending` 的 opposite-side 语义**

要求：

- 如果 `FlattenPending` 还没触发真正 `flatten` 就观察到 opposite-side out-of-band：
  - 必须丢弃旧 pending
  - 按当前带外侧重建新的 `FlattenPending`
- 不允许继续沿用旧 `boundary`
- 不要求必须先回到 in-band 才能 re-arm

- [ ] **Step 4: 运行最小回归**

Run:

- `cargo test -p poise-engine reconciler::tests::freeze_policy_uses_frozen_without_boundary_guard -- --exact`
- `cargo test -p poise-engine reconciler::tests::flatten_policy_uses_flatten_pending_before_flattening -- --exact`
- `cargo test -p poise-engine reconciler::tests::flatten_pending_rearms_when_price_flips_to_opposite_out_of_band_side -- --exact`
- `cargo test -p poise-engine snapshot::tests::snapshot_round_trips_flatten_pending_runtime_state -- --exact`
- `cargo test -p poise-storage sqlite::tests::save_transition_persists_runtime_state_json -- --exact`
- `cargo test -p poise-application track_read_source::tests::flatten_pending_projects_as_frozen_without_leaking_private_state -- --exact`
- `cargo test -p poise-server projector::tests::runtime_boundary_migration_removes_holding_from_public_status_projection -- --exact`
- `cargo test -p poise-tui views::dashboard::tests::renders_flattening_without_holding_status -- --exact`

- [ ] **Step 5: Commit**

```bash
git add engine/src/runtime.rs engine/src/reconciler.rs engine/src/snapshot.rs engine/src/persisted_runtime.rs engine/src/manager.rs storage/src/sqlite.rs application/src/track_read_source.rs application/src/query_service.rs application/src/debug_query_service.rs application/src/runtime_lifecycle_service.rs application/src/runtime_read_state_loader.rs application/src/track_read_source_loader.rs application/src/track_mutation_store.rs application/src/track_persistence.rs application/src/mutation_executor.rs application/src/read_model.rs protocol/src/lib.rs server/src/http.rs server/src/websocket.rs server/src/assembly.rs server/src/projector.rs server/src/runtime/tests/support.rs server/src/runtime/tests/user_data.rs server/src/effect_worker/tests/support.rs server/src/effect_worker/tests/mod.rs tui/src/views/dashboard.rs tui/src/theme.rs
git commit -m "refactor: migrate runtime snapshot boundary to final protection state"
```

Recorded commit: `TODO`

## Task 3: 最终校验与文档回写

**Files:**

- Modify: `docs/superpowers/specs/2026-04-20-track-protection-state-model-design.md`
- Modify: `docs/superpowers/plans/2026-04-20-track-protection-state-model.md`

- [ ] **Step 1: 运行最终语义校验**

Run:

- `cargo test -p poise-core strategy::tests::band_protection_policy_parses_flatten_with_trigger_and_price_confirm -- --exact`
- `cargo test -p poise-engine reconciler::tests::flatten_pending_rearms_when_price_flips_to_opposite_out_of_band_side -- --exact`
- `cargo test -p poise-engine reconciler::tests::flattening_price_confirm_recovery_is_boundary_specific -- --exact`
- `cargo test -p poise-server projector::tests::projector_shows_flatten_trigger_and_recover_policy -- --exact`
- `cargo test -p poise-server http::tests::get_track_detail_returns_track_detail_view -- --exact`
- `cargo test -p poise-server websocket::tests::broadcasts_track_detail_changed_after_write_commit -- --exact`
- `cargo fmt --all`
- `git diff --check`

- [ ] **Step 2: 文档负向检查**

Run:

- `! rg -n "Hold|Holding" core protocol application engine storage server tui tools README.md docs/protocol-contract.md configs`
- `! rg -n "Freeze \\{ recover|freeze = \\{ recover" core protocol application engine storage server tui tools README.md docs/protocol-contract.md configs`
- `! rg -n "ReentryGuard" core application engine storage server tui tools`

- [ ] **Step 3: 回写任务清单并提交**

```bash
git add docs/superpowers/specs/2026-04-20-track-protection-state-model-design.md docs/superpowers/plans/2026-04-20-track-protection-state-model.md
git commit -m "docs: sync track protection state model tasks"
```

Recorded commit: `TODO`

## Final Acceptance Criteria

- `BandProtectionPolicy` 只保留 `Freeze / Flatten / Terminate`
- 共享 `BandProtectionPolicy` 边界没有被拆成半迁移 task
- `flatten` 的配置同时包含 `trigger_bps` 和 `recover`
- `freeze` 的恢复语义固定为“回主带立即恢复”
- `AutoState` 最终只保留 `FollowingBand / Frozen / FlattenPending / Flattening`
- `TrackRuntimeSnapshot` 边界迁移和所有直接消费者在同一 task 完成
- `Holding` 在 runtime、application read-model、protocol 与 TUI 中同 task 删除，不留下公开 `holding` 生命周期值
- `FlattenPending` 对外投影为 `frozen`
- `FlattenPending` 的 opposite-side re-arm 语义有明确测试
- risk 触发仍直接进入 `terminate`
- `TrackState` 没有再次泄漏到 server read-side
