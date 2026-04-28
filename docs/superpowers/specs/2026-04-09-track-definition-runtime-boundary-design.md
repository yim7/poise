# Track Definition 与 Runtime 边界设计

> 本文描述当前设计。早期 implementation plan 里的历史命名保留为执行记录，不作为新的概念归属依据。

## 背景

track 的静态定义、运行态状态、读模型和持久化事实如果混在一起，会带来几个直接问题：

- 同一份 track 静态知识散落在 `server`、`application`、`engine` 多层。
- runtime snapshot 或 read model 容易反向携带 definition 字段，导致 query 和恢复逻辑依赖运行时细节。
- 默认值、风控校验、启动预算换算等语义如果由调用方拼装，会放大后续改动成本。
- 配置文件 schema、领域定义和运行态对象如果都叫 definition，读代码时很难判断谁拥有不变量。

## 目标

- 配置文件规格、静态领域定义和运行态对象各自有明确 owner。
- 默认值补齐、合法性校验和静态计算集中在领域定义 owner 内。
- `TrackRuntime` 表达“一个活着的 track”，而不是重复暴露一组静态字段。
- query/read model 只消费用例层需要的输入，不直接依赖 engine 内部结构。
- storage 不持久化完整 definition，只持久化运行事实、事件、effect、控制状态、ledger 和最小 track presence。

## 分层结论

当前固定路径是：

```text
server::config::TrackSpec
  -> core::track::TrackDefinition
      -> engine::runtime::TrackRuntime
```

### `server`

`server` 拥有外部世界和进程装配：

- `TrackSpec` 表达 TOML / `serde` schema。
- `ExchangeConfig::venue()` 提供 service-level venue。
- `TrackSpec::to_track_definition(venue)` 负责把 TOML 字段映射为领域对象，并调用 `TrackDefinition::try_new(...)`。
- startup leverage、exchange wiring、HTTP/WebSocket 和 runtime task 装配仍属于 `server`。

`server` 不拥有：

- track 默认值补齐规则。
- 风控和价格区间合法性校验。
- `required_additional_notional(...)` 这类领域计算。

### `core`

`core` 拥有纯领域概念和不变量：

- `TrackId`
- `Venue`
- `Instrument`
- `TrackDefinition`
- `TrackConfig`
- `LossLimits`
- `ExchangeRules`

`TrackDefinition` 是静态 track 定义的 owner。它包含：

- `track_id`
- `instrument`
- `track_config`
- `max_notional`
- `loss_limits`
- `tick_timeout_secs`

`TrackDefinition::try_new(...)` 负责：

- 校验 `TrackConfig`。
- 补齐 `max_notional` 默认值。
- 校验 `max_notional` 和 `LossLimits`。
- 补齐 `tick_timeout_secs` 默认值。

`TrackDefinition` 也拥有静态计算：

- `curve_max_notional()`
- `effective_max_notional()`
- `required_additional_notional(position_qty)`
- `exposure_from_position_qty(position_qty)`

这些计算不应由 `server` 或 runtime 调用方重新拼公式。

### `application`

`application` 拥有用例边界和读侧组装：

- `TrackDefinitionRegistry` 保存一组 `core::TrackDefinition`，提供按 `track_id` 查询和遍历。
- `TrackReadSourceLoader` 组合静态定义、live runtime view、持久化事件/effect 和更新时间。
- `TrackReadModel` 从 `TrackReadSource` 构造外部 read model。

`TrackDefinitionRegistry` 只是用例层索引，不是静态定义语义 owner。它不重新定义默认值、风控校验或领域计算。

### `engine`

`engine` 拥有运行时状态机和交易决策：

- `TrackRuntime`
- `TrackManager`
- executor / reconciler
- mutation frame / transition
- effect 和 execution action

`TrackRuntime` 的语义是：

```text
TrackRuntime = TrackDefinition + dynamic state
```

因此 runtime 内部可以持有 `TrackDefinition`，但调用方应通过 runtime 的语义方法读取高频信息：

- `id()`
- `instrument()`
- `config()`
- `max_notional()`
- `loss_limits()`
- `exchange_rules()`

不要让调用方写 `track.definition.track_config` 这类穿透访问；如果某个计算经常出现，应放在 `TrackRuntime` 或 `TrackDefinition` 的方法上。

### `storage`

`storage` 只拥有持久化实现：

- events
- effects
- control state
- ledger state
- account monitor state
- persisted track presence

它不持久化完整 `TrackDefinition`，也不通过旧 definition 字段反推当前配置。读侧需要静态定义时，从当前 config 构造出的 `TrackDefinitionRegistry` 获取。

## Query 边界

query 的输入分成三类：

- 当前静态定义：来自 `TrackDefinitionRegistry`。
- live runtime view：来自 application/runtime service。
- 持久化读侧记录：来自 `TrackQueryStore`。

`TrackReadSource` 是 query 组装边界。它可以组合 `core::TrackDefinition`，但不应该复制一份新的完整 definition 类型。

这样可以避免：

- query 直接依赖 engine 内部 runtime 结构。
- application 为 read/startup 各自造浅层 projection。
- 修改 `TrackDefinition` 字段时需要同步维护多份 wrapper。

## Bootstrap 边界

`server::state_bootstrap` 是当前 config 到 application repository 的编排点：

1. 读取 `TrackSpec`。
2. 使用 `ExchangeConfig::venue()` 构造 `TrackDefinition`。
3. 构造 `TrackDefinitionRegistry`。
4. 打开并初始化持久化 repository。
5. 返回 query-ready repositories 与 registry。

`state_bootstrap` 不再拥有 definition 语义。它只是把 server 配置、core 定义和 application repository 组合到一起。

## 设计约束

- `Instrument` 保留 `venue + symbol`。`venue` 表达完整身份、自描述日志/持久化和未来多 venue 扩展，不用于运行时动态选择 exchange port。
- `TrackDefinition.instrument.venue` 必须来自 service-level `ExchangeConfig::venue()`。
- 不引入额外 definition input 对象，除非出现第二个真实的非 server 构造入口。
- 不为 read/startup 保留只复制字段的 definition wrapper。
- 不把 `TrackDefinitionRegistry` 下沉到 core；除非多个下层 crate 真的需要同一组定义索引不变量。

## 结果

当前设计完成后应满足：

- 静态定义只有一个领域 owner：`core::TrackDefinition`。
- 配置 schema 只有一个 server owner：`server::config::TrackSpec`。
- 用例层只有 definition registry，不拥有 definition 语义。
- runtime 是静态 definition 加动态状态，不重复展开静态字段。
- query 不直接消费 engine 内部结构。
- storage 不持久化完整 definition。
- 旧的 budget catalog、浅层 read/startup projection 和 application-owned完整 definition wrapper 不再恢复。
