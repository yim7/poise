# 删除顶层 environment 配置实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: 使用 `superpowers:subagent-driven-development`（推荐）或 `superpowers:executing-plans` 按 task 执行本计划。步骤使用 checkbox (`- [ ]`) 语法追踪。

**Goal:** 删除 `Config.environment`，把本地 SQLite 路径固定为 `<instance-dir>/.data/poise-server.sqlite`，让实例目录成为唯一的本地状态隔离边界。

**Architecture:** 本次实现不引入兼容层，也不做自动迁移。先删除现行入口里的 `environment` 语义，包括配置解析、demo 配置、README 和当前测试夹具；再把 `InstanceDir`、`main` 和 `state_bootstrap` 改成固定数据库路径；最后把依赖 `environment` 分桶的测试改成临时实例目录隔离。交易所 deployment 继续归 `exchange.deployment` owner，本次不修改交易所接入边界。

**Tech Stack:** Rust workspace, Cargo, Tokio, Axum, Serde, anyhow, tempfile

**Spec context:** 当前结论来自本线程已确认设计：`environment` 不再承担交易所语义，现仅剩本地状态命名空间职责，而该职责已被 `instance-dir` 重复覆盖，因此直接删除。

---

## File Structure

### 重点修改文件

- `server/src/config.rs`
  - 删除 `Config.environment`
  - 更新解析测试、demo 配置约束和 README 回归约束
- `server/src/instance_dir.rs`
  - `db_path()` 改为固定返回 `<instance-dir>/.data/poise-server.sqlite`
- `server/src/main.rs`
  - 启动路径不再读取 `config.environment`
  - 启动测试配置片段去掉 `environment`
- `server/src/assembly.rs`
  - 测试夹具去掉 `environment`
- `server/src/state_bootstrap.rs`
  - 生产路径使用固定 `db_path()`
  - 测试从 `environment` 分桶改成临时实例目录隔离
- `README.md`
  - 删除现行配置示例中的 `environment`
  - 更新 SQLite 路径说明
- `configs/binance-testnet.demo.toml`
  - 删除 `environment`
- `configs/test.demo.toml`
  - 删除 `environment`
- `tui/src/main.rs`
  - 慢速 e2e 配置夹具去掉 `environment`

### 实施约束

- 不保留 `environment` 兼容解析
- 不做旧 SQLite 路径的自动迁移；用户如需保留旧库，手工移动或使用 `--rebuild-state`
- `exchange.deployment` 不得替代 `environment` 进入本地 SQLite 路径
- 每个 task 必须先写失败测试，再写最小实现
- 每个 task 验收通过后必须立即提交，并把 commit SHA 回写到本计划
- 未完成 `git add`、`git commit` 和计划回写，不得开始下一个 task
- 最终验收至少包含 `cargo test -p poise-server`、`cargo test -p poise-tui`

---

### Task 1: 一次完成现行入口与生产路径的 environment 删除

**Files:**
- Modify: `server/src/config.rs`
- Modify: `server/src/instance_dir.rs`
- Modify: `server/src/main.rs`
- Modify: `server/src/assembly.rs`
- Modify: `server/src/state_bootstrap.rs`
- Modify: `README.md`
- Modify: `configs/binance-testnet.demo.toml`
- Modify: `configs/test.demo.toml`
- Modify: `tui/src/main.rs`
- Test: `server/src/config.rs`
- Test: `server/src/instance_dir.rs`
- Test: `server/src/main.rs`
- Test: `server/src/assembly.rs`
- Test: `tui/src/main.rs`

- [ ] **Step 1: 先写失败测试，固定“现行入口和生产路径都不再使用 environment”**

在 `server/src/config.rs` 添加：

```rust
#[test]
fn parses_config_without_environment_field() {
    let config = parse_config(
        r#"
bind_address = "127.0.0.1:8000"

[exchange]
venue = "binance"
deployment = "testnet"

[[tracks]]
track_id = "btc-core"
symbol = "BTCUSDT"
lower_price = 90.0
upper_price = 110.0
long_exposure_units = 8.0
short_exposure_units = 8.0
notional_per_unit = 375.0
"#,
    )
    .unwrap();

    assert_eq!(config.bind_address, "127.0.0.1:8000");
}
```

再添加：

```rust
#[test]
fn demo_configs_do_not_define_environment() {
    for raw in [
        include_str!("../../configs/binance-testnet.demo.toml"),
        include_str!("../../configs/test.demo.toml"),
    ] {
        assert!(!raw.contains("environment = "));
    }
}
```

把现有 README 回归测试扩展为：

```rust
assert!(!raw.contains("environment = \"testnet\""));
assert!(!raw.contains("environment = \"mainnet\""));
assert!(!raw.contains("environment = \"test\""));
```

在 `server/src/instance_dir.rs` 添加：

```rust
#[test]
fn instance_dir_db_path_is_fixed_under_instance_data_root() {
    let dir = InstanceDir::new("/tmp/poise/a");

    assert_eq!(
        dir.db_path(),
        PathBuf::from("/tmp/poise/a/.data/poise-server.sqlite")
    );
}
```

在 `server/src/main.rs` / `server/src/assembly.rs` 现有测试里，把当前依赖 `config.environment` 拼接数据库路径的断言改成固定路径断言，并新增一个真实入口约束测试，明确 deployment 不得进入本地 SQLite 路径。这个测试必须放在 `server/src/main.rs`，并按 `main()` 当前启动顺序执行 `load_config(instance_dir/config.toml) -> InstanceDir::db_path()`，而不是只静态调用 `InstanceDir::db_path()`。例如：

```rust
#[test]
fn startup_db_path_does_not_depend_on_exchange_deployment() {
    let instance_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        instance_dir.path().join("config.toml"),
        /* deployment = "testnet" 的配置 */,
    )
    .unwrap();
    let testnet_config = crate::config::load_config(instance_dir.path().join("config.toml")).unwrap();
    let testnet_path = InstanceDir::new(instance_dir.path()).db_path();

    std::fs::write(
        instance_dir.path().join("config.toml"),
        /* deployment = "mainnet" 的配置 */,
    )
    .unwrap();
    let mainnet_config = crate::config::load_config(instance_dir.path().join("config.toml")).unwrap();
    let mainnet_path = InstanceDir::new(instance_dir.path()).db_path();

    assert_eq!(testnet_config.bind_address, mainnet_config.bind_address);
    assert_eq!(testnet_path, mainnet_path);
    assert_eq!(mainnet_path, instance_dir.path().join(".data").join("poise-server.sqlite"));
}
```

在 `tui/src/main.rs` 保留并扩展当前慢速 e2e 配置约束测试，要求源码中不存在 `environment = "test"` 夹具片段。

- [ ] **Step 2: 运行定向测试，确认当前行为仍依赖 environment**

Run:
`cargo test -p poise-server parses_config_without_environment_field`

Expected:
- FAIL，原因是 `Config` 仍要求 `environment`

Run:
`cargo test -p poise-server demo_configs_do_not_define_environment`

Expected:
- FAIL，原因是 demo 配置仍包含 `environment`

Run:
`cargo test -p poise-server instance_dir_db_path_is_fixed_under_instance_data_root`

Expected:
- FAIL，原因是 `InstanceDir::db_path()` 仍要求 `environment`

Run:
`cargo test -p poise-server startup_db_path_does_not_depend_on_exchange_deployment`

Expected:
- FAIL，原因是本地路径当前还通过 `config.environment` 派生，边界没有固定

Run:
`cargo test -p poise-tui slow_e2e_server_config_examples_use_service_level_exchange_boundary`

Expected:
- FAIL，原因是 `tui` 慢速 e2e 配置夹具仍包含 `environment`

- [ ] **Step 3: 写最小实现**

要求：
- `Config` 删除 `environment`
- 配置解析和对应测试输入全部去掉 `environment`
- `InstanceDir::db_path()` 改为无参固定路径
- `main.rs`、`assembly.rs` 和其他当前生产调用点全部改成固定路径
- `state_bootstrap.rs` 中所有直接构造 `Config { environment: ... }`、调用 `db_path(&config.environment)` 的编译面一并改掉，不能留到下一个 task 再补
- `README` 的现行配置示例、SQLite 路径说明、`--rebuild-state` 说明去掉 `environment`
- `configs/binance-testnet.demo.toml` 和 `configs/test.demo.toml` 去掉 `environment`
- `tui` 慢速 e2e 配置 helper 去掉 `environment`
- 不引入 `deployment -> db_path` 新映射

- [ ] **Step 4: 运行回归**

Run:
`cargo test -p poise-server parses_config_without_environment_field`

Expected:
- PASS

Run:
`cargo test -p poise-server demo_configs_do_not_define_environment`

Expected:
- PASS

Run:
`cargo test -p poise-server readme_example_matches_service_level_exchange_boundary`

Expected:
- PASS

Run:
`cargo test -p poise-server instance_dir_db_path_is_fixed_under_instance_data_root`

Expected:
- PASS

Run:
`cargo test -p poise-server startup_db_path_does_not_depend_on_exchange_deployment`

Expected:
- PASS

Run:
`cargo test -p poise-server run_instance_server_script_dry_run_uses_instance_dir`

Expected:
- PASS

Run:
`cargo test -p poise-tui slow_e2e_server_config_examples_use_service_level_exchange_boundary`

Expected:
- PASS

Run:
`cargo test -p poise-server`

Expected:
- PASS

- [ ] **Step 5: 提交**

```bash
git add server/src/config.rs server/src/instance_dir.rs server/src/main.rs server/src/assembly.rs server/src/state_bootstrap.rs README.md configs/binance-testnet.demo.toml configs/test.demo.toml tui/src/main.rs docs/superpowers/plans/2026-04-09-remove-environment.md
git commit -m "refactor: remove environment from runtime surfaces"
```

提交后回写：
- [ ] Task 1 commit: `<SHA>`

---

### Task 2: 把 state_bootstrap 和实例测试隔离改成 instance-dir

**Files:**
- Modify: `server/src/state_bootstrap.rs`
- Modify: `server/src/main.rs`
- Modify: `server/src/assembly.rs`
- Modify: `README.md`
- Test: `server/src/state_bootstrap.rs`
- Test: `server/src/main.rs`
- Test: `server/src/assembly.rs`

- [ ] **Step 1: 先写失败测试，固定“实例测试隔离靠 instance-dir，不靠 environment 字符串”**

在 `server/src/state_bootstrap.rs` 保留现有行为测试，但把测试 helper 改成基于 `tempfile::tempdir()` 的实例目录，并增加一条真正调用 `prepare_state_repository(...)` 的约束测试：

```rust
#[tokio::test]
fn rebuild_mode_only_touches_database_under_current_instance_dir() {
    let instance_dir = tempfile::tempdir().unwrap();
    let config = test_config_with_instance_dir(instance_dir.path(), 90.0);
    let db_path = InstanceDir::new(instance_dir.path()).db_path();

    let _ = prepare_state_repository(&config, &db_path, StateBootstrapMode::Rebuild)
        .await
        .unwrap();

    assert!(db_path.exists());
    assert!(db_path.starts_with(instance_dir.path()));
}
```

- [ ] **Step 2: 运行定向测试，确认当前测试 helper 仍绑在 environment 分桶上**

Run:
`cargo test -p poise-server rebuild_mode_only_touches_database_under_current_instance_dir`

Expected:
- FAIL，原因是 `state_bootstrap` 测试 helper 仍按 `environment` 生成数据库路径或构造 `Config`

- [ ] **Step 3: 写最小实现**

要求：
- `state_bootstrap` 测试 helper 删除 `unique_test_environment()`、`cleanup_environment(...)`、`test_db_path(environment)` 这类按字符串分桶的工具
- 相关测试改用临时实例目录
- `Config` 构造 helper 删除 `environment`
- `README` 的 `--rebuild-state` 说明去掉 `<environment>` 目录层级
- 保留 `rebuild_mode_only_touches_database_under_current_instance_dir` 对真实 `prepare_state_repository(...)` 行为的约束，不降级成纯路径字符串断言

- [ ] **Step 4: 运行最终验证**

Run:
`cargo test -p poise-server`

Expected:
- PASS

Run:
`cargo test -p poise-tui`

Expected:
- PASS

- [ ] **Step 5: 提交**

```bash
git add server/src/state_bootstrap.rs server/src/main.rs server/src/assembly.rs README.md docs/superpowers/plans/2026-04-09-remove-environment.md
git commit -m "refactor: remove environment from state bootstrap"
```

提交后回写：
- [ ] Task 2 commit: `<SHA>`
