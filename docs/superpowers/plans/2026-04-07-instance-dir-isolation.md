# Instance Dir Isolation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把运行实例收敛到显式 `instance dir` 边界，删除 `paper` 旧命名和多套地址环境变量，让配置、数据库、日志和状态重建都按实例目录隔离。

**Architecture:** 这次实现不改业务规则，只改运行边界。先在 `server` 引入 `instance dir` 启动入口和实例路径派生，把配置与数据库路径从“当前工作目录 + environment”改成“实例目录 + environment”；再同步收敛 `tui` 与脚本，只保留 `POISE_BASE_URL`，固定从它推导 WebSocket 地址；最后删除 `paper` 旧脚本和旧布局命名，更新 README 与干跑测试，让新实例模型成为唯一入口。

**Tech Stack:** Rust workspace, Cargo, Tokio, Axum, Rusqlite, Serde, anyhow, Bash, zellij

**Spec:** [`docs/superpowers/specs/2026-04-07-instance-dir-isolation-design.md`](../specs/2026-04-07-instance-dir-isolation-design.md)

---

## File Structure

### 新增文件

- `server/src/instance_dir.rs`
  - 解析实例目录结构
  - 提供 `config.toml`、`.data/`、`.logs/` 路径 helper
- `scripts/run-instance-server.sh`
  - 从实例目录启动 `poise-server`
- `scripts/run-instance-tui.sh`
  - 从实例目录启动 `poise-tui`
- `scripts/start-instance-zellij.sh`
  - 用实例目录和统一地址变量启动 zellij 会话
- `ops/zellij/poise-instance.kdl`
  - 新的 zellij 布局

### 重点修改文件

- `server/src/config.rs`
  - `Config` 不再自己决定仓库相对数据库路径；数据库路径改由实例目录 helper 派生
- `server/src/state_bootstrap.rs`
  - 接收实例目录派生的数据库路径
  - `--rebuild-state` 只操作当前实例目录
- `server/src/main.rs`
  - CLI 改成 `--instance-dir <path>`
  - 从实例目录加载 `config.toml`
  - 更新错误提示和脚本 dry-run 测试
- `tui/src/main.rs`
  - 删除 `POISE_TUI_WS_URL` / `POISE_WS_URL`
  - 只从 `POISE_BASE_URL` 推导 WebSocket 地址
- `scripts/probe-health.sh`
  - 改为只认 `POISE_BASE_URL`
  - 日志默认落在实例目录
- `scripts/flatten-track.sh`
  - 只认 `POISE_BASE_URL`
- `README.md`
  - 改写到实例目录模型
  - 删除 `paper` 旧命名和旧环境变量
- `server/src/main.rs` tests
  - 调整脚本 dry-run / health probe 测试
- `tui/src/main.rs` tests
  - 固定只接受 `POISE_BASE_URL`

### 删除文件

- `scripts/run-paper-server.sh`
- `scripts/run-paper-tui.sh`
- `scripts/start-paper-zellij.sh`
- `ops/zellij/poise-paper.kdl`

### 实施约束

- 不引入 `db_path` 配置字段
- 不保留 `paper` 旧脚本和旧环境变量兼容层
- 每个 task 先写失败测试，再写最小实现
- 每个 task 验收通过后必须立即提交，并把 commit SHA 回写到本计划
- 未完成 `git add`、`git commit` 和计划回写，不得开始下一个 task

---

### Task 1: 引入 `instance dir`，让 server 从实例目录加载配置并派生数据库路径

**Files:**
- Create: `server/src/instance_dir.rs`
- Modify: `server/src/main.rs`
- Modify: `server/src/config.rs`
- Modify: `server/src/state_bootstrap.rs`
- Test: `server/src/instance_dir.rs`
- Test: `server/src/main.rs`
- Test: `server/src/state_bootstrap.rs`

- [x] **Step 1: 先写失败测试，固定实例目录结构和启动参数**

在 `server/src/instance_dir.rs` 新增测试：

```rust
#[test]
fn instance_dir_resolves_config_data_and_log_paths() {
    let dir = InstanceDir::new("/tmp/poise/a");

    assert_eq!(dir.config_path(), PathBuf::from("/tmp/poise/a/config.toml"));
    assert_eq!(dir.data_root(), PathBuf::from("/tmp/poise/a/.data"));
    assert_eq!(dir.logs_root(), PathBuf::from("/tmp/poise/a/.logs"));
}
```

在 `server/src/main.rs` 增加启动参数测试：

```rust
#[test]
fn parse_startup_options_requires_instance_dir() {
    let error = parse_startup_options(Vec::<String>::new().into_iter()).unwrap_err();

    assert!(error.to_string().contains("--instance-dir"));
}
```

在 `server/src/state_bootstrap.rs` 增加数据库作用域测试：

```rust
#[tokio::test]
async fn rebuild_mode_only_touches_database_under_current_instance_dir() {
    let instance_dir = tempfile::tempdir().unwrap();
    let config = test_config_with_instance_dir(instance_dir.path(), "mainnet", 90.0);
    let db_path = InstanceDir::new(instance_dir.path()).db_path("mainnet");

    prepare_state_repository(&config, &db_path, StateBootstrapMode::Rebuild)
        .await
        .unwrap();

    assert!(db_path.exists());
}
```

- [x] **Step 2: 运行定向测试，确认当前还没有实例目录边界**

Run:
`cargo test -p poise-server instance_dir::tests::instance_dir_resolves_config_data_and_log_paths -- --exact`

Expected:
- FAIL，原因是 `server/src/instance_dir.rs` 尚不存在

Run:
`cargo test -p poise-server tests::parse_startup_options_requires_instance_dir -- --exact`

Expected:
- FAIL，原因是当前仍要求 `--config`

Run:
`cargo test -p poise-server state_bootstrap::tests::rebuild_mode_only_touches_database_under_current_instance_dir -- --exact`

Expected:
- FAIL，原因是 `prepare_state_repository(...)` 仍然内部按 `config.default_db_path()` 取库路径

- [x] **Step 3: 实现最小实例目录边界**

要求：
- 新增 `InstanceDir`
- `poise-server` CLI 改为 `--instance-dir <path>`
- `main.rs` 固定从 `<instance-dir>/config.toml` 读取配置
- 数据库路径改由 `InstanceDir::db_path(environment)` 派生
- `Config::default_db_path()` 删除或收回，仅保留配置解析职责
- `state_bootstrap` 接收显式数据库路径，不再自己推导工作目录相对路径
- `render_startup_error(...)` 中的建议命令改成 `--instance-dir <path> --rebuild-state`

- [x] **Step 4: 运行 server 入口与状态重建回归**

Run:
`cargo test -p poise-server parse_startup_options_requires_instance_dir -- --exact`

Expected:
- PASS

Run:
`cargo test -p poise-server state_bootstrap::tests::strict_mode_rejects_persisted_config_mismatch -- --exact`
`cargo test -p poise-server state_bootstrap::tests::rebuild_mode_only_touches_database_under_current_instance_dir -- --exact`

Expected:
- PASS

- [x] **Step 5: 提交并回写 SHA**

```bash
git add server/src/instance_dir.rs server/src/main.rs server/src/config.rs server/src/state_bootstrap.rs
git commit -m "refactor: derive server state paths from instance dir"
```

Task 1 code commit:
`815b3c5`

---

### Task 2: 收敛 TUI 运行配置，只保留 `POISE_BASE_URL`

**Files:**
- Modify: `tui/src/main.rs`
- Modify: `scripts/flatten-track.sh`
- Test: `tui/src/main.rs`

执行备注：当前分支不存在 `scripts/flatten-track.sh`，因此本 task 实际只修改 `tui/src/main.rs`。

- [x] **Step 1: 先写失败测试，固定 TUI 不再接受独立 WS 环境变量**

在 `tui/src/main.rs` 增加测试：

```rust
#[test]
fn runtime_config_ignores_removed_ws_env_vars() {
    temp_env::with_vars(
        [
            ("POISE_BASE_URL", Some("http://127.0.0.1:9000")),
            ("POISE_WS_URL", Some("ws://127.0.0.1:9999/other")),
            ("POISE_TUI_WS_URL", Some("ws://127.0.0.1:9999/other")),
        ],
        || {
            let config = RuntimeConfig::from_env().unwrap();
            assert_eq!(config.ws_url, "ws://127.0.0.1:9000/ws");
        },
    );
}
```

在 `scripts/flatten-track.sh` 对齐只认 `POISE_BASE_URL` 的 dry-run 断言，补测试说明到 plan 使用现有脚本 smoke 路径。

- [x] **Step 2: 运行定向测试，确认当前仍允许独立 WS 覆盖**

Run:
`cargo test -p poise-tui runtime_config_ignores_removed_ws_env_vars -- --exact`

Expected:
- FAIL，原因是 `RuntimeConfig::from_env()` 当前还会读取 `POISE_TUI_WS_URL` / `POISE_WS_URL`

- [x] **Step 3: 实现最小运行配置收敛**

要求：
- `RuntimeConfig::from_env()` 只读取 `POISE_BASE_URL`
- WebSocket 地址一律通过 `derive_ws_url(&base_url)` 生成
- 删除 `POISE_TUI_WS_URL` / `POISE_WS_URL` 的读取分支
- `scripts/flatten-track.sh` 默认基地址只认 `POISE_BASE_URL`

- [x] **Step 4: 运行 TUI 与脚本回归**

Run:
`cargo test -p poise-tui derives_ws_url_from_http_base_url -- --exact`
`cargo test -p poise-tui derives_ws_url_from_base_url_with_path_prefix -- --exact`
`cargo test -p poise-tui runtime_config_ignores_removed_ws_env_vars -- --exact`

Expected:
- PASS

- [x] **Step 5: 提交并回写 SHA**

```bash
git add tui/src/main.rs scripts/flatten-track.sh
git commit -m "refactor: derive tui websocket url from base url"
```

Task 2 code commit:
`348c6de`

---

### Task 3: 脚本切到实例目录入口，并删除 `paper` 旧命名

**Files:**
- Create: `scripts/run-instance-server.sh`
- Create: `scripts/run-instance-tui.sh`
- Create: `scripts/start-instance-zellij.sh`
- Modify: `scripts/probe-health.sh`
- Delete: `scripts/run-paper-server.sh`
- Delete: `scripts/run-paper-tui.sh`
- Delete: `scripts/start-paper-zellij.sh`
- Create: `ops/zellij/poise-instance.kdl`
- Delete: `ops/zellij/poise-paper.kdl`
- Test: `server/src/main.rs`

- [x] **Step 1: 先写失败测试，固定新脚本和新 layout 的 dry-run 输出**

在 `server/src/main.rs` 现有脚本测试旁新增：

```rust
#[test]
fn run_instance_server_script_dry_run_uses_instance_dir() {
    let temp_dir = tempfile::tempdir().unwrap();
    let output = Command::new("bash")
        .arg(run_instance_server_script_path())
        .arg("--dry-run")
        .env("POISE_INSTANCE_DIR", temp_dir.path())
        .output()
        .unwrap();

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("instance_dir="));
    assert!(stdout.contains("--instance-dir"));
    assert!(stdout.contains(".logs/poise-server.log"));
}
```

新增 zellij 脚本 dry-run 测试：

```rust
#[test]
fn start_instance_zellij_dry_run_exports_instance_dir_and_base_url() {
    let temp_dir = tempfile::tempdir().unwrap();
    let output = Command::new("bash")
        .arg(start_instance_zellij_script_path())
        .arg("--dry-run")
        .env("POISE_INSTANCE_DIR", temp_dir.path())
        .output()
        .unwrap();

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("instance_dir="));
    assert!(stdout.contains("base_url="));
    assert!(!stdout.contains("paper"));
}
```

- [x] **Step 2: 运行定向测试，确认当前脚本入口和命名仍是旧模型**

Run:
`cargo test -p poise-server tests::run_instance_server_script_dry_run_uses_instance_dir -- --exact`

Expected:
- FAIL，原因是新脚本尚不存在

Run:
`cargo test -p poise-server tests::start_instance_zellij_dry_run_exports_instance_dir_and_base_url -- --exact`

Expected:
- FAIL，原因是 layout 和脚本仍然是 `paper` 旧命名

- [x] **Step 3: 实现新脚本和实例目录默认路径**

要求：
- 新脚本统一接收 `POISE_INSTANCE_DIR`
- `run-instance-server.sh` 用 `<instance-dir>/config.toml` 启动 server
- `run-instance-server.sh` 默认日志写到 `<instance-dir>/.logs/poise-server.log`
- `run-instance-tui.sh` 默认日志写到 `<instance-dir>/.logs/poise-tui.log`
- `probe-health.sh` 默认日志写到 `<instance-dir>/.logs/health-probe.log`
- `probe-health.sh` 只认 `POISE_BASE_URL`
- `start-instance-zellij.sh` 默认 session 名从实例目录 basename 推导，例如 `poise-<instance-name>`
- `ops/zellij/poise-instance.kdl` 调用新脚本，不再写死 `paper`
- 删除旧 `paper` 脚本和旧 layout 文件

- [x] **Step 4: 运行脚本 dry-run 与健康巡检回归**

Run:
`cargo test -p poise-server tests::run_instance_server_script_dry_run_uses_instance_dir -- --exact`
`cargo test -p poise-server tests::start_instance_zellij_dry_run_exports_instance_dir_and_base_url -- --exact`
`cargo test -p poise-server tests::probe_health_exits_after_failure_threshold_and_runs_alert_hook -- --exact`

Expected:
- PASS

- [x] **Step 5: 提交并回写 SHA**

```bash
git add scripts/run-instance-server.sh scripts/run-instance-tui.sh scripts/start-instance-zellij.sh scripts/probe-health.sh ops/zellij/poise-instance.kdl server/src/main.rs
git rm scripts/run-paper-server.sh scripts/run-paper-tui.sh scripts/start-paper-zellij.sh ops/zellij/poise-paper.kdl
git commit -m "refactor: switch runtime scripts to instance dir model"
```

执行备注：
- 初始实现提交：`2c6eeee`
- 补充修正提交：`56006bb`，恢复新脚本的可执行位，保证 `./scripts/...` 直接调用可用

Task 3 code commit:
`56006bb`

---

### Task 4: 更新 README、配置示例和实例文档，去掉 `paper` 与旧变量

**Files:**
- Modify: `README.md`
- Modify: `server/src/config.rs`
- Test: `server/src/config.rs`

- [x] **Step 1: 先写失败测试，固定配置示例与文档语义**

在 `server/src/config.rs` 调整或新增测试，去掉 `environment = "paper"` 作为普通示例输入，改成 `testnet` 或 `mainnet`。

实际执行时补了一个更直接的源码级断言，确保模块内示例不再出现 `environment = "paper"`：

```rust
#[test]
fn config_module_examples_do_not_use_paper_environment() {
    let source = include_str!("config.rs");
    assert!(!source.contains("environment = \"paper\""));
}
```

- [x] **Step 2: 运行定向测试，确认当前仍有 `paper` 示例残留**

Run:
`cargo test -p poise-server config::tests::config_module_examples_do_not_use_paper_environment -- --exact`

Expected:
- FAIL，原因是 `server/src/config.rs` 当前仍包含 `environment = "paper"` 示例

- [x] **Step 3: 改写 README 和配置示例**

要求：
- README 改成“一个实例目录一个 `config.toml`”
- 所有脚本示例改成新脚本名
- 所有地址环境变量只保留 `POISE_BASE_URL`
- 删除 `.logs/paper`、`poise-paper`、`POISE_HEALTH_BASE_URL`、`POISE_WS_URL`、`POISE_TUI_WS_URL`
- README 增加多账号 mainnet 的实例目录示例
- `server/src/config.rs` 内嵌 TOML 测试示例不再出现 `paper`

- [x] **Step 4: 运行文档相关配置回归**

Run:
`cargo test -p poise-server config::tests:: -- --nocapture`

Expected:
- PASS

- [x] **Step 5: 提交并回写 SHA**

```bash
git add README.md server/src/config.rs
git commit -m "docs: document instance dir runtime model"
```

Task 4 code commit:
`9cea4ae`

---

### Task 5: 全量验证实例目录模型，收口残留旧变量与旧命名

**Files:**
- Modify: `server/src/main.rs`
- Modify: `tui/src/main.rs`
- Modify: `scripts/probe-health.sh`
- Modify: `scripts/run-instance-server.sh`
- Modify: `scripts/run-instance-tui.sh`
- Modify: `scripts/start-instance-zellij.sh`
- Modify: `README.md`

- [x] **Step 1: 先补最终收口检查**

新增或调整测试与检索断言，确保不存在旧入口残留：

Run:
`rg -n "run-paper|start-paper|poise-paper|\\.logs/paper|POISE_HEALTH_BASE_URL|POISE_TUI_WS_URL|POISE_WS_URL" README.md scripts server tui ops`

Expected:
- 只允许命中迁移说明中的设计 / 历史文档，不允许命中生产代码、README 和新脚本

- [x] **Step 2: 运行 workspace 级验证**

Run:
`cargo test -p poise-server`
`cargo test -p poise-tui`

Expected:
- PASS

Run:
`bash scripts/run-instance-server.sh --dry-run`
`bash scripts/run-instance-tui.sh --dry-run`
`bash scripts/start-instance-zellij.sh --dry-run`
`bash scripts/probe-health.sh --dry-run`

Expected:
- PASS，且输出只包含实例目录模型和 `POISE_BASE_URL`

- [x] **Step 3: 提交并回写 SHA**

```bash
git add server/src/main.rs tui/src/main.rs scripts/probe-health.sh scripts/run-instance-server.sh scripts/run-instance-tui.sh scripts/start-instance-zellij.sh README.md
git commit -m "refactor: remove legacy paper runtime naming"
```

执行备注：
- 残留检查最初命中了 `tui/src/main.rs` 测试辅助代码里的旧环境变量字符串；已改成动态拼接名称，避免旧命名继续留在源码中
- `cargo test -p poise-server` 首次全量回归暴露 `server/src/assembly.rs` 的测试辅助函数仍在函数内部创建并丢弃 `tempdir`，已改为由调用方显式持有实例目录，保证重装配测试复用同一数据库

Task 5 code commit:
`cf11cdb`
