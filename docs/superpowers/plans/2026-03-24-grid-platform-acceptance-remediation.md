# Grid Platform 实盘闭环与验收修复 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 修复当前验收阻塞项，让平台从“只会算目标值的框架”变成“状态语义正确、能发出真实订单、能回写真实仓位、能通过整体验收”的可继续演进基线。

**Architecture:** 本次修复不再把问题拆成彼此独立的子计划，因为风险评估、目标/实际仓位分离、订单执行闭环、HTTP/TUI 快照契约共享同一套状态模型。实现上保持六边形架构不变：`grid-core` 负责纯计算，`grid-engine` 负责纯规划和状态变换，`grid-server` 负责把市场数据、用户数据、执行计划和持久化串成真实闭环。

**Tech Stack:** Rust 2024 edition, tokio, axum, reqwest, tokio-tungstenite, rusqlite, serde

## 执行状态

- [x] Task 1 已完成：对外契约统一为 snake_case，`--config` 改为必填，默认风险预算公式修正
- [x] Task 2 已完成：真实仓位、目标仓位、挂单状态拆分，并完成快照/持久化迁移
- [x] Task 3 已完成：风险预算、止损和日亏损限制接入 `evaluate_risk`
- [x] Task 4 已完成：`reconciler` 生成真实订单动作，交易所规则纳入纯计划
- [x] Task 5 已完成：server runtime 补齐 startup sync、tick 执行、user data 回写闭环
- [x] Task 6 已完成：TUI 展示真实仓位/目标仓位/挂单状态，并通过整仓验收
- [x] 2026-03-24 重新验收通过：`cargo fmt --all --check`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo test --workspace -- --nocapture`
- [x] 2026-03-25 Task 5 启动语义修正完成：改为 `user stream -> server time cutoff -> startup sync -> buffered replay -> live apply`，并验证旧 user-data 事件不会回滚 `current_exposure`、`pending_order`、`risk_state`
- [x] 2026-03-25 Task 5 运行期闭环补齐：`PositionUpdate` 与 `CANCELED/REJECTED/EXPIRED` 订单事件会在无新行情时触发一次 `reconcile`，补齐挂单恢复窗口
- [x] 2026-03-25 复验通过：启动流程改为 `user stream -> server time cutoff -> startup sync -> buffered replay -> live apply`，并补齐 runtime/TUI 回归测试
- [x] 2026-03-25 Review 问题修复完成：提交/撤单先持久化 `Submitting` / `Canceling` 意图，订单状态归一化为 `OrderStatus`，快照变化但无领域事件时广播 `SnapshotUpdated`，并删除 SQLite 旧兼容逻辑
- [x] 2026-03-25 重新验收通过：`cargo fmt --all --check`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo test --workspace`

---

## 参考资料

- 架构设计：`docs/superpowers/specs/2026-03-24-grid-platform-architecture-design.md`
- 策略族设计：`docs/superpowers/specs/2026-03-24-grid-strategy-family-design.md`
- 当前实现计划：`docs/superpowers/plans/2026-03-24-grid-platform-phase1-core-engine.md` 到 `docs/superpowers/plans/2026-03-24-grid-platform-phase5-tui.md`
- 当前验收结论：本轮代码审查中的 P0/P1 findings

## 文件结构

### 新建文件

```
server/src/runtime.rs    # 市场数据循环、用户数据循环、执行计划执行、回写真实状态
```

### 修改文件

```
core/src/strategy.rs         # snake_case 契约、GridConfig helper
core/src/events.rs           # snake_case 事件契约
core/src/risk.rs             # 风控输入与 Cap / Deny 规则

engine/src/instance.rs       # 实例运行时：目标仓位、真实仓位、挂单、风险状态
engine/src/ports.rs          # 端口类型：Snapshot、OpenOrder、ExchangePort 契约
engine/src/execution_plan.rs # 订单动作与辅助构造器
engine/src/reconciler.rs     # 纯函数：目标计算 → 风控裁剪 → 执行计划
engine/src/manager.rs        # 真实仓位回写、挂单回写、tick 处理不再伪造仓位
engine/src/lib.rs            # 导出新增模块（如果需要）

storage/src/schema.rs        # 快照表迁移，补 target_exposure / pending_order / risk columns
storage/src/sqlite.rs        # Snapshot 新字段序列化与反序列化

exchanges/binance/src/types.rs      # OpenOrder 增加 symbol，必要时补 realized pnl 字段
exchanges/binance/src/rest.rs       # cancel_all 语义收紧
exchanges/binance/src/adapter.rs    # ExchangePort 契约对齐、clippy 清理
exchanges/binance/src/websocket.rs  # 解析用户流的订单状态、仓位、已实现收益

server/src/assembly.rs       # 组装 runtime，预取 exchange rules，缩小职责
server/src/config.rs         # 默认预算公式修正，snake_case 配置解析
server/src/http.rs           # 快照响应新增 target_exposure / pending_order
server/src/main.rs           # `--config` 变成必填并打印用法

tui/src/protocol.rs               # snake_case 线协议 + 新快照字段
tui/src/views/instance.rs         # 展示真实仓位、目标仓位、挂单状态
tui/src/app.rs                    # Snapshot 缓存兼容新字段
tui/tests/fixtures/*.json         # fixture 改成 snake_case 并补新字段
```

### 职责分解

- `grid-core` 只定义纯数据和纯计算，不能知道 websocket、HTTP、SQLite。
- `grid-engine` 负责两件事：
  1. 在纯函数里算出“应当做什么”
  2. 在同步状态机里回写“已经发生了什么”
- `grid-server` 负责三条异步链路：
  1. 行情 tick 驱动 `reconcile`
  2. 执行计划调用交易所端口
  3. 用户数据 / 持仓回写真实仓位和挂单状态
- `grid-tui` 只消费 server 给出的快照，不自己推导被风控裁剪后的目标仓位。
- 为了让交易所侧按 `symbol` 回写仓位和挂单时语义保持闭合，v1 明确限制“一个 `symbol` 只能绑定一个实例”；同品种多实例分摊同一真实仓位不在本轮范围内。

---

### Task 1: 对齐外部契约与启动护栏

**Files:**
- Modify: `core/src/strategy.rs`
- Modify: `core/src/events.rs`
- Modify: `engine/src/instance.rs`
- Modify: `server/src/config.rs`
- Modify: `server/src/assembly.rs`
- Modify: `server/src/main.rs`
- Modify: `tui/src/protocol.rs`
- Modify: `tui/tests/fixtures/instance_snapshot.json`
- Modify: `tui/tests/fixtures/instance_summaries.json`
- Modify: `tui/tests/fixtures/ws_event.json`
- Modify: `exchanges/binance/src/adapter.rs`
- Modify: `exchanges/binance/src/websocket.rs`

- [ ] **Step 1: 先写会失败的配置与协议测试**

在 `server/src/config.rs` 增加：

```rust
#[test]
fn parses_snake_case_strategy_enums() {
    let config = parse_config(
        r#"
environment = "paper"

[[instances]]
symbol = "BTCUSDT"
lower_price = 90.0
upper_price = 110.0
long_capacity = 8.0
short_capacity = 4.0
capacity_notional = 375.0
shape_family = "concave"
out_of_band_policy = "reduce_only"
"#,
    )
    .unwrap();

    assert_eq!(config.instances[0].shape_family, ShapeFamily::Concave);
    assert_eq!(
        config.instances[0].out_of_band_policy,
        OutOfBandPolicy::ReduceOnly,
    );
}
```

在 `tui/src/protocol.rs` 增加：

```rust
#[test]
fn deserializes_snake_case_snapshot() {
    let snapshot: InstanceSnapshot = serde_json::from_str(
        r#"{
            "id":"BTCUSDT",
            "symbol":"BTCUSDT",
            "status":"holding",
            "current_exposure":3.5,
            "target_exposure":0.0,
            "last_price":112.0,
            "config":{
                "lower_price":90.0,
                "upper_price":110.0,
                "long_capacity":8.0,
                "short_capacity":4.0,
                "capacity_notional":375.0,
                "shape_family":"linear",
                "out_of_band_policy":"freeze"
            }
        }"#,
    )
    .unwrap();

    assert_eq!(snapshot.status, InstanceStatus::Holding);
    assert_eq!(snapshot.config.shape_family, ShapeFamily::Linear);
}
```

在 `server/src/main.rs` 增加：

```rust
#[test]
fn parse_config_path_requires_config_flag() {
    let error = parse_config_path(Vec::<String>::new().into_iter()).unwrap_err();
    assert!(error.to_string().contains("--config"));
}
```

在 `server/src/assembly.rs` 增加：

```rust
#[tokio::test]
async fn assemble_rejects_duplicate_symbols() {
    let config = test_config_with_instances(vec![
        test_instance_config("btc-a", "BTCUSDT"),
        test_instance_config("btc-b", "BTCUSDT"),
    ]);

    let error = assemble(config).await.unwrap_err();
    assert!(error.to_string().contains("duplicate symbol"));
}
```

- [ ] **Step 2: 运行这些测试，确认它们因为当前契约不匹配而失败**

Run: `cargo test -p grid-server parses_snake_case_strategy_enums -- --exact`
Expected: FAIL，原因是 snake_case 解析失败

Run: `cargo test -p grid-server parse_config_path_requires_config_flag -- --exact`
Expected: FAIL，原因是 `parse_config_path` 仍允许缺省

Run: `cargo test -p grid-server assemble_rejects_duplicate_symbols -- --exact`
Expected: FAIL，原因是当前还没有实例级 `symbol` 唯一性校验

Run: `cargo test -p grid-tui deserializes_snake_case_snapshot -- --exact`
Expected: FAIL，原因是 `status` / `shape_family` / `out_of_band_policy` 仍使用 Rust 变体名

- [ ] **Step 3: 最小实现 snake_case 契约和必填启动参数**

在所有对外枚举上统一加：

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShapeFamily { ... }
```

本轮至少覆盖这些类型：

- `core/src/strategy.rs`
  - `ShapeFamily`
  - `OutOfBandPolicy`
  - `BandBoundary`
- `core/src/events.rs`
  - `DomainEvent`
- `engine/src/instance.rs`
  - `InstanceStatus`
- `tui/src/protocol.rs`
  - 同步客户端侧镜像类型

同时把 `server/src/main.rs` 改成：

```rust
fn parse_config_path(mut args: impl Iterator<Item = String>) -> Result<String> {
    while let Some(arg) = args.next() {
        if arg == "--config" {
            return args
                .next()
                .ok_or_else(|| anyhow::anyhow!("missing value for --config"));
        }
        return Err(anyhow::anyhow!("unknown argument: {arg}"));
    }

    Err(anyhow::anyhow!("missing required --config <path>"))
}
```

并在 `server/src/assembly.rs` 启动组装前增加唯一性校验：

```rust
fn validate_unique_symbols(instances: &[InstanceConfig]) -> Result<()> {
    let mut seen = BTreeMap::new();
    for instance in instances {
        if let Some(previous) = seen.insert(instance.symbol.clone(), instance.id.clone()) {
            bail!(
                "duplicate symbol {} for instances {} and {}",
                instance.symbol,
                previous,
                instance.id
            );
        }
    }
    Ok(())
}
```

`assemble` 在创建交易所适配器和 manager 之前先调用它，明确把“同一 `symbol` 多实例”挡在启动阶段之外。

- [ ] **Step 4: 修正默认预算公式**

在 `server/src/config.rs` 把：

```rust
max_notional: self.capacity_notional,
```

改成：

```rust
max_notional: self.long_capacity.max(self.short_capacity) * self.capacity_notional,
```

并补测试断言：

```rust
assert!((config.instances[0].budget().max_notional - 3000.0).abs() < f64::EPSILON);
```

- [ ] **Step 5: 更新 fixture，并顺手清掉 clippy 已知告警**

把 `tui/tests/fixtures/*.json` 改成 snake_case：

- `"status": "active"`
- `"shape_family": "linear"`
- `"out_of_band_policy": "freeze"`
- `"event": { "exposure_target_changed": { ... } }`

并删除测试代码里多余的 `.into()`：

```rust
Message::Text(payload.to_string())
```

- [ ] **Step 6: 重新运行最小验证**

Run: `cargo test -p grid-server parses_snake_case_strategy_enums -- --exact`
Expected: PASS

Run: `cargo test -p grid-server parse_config_path_requires_config_flag -- --exact`
Expected: PASS

Run: `cargo test -p grid-server assemble_rejects_duplicate_symbols -- --exact`
Expected: PASS

Run: `cargo test -p grid-tui deserializes_snake_case_snapshot -- --exact`
Expected: PASS

Run: `cargo clippy -p grid-binance --tests -- -D warnings`
Expected: PASS

- [ ] **Step 7: 提交**

```bash
git add core/src/strategy.rs core/src/events.rs engine/src/instance.rs server/src/config.rs server/src/assembly.rs server/src/main.rs tui/src/protocol.rs tui/tests/fixtures exchanges/binance/src/adapter.rs exchanges/binance/src/websocket.rs
git commit -m "fix: align config and wire protocol contracts"
```

---

### Task 2: 拆分真实仓位、目标仓位和挂单状态

**Files:**
- Modify: `engine/src/instance.rs`
- Modify: `engine/src/ports.rs`
- Modify: `engine/src/manager.rs`
- Modify: `storage/src/schema.rs`
- Modify: `storage/src/sqlite.rs`
- Modify: `server/src/assembly.rs`
- Modify: `server/src/http.rs`
- Modify: `tui/src/protocol.rs`
- Modify: `tui/src/app.rs`
- Modify: `tui/src/views/instance.rs`

- [ ] **Step 1: 先写会失败的状态语义测试**

在 `engine/src/manager.rs` 增加：

```rust
#[test]
fn on_price_tick_returns_tick_outcome_with_plan_and_events() {
    let mut manager = test_manager();
    manager
        .add_instance(
            "btc1".into(),
            "BTCUSDT".into(),
            test_config(),
            test_budget(),
            test_rules(),
        )
        .unwrap();

    let tick = PriceTick {
        symbol: "BTCUSDT".into(),
        last_price: 90.0,
        mark_price: 90.0,
        timestamp: Utc::now(),
    };

    let outcome = manager.on_price_tick(&tick).unwrap();

    assert!(outcome.plan.has_actions());
    assert!(!outcome.events.is_empty());
}

#[test]
fn on_price_tick_updates_target_without_faking_current_exposure() {
    let mut manager = test_manager();
    manager
        .add_instance(
            "btc1".into(),
            "BTCUSDT".into(),
            test_config(),
            test_budget(),
            test_rules(),
        )
        .unwrap();

    let tick = PriceTick {
        symbol: "BTCUSDT".into(),
        last_price: 90.0,
        mark_price: 90.0,
        timestamp: Utc::now(),
    };

    let _ = manager.on_price_tick(&tick);
    let instance = manager.get_instance("btc1").unwrap();

    assert_eq!(instance.current_exposure.0, 0.0);
    assert_eq!(instance.target_exposure.as_ref().unwrap().0, 8.0);
}
```

在 `storage/src/sqlite.rs` 增加 roundtrip 测试：

```rust
assert_eq!(loaded.target_exposure, Some(Exposure(6.0)));
assert!(loaded.pending_order.is_some());
```

在 `server/src/http.rs` 增加快照测试：

```rust
assert_eq!(payload.target_exposure, Some(4.0));
assert!(payload.pending_order.is_some());
```

- [ ] **Step 2: 运行这些测试，确认它们因为 snapshot/state 还没扩宽而失败**

Run: `cargo test -p grid-engine on_price_tick_updates_target_without_faking_current_exposure -- --exact`
Expected: FAIL，原因是 `add_instance` / `StrategyInstance` 还没有 `target_exposure`

Run: `cargo test -p grid-engine on_price_tick_returns_tick_outcome_with_plan_and_events -- --exact`
Expected: FAIL，原因是 `on_price_tick` 还没有返回可供 runtime 执行的 `TickOutcome`

Run: `cargo test -p grid-storage save_and_load_roundtrip -- --exact`
Expected: FAIL，原因是 snapshot 新字段尚未持久化

Run: `cargo test -p grid-server get_snapshot_returns_instance_snapshot -- --exact`
Expected: FAIL，原因是 HTTP 快照里还没有目标仓位和挂单信息

- [ ] **Step 3: 定义实例运行时的新状态模型**

在 `engine/src/instance.rs` 增加：

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingOrder {
    pub symbol: String,
    pub order_id: Option<String>,
    pub client_order_id: String,
    pub side: Side,
    pub price: f64,
    pub quantity: f64,
    pub target_exposure: Exposure,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RiskState {
    pub realized_pnl_day: Option<NaiveDate>,
    pub realized_pnl_today: f64,
    pub unrealized_pnl: f64,
}
```

并把 `StrategyInstance` 扩成：

```rust
pub struct StrategyInstance {
    pub id: String,
    pub symbol: String,
    pub config: GridConfig,
    pub exchange_rules: ExchangeRules,
    pub status: InstanceStatus,
    pub current_exposure: Exposure,
    pub target_exposure: Option<Exposure>,
    pub pending_order: Option<PendingOrder>,
    pub risk_state: RiskState,
    pub last_price: Option<f64>,
    pub out_of_band_since: Option<DateTime<Utc>>,
}
```

- [ ] **Step 4: 扩宽 Snapshot、HTTP 响应和 TUI 协议**

把 `engine/src/ports.rs::InstanceSnapshot`、`server/src/http.rs::InstanceSnapshot`、`tui/src/protocol.rs::InstanceSnapshot` 同步成：

```rust
pub struct InstanceSnapshot {
    pub id: String,
    pub symbol: String,
    pub status: InstanceStatus,
    pub current_exposure: f64,
    pub target_exposure: Option<f64>,
    pub last_price: Option<f64>,
    pub pending_order: Option<PendingOrder>,
    pub config: GridConfig,
}
```

客户端侧 `target_exposure()` helper 改成优先用服务端给出的字段，只在 `None` 时再退回本地推导。

- [ ] **Step 5: 做 SQLite 迁移，不允许破坏已有 `.data`**

在 `storage/src/schema.rs` 增加列迁移测试：

```rust
#[test]
fn initialize_upgrades_existing_snapshot_table() { ... }
```

实现一个 `ensure_column` helper：

```rust
fn ensure_column(conn: &Connection, table: &str, column: &str, ddl: &str) -> Result<()> {
    let exists = ... PRAGMA table_info(table) ...
    if !exists {
        conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {ddl}"))?;
    }
    Ok(())
}
```

至少补这些列：

- `target_exposure REAL`
- `pending_order_json TEXT`
- `realized_pnl_day TEXT`
- `realized_pnl_today REAL NOT NULL DEFAULT 0`
- `unrealized_pnl REAL NOT NULL DEFAULT 0`

- [ ] **Step 6: 修正 manager 的 tick 更新行为**

`on_price_tick` 只允许更新：

- `target_exposure`
- `status`
- `last_price`
- `events`
- `plan`

不允许再直接写：

```rust
instance.current_exposure = result.target_exposure;
```

替代为：

```rust
instance.target_exposure = Some(result.target_exposure);
```

`current_exposure` 只允许通过后续的持仓同步方法更新。

同时把 manager 和 runtime 的边界写死成显式返回值：

```rust
pub struct TickOutcome {
    pub plan: ExecutionPlan,
    pub events: Vec<DomainEvent>,
}

pub fn on_price_tick(&mut self, tick: &PriceTick) -> Result<TickOutcome> {
    ...
    Ok(TickOutcome {
        plan: result.plan,
        events,
    })
}
```

这样 runtime 拿到的不是隐式副作用，而是可执行的 `plan` 和可广播的 `events`。

- [ ] **Step 7: 重新运行任务 2 的定向测试**

Run: `cargo test -p grid-engine on_price_tick_updates_target_without_faking_current_exposure -- --exact`
Expected: PASS

Run: `cargo test -p grid-engine on_price_tick_returns_tick_outcome_with_plan_and_events -- --exact`
Expected: PASS

Run: `cargo test -p grid-storage save_and_load_roundtrip -- --exact`
Expected: PASS

Run: `cargo test -p grid-storage initialize_upgrades_existing_snapshot_table -- --exact`
Expected: PASS

Run: `cargo test -p grid-server get_snapshot_returns_instance_snapshot -- --exact`
Expected: PASS

- [ ] **Step 8: 提交**

```bash
git add engine/src/instance.rs engine/src/ports.rs engine/src/manager.rs storage/src/schema.rs storage/src/sqlite.rs server/src/assembly.rs server/src/http.rs tui/src/protocol.rs tui/src/app.rs tui/src/views/instance.rs
git commit -m "refactor: separate actual exposure from target exposure"
```

---

### Task 3: 让风控真正消费预算和风险状态

**Files:**
- Modify: `core/src/risk.rs`
- Modify: `engine/src/reconciler.rs`
- Modify: `engine/src/manager.rs`
- Modify: `engine/src/instance.rs`

- [ ] **Step 1: 先写会失败的纯函数测试**

在 `core/src/risk.rs` 增加：

```rust
#[test]
fn caps_target_when_max_notional_is_exceeded() {
    let intent = ExposureIntent {
        current: Exposure(0.0),
        target: Exposure(12.0),
        unit_notional: 375.0,
        realized_pnl_today: 0.0,
        unrealized_pnl: 0.0,
    };
    let budget = CapacityBudget {
        max_notional: 3000.0,
        daily_loss_limit: -500.0,
        stop_loss_pct: 20.0,
    };

    assert_eq!(evaluate_risk(&intent, &budget), RiskDecision::Cap(Exposure(8.0)));
}

#[test]
fn caps_to_zero_when_daily_loss_limit_is_breached() {
    let intent = ExposureIntent {
        current: Exposure(4.0),
        target: Exposure(6.0),
        unit_notional: 375.0,
        realized_pnl_today: -600.0,
        unrealized_pnl: 0.0,
    };
    let budget = budget();

    assert_eq!(evaluate_risk(&intent, &budget), RiskDecision::Cap(Exposure(0.0)));
}

#[test]
fn caps_to_zero_when_stop_loss_pct_is_breached() {
    let intent = ExposureIntent {
        current: Exposure(4.0),
        target: Exposure(4.0),
        unit_notional: 375.0,
        realized_pnl_today: -400.0,
        unrealized_pnl: -250.0,
    };
    let budget = CapacityBudget {
        max_notional: 3000.0,
        daily_loss_limit: -1000.0,
        stop_loss_pct: 20.0,
    };

    assert_eq!(evaluate_risk(&intent, &budget), RiskDecision::Cap(Exposure(0.0)));
}
```

- [ ] **Step 2: 跑纯函数测试，确认当前实现仍然全部 `Allow`**

Run: `cargo test -p grid-core caps_target_when_max_notional_is_exceeded -- --exact`
Expected: FAIL，原因是 `evaluate_risk` 还没消费 `max_notional`

Run: `cargo test -p grid-core caps_to_zero_when_daily_loss_limit_is_breached -- --exact`
Expected: FAIL，原因是 `evaluate_risk` 还没消费 `daily_loss_limit`

Run: `cargo test -p grid-core caps_to_zero_when_stop_loss_pct_is_breached -- --exact`
Expected: FAIL，原因是 `evaluate_risk` 还没消费 `stop_loss_pct`

- [ ] **Step 3: 扩宽风控输入，而不是在 engine 外面偷偷裁剪**

把 `ExposureIntent` 改成：

```rust
pub struct ExposureIntent {
    pub current: Exposure,
    pub target: Exposure,
    pub unit_notional: f64,
    pub realized_pnl_today: f64,
    pub unrealized_pnl: f64,
}
```

实现规则：

```rust
pub fn evaluate_risk(intent: &ExposureIntent, budget: &CapacityBudget) -> RiskDecision {
    let total_pnl = intent.realized_pnl_today + intent.unrealized_pnl;
    let stop_loss_triggered =
        budget.max_notional > f64::EPSILON &&
        (-total_pnl / budget.max_notional) * 100.0 >= budget.stop_loss_pct;

    if total_pnl <= budget.daily_loss_limit || stop_loss_triggered {
        return RiskDecision::Cap(Exposure(0.0));
    }

    let max_abs_exposure = budget.max_notional / intent.unit_notional;
    if intent.target.0.abs() > max_abs_exposure {
        return RiskDecision::Cap(Exposure(intent.target.0.signum() * max_abs_exposure));
    }

    RiskDecision::Allow(intent.target.clone())
}
```

- [ ] **Step 4: 在 reconciler 里接上新的风险输入**

构造 `ExposureIntent` 时补上：

```rust
let intent = ExposureIntent {
    current: instance.current_exposure.clone(),
    target: target.clone(),
    unit_notional: instance.config.capacity_notional,
    realized_pnl_today: instance.risk_state.realized_pnl_today,
    unrealized_pnl: instance.risk_state.unrealized_pnl,
};
```

并补 engine 侧测试：

```rust
#[test]
fn reconcile_emits_risk_cap_event_when_budget_caps_target() { ... }
```

- [ ] **Step 5: 明确风险状态只通过真实运行时事件更新**

不要再额外引入和 `apply_position_update` / `apply_order_update` 重叠的 `sync_risk_state` 入口。

本轮统一约束为：

- `unrealized_pnl` 只由 `apply_position_update` 回写
- `realized_pnl_today` 只由 `apply_order_update` 回写
- `evaluate_risk` 只消费 `instance.risk_state`

这样实现者不会在 server 层偷偷维护另一份风险状态。

- [ ] **Step 6: 重新运行 core + engine 的风险验证**

Run: `cargo test -p grid-core risk::tests -- --nocapture`
Expected: PASS

Run: `cargo test -p grid-engine reconciler::tests::reconcile_emits_risk_cap_event_when_budget_caps_target -- --exact`
Expected: PASS

- [ ] **Step 7: 提交**

```bash
git add core/src/risk.rs engine/src/reconciler.rs engine/src/manager.rs engine/src/instance.rs
git commit -m "feat: implement budget-aware risk evaluation"
```

---

### Task 4: 让 reconciler 生成真实订单动作

**Files:**
- Modify: `engine/src/execution_plan.rs`
- Modify: `engine/src/reconciler.rs`
- Modify: `engine/src/instance.rs`
- Modify: `engine/src/manager.rs`
- Modify: `engine/src/ports.rs`
- Modify: `exchanges/binance/src/types.rs`
- Modify: `exchanges/binance/src/websocket.rs`

- [ ] **Step 1: 先写会失败的执行计划测试**

在 `engine/src/reconciler.rs` 增加：

```rust
#[test]
fn reconcile_generates_submit_order_for_delta() {
    let mut instance = test_instance();
    instance.status = InstanceStatus::Active;
    instance.current_exposure = Exposure(0.0);
    instance.last_price = Some(90.0);

    let result = reconcile(&instance, 90.0, &test_budget());

    assert!(matches!(
        result.plan.actions.as_slice(),
        [ExecutionAction::SubmitOrder(_)]
    ));
}

#[test]
fn reconcile_does_not_resubmit_when_pending_order_already_matches_target() {
    let mut instance = test_instance();
    instance.status = InstanceStatus::Active;
    instance.pending_order = Some(test_pending_order(Exposure(8.0)));

    let result = reconcile(&instance, 90.0, &test_budget());
    assert!(!result.plan.has_actions());
}

#[test]
fn freeze_keeps_last_in_band_target_instead_of_current_exposure() {
    let mut instance = test_instance();
    instance.status = InstanceStatus::Active;
    instance.current_exposure = Exposure(4.0);
    instance.target_exposure = Some(Exposure(6.0));
    instance.config.out_of_band_policy = OutOfBandPolicy::Freeze;

    let result = reconcile(&instance, 85.0, &test_budget());

    assert_eq!(result.target_exposure.0, 6.0);
    assert!(!result.plan.has_actions());
}
```

- [ ] **Step 2: 运行这些测试，确认当前实现仍然只有 `NoOp`**

Run: `cargo test -p grid-engine reconcile_generates_submit_order_for_delta -- --exact`
Expected: FAIL，原因是 `ExecutionPlan` 还没生成真实下单动作

Run: `cargo test -p grid-engine reconcile_does_not_resubmit_when_pending_order_already_matches_target -- --exact`
Expected: FAIL，原因是当前实现还没有 pending order 匹配逻辑

Run: `cargo test -p grid-engine freeze_keeps_last_in_band_target_instead_of_current_exposure -- --exact`
Expected: FAIL，原因是当前 `freeze` / `hold` 仍然按 `current_exposure` 冻结

- [ ] **Step 3: 把交易所规则纳入实例状态**

把 `InstanceManager::add_instance` 改成：

```rust
pub fn add_instance(
    &mut self,
    id: String,
    symbol: String,
    config: GridConfig,
    budget: CapacityBudget,
    exchange_rules: ExchangeRules,
) -> Result<()>
```

`StrategyInstance::new` 也同步接收 `exchange_rules`。

- [ ] **Step 4: 在 pure plan 里生成可执行订单**

在 `engine/src/execution_plan.rs` 增加 helper：

```rust
pub fn round_to_step(value: f64, step: f64) -> f64 { ... }
pub fn is_meetable_minimum(price: f64, quantity: f64, rules: &ExchangeRules) -> bool { ... }
```

在 `reconcile` 中把 delta 翻译成 `OrderRequest`：

```rust
let delta = instance.current_exposure.delta(&approved_target);
let quantity = round_to_step(
    delta.0.abs() * instance.config.capacity_unit_qty(),
    instance.exchange_rules.quantity_step,
);

if quantity < instance.exchange_rules.min_qty
    || quantity * price < instance.exchange_rules.min_notional
{
    return ReconcileResult { plan: ExecutionPlan::noop(), ... };
}

let side = Side::from_exposure(&delta).unwrap();
let request = OrderRequest {
    symbol: instance.symbol.clone(),
    side,
    price: round_to_step(price, instance.exchange_rules.price_tick),
    quantity,
    client_order_id: format!("{}-{}", instance.id, Utc::now().timestamp_millis()),
};
```

`freeze` / `hold` 要显式保留“离开带之前最后一个目标值”，不能退化成冻结真实仓位：

```rust
let frozen_target = instance
    .target_exposure
    .clone()
    .unwrap_or_else(|| instance.current_exposure.clone());
```

在 `apply_out_of_band` 中，把 `Freeze` / `Hold` 都改成返回 `frozen_target`。

同时增加带外守卫，避免为了追赶冻结目标而继续加风险：

```rust
let would_increase_risk_out_of_band =
    matches!(
        band,
        BandStatus::OutOfBand {
            policy: OutOfBandPolicy::Freeze | OutOfBandPolicy::Hold,
            ..
        }
    ) && approved_target.0.abs() > instance.current_exposure.0.abs();

if would_increase_risk_out_of_band {
    return ReconcileResult {
        plan: ExecutionPlan::noop(),
        target_exposure: approved_target,
        new_status,
    };
}
```

如果存在不匹配的挂单，动作顺序应为：

```rust
vec![ExecutionAction::CancelAll, ExecutionAction::SubmitOrder(request)]
```

- [ ] **Step 5: 扩宽订单更新类型，保证 user stream 能路由回实例**

当前 `OpenOrder` 没有 `symbol`，先在 `engine/src/ports.rs` 加上：

```rust
pub struct OpenOrder {
    pub symbol: String,
    pub order_id: String,
    pub client_order_id: String,
    pub realized_pnl: f64,
    ...
}
```

然后在 `exchanges/binance/src/types.rs`、`exchanges/binance/src/websocket.rs` 的 JSON 解析里把 `symbol` 补全，并把 Binance `ORDER_TRADE_UPDATE.rp` 映射进 `realized_pnl`。

- [ ] **Step 6: 让 manager 只记录挂单，不伪造成交**

新增这些方法：

```rust
pub fn record_submitted_order(&mut self, id: &str, pending: PendingOrder) -> Result<()>;
pub fn clear_pending_order(&mut self, symbol: &str) -> Result<()>;
pub fn apply_position_update(&mut self, position: &Position) -> Result<()>;
pub fn apply_order_update(&mut self, order: &OpenOrder) -> Result<()>;
```

其中：

- `record_submitted_order` 在下单成功后写 `pending_order`
- 以上按 `symbol` 路由的方法依赖 Task 1 已建立的“实例 `symbol` 全局唯一”约束；本轮不支持同一 `symbol` 多实例共享真实仓位
- `apply_position_update` 负责把 `qty` 换算成容量单位：

```rust
let unit_qty = instance.config.capacity_unit_qty();
instance.current_exposure = if unit_qty <= f64::EPSILON {
    Exposure(0.0)
} else {
    Exposure(position.qty / unit_qty)
};
instance.risk_state.unrealized_pnl = position.unrealized_pnl;
```

- `apply_order_update` 需要同时覆盖“恢复挂单”和“清理挂单”两种路径：

```rust
if matches!(order.status.as_str(), "NEW" | "PARTIALLY_FILLED") {
    instance.pending_order = Some(PendingOrder {
        symbol: order.symbol.clone(),
        order_id: Some(order.order_id.clone()),
        client_order_id: order.client_order_id.clone(),
        side: order.side,
        price: order.price,
        quantity: order.orig_qty,
        target_exposure: instance
            .pending_order
            .as_ref()
            .map(|pending| pending.target_exposure.clone())
            .or_else(|| instance.target_exposure.clone())
            .unwrap_or_else(|| instance.current_exposure.clone()),
        status: order.status.clone(),
    });
    return Ok(());
}
```

这样启动同步把 `get_open_orders()` 喂进来时，就能先恢复 `pending_order`，第一笔 tick 不会因为“本地误以为没有挂单”而重复下单。

- `apply_order_update` 在终态时清理本地挂单，只清理当前实例上同一 `order_id` 或 `client_order_id` 的订单：

```rust
if matches!(order.status.as_str(), "FILLED" | "CANCELED" | "EXPIRED" | "REJECTED") {
    let should_clear = instance
        .pending_order
        .as_ref()
        .map(|pending| {
            pending.order_id.as_deref() == Some(order.order_id.as_str())
                || pending.client_order_id == order.client_order_id
        })
        .unwrap_or(false);

    if should_clear {
        instance.pending_order = None;
    }
}
```

- `apply_order_update` 在接收 websocket 订单事件时累计 `realized_pnl_today`，并按 UTC 日切。`realized_pnl` 视为成交增量，不能只在终态时累计：

```rust
let today = self.clock.now().date_naive();
if instance.risk_state.realized_pnl_day != Some(today) {
    instance.risk_state.realized_pnl_day = Some(today);
    instance.risk_state.realized_pnl_today = 0.0;
}

if order.realized_pnl.abs() > f64::EPSILON {
    instance.risk_state.realized_pnl_today += order.realized_pnl;
}
```

- 新增 manager 测试，确保启动同步和用户流都能回填挂单：

```rust
#[test]
fn apply_order_update_rebuilds_pending_order_for_open_status() { ... }

#[test]
fn apply_order_update_clears_matching_pending_order_on_terminal_status() { ... }
```

- [ ] **Step 7: 重新跑 engine 的纯计划测试**

Run: `cargo test -p grid-engine reconciler::tests -- --nocapture`
Expected: PASS

Run: `cargo test -p grid-engine manager::tests -- --nocapture`
Expected: PASS，且新增测试覆盖 pending order 与 position sync

- [ ] **Step 8: 提交**

```bash
git add engine/src/execution_plan.rs engine/src/reconciler.rs engine/src/instance.rs engine/src/manager.rs engine/src/ports.rs exchanges/binance/src/types.rs exchanges/binance/src/websocket.rs
git commit -m "feat: generate executable orders from reconcile plans"
```

---

### Task 5: 在 server 里补齐真实执行闭环

**Files:**
- Create: `server/src/runtime.rs`
- Modify: `server/src/assembly.rs`
- Modify: `server/src/main.rs`
- Modify: `server/src/http.rs`
- Modify: `exchanges/binance/src/adapter.rs`
- Modify: `exchanges/binance/src/rest.rs`

- [ ] **Step 1: 先写会失败的集成测试**

在 `server/src/assembly.rs` 或新建 `server/src/runtime.rs` 测试里增加：

```rust
#[tokio::test]
async fn market_tick_submits_order_and_records_pending_order() { ... }

#[tokio::test]
async fn position_update_reconciles_actual_exposure_without_overwriting_target() { ... }

#[tokio::test]
async fn order_update_clears_pending_order_on_terminal_status() { ... }

#[tokio::test]
async fn startup_sync_uses_live_position_and_open_orders_before_first_tick() { ... }

#[tokio::test]
async fn filled_order_updates_realized_pnl_and_trips_daily_loss_cap() { ... }
```

测试里的 fake exchange 需要能记录：

- 收到的 `submit_order`
- 收到的 `cancel_all`
- 推送的 `PriceTick`
- 推送的 `UserDataEvent`

其中 `startup_sync_uses_live_position_and_open_orders_before_first_tick` 需要同时断言：

- assembly 恢复的持久化快照先被装回 manager
- `runtime.start()` 启动同步后，真实 `position` / `open_orders` 覆盖快照里的 `current_exposure` / `pending_order`
- 快照里的 `target_exposure`、`out_of_band_since` 这类本地上下文继续保留
- 第一笔 tick 不会对启动前已经存在的挂单重复下单

- [ ] **Step 2: 运行这些集成测试，确认当前 server 还没有执行闭环**

Run: `cargo test -p grid-server market_tick_submits_order_and_records_pending_order -- --exact`
Expected: FAIL，原因是 `start_market_data_tasks` 还只做 tick → mutate，没有执行动作

Run: `cargo test -p grid-server position_update_reconciles_actual_exposure_without_overwriting_target -- --exact`
Expected: FAIL，原因是当前还没有 user stream / position sync 回写

Run: `cargo test -p grid-server order_update_clears_pending_order_on_terminal_status -- --exact`
Expected: FAIL，原因是当前还没有 pending order 终态清理逻辑

Run: `cargo test -p grid-server startup_sync_uses_live_position_and_open_orders_before_first_tick -- --exact`
Expected: FAIL，原因是当前启动路径还没有把“恢复本地快照”与“实时状态覆盖”串成同一条启动链路

Run: `cargo test -p grid-server filled_order_updates_realized_pnl_and_trips_daily_loss_cap -- --exact`
Expected: FAIL，原因是当前 order update 还没有把 `realized_pnl_today` 串到风控

- [ ] **Step 3: 新建 `server/src/runtime.rs`，把 assembly.rs 里的运行时逻辑搬出去**

新文件提供：

```rust
pub struct Runtime {
    state: AppState,
    exchange: Arc<dyn ExchangePort>,
    market_data: Arc<dyn MarketDataPort>,
}

pub struct RuntimeHandles {
    pub market_task: JoinHandle<()>,
    pub user_task: JoinHandle<()>,
}

impl Runtime {
    pub async fn start(&self) -> Result<RuntimeHandles> { ... }
}
```

`assembly.rs` 只做：

- 创建适配器
- 预取每个 symbol 的 `ExchangeRules`
- 组装 `InstanceManager`
- 从 SQLite 读取已持久化快照，并调用 `manager.restore_instance_state(...)`
- 返回 `Platform { state, runtime }`

`Runtime::start()` 的职责必须写死：

1. 先做启动同步，失败就返回错误
2. 启动同步成功后再 `tokio::spawn` 市场数据任务和用户数据任务
3. 返回 `RuntimeHandles`，自己不能阻塞 HTTP 服务启动

`server/src/main.rs` 的启动顺序固定为：

```rust
let platform = assemble(config).await?;
let _runtime_handles = platform.runtime.start().await?;
serve_http(platform.state.clone()).await?;
```

也就是说：先恢复快照，再做实时同步，再启动后台任务，最后启动 HTTP / WS 服务。

- [ ] **Step 4: 在 bootstrap 阶段预取 exchange rules**

在 `assemble` 中，对每个实例做：

```rust
let info = exchange.get_exchange_info(&instance.symbol).await?;
manager.add_instance(
    instance_id.clone(),
    instance.symbol.clone(),
    instance.grid_config(),
    instance.budget(),
    info.rules,
)?;
```

这样 `reconcile` 不需要再访问 IO。

assembly 里的恢复顺序也要写明：

```rust
manager.add_instance(...)?;
if let Some(snapshot) = storage.load_instance_state(&instance_id)? {
    manager.restore_instance_state(snapshot)?;
}
```

恢复目的不是拿本地快照当真值，而是恢复：

- `target_exposure`
- `last_price`
- `out_of_band_since`
- `risk_state.realized_pnl_today`

在 `runtime.start()` 真正启动 tick / user stream 之前，再做一次启动同步：

```rust
let position = exchange.get_position(&instance.symbol).await?;
let open_orders = exchange.get_open_orders(&instance.symbol).await?;

manager.apply_position_update(&position)?;
for order in open_orders {
    manager.apply_order_update(&order)?;
}
```

同步原则：

- Task 1 已保证 `symbol` 全局唯一，因此按 `symbol` 拉取和回写不会把不同实例的真实状态混在一起
- 交易所真实仓位和挂单优先于持久化快照
- 持久化快照只负责补齐本地运行时上下文
- `get_open_orders()` 返回的 `NEW` / `PARTIALLY_FILLED` 订单必须先重建本地 `pending_order`，再允许第一笔 tick 进入 `reconcile`
- 如果启动同步失败，`runtime.start()` 返回错误，不允许带着零状态继续跑

- [ ] **Step 5: 实现 tick → plan → execute → persist 的顺序**

`runtime.rs` 里保持这个顺序：

1. 持锁调用 `manager.on_price_tick(&tick)`，拿到 `ExecutionPlan` 和要广播的事件
   具体类型是 `TickOutcome { plan, events }`
2. 释放锁
3. 对 `ExecutionPlan.actions` 顺序执行
4. 下单成功后重新持锁 `record_submitted_order`
5. 每次状态变化后持久化 snapshot
6. 最后广播事件

核心骨架：

```rust
let outcome = mutate_instance(... manager.on_price_tick(&tick) ...).await?;
for action in outcome.plan.actions {
    match action {
        ExecutionAction::CancelAll => exchange.cancel_all(&symbol).await?,
        ExecutionAction::SubmitOrder(req) => {
            let receipt = exchange.submit_order(req.clone()).await?;
            mutate_instance(... manager.record_submitted_order(...)).await?;
        }
        ExecutionAction::NoOp => {}
        _ => {}
    }
}

broadcast(outcome.events).await?;
```

- [ ] **Step 6: 启动 user data stream，把真实仓位回写到 manager**

新增用户流任务：

```rust
match market_data.subscribe_user_data().await {
    Ok(mut receiver) => {
        while let Some(event) = receiver.recv().await {
            match event {
                UserDataEvent::PositionUpdate(position) => {
                    mutate_instance(... manager.apply_position_update(&position)).await?;
                }
                UserDataEvent::OrderUpdate(order) => {
                    mutate_instance(... manager.apply_order_update(&order)).await?;
                }
            }
        }
    }
    Err(error) => tracing::warn!(...),
}
```

挂单终态至少覆盖：

- `FILLED`
- `CANCELED`
- `EXPIRED`
- `REJECTED`

并在 `OrderUpdate` 处理里显式串起已实现收益：

- 解析 Binance `ORDER_TRADE_UPDATE.rp`
- 累加到 `risk_state.realized_pnl_today`
- 下一次 tick 时让 `daily_loss_limit` / `stop_loss_pct` 参与真实风控

- [ ] **Step 7: 缩紧 `cancel_all` 契约，去掉不可靠的返回 ID**

把 `ExchangePort` 改成：

```rust
async fn cancel_all(&self, symbol: &str) -> Result<()>;
```

并同步更新：

- `engine/src/ports.rs`
- `exchanges/binance/src/adapter.rs`
- `server/src/*` fake implementations
- `engine/src/manager.rs` fake implementations

因为 Binance 的 `/allOpenOrders` 端点本身不保证返回“被撤销订单 ID 快照”，继续暴露 `Vec<String>` 只会制造错误承诺。

- [ ] **Step 8: 重新运行 server 闭环集成测试**

Run: `cargo test -p grid-server assembly::tests -- --nocapture`
Expected: PASS

Run: `cargo test -p grid-server http::tests -- --nocapture`
Expected: PASS，尤其是 pause/resume 和 snapshot 测试继续稳定

Run: `cargo test -p grid-server startup_sync_uses_live_position_and_open_orders_before_first_tick -- --exact`
Expected: PASS，并断言启动后第一笔 tick 不会对已存在挂单重复下单

Run: `cargo test -p grid-server filled_order_updates_realized_pnl_and_trips_daily_loss_cap -- --exact`
Expected: PASS

- [ ] **Step 9: 提交**

```bash
git add server/src/runtime.rs server/src/assembly.rs server/src/main.rs server/src/http.rs engine/src/ports.rs exchanges/binance/src/adapter.rs exchanges/binance/src/rest.rs
git commit -m "feat(server): wire reconcile plans into exchange execution"
```

---

### Task 6: 更新 TUI 展示并完成整体验证

**Files:**
- Modify: `tui/src/protocol.rs`
- Modify: `tui/src/views/instance.rs`
- Modify: `tui/src/app.rs`
- Modify: `tui/src/main.rs`
- Modify: `server/src/http.rs`
- Modify: `configs/test.toml`
- Modify: `docs/superpowers/plans/2026-03-24-grid-platform-acceptance-remediation.md`

- [ ] **Step 1: 先写会失败的 TUI 展示测试**

在 `tui/src/views/instance.rs` 增加：

```rust
assert!(text.contains("actual exposure"));
assert!(text.contains("target exposure"));
assert!(text.contains("pending order"));
```

并把测试快照构造体补成：

```rust
InstanceSnapshot {
    current_exposure: 1.0,
    target_exposure: Some(4.0),
    pending_order: Some(PendingOrder {
        symbol: "BTCUSDT".into(),
        order_id: Some("12345".into()),
        client_order_id: "btc-grid-1".into(),
        side: Side::Buy,
        price: 90.0,
        quantity: 0.5,
        status: OrderStatus::New,
    }),
    ...
}
```

- [ ] **Step 2: 运行 TUI 视图测试，确认当前界面还没有展示挂单和服务端目标值**

Run: `cargo test -p grid-tui views::instance::tests::renders_instance_details_and_events -- --exact`
Expected: FAIL，原因是视图仍然只展示 `current exposure` 和客户端推导的 `target exposure`

- [ ] **Step 3: 用服务端快照替代客户端猜测**

在 `tui/src/protocol.rs` 保留：

```rust
pub fn target_exposure(&self) -> Option<f64> {
    self.target_exposure
        .or_else(|| self.last_price.map(|last_price| self.config.target_exposure(last_price)))
}
```

在 `tui/src/views/instance.rs` 改为：

```rust
Line::from(format!("actual exposure: {:.4}", snapshot.current_exposure)),
Line::from(format!(
    "target exposure: {}",
    snapshot
        .target_exposure()
        .map(|value| format!("{value:.4}"))
        .unwrap_or_else(|| "-".to_string())
)),
Line::from(format!(
    "pending order: {}",
    snapshot
        .pending_order
        .as_ref()
        .map(|order| format!("{} {} @ {:.4}", order.side, order.quantity, order.price))
        .unwrap_or_else(|| "none".to_string())
)),
```

- [ ] **Step 4: 跑 TUI 定向测试**

Run: `cargo test -p grid-tui views::instance::tests::renders_instance_details_and_events -- --exact`
Expected: PASS

Run: `cargo test -p grid-tui -- --nocapture`
Expected: PASS

- [x] **Step 5: 跑整仓验证命令**

Run: `cargo fmt --all --check`
Expected: PASS

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: PASS

Run: `cargo test --workspace`
Expected: PASS

- [x] **Step 6: 人工对照验收问题逐条勾销**

确认这几项都成立：

- `current_exposure` 不再被 `reconcile` 目标值直接覆盖
- `evaluate_risk` 会消费预算和风险状态
- `reconcile` 能产出真实订单动作
- server 会执行动作并通过 user stream / position sync 回写真实仓位
- runtime 不再依赖交易所原始订单状态字符串，统一消费 `OrderStatus`
- 提交/撤单会先持久化本地意图，再执行交易所副作用
- 快照变化但无领域事件时会广播 `SnapshotUpdated`，TUI 能刷新
- 配置和协议使用 snake_case
- 默认预算公式正确
- `--config` 缺失时启动失败且退出非零
- `cargo clippy --workspace --all-targets -- -D warnings` 通过

- [x] **Step 7: 同步任务清单**

把本计划文件顶部或尾部增加一个“验收记录”小节，记录：

- 验证命令
- 通过时间
- 仍留待后续探索的事项（如果有）

不要恢复旧的废弃文档或保留过渡说明。

- [ ] **Step 8: 提交**

```bash
git add tui/src/protocol.rs tui/src/views/instance.rs tui/src/app.rs tui/src/main.rs server/src/http.rs configs/test.toml docs/superpowers/plans/2026-03-24-grid-platform-acceptance-remediation.md
git commit -m "test: re-accept grid platform after execution loop fixes"
```

---

## 完成标准

满足以下条件才能宣称本轮修复完成：

1. `cargo fmt --all --check` 通过
2. `cargo clippy --workspace --all-targets -- -D warnings` 通过
3. `cargo test --workspace` 通过
4. 新增测试能证明：
   - tick 只更新目标仓位，不伪造真实仓位
   - 风控会 cap / flatten
   - `reconcile` 产出真实订单动作
   - user stream / position sync 会回写真实仓位和挂单状态
5. HTTP/TUI 快照里同时能看到 `current_exposure` 与 `target_exposure`
6. 本计划文件已补“验收记录”

## 验收记录

- 通过时间：2026-03-25
- 验证命令：
  - `cargo fmt --all --check`
  - `cargo clippy --workspace --all-targets -- -D warnings`
  - `cargo test --workspace`
- 本轮关闭的问题：
  - 交易所下单/撤单前先持久化 `Submitting` / `Canceling` 意图，避免交易所已产生副作用但本地回滚到旧快照
  - `runtime` 改为消费归一化后的 `OrderStatus`，不再匹配交易所原始状态字符串
  - `PositionUpdate`、终态 `OrderUpdate` 这类只改快照的路径会广播 `SnapshotUpdated`，TUI 能及时刷新
  - SQLite 只接受当前 schema 和当前 JSON 结构，删除旧列名、旧字段名、旧事件格式兼容
- 仍留待后续探索的事项：
  - 暂无新的验收阻塞项
