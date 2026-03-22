# K9 Mainnet Safety Baseline Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 为 `service` 增加 mainnet 准入保护、启动对账、稳定原因码和实例级安全边界，让一个已配置实例可以安全进入自动运行。

**Architecture:** 新增 `service/src/startup.rs` 承载运行级别解析、mainnet 显式开启、默认持久化路径推导、启动前校验与对账决策，`service/src/main.rs` 只负责组装环境变量、transport 与启动流程。`service/src/application.rs`、`service/src/integrations/binance.rs`、`service/src/storage.rs`、`service/src/kernel.rs` 分别负责安全 bootstrap、签名查询、原因码持久化和运行期硬保护自动暂停；K9 不引入 Web UI 或远程操控，结果统一通过系统事件、日志和现有 control plane 暴露。

**Tech Stack:** Rust 2024、tokio、axum、clap、reqwest、rusqlite、serde、cargo test

---

## 文件结构

### 新建文件

- `service/src/startup.rs`
  - 定义 `RuntimeMode`
  - 解析 `GRID_PLATFORM_BINANCE_ENABLED / GRID_PLATFORM_BINANCE_ENV / GRID_PLATFORM_ALLOW_MAINNET / GRID_PLATFORM_INSTANCE_ID / GRID_PLATFORM_SERVICE_DB_PATH`
  - 推导实例级默认 SQLite 路径
  - 定义启动前校验结果、对账决策与稳定原因码
- `service/tests/mainnet_bootstrap.rs`
  - 覆盖 K9 主线验收：mainnet 显式开启、启动前校验、启动对账、拒绝启动、自动暂停、实例级路径

### 重点修改文件

- `service/src/lib.rs`
  - 导出 `startup` 模块
- `service/src/main.rs`
  - 改用 `startup` 模块统一解析启动配置
  - 在真正 build `Application` 之前执行 mainnet 显式开启和启动前校验
- `service/src/application.rs`
  - 新增“带启动报告”的 bootstrap 路径
  - 在 supervisor 启动前应用启动对账结论与系统事件
- `service/src/integrations/binance.rs`
  - 为 `BinanceTransport` 增加启动前签名查询入口
  - 真实 transport 增加 position/open orders 启动快照拉取
- `service/src/protocol.rs`
  - 为 `SystemEvent` 增加稳定原因码字段
  - 如有必要，补启动报告或运行级别字段
- `service/src/storage.rs`
  - 持久化、恢复 `SystemEvent.code`
  - 为新原因码字段补 SQLite schema 迁移
- `service/src/kernel.rs`
  - 在 breaker 或连续执行失败等硬保护命中时自动暂停策略
  - 记录稳定原因码系统事件
- `service/src/risk.rs`
  - 维持现有 breaker 语义，并为自动暂停留出稳定原因码
- `service/tests/cli.rs`
  - 覆盖 mainnet 未显式开启时启动失败
- `service/tests/binance_integration.rs`
  - 覆盖启动前签名查询与 SQLite bootstrap 结合路径
- `service/tests/control_plane.rs`
  - 覆盖 `SystemEvent.code` 的线协议暴露
- `service/tests/persistence_recovery.rs`
  - 覆盖 `SystemEvent.code` 的持久化和恢复
- `service/tests/kernel_flow.rs`
  - 覆盖 breaker/连续策略执行失败后的自动暂停
- `service/README.md`
  - 补 mainnet 显式开启、实例级路径与 K9 启动约束说明
- `README.md`
  - 补单实例 mainnet 运行的最小配置约束
- `docs/plan.md`
  - K9 实现完成后同步状态
- `TODO.md`
  - K9 验收完成后同步勾选状态与最近验证结果

---

### Task 1: 建立启动配置入口与 mainnet 显式开启

**Files:**
- Create: `service/src/startup.rs`
- Modify: `service/src/lib.rs`
- Modify: `service/src/main.rs`
- Modify: `service/tests/cli.rs`
- Test: `service/src/startup.rs`

- [ ] **Step 1: 先写失败测试，锁定运行级别推导、默认路径和 mainnet 显式开启语义**

```rust
#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{RuntimeMode, StartupConfig};

    #[test]
    fn infers_mainnet_mode_and_default_db_path() {
        let config = StartupConfig::from_pairs([
            ("GRID_PLATFORM_BINANCE_ENABLED", "1"),
            ("GRID_PLATFORM_BINANCE_ENV", "mainnet"),
            ("GRID_PLATFORM_ALLOW_MAINNET", "1"),
        ])
        .expect("startup config");

        assert_eq!(config.runtime_mode, RuntimeMode::Mainnet);
        assert_eq!(config.instance_id, "local");
        assert_eq!(config.db_path, PathBuf::from(".data/mainnet/local.db"));
    }

    #[test]
    fn rejects_mainnet_without_explicit_allow_flag() {
        let error = StartupConfig::from_pairs([
            ("GRID_PLATFORM_BINANCE_ENABLED", "1"),
            ("GRID_PLATFORM_BINANCE_ENV", "mainnet"),
        ])
        .expect_err("mainnet must require explicit allow flag");

        assert!(error.to_string().contains("GRID_PLATFORM_ALLOW_MAINNET=1"));
    }
}
```

```rust
#[test]
fn mainnet_requires_explicit_opt_in() -> Result<()> {
    let output = run_cli_and_wait_with_env(
        &[],
        &[
            ("GRID_PLATFORM_SERVICE_ADDR", "127.0.0.1:0"),
            ("GRID_PLATFORM_BINANCE_ENABLED", "1"),
            ("GRID_PLATFORM_BINANCE_ENV", "mainnet"),
        ],
    )?;

    assert!(!output.status.success());
    assert!(String::from_utf8(output.stderr)?.contains("GRID_PLATFORM_ALLOW_MAINNET=1"));
    Ok(())
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p grid-platform-service startup::tests --lib`
Expected: FAIL，提示 `startup` 模块、`StartupConfig` 或 `RuntimeMode` 不存在

Run: `cargo test -p grid-platform-service --test cli mainnet_requires_explicit_opt_in -- --exact`
Expected: FAIL，当前 CLI 仍会尝试正常启动，而不是直接拒绝 mainnet

- [ ] **Step 3: 只实现最小启动配置解析和 mainnet 显式开启**

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeMode {
    Paper,
    Testnet,
    Mainnet,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupConfig {
    pub runtime_mode: RuntimeMode,
    pub instance_id: String,
    pub db_path: PathBuf,
}

impl StartupConfig {
    pub fn from_pairs<'a>(
        pairs: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> anyhow::Result<Self> {
        // 先推导 RuntimeMode，再检查 mainnet 显式开启，再解析 instance_id 和默认 db_path。
        # unimplemented!()
    }
}
```

```rust
let startup = startup::StartupConfig::from_env()?;
let application = startup.build_application().await?;
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p grid-platform-service startup::tests --lib`
Expected: PASS

Run: `cargo test -p grid-platform-service --test cli mainnet_requires_explicit_opt_in -- --exact`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
git add service/src/startup.rs service/src/lib.rs service/src/main.rs service/tests/cli.rs
git commit -m "feat: add mainnet startup guard config"
```

### Task 2: 为 Binance transport 增加启动前签名快照

**Files:**
- Modify: `service/src/startup.rs`
- Modify: `service/src/integrations/binance.rs`
- Create: `service/tests/mainnet_bootstrap.rs`
- Modify: `service/tests/binance_integration.rs`

- [ ] **Step 1: 先写失败测试，锁定启动前需要拿到的交易所事实**

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_preflight_collects_exchange_position_and_open_orders() -> Result<()> {
    let transport = FakeBinanceTransport::new()
        .with_position_snapshot(PositionSnapshot {
            symbol: "XAUUSDT".into(),
            qty: 1.25,
            avg_price: 2368.5,
            unrealized_pnl: 0.0,
            realized_pnl: 0.0,
        })
        .with_open_orders(vec![sample_open_order("grid_buy_01")]);

    let state = collect_startup_exchange_state("XAUUSDT", Arc::new(transport)).await?;

    assert_eq!(state.position.as_ref().expect("position").qty, 1.25);
    assert_eq!(state.open_orders.len(), 1);
    Ok(())
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p grid-platform-service --test mainnet_bootstrap startup_preflight_collects_exchange_position_and_open_orders -- --exact`
Expected: FAIL，提示启动前快照收集函数或 `fetch_position_snapshot` 不存在

- [ ] **Step 3: 只实现最小签名查询入口与启动前快照收集**

```rust
#[derive(Debug, Clone, PartialEq)]
pub struct StartupExchangeState {
    pub position: Option<PositionSnapshot>,
    pub open_orders: Vec<OpenOrder>,
}

#[async_trait]
pub trait BinanceTransport: Send + Sync + 'static {
    async fn fetch_position_snapshot(&self, _symbol: &str) -> Result<Option<PositionSnapshot>> {
        Ok(None)
    }
}

pub async fn collect_startup_exchange_state(
    symbol: &str,
    transport: Arc<dyn BinanceTransport>,
) -> Result<StartupExchangeState> {
    Ok(StartupExchangeState {
        position: transport.fetch_position_snapshot(symbol).await?,
        open_orders: transport.fetch_open_orders(symbol).await?.unwrap_or_default(),
    })
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p grid-platform-service --test mainnet_bootstrap startup_preflight_collects_exchange_position_and_open_orders -- --exact`
Expected: PASS

Run: `cargo test -p grid-platform-service --test binance_integration sqlite_binance_sync_persists_latest_runtime_for_recovery -- --exact`
Expected: PASS，现有 Binance bootstrap 路径不回退

- [ ] **Step 5: 提交**

```bash
git add service/src/startup.rs service/src/integrations/binance.rs service/tests/mainnet_bootstrap.rs service/tests/binance_integration.rs
git commit -m "feat: add startup exchange snapshot queries"
```

### Task 3: 实现启动对账决策与安全 bootstrap

**Files:**
- Modify: `service/src/startup.rs`
- Modify: `service/src/application.rs`
- Modify: `service/src/main.rs`
- Modify: `service/tests/mainnet_bootstrap.rs`
- Modify: `service/tests/persistence_recovery.rs`

- [ ] **Step 1: 先写失败测试，锁定“继续运行 / 暂停待确认 / 拒绝启动”语义**

```rust
#[test]
fn startup_reconcile_pauses_when_exchange_position_exists_but_persisted_runtime_is_flat() {
    let persisted = PersistedRuntime::sqlite_bootstrap();
    let exchange = StartupExchangeState {
        position: Some(PositionSnapshot {
            symbol: "XAUUSDT".into(),
            qty: 1.0,
            avg_price: 2360.0,
            unrealized_pnl: 0.0,
            realized_pnl: 0.0,
        }),
        open_orders: vec![],
    };

    let decision = reconcile_startup(&persisted, &exchange).expect("decision");

    assert!(matches!(
        decision,
        StartupDecision::Pause { code, .. }
            if code == "STARTUP_RECONCILE_POSITION_MISMATCH"
    ));
}
```

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bootstrap_applies_pause_decision_before_supervisor_starts() -> Result<()> {
    let app = build_mainnet_application_with_reconcile_pause().await?;
    let snapshot = app.snapshot();

    assert_eq!(snapshot.runtime.strategy_state, "paused");
    assert!(app
        .system_events()
        .iter()
        .any(|event| event.code.as_deref() == Some("STARTUP_RECONCILE_POSITION_MISMATCH")));
    Ok(())
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p grid-platform-service --test mainnet_bootstrap startup_reconcile_pauses_when_exchange_position_exists_but_persisted_runtime_is_flat -- --exact`
Expected: FAIL，提示 `StartupDecision`、`reconcile_startup` 或 `SystemEvent.code` 不存在

Run: `cargo test -p grid-platform-service --test mainnet_bootstrap bootstrap_applies_pause_decision_before_supervisor_starts -- --exact`
Expected: FAIL，当前 bootstrap 不会在 supervisor 启动前应用暂停结论

- [ ] **Step 3: 只实现最小启动对账与安全 bootstrap**

```rust
pub enum StartupDecision {
    Continue,
    Pause { code: &'static str, message: String },
    Refuse { code: &'static str, message: String },
}

pub fn reconcile_startup(
    persisted: &PersistedRuntime,
    exchange: &StartupExchangeState,
) -> anyhow::Result<StartupDecision> {
    if exchange.position.as_ref().is_some_and(|position| position.qty.abs() > f64::EPSILON)
        && persisted.snapshot.runtime.position_qty.abs() <= f64::EPSILON
    {
        return Ok(StartupDecision::Pause {
            code: "STARTUP_RECONCILE_POSITION_MISMATCH",
            message: "exchange position exists while persisted runtime is flat".into(),
        });
    }
    Ok(StartupDecision::Continue)
}
```

```rust
let startup = StartupReport::collect(&config, storage.as_ref(), transport.clone()).await?;
let runtime = startup.apply_to(runtime);
let application = Application::bootstrap_with_runtime_storage_and_binance(
    runtime,
    storage,
    config,
    transport,
);
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p grid-platform-service --test mainnet_bootstrap startup_reconcile_pauses_when_exchange_position_exists_but_persisted_runtime_is_flat -- --exact`
Expected: PASS

Run: `cargo test -p grid-platform-service --test mainnet_bootstrap bootstrap_applies_pause_decision_before_supervisor_starts -- --exact`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
git add service/src/startup.rs service/src/application.rs service/src/main.rs service/tests/mainnet_bootstrap.rs service/tests/persistence_recovery.rs
git commit -m "feat: reconcile mainnet startup state before bootstrap"
```

### Task 4: 持久化并暴露稳定原因码

**Files:**
- Modify: `service/src/protocol.rs`
- Modify: `service/src/storage.rs`
- Modify: `service/src/control_plane.rs`
- Modify: `service/src/kernel.rs`
- Modify: `service/tests/persistence_recovery.rs`
- Modify: `service/tests/control_plane.rs`

- [ ] **Step 1: 先写失败测试，锁定 `SystemEvent.code` 的持久化与对外暴露**

```rust
#[test]
fn sqlite_storage_roundtrips_system_event_code() -> Result<()> {
    let storage = SqliteStorage::open(temp.path().join("service.db"))?;
    storage.persist_runtime(&PersistedRuntime {
        snapshot: RuntimeSnapshot::sample(),
        risk_events: vec![],
        system_events: vec![SystemEvent {
            level: "error".into(),
            source: "startup".into(),
            code: Some("STARTUP_RECONCILE_POSITION_MISMATCH".into()),
            message: "startup paused".into(),
            created_at: "2025-01-01T00:00:00Z".into(),
        }],
        last_sequence: 1,
    })?;

    let recovered = storage.load_runtime()?.expect("runtime");
    assert_eq!(
        recovered.system_events[0].code.as_deref(),
        Some("STARTUP_RECONCILE_POSITION_MISMATCH")
    );
    Ok(())
}
```

```rust
assert_eq!(
    alerts["data"]["items"][0]["code"],
    "STARTUP_RECONCILE_POSITION_MISMATCH"
);
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p grid-platform-service --test persistence_recovery sqlite_storage_roundtrips_system_event_code -- --exact`
Expected: FAIL，提示 `SystemEvent.code` 字段或 SQLite schema 不存在

Run: `cargo test -p grid-platform-service --test control_plane web_query_alerts_include_system_event_codes -- --exact`
Expected: FAIL，当前 `/query/alerts` 不会暴露系统事件原因码

- [ ] **Step 3: 只实现最小原因码字段、迁移与映射**

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SystemEvent {
    pub level: String,
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    pub message: String,
    pub created_at: String,
}
```

```rust
ensure_column(connection, "system_events", "code", "TEXT")?;
```

```rust
AlertRecord {
    category: "system".into(),
    severity: event.level.clone(),
    source: event.source,
    code: event.code,
    message: event.message,
    created_at: event.created_at,
    acknowledged_at: None,
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p grid-platform-service --test persistence_recovery sqlite_storage_roundtrips_system_event_code -- --exact`
Expected: PASS

Run: `cargo test -p grid-platform-service --test control_plane web_query_alerts_include_system_event_codes -- --exact`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
git add service/src/protocol.rs service/src/storage.rs service/src/control_plane.rs service/src/kernel.rs service/tests/persistence_recovery.rs service/tests/control_plane.rs
git commit -m "feat: persist startup guard reason codes"
```

### Task 5: 让运行期硬保护自动暂停策略

**Files:**
- Modify: `service/src/kernel.rs`
- Modify: `service/src/risk.rs`
- Modify: `service/tests/kernel_flow.rs`

- [ ] **Step 1: 先写失败测试，锁定 breaker 和连续策略执行失败后的自动暂停**

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn breaker_engagement_auto_pauses_strategy_and_records_guard_code() -> Result<()> {
    let (engine, read_model, _events_rx) = spawn_engine();

    engine
        .sync_runtime(RuntimePatch {
            position_qty: Some(0.5),
            unrealized_pnl: Some(-500.0),
            ..Default::default()
        })
        .await?;

    let snapshot = read_model.read().expect("read model").snapshot();
    assert_eq!(snapshot.runtime.strategy_state, "paused");
    assert!(read_model
        .read()
        .expect("read model")
        .system_events()
        .iter()
        .any(|event| event.code.as_deref() == Some("RUNTIME_GUARD_PAUSED")));
    Ok(())
}
```

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_strategy_sync_failures_pause_strategy_after_threshold() -> Result<()> {
    let runtime = runtime_seed::sample_runtime();
    let adapter = Arc::new(FailingPlacementExecutionAdapter::with_failures(3));
    let (engine, read_model, _events_rx) =
        spawn_engine_with_runtime_and_adapter(runtime, None, adapter);

    for _ in 0..3 {
        let _ = engine.emit_price_tick().await;
    }

    let snapshot = read_model.read().expect("read model").snapshot();
    assert_eq!(snapshot.runtime.strategy_state, "paused");
    assert!(read_model
        .read()
        .expect("read model")
        .system_events()
        .iter()
        .any(|event| event.code.as_deref() == Some("RUNTIME_GUARD_EXECUTION_FAILURES")));
    Ok(())
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p grid-platform-service --test kernel_flow breaker_engagement_auto_pauses_strategy_and_records_guard_code -- --exact`
Expected: FAIL，当前 breaker 只更新 risk，不会自动暂停策略

Run: `cargo test -p grid-platform-service --test kernel_flow repeated_strategy_sync_failures_pause_strategy_after_threshold -- --exact`
Expected: FAIL，当前策略执行失败只回滚当前 tick，不会累计并自动暂停

- [ ] **Step 3: 只实现最小自动暂停与计数器**

```rust
fn apply_runtime_guard(
    snapshot: &mut RuntimeSnapshot,
    system_events: &mut Vec<SystemEvent>,
    reason_code: &'static str,
    message: &str,
) {
    if snapshot.runtime.strategy_state != "paused" {
        snapshot.runtime.strategy_state = "paused".into();
    }
    system_events.insert(
        0,
        SystemEvent {
            level: "error".into(),
            source: "runtime_guard".into(),
            code: Some(reason_code.into()),
            message: message.into(),
            created_at: now_utc(),
        },
    );
}
```

```rust
if self.snapshot.risk.breaker_engaged {
    apply_runtime_guard(
        &mut self.snapshot,
        &mut self.system_events,
        "RUNTIME_GUARD_PAUSED",
        "breaker engaged; strategy paused automatically",
    );
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p grid-platform-service --test kernel_flow breaker_engagement_auto_pauses_strategy_and_records_guard_code -- --exact`
Expected: PASS

Run: `cargo test -p grid-platform-service --test kernel_flow repeated_strategy_sync_failures_pause_strategy_after_threshold -- --exact`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
git add service/src/kernel.rs service/src/risk.rs service/tests/kernel_flow.rs
git commit -m "feat: auto-pause strategy on runtime guard hits"
```

### Task 6: 同步文档并完成 K9 验证

**Files:**
- Modify: `README.md`
- Modify: `service/README.md`
- Modify: `docs/plan.md`
- Modify: `TODO.md`

- [ ] **Step 1: 先写文档要覆盖的关键信息，再补实现后的文档更新**

```markdown
- mainnet 需要显式设置 `GRID_PLATFORM_ALLOW_MAINNET=1`
- 未显式指定 `GRID_PLATFORM_SERVICE_DB_PATH` 时，默认路径按 `instance_id` 和运行环境推导
- 启动前校验失败会拒绝进入自动运行
- 启动对账失败会进入暂停或拒绝启动
```

- [ ] **Step 2: 跑最小目标测试矩阵**

Run: `cargo test -p grid-platform-service --lib`
Expected: PASS

Run: `cargo test -p grid-platform-service --test cli -- --nocapture`
Expected: PASS

Run: `cargo test -p grid-platform-service --test mainnet_bootstrap -- --nocapture`
Expected: PASS

Run: `cargo test -p grid-platform-service --test control_plane -- --nocapture`
Expected: PASS

Run: `cargo test -p grid-platform-service --test persistence_recovery -- --nocapture`
Expected: PASS

Run: `cargo test -p grid-platform-service --test kernel_flow -- --nocapture`
Expected: PASS

- [ ] **Step 3: 跑工作区回归**

Run: `cargo test`
Expected: PASS，全工作区无回退

- [ ] **Step 4: 同步文档状态**

```markdown
- 将 `docs/plan.md` 的 K9 状态改为已完成
- 更新 `TODO.md` 勾选状态与最近验证结果
- 在 README 中写清 mainnet 显式开启与实例级路径约束
```

- [ ] **Step 5: 提交**

```bash
git add README.md service/README.md docs/plan.md TODO.md
git commit -m "docs: document k9 mainnet safety baseline"
```
