# 网格运行时边界重构设计

## 1. 背景

当前代码已经把对外对象统一成 `Grid`，但内部仍残留一套旧抽象：

- `InstanceManager`
- `StrategyInstance`
- 按 `symbol` 路由运行态更新
- 由 `server` 外层拼装和回滚快照
- 订单/仓位更新后通过伪造 `PriceTick` 触发重算

这说明架构收敛只完成了命名层，真正的知识边界还没有收回来。继续在这个结构上叠加功能，会持续放大三个问题：

- `change amplification`：支持多交易所、多网格身份、回放或更多控制面时，要同时改 `engine`、`server`、`storage`
- `cognitive load`：开发者需要同时记住 `grid id`、`symbol`、快照拼装、事务回滚、重算入口的隐式约定
- `unknown unknowns`：看不清一个变化会不会影响路由、持久化、广播或协议投影

这次重构不追求兼容旧边界，而是重新定义所有权。

## 2. 设计目标

### 2.1 主目标

- `GridId` 成为唯一的一等运行时身份
- `engine` 真正拥有网格运行态、状态迁移和快照
- `server` 只负责事务编排、传输适配和外部接入
- 外部事件进入系统后，不再依赖 `symbol -> track_id` 回查和伪造 `PriceTick`

### 2.2 非目标

- 这次不扩展新的交易所功能
- 这次不增加新的策略族能力
- 这次不保留旧命名兼容层
- 这次不做“最小改动”，而是优先重画边界

## 3. 核心设计决策

### 3.1 `GridId` 不再由 `symbol` 派生

`GridId` 是稳定身份，不是显示字段，也不是交易路由字段。

当前约束仍然成立：

- 同一交易所内，同一 `symbol` 只允许一个网格

但这个约束属于注册规则，不属于身份定义。未来即使放开这个约束，也不应该改动所有运行时接口。

因此配置层需要显式声明：

```toml
[[tracks]]
track_id = "btc-core"
venue = "binance"
symbol = "BTCUSDT"
```

而不是继续从 `symbol` 推导 `GridId`。

### 3.2 `symbol` 降级为 `Instrument` 的一部分

引入新的市场绑定类型：

```rust
pub struct Instrument {
    pub venue: Venue,
    pub symbol: String,
}
```

它只负责交易所路由，不承担网格身份。

这样之后：

- Binance 用户流按 `Instrument` 找到对应网格
- 持久化按 `GridId` 存取
- HTTP / WebSocket 按 `GridId` 暴露

每个概念只做一件事，不再混用。

### 3.3 `GridRuntime` 成为真正的运行时聚合

当前 `StrategyInstance`、`budgets`、`exchange_rules`、`GridSnapshot` 的知识分散在多个模块里。以后统一收进一个聚合：

```rust
pub struct GridRuntime {
    pub definition: GridDefinition,
    pub exchange_rules: ExchangeRules,
    pub status: GridStatus,
    pub observed: ObservedState,
    pub risk: RiskState,
    pub pending_order: Option<PendingOrder>,
}
```

其中：

- `GridDefinition` 持有静态配置
- `ObservedState` 持有最新参考价、仓位、带外时间等观察结果
- `RiskState` 持有已实现/未实现盈亏等风控运行态

外层不再自己拼快照。`GridRuntime` 自己负责：

- `snapshot()`
- `restore(snapshot)`
- `apply(input)`

这意味着状态结构的变化只在 `engine` 内部收敛。

### 3.4 `GridManager` 只按 `GridId` 管理

`InstanceManager` 重命名为 `GridManager`，但不是机械重命名。它的职责也要一起收敛：

- 注册 `GridDefinition`
- 维护 `GridId -> GridRuntime`
- 维护 `Instrument -> GridId`
- 应用外部观察或命令并返回 `GridTransition`
- 暴露读模型和快照

建议接口：

```rust
pub trait GridEngine {
    fn register(&mut self, definition: GridDefinition, rules: ExchangeRules) -> Result<()>;
    fn restore(&mut self, snapshot: GridRuntimeSnapshot) -> Result<()>;
    fn resolve_track_id(&self, instrument: &Instrument) -> Option<GridId>;
    fn observe(&mut self, id: &GridId, observation: GridObservation) -> Result<GridTransition>;
    fn command(&mut self, id: &GridId, command: GridCommand) -> Result<GridTransition>;
    fn snapshot(&self, id: &GridId) -> Option<GridRuntimeSnapshot>;
    fn list(&self) -> Vec<GridView>;
}
```

设计重点：

- 外部只能按 `GridId` 修改网格
- `symbol` 解析逻辑收进 `resolve_track_id()`
- `server` 不再直接扫描 manager 内部实例

### 3.5 拆分 `GridObservation` 与 `GridCommand`

`GridInput` 这个名字看起来像“用户输入”，而这个抽象实际承载的是两类不同语义：

- 观察事实
- 控制命令

如果继续把它们混成一个公开枚举，会增加阅读成本，也会让接口表达变弱。

当前最大的问题之一，是不同外部事件没有稳定的内部表达。价格流直接进 `on_price_tick()`，订单/仓位更新则先改状态，再伪造一个价格 tick 重算。

这属于错误的时间顺序分解。公开接口改成两个枚举：

```rust
pub enum GridObservation {
    Market(MarketObservation),
    Position(PositionObservation),
    Order(OrderObservation),
}

pub enum GridCommand {
    Pause,
    Resume,
    Reconcile,
}
```

规则如下：

- `GridObservation::Market`：更新最新参考价并触发重算
- `GridObservation::Position`：更新观察到的仓位，不自动伪装成市场价格
- `GridObservation::Order`：更新挂单和成交观察状态
- `GridCommand::Pause` / `GridCommand::Resume`：纯控制命令
- `GridCommand::Reconcile`：基于当前最新参考价重算

这样订单/仓位更新后要立即重算时，`server` 只需追加一个 `GridCommand::Reconcile`，不再伪造 `PriceTick`。

如果 `engine` 内部状态机实现上仍然希望统一入口，可以保留私有枚举：

```rust
enum GridTrigger {
    Observation(GridObservation),
    Command(GridCommand),
}
```

但 `GridTrigger` 不作为公开 API 名字暴露给外部。

### 3.6 `GridTransition` 成为唯一写侧产物

所有写操作都统一返回：

```rust
pub struct GridTransition {
    pub snapshot: GridRuntimeSnapshot,
    pub events: Vec<DomainEvent>,
    pub effects: Vec<GridEffect>,
}
```

其中：

- `snapshot` 是变更后的完整快照
- `events` 是需要持久化和广播的领域事件
- `effects` 是需要外部执行的动作，例如下单、撤单

这能把三件事一次性说清楚：

- 状态变成了什么
- 产生了什么业务语义
- 外部还要做什么动作

外层不再自己比较“前后快照是否不同”来决定是否发 `SnapshotUpdated` 之类的补丁语义。

## 4. 模块所有权

### 4.1 `poise-core`

拥有：

- 策略模型
- 风控模型
- 领域事件定义

不拥有：

- 运行态
- 快照
- 交易所路由

### 4.2 `poise-engine`

拥有：

- `GridId`
- `Venue`
- `Instrument`
- `GridDefinition`
- `GridRuntime`
- `GridManager`
- `GridObservation`
- `GridCommand`
- `GridTransition`
- `GridRuntimeSnapshot`

不拥有：

- HTTP / WebSocket DTO
- SQLite 事务
- Binance API 细节

### 4.3 `poise-storage`

拥有：

- `GridRuntimeSnapshot + DomainEvent[]` 的原子持久化

不拥有：

- 快照结构的拼装知识
- 协议投影

### 4.4 `poise-server`

分成三个明确边界：

- `runtime.rs`
  只接收交易所流，转换成 `GridObservation`
- `application.rs`
  只做写侧事务：调用 `GridManager`、保存 transition、发布内部事件
- `http.rs` / `websocket.rs`
  只做 `protocol` 映射

`server` 不再拥有：

- 网格状态结构
- `symbol -> grid id` 路由规则
- 快照拼装和回滚知识

### 4.5 `poise-protocol`

只拥有对外契约：

- `GridSummary`
- `GridSnapshot`
- `CommandRequest`
- `CommandResponse`
- `WsEvent`

不拥有：

- engine 内部快照
- server 内部事务语义

## 5. 数据流

### 5.1 市场价格

1. Binance 适配器产出 `MarketObservation`
2. `server/runtime` 根据 `Instrument` 找到 `GridId`
3. `application` 调用 `engine.observe(track_id, GridObservation::Market(...))`
4. engine 返回 `GridTransition`
5. application 原子保存 `snapshot + events`
6. application 发布事件并执行 `effects`

### 5.2 订单 / 仓位更新

1. Binance 适配器产出 `OrderObservation` 或 `PositionObservation`
2. `server/runtime` 根据 `Instrument` 找到 `GridId`
3. `application` 调用 `engine.observe(track_id, GridObservation::Position(...))` 或 `engine.observe(track_id, GridObservation::Order(...))`
4. 如果该观察更新要求立即重算，application 继续调用 `engine.command(track_id, GridCommand::Reconcile)`

注意：这里的“立即重算”是显式业务动作，不再通过伪造市场 tick 间接触发。

### 5.3 HTTP 控制命令

1. `http.rs` 解析 `track_id`
2. `application` 调用 `engine.command(track_id, GridCommand::Pause)` 或 `engine.command(track_id, GridCommand::Resume)`
3. engine 返回 transition
4. application 保存并广播

协议示例里应明确展示这种关系：

- `track_id = "btc-core"`
- `symbol = "BTCUSDT"`

## 6. 旧抽象替换表

| 旧抽象 | 新抽象 | 处理方式 |
|---|---|---|
| `InstanceManager` | `GridManager` | 重命名并重写边界 |
| `StrategyInstance` | `GridRuntime` | 删除旧名，不保留别名 |
| `instance_snapshots` | `grid_runtime_snapshots` 或 `grid_snapshots` | 统一数据命名 |
| `on_price_tick()` | `observe(GridObservation::Market(...))` | 删除旧入口 |
| `apply_position_update()` | `observe(GridObservation::Position(...))` | 删除旧入口 |
| `apply_order_update()` | `observe(GridObservation::Order(...))` | 删除旧入口 |
| 伪造 `PriceTick` 重算 | `command(GridCommand::Reconcile)` | 彻底删除 |

## 7. 为什么不采用其他方案

### 7.1 不继续强化 `GridPlatformService`

原因：

- 它已经同时承担事务、查询投影、协议映射、事件广播
- 再继续扩张，只会形成更大的浅模块

正确方向不是“把所有东西都收进 service”，而是把知识压回真正拥有它的模块。

### 7.2 不做只改名不改边界

原因：

- 这会让代码表面术语统一，但结构问题原封不动
- 后续每次扩展都会再次碰到同样的隐式耦合

## 8. 迁移策略

这次迁移分四段，不并发推进：

### 阶段 1：术语和身份模型收敛

- 引入显式 `track_id`
- 引入 `Venue` 和 `Instrument`
- `InstanceManager` 改成 `GridManager`
- `StrategyInstance` 改成 `GridRuntime`
- 删除所有新代码中的 `instance` 术语

### 阶段 2：运行态和快照下沉到 `engine`

- 引入 `GridRuntimeSnapshot`
- `GridRuntime` 自己实现 `snapshot()` / `restore()`
- 删除 `server` 中的手工快照拼装

### 阶段 3：统一写侧输入和 transition

- 引入 `GridObservation`
- 引入 `GridCommand`
- 引入 `GridTransition`
- 删除 `on_price_tick()`、`apply_position_update()`、`apply_order_update()` 这些分散入口
- 删除伪造 tick 的重算路径

### 阶段 4：瘦身 server

- `runtime` 只做外部流适配
- `application` 只做事务
- `http` / `ws` 只做协议映射

## 9. 验收标准

满足以下条件才算完成：

- `engine` 不再暴露按 `symbol` 修改网格状态的接口
- `server` 中不存在伪造 `PriceTick` 触发重算的路径
- `GridId` 不再由 `symbol` 派生
- `GridRuntimeSnapshot` 的组装和恢复逻辑只存在于 `engine`
- `InstanceManager`、`StrategyInstance`、`instance_*` 旧术语从核心代码中删除
- `runtime`、`application`、`protocol` 的职责边界可以用一句话清楚说明

## 10. 实施建议

这份设计文档之后应单独产出一份实施计划，计划必须满足：

- 测试先行
- 每个阶段都可独立验证
- 每个阶段完成后同步更新任务清单
- 不保留兼容层和过渡命名

在计划落地前，不要直接开始局部修补式实现。
