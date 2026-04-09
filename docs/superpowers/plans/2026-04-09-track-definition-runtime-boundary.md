# Track Definition 与 Runtime 边界重构 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在 `main` 基线上实现最新的 `Track Definition` 与 `Runtime` 边界设计，让 definition、persisted runtime、bootstrap、query 各自拥有单一 owner。

**Architecture:** 先在 `poise-application` 建立 definition 语义对象与 `PreparedTrackRegistry`，并立即把 registry owner 前移到 `server::state_bootstrap`，让 `assembly` 不再直接从 `server::config` 派生业务语义。然后把 query 改成消费 `TrackReadSource`，再把 `poise-engine` 的 persisted runtime 收敛成 runtime-only snapshot 与 restore primitives，并为 `Strict` 启动补一个最小的 persisted track presence 事实，最后由 `state_bootstrap` 统一编排 restore revision、post-restore constraints 和缺失 runtime seed。整个实现保持 storage 不持久化完整 definition，也不恢复 `track_definitions` 或 `TrackBudgetCatalog` 这类已废弃路径。

**Tech Stack:** Rust workspace (`poise-application`, `poise-engine`, `poise-storage`, `poise-server`), `serde`, `tokio`, `rusqlite`, crate 内单元测试与模块测试。

---

## 相关设计

- `../specs/2026-04-09-track-definition-runtime-boundary-design.md`

## 文件边界

- Create: `application/src/track_definition.rs`
  负责 `ConfiguredTrackInput`、`ConfiguredTrackDefinition`、`TrackPreparedDefinition`、`TrackReadDefinition`、`PreparedTrackRegistry`。
- Create: `application/src/track_read_source.rs`
  负责 `TrackRuntimeReadState`、`TrackReadSource` 以及 runtime -> query-facing 投影。
- Create: `engine/src/persisted_runtime.rs`
  负责 `TrackRestoreRevision`、`TrackRuntimeSeed`、`PostRestoreConstraints`、`PersistedRuntimeCodec`。
- Modify: `application/src/lib.rs`
  导出新的 definition / read source 类型，删除 `TrackBudgetCatalog` 等临时接口。
- Modify: `application/src/query_service.rs`
  改成依赖 `PreparedTrackRegistry + TrackQueryStore`，不再自己维护 budget catalog。
- Modify: `application/src/read_model.rs`
  `TrackReadModel::from_source(...)` 只消费 `TrackReadSource`。
- Modify: `application/src/track_persistence.rs`
  把 `StoredTrackSnapshot` 收敛成 runtime-only persisted 记录类型。
- Modify: `application/src/track_query_store.rs`
  查询接口只返回 persisted runtime snapshot，不再承担 definition。
- Modify: `engine/src/snapshot.rs`
  `TrackRuntimeSnapshot` 改为 runtime-only，并接 `PersistedRuntimeCodec`。
- Modify: `engine/src/runtime.rs`
  增加 `initial_from_seed(...)`、`restore_from_snapshot(...)` 的新语义和 `apply_post_restore_constraints(...)`。
- Modify: `engine/src/manager.rs`
  恢复与 snapshot 交互改成基于 revision / runtime-only snapshot。
- Modify: `engine/src/lib.rs`
  导出 persisted runtime 模块。
- Modify: `storage/src/schema.rs`
  `track_snapshots` 写路径改为 runtime-only 所需列，增加 `restore_revision` 和最小 persisted track presence 持久化支持。
- Modify: `storage/src/sqlite.rs`
  所有 save/load/list 走 runtime-only snapshot 和 `PersistedRuntimeCodec`，并在同一个事务里维护 persisted track presence。
- Modify: `server/src/config.rs`
  定义 `TrackFileDefinition`，并只做 raw file -> `ConfiguredTrackInput` 的机械映射。
- Modify: `server/src/state_bootstrap.rs`
  构造 `PreparedTrackRegistry`，比较 restore revision，产出新的结构化 mismatch payload，应用 post-restore constraints，补写缺失 runtime。
- Modify: `server/src/assembly.rs`
  只消费 registry + runtime repository，不再重复派生 definition / budget。
- Modify: `server/src/main.rs`
  接新 bootstrap 返回值与 strict / rebuild 语义，并渲染 revision/presence/runtime-only mismatch 细节。
- Modify: `server/src/http.rs`
  测试和 helper 适配新 query service / read source。
- Modify: `server/src/websocket.rs`
  测试和 helper 适配新 query service / read source。
- Modify: `server/src/test_support.rs`
  提供 registry / configured definition 测试辅助，而不是 budget catalog。
- Modify: `server/src/runtime/tests/support.rs`
  适配新 query service 与 runtime-only snapshot helper。

## 执行约束

- 先写失败测试，再做最小实现。
- 每个 task 验收通过后必须立即 `git add`、`git commit`，并把 commit SHA 回写到本计划。
- 未完成 `git add`、`git commit` 和任务清单回写前，不开始下一个 task。
- 不恢复 `stash@{0}`，也不重新引入 `TrackDefinitionStore`、`track_definitions`、`TrackBudgetCatalog` 这类已废弃路径。

### Task 1: 建立 definition pipeline，并把 registry owner 前移到 bootstrap

**Files:**

- Create: `application/src/track_definition.rs`
- Create: `engine/src/persisted_runtime.rs`
- Modify: `application/src/lib.rs`
- Modify: `engine/src/lib.rs`
- Modify: `server/src/config.rs`
- Modify: `server/src/state_bootstrap.rs`
- Modify: `server/src/assembly.rs`
- Modify: `server/src/main.rs`
- Modify: `server/src/test_support.rs`
- Test: `application/src/track_definition.rs`
- Test: `engine/src/persisted_runtime.rs`
- Test: `server/src/config.rs`
- Test: `server/src/state_bootstrap.rs`

- [x] **Step 1: 写失败测试，固定 normalize / registry / revision 的语义**

  在 `application/src/track_definition.rs` 增加测试，至少覆盖：
  - `ConfiguredTrackDefinition::try_from_input(...)` 会展开默认值并校验 `daily_loss_limit` / `total_loss_limit`
  - `PreparedTrackRegistry` 会为每个 track 产出 `TrackPreparedDefinition`
  - `TrackPreparedDefinition` 能稳定投影出 `TrackReadDefinition`、`TrackRuntimeSeed` 和 `PostRestoreConstraints`

  在 `engine/src/persisted_runtime.rs` 增加测试，至少覆盖：
  - `TrackRestoreRevisionV1` 对相同 `instrument + TrackConfig` 结果稳定
  - `CapacityBudget` 与 `tick_timeout_secs` 变化不会改变 revision

  在 `server/src/config.rs` 增加测试，固定：
  - `TrackFileDefinition` 只负责 TOML 形状
  - raw file record 可以机械映射到 `ConfiguredTrackInput`

  在 `server/src/state_bootstrap.rs` 增加测试，固定：
  - bootstrap 会构造 `PreparedTrackRegistry`
  - `PreparedStateStore` 返回后，`assembly` 不再需要直接读取 `TrackFileDefinition` 的业务 helper

- [x] **Step 2: 运行定向测试，确认它们先失败**

  Run:
  - `cargo test -p poise-application track_definition::tests:: -- --nocapture`
  - `cargo test -p poise-engine persisted_runtime::tests:: -- --nocapture`
  - `cargo test -p poise-server config::tests:: -- --nocapture`
  - `cargo test -p poise-server state_bootstrap::tests:: -- --nocapture`

  Expected:
  - 失败，原因是新模块 / 类型 / 映射入口尚不存在。

- [x] **Step 3: 实现 definition owner 与 revision primitives**

  最小实现：
  - 在 `application/src/track_definition.rs` 定义 `ConfiguredTrackInput`、`ConfiguredTrackDefinition`、`TrackPreparedDefinition`、`TrackReadDefinition`、`PreparedTrackRegistry`
  - 在 `engine/src/persisted_runtime.rs` 定义 `TrackRestoreRevision`、`TrackRuntimeSeed`、`PostRestoreConstraints`
  - `server/src/config.rs` 只保留 raw `TrackFileDefinition` 和到 `ConfiguredTrackInput` 的机械映射
  - `state_bootstrap` 构造并持有 `PreparedTrackRegistry`
  - `assembly` 改成消费 bootstrap 产出的 registry，而不是直接从 `server::config` 派生 `instrument` / `budget` / `tick_timeout_secs`
  - `application/src/lib.rs` / `engine/src/lib.rs` 导出新类型

- [x] **Step 4: 运行定向测试，确认新语义稳定**

  Run:
  - `cargo test -p poise-application track_definition::tests:: -- --nocapture`
  - `cargo test -p poise-engine persisted_runtime::tests:: -- --nocapture`
  - `cargo test -p poise-server config::tests:: -- --nocapture`
  - `cargo test -p poise-server state_bootstrap::tests:: -- --nocapture`

  Expected:
  - 通过，说明 definition normalize、registry 派生和 restore revision owner 已经定住。

- [x] **Step 5: 提交并回写 SHA**

  Run:
  - `git add application/src/track_definition.rs application/src/lib.rs engine/src/persisted_runtime.rs engine/src/lib.rs server/src/config.rs server/src/state_bootstrap.rs server/src/assembly.rs server/src/main.rs server/src/test_support.rs`
  - `git commit -m "refactor: bootstrap prepared track registry"`

  Commit SHA: `89f0423`

### Task 2: 重写 query-facing source 与读模型

**Files:**

- Create: `application/src/track_read_source.rs`
- Modify: `application/src/lib.rs`
- Modify: `application/src/query_service.rs`
- Modify: `application/src/read_model.rs`
- Modify: `application/src/track_query_store.rs`
- Modify: `server/src/assembly.rs`
- Modify: `server/src/http.rs`
- Modify: `server/src/websocket.rs`
- Modify: `server/src/test_support.rs`
- Modify: `server/src/runtime/tests/support.rs`
- Test: `application/src/query_service.rs`
- Test: `application/src/read_model.rs`
- Test: `server/src/http.rs`
- Test: `server/src/websocket.rs`

- [x] **Step 1: 写失败测试，固定 query 只从 registry + runtime 组合读侧 source**

  增加测试，至少覆盖：
  - `TrackQueryService` 从 `PreparedTrackRegistry` 读 `TrackReadDefinition`，从 `TrackQueryStore` 读 runtime 和 events/effects
  - `TrackReadModel::from_source(...)` 不再直接吃 `TrackRuntimeSnapshot`
  - HTTP / WebSocket 测试 helper 不再构造 `TrackBudgetCatalog`

- [x] **Step 2: 运行定向测试，确认当前实现还依赖旧 query 接口**

  Run:
  - `cargo test -p poise-application query_service::tests:: -- --nocapture`
  - `cargo test -p poise-application read_model::tests:: -- --nocapture`
  - `cargo test -p poise-server http::tests:: -- --nocapture`

  Expected:
  - 失败，原因是 query service 仍依赖 `TrackBudgetCatalog`，读模型仍直接消费 runtime snapshot。

- [x] **Step 3: 实现 `TrackReadSource` 路径并删除 `TrackBudgetCatalog`**

  最小实现：
  - 新增 `application/src/track_read_source.rs`
  - `TrackQueryService` 改成依赖 `PreparedTrackRegistry + TrackQueryStore`
  - `TrackReadModel::from_source(...)` 只消费 `TrackReadSource`
  - `server` 的 HTTP / WebSocket / test helper 改成消费 bootstrap 提供的 registry，而不是传 budget catalog

- [x] **Step 4: 运行定向测试，确认 query 边界已经改成 definition + runtime 组合**

  Run:
  - `cargo test -p poise-application query_service::tests:: -- --nocapture`
  - `cargo test -p poise-application read_model::tests:: -- --nocapture`
  - `cargo test -p poise-server http::tests::get_track_detail_returns_track_detail_view -- --exact`
  - `cargo test -p poise-server websocket::tests:: -- --nocapture`

  Expected:
  - 通过，说明 query 已不再依赖 `TrackBudgetCatalog` 和 restore artifact 直读。

- [x] **Step 5: 提交并回写 SHA**

  Run:
  - `git add application/src/track_read_source.rs application/src/lib.rs application/src/query_service.rs application/src/read_model.rs application/src/track_query_store.rs server/src/assembly.rs server/src/http.rs server/src/websocket.rs server/src/test_support.rs server/src/runtime/tests/support.rs`
  - `git commit -m "refactor: project tracks from prepared registry"`

  Commit SHA: `0b495e1`

### Task 3: 把 persisted runtime 改成 runtime-only snapshot，并补最小 persisted track presence

**Files:**

- Modify: `application/src/track_persistence.rs`
- Modify: `application/src/track_query_store.rs`
- Modify: `engine/src/persisted_runtime.rs`
- Modify: `engine/src/snapshot.rs`
- Modify: `engine/src/runtime.rs`
- Modify: `engine/src/manager.rs`
- Modify: `storage/src/schema.rs`
- Modify: `storage/src/sqlite.rs`
- Modify: `server/src/runtime/tests/support.rs`
- Test: `engine/src/persisted_runtime.rs`
- Test: `engine/src/snapshot.rs`
- Test: `engine/src/runtime.rs`
- Test: `storage/src/sqlite.rs`

- [x] **Step 1: 写失败测试，锁住 runtime-only snapshot、legacy codec 和 post-restore constraints**

  增加测试，至少覆盖：
  - persisted runtime 新快照不再包含 `instrument` / `config`
  - `PersistedRuntimeCodec::decode(...)` 能兼容旧 JSON snapshot 和 SQLite 旧行
  - `TrackRuntime::initial_from_seed(...)` 由 engine 默认值生成初始 runtime
  - `TrackRuntime::apply_post_restore_constraints(...)` 会在预算变化或 loss guard 触发时收敛 `desired_exposure`
  - `SqliteStorage` roundtrip 使用 runtime-only snapshot，旧行仍可读
  - `SqliteStorage` 会维护最小 persisted track presence 记录
  - 初始 runtime seed 写入时，不会出现“presence 已写入但 runtime 没写入”的持久化裂缝

- [x] **Step 2: 运行定向测试，确认旧 snapshot 结构会把这些测试打红**

  Run:
  - `cargo test -p poise-engine snapshot::tests:: -- --nocapture`
  - `cargo test -p poise-engine runtime::tests:: -- --nocapture`
  - `cargo test -p poise-storage sqlite::tests:: -- --nocapture`

  Expected:
  - 失败，原因是当前 snapshot 仍包含 definition 字段，storage 仍直接读写 `venue/symbol/config_json`，也还没有 persisted track presence。

- [x] **Step 3: 实现 runtime-only persisted artifact 与 codec**

  最小实现：
  - `TrackRuntimeSnapshot` 改为 runtime-only
  - `TrackRestoreRevision`、`TrackRuntimeSeed`、`PostRestoreConstraints` 在 engine 内闭合
  - `PersistedRuntimeCodec` 成为唯一 legacy decode 入口
  - `SqliteStorage` 写路径停止依赖 definition 字段，读路径统一经过 codec
  - `SqliteStorage` 在保存初始 runtime 和后续 runtime snapshot 时，同事务维护最小 persisted track presence
  - `application/src/track_persistence.rs` 改成 runtime-only persisted 记录类型

- [x] **Step 4: 运行定向测试，确认 persisted runtime 边界成立**

  Run:
  - `cargo test -p poise-engine snapshot::tests:: -- --nocapture`
  - `cargo test -p poise-engine runtime::tests:: -- --nocapture`
  - `cargo test -p poise-storage sqlite::tests:: -- --nocapture`

  Expected:
  - 通过，说明 runtime snapshot、legacy decode 和 storage 写读语义已经切到 runtime-only。

- [x] **Step 5: 提交并回写 SHA**

  Run:
  - `git add application/src/track_persistence.rs application/src/track_query_store.rs engine/src/persisted_runtime.rs engine/src/snapshot.rs engine/src/runtime.rs engine/src/manager.rs storage/src/schema.rs storage/src/sqlite.rs server/src/runtime/tests/support.rs`
  - `git commit -m "refactor: persist runtime-only track state"`

  Commit SHA: `73f2be9`

### Task 4: 完成 bootstrap 的 restore revision、presence 校验与 post-restore constraints

**Files:**

- Modify: `server/src/state_bootstrap.rs`
- Modify: `server/src/main.rs`
- Modify: `server/src/test_support.rs`
- Modify: `server/src/runtime/tests/support.rs`
- Test: `server/src/state_bootstrap.rs`
- Test: `server/src/main.rs`

- [x] **Step 1: 写失败测试，固定 strict mismatch、缺失 runtime seed 和 post-restore constraints 的启动语义**

  在 `server/src/state_bootstrap.rs` 增加测试，至少覆盖：
  - strict 模式下，persisted runtime 的 `restore_revision` 不匹配时返回 mismatch
  - strict 模式下，config 新增 track 且没有 persisted track presence 时不报 mismatch，而是补初始 snapshot
  - strict 模式下，track 已有 persisted track presence 但 runtime snapshot 丢失时返回 mismatch
  - strict 模式下，数据库里存在不在当前 config 中的 persisted track 时返回 mismatch
  - `CapacityBudget` 变化不触发 mismatch，但 bootstrap 返回前必须先应用 `post_restore_constraints`
  - repository 返回后已经 query-ready

  在 `server/src/main.rs` 增加测试，固定：
  - CLI 只渲染 `RestoreRevisionMismatch` / `PersistedTrackMissingRuntime` / `PersistedTrackMissingFromConfig`
  - 启动错误输出不再依赖 persisted `instrument/config` JSON

- [x] **Step 2: 运行定向测试，确认当前 bootstrap 仍在直接比较 `instrument/config`**

  Run:
  - `cargo test -p poise-server state_bootstrap::tests:: -- --nocapture`
  - `cargo test -p poise-server main::tests:: -- --nocapture`

  Expected:
  - 失败，原因是当前 bootstrap 还在直接读取 snapshot 中的 `instrument/config`，还没有 `restore_revision`、persisted track presence 和 `post_restore_constraints` 路径。

- [x] **Step 3: 实现 registry-aware bootstrap**

  最小实现：
  - `state_bootstrap` 读取 persisted track presence 和 persisted runtime
  - 通过 `restore_revision` 做 strict mismatch 判断
  - 用 persisted track presence 区分“新增 track”与“已知 track 的 runtime 丢失”
  - 对不在当前 config 中的 persisted track 返回 mismatch
  - `PersistedStateMismatchDetail` 改成只依赖 revision / presence / runtime 缺失的结构化事实
  - 对匹配项应用 `PostRestoreConstraints`
  - 为新增 track 写入 `initial_from_seed(...)`
  - `main` 改成只渲染新的 mismatch payload
  - 返回已经 query-ready 的 repositories + registry

- [x] **Step 4: 运行定向测试，确认启动边界符合设计**

  Run:
  - `cargo test -p poise-server state_bootstrap::tests:: -- --nocapture`
  - `cargo test -p poise-server render_startup_error_formats_structured_mismatch_for_cli -- --nocapture`
  - `cargo test -p poise-server runtime::tests::startup_sync:: -- --nocapture`

  Expected:
  - 通过，说明 strict mismatch、seed 缺失 runtime、budget 变化后的约束同步都已稳定。

- [x] **Step 5: 提交并回写 SHA**

  Run:
  - `git add Cargo.lock application/src/lib.rs application/src/query_service.rs application/src/read_model.rs application/src/track_definition.rs engine/src/lib.rs server/Cargo.toml server/src/assembly.rs server/src/config.rs server/src/effect_worker/tests/support.rs server/src/main.rs server/src/runtime/tests/reconcile.rs server/src/runtime/tests/support.rs server/src/runtime/tests/user_data.rs server/src/state_bootstrap.rs storage/src/sqlite.rs`
  - `git commit -m "refactor: finish bootstrap restore semantics"`

  Commit SHA: `bc8c757`

### Task 5: 删除旧接口痕迹并跑全量验收

**Files:**

- Modify: `application/src/lib.rs`
- Modify: `application/src/query_service.rs`
- Modify: `application/src/read_model.rs`
- Modify: `engine/src/lib.rs`
- Modify: `server/src/assembly.rs`
- Modify: `server/src/http.rs`
- Modify: `server/src/websocket.rs`
- Modify: `README.md`
- Modify: `configs/binance-testnet.demo.toml`
- Modify: `configs/test.demo.toml`

- [ ] **Step 1: 写最后一组守卫测试 / 搜索检查**

  增加或补齐测试，固定：
  - 不再导出或引用 `TrackBudgetCatalog`
  - query 层不再直接消费 `TrackRuntimeSnapshot`
  - persisted snapshot 不再要求 definition 字段
  - `Strict` 模式下不会把“已知 track 的 runtime 丢失”误判成新增 track
  - `Strict` 模式下不会忽略不在当前 config 中的旧 persisted track

  额外执行搜索检查：
  - `rg "TrackBudgetCatalog|StoredTrackSnapshot|track_definitions" application server storage`

- [ ] **Step 2: 清理旧接口和文档示例**

  删除：
  - `TrackBudgetCatalog`
  - 旧的 snapshot definition 依赖
  - 过时的 helper / 测试夹具

  同步：
  - `README.md`
  - `configs/binance-testnet.demo.toml`
  - `configs/test.demo.toml`

- [ ] **Step 3: 跑格式化和 workspace 全量验收**

  Run:
  - `cargo fmt --all --check`
  - `cargo test --workspace`

  Expected:
  - 全绿；如果失败，先修回归，再重复这一组验收。

- [ ] **Step 4: 回写计划结果**

  把 Task 1-5 的 commit SHA 回写到本计划对应位置，补充最终验收命令结果。

- [ ] **Step 5: 提交并回写 SHA**

  Run:
  - `git add application/src/lib.rs application/src/query_service.rs application/src/read_model.rs engine/src/lib.rs server/src/assembly.rs server/src/http.rs server/src/websocket.rs README.md configs/binance-testnet.demo.toml configs/test.demo.toml docs/superpowers/specs/2026-04-09-track-definition-runtime-boundary-design.md docs/superpowers/plans/2026-04-09-track-definition-runtime-boundary.md`
  - `git commit -m "refactor: finish track definition runtime boundary"`

  Commit SHA: `<pending>`
