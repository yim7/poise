# Multi-Instance Fixed-Range Grid Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 为 `service` 增加“一环境一配置文件启动多标的固定区间网格实例”的能力，并让 `tui` 新增实例列表与实例切换，同时保持现有详情页的单实例交互模式。

**Architecture:** 在 `service` 中新增配置文件解析与多实例注册表，使用 `symbol` 作为实例作用域，把现有单实例 `Application` 包装为多个独立运行单元。策略侧引入固定区间梯子模型与等待态语义，控制面新增 `/instances` 与实例作用域路由；`tui` 先拉实例列表，再按选中 `symbol` 拉快照和建立单实例 WebSocket。

**Tech Stack:** Rust 2024、axum、tokio、serde、toml、rusqlite、reqwest、tokio-tungstenite、ratatui、crossterm、insta、cargo test

---

## 文件结构

### 新建文件

- `service/src/config.rs`
  - 定义多实例配置文件模型
  - 提供 TOML 解析与校验
  - 推导实例默认数据路径
- `service/src/registry.rs`
  - 管理多实例 `Application`
  - 暴露实例列表读模型
  - 提供按 `symbol` 查找与兼容默认实例别名
- `service/tests/multi_instance_control_plane.rs`
  - 覆盖 `/instances` 与实例作用域 HTTP/WS 路由
- `tui/tests/instance_switching.rs`
  - 覆盖实例列表加载、默认实例选择、切换时重拉快照与重建 ws
- `tui/src/snapshots/grid_platform_tui__render__tests__instance_picker_snapshot_100x16.snap`
- `tui/src/snapshots/grid_platform_tui__render__tests__dashboard_render_snapshot_waiting_range_entry_100x16.snap`
- `tui/src/snapshots/grid_platform_tui__render__tests__grid_render_snapshot_waiting_market_price_100x16.snap`

### 重点修改文件

- `service/src/lib.rs`
  - 导出 `config` 与 `registry` 模块
- `service/src/main.rs`
  - 增加 `--config` 启动入口
  - 在配置文件模式与现有单实例模式之间分流
- `service/src/application.rs`
  - 保持单实例边界
  - 增加供注册表组合使用的实例摘要读取入口
- `service/src/control_plane.rs`
  - 新增 `/instances`
  - 将现有接口扩展为 `/instances/{symbol}/...`
  - 为旧单实例路由保留 `default_symbol` 兼容别名
- `service/src/protocol.rs`
  - 将 `strategy.config` 改为用户视角范围配置
  - 增加实例列表响应模型
  - 扩展 `StrategyStatus` 与 `status_reason`
- `service/src/strategy.rs`
  - 以固定区间梯子替代中心对称层级生成
  - 增加 `waiting_market_price / waiting_range_entry / active / occupied` 状态逻辑
- `service/src/kernel.rs`
  - 按固定区间梯子驱动挂单、撤单与等待态切换
- `service/src/integrations/binance.rs`
  - 支持为每个实例启动各自 supervisor / transport 任务
- `service/tests/kernel_flow.rs`
  - 补固定区间、区间外等待、回到区间自动激活等回归
- `tui/src/protocol.rs`
  - 与服务端对齐新的范围配置与实例摘要协议
- `tui/src/events.rs`
  - 增加实例列表与切换动作
- `tui/src/input/mod.rs`
  - 增加实例列表快捷键映射
- `tui/src/state.rs`
  - 增加实例目录、当前实例、实例列表 UI 状态
- `tui/src/effects.rs`
  - 增加拉取实例列表与切换实例相关 effect
- `tui/src/runtime.rs`
  - 启动时先拉实例列表，再按默认实例启动
- `tui/src/transport/mod.rs`
  - 增加 `/instances` 与 `/instances/{symbol}/...` 请求封装
- `tui/src/store.rs`
  - 处理实例列表打开、选择实例、切换后重拉快照和重建 ws
- `tui/src/render.rs`
  - 渲染实例列表面板、等待态文案与当前 symbol 标识
- `tui/src/locale.rs`
  - 增加实例列表和等待态相关文案
- `tui/src/selectors.rs`
  - 为 header / grid / dashboard 暴露当前实例摘要与等待原因
- `README.md`
  - 更新配置文件启动与多实例运行说明
- `TODO.md`
  - 验收完成后同步任务清单

---

### Task 1: 建立配置文件模型与启动入口

**Files:**
- Create: `service/src/config.rs`
- Modify: `service/src/lib.rs`
- Modify: `service/src/main.rs`
- Test: `service/src/config.rs`
- Test: `service/tests/cli.rs`

- [ ] **Step 1: 先写失败测试，锁定配置文件解析与校验规则**

```rust
#[cfg(test)]
mod tests {
    use super::ServiceConfig;

    #[test]
    fn parses_valid_environment_file() {
        let raw = r#"
environment = "testnet"
default_symbol = "BTCUSDT"

[[instances]]
symbol = "BTCUSDT"

[instances.range]
lower_price = 90000
upper_price = 110000
grid_levels = 6
max_position_notional = 3000
"#;

        let config = ServiceConfig::from_toml_str(raw).expect("parse config");
        assert_eq!(config.environment, "testnet");
        assert_eq!(config.default_symbol.as_deref(), Some("BTCUSDT"));
        assert_eq!(config.instances.len(), 1);
    }

    #[test]
    fn rejects_duplicate_symbols_in_same_file() {
        let raw = r#"
environment = "testnet"

[[instances]]
symbol = "BTCUSDT"
[instances.range]
lower_price = 90000
upper_price = 110000
grid_levels = 6
max_position_notional = 3000

[[instances]]
symbol = "BTCUSDT"
[instances.range]
lower_price = 91000
upper_price = 111000
grid_levels = 6
max_position_notional = 3000
"#;

        let error = ServiceConfig::from_toml_str(raw).expect_err("duplicate symbol");
        assert!(error.to_string().contains("duplicate symbol"));
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p grid-platform-service config::tests --lib`
Expected: FAIL，提示 `config` 模块或 `ServiceConfig::from_toml_str` 不存在

- [ ] **Step 3: 最小实现配置文件模型与 `--config` 启动入口**

```rust
#[derive(Debug, Deserialize)]
pub struct ServiceConfig {
    pub environment: String,
    pub default_symbol: Option<String>,
    pub instances: Vec<InstanceConfig>,
}

#[derive(Debug, Deserialize)]
pub struct InstanceConfig {
    pub symbol: String,
    pub range: RangeConfig,
}

#[derive(Debug, Deserialize)]
pub struct RangeConfig {
    pub lower_price: f64,
    pub upper_price: f64,
    pub grid_levels: usize,
    pub max_position_notional: f64,
}

impl ServiceConfig {
    pub fn from_toml_str(raw: &str) -> anyhow::Result<Self> {
        let parsed: Self = toml::from_str(raw)?;
        parsed.validate()?;
        Ok(parsed)
    }
}
```

- [ ] **Step 4: 跑测试确认通过，并补 CLI 启动参数断言**

Run: `cargo test -p grid-platform-service config::tests --lib && cargo test -p grid-platform-service --test cli`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
git add service/src/config.rs service/src/lib.rs service/src/main.rs service/tests/cli.rs
git commit -m "feat: add multi-instance config loading"
```

### Task 2: 用测试锁定固定区间梯子协议与状态语义

**Files:**
- Modify: `service/src/protocol.rs`
- Modify: `tui/src/protocol.rs`
- Modify: `service/src/strategy.rs`
- Test: `service/src/strategy.rs`
- Test: `service/src/protocol.rs`
- Test: `tui/src/protocol.rs`

- [ ] **Step 1: 先写失败测试，锁定固定区间梯子价格和等待态**

```rust
#[test]
fn builds_range_ladder_with_inclusive_boundaries() {
    let config = RangeGridConfig {
        lower_price: 90.0,
        upper_price: 110.0,
        grid_levels: 6,
        max_position_notional: 3000.0,
    };

    let levels = build_levels(100.0, &config, 0.0);
    let prices: Vec<f64> = levels.iter().map(|level| level.price).collect();
    assert_eq!(prices, vec![90.0, 94.0, 98.0, 102.0, 106.0, 110.0]);
}

#[test]
fn stays_waiting_when_price_is_out_of_range_and_flat() {
    let strategy = reconcile_range_grid(None, 112.0, 0.0, &config());
    assert_eq!(strategy.status, StrategyStatus::WaitingRangeEntry);
    assert!(strategy.status_reason.as_deref().is_some_and(|reason| reason.contains("112.0")));
}

#[test]
fn keeps_occupied_when_price_is_out_of_range_but_inventory_exists() {
    let strategy = reconcile_range_grid(None, 112.0, 0.6, &config());
    assert_eq!(strategy.status, StrategyStatus::Occupied);
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p grid-platform-service strategy::tests --lib`
Expected: FAIL，提示 `RangeGridConfig`、新状态或固定区间函数不存在

- [ ] **Step 3: 最小实现范围配置协议与梯子状态**

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RangeGridConfig {
    pub lower_price: f64,
    pub upper_price: f64,
    pub grid_levels: usize,
    pub max_position_notional: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StrategyStatus {
    WaitingMarketPrice,
    WaitingRangeEntry,
    Active,
    Occupied,
}
```

- [ ] **Step 4: 跑协议与策略测试确认通过**

Run: `cargo test -p grid-platform-service protocol::tests --lib && cargo test -p grid-platform-service strategy::tests --lib && cargo test -p grid-platform-tui protocol::tests --lib`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
git add service/src/protocol.rs tui/src/protocol.rs service/src/strategy.rs
git commit -m "feat: add fixed-range ladder protocol"
```

### Task 3: 在内核中接入固定区间等待态与自动激活

**Files:**
- Modify: `service/src/kernel.rs`
- Modify: `service/src/risk.rs`
- Modify: `service/src/strategy.rs`
- Test: `service/tests/kernel_flow.rs`

- [ ] **Step 1: 先写失败测试，锁定区间外等待与回到区间自动激活**

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn out_of_range_start_keeps_instance_waiting_without_orders() -> Result<()> {
    let runtime = bootstrap_runtime_with_range("BTCUSDT", 90.0, 110.0, 6, 3000.0, 120.0);
    let (engine, read_model, adapter) = spawn_range_engine(runtime).await?;

    tokio::time::sleep(Duration::from_millis(50)).await;

    let snapshot = read_model.read().expect("read model").snapshot();
    assert_eq!(snapshot.strategy.status, StrategyStatus::WaitingRangeEntry);
    assert!(snapshot.execution.open_orders.is_empty());
    assert!(adapter.submit_calls().is_empty());
    drop(engine);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn price_reentering_range_places_orders_automatically() -> Result<()> {
    let runtime = bootstrap_runtime_with_range("BTCUSDT", 90.0, 110.0, 6, 3000.0, 112.0);
    let (engine, read_model, adapter) = spawn_range_engine(runtime).await?;

    engine.apply_market_price(106.0).await?;

    let snapshot = read_model.read().expect("read model").snapshot();
    assert_eq!(snapshot.strategy.status, StrategyStatus::Active);
    assert!(!snapshot.execution.open_orders.is_empty());
    assert!(!adapter.submit_calls().is_empty());
    drop(engine);
    Ok(())
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p grid-platform-service --test kernel_flow out_of_range_start_keeps_instance_waiting_without_orders && cargo test -p grid-platform-service --test kernel_flow price_reentering_range_places_orders_automatically`
Expected: FAIL，提示等待态或自动激活逻辑未实现

- [ ] **Step 3: 最小实现内核等待态切换与策略挂单清理**

```rust
if strategy.status == StrategyStatus::WaitingRangeEntry && runtime.position_qty.abs() <= EPSILON {
    cancel_strategy_orders_for_symbol(symbol).await?;
    snapshot.execution.open_orders.clear();
    return Ok(());
}

if entered_range && snapshot.runtime.strategy_state == "running" {
    sync_strategy_orders(&snapshot.strategy).await?;
}
```

- [ ] **Step 4: 跑内核回归确认通过**

Run: `cargo test -p grid-platform-service --test kernel_flow`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
git add service/src/kernel.rs service/src/risk.rs service/src/strategy.rs service/tests/kernel_flow.rs
git commit -m "feat: add fixed-range waiting lifecycle"
```

### Task 4: 引入多实例注册表与实例作用域控制面

**Files:**
- Create: `service/src/registry.rs`
- Modify: `service/src/application.rs`
- Modify: `service/src/control_plane.rs`
- Modify: `service/src/main.rs`
- Modify: `service/src/lib.rs`
- Test: `service/tests/multi_instance_control_plane.rs`
- Test: `service/tests/control_plane.rs`

- [ ] **Step 1: 先写失败测试，锁定 `/instances` 与实例作用域快照**

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn instances_endpoint_lists_symbols_from_config() -> Result<()> {
    let app = bootstrap_multi_instance_app(["BTCUSDT", "ETHUSDT"])?;
    let response = app.get("/instances").await?;

    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.text().await?.contains("BTCUSDT"));
    assert!(response.text().await?.contains("ETHUSDT"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn instance_scoped_snapshot_returns_target_symbol_only() -> Result<()> {
    let app = bootstrap_multi_instance_app(["BTCUSDT", "ETHUSDT"])?;
    let body = app.get_json("/instances/ETHUSDT/runtime/snapshot").await?;

    assert_eq!(body["data"]["runtime"]["symbol"], "ETHUSDT");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_runtime_snapshot_alias_uses_default_symbol() -> Result<()> {
    let app = bootstrap_multi_instance_app(["BTCUSDT", "ETHUSDT"])?;
    let body = app.get_json("/runtime/snapshot").await?;

    assert_eq!(body["data"]["runtime"]["symbol"], "BTCUSDT");
    Ok(())
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p grid-platform-service --test multi_instance_control_plane`
Expected: FAIL，提示 `/instances` 或实例作用域路由不存在

- [ ] **Step 3: 最小实现注册表与实例作用域路由**

```rust
pub struct ApplicationRegistry {
    environment: String,
    default_symbol: String,
    instances: BTreeMap<String, Application>,
}

impl ApplicationRegistry {
    pub fn instance(&self, symbol: &str) -> Option<&Application> {
        self.instances.get(symbol)
    }
}
```

- [ ] **Step 4: 跑控制面回归确认通过**

Run: `cargo test -p grid-platform-service --test multi_instance_control_plane && cargo test -p grid-platform-service --test control_plane`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
git add service/src/registry.rs service/src/application.rs service/src/control_plane.rs service/src/main.rs service/src/lib.rs service/tests/multi_instance_control_plane.rs service/tests/control_plane.rs
git commit -m "feat: add multi-instance control plane"
```

### Task 5: 让 TUI 先拉实例列表，再按 symbol 建立单实例连接

**Files:**
- Modify: `tui/src/protocol.rs`
- Modify: `tui/src/events.rs`
- Modify: `tui/src/effects.rs`
- Modify: `tui/src/runtime.rs`
- Modify: `tui/src/state.rs`
- Modify: `tui/src/store.rs`
- Modify: `tui/src/transport/mod.rs`
- Test: `tui/tests/instance_switching.rs`
- Test: `tui/src/store.rs`

- [ ] **Step 1: 先写失败测试，锁定默认实例选择与切换流程**

```rust
#[tokio::test]
async fn app_bootstraps_from_instances_then_default_symbol_snapshot() {
    let server = spawn_fixture_server()
        .with_instances(["BTCUSDT", "ETHUSDT"], Some("ETHUSDT"))
        .with_snapshot("ETHUSDT");

    let events = drive_runtime_until_snapshot(server).await;

    assert!(events.contains(&"fetch_instances".to_string()));
    assert!(events.contains(&"fetch_snapshot:ETHUSDT".to_string()));
}

#[test]
fn selecting_instance_marks_transport_for_reconnect() {
    let mut state = AppState::sample();
    reduce(&mut state, AppEvent::LocalUi(LocalUiEvent::SelectInstance("ETHUSDT".into())));
    assert_eq!(state.ui.current_symbol.as_deref(), Some("ETHUSDT"));
    assert!(state.connection.ws_connected == false);
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p grid-platform-tui --test instance_switching && cargo test -p grid-platform-tui store::tests --lib`
Expected: FAIL，提示实例目录协议、选择动作或 transport 接口不存在

- [ ] **Step 3: 最小实现实例目录协议与 symbol 作用域 transport**

```rust
pub async fn fetch_instances(&self) -> Result<InstancesResponse> {
    self.get_json(format!("{}/instances", self.base_url)).await
}

pub async fn fetch_snapshot(&self, symbol: &str) -> Result<RuntimeSnapshot> {
    self.get_json(format!("{}/instances/{symbol}/runtime/snapshot", self.base_url)).await
}

pub fn scoped_ws_url(&self, symbol: &str) -> String {
    format!("{}/instances/{symbol}/ws", self.ws_url_base)
}
```

- [ ] **Step 4: 跑 TUI 运行态与切换测试确认通过**

Run: `cargo test -p grid-platform-tui --test instance_switching && cargo test -p grid-platform-tui store::tests --lib && cargo test -p grid-platform-tui runtime::tests --lib`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
git add tui/src/protocol.rs tui/src/events.rs tui/src/effects.rs tui/src/runtime.rs tui/src/state.rs tui/src/store.rs tui/src/transport/mod.rs tui/tests/instance_switching.rs
git commit -m "feat: bootstrap tui from instances list"
```

### Task 6: 加入实例列表 UI、等待态文案与全量回归

**Files:**
- Modify: `tui/src/input/mod.rs`
- Modify: `tui/src/render.rs`
- Modify: `tui/src/selectors.rs`
- Modify: `tui/src/locale.rs`
- Modify: `README.md`
- Modify: `TODO.md`
- Create: `tui/src/snapshots/grid_platform_tui__render__tests__instance_picker_snapshot_100x16.snap`
- Create: `tui/src/snapshots/grid_platform_tui__render__tests__dashboard_render_snapshot_waiting_range_entry_100x16.snap`
- Create: `tui/src/snapshots/grid_platform_tui__render__tests__grid_render_snapshot_waiting_market_price_100x16.snap`
- Test: `tui/src/render.rs`
- Test: `tui/tests/local_paper_e2e.rs`

- [ ] **Step 1: 先写失败测试，锁定实例列表 UI 与等待态渲染**

```rust
#[test]
fn plain_i_opens_instance_picker() {
    let action = map_key_event(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE));
    assert_eq!(action, Some(KeyAction::ToggleInstances));
}

#[test]
fn waiting_range_entry_is_rendered_in_dashboard() {
    let rendered = normalized_page_string(Page::Dashboard, 100, 16, |state| {
        state.runtime.symbol = "BTCUSDT".into();
        state.strategy.status = StrategyStatus::WaitingRangeEntry;
        state.strategy.status_reason = Some("当前价格高于上边界".into());
    });
    assert!(rendered.contains("等待进入区间"));
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p grid-platform-tui input::tests --lib && cargo test -p grid-platform-tui render::tests --lib`
Expected: FAIL，提示实例列表动作、等待态文案或快照不存在

- [ ] **Step 3: 最小实现实例列表交互与渲染**

```rust
match event.code {
    KeyCode::Char('i') => Some(KeyAction::ToggleInstances),
    // ...
}

if state.ui.instances_panel_open {
    render_instance_picker(frame, state, theme);
}
```

- [ ] **Step 4: 跑快照、README 与全量测试确认通过**

Run: `cargo test -p grid-platform-tui render::tests --lib && cargo test -p grid-platform-tui --test local_paper_e2e && cargo test`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
git add tui/src/input/mod.rs tui/src/render.rs tui/src/selectors.rs tui/src/locale.rs tui/src/snapshots README.md TODO.md
git commit -m "feat: add tui instance switching ui"
```
