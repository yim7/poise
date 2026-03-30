# Poise 架构设计

基于[网格策略族模型设计](2026-03-24-grid-strategy-family-design.md)，从零定义 `Poise` 的技术架构。

当前工作区仍保留 `grid-*` crate、二进制和 `Grid*` 类型命名；本文只统一产品名与对外文案。

补充说明：身份模型、运行态边界和 transport/application 的职责划分，以
[`2026-03-25-grid-runtime-boundary-redesign.md`](2026-03-25-grid-runtime-boundary-redesign.md)
为最新版本。本文档保留整体 crate 结构和依赖方向，具体到运行时术语时应以
`GridId`、`Instrument`、`GridRuntime`、`GridManager` 为准。

## 1. 设计约束

- 单机单进程，进程内通过 channel/trait 隔离
- 多交易所作为一等设计约束，第一版只实现 Binance
- 对外接入面：TUI（独立进程）、HTTP API、可嵌入 Rust library
- 正确性与模块化同等重要
- 技术栈：Rust + tokio

## 2. 架构风格

六边形架构（Ports & Adapters）。

- 领域核心是纯函数库，无 async，无 IO
- 引擎编排层定义端口 trait，编排策略-风控-执行流程
- 适配器实现端口 trait，封装具体交易所、持久化、传输细节
- 所有依赖方向从外向内，内层不知道外层的存在

## 3. Crate 结构

```
poise/
├── core/           # grid-core (library)
├── engine/         # grid-engine (library)
├── protocol/       # grid-protocol (library)
├── exchanges/
│   └── binance/    # grid-binance (library)
├── storage/        # grid-storage (library)
├── server/         # grid-server (binary)
└── tui/            # grid-tui (binary)
```

依赖方向：

```
grid-server (bin)
  ├── grid-engine    → grid-core
  ├── grid-protocol
  ├── grid-binance   → grid-engine (实现端口 trait)
  └── grid-storage   → grid-engine (实现端口 trait)

grid-tui (bin)
  ├── grid-protocol
  └── 通过 HTTP/WS 连接 grid-server，不依赖 engine 或 core
```

| Crate | 职责 | 不做什么 |
|---|---|---|
| `grid-core` | 策略模型、风控规则、领域类型、领域事件。纯函数，无 IO | 不知道交易所、数据库、网络的存在 |
| `grid-engine` | 引擎编排：实例生命周期、执行规划、风控拦截。定义端口 trait | 不实现任何具体适配器 |
| `grid-binance` | Binance REST/WS 适配器 | 不包含策略逻辑 |
| `grid-storage` | SQLite 持久化适配器 | 不包含业务规则 |
| `grid-server` | HTTP/WS 服务 + 组件组装 + 启动入口 | 不包含策略逻辑 |
| `grid-protocol` | 共享 HTTP / WS DTO，与服务端内部类型解耦 | 不依赖 engine / server 实现细节 |
| `grid-tui` | 终端 UI，纯客户端 | 不直接依赖 engine |

`grid-core` 的 `Cargo.toml` 只允许 `serde`，不允许 `tokio`/`async`/`reqwest` 等 IO 依赖。

嵌入库场景：依赖 `grid-core` + `grid-engine`，自行实现端口 trait。

## 4. grid-core — 纯领域核心

零 async，零 IO，只有纯类型和纯函数。

### 4.1 模块结构

```
core/src/
├── lib.rs
├── types.rs        # 领域基础类型
├── strategy.rs     # 策略模型 + 目标占用函数
├── risk.rs         # 风控规则
└── events.rs       # 领域事件
```

### 4.2 types.rs

```rust
pub struct Symbol(pub String);
pub struct Price(pub f64);
pub struct Quantity(pub f64);
pub struct Exposure(pub f64);

pub enum Side { Buy, Sell }

pub struct ExchangeRules {
    pub price_tick: f64,
    pub quantity_step: f64,
    pub min_qty: f64,
    pub min_notional: f64,
}
```

### 4.3 strategy.rs

直接映射策略族设计文档：

```rust
pub struct GridConfig {
    pub lower_price: f64,
    pub upper_price: f64,
    pub long_capacity: f64,
    pub short_capacity: f64,
    pub capacity_notional: f64,
    pub shape_family: ShapeFamily,
    pub out_of_band_policy: OutOfBandPolicy,
}

pub enum ShapeFamily { Linear, Convex, Concave }
pub enum OutOfBandPolicy { Freeze, ReduceOnly, Terminate, Hold }

/// 纯函数：给定价格，返回目标占用
pub fn target_exposure(price: f64, config: &GridConfig) -> Exposure;

/// 纯函数：验证配置合法性
pub fn validate_config(config: &GridConfig) -> Result<(), ConfigError>;

/// 纯函数：判断带内/带外状态
pub fn band_status(price: f64, config: &GridConfig) -> BandStatus;

pub enum BandStatus {
    InBand { target: Exposure },
    OutOfBand { policy: OutOfBandPolicy, boundary: BandBoundary },
}

pub enum BandBoundary { Below, Above }
```

### 4.4 risk.rs

```rust
pub struct ExposureIntent {
    pub current_exposure: Exposure,
    pub target_exposure: Exposure,
}

pub enum RiskDecision {
    Allow(Exposure),
    Cap(Exposure),
    Deny { reason: String },
}

pub struct CapacityBudget {
    pub max_notional: f64,
    pub daily_loss_limit: f64,
    pub stop_loss_pct: f64,
}

/// 纯函数：评估风险
pub fn evaluate_risk(
    intent: &ExposureIntent,
    budget: &CapacityBudget,
) -> RiskDecision;
```

### 4.5 events.rs

```rust
pub enum DomainEvent {
    ExposureTargetChanged { from: Exposure, to: Exposure },
    BandBreached { boundary: BandBoundary, price: f64 },
    BandReentered { price: f64 },
    PolicyTriggered { policy: OutOfBandPolicy },
    RiskCapApplied { intended: Exposure, capped: Exposure },
    RiskDenied { reason: String },
}
```

### 4.6 设计决策

- 所有公开函数都是纯函数，可直接单元测试
- core 不管状态，只做单次计算。状态管理在 engine 层
- `ExchangeRules` 作为传入参数，core 不知道交易所的存在
- 领域事件是值类型，core 产生事件但不负责发布

## 5. grid-engine — 引擎编排

定义端口 trait，编排策略计算 → 风控拦截 → 执行决策。

### 5.1 模块结构

```
engine/src/
├── lib.rs
├── ports.rs            # 端口 trait 定义
├── instance.rs         # 单个策略实例运行时
├── reconciler.rs       # 核心循环：目标 vs 实际 → 执行计划
├── execution_plan.rs   # exposure delta → 订单意图
└── manager.rs          # 多实例管理
```

### 5.2 ports.rs

```rust
#[async_trait]
pub trait ExchangePort: Send + Sync {
    async fn submit_order(&self, req: OrderRequest) -> Result<OrderReceipt>;
    async fn cancel_order(&self, symbol: &str, order_id: &str) -> Result<()>;
    async fn cancel_all(&self, symbol: &str) -> Result<Vec<String>>;
    async fn get_position(&self, symbol: &str) -> Result<Position>;
    async fn get_open_orders(&self, symbol: &str) -> Result<Vec<ExchangeOrder>>;
    async fn get_exchange_info(&self, symbol: &str) -> Result<ExchangeInfo>;
}

#[async_trait]
pub trait MarketDataPort: Send + Sync {
    async fn subscribe_prices(&self, symbol: &str)
        -> Result<mpsc::Receiver<PriceTick>>;
    async fn subscribe_user_data(&self)
        -> Result<mpsc::Receiver<UserDataEvent>>;
}

#[async_trait]
pub trait StateRepositoryPort: Send + Sync {
    async fn save_transition(
        &self,
        id: &str,
        state: &GridSnapshot,
        events: &[DomainEvent],
    ) -> Result<()>;
    async fn load_grid_state(&self, id: &str) -> Result<Option<GridSnapshot>>;
    async fn list_events(&self, id: &str) -> Result<Vec<DomainEvent>>;
}

pub trait ClockPort: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}
```

### 5.3 instance.rs

```rust
pub struct StrategyInstance {
    pub id: String,
    pub symbol: String,
    pub config: GridConfig,
    pub status: GridStatus,
    pub current_exposure: Exposure,
    pub reference_price: Option<f64>,
    pub out_of_band_since: Option<DateTime<Utc>>,
}

pub enum GridStatus {
    WaitingMarketData,
    Active,
    Frozen,
    ReducingOnly,
    Holding,
    Terminated,
    Paused,
}
```

### 5.4 reconciler.rs

核心协调逻辑，替代当前的 `kernel.rs`：

```rust
/// 纯函数：输入价格 → 输出执行计划
pub fn reconcile(
    instance: &StrategyInstance,
    price: f64,
    budget: &CapacityBudget,
) -> ReconcileResult {
    // 1. core::band_status 判断带内/带外
    // 2. 根据状态决定目标
    // 3. core::evaluate_risk 风控拦截
    // 4. 生成 ExecutionPlan
}
```

reconcile 本身是纯函数。IO（下单、撤单）由调用方拿到执行计划后通过端口执行。

### 5.5 execution_plan.rs

```rust
pub struct ExecutionPlan {
    pub actions: Vec<ExecutionAction>,
    pub events: Vec<DomainEvent>,
}

pub enum ExecutionAction {
    SubmitOrder(OrderRequest),
    CancelOrder { order_id: String },
    CancelAll,
    NoOp,
}
```

### 5.6 设计决策

- reconcile 是纯函数，IO 只发生在 manager 的 async 循环里
- 端口 trait 定义在 engine 层（core 无 async，适配器不应反向依赖）
- 执行计划是数据而非动作，测试可以断言计划内容而不需要 mock 交易所
- 状态在 instance 里，core 只做无状态计算

## 6. 适配器层

### 6.1 grid-binance

```
exchanges/binance/src/
├── lib.rs
├── rest.rs         # REST API 客户端
├── websocket.rs    # WS 市场数据 + 用户数据流
├── adapter.rs      # 实现 ExchangePort + MarketDataPort
└── types.rs        # Binance JSON ↔ core 领域类型转换
```

- Binance 的签名、限速、重连逻辑全部封装在此 crate
- 加第二家交易所 = 新建 `exchanges/okx/`，实现同样的 trait

### 6.2 grid-storage

```
storage/src/
├── lib.rs
├── sqlite.rs       # 实现 StateRepositoryPort
└── schema.rs       # 表结构定义
```

## 7. grid-server — 服务端入口

```
server/src/
├── main.rs         # 读配置 → 组装组件 → 启动
├── config.rs       # 配置文件解析
├── http.rs         # HTTP 路由和 handler
├── websocket.rs    # WebSocket 事件推送
└── assembly.rs     # 组件组装
```

### 7.1 assembly.rs

六边形架构的组装点，只有这个文件知道所有具体类型：

```rust
pub fn assemble(config: &Config) -> Result<ServerPlatform> {
    let clock = SystemClock::new();
    let exchange = BinanceAdapter::new(&config.exchange)?;
    let repository = SqliteStorage::new(&config.db_path)?;

    let manager = InstanceManager::new(Arc::new(clock));
    let service = Arc::new(GridPlatformService::new(
        manager,
        Arc::new(repository),
        broadcast::channel(256).0,
    ));
    let state = ServerState { service: service.clone() };
    let runtime = ServerRuntime::new(state.clone(), Arc::new(exchange), Arc::new(exchange));

    Ok(ServerPlatform { state, runtime })
}
```

### 7.2 HTTP/WS 接口

| 路径 | 方法 | 用途 |
|---|---|---|
| `/grids` | GET | 列出所有网格 |
| `/grids/{id}/snapshot` | GET | 查询网格快照 |
| `/grids/{id}/commands` | POST | 提交命令 |
| `/ws` | WS | 实时事件流 |

## 8. 整体数据流

一次价格变动的完整路径：

1. `MarketDataPort` 推送 `PriceTick`
2. `GridManager` 根据 `Instrument` 找到对应 `GridRuntime`
3. 调用 `observe(grid_id, GridObservation::Market(...))`
4. engine 生成 `GridTransition`，如果 `effects` 为空则跳过执行
5. 通过 `ExchangePort` 执行订单动作
6. 执行成功后更新 `GridRuntime` 状态
7. 通过 `StateRepositoryPort` 原子持久化快照与事件
8. 广播 `DomainEvent` 给 HTTP/WS 层推送给客户端

## 9. 非目标

- 不在第一版引入事件溯源
- 不引入 actor 框架
- 不做多进程部署
- 不在 core/engine 层引入 HTTP 框架依赖
