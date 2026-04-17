# Runtime 启动 Bootstrap 边界设计

## 背景

当前启动流程已经有一条明确的 live takeover 主线：

1. 订阅 user data
2. 获取 server time cutoff
3. `startup_sync`
4. 回放 cutoff 之后的 buffered user data
5. 进入 live user task

但最近这轮启动保证金预检修复之后，启动期又长出了一条额外的实时交易所状态探测路径：

- `server/src/assembly.rs` 在装配期先查询 `position`
- `server/src/assembly.rs` 再查询 `account_capacity_snapshot`
- 用当前持仓折算后的剩余 `required_additional_notional` 做启动失败判断
- 同时把 `account_capacity_snapshot` 直接种进 runtime 的 guard store

然后 runtime 启动后又会在 `startup_sync` 里重新查询：

- `position`
- `open_orders`

这造成两类问题：

1. 启动期“需要哪些实时交易所状态、如何重试、哪份结果算权威”被拆在 `assembly` 和 `runtime` 两处。
2. `runtime` 本身并不拥有做启动预检所需的静态定义边界；如果只是把预检代码机械搬过去，会逼 runtime 反向穿透 manager 或 query 层去找 `budget / track_config`。

另外，`qty -> strategy notional` 的换算问题已经在本轮前置修复中处理掉了：当前统一由 `TrackConfig::abs_notional_from_position_qty(...)` 提供，见 [Track Definition 与 Runtime 边界设计](2026-04-09-track-definition-runtime-boundary-design.md) 之后形成的 prepared definition / runtime 边界，以及本轮实现中的 `TrackConfig` helper。本文不再重复讨论该问题，本文只处理“启动实时状态探测应该归谁拥有”。

## 目标

- 让启动期实时交易所状态探测只由一个模块拥有
- 保留现有外部启动语义：启动失败仍然直接终止服务
- 让保证金预检、`startup_sync` 初始化和 account margin guard seed 共享同一份启动探测结果
- 不让 runtime 通过测试专用入口或读侧接口反向读取 `TrackManager` 内部定义
- 不把 `PreparedTrackRegistry` 整体泄露给 runtime

## 非目标

- 不修改 user data cutoff / replay 的外部顺序语义
- 不重做 `TrackConfig`、`CapacityBudget` 或 `PreparedTrackRegistry` 的归属
- 不把 `exchange info`、startup leverage 设置也一并搬进 runtime
- 不在本轮把 account capacity 重新定义成完全 account 级共享模型
- 不修改 HTTP / WebSocket / TUI 协议

## 当前问题

### 1. `assembly` 同时拥有静态装配和动态探测

`server/src/assembly.rs` 当前同时负责：

- 读取 prepared definition
- 查询 `exchange_info`
- 查询 `position`
- 查询 `account_capacity_snapshot`
- 做启动保证金预检
- 构建 `TrackManager`
- 恢复本地持久化状态

其中前半部分混入了两种不同性质的知识：

- 静态启动准备：definition、rules、startup leverage、持久化恢复
- 动态 live probe：position、open orders、capacity、cutoff 后 buffered replay

前者属于“装配期输入”，后者属于“接管交易所实时状态”。

### 2. 启动实时状态探测被按时间顺序拆开

当前动态 probe 的知识分散在两条路径：

- `assembly`：`position + account_capacity_snapshot`
- `runtime::startup_sync`：`position + open_orders`

这不是按知识边界拆分，而是按执行时序拆分。后续只要启动期再增加一个实时输入，例如：

- open orders 参与预检
- 更统一的 retry / logging
- startup freshness rule
- 额外的 account-level 状态

就必须同时修改两边。

### 3. `runtime` 目前没有做预检所需的静态定义输入

`runtime` 当前能稳定拿到的是：

- 端口：`execution / account / metadata / market_data`
- 运行态服务：`observation_service`
- 通过 `track_instruments()` 看到 `track_id -> instrument`

但生产代码里，`runtime` 没有 `budget` 和 `track_config` 的显式输入。它也不应该通过测试专用的 `manager()` 暴露面去读取这些信息。

因此，正确的方向不是“把预检代码拷到 runtime 里”，而是显式给 runtime 一个窄的静态启动目录。

### 4. 启动结果里混着不同作用域的状态

启动时需要拉取的实时状态至少有两类：

- track / instrument 级：
  - `position`
  - `open_orders`
- 当前 guard store 使用的 instrument keyed 容量状态：
  - `account_capacity_snapshot`

如果把它们无差别塞进“每个 track 一份的大快照”，会弱化作用域边界。即使当前接口按 instrument 查询 capacity，也不应该把这件事表述成“track 自己拥有 capacity 状态”。

## 备选方案

### 方案 A：继续把预检留在 `assembly`，只抽共享 helper

做法：

- 保留 `assembly` 里的 `position` / `account_capacity_snapshot` 查询
- 抽一个 helper 给 `startup_sync` 和预检共用

优点：

- 改动最小

缺点：

- 动态 probe 仍然分散在两处
- user data subscription 之后与之前各自看到的实时状态不再有单一 owner
- review 指出的 temporal decomposition 仍然存在

结论：

- 不采用

### 方案 B：把 `PreparedTrackRegistry` 直接交给 runtime

做法：

- runtime 在启动时直接读取完整 prepared definition
- 动态 probe 和预检全部移入 runtime

优点：

- 运行期能拿到足够的静态定义

缺点：

- runtime 暴露面过宽
- `PreparedTrackRegistry` 带着 query / restore / runtime seed 等多种投影职责，超出了 runtime 启动真正需要的最小知识
- 会把 application-owned 的 definition owner 直接泄露给 runtime

结论：

- 不采用

### 方案 C：由 application 输出 startup definition，由 runtime 统一执行动态 probe

做法：

- `assembly` 继续完成静态装配
- `application` 从 prepared definition 投影出 startup definition
- runtime startup 用这份 startup definition 做动态 probe、保证金预检、guard seed、exchange state apply 和 buffered replay

优点：

- 动态 probe 有单一 owner
- startup 输入字段仍由 definition owner 决定，不需要 server 手工挑字段
- runtime 拿到恰好够用的静态定义，不需要穿透 manager / query 层

缺点：

- 需要在 definition owner 上新增一条 startup-only 投影

结论：

- 采用

## 最终设计

采用 **方案 C：application-owned startup definition + runtime-owned 动态 bootstrap**。

### 核心原则

- `assembly` 只拥有静态启动准备
- `runtime` 只拥有实时交易所状态接管
- 保证金预检属于“实时状态接管”的一部分，不再属于装配期
- startup 输入字段由 prepared definition owner 决定，server 不再手工切片
- runtime 启动只接收启动期真正需要的静态定义，不接收完整 prepared registry

## 模块边界

### `server/src/assembly.rs`

继续负责：

- track 唯一性校验
- startup leverage 设置
- `exchange_info` / rules 查询
- 构建 `TrackManager`
- 恢复本地持久化状态
- 把 application 给出的 startup definition 传给 runtime

不再负责：

- 查询 live `position`
- 查询 `account_capacity_snapshot`
- 做启动保证金预检
- 给 runtime 直接注入 startup account capacity snapshots

### `server/src/runtime/startup_bootstrap.rs`

新增 runtime-owned 模块，负责：

- 按统一 retry 语义探测启动期实时状态
- 做保证金预检
- 把 startup probe 结果转换成 runtime 可用的启动 seed
- 写入 `account_margin_guard`
- 调用 `observation_service.sync_exchange_state(...)`
- 回放 cutoff 之后的 buffered user data

它拥有以下知识：

- 启动期需要哪些实时状态
- 这些状态的获取顺序和 retry 策略
- 哪份探测结果被视为启动真相

### `server/src/runtime/startup_sync.rs`

本次设计里不再保留独立的 `startup_sync` owner。

处理方式固定为：

- 删除现有 `startup_sync.rs`
- 如果需要保留一层纯应用 helper，只允许存在被动的私有函数，例如 `apply_startup_seed(...)`
- 这类 helper 只能消费 `startup_bootstrap` 已经准备好的 seed，不能访问交易所端口

也就是说，启动探测、预检、apply 和 replay 的 owner 全部固定在 `startup_bootstrap` 模块。

## 接口形状

### `TrackStartupDefinition`

由 definition owner 提供，推荐放在 `application/src/track_definition.rs`。

建议由 `TrackPreparedDefinition` 投影，但对 runtime 固定暴露行为接口，而不是原始字段：

```rust
impl TrackStartupDefinition {
    pub fn track_id(&self) -> &TrackId;
    pub fn instrument(&self) -> &Instrument;
    pub fn required_additional_notional(&self, position_qty: f64) -> f64;
}
```

说明：

- 内部可以继续持有 `track_config` 和 `budget`
- 但这些字段保持私有，不进入 runtime 边界
- `server` 只消费这份 startup definition，不再自己从 prepared definition 手工挑字段
- runtime 不需要同时理解 `track_config` 和 `budget` 才能完成启动预检

### `startup_bootstrap` 的外部接口

`startup_bootstrap` 不应该对外暴露通用的 `StartupBootstrapResult`。

更合适的外部接口是一个单动作入口，例如：

```rust
pub(super) async fn complete_startup(
    runtime: &ServerRuntime,
    receiver: &mut mpsc::Receiver<UserDataEvent>,
    startup_cutoff: DateTime<Utc>,
) -> Result<()>;
```

如果模块内部需要中间类型，它们保持私有即可。

私有中间类型按作用域拆开：

- `TrackStartupProbe`
  - track / instrument 级原始探测结果
  - 允许暂时持有 `Position`、`ExchangeOrder`、`AccountCapacitySnapshot`
- `TrackStartupSeed`
  - 写入 runtime 时使用的归一化结果
  - 使用 `PositionObservation`、`OrderObservation`
- `AccountCapacitySeed`
  - 供 `account_margin_guard` 初始化的 instrument keyed snapshot 集合

这样 raw exchange DTO 不会越过 `startup_bootstrap` 模块边界。

## 启动顺序

调整后的启动主线为：

1. `assembly` 完成静态装配，创建 `ServerRuntime`
2. runtime `start()` 先订阅 user data
3. runtime 获取 `server_time` 作为 cutoff
4. runtime 调用 `startup_bootstrap::complete_startup(...)`
5. `startup_bootstrap` 按 startup definition 查询每个 track 的：
   - `position`
   - `open_orders`
   - `account_capacity_snapshot`
6. `startup_bootstrap` 用 startup definition 计算当前 `required_additional_notional`
7. `startup_bootstrap` 若发现 `required_additional_notional > snapshot.max_increase_notional`，启动失败
8. `startup_bootstrap` 若通过，则：
   - 写入 `account_margin_guard`
   - 应用 `sync_exchange_state(...)`
   - 回放 cutoff 之后的 buffered user data
9. runtime 进入 live user task

这样，启动探测、保证金预检、runtime 初始 live state、account guard seed 和 startup replay 都归同一个 owner。

## 错误语义

- 任何一个 track 的启动 probe 失败，整个服务启动失败
- 任何一个 track 的保证金预检失败，整个服务启动失败
- 错误文案继续保留 track 级上下文：
  - `track_id`
  - `symbol / instrument`
  - `required`
  - `available`

但“如何重试、在哪一步失败”由 runtime bootstrap 统一产生日志和错误上下文，不再分散在 `assembly` 和 `startup_sync` 两边。

## 与现有设计的关系

- 本文延续了 [Track Definition 与 Runtime 边界设计](2026-04-09-track-definition-runtime-boundary-design.md) 中“prepared definition 由 application 拥有”的前提
- 本文不改变 [每个 Track 杠杆启动设置设计](2026-04-17-track-leverage-startup-design.md) 里对 startup leverage 的归属；杠杆设置仍然是装配期静态控制动作
- 本文只把“实时交易所状态接管”从装配期剥离到 runtime-owned bootstrap

## 测试策略

### `server/src/runtime`

- `start()` 保持：
  - `subscribe_user_data -> get_server_time -> startup_bootstrap::complete_startup -> live apply`
- `startup_bootstrap` 会同时完成：
  - 保证金预检
  - account margin guard seed
  - exchange state apply
  - buffered replay
- 现有持仓已经覆盖部分 `max_notional` 时，启动允许通过
- 剩余 required notional 超过 `max_increase_notional` 时，启动失败

### `server/src/assembly.rs`

- 不再测试装配期 live `position` / `account_capacity_snapshot` 查询
- 继续测试：
  - startup leverage 顺序
  - `exchange_info` / rules 装配
  - 本地状态恢复

### 回归要求

- `assembly` 不再在装配期查询 `get_position`
- `assembly` 不再通过构造函数向 runtime 注入 startup `account_capacity_snapshots`
- runtime 不再通过 server 手工拼装的 DTO 获取 startup 静态输入
- raw `Position` / `ExchangeOrder` 不再跨出 `startup_bootstrap` 模块边界
- `startup_sync.rs` 删除，或降为不访问端口的私有 apply helper

## 落地拆分

后续实现建议按下面顺序推进：

1. 在 definition owner 上新增 `TrackStartupDefinition`，固定暴露 `track_id / instrument / required_additional_notional(...)`
2. 新增 `startup_bootstrap::complete_startup(...)`，让它统一拥有 probe / preflight / apply / replay
3. 把动态 probe、保证金预检和 guard seed 从 `assembly` 搬到 `startup_bootstrap`
4. 删除 `ServerRuntime::with_account_capacity_snapshots(...)` 这条启动注入路径
5. 删除 `startup_sync.rs`，或把它降为 `startup_bootstrap` 内部私有 apply helper
6. 把相关测试从 `assembly` 迁到 runtime 启动测试

## 实施原则

- 不让 runtime 通过测试专用的 `manager()` 读取静态定义
- 不把完整 `PreparedTrackRegistry` 暴露给 runtime
- 不让 server 自己手工维护第二份 startup 字段切片知识
- 不把 raw exchange DTO 暴露成 bootstrap 模块外的公共结果类型
- 先补 runtime 启动验收测试，再做边界调整
