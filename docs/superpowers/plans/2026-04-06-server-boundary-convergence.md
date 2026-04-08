# Server Boundary Convergence Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 新增 `poise-application` 作为应用层 owner，把 query、effect 调度、写侧事务、账户监控和通知流从 `server` / `engine` 的混合边界中拆出来，并让 `server` 回到 transport 与 runtime host 角色。

**Architecture:** 实现按 owner 迁移顺序推进，而不是按文件大小处理。先建立 `poise-application` 和它拥有的类型、store 契约、通知流；再让 `poise-storage` 实现这些 application-owned stores 并同步瘦身 `engine::ports`；接着迁移 query、diagnostics、account monitor 和写侧服务；最后替换 `ServerState` 并把 `runtime`、`effect_worker` 拆成按稳定职责组织的目录模块。整个过程不引入 projection store，不重写 `TrackManager` 状态推进，不改对外 HTTP / WebSocket 协议语义。

**Tech Stack:** Rust workspace, Cargo, Tokio, Axum, Rusqlite, Serde, anyhow

**Baseline:** `cargo test --workspace --quiet` 和 `cargo build -p poise-server` 在 2026-04-06 当前基线通过，可直接作为每个 task 的最终回归命令。

---

## File Structure

### 新增 crate

- `application/Cargo.toml`
  - 新 application crate 依赖声明
- `application/src/lib.rs`
  - 统一导出 application-owned models、stores、services、notifications

### `poise-application` 新增文件

- `application/src/notifications.rs`
  - `ApplicationNotification`
- `application/src/track_persistence.rs`
  - `PersistedTrackEffect`、`EffectStatus`、`EffectStatusUpdate`、`FollowUpRetirementRequest`、`StoredTrackEvent`、`StoredTrackSnapshot`
- `application/src/track_query_store.rs`
  - `TrackQueryStore`
- `application/src/track_effect_store.rs`
  - `TrackEffectStore`
- `application/src/track_mutation_store.rs`
  - `TrackMutationStore`
- `application/src/account_monitor_store.rs`
  - `AccountMonitorStore`
- `application/src/read_model.rs`
  - `TrackReadModel`
- `application/src/account_read_model.rs`
  - `AccountReadModel`
- `application/src/diagnostics.rs`
  - application 层 diagnostics model
- `application/src/query_service.rs`
  - `TrackQueryService`
- `application/src/debug_query_service.rs`
  - `TrackDebugQueryService`
- `application/src/account_monitor.rs`
  - `AccountMonitor`
- `application/src/mutation_executor.rs`
  - 共享写侧事务执行器
- `application/src/track_command_service.rs`
  - `TrackCommandService`
- `application/src/track_observation_service.rs`
  - `TrackObservationService`
- `application/src/track_effect_service.rs`
  - `TrackEffectService`

### `server` 侧新增或重组文件

- `server/src/server_context.rs`
  - `HttpState`、`WebSocketState`、`RuntimeState`、`EffectWorkerState`
- `server/src/runtime/mod.rs`
  - runtime 主入口和装配
- `server/src/runtime/startup_sync.rs`
  - 启动恢复与对齐
- `server/src/runtime/market_data.rs`
  - market data 消费
- `server/src/runtime/user_data.rs`
  - user data 消费
- `server/src/runtime/reconcile.rs`
  - reconcile 相关运行流程
- `server/src/runtime/account_refresh.rs`
  - 账户刷新
- `server/src/runtime/guards.rs`
  - runtime 侧 guard / preflight 协调
- `server/src/effect_worker/mod.rs`
  - worker 主入口和装配
- `server/src/effect_worker/dispatch.rs`
  - effect 选择与 dispatch
- `server/src/effect_worker/execute.rs`
  - submit / cancel 执行
- `server/src/effect_worker/retry.rs`
  - 错误分类、重试与 retirement 协调

### 重点修改文件

- `Cargo.toml`
  - workspace members 增加 `application`
- `server/Cargo.toml`
  - 新增 `poise-application` 依赖
- `storage/Cargo.toml`
  - 新增 `poise-application` 依赖
- `engine/src/ports.rs`
  - 删掉 application-owned 持久化记录类型和 query / effect queue 契约
- `storage/src/sqlite.rs`
  - 改为实现 `TrackMutationStore`、`TrackQueryStore`、`TrackEffectStore`
- `server/src/state_bootstrap.rs`
  - `StateRepositories` 暴露 application-owned stores
- `server/src/assembly.rs`
  - 装配 `poise-application` 服务与角色化 context
- `server/src/http.rs`
  - 改依赖 `HttpState` 与 application services
- `server/src/websocket.rs`
  - 改依赖 `WebSocketState` 与 application notifications
- `server/src/projector.rs`
  - 改消费 `poise_application::read_model::TrackReadModel`
- `server/src/account_projector.rs`
  - 改消费 `poise_application::account_read_model::AccountReadModel`
- `server/src/event_presentation.rs`
  - 改消费 application models / diagnostics models，而不是直接绑 `server` 私有类型
- `server/src/main.rs`
  - 模块声明、装配路径和目录模块入口更新

### 迁移后删除的旧文件

- 说明：以下文件名反映计划制定时的迁移目标，其中 `server/src/write_service.rs` 已在本轮实现中删除
- `server/src/notifications.rs`
- `server/src/read_model.rs`
- `server/src/account_read_model.rs`
- `server/src/query_service.rs`
- `server/src/debug_query_service.rs`
- `server/src/account_monitor.rs`
- `server/src/account_monitor_store.rs`
- `server/src/write_service.rs`
- `server/src/runtime.rs`
- `server/src/effect_worker.rs`

### 实施约束

- 每个 task 先写失败测试，再写最小实现
- 每个 task 验收通过后必须立即提交，并把 commit SHA 回写到本计划
- 未完成 `git add`、`git commit` 和计划回写，不得开始下一个 task
- 最终验收至少包含 `cargo test --workspace --quiet` 和 `cargo build -p poise-server`
- 除修复重构过程中暴露的明确 bug 外，不允许引入新的业务规则、策略口径或对外行为变化
- 不因为“文件太大”单独拆 `engine/src/manager.rs`；只有在前面任务已经证明它仍混合多类变化原因时，才另起 spec / plan 处理

---

### Task 1: 建立 `poise-application` crate，并迁移 owner 正确的共享类型

**Files:**
- Create: `application/Cargo.toml`
- Create: `application/src/lib.rs`
- Create: `application/src/notifications.rs`
- Create: `application/src/track_persistence.rs`
- Create: `application/src/track_query_store.rs`
- Create: `application/src/track_effect_store.rs`
- Create: `application/src/track_mutation_store.rs`
- Create: `application/src/account_monitor_store.rs`
- Create: `application/src/read_model.rs`
- Create: `application/src/account_read_model.rs`
- Create: `application/src/diagnostics.rs`
- Modify: `Cargo.toml`
- Modify: `server/Cargo.toml`
- Modify: `storage/Cargo.toml`
- Modify: `server/src/main.rs`
- Test: `application/src/read_model.rs`
- Test: `application/src/diagnostics.rs`

- [ ] **Step 1: 先把现有模型测试搬到新 owner，形成失败测试**

在 `application/src/read_model.rs` 放入从 `server/src/query_service.rs` 迁出的 `TrackReadModel::from_snapshot(...)` 测试，在 `application/src/diagnostics.rs` 放入 diagnostics model 的最小构造测试。例如：

```rust
#[test]
fn read_model_from_snapshot_flattens_runtime_state() {
    let read_model = TrackReadModel::from_snapshot(
        test_snapshot(),
        updated_at(),
        vec![test_event()],
        vec![test_effect()],
    );

    assert_eq!(read_model.track_id, "btc-core");
    assert_eq!(read_model.symbol, "BTCUSDT");
    assert_eq!(read_model.recent_effects.len(), 1);
}
```

- [ ] **Step 2: 运行定向测试，确认当前还没有目标 crate 和 owner**

Run:
`cargo test -p poise-application read_model::tests::read_model_from_snapshot_flattens_runtime_state -- --exact`

Expected:
- FAIL，原因是 `poise-application` crate 尚不存在，或 `TrackReadModel` 仍在 `server`

- [ ] **Step 3: 创建 crate 并迁移共享类型**

要求：
- 在 workspace 中加入 `application`
- 将这些类型迁入 `poise-application`：
  - `ApplicationNotification`
  - `PersistedTrackEffect`
  - `EffectStatus`
  - `EffectStatusUpdate`
  - `FollowUpRetirementRequest`
  - `StoredTrackEvent`
  - `StoredTrackSnapshot`
  - `TrackReadModel`
  - `AccountReadModel`
  - diagnostics model
- `server/src/main.rs` 先只改模块声明和依赖，允许旧实现继续工作到下一任务

- [ ] **Step 4: 运行新 crate 的模型测试和编译回归**

Run:
`cargo test -p poise-application --lib`

Expected:
- PASS，`poise-application` 已能独立编译并承载 application-owned types

- [ ] **Step 5: 提交并回写 SHA**

```bash
git add Cargo.toml server/Cargo.toml storage/Cargo.toml application/Cargo.toml application/src server/src/main.rs
git commit -m "refactor: introduce poise-application shared domain boundary"
```

Task 1 code commit:
`9e64514`

---

### Task 2: 让 `poise-storage` 实现 application-owned stores，并瘦身 `engine::ports`

**Files:**
- Modify: `engine/src/ports.rs`
- Modify: `storage/src/sqlite.rs`
- Modify: `storage/Cargo.toml`
- Modify: `server/src/state_bootstrap.rs`
- Modify: `server/src/assembly.rs`
- Modify: `application/src/track_persistence.rs`
- Modify: `application/src/track_query_store.rs`
- Modify: `application/src/track_effect_store.rs`
- Modify: `application/src/track_mutation_store.rs`
- Test: `storage/src/sqlite.rs`
- Test: `server/src/state_bootstrap.rs`

- [ ] **Step 1: 先写失败测试，固定 `SqliteStorage` 改实现 application-owned stores**

在 `storage/src/sqlite.rs` 新增或改写测试，让测试直接通过 application store trait 使用 SQLite，而不是通过 `poise_engine::ports::TrackReadRepositoryPort` 或旧 `StateRepositoryPort` 扩展能力。例如：

```rust
#[tokio::test]
async fn sqlite_storage_lists_recent_track_effects_via_track_query_store() {
    let storage = SqliteStorage::in_memory().unwrap();
    seed_track_snapshot_and_effects(&storage).await;

    let effects = TrackQueryStore::list_recent_track_effects(
        &storage,
        &TrackId::new("btc-core"),
        10,
    )
    .await
    .unwrap();

    assert_eq!(effects.len(), 1);
}
```

在 `server/src/state_bootstrap.rs` 增加测试，固定 `StateRepositories` 暴露三类 application store。

- [ ] **Step 2: 运行定向测试，确认当前边界还没迁过去**

Run:
`cargo test -p poise-storage sqlite::tests::sqlite_storage_lists_recent_track_effects_via_track_query_store -- --exact`

Expected:
- FAIL，原因是 `SqliteStorage` 还没有实现 `TrackQueryStore`

Run:
`cargo test -p poise-server state_bootstrap::tests::prepared_state_exposes_application_owned_stores -- --exact`

Expected:
- FAIL，原因是 `StateRepositories` 仍暴露旧组合仓库形状

- [ ] **Step 3: 实现 store 迁移和端口瘦身**

要求：
- `engine/src/ports.rs` 只保留：
  - exchange / market data / clock ports
  - 运行态推进真正需要的最小写侧能力
- 删除 `TrackReadRepositoryPort`
- 删除 `StateRepositoryPort` 中的 query / effect queue / retirement 能力
- `SqliteStorage` 改为分别实现：
  - `TrackMutationStore`
  - `TrackQueryStore`
  - `TrackEffectStore`
  - `AccountMonitorStore`
- `server/src/state_bootstrap.rs` 的 `StateRepositories` 改成显式字段，而不是继续依赖一个总仓库接口

- [ ] **Step 4: 运行 storage 与 bootstrap 回归**

Run:
`cargo test -p poise-storage`

Expected:
- PASS，SQLite 落地已完全通过 application-owned stores 暴露

Run:
`cargo test -p poise-server state_bootstrap::tests:: -- --nocapture`

Expected:
- PASS，启动准备路径能拿到 mutation/query/effect/account 四类 stores

- [ ] **Step 5: 提交并回写 SHA**

```bash
git add engine/src/ports.rs storage/Cargo.toml storage/src/sqlite.rs server/src/state_bootstrap.rs server/src/assembly.rs application/src/track_persistence.rs application/src/track_query_store.rs application/src/track_effect_store.rs application/src/track_mutation_store.rs
git commit -m "refactor: move persistence and queue stores under poise-application"
```

Task 2 code commit:
`ac46b3b`

---

### Task 3: 把 query、diagnostics 和 account monitor 迁入 `poise-application`

**Files:**
- Create: `application/src/query_service.rs`
- Create: `application/src/debug_query_service.rs`
- Create: `application/src/account_monitor.rs`
- Modify: `application/src/lib.rs`
- Modify: `server/src/http.rs`
- Modify: `server/src/websocket.rs`
- Modify: `server/src/projector.rs`
- Modify: `server/src/account_projector.rs`
- Modify: `server/src/event_presentation.rs`
- Modify: `server/src/assembly.rs`
- Modify: `server/src/main.rs`
- Delete: `server/src/query_service.rs`
- Delete: `server/src/debug_query_service.rs`
- Delete: `server/src/account_monitor.rs`
- Delete: `server/src/account_monitor_store.rs`
- Delete: `server/src/read_model.rs`
- Delete: `server/src/account_read_model.rs`
- Delete: `server/src/notifications.rs`
- Test: `application/src/query_service.rs`
- Test: `application/src/debug_query_service.rs`
- Test: `application/src/account_monitor.rs`
- Test: `server/src/http.rs`
- Test: `server/src/websocket.rs`

- [ ] **Step 1: 先迁测试，固定服务 owner 和 protocol 边界**

把这些现有测试迁到新 crate：
- `server/src/query_service.rs` 的 query 组装测试
- `server/src/debug_query_service.rs` 的 diagnostics 选择测试
- `server/src/account_monitor.rs` 的账户监控状态与通知测试

同时在 `server/src/http.rs` 或 `server/src/websocket.rs` 新增一个适配层测试，固定 `server` 只负责把 application model 映射到 protocol DTO。例如：

```rust
#[tokio::test]
async fn track_detail_endpoint_projects_application_read_model() {
    let app_service = test_query_service_returning_application_model();
    let response = app_with_service(app_service)
        .oneshot(track_detail_request("btc-core"))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}
```

- [ ] **Step 2: 运行定向测试，确认当前实现仍耦合在 `server`**

Run:
`cargo test -p poise-application query_service::tests::list_track_sources_reads_all_registered_snapshots -- --exact`

Expected:
- FAIL，原因是 `TrackQueryService` 还在 `server`

Run:
`cargo test -p poise-server http::tests::track_detail_endpoint_projects_application_read_model -- --exact`

Expected:
- FAIL，原因是 HTTP 仍直接依赖 `server` 私有 query service / read model

- [ ] **Step 3: 实现服务迁移和 adapter 分层**

要求：
- 将 `TrackQueryService`、`TrackDebugQueryService`、`AccountMonitor` 迁入 `poise-application`
- `TrackDebugQueryService` 改返回 application-owned diagnostics model，而不是 `poise-protocol` DTO
- `server` 侧保留：
  - `projector`
  - `account_projector`
  - `event_presentation` 或同职责 adapter
- 所有 protocol DTO 映射都留在 `server`
- 删除已被替代的 `server` 私有 models / notifications / service 文件

- [ ] **Step 4: 运行 application 和 server 的针对性回归**

Run:
`cargo test -p poise-application query_service::tests:: -- --nocapture`

Expected:
- PASS

Run:
`cargo test -p poise-application debug_query_service::tests:: -- --nocapture`

Expected:
- PASS

Run:
`cargo test -p poise-application account_monitor::tests:: -- --nocapture`

Expected:
- PASS

Run:
`cargo test -p poise-server http::tests:: -- --nocapture`
`cargo test -p poise-server websocket::tests:: -- --nocapture`

Expected:
- PASS，`server` 仅承担 transport adapter 职责

- [ ] **Step 5: 提交并回写 SHA**

```bash
git add application/src/query_service.rs application/src/debug_query_service.rs application/src/account_monitor.rs application/src/lib.rs server/src/http.rs server/src/websocket.rs server/src/projector.rs server/src/account_projector.rs server/src/event_presentation.rs server/src/assembly.rs server/src/main.rs
git rm server/src/query_service.rs server/src/debug_query_service.rs server/src/account_monitor.rs server/src/account_monitor_store.rs server/src/read_model.rs server/src/account_read_model.rs server/src/notifications.rs
git commit -m "refactor: move query and account application services into poise-application"
```

Task 3 code commit:
`ffa4985`

---

### Task 4: 把写侧拆成 command / observation / effect services，并引入 `MutationExecutor`

**Files:**
- Create: `application/src/mutation_executor.rs`
- Create: `application/src/track_command_service.rs`
- Create: `application/src/track_observation_service.rs`
- Create: `application/src/track_effect_service.rs`
- Modify: `application/src/lib.rs`
- Modify: `server/src/assembly.rs`
- Modify: `server/src/runtime.rs` or `server/src/runtime/mod.rs`
- Modify: `server/src/effect_worker.rs` or `server/src/effect_worker/mod.rs`
- Modify: `server/src/http.rs`
- Delete: `server/src/write_service.rs`
- Test: `application/src/track_command_service.rs`
- Test: `application/src/track_observation_service.rs`
- Test: `application/src/track_effect_service.rs`
- Test: `server/src/runtime.rs` or `server/src/runtime/mod.rs`
- Test: `server/src/effect_worker.rs` or `server/src/effect_worker/mod.rs`

- [ ] **Step 1: 先写失败测试，固定三个 use-case 服务而不是总写服务**

至少覆盖三个代表性行为：

```rust
#[tokio::test]
async fn command_service_pause_persists_state_and_notifies() { /* ... */ }

#[tokio::test]
async fn observation_service_applies_price_tick_and_persists_effects() { /* ... */ }

#[tokio::test]
async fn effect_service_records_submit_failure_and_updates_effect_status() { /* ... */ }
```

再补一个执行器测试，锁住共享事务边界：

```rust
#[tokio::test]
async fn mutation_executor_rolls_back_when_store_commit_fails() { /* ... */ }
```

- [ ] **Step 2: 运行定向测试，确认当前仍只有 `TrackWriteService`**

Run:
`cargo test -p poise-application track_command_service::tests::command_service_pause_persists_state_and_notifies -- --exact`

Expected:
- FAIL，原因是 `TrackCommandService` 尚不存在

Run:
`cargo test -p poise-server runtime::tests:: -- --nocapture`

Expected:
- FAIL 或编译失败，提示 runtime 仍直接依赖旧 `TrackWriteService`

- [ ] **Step 3: 实现服务拆分和共享执行器**

要求：
- `MutationExecutor` 吸收：
  - per-track mutation lock
  - rollback
  - 调用 `TrackManager`
  - 调用 `TrackMutationStore`
  - 发布 `ApplicationNotification`
  - account margin guard 协调
- `TrackCommandService` 只暴露用户命令
- `TrackObservationService` 只暴露外部事实输入
- `TrackEffectService` 只暴露 effect 准备、writeback、retirement 相关入口
- `server` 所有调用点改为依赖这三个服务
- 删除 `server/src/write_service.rs`

- [ ] **Step 4: 运行写侧与运行时回归**

Run:
`cargo test -p poise-application track_command_service::tests:: -- --nocapture`
`cargo test -p poise-application track_observation_service::tests:: -- --nocapture`
`cargo test -p poise-application track_effect_service::tests:: -- --nocapture`

Expected:
- PASS

Run:
`cargo test -p poise-server runtime::tests:: -- --nocapture`
`cargo test -p poise-server effect_worker::tests:: -- --nocapture`

Expected:
- PASS，runtime 与 worker 已只依赖对应的 application services

- [ ] **Step 5: 提交并回写 SHA**

```bash
git add application/src/mutation_executor.rs application/src/track_command_service.rs application/src/track_observation_service.rs application/src/track_effect_service.rs application/src/lib.rs server/src/assembly.rs server/src/http.rs
git add server/src/runtime.rs server/src/effect_worker.rs
git rm server/src/write_service.rs
git commit -m "refactor: split track write paths into application services"
```

Task 4 code commit:
`ea9581e`

---

### Task 5: 用角色化 context 替换 `ServerState`，并把 `runtime` / `effect_worker` 拆成稳定子域模块

**Files:**
- Create: `server/src/server_context.rs`
- Create: `server/src/runtime/mod.rs`
- Create: `server/src/runtime/startup_sync.rs`
- Create: `server/src/runtime/market_data.rs`
- Create: `server/src/runtime/user_data.rs`
- Create: `server/src/runtime/reconcile.rs`
- Create: `server/src/runtime/account_refresh.rs`
- Create: `server/src/runtime/guards.rs`
- Create: `server/src/effect_worker/mod.rs`
- Create: `server/src/effect_worker/dispatch.rs`
- Create: `server/src/effect_worker/execute.rs`
- Create: `server/src/effect_worker/retry.rs`
- Modify: `server/src/assembly.rs`
- Modify: `server/src/http.rs`
- Modify: `server/src/websocket.rs`
- Modify: `server/src/main.rs`
- Delete: `server/src/runtime.rs`
- Delete: `server/src/effect_worker.rs`
- Test: `server/src/http.rs`
- Test: `server/src/websocket.rs`
- Test: `server/src/assembly.rs`
- Test: `server/src/runtime/mod.rs`
- Test: `server/src/effect_worker/mod.rs`

- [x] **Step 1: 先写失败测试，固定每个角色只拿自己需要的依赖**

增加或改写这些测试：

```rust
#[tokio::test]
async fn router_accepts_http_state_without_runtime_dependencies() { /* ... */ }

#[tokio::test]
async fn websocket_accepts_websocket_state_without_effect_worker_dependencies() { /* ... */ }

#[tokio::test]
async fn runtime_state_exposes_observation_and_account_paths_only() { /* ... */ }

#[tokio::test]
async fn effect_worker_state_exposes_effect_execution_paths_only() { /* ... */ }
```

- [x] **Step 2: 运行定向测试，确认当前仍被 `ServerState` 绑定**

Run:
`cargo test -p poise-server http::tests::router_accepts_http_state_without_runtime_dependencies -- --exact`

Expected:
- FAIL，原因是 router 仍接收 `ServerState`

Run:
`cargo test -p poise-server assembly::tests:: -- --nocapture`

Expected:
- FAIL 或编译失败，提示 assembly 仍导出统一 `ServerState`

- [x] **Step 3: 实现角色化 context 和目录模块拆分**

要求：
- 新增 `HttpState`、`WebSocketState`、`RuntimeState`、`EffectWorkerState`
- `assembly` 只负责装配这些 context 和 runtime host
- `runtime` 目录按稳定职责拆分：
  - startup / recovery
  - market data
  - user data
  - reconcile
  - account refresh
  - guards / preflight
- `effect_worker` 目录按稳定职责拆分：
  - dispatch
  - execute
  - retry / retirement
- 顶层 `mod.rs` 只保留公开入口和少量编排

- [x] **Step 4: 运行 server 包的角色与模块边界回归**

Run:
`cargo test -p poise-server http::tests:: -- --nocapture`
`cargo test -p poise-server websocket::tests:: -- --nocapture`
`cargo test -p poise-server runtime::tests:: -- --nocapture`
`cargo test -p poise-server effect_worker::tests:: -- --nocapture`
`cargo test -p poise-server assembly::tests:: -- --nocapture`

Expected:
- PASS，`ServerState` 已删除，`runtime` / `effect_worker` 已按稳定职责组织

- [x] **Step 5: 提交并回写 SHA**

```bash
git add server/src/server_context.rs server/src/runtime server/src/effect_worker server/src/assembly.rs server/src/http.rs server/src/websocket.rs server/src/main.rs
git rm server/src/runtime.rs server/src/effect_worker.rs
git commit -m "refactor: replace server state bag with role-specific contexts"
```

Task 5 code commit:
`083b5e5`

---

### Task 6: 全量验收、清理死代码并回写文档

**Files:**
- Modify: `docs/superpowers/plans/2026-04-06-server-boundary-convergence.md`
- Modify: `docs/superpowers/specs/2026-04-06-server-boundary-convergence-design.md` (only if implementation names drift)
- Modify: `Cargo.toml` / crate `Cargo.toml` files (only if final cleanup needed)

- [x] **Step 1: 做一次死代码和边界残留检查**

Run:
`rg -n "ServerState|TrackWriteService|TrackReadRepositoryPort|ServerNotification|server::read_model|server::query_service" .`

Expected:
- 只剩计划文档或 spec 中的历史描述
- 生产代码中不存在这些旧 owner / 旧入口

- [x] **Step 2: 运行格式化和 workspace 全量验收**

Run:
`cargo fmt --all`

Run:
`cargo test --workspace --quiet`

Expected:
- PASS，workspace 全绿

- [x] **Step 3: 回写计划勾选状态和 commit SHA**

要求：
- 把本文件所有已完成步骤勾选
- 把每个 task 的 commit SHA 回填到对应位置
- 如果实现文件名与 spec 有偏差，更新 spec 文档中的最终命名

- [x] **Step 4: 提交最终清理**

```bash
git add docs/superpowers/plans/2026-04-06-server-boundary-convergence.md docs/superpowers/specs/2026-04-06-server-boundary-convergence-design.md Cargo.toml application/Cargo.toml server/Cargo.toml storage/Cargo.toml
git commit -m "docs: finalize server boundary convergence implementation notes"
```

Task 6 code commit:
`33c5afd`

---

## Acceptance Checklist

- [x] `poise-application` 已成为 query、effect store、mutation store、account monitor store、notifications、read models、diagnostics 和写侧服务的 owner
- [x] `engine::ports` 不再承载 read-side 和 effect queue / follow-up retirement 契约
- [x] `poise-storage` 通过 application-owned stores 暴露 SQLite 能力
- [x] `server` 不再持有 `ServerState`
- [x] `server` 不再持有 `TrackWriteService`
- [x] `runtime` 与 `effect_worker` 已拆成按稳定职责组织的目录模块
- [x] `TrackDebugQueryService` 返回 application model，protocol DTO 映射留在 `server`
- [x] `cargo test --workspace --quiet` 通过
