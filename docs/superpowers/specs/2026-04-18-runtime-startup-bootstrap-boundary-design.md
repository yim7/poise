# Runtime 启动 Bootstrap 边界设计

> 本文描述当前启动边界。早期 implementation plan 中的历史命名保留为执行记录，不作为新的概念归属依据。

## 背景

启动流程需要同时处理两类知识：

- 静态启动准备：track definition、exchange rules、startup leverage、runtime 装配。
- 动态实时接管：position、open orders、account capacity、cutoff 之后的 buffered user data。

如果装配期和 runtime 启动期各自查询一部分交易所实时状态，会出现两个问题：

- 启动期“需要哪些实时状态、如何重试、哪份结果算权威”被拆到多个模块。
- 保证金预检需要静态定义，如果 runtime 只能反向穿透 manager/query 层读取字段，边界会变得很脆。

因此，静态准备和动态接管需要分层：`assembly` 做静态装配，`startup_bootstrap` 统一拥有启动实时探测和接管时序。

## 目标

- 启动期实时交易所状态探测只由一个模块拥有。
- 保留外部启动语义：启动预检失败仍直接终止服务。
- 保证金预检、exchange state apply、account margin guard seed、buffered replay 共享同一份启动探测结果。
- runtime 不通过测试专用入口或读侧接口反向读取 `TrackManager` 内部状态。
- runtime 不接收完整 `TrackDefinitionRegistry`，只接收启动所需的窄输入。

## 非目标

- 不修改 user data cutoff / replay 的外部顺序语义。
- 不把 startup leverage 设置搬进 runtime bootstrap；它仍是装配期静态控制动作。
- 不重做 account capacity 的 account-level 共享模型。
- 不修改 HTTP / WebSocket / TUI 协议。

## 当前设计

采用：

```text
assembly 静态装配
  -> RuntimeStartupDefinition
      -> startup_bootstrap 动态接管
```

### `server/src/assembly.rs`

`assembly` 继续负责：

- track 唯一性校验。
- startup leverage 设置。
- `exchange_info` / rules 查询。
- 构建 `TrackManager`。
- 恢复本地持久化状态。
- 为 runtime 构造启动所需的 `RuntimeStartupDefinition`。

`assembly` 不负责：

- 查询 live `position`。
- 查询 `open_orders`。
- 查询 `account_capacity_snapshot`。
- 做启动保证金预检。
- 给 runtime 直接注入 startup account capacity snapshots。

### `server/src/runtime::RuntimeStartupDefinition`

`RuntimeStartupDefinition` 是 server runtime 内部的启动输入，不是新的领域 definition。

它由两部分组成：

- `core::track::TrackDefinition`
- `RuntimeStartupCapacityMode`

它暴露的是启动所需行为，而不是让调用方重新拼字段：

```rust
impl RuntimeStartupDefinition {
    pub(crate) fn track_id(&self) -> &TrackId;
    pub(crate) fn instrument(&self) -> &Instrument;
    pub(crate) fn required_additional_notional(&self, position_qty: f64) -> f64;
    pub(crate) fn exposure_from_position_qty(&self, position_qty: f64) -> Exposure;
    pub(crate) fn startup_capacity_mode(&self) -> &RuntimeStartupCapacityMode;
}
```

这样：

- `TrackDefinition` 继续拥有 `position_qty -> exposure / required notional` 的领域计算。
- `RuntimeStartupCapacityMode` 表达 server runtime 的启动容量来源。
- runtime 不需要接收完整 registry，也不需要理解 config schema。

### `server/src/runtime/startup_bootstrap.rs`

`startup_bootstrap` 统一负责：

- 按统一 retry 语义探测启动期实时状态。
- 查询 position / open orders / account capacity。
- 做保证金预检。
- 把 startup probe 结果转换成 runtime 可用的启动 seed。
- 写入 `account_margin_guard`。
- 调用 `observation_service.sync_exchange_state(...)`。
- 回放 cutoff 之后的 buffered user data。
- seed startup pending submit effects。

它拥有以下知识：

- 启动期需要哪些实时状态。
- 这些状态的获取顺序和 retry 策略。
- 哪份探测结果被视为启动真相。

### `server/src/runtime/exchange_state.rs`

`exchange_state.rs` 是 runtime 私有的中性 helper 模块，负责：

- `Position` / `ExchangeOrder` 到 observation 的翻译。
- steady-state user data 的吸收。
- startup bootstrap、reconcile、user task 之间共享的 exchange state 应用细节。

它不拥有：

- 启动期需要探测哪些端口。
- startup probe / preflight / replay 的顺序。
- 哪份结果算 startup 真相。

## 启动顺序

当前启动主线是：

1. `assembly` 完成静态装配，创建 `ServerRuntime`。
2. runtime `start()` 先订阅 user data。
3. runtime 获取 `server_time` 作为 cutoff。
4. runtime 调用 `startup_bootstrap::complete_startup(...)`。
5. `startup_bootstrap` 按 `RuntimeStartupDefinition` 查询每个 track 的实时状态。
6. `startup_bootstrap` 用 `required_additional_notional(position_qty)` 计算当前还需要的名义金额。
7. 如果 required notional 超过可增加容量，启动失败。
8. 如果通过，则写入 guard、同步 exchange state、回放 buffered user data、seed startup pending effects。
9. runtime 进入 live user task。

这样，启动探测、保证金预检、runtime 初始 live state、account guard seed、startup replay 和 startup pending seed 都归同一个 owner。

## 错误语义

- 任一 track 的启动 probe 失败，整个服务启动失败。
- 任一 track 的保证金预检失败，整个服务启动失败。
- 错误文案保留 track 级上下文：
  - `track_id`
  - `symbol / instrument`
  - `required`
  - `available`

但“如何重试、在哪一步失败”由 runtime bootstrap 统一产生日志和错误上下文，不再分散在装配期和 runtime apply helper 中。

## 与其他设计的关系

- 静态定义归属遵循 [Track Definition 与 Runtime 边界设计](2026-04-09-track-definition-runtime-boundary-design.md)：`TrackDefinition` 属于 core，`TrackRuntime` 属于 engine，registry 属于 application。
- startup leverage 仍遵循 [每个 Track 杠杆启动设置设计](2026-04-17-track-leverage-startup-design.md)：杠杆设置是装配期静态控制动作。
- 本文只定义“实时交易所状态接管”的 owner。

## 测试策略

### `server/src/runtime`

- `start()` 保持：
  - `subscribe_user_data -> get_server_time -> startup_bootstrap::complete_startup -> live apply`
- `startup_bootstrap` 同时完成：
  - 保证金预检
  - account margin guard seed
  - exchange state apply
  - buffered replay
  - startup pending submit seed
- 现有持仓已经覆盖部分 `max_notional` 时，启动允许通过。
- 剩余 required notional 超过可增加容量时，启动失败。

### `server/src/assembly.rs`

- 不测试装配期 live `position` / `account_capacity_snapshot` 查询。
- 继续测试：
  - startup leverage 顺序
  - `exchange_info` / rules 装配
  - 本地状态恢复

## 回归要求

- `assembly` 不在装配期查询 `get_position`。
- `assembly` 不通过构造函数向 runtime 注入 startup account capacity snapshots。
- runtime 不通过 server 手工复制的 config 字段获取 startup 静态语义。
- raw `Position` / `ExchangeOrder` 不暴露成 runtime 私有 helper 之外的公共结果类型。
- `exchange_state` 可以共享 observation translation，但不能重新拥有 startup 时序。
- 启动保证金计算必须通过 `TrackDefinition` / `RuntimeStartupDefinition` 的语义方法完成。
