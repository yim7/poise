# State Bootstrap Boundary Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把 `--rebuild-state` 的状态准备逻辑从 `assembly` 中拆出到独立模块，并用显式启动策略与结构化错误收敛启动边界。

**Architecture:** 这次实现按三层收敛：先把启动入口的参数与策略显式化，再引入 `state_bootstrap` 模块承接状态仓库准备与 mismatch 结构化错误，最后把 `assembly` 收回到纯装配职责。状态仓库对外暴露为组合接口 `StateStore`，不再把 `SqliteStorage` 细节泄露到启动主路径。

**Tech Stack:** Rust workspace, Cargo, Tokio, Axum, Rusqlite, Serde, anyhow

---

## File Structure

### 新增文件

- `server/src/state_bootstrap.rs`：状态仓库准备、mismatch 检查、重建备份、结构化错误

### 重点修改文件

- `server/src/main.rs`：CLI 参数解析、`StateBootstrapMode` 映射、结构化错误渲染
- `server/src/assembly.rs`：去掉状态库生命周期逻辑，改为接收抽象状态仓库并只做装配
- `engine/src/ports.rs` 或 `server/src/state_bootstrap.rs`：定义组合接口 `StateStore` 的落点
- `docs/superpowers/specs/2026-04-03-state-bootstrap-boundary-design.md`：如实现中有必要，对齐最终接口名和模块落点

### 测试落点

- `server/src/main.rs`：CLI 参数解析测试
- `server/src/state_bootstrap.rs`：严格模式 / 重建模式行为测试
- `server/src/assembly.rs`：装配测试改为依赖抽象状态仓库，不再覆盖 SQLite 生命周期细节

### 实施约束

- 每个 task 必须先写失败测试，再写实现
- 每个 task 验收通过后必须立即提交，并把 commit SHA 回写到本计划
- 未完成 `git add`、`git commit` 和计划回写，不得开始下一个 task

---

### Task 1: 建立 `state_bootstrap` 边界并显式化启动策略

**Files:**
- Modify: `server/src/main.rs`
- Create: `server/src/state_bootstrap.rs`
- Modify: `server/src/assembly.rs`
- Modify: `engine/src/ports.rs` or `server/src/state_bootstrap.rs`
- Test: `cargo test -p poise-server parse_config_path_accepts_rebuild_state_flag -- --nocapture`
- Test: `cargo test -p poise-server state_bootstrap::tests:: -- --nocapture`

- [x] **Step 1: 先写失败测试，固定新边界而不是旧路径**

要求：
- 为 `main.rs` 增加测试，断言 CLI 解析结果不再是裸 `bool` 语义，而是显式启动策略
- 为 `state_bootstrap.rs` 增加模块级测试骨架，固定状态仓库准备将从新模块进入
- 为 `assembly.rs` 增加或调整编译级测试，固定 `assemble` 不再负责接收 `rebuild_state`

- [x] **Step 2: 运行定向测试，确认当前代码还没有形成目标边界**

Run:
`cargo test -p poise-server parse_config_path_accepts_rebuild_state_flag -- --nocapture`
`cargo test -p poise-server state_bootstrap::tests:: -- --nocapture`

Expected:
测试失败，或编译失败并显示：
- 启动参数仍是旧语义
- `state_bootstrap` 模块与测试目标尚不存在
- `assembly` 仍保留旧装配职责

- [x] **Step 3: 实现模块骨架、显式启动策略与组合仓库接口**

要求：
- 创建 `server/src/state_bootstrap.rs`，先建立正确模块边界
- 引入 `StateBootstrapMode`
- 将 CLI 选项映射到 `StateBootstrapMode`
- 定义 `StateStore` 组合接口，表达 server 启动真正依赖的仓库能力
- 由 `main` 调用 `state_bootstrap` 准备状态仓库
- 修改 `assembly::assemble(...)`，让它接收抽象仓库，不再接收 `rebuild_state`

- [x] **Step 4: 运行定向测试，确认第一步边界已经稳定**

Run:
`cargo test -p poise-server parse_config_path_accepts_rebuild_state_flag -- --nocapture`
`cargo test -p poise-server state_bootstrap::tests:: -- --nocapture`

Expected:
测试通过；`main`、`state_bootstrap`、`assembly` 三层边界已经成形，不再需要先改接口再搬职责。

- [x] **Step 5: 提交并回写 SHA**

```bash
git add server/src/main.rs server/src/assembly.rs engine/src/ports.rs server/src/state_bootstrap.rs
git commit -m "refactor(server): introduce state bootstrap boundary"
```

Task 1 code commit:
`da1deb4283478e8d6d269414c6461131cf72ca3d`

---

### Task 2: 把状态重建逻辑迁入 `state_bootstrap`

**Files:**
- Modify: `server/src/main.rs`
- Modify: `server/src/assembly.rs`
- Modify: `server/src/state_bootstrap.rs`
- Test: `cargo test -p poise-server state_bootstrap::tests::strict_mode_rejects_persisted_config_mismatch -- --nocapture`
- Test: `cargo test -p poise-server state_bootstrap::tests::rebuild_mode_recreates_repository_after_mismatch -- --nocapture`

- [x] **Step 1: 先把状态检查 / 重建行为改写成围绕新接口的测试**

要求：
- 新增或调整测试，固定以下行为：
  - 严格模式下 mismatch 返回错误
  - 重建模式下会备份旧库并清理 sidecar
- 测试入口直接指向 `state_bootstrap::prepare_state_repository(...)`
- 不再沿用 `assemble_with_fake_ports_*` 作为核心行为名

- [x] **Step 2: 运行定向测试，确认它们在当前结构下失败**

Run:
`cargo test -p poise-server state_bootstrap::tests::strict_mode_rejects_persisted_config_mismatch -- --nocapture`
`cargo test -p poise-server state_bootstrap::tests::rebuild_mode_recreates_repository_after_mismatch -- --nocapture`

Expected:
测试失败，说明状态准备行为还没有完全收敛到 `state_bootstrap` 新接口。

- [x] **Step 3: 实现 `state_bootstrap` 模块**

要求：
- 将 mismatch 检查、备份旧库、删除 `-wal` / `-shm`、重建 repository 迁入 `server/src/state_bootstrap.rs`
- 由 `main` 调用 `state_bootstrap::prepare_state_repository(...)`
- `assembly` 删除 SQLite 生命周期与 mismatch 处理逻辑

- [x] **Step 4: 运行定向测试，确认状态准备职责完成迁移**

Run:
`cargo test -p poise-server state_bootstrap::tests::strict_mode_rejects_persisted_config_mismatch -- --nocapture`
`cargo test -p poise-server state_bootstrap::tests::rebuild_mode_recreates_repository_after_mismatch -- --nocapture`

Expected:
状态启动模块测试通过；`assembly` 中不再包含状态重建逻辑。

- [x] **Step 5: 提交并回写 SHA**

```bash
git add server/src/state_bootstrap.rs server/src/main.rs server/src/assembly.rs
git commit -m "refactor(server): move state rebuild logic into bootstrap module"
```

Task 2 code commit:
`adef4cd736c5f615b2430cf962732a7c68bc57d7`

---

### Task 3: 结构化 mismatch 错误并收敛 CLI 渲染

**Files:**
- Modify: `server/src/state_bootstrap.rs`
- Modify: `server/src/main.rs`
- Test: `cargo test -p poise-server state_bootstrap::tests::strict_mode_returns_structured_mismatch -- --nocapture`
- Test: `cargo test -p poise-server tests::parse_config_path_accepts_rebuild_state_flag -- --nocapture`

- [x] **Step 1: 先写失败测试，固定结构化错误与 CLI 提示的分层**

要求：
- 新增测试，断言 `state_bootstrap` 返回结构化 mismatch 错误，而不是拼接完整 CLI 文案
- 新增或调整 `main` 层测试，断言 CLI 层会把结构化错误渲染成包含数据库路径、差异详情和 `--rebuild-state` 提示的消息

- [x] **Step 2: 运行定向测试，确认当前实现仍未分层**

Run:
`cargo test -p poise-server state_bootstrap::tests::strict_mode_returns_structured_mismatch -- --nocapture`

Expected:
测试失败，说明错误渲染仍耦合在状态准备逻辑里。

- [x] **Step 3: 实现结构化错误与 CLI 渲染**

要求：
- 定义结构化 mismatch 错误类型
- `state_bootstrap` 仅返回结构化结果
- `main` 负责把该错误渲染为面向用户的终端提示

- [x] **Step 4: 运行 server 包复验**

Run:
`cargo test -p poise-server`

Expected:
`poise-server` 全量测试通过；启动边界、状态准备和装配边界收敛完成。

- [x] **Step 5: 提交并回写 SHA**

```bash
git add server/src/state_bootstrap.rs server/src/main.rs server/src/assembly.rs docs/superpowers/specs/2026-04-03-state-bootstrap-boundary-design.md
git commit -m "refactor(server): separate bootstrap errors from cli rendering"
```

Task 3 code commit:
`384c4fc7313b3d9db18a66f2936ebfbf3fe4d88a`
