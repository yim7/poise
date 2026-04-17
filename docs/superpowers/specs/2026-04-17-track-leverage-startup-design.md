# 每个 Track 杠杆启动设置设计

## 背景

当前 `Poise` 的 `track` 配置没有显式杠杆字段，服务启动时也不会主动把目标杠杆下发到交易所。

这会带来两个实际问题：

1. 配置里声明的风险预算和交易所当前杠杆状态可能不一致。
2. 启动阶段需要做的交易所控制动作还没有清晰边界，继续沿用 `engine` 共享 port、`Connected` / `Exchange` getter 扩张，或者把 startup-only 配置塞进共享 prepared 定义，都会把 server 启动期问题扩散到更广的运行时抽象。

另外，当前不同交易所的容量语义并不相同：

- Binance 的 `AccountCapacitySnapshot.max_increase_notional` 由可用余额和 symbol 当前杠杆共同决定。
- Bybit 现有实现主要来自钱包可用余额，当前不直接受 symbol 杠杆影响。

因此，本轮需要同时解决两件事：补齐每个 `track` 的 `leverage` 配置，以及把这项能力留在 server 自己拥有的 symbol 启动控制边界里。

## 目标

- `tracks[]` 支持 `leverage` 配置，默认值是 `10`
- 配置校验阶段拒绝无效杠杆值
- 服务启动时按 `track` 对交易所 symbol 设置目标杠杆
- 任一 `track` 设置杠杆失败时，整个服务启动失败
- 杠杆设置留在 server 启动控制边界，不进入 `engine/src/ports.rs`
- `leverage` 不进入 `ConfiguredTrackDefinition` / `TrackPreparedDefinition`
- `Connected` / `Exchange` 不再通过新增 pass-through getter 承担新的启动控制职责

## 非目标

- 不在本轮把杠杆展示到 HTTP / WebSocket / TUI
- 不在 `core::strategy::TrackConfig` 里加入杠杆语义
- 不把 `margin mode`、`position mode` 和 `leverage` 混成一个字段
- 不在本轮为 account / session 级启动控制定义统一接口
- 不引入“设置失败后降级运行”或“后台异步补设”语义

说明：`margin mode` 和 `position mode` 都不是 `leverage`。它们不仅是另一类启动控制配置，而且不同交易所对它们的作用域和组合方式也不一致：有的更接近账户级设置，有的可以按 symbol 设置，有的同时存在多种层级。以后如果需要支持，应进入独立的 venue-aware bootstrap 边界，而不是被塞进这次的 symbol 杠杆流程。

## 配置模型

### 新增字段

在 `server` 配置文件的每个 `[[tracks]]` 段增加：

```toml
leverage = 10
```

字段语义：

- 表示该 `track` 对应合约 `symbol` 的目标杠杆倍数
- 默认值为 `10`
- 必须是正整数

### 配置展开

`server/src/config.rs` 的 `TrackFileDefinition` 增加 `leverage: Option<u32>`。

该字段只留在 server 侧，不进入 `application/src/track_definition.rs` 的 `ConfiguredTrackInput`、`ConfiguredTrackDefinition`、`TrackPreparedDefinition`。

server 只需要从配置中提取一份 startup-only 的 `track_id -> leverage` 索引，例如：

```rust
HashMap<TrackId, u32>
```

它的职责只有两件事：

- 把 `None` 展开为默认值 `10`
- 在 server 边界校验 startup-only 配置是否合法

这份索引只保存 startup-only 字段，不复制 `instrument`。`instrument` 仍然只从 `PreparedTrackRegistry` 取得，避免形成两份并行的身份来源。

## 启动控制边界

### 归属

杠杆设置是 server 启动期的控制面问题，不属于 `engine` 运行时协作。因此本轮不在 `engine/src/ports.rs` 新增 `LeveragePort`。

新的边界放在 `server/src/exchange_startup.rs`，只负责“在 runtime 初始化之前，执行 symbol 杠杆设置”。

### 作用域

本轮只定义 **symbol 级杠杆设置**。

推荐边界形状：

```rust
#[async_trait]
trait SymbolLeverageSetter: Send + Sync {
    async fn set_leverage(&self, instrument: &Instrument, leverage: u32) -> Result<()>;
}
```

这里特意不定义一个“通吃所有未来启动动作”的 startup 接口，原因是不同启动控制项的作用域和 venue 语义都不相同：

- `leverage` 是 symbol 级动作
- `margin mode`、`position mode` 未来可能是 account 级、symbol 级，或两者混合

如果以后要支持后者，应新增独立的 venue-aware bootstrap 边界，而不是把它们折叠进 `set_leverage(...)`。

### 依赖方向

为了不引入依赖环，symbol 启动控制的抽象放在 `server` crate 内，由 server 去包裹交易所 crate 暴露的最小控制 helper。

也就是说：

- `poise_binance` 暴露一个最小公开 helper，用于设置 symbol 杠杆
- `poise_bybit` 暴露一个最小公开 helper，用于设置 symbol 杠杆
- `server` 用这些 helper 组装出 `SymbolLeverageSetter`

这样 server 拥有抽象边界，交易所 crate 只暴露具体能力，不需要依赖 server，也不需要理解 `track` 或 prepared definition。

## Binance / Bybit 责任划分

### 交易所 crate

Binance 和 Bybit crate 各自负责：

- 调用本交易所设置杠杆的 REST 接口
- 保留交易所错误文本
- 不把这项能力并入 `Connected` 的运行时 port 集合

它们可以新增公开的窄能力 helper，例如：

- `poise_binance::SymbolLeverageControl`
- `poise_bybit::SymbolLeverageControl`

这些 helper 只暴露本轮需要的最小动作：`set_leverage(symbol, leverage)`。不引入更宽的 `StartupControl` 名称，避免把后续无关的启动动作继续吸进来。

### server crate

`server` 负责：

- 从 `TrackFileDefinition` 构造 `track_id -> leverage` 索引
- 根据 `ExchangeConfig` 选择对应交易所 helper
- 在装配点把 prepared track 的 `instrument` 和 startup-only 的 `leverage` 组合起来调用设置动作
- 决定启动失败语义和错误文案

## 启动顺序

`server/src/assembly.rs` 的每个 `track` 启动顺序调整为：

1. 从 `PreparedTrackRegistry` 读取 runtime 需要的 track 定义
2. 从 server 的 `track_id -> leverage` 索引读取对应杠杆
3. 调用 `SymbolLeverageSetter::set_leverage(instrument, leverage)`
4. 读取 `exchange info`
5. 读取 `account_capacity_snapshot`
6. 注册到 `TrackManager`
7. 恢复本地持久化状态

这里的关键不是把“先设杠杆再读容量”定义成跨交易所共享不变量，而是把“symbol 杠杆设置发生在 runtime bootstrap 之前”固定下来。

具体到不同交易所：

- 对 Binance，这个顺序还能保证容量快照反映目标杠杆后的 `max_increase_notional`
- 对 Bybit，当前容量快照实现并不依赖 symbol 杠杆；沿用同一顺序只是为了让启动过程保持确定性

因此，`AccountCapacitySnapshot` 在本轮仍然保持 venue-defined 语义，不要求因为杠杆设置而变成一套跨交易所统一模型。

## 失败语义

任一 `track` 设置杠杆失败，整个服务启动失败。

错误文案需要包含：

- `track_id`
- `symbol`
- 目标杠杆
- 交易所原始错误或其摘要

建议格式：

```text
failed to set startup leverage for track `btc` symbol `BTCUSDT` to 10x: <exchange error>
```

这里直接把错误写成具体的杠杆设置失败，不再加一层更泛的 startup-state 包装，避免为当前单一能力引入额外抽象。

## 测试方案

### 配置与 startup-only 杠杆索引

`server/src/config.rs`

- `leverage` 显式配置时能正确解析
- `leverage = 0` 时校验失败

`server/src/exchange_startup.rs`

- startup-only 杠杆索引会把默认 `leverage` 展开为 `10`
- 显式 `leverage` 会保留到 `track_id -> leverage` 索引
- 索引不复制 `instrument`

### 启动装配

`server/src/assembly.rs`

- 启动会先读取 `track_id -> leverage` 索引，再执行 `set_leverage`
- `set_leverage` 失败时，启动直接失败
- 错误中会带上 `track_id`、`symbol` 和目标杠杆
- `Exchange` 现有运行时 port 装配不需要增加新的 getter

### 交易所适配

`exchanges/binance`

- startup helper 会向正确接口发送 `symbol + leverage`
- 成功响应返回 `Ok(())`
- 错误响应保留失败信息

`exchanges/bybit`

- startup helper 会向正确接口发送 linear 合约所需的杠杆请求体
- 成功响应返回 `Ok(())`
- 错误响应保留失败信息

## 文档变更

- `README.md` 的配置示例增加 `leverage = 10`
- `server/src/config.rs` 内嵌示例补充 `leverage`
- 本轮不修改协议文档，因为当前不对外暴露杠杆字段

## 兼容性与演进

- 现有未配置 `leverage` 的实例在升级后会自动按默认值 `10` 运行
- 杠杆属于 server-owned 的 startup-only 状态，不属于 `engine` 领域状态，也不属于 `application` 的 prepared runtime 定义
- 当前系统同一交易所内不允许重复 `symbol`，因此不会出现多个 `track` 竞争同一个杠杆设置的问题
- 未来如果要支持 `margin mode` 或 `position mode`，应新增独立的 venue-aware bootstrap 边界；该边界需要先表达各交易所自己的作用域模型，再决定是否值得抽公共接口，而不是继续扩张 `set_leverage` 这条窄能力接口

## 实施原则

- 先补测试，再做最小实现
- `engine/src/ports.rs` 不新增杠杆相关共享 port
- `ConfiguredTrackDefinition` / `TrackPreparedDefinition` 不新增 `leverage`
- `Connected` / `Exchange` 不为这次功能新增 pass-through getter
- 只在 server 启动装配边界引入交易所控制能力
