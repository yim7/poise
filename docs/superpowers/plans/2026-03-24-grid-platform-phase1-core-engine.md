# 网格平台第一阶段实现计划：grid-core + grid-engine

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 搭建网格平台的领域核心和引擎编排层，形成可嵌入的策略计算库。

**Architecture:** 六边形架构，grid-core 是纯函数库（无 async/IO），grid-engine 定义端口 trait 并编排策略→风控→执行。详见 [架构设计 spec](../specs/2026-03-24-grid-platform-architecture-design.md) 和 [策略族设计 spec](../specs/2026-03-24-grid-strategy-family-design.md)。

**Tech Stack:** Rust 2024 edition, serde (core only), tokio + async-trait (engine only)

**Scope:** 本计划只覆盖 grid-core 和 grid-engine。适配器（binance/storage）、服务端（server）和客户端（tui）各自有独立的后续计划。

---

## File Structure

### 新建文件

```
core/
├── Cargo.toml
└── src/
    ├── lib.rs
    ├── types.rs
    ├── strategy.rs
    ├── risk.rs
    └── events.rs

engine/
├── Cargo.toml
└── src/
    ├── lib.rs
    ├── ports.rs
    ├── instance.rs
    ├── reconciler.rs
    ├── execution_plan.rs
    └── manager.rs
```

### 修改文件

- `Cargo.toml`（workspace 根）：重建 workspace members

---

### Task 1: 初始化 Workspace

**Files:**
- Modify: `Cargo.toml`
- Create: `core/Cargo.toml`
- Create: `core/src/lib.rs`
- Create: `engine/Cargo.toml`
- Create: `engine/src/lib.rs`

- [ ] **Step 1: 创建 workspace 根 Cargo.toml**

```toml
[workspace]
members = ["core", "engine"]
resolver = "3"

[workspace.package]
version = "0.1.0"
edition = "2024"

[workspace.dependencies]
serde = { version = "1", features = ["derive"] }
tokio = { version = "1", features = ["macros", "rt-multi-thread", "sync", "time"] }
async-trait = "0.1"
anyhow = "1"
chrono = { version = "0.4", features = ["serde"] }
```

- [ ] **Step 2: 创建 core/Cargo.toml**

```toml
[package]
name = "grid-core"
version.workspace = true
edition.workspace = true

[dependencies]
serde.workspace = true
```

注意：grid-core 不允许 tokio/async-trait/reqwest 等 IO 依赖。

- [ ] **Step 3: 创建 core/src/lib.rs**

```rust
pub mod types;
pub mod strategy;
pub mod risk;
pub mod events;
```

- [ ] **Step 4: 创建 engine/Cargo.toml**

```toml
[package]
name = "grid-engine"
version.workspace = true
edition.workspace = true

[dependencies]
grid-core = { path = "../core" }
serde.workspace = true
tokio.workspace = true
async-trait.workspace = true
anyhow.workspace = true
chrono.workspace = true
```

- [ ] **Step 5: 创建 engine/src/lib.rs**

```rust
pub mod ports;
pub mod instance;
pub mod reconciler;
pub mod execution_plan;
pub mod manager;
```

- [ ] **Step 6: 创建占位模块文件，确保编译通过**

为 core 和 engine 的每个子模块创建空文件。

- [ ] **Step 7: 验证编译**

Run: `cargo check`
Expected: 编译成功，无错误

- [ ] **Step 8: 提交**

```bash
git add -A && git commit -m "feat: initialize grid-core and grid-engine workspace"
```

---

### Task 2: grid-core 领域类型

**Files:**
- Create: `core/src/types.rs`

- [ ] **Step 1: 写测试**

在 `core/src/types.rs` 底部：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposure_arithmetic() {
        let a = Exposure(3.0);
        let b = Exposure(5.0);
        assert!((a.delta(&b).0 - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn side_from_exposure() {
        assert_eq!(Side::from_exposure(&Exposure(1.0)), Some(Side::Buy));
        assert_eq!(Side::from_exposure(&Exposure(-1.0)), Some(Side::Sell));
        assert_eq!(Side::from_exposure(&Exposure(0.0)), None);
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p grid-core -- types`
Expected: FAIL（类型未定义）

- [ ] **Step 3: 实现类型**

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Exposure(pub f64);

impl Exposure {
    pub fn delta(&self, target: &Exposure) -> Exposure {
        Exposure(target.0 - self.0)
    }

    pub fn is_zero(&self) -> bool {
        self.0.abs() < f64::EPSILON
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    pub fn from_exposure(exposure: &Exposure) -> Option<Side> {
        if exposure.0 > f64::EPSILON {
            Some(Side::Buy)
        } else if exposure.0 < -f64::EPSILON {
            Some(Side::Sell)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExchangeRules {
    pub price_tick: f64,
    pub quantity_step: f64,
    pub min_qty: f64,
    pub min_notional: f64,
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p grid-core -- types`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
git add -A && git commit -m "feat(core): add domain types - Exposure, Side, ExchangeRules"
```

---

### Task 3: grid-core 策略模型

**Files:**
- Create: `core/src/strategy.rs`

- [ ] **Step 1: 写测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn neutral_config() -> GridConfig {
        GridConfig {
            lower_price: 90.0,
            upper_price: 110.0,
            long_capacity: 8.0,
            short_capacity: 8.0,
            capacity_notional: 375.0,
            shape_family: ShapeFamily::Linear,
            out_of_band_policy: OutOfBandPolicy::Freeze,
        }
    }

    fn long_only_config() -> GridConfig {
        GridConfig {
            long_capacity: 8.0,
            short_capacity: 0.0,
            ..neutral_config()
        }
    }

    #[test]
    fn validate_rejects_inverted_prices() {
        let config = GridConfig { lower_price: 110.0, upper_price: 90.0, ..neutral_config() };
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn validate_rejects_negative_capacity() {
        let config = GridConfig { long_capacity: -1.0, ..neutral_config() };
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn validate_rejects_both_zero_capacity() {
        let config = GridConfig { long_capacity: 0.0, short_capacity: 0.0, ..neutral_config() };
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn validate_accepts_valid_config() {
        assert!(validate_config(&neutral_config()).is_ok());
        assert!(validate_config(&long_only_config()).is_ok());
    }

    #[test]
    fn target_exposure_neutral_at_center() {
        let exposure = target_exposure(100.0, &neutral_config());
        assert!((exposure.0).abs() < 0.001);
    }

    #[test]
    fn target_exposure_full_long_at_lower() {
        let exposure = target_exposure(90.0, &neutral_config());
        assert!((exposure.0 - 8.0).abs() < 0.001);
    }

    #[test]
    fn target_exposure_full_short_at_upper() {
        let exposure = target_exposure(110.0, &neutral_config());
        assert!((exposure.0 + 8.0).abs() < 0.001);
    }

    #[test]
    fn target_exposure_long_only_zero_at_upper() {
        let exposure = target_exposure(110.0, &long_only_config());
        assert!((exposure.0).abs() < 0.001);
    }

    #[test]
    fn target_exposure_long_only_half_at_center() {
        let exposure = target_exposure(100.0, &long_only_config());
        assert!((exposure.0 - 4.0).abs() < 0.001);
    }

    #[test]
    fn band_status_in_band() {
        let status = band_status(100.0, &neutral_config());
        assert!(matches!(status, BandStatus::InBand { .. }));
    }

    #[test]
    fn band_status_below() {
        let status = band_status(85.0, &neutral_config());
        assert!(matches!(status, BandStatus::OutOfBand { boundary: BandBoundary::Below, .. }));
    }

    #[test]
    fn band_status_above() {
        let status = band_status(115.0, &neutral_config());
        assert!(matches!(status, BandStatus::OutOfBand { boundary: BandBoundary::Above, .. }));
    }

    #[test]
    fn convex_shape_slower_departure() {
        let config = GridConfig { shape_family: ShapeFamily::Convex, ..neutral_config() };
        let linear_mid = target_exposure(95.0, &neutral_config());
        let convex_mid = target_exposure(95.0, &config);
        // convex 在接近边缘时离开得更慢，所以 95 处 convex 的多头占用 > linear
        assert!(convex_mid.0 > linear_mid.0);
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p grid-core -- strategy`
Expected: FAIL

- [ ] **Step 3: 实现策略模型**

```rust
use serde::{Deserialize, Serialize};
use crate::types::Exposure;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GridConfig {
    pub lower_price: f64,
    pub upper_price: f64,
    pub long_capacity: f64,
    pub short_capacity: f64,
    pub capacity_notional: f64,
    pub shape_family: ShapeFamily,
    pub out_of_band_policy: OutOfBandPolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ShapeFamily { Linear, Convex, Concave }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutOfBandPolicy { Freeze, ReduceOnly, Terminate, Hold }

#[derive(Debug, Clone, PartialEq)]
pub enum BandStatus {
    InBand { target: Exposure },
    OutOfBand { policy: OutOfBandPolicy, boundary: BandBoundary },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BandBoundary { Below, Above }

pub fn validate_config(config: &GridConfig) -> Result<(), String> {
    if config.lower_price >= config.upper_price {
        return Err("lower_price must be less than upper_price".into());
    }
    if config.long_capacity < 0.0 || config.short_capacity < 0.0 {
        return Err("capacities must not be negative".into());
    }
    if config.long_capacity + config.short_capacity <= f64::EPSILON {
        return Err("at least one capacity must be positive".into());
    }
    if config.capacity_notional <= 0.0 {
        return Err("capacity_notional must be positive".into());
    }
    Ok(())
}

pub fn target_exposure(price: f64, config: &GridConfig) -> Exposure {
    let x = ((price - config.lower_price) / (config.upper_price - config.lower_price))
        .clamp(0.0, 1.0);
    let g = match config.shape_family {
        ShapeFamily::Linear => 1.0 - x,
        ShapeFamily::Convex => 1.0 - x.powi(2),
        ShapeFamily::Concave => 1.0 - x.sqrt(),
    };
    Exposure(-config.short_capacity + (config.long_capacity + config.short_capacity) * g)
}

pub fn band_status(price: f64, config: &GridConfig) -> BandStatus {
    if price < config.lower_price - f64::EPSILON {
        BandStatus::OutOfBand {
            policy: config.out_of_band_policy,
            boundary: BandBoundary::Below,
        }
    } else if price > config.upper_price + f64::EPSILON {
        BandStatus::OutOfBand {
            policy: config.out_of_band_policy,
            boundary: BandBoundary::Above,
        }
    } else {
        BandStatus::InBand {
            target: target_exposure(price, config),
        }
    }
}

impl GridConfig {
    pub fn band_center(&self) -> f64 {
        (self.lower_price + self.upper_price) / 2.0
    }

    pub fn capacity_unit_qty(&self) -> f64 {
        let center = self.band_center();
        if center <= f64::EPSILON { 0.0 } else { self.capacity_notional / center }
    }
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p grid-core -- strategy`
Expected: 全部 PASS

- [ ] **Step 5: 提交**

```bash
git add -A && git commit -m "feat(core): add strategy model - GridConfig, target_exposure, band_status"
```

---

### Task 4: grid-core 风控规则

**Files:**
- Create: `core/src/risk.rs`

- [ ] **Step 1: 写测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn budget() -> CapacityBudget {
        CapacityBudget {
            max_notional: 3000.0,
            daily_loss_limit: -120.0,
            stop_loss_pct: 4.0,
        }
    }

    #[test]
    fn allow_when_within_budget() {
        let intent = ExposureIntent {
            current: Exposure(0.0),
            target: Exposure(4.0),
        };
        let decision = evaluate_risk(&intent, &budget());
        assert!(matches!(decision, RiskDecision::Allow(_)));
    }

    #[test]
    fn allow_when_reducing_exposure() {
        let intent = ExposureIntent {
            current: Exposure(8.0),
            target: Exposure(4.0),
        };
        let decision = evaluate_risk(&intent, &budget());
        assert!(matches!(decision, RiskDecision::Allow(_)));
    }

    #[test]
    fn allow_no_change() {
        let intent = ExposureIntent {
            current: Exposure(4.0),
            target: Exposure(4.0),
        };
        let decision = evaluate_risk(&intent, &budget());
        assert!(matches!(decision, RiskDecision::Allow(_)));
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p grid-core -- risk`
Expected: FAIL

- [ ] **Step 3: 实现风控规则**

```rust
use serde::{Deserialize, Serialize};
use crate::types::Exposure;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapacityBudget {
    pub max_notional: f64,
    pub daily_loss_limit: f64,
    pub stop_loss_pct: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExposureIntent {
    pub current: Exposure,
    pub target: Exposure,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RiskDecision {
    Allow(Exposure),
    Cap(Exposure),
    Deny { reason: String },
}

pub fn evaluate_risk(intent: &ExposureIntent, _budget: &CapacityBudget) -> RiskDecision {
    // 减仓或不变总是允许
    if intent.target.0.abs() <= intent.current.0.abs() {
        return RiskDecision::Allow(intent.target.clone());
    }
    // 第一版：在预算范围内直接允许
    RiskDecision::Allow(intent.target.clone())
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p grid-core -- risk`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
git add -A && git commit -m "feat(core): add risk model - CapacityBudget, evaluate_risk"
```

---

### Task 5: grid-core 领域事件

**Files:**
- Create: `core/src/events.rs`

- [ ] **Step 1: 实现领域事件类型**

```rust
use serde::{Deserialize, Serialize};
use crate::types::Exposure;
use crate::strategy::{BandBoundary, OutOfBandPolicy};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum DomainEvent {
    ExposureTargetChanged { from: Exposure, to: Exposure },
    BandBreached { boundary: BandBoundary, price: f64 },
    BandReentered { price: f64 },
    PolicyTriggered { policy: OutOfBandPolicy },
    RiskCapApplied { intended: Exposure, capped: Exposure },
    RiskDenied { reason: String },
}
```

（事件类型是值类型定义，不需要独立测试，后续在 reconciler 测试中覆盖。）

- [ ] **Step 2: 验证 grid-core 全部测试通过**

Run: `cargo test -p grid-core`
Expected: 全部 PASS

- [ ] **Step 3: 提交**

```bash
git add -A && git commit -m "feat(core): add domain events"
```

---

### Task 6: grid-engine 端口 trait 定义

**Files:**
- Create: `engine/src/ports.rs`

- [ ] **Step 1: 定义端口 trait**

```rust
use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrderRequest {
    pub symbol: String,
    pub side: grid_core::types::Side,
    pub price: f64,
    pub quantity: f64,
    pub client_order_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrderReceipt {
    pub order_id: String,
    pub client_order_id: String,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Position {
    pub symbol: String,
    pub qty: f64,
    pub avg_price: f64,
    pub unrealized_pnl: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpenOrder {
    pub order_id: String,
    pub client_order_id: String,
    pub side: grid_core::types::Side,
    pub price: f64,
    pub qty: f64,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PriceTick {
    pub symbol: String,
    pub last_price: f64,
    pub mark_price: f64,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExchangeInfo {
    pub symbol: String,
    pub rules: grid_core::types::ExchangeRules,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum UserDataEvent {
    OrderUpdate(OpenOrder),
    PositionUpdate(Position),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstanceSnapshot {
    pub id: String,
    pub symbol: String,
    pub config: grid_core::strategy::GridConfig,
    pub status: super::instance::InstanceStatus,
    pub current_exposure: grid_core::types::Exposure,
    pub last_price: Option<f64>,
}

#[async_trait]
pub trait ExchangePort: Send + Sync {
    async fn submit_order(&self, req: OrderRequest) -> Result<OrderReceipt>;
    async fn cancel_order(&self, symbol: &str, order_id: &str) -> Result<()>;
    async fn cancel_all(&self, symbol: &str) -> Result<Vec<String>>;
    async fn get_position(&self, symbol: &str) -> Result<Position>;
    async fn get_open_orders(&self, symbol: &str) -> Result<Vec<OpenOrder>>;
    async fn get_exchange_info(&self, symbol: &str) -> Result<ExchangeInfo>;
}

#[async_trait]
pub trait MarketDataPort: Send + Sync {
    async fn subscribe_prices(&self, symbol: &str) -> Result<mpsc::Receiver<PriceTick>>;
    async fn subscribe_user_data(&self) -> Result<mpsc::Receiver<UserDataEvent>>;
}

#[async_trait]
pub trait PersistencePort: Send + Sync {
    async fn save_instance_state(&self, id: &str, state: &InstanceSnapshot) -> Result<()>;
    async fn load_instance_state(&self, id: &str) -> Result<Option<InstanceSnapshot>>;
}

pub trait ClockPort: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}
```

- [ ] **Step 2: 验证编译通过**

Run: `cargo check -p grid-engine`
Expected: 编译成功

- [ ] **Step 3: 提交**

```bash
git add -A && git commit -m "feat(engine): define port traits - Exchange, MarketData, Persistence, Clock"
```

---

### Task 7: grid-engine 实例模型

**Files:**
- Create: `engine/src/instance.rs`

- [ ] **Step 1: 实现实例模型**

```rust
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use grid_core::strategy::GridConfig;
use grid_core::types::Exposure;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum InstanceStatus {
    WaitingMarketData,
    Active,
    Frozen,
    ReducingOnly,
    Holding,
    Terminated,
    Paused,
}

#[derive(Debug, Clone)]
pub struct StrategyInstance {
    pub id: String,
    pub symbol: String,
    pub config: GridConfig,
    pub status: InstanceStatus,
    pub current_exposure: Exposure,
    pub last_price: Option<f64>,
    pub out_of_band_since: Option<DateTime<Utc>>,
}

impl StrategyInstance {
    pub fn new(id: String, symbol: String, config: GridConfig) -> Self {
        Self {
            id,
            symbol,
            config,
            status: InstanceStatus::WaitingMarketData,
            current_exposure: Exposure(0.0),
            last_price: None,
            out_of_band_since: None,
        }
    }
}
```

- [ ] **Step 2: 验证编译通过**

Run: `cargo check -p grid-engine`
Expected: 编译成功

- [ ] **Step 3: 提交**

```bash
git add -A && git commit -m "feat(engine): add StrategyInstance model"
```

---

### Task 8: grid-engine 执行计划类型

**Files:**
- Create: `engine/src/execution_plan.rs`

- [ ] **Step 1: 实现执行计划类型**

```rust
use grid_core::events::DomainEvent;
use crate::ports::OrderRequest;

#[derive(Debug, Clone)]
pub struct ExecutionPlan {
    pub actions: Vec<ExecutionAction>,
    pub events: Vec<DomainEvent>,
}

#[derive(Debug, Clone)]
pub enum ExecutionAction {
    SubmitOrder(OrderRequest),
    CancelOrder { order_id: String },
    CancelAll,
    NoOp,
}

impl ExecutionPlan {
    pub fn noop() -> Self {
        Self { actions: vec![ExecutionAction::NoOp], events: vec![] }
    }

    pub fn hold(reason: String) -> Self {
        Self {
            actions: vec![ExecutionAction::NoOp],
            events: vec![DomainEvent::RiskDenied { reason }],
        }
    }

    pub fn has_actions(&self) -> bool {
        self.actions.iter().any(|a| !matches!(a, ExecutionAction::NoOp))
    }
}
```

- [ ] **Step 2: 验证编译通过**

Run: `cargo check -p grid-engine`
Expected: 编译成功

- [ ] **Step 3: 提交**

```bash
git add -A && git commit -m "feat(engine): add ExecutionPlan and ExecutionAction types"
```

---

### Task 9: grid-engine 协调器

**Files:**
- Create: `engine/src/reconciler.rs`

- [ ] **Step 1: 写测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use grid_core::strategy::*;
    use grid_core::risk::CapacityBudget;
    use grid_core::types::Exposure;
    use crate::instance::{StrategyInstance, InstanceStatus};

    fn test_instance() -> StrategyInstance {
        StrategyInstance::new(
            "test".into(),
            "BTCUSDT".into(),
            GridConfig {
                lower_price: 90.0,
                upper_price: 110.0,
                long_capacity: 8.0,
                short_capacity: 8.0,
                capacity_notional: 375.0,
                shape_family: ShapeFamily::Linear,
                out_of_band_policy: OutOfBandPolicy::Freeze,
            },
        )
    }

    fn test_budget() -> CapacityBudget {
        CapacityBudget {
            max_notional: 3000.0,
            daily_loss_limit: -120.0,
            stop_loss_pct: 4.0,
        }
    }

    #[test]
    fn reconcile_produces_noop_when_exposure_unchanged() {
        let mut instance = test_instance();
        instance.status = InstanceStatus::Active;
        instance.current_exposure = Exposure(0.0);
        instance.last_price = Some(100.0);

        // 价格在中心，目标 exposure = 0，当前也是 0
        let result = reconcile(&instance, 100.0, &test_budget());
        assert!(!result.plan.has_actions());
    }

    #[test]
    fn reconcile_produces_action_when_exposure_changes() {
        let mut instance = test_instance();
        instance.status = InstanceStatus::Active;
        instance.current_exposure = Exposure(0.0);

        // 价格在下沿，目标 exposure = 8.0，当前 0
        let result = reconcile(&instance, 90.0, &test_budget());
        assert!(result.plan.has_actions());
        assert!((result.target_exposure.0 - 8.0).abs() < 0.001);
    }

    #[test]
    fn reconcile_freezes_when_out_of_band() {
        let mut instance = test_instance();
        instance.status = InstanceStatus::Active;
        instance.current_exposure = Exposure(8.0);

        let result = reconcile(&instance, 85.0, &test_budget());
        assert_eq!(result.new_status, Some(InstanceStatus::Frozen));
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p grid-engine -- reconciler`
Expected: FAIL

- [ ] **Step 3: 实现协调器**

```rust
use grid_core::strategy::{self, BandStatus, OutOfBandPolicy};
use grid_core::risk::{self, CapacityBudget, ExposureIntent, RiskDecision};
use grid_core::types::Exposure;
use grid_core::events::DomainEvent;
use crate::instance::{StrategyInstance, InstanceStatus};
use crate::execution_plan::{ExecutionPlan, ExecutionAction};
use crate::ports::OrderRequest;

pub struct ReconcileResult {
    pub plan: ExecutionPlan,
    pub target_exposure: Exposure,
    pub new_status: Option<InstanceStatus>,
}

pub fn reconcile(
    instance: &StrategyInstance,
    price: f64,
    budget: &CapacityBudget,
) -> ReconcileResult {
    let band = strategy::band_status(price, &instance.config);

    let (target, new_status) = match band {
        BandStatus::InBand { target } => (target, resolve_in_band_status(instance)),
        BandStatus::OutOfBand { policy, .. } => {
            apply_out_of_band(instance, policy)
        }
    };

    let intent = ExposureIntent {
        current: instance.current_exposure.clone(),
        target: target.clone(),
    };

    let decision = risk::evaluate_risk(&intent, budget);

    let (approved_target, mut events) = match decision {
        RiskDecision::Allow(t) => (t, vec![]),
        RiskDecision::Cap(t) => {
            let events = vec![DomainEvent::RiskCapApplied {
                intended: target.clone(),
                capped: t.clone(),
            }];
            (t, events)
        }
        RiskDecision::Deny { reason } => {
            return ReconcileResult {
                plan: ExecutionPlan::hold(reason),
                target_exposure: instance.current_exposure.clone(),
                new_status: None,
            };
        }
    };

    let delta = instance.current_exposure.delta(&approved_target);
    if delta.is_zero() {
        return ReconcileResult {
            plan: ExecutionPlan::noop(),
            target_exposure: approved_target,
            new_status,
        };
    }

    events.push(DomainEvent::ExposureTargetChanged {
        from: instance.current_exposure.clone(),
        to: approved_target.clone(),
    });

    let plan = ExecutionPlan {
        actions: vec![ExecutionAction::NoOp], // 具体订单生成留给后续完善
        events,
    };

    ReconcileResult {
        plan,
        target_exposure: approved_target,
        new_status,
    }
}

fn resolve_in_band_status(instance: &StrategyInstance) -> Option<InstanceStatus> {
    match instance.status {
        InstanceStatus::WaitingMarketData => Some(InstanceStatus::Active),
        InstanceStatus::Frozen | InstanceStatus::Holding => Some(InstanceStatus::Active),
        _ => None,
    }
}

fn apply_out_of_band(
    instance: &StrategyInstance,
    policy: OutOfBandPolicy,
) -> (Exposure, Option<InstanceStatus>) {
    match policy {
        OutOfBandPolicy::Freeze => {
            (instance.current_exposure.clone(), Some(InstanceStatus::Frozen))
        }
        OutOfBandPolicy::Hold => {
            (instance.current_exposure.clone(), Some(InstanceStatus::Holding))
        }
        OutOfBandPolicy::ReduceOnly => {
            (Exposure(0.0), Some(InstanceStatus::ReducingOnly))
        }
        OutOfBandPolicy::Terminate => {
            (Exposure(0.0), Some(InstanceStatus::Terminated))
        }
    }
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p grid-engine -- reconciler`
Expected: 全部 PASS

- [ ] **Step 5: 提交**

```bash
git add -A && git commit -m "feat(engine): add reconciler - core orchestration logic"
```

---

### Task 10: grid-engine 多实例管理器

**Files:**
- Create: `engine/src/manager.rs`

- [ ] **Step 1: 实现 InstanceManager 骨架**

```rust
use std::collections::HashMap;
use std::sync::Arc;
use anyhow::Result;
use grid_core::strategy::GridConfig;
use grid_core::risk::CapacityBudget;
use crate::instance::StrategyInstance;
use crate::ports::{ExchangePort, MarketDataPort, PersistencePort, ClockPort, PriceTick};
use crate::reconciler;

pub struct InstanceManager {
    instances: HashMap<String, StrategyInstance>,
    budgets: HashMap<String, CapacityBudget>,
    exchange: Arc<dyn ExchangePort>,
    persistence: Arc<dyn PersistencePort>,
    clock: Arc<dyn ClockPort>,
}

impl InstanceManager {
    pub fn new(
        exchange: Arc<dyn ExchangePort>,
        persistence: Arc<dyn PersistencePort>,
        clock: Arc<dyn ClockPort>,
    ) -> Self {
        Self {
            instances: HashMap::new(),
            budgets: HashMap::new(),
            exchange,
            persistence,
            clock,
        }
    }

    pub fn add_instance(
        &mut self,
        id: String,
        symbol: String,
        config: GridConfig,
        budget: CapacityBudget,
    ) -> Result<()> {
        grid_core::strategy::validate_config(&config)
            .map_err(|e| anyhow::anyhow!(e))?;
        let instance = StrategyInstance::new(id.clone(), symbol, config);
        self.instances.insert(id.clone(), instance);
        self.budgets.insert(id, budget);
        Ok(())
    }

    pub async fn on_price_tick(&mut self, tick: &PriceTick) -> Vec<grid_core::events::DomainEvent> {
        let mut all_events = vec![];
        let ids: Vec<String> = self.instances.keys()
            .filter(|id| self.instances[*id].symbol == tick.symbol)
            .cloned()
            .collect();

        for id in ids {
            let instance = self.instances.get(&id).unwrap();
            let budget = self.budgets.get(&id).unwrap();
            let result = reconciler::reconcile(instance, tick.last_price, budget);

            if let Some(new_status) = result.new_status {
                self.instances.get_mut(&id).unwrap().status = new_status;
            }
            self.instances.get_mut(&id).unwrap().current_exposure = result.target_exposure;
            self.instances.get_mut(&id).unwrap().last_price = Some(tick.last_price);

            all_events.extend(result.plan.events);
        }
        all_events
    }

    pub fn list_instances(&self) -> Vec<&StrategyInstance> {
        self.instances.values().collect()
    }

    pub fn get_instance(&self, id: &str) -> Option<&StrategyInstance> {
        self.instances.get(id)
    }
}
```

- [ ] **Step 2: 验证编译通过**

Run: `cargo check -p grid-engine`
Expected: 编译成功

- [ ] **Step 3: 验证全部测试通过**

Run: `cargo test`
Expected: 全部 PASS

- [ ] **Step 4: 提交**

```bash
git add -A && git commit -m "feat(engine): add InstanceManager - multi-instance lifecycle management"
```

---

## 验收标准

完成上述 10 个 Task 后：

1. `cargo test -p grid-core` 全部通过 — 策略计算、风控评估均有覆盖
2. `cargo test -p grid-engine` 全部通过 — reconciler 流程有覆盖
3. `cargo check` 零警告编译 — 类型系统完整
4. grid-core 的 Cargo.toml 不含 tokio/async 依赖 — 纯函数约束成立
5. grid-engine 的 reconcile 是纯函数 — 无 IO 调用
6. 端口 trait 定义完整 — ExchangePort, MarketDataPort, PersistencePort, ClockPort

## 后续计划

本计划完成后，按顺序创建独立实现计划：

1. **第二阶段：grid-storage** — SQLite 持久化适配器
2. **第三阶段：grid-binance** — Binance 交易所适配器
3. **第四阶段：grid-server** — HTTP/WS 服务端 + 组件组装
4. **第五阶段：grid-tui** — 终端 UI 适配
