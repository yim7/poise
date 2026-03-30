# Poise / Track 全量改名 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把当前系统从 `grid-platform` / `grid-*` / `Grid*` 无兼容层地全量收敛到 `Poise` / `poise-*` / `Track*`，并同步更新配置、协议、存储、测试与文档。

**Architecture:** 这次不是功能改造，而是一次破坏性命名收敛。实现按四层推进：先收 package 与 binary，再收领域类型与模块，再收配置 / 协议 / 路由 / TUI，最后收 SQLite schema 与文档。仓库根目录改名放到所有 git 变更完成并提交后再执行，避免当前工作区路径中途失效。

**Tech Stack:** Rust workspace, Cargo, Tokio, Axum, Rusqlite, Serde, Ratatui, Markdown

---

## File Structure

### 重点修改目录

- `Cargo.toml`：workspace 成员与共享依赖入口
- `core/`：`grid-core` -> `poise-core`，`GridConfig` 等基础领域类型收敛到 `Track*`
- `engine/`：`grid.rs` 模块改为 `track.rs`，`GridId` / `GridManager` / `GridRuntime` 等统一改为 `Track*`
- `storage/`：SQLite schema、表名、列名和持久化读写从 `grid_*` 切到 `track_*`
- `protocol/`：DTO 和 JSON 字段从 `Grid*` / `grid_id` / `/grids` 切到 `Track*` / `track_id` / `/tracks`
- `server/`：配置结构、HTTP / WS 路由、projector、query/read/write service、运行入口和默认数据库文件名
- `tui/`：客户端 DTO、HTTP / WS 请求路径、环境变量、binary 名和 fixture 文件名
- `configs/`：`[[grids]]` / `grid_id` 改成 `[[tracks]]` / `track_id`
- `README.md` 与 `docs/`：当前入口文档、架构 spec、协议文档、当前主线计划和所有仍描述现行行为的文档

### 关键文件分组

- Package / binary：
  - `Cargo.toml`
  - `core/Cargo.toml`
  - `engine/Cargo.toml`
  - `storage/Cargo.toml`
  - `protocol/Cargo.toml`
  - `server/Cargo.toml`
  - `tui/Cargo.toml`
  - `exchanges/binance/Cargo.toml`
- 领域与模块：
  - `core/src/strategy.rs`
  - `engine/src/lib.rs`
  - `engine/src/grid.rs`
  - `engine/src/manager.rs`
  - `engine/src/runtime.rs`
  - `engine/src/snapshot.rs`
  - `engine/src/ports.rs`
  - `engine/src/transition.rs`
  - `engine/src/observation.rs`
- 协议与入口：
  - `protocol/src/lib.rs`
  - `server/src/config.rs`
  - `server/src/http.rs`
  - `server/src/websocket.rs`
  - `server/src/projector.rs`
  - `server/src/read_model.rs`
  - `server/src/main.rs`
  - `tui/src/api_client.rs`
  - `tui/src/protocol.rs`
  - `tui/src/main.rs`
  - `tui/src/views/*.rs`
- 存储：
  - `storage/src/schema.rs`
  - `storage/src/sqlite.rs`
- 文档与示例：
  - `README.md`
  - `configs/binance-testnet.toml`
  - `configs/test.toml`
  - `docs/protocol-contract.md`
  - `docs/2026-03-30-architecture-review.md`
  - `docs/grid-strategy-product-theory-research.md`
  - `docs/superpowers/specs/*.md`
  - `docs/superpowers/plans/*.md`

### 验收约束

- 每个 task 验收通过后必须立即提交，并把 commit SHA 回写到本计划
- 不得在未提交前开始下一个 task
- 仓库根目录改名不属于 git tracked 改动，放到全部 task 完成后的工作区切换步骤执行

---

### Task 1: 收敛 workspace / package / binary 名到 `poise-*`

**Files:**
- Modify: `Cargo.toml`
- Modify: `core/Cargo.toml`
- Modify: `engine/Cargo.toml`
- Modify: `storage/Cargo.toml`
- Modify: `protocol/Cargo.toml`
- Modify: `server/Cargo.toml`
- Modify: `tui/Cargo.toml`
- Modify: `exchanges/binance/Cargo.toml`
- Modify: `server/src/main.rs`
- Modify: `tui/src/main.rs`
- Test: `cargo metadata`
- Test: `cargo check -p poise-core`
- Test: `cargo check -p poise-engine`
- Test: `cargo check -p poise-server`
- Test: `cargo check -p poise-tui`

- [x] **Step 1: 先运行 package 解析命令，确认新包名当前不存在**

Run:
`cargo check -p poise-server`

Expected:
失败，并提示 package `poise-server` 不存在。

- [x] **Step 2: 修改 workspace 和 7 个 crate 的 package 名**

要求：
- `grid-core` -> `poise-core`
- `grid-engine` -> `poise-engine`
- `grid-storage` -> `poise-storage`
- `grid-protocol` -> `poise-protocol`
- `grid-binance` -> `poise-binance`
- `grid-server` -> `poise-server`
- `grid-tui` -> `poise-tui`

同时修复各 `Cargo.toml` 中的 path 依赖 key，保证依赖声明也切到 `poise-*`。

- [x] **Step 3: 修改代码里所有 crate import 路径**

全局搜索并替换：
- `use grid_core::` -> `use poise_core::`
- `use grid_engine::` -> `use poise_engine::`
- `use grid_storage::` -> `use poise_storage::`
- `use grid_protocol::` -> `use poise_protocol::`
- `use grid_binance::` -> `use poise_binance::`

同时更新测试中显式写死的 package / binary 名：
- `grid-server` -> `poise-server`
- `grid-tui` -> `poise-tui`

- [x] **Step 4: 更新 `Cargo.lock` 并运行 compile-only 验证**

Run:
`cargo check -p poise-core`
`cargo check -p poise-engine`
`cargo check -p poise-server`
`cargo check -p poise-tui`

Expected:
包名解析通过；如果还有编译错误，应只剩领域类型和协议命名未收敛导致的问题，不再出现旧 package 名解析错误。

- [x] **Step 5: 提交并回写 SHA**

```bash
git add Cargo.toml Cargo.lock core/Cargo.toml engine/Cargo.toml storage/Cargo.toml protocol/Cargo.toml server/Cargo.toml tui/Cargo.toml exchanges/binance/Cargo.toml core engine storage protocol server tui exchanges/binance
git commit -m "refactor(workspace): rename packages to poise"
```

Task 1 code commit:
`a8cd74aba2c3e078754fe37e28546e7887ec525f`

---

### Task 2: 把领域主语从 `Grid*` / `grid_*` 收敛到 `Track*` / `track_*`

**Files:**
- Modify: `core/src/strategy.rs`
- Modify: `engine/src/lib.rs`
- Move: `engine/src/grid.rs` -> `engine/src/track.rs`
- Modify: `engine/src/manager.rs`
- Modify: `engine/src/runtime.rs`
- Modify: `engine/src/snapshot.rs`
- Modify: `engine/src/ports.rs`
- Modify: `engine/src/transition.rs`
- Modify: `engine/src/observation.rs`
- Modify: `engine/src/execution_plan.rs`
- Modify: `engine/src/executor/planning.rs`
- Modify: `engine/src/executor/recovery.rs`
- Modify: `storage/src/sqlite.rs`
- Modify: `server/src/assembly.rs`
- Modify: `server/src/read_model.rs`
- Modify: `server/src/query_service.rs`
- Modify: `server/src/write_service.rs`
- Modify: `server/src/runtime.rs`
- Modify: `server/src/projector.rs`
- Test: `cargo test -p poise-engine`
- Test: `cargo test -p poise-server projector::tests:: -- --nocapture`
- Test: `cargo test -p poise-server query_service::tests:: -- --nocapture`

- [x] **Step 1: 先运行旧类型不存在的新命名测试，确认 `Track*` 当前还没落地**

Run:
`cargo test -p poise-engine track_id -- --nocapture`

Expected:
测试筛选不到或编译失败，因为当前类型仍是 `Grid*`。

- [x] **Step 2: 在 engine 层完成模块和主类型重命名**

要求：
- `engine/src/grid.rs` 改为 `engine/src/track.rs`
- `pub mod grid;` 改为 `pub mod track;`
- `GridId` -> `TrackId`
- `GridDefinition` -> `TrackDefinition`
- `GridManager` -> `TrackManager`
- `GridRuntime` -> `TrackRuntime`
- `GridRuntimeSnapshot` -> `TrackRuntimeSnapshot`
- `GridStatus` -> `TrackStatus`
- `GridObservation` -> `TrackObservation`
- `GridTransition` -> `TrackTransition`
- `GridEffect` -> `TrackEffect`

- [x] **Step 3: 在 core / server / storage 中同步类型和字段主语**

要求：
- `GridConfig` -> `TrackConfig`
- `grid_id` 字段 / 变量统一改为 `track_id`
- `grid` / `grids` 局部变量统一改为 `track` / `tracks`
- `GridReadModel` -> `TrackReadModel`
- `GridProjector` / `GridQueryService` / `GridWriteService` 等 server 层类型同步收敛

- [x] **Step 4: 运行领域与投影层定向测试**

Run:
`cargo test -p poise-engine`
`cargo test -p poise-server projector::tests:: -- --nocapture`
`cargo test -p poise-server query_service::tests:: -- --nocapture`

Expected:
engine 全量测试通过；server 中不依赖本地监听端口的 projector / query_service 测试通过；不再有 `Grid*` 类型残留导致的编译错误。

- [x] **Step 5: 提交并回写 SHA**

```bash
git add core/src/strategy.rs engine/src/lib.rs engine/src/track.rs engine/src/manager.rs engine/src/runtime.rs engine/src/snapshot.rs engine/src/ports.rs engine/src/transition.rs engine/src/observation.rs engine/src/execution_plan.rs engine/src/executor server/src/assembly.rs server/src/read_model.rs server/src/query_service.rs server/src/write_service.rs server/src/runtime.rs server/src/projector.rs storage/src/sqlite.rs
git commit -m "refactor(domain): rename grid types to track"
```

Task 2 code commits:
- `5b6a53487b4703cf29ce88dbfd54f7396494e1ff`
- `702c6874240bb2702750107df1ebe0b7d7587a2a`
- `0be73422f7f4dcfb54d2e84a80ee5ff9ef60de67`

---

### Task 3: 收敛配置、路由、协议、TUI 和用户入口到 `track` / `poise`

**Files:**
- Modify: `server/src/config.rs`
- Modify: `server/src/http.rs`
- Modify: `server/src/websocket.rs`
- Modify: `server/src/main.rs`
- Modify: `protocol/src/lib.rs`
- Modify: `tui/src/api_client.rs`
- Modify: `tui/src/protocol.rs`
- Modify: `tui/src/app.rs`
- Modify: `tui/src/input.rs`
- Modify: `tui/src/main.rs`
- Modify: `tui/src/views/dashboard.rs`
- Modify: `tui/src/views/instance.rs`
- Modify: `tui/src/views/help.rs`
- Modify: `tui/src/views/mod.rs`
- Move: `tui/tests/fixtures/grid_detail_view.json` -> `tui/tests/fixtures/track_detail_view.json`
- Move: `tui/tests/fixtures/grid_list_response.json` -> `tui/tests/fixtures/track_list_response.json`
- Move: `tui/tests/fixtures/ws_grid_detail_changed.json` -> `tui/tests/fixtures/ws_track_detail_changed.json`
- Move: `tui/tests/fixtures/ws_grid_list_item_changed.json` -> `tui/tests/fixtures/ws_track_list_item_changed.json`
- Modify: `configs/binance-testnet.toml`
- Modify: `configs/test.toml`
- Test: `cargo test -p poise-protocol`
- Test: `cargo test -p poise-server http::tests:: -- --nocapture`
- Test: `cargo test -p poise-tui protocol::tests:: -- --nocapture`
- Test: `cargo test -p poise-tui views::tests::renders_poise_header -- --exact`

- [x] **Step 1: 先用现有命令验证外部接口还停留在旧命名**

Run:
`rg -n "/grids|grid_id|\\[\\[grids\\]\\]|GRID_PLATFORM|GRID_TUI" protocol server tui configs README.md docs/protocol-contract.md`

Expected:
能搜到旧接口、旧配置键和旧环境变量。

- [x] **Step 2: 修改 server 配置和 HTTP / WS 入口**

要求：
- `Config.grids` -> `Config.tracks`
- `GridDefinition` -> `TrackDefinition`
- `grid_id` -> `track_id`
- `[[grids]]` -> `[[tracks]]`
- `/grids` -> `/tracks`
- `/grids/:id` -> `/tracks/:id`
- `/grids/:id/commands` -> `/tracks/:id/commands`
- `grid-server.sqlite` 默认文件名改为 `poise-server.sqlite`

- [x] **Step 3: 修改 protocol DTO 和 JSON 字段**

要求：
- `GridListResponse` -> `TrackListResponse`
- `GridListItemView` -> `TrackListItemView`
- `GridDetailView` -> `TrackDetailView`
- `GridCommandRequest` -> `TrackCommandRequest`
- `GridCommandAccepted` -> `TrackCommandAccepted`
- `GridStreamEvent` -> `TrackStreamEvent`
- JSON 字段 `grid_id` -> `track_id`

- [x] **Step 4: 修改 TUI 客户端、fixture 和环境变量**

要求：
- API client 与 protocol 解析全部改为 `/tracks` 和 `track_id`
- 环境变量：
  - `GRID_PLATFORM_BASE_URL` -> `POISE_BASE_URL`
  - `GRID_PLATFORM_WS_URL` -> `POISE_WS_URL`
  - `GRID_TUI_WS_URL` -> `POISE_TUI_WS_URL`
- 测试 fixture 文件名和内容同步改为 `track_*`
- TUI 内所有用户可见命令示例改成 `poise-server` / `poise-tui`

- [x] **Step 5: 运行外部接口定向测试**

Run:
`cargo test -p poise-protocol`
`cargo test -p poise-server http::tests:: -- --nocapture`
`cargo test -p poise-tui protocol::tests:: -- --nocapture`
`cargo test -p poise-tui views::tests::renders_poise_header -- --exact`

Expected:
协议类型、HTTP 入口和 TUI 协议解析通过；如果网络相关测试仍受本地端口限制，只记录为环境限制，不回退到旧命名。

- [x] **Step 6: 提交并回写 SHA**

```bash
git add server/src/config.rs server/src/http.rs server/src/websocket.rs server/src/main.rs protocol/src/lib.rs tui/src/api_client.rs tui/src/protocol.rs tui/src/app.rs tui/src/input.rs tui/src/main.rs tui/src/views configs/binance-testnet.toml configs/test.toml tui/tests/fixtures
git commit -m "refactor(api): rename grid surface to track"
```

Task 3 code commit:
`80f30d534bcefda26e5e9dfa3474294c56a10ec8`

---

### Task 4: 收敛 SQLite schema、持久化读写和存储测试到 `track_*`

**Files:**
- Modify: `storage/src/schema.rs`
- Modify: `storage/src/sqlite.rs`
- Modify: `engine/src/ports.rs`
- Modify: `server/src/read_model.rs`
- Modify: `server/src/query_service.rs`
- Modify: `server/src/write_service.rs`
- Modify: `server/src/effect_worker.rs`
- Test: `cargo test -p poise-storage`
- Test: `cargo test -p poise-server write_service::tests:: -- --nocapture`
- Test: `cargo test -p poise-server effect_worker::tests:: -- --nocapture`

- [x] **Step 1: 先写 / 调整失败测试，锁住新 schema 名和列名**

至少覆盖：
- `track_snapshots`
- `track_events`
- `track_effects`
- `track_id`
- `idx_track_effects_*`

如果现有测试直接断言旧表名，先把断言改成新名字，确认红灯。

- [x] **Step 2: 运行 storage 定向测试确认失败**

Run:
`cargo test -p poise-storage schema::tests:: -- --nocapture`

Expected:
失败，因为 schema 仍是 `grid_*`。

- [x] **Step 3: 做最小实现，统一 schema / SQL / repository 命名**

要求：
- `grid_snapshots` -> `track_snapshots`
- `domain_events` -> `track_events`
- `grid_effects` -> `track_effects`
- 所有 `grid_id` 列 -> `track_id`
- 索引名同步改为 `idx_track_*`
- `StoredGridSnapshot` / `CommittedGridWrite` / `PersistedGridEffect` 等存储接口类型统一切到 `Track*`

- [x] **Step 4: 运行 storage 和写侧定向测试**

Run:
`cargo test -p poise-storage`
`cargo test -p poise-server write_service::tests:: -- --nocapture`
`cargo test -p poise-server effect_worker::tests:: -- --nocapture`

Expected:
存储层测试通过；server 写侧和 effect worker 不再依赖旧表名 / 旧列名。

- [x] **Step 5: 提交并回写 SHA**

```bash
git add storage/src/schema.rs storage/src/sqlite.rs engine/src/ports.rs server/src/read_model.rs server/src/query_service.rs server/src/write_service.rs server/src/effect_worker.rs
git commit -m "refactor(storage): rename sqlite grid schema to track"
```

Task 4 code commit:
`c087adc7a5da174b8fed3e8d10bc09f51c07e773`

---

### Task 5: 全量改文档、示例和当前入口说明

**Files:**
- Modify: `README.md`
- Modify: `docs/protocol-contract.md`
- Modify: `docs/2026-03-30-architecture-review.md`
- Modify: `docs/grid-strategy-product-theory-research.md`
- Modify: `docs/superpowers/specs/2026-03-24-grid-platform-architecture-design.md`
- Modify: `docs/superpowers/specs/2026-03-24-grid-strategy-family-design.md`
- Modify: `docs/superpowers/specs/2026-03-25-grid-runtime-boundary-redesign.md`
- Modify: `docs/superpowers/specs/2026-03-25-grid-write-boundary-convergence-design.md`
- Modify: `docs/superpowers/specs/2026-03-26-grid-phase2-application-projection-design.md`
- Modify: `docs/superpowers/specs/2026-03-27-grid-engine-runtime-internalization-design.md`
- Modify: `docs/superpowers/specs/2026-03-28-grid-order-replacement-threshold-design.md`
- Modify: `docs/superpowers/specs/2026-03-28-grid-replacement-gate-observability-design.md`
- Modify: `docs/superpowers/specs/2026-03-28-grid-strategy-statistics-design.md`
- Modify: `docs/superpowers/specs/2026-03-28-grid-bandwidth-2000-design.md`
- Modify: `docs/superpowers/specs/2026-03-28-tui-activity-local-time-design.md`
- Modify: `docs/superpowers/specs/2026-03-29-inventory-executor-architecture-design.md`
- Modify: `docs/superpowers/specs/2026-03-31-poise-track-rename-design.md`
- Modify: `docs/superpowers/plans/*.md`
- Modify: `configs/binance-testnet.toml`
- Modify: `configs/test.toml`
- Test: `rg`

- [ ] **Step 1: 先用搜索命令盘点文档里的旧命名残留**

Run:
`rg -n "grid-platform|grid-server|grid-tui|grid-protocol|grid-core|grid-engine|grid-storage|grid-binance|Grid[A-Z]|grid_id|\\[\\[grids\\]\\]|/grids|GRID_PLATFORM|GRID_TUI" README.md docs configs`

Expected:
输出大量旧引用，确认文档和示例还未完成收敛。

- [ ] **Step 2: 修改 README、协议文档、配置示例和当前架构入口文档**

要求：
- README 的启动命令、环境变量、路由、配置块、包名全部切到 `poise-*` / `track_*`
- `docs/protocol-contract.md` 改成 `/tracks` 和 `track_id`
- 当前架构 spec、当前 rename spec、当前主线引用文档不再把当前系统称为 `grid-platform`

- [ ] **Step 3: 批量清理仍描述现行行为的 spec / plan 正文**

要求：
- 所有当前仍被入口文档引用、且正文在描述现行系统行为的 spec / plan，统一把当前主语改为 `Poise` / `track`
- 历史文件名可保留，但正文中不再把现行入口、现行命令、现行配置称为 `grid-*`
- 引用链接同步修正到新的 package / binary / route / config 名

- [ ] **Step 4: 用搜索命令做文档收尾验证**

Run:
`rg -n "grid-platform|grid-server|grid-tui|grid-protocol|grid-core|grid-engine|grid-storage|grid-binance|grid_id|\\[\\[grids\\]\\]|/grids|GRID_PLATFORM|GRID_TUI" README.md docs configs`

Expected:
当前文档入口、示例配置和现行说明不再命中旧命名。若历史说明中必须保留旧名，只能出现在明确的历史语境里，不能出现在现行入口段落。

- [ ] **Step 5: 提交并回写 SHA**

```bash
git add README.md docs configs
git commit -m "docs: rename current product language to poise track"
```

Task 5 code commit:
`TODO`

---

## 全量验证

- [ ] **Step 1: 运行 workspace 验证**

Run:
`cargo test -p poise-core`
`cargo test -p poise-engine`
`cargo test -p poise-storage`
`cargo test -p poise-protocol`
`cargo test -p poise-server`
`cargo test -p poise-tui`
`cargo test`

Expected:
新命名下 workspace 可通过验证。若 server / tui 的本地监听测试在当前执行环境被系统拒绝，应记录为环境限制，并在可监听端口的本机 shell 再补跑同样命令。

- [ ] **Step 2: 记录最终验证结论**

把各命令结果补回本计划末尾，写清：
- 通过的命令
- 因环境限制未通过的命令
- 是否需要在仓库目录改名后补跑一次

---

## 工作区切换步骤

以下步骤不属于 git tracked 改动，不记 commit SHA，但必须在全部 task 提交完成后执行：

1. 关闭当前基于 `/Users/yim/github/trading-lab/grid-platform` 的 Codex 工作区
2. 在父目录执行：

```bash
cd /Users/yim/github/trading-lab
mv grid-platform poise
```

3. 用新路径 `/Users/yim/github/trading-lab/poise` 重新打开工作区
4. 重新运行一次：

```bash
cargo test -p poise-core
cargo test -p poise-engine
cargo test -p poise-storage
cargo test -p poise-protocol
cargo test -p poise-server
cargo test -p poise-tui
cargo test
```

5. 如果改目录后出现路径硬编码残留，再单开一个修复 task；不要在未记录 plan 的情况下继续漂移修改
