# Poise 系统构建说明

本文是 `docs/` 下唯一长期文档。它回答两个问题：如果从零构建 Poise，应该按什么闭环和边界搭起来；以及当前实现有哪些仍成立的运行语义。它是架构叙事和构建顺序，不替代代码里的 schema、trait 和测试。历史 plan、旧 spec、旧协议说明和阶段性评审不再作为文档保留；如果设计继续变化，应直接更新本文或 README。

## 事实源

- 架构约束和构建顺序由本文叙述；具体字段、接口和行为细节以代码和测试为准。
- 配置 schema 的事实源是 `server/src/config.rs`。
- HTTP / WebSocket DTO 的事实源是 `protocol/src/lib.rs` 和对应序列化测试。
- HTTP 路由事实源是 `server/src/http.rs`，WebSocket 推送事实源是 `server/src/websocket.rs`。
- 策略和风险计算事实源是 `core/src/strategy.rs`、`core/src/risk.rs` 和 `core/src/track.rs`。
- 运行时状态机和执行器事实源是 `engine/src/runtime.rs`、`engine/src/manager.rs` 和 `engine/src/executor/`。

## 分层边界

Poise 目前按知识边界分层，而不是按启动步骤分层。

- `core/` 拥有领域概念：`TrackId`、`Venue`、`Instrument`、`TrackDefinition`、`TrackConfig`、风险限制和领域事件。
- `engine/` 拥有单个 track 的运行时状态、目标计算、执行规划、恢复和对账逻辑。
- `application/` 拥有用例层服务、读模型、持久化 port、`TrackDefinitionRegistry` 和 session effect 队列。
- `storage/` 是 SQLite 适配层，只实现 application 定义的持久化 port。
- `exchanges/binance/`、`exchanges/bybit/`、`exchanges/hyperliquid/` 和 `exchanges/okx/` 各自封装交易所协议、鉴权、REST / WebSocket 映射和窄控制 helper。
- `server/` 拥有配置解析、启动装配、交易所选择、HTTP / WebSocket、runtime task 和进程级状态。
- `protocol/` 拥有 `server` 与 `tui` 共享的 wire DTO。
- `tui/` 是本地值守界面，只依赖 HTTP / WebSocket 协议。

当前静态 track 定义链路是：

```text
server::config::TrackSpec
  -> core::track::TrackDefinition
  -> application::TrackDefinitionRegistry
  -> engine::runtime::TrackRuntime
```

`TrackSpec` 只表达 TOML / serde schema。`TrackDefinition` 是核心静态定义 owner。`TrackDefinitionRegistry` 是 application 层索引，不重新定义默认值或领域计算。`TrackRuntime` 是 `TrackDefinition + dynamic state`，外部通过语义方法读取静态字段，不直接穿透内部 definition 字段。

## 从零构建闭环

从零搭建 Poise 时，不按 crate 先后堆半成品，而是按可运行闭环推进。每个闭环都要包含真实输入、核心决策、状态变化、可观察输出和局部测试。

1. 领域内核：先在 `core/` 定义 `Venue`、`Instrument`、`TrackId`、`TrackConfig`、`TrackDefinition`、风险限制、领域事件和策略曲线计算。验收方式是纯函数测试：配置校验、价格带目标、风险 cap 和终止规则都不依赖数据库、HTTP 或交易所。
2. 单 track 决策闭环：在 `engine/` 建 `TrackRuntime`、`reconciler` 和最小 `manager`。输入是静态定义、当前仓位、策略价格和风险状态；输出是 `desired_exposure`、运行时状态和领域事件。此时不接真实交易所，先用测试证明自动、暂停、手动、带外和风险 cap 的目标计算成立。
3. 执行规划闭环：在 `engine/src/executor/` 建 `ExecutorState`、交易规则输入、binding ledger 和 policy。输入是当前仓位、执行目标、盘口和 exchange rules；输出是 `TrackEffect`，不是直接下单。测试要覆盖 catch up、reduce-only maker、订单复用、撤单、恢复和异常状态。
4. mutation 闭环：用 `TrackMutationFrame` 把一次决策前后的 runtime 状态包起来，`manager` 负责提交或回滚当前进程状态。验收点是：没有 effect 的 live-only tick 不做持久写；有事件、effect 或 durable 控制状态变化时才进入 application 写路径。
5. application / storage 闭环：`application/` 定义 mutation、query、read model、effect queue 和持久化 port；`storage/` 只实现这些 port。SQLite 持久化 durable 控制状态、领域事件、effect journal 和 PNL 明细；读模型从当前 config registry、live runtime view 和持久记录投影出来。
6. server 装配闭环：`server/` 读取实例目录配置，构造 `TrackDefinitionRegistry`，准备 SQLite，连接交易所 ports，设置启动期杠杆，加载交易规则，把 tracks 注册进 `TrackManager`，再启动 runtime task 和 HTTP / WebSocket。验收点是 config、assembly、startup preparation 和 projector 的局部测试。
7. 交易所闭环：每个 `exchanges/<venue>/` 先在私有模块完成签名、REST / WebSocket DTO、symbol 规则、账户模式和错误映射，再通过 `connected.rs` 输出 `ExchangePorts`。新增交易所只把 venue 分支接到 core/server/assembly，不把交易所私有字段上移到共享领域层。
8. 协议和界面闭环：`protocol/` 定义 wire DTO，`server` projector 负责从 read model 投影，`tui/` 只消费 HTTP / WebSocket。TUI 可以合并 `track_detail_changed` 和 `track_live_view_changed`，但不能重新实现目标计算。
9. 工具闭环：workbench 或其他工具只能编辑配置、展示 read/live model 或调用公开命令。它们不成为运行时事实源，也不绕过 server / protocol 边界。

这个顺序把最难变的领域语义先固定，再逐步接入可替换的外部世界。它避免两个常见问题：按技术层拆出一批不可验证的半成品；或者让交易所、UI、配置层反向决定 engine 的运行语义。

## 配置与身份

服务端只支持单实例连接单交易所。`[exchange]` 决定当前实例的 `venue`，`[[tracks]]` 不再配置 venue。

当前重要约束：

- `exchange.venue` 当前支持 `binance`、`bybit`、`hyperliquid` 和 `okx`。
- Binance / Bybit 使用 `api_key` 和 `api_secret`；Hyperliquid 使用 `private_key` 和 `wallet_address`，可选 `vault_address`；OKX 使用 `api_key`、`api_secret` 和 `passphrase`。
- `track_id` 是显式配置的稳定业务标识。
- `Instrument` 由 `exchange.venue()` 和 track `symbol` 组成。
- `symbol` 是当前交易所的合约标识：Binance / Bybit 使用 `BTCUSDT` 这类合约符号，Hyperliquid 默认 perpetuals 使用 `BTC`、`ETH` 这类 coin 名称，Hyperliquid HIP-3 builder-deployed perpetuals 使用 `xyz:CBRS` 这类 `{dex}:{coin}` wire name，OKX SWAP 使用 `BTC-USDT-SWAP` 这类 instrument id。
- 同一实例内 `track_id` 必须唯一。
- 同一实例内 `Instrument { venue, symbol }` 必须唯一。
- `leverage` 是 server-owned startup-only 配置，不进入 `TrackDefinition` 或 `TrackConfig`。
- 未配置 `leverage` 时默认 `10`。
- `out_of_band_policy` 是每个 track 的单字段枚举配置，默认 `freeze`；`flatten` 可以用简写，也可以用对象形式配置 `trigger` 和 `recover`。
- `risk_acquisition` 是每个 track 的参数子表，TOML 写作 `[tracks.risk_acquisition]`，归属于它前面最近的 `[[tracks]]`；省略子表时使用默认参数，不支持 `risk_acquisition = { ... }` 行内对象形式。

当前交易所接入只覆盖 Poise 运行需要的合约能力。Hyperliquid 适配器只接入 perpetuals，不提供 spot、提现、划转、TWAP 或 vault 运维功能；可选 `vault_address` 只作为 Hyperliquid action 签名上下文进入适配器内部。Hyperliquid 默认 perpetuals 和 HIP-3 perpetuals 可以在同一实例中混配，适配器按 symbol 解析默认 dex 或 `{dex}:{coin}` HIP-3 dex 上下文；账户资产按 Hyperliquid 账户级共享余额处理。Hyperliquid standard mode 使用账户级 perps `clearinghouseState.withdrawable` 作为可用保证金口径；unified account / portfolio margin 使用 `spotClearinghouseState` 的 USDC 可用余额作为启动容量口径。OKX 适配器只接入 `SWAP` 永续合约，REST 覆盖规则查询、账户摘要、持仓、open orders、下单、撤单、cancel all 和启动期杠杆设置；WebSocket 覆盖 ticker、mark price、订单更新、成交 PNL、持仓更新、断线重连和重订阅。OKX 当前只支持 `cross` 保证金模式和 `net` 持仓模式，不提供 spot、期权、划转、提现或资金账户操作。

`TrackSpec::to_track_definition(venue)` 负责把配置字段投影为 `TrackDefinition`，并触发 core 层策略、风险和默认值校验。

## 交易所接入边界

交易所 crate 的公共连接入口是 `connect(config) -> ExchangePorts`。`ExchangePorts` 只是短生命周期的连接结果容器，用于一次性返回 Poise runtime 需要的几个能力 port；它不进入 runtime、read model 或领域层。

当前有效 port 边界：

- `ExecutionPort`：下单、撤单、cancel all、持仓查询、open orders 查询。
- `MarketDataPort`：订阅当前 track 需要的价格流。
- `AccountPort`：账户容量快照和 user data 订阅。
- `AccountSummaryPort`：账户摘要，用于 account monitor 和部分启动资金策略。
- `MetadataPort`：交易规则和交易所时间。

新增交易所时按下面顺序接入：

1. 在新的 `exchanges/<venue>/` crate 内实现 REST / WebSocket 私有 client、协议模型、签名和 mapper。交易所字段、账户模式、symbol 规则和错误 envelope 不上移到共享层。
2. 在 `exchanges/<venue>/src/connected.rs` 把私有 client 组装为 `ExchangePorts`。如果 REST / WS client 方法已经直接匹配 port 语义，可以直接为 client 实现 port trait；只有在需要错误语义转换、symbol 转换、REST+WS 组合或测试专用缺省行为时才保留 wrapper。
3. 在 `core::track::Venue`、`server/src/config.rs` 和 `server/src/assembly.rs` 增加 venue / 配置 / connect 分支。`assembly` 拿到 `ExchangePorts` 后立刻拆给启动准备、account monitor 和 runtime。
4. 在 `server/src/exchange_startup.rs` 增加启动期窄控制：`SymbolLeverageSetter` 用于按 track 设置 startup-only leverage。启动恢复统一使用 instrument 对应的 available balance 乘以 track startup leverage 计算可增加名义金额，避免按交易所维护两套启动容量策略；单保证金账户可以直接复用账户级 available，OKX 这类多币种账户按 symbol 的 quote asset 取余额。
5. 补最小测试：配置解析、`connected::tests::`、关键 mapper、`exchange_startup::tests::`，以及必要的 `assembly::tests::`。如果新增交易所只改某个 crate，优先跑该 crate 的最小测试，再按影响面扩大。

## 启动与运行时

启动主路径：

1. `server/src/main.rs` 读取 `--instance-dir` 下的 `config.toml`。
2. `state_bootstrap::prepare_state_repository(...)` 初始化 SQLite，构造 `TrackDefinitionRegistry`，并检查当前配置与持久业务状态是否兼容。
3. `assembly::assemble(...)` 构造交易所连接，按 track 执行 startup-only 杠杆设置，加载交易所规则，并把每个 `TrackDefinition` 注册进 `TrackManager`。
4. `RuntimeStartupDefinition` 把 `TrackDefinition` 和 startup leverage 组合成 server runtime 内部输入。
5. 启动恢复用 instrument 对应的 available balance 和 track startup leverage 计算可增加名义金额。
6. `ServerRuntime::start()` 完成 live exchange state bootstrap，然后启动 market data、user data、recovery、effect worker、account monitor 和 health 相关 task。

启动遇到持久状态与当前配置不兼容时会失败，操作者应显式处理实例目录或数据库。

## 策略与执行语义

策略价格：

- `strategy_price = book_mid = (best_bid + best_ask) / 2`。
- `mark_price` 只用于风险、展示和价格执行 gate，不参与目标仓位计算。
- 自动执行使用盘口一档定价：`Buy -> best_ask`，`Sell -> best_bid`。

曲线参数：

- `shape_family` 支持 `linear`、`inertial`、`responsive`。
- 三种曲线都围绕价格带中点对称解释。
- `long_exposure_units` 和 `short_exposure_units` 决定目标曲线整体偏多或偏空。

执行门槛：

- `min_rebalance_units` 表示触发下一次执行动作的最小目标变化。
- 没有活动生命周期时，参考点是 `current_exposure`。
- 存在 `SubmitPending` 或 `Working` 时，参考点是当前执行目标。

风险暴露获取：

- `risk_acquisition` 默认启用，只约束增加风险暴露；降低风险暴露时直接重新评估并优先执行到曲线目标。
- `desired_exposure` 是策略曲线理论目标，用于减仓规划和展示。
- `risk_release_frontier` 是风险门控已经释放的新增风险边界，只限制继续增加风险，不是减仓目标。
- `execution_target_exposure` 由 `current_exposure`、`desired_exposure` 和 `risk_release_frontier` 派生，是 executor 当前真正追随的目标。
- 从零启动或进入新的同方向 backlog 时，系统先释放 `initial_ratio` 对应的目标暴露；如果曲线目标不小于 `min_rebalance_units`，至少释放一个最小调仓单位。
- 增加风险暴露后，系统记录 `anchor_price` 和 `anchor_curve_target`。只有曲线目标相对锚点继续走出 `advantage_steps * min_rebalance_units`，才释放一部分 backlog。
- 如果同一个 anchor 等待达到 `stale_release_minutes`，即使价格没有走到优势阈值，也允许释放一部分 backlog；设为 `0` 表示关闭时间释放。
- 每次释放量由 backlog、`catchup_ratio`、`min_release_steps` 和 `max_release_steps` 共同决定：先按 backlog 比例计算，再限制在最小/最大释放单位之间。
- 运行时公开 `risk_release_frontier`、`backlog_units`、`next_advantage_price` 和 `next_release_units` 等观测字段；TUI 的 Execution 区会展示释放边界、backlog 和下一次释放信息。

风险释放不可变式：

- `reconciler` 先计算理论 `desired_exposure`，再让 `risk_exposure_gate` 计算 `risk_release_frontier`。gate 不覆盖 desired。
- 没有 frontier 时，`execution_target_exposure = desired_exposure`。
- frontier 和 desired 异号时，执行目标先回到 `0`，不能在一次 reconcile 里直接反向增加风险。
- desired 回到 frontier 内侧时，退出门控，执行目标直接回到 desired，用于降低风险。
- desired 仍在 frontier 外侧，且当前仓位还没到达 frontier 时，执行目标最多到 frontier，不继续释放下一段 backlog。
- 当前仓位到达或穿过 frontier，但没有超过 desired 时，frontier 推进到当前仓位；这是承认已经获得的仓位，不算一次新的释放，也不重置 anchor。
- 当前仓位超过 desired 时，执行目标回到 desired，优先降低风险。
- 增加风险不规划 increase maker，统一由 `CatchUp` 追随 `execution_target_exposure`。
- `CurveMaker` 只保留降低风险方向的 reduce-only maker，并且基于 `desired_exposure` 判断。

释放量计算：

```text
target_units = abs(desired_exposure)
initial_units = min(target_units, max(target_units * initial_ratio, min_rebalance_units))
backlog_units = abs(desired_exposure - risk_release_frontier)
release_units = clamp(backlog_units * catchup_ratio, min_release_steps * min_rebalance_units, max_release_steps * min_rebalance_units)
release_units = min(release_units, backlog_units)
```

价格优势释放和时间释放使用同一套 `release_units`。只要还有同方向 backlog，且价格优势满足，就把 frontier 向 desired 推进，并重置 `anchor_price`、`anchor_curve_target` 和 `anchor_started_at`；因此即使 stale 已经接近到期，价格优势释放也会让 stale 重新计时。时间释放用于打破等待：当前 anchor 等待达到 `stale_release_minutes` 后可以释放一段，即使当前仓位还没有吸收上一段 frontier；但一次 stale 释放后，必须等当前仓位到达或穿过新的 frontier，重新获得进展，stale 才会再次可用。frontier 推进后由 `CatchUp` 追新的 `execution_target_exposure`。如果当前仓位已经到达或穿过 frontier，gate 会先 ratchet 承认已获得的仓位。价格继续移动产生的新 desired 变化只进入同一个 backlog，不单独立即释放。

风险释放示例：

```text
current_exposure = +3.9
desired_exposure = +4.263
risk_release_frontier = +3.9
backlog_units = +0.363
execution_target_exposure = +3.9
```

这表示策略理论目标已经到 `+4.263`，但风险门控还没有释放新增风险。executor 当前不追 `+4.263`，而是保持 `+3.9`。等价格优势或时间释放把 `risk_release_frontier` 往外推进后，`CatchUp` 才追新的 `execution_target_exposure`。

带外策略：

- `freeze`：离开主带后冻结目标，回到主带后自动恢复。
- `flatten`：离开主带后按 trigger / recover 规则自动压到 `0` 并恢复。
- `terminate`：离开主带后进入终态，不自动恢复。

人工命令：

- `pause` 暂停自动控制。
- `resume` 从暂停或手动平仓恢复。
- `flatten` 写入人工目标 `0`，进入 `manual_flattening`，必须用 `resume` 清除。
- `terminate` 进入终态，目标收敛到 `0`，不会自动恢复。

## 持久化与读模型

SQLite 文件位于 `<instance-dir>/.data/poise-server.sqlite`。

持久化边界保存 durable 业务事实，不保存完整当前配置定义，也不把 read model 当成事实源。读侧需要静态定义时，从当前配置构造出的 `TrackDefinitionRegistry` 读取。

当前持久化事实：

- `track_control_state`：只保存可跨进程恢复的控制状态，例如自动、暂停、手动目标和终止。自动运行中的 `AcquiringRiskExposure`、冻结确认、flatten pending 等运行阶段在写入持久控制状态时折回对应的 durable 控制模式。
- `track_events`：保存领域事件，用于审计和读侧更新。
- `track_effects`：保存 effect journal、执行状态、重试次数和错误摘要。它是 effect worker 的持久队列事实，不是交易所订单事实的完整替代。
- `track_pnl_records`：保存可归属到 track 的成交盈亏、交易手续费和资金费明细。
- `persisted_track_presence`：读模型辅助表，只用于 listing 和 updated-at 元数据。
- `account_monitor_state`：账户监控的跨进程基线和最近观测。

当前进程事实：

- `TrackMutationFrame` 是一次 mutation 的当前进程快照，用于提交、回滚和 durable-write 判断；它不是持久文档。
- `ExecutorState`、binding ledger、recovery anomaly、risk acquisition gate、`desired_exposure` 和 live market fields 属于 runtime 当前会话状态。需要展示时从 live runtime 投影；重启后根据 durable 控制状态、当前配置、交易所快照和新行情重新建立。
- `TrackRuntimeView`、read model 和 protocol DTO 都是投影结果。它们可以被查询和推送，但不能反向作为 engine 或 persistence 的事实源。

读模型链路：

```text
TrackDefinitionRegistry + TrackRuntimeView + persisted events/effects
  -> application read model
  -> server projector
  -> protocol DTO
```

`server` projector 不直接暴露 engine 内部运行时结构。HTTP / WebSocket 对外只推投影后的读模型。

Track PNL 统计只把本地可归属到 track 的明细作为事实来源：

- `track_pnl_records` 记录每笔成交已实现盈亏、交易手续费，以及可归属到 symbol/track 的资金费。
- `pnl_asset` 只存在于 HTTP / WebSocket 公开读模型，由当前 track 的 instrument 推导；PNL 明细和运行时统计不重复保存这份可推导信息。
- 非 `pnl_asset` 计价的手续费不进入本地 PNL 统计。
- `TrackPnlStats` 是运行时和读模型使用的即时统计结果，不是持久化真值；启动和查询时可以从明细重新聚合。
- 当日 PNL 窗口按当前 UTC 日展示，明细是否进入当日统计由它自己的发生时间决定。
- 订单成交更新只更新订单/执行器状态；PNL 明细作为独立记录写入，避免订单状态和财务统计互相隐藏规则。
- 资金费如果无法归属到某个 track，就不进入 track PNL 统计。
- HTTP / WebSocket 公开读模型使用 `pnl` 字段，不再用旧的 `ledger` 命名承载 PNL 统计。

## HTTP / WebSocket

当前公开入口：

- `GET /health`
- `GET /account`
- `GET /tracks`
- `GET /tracks/:id`
- `POST /tracks/:id/commands`
- `GET /debug/tracks/:id/diagnostics`
- `GET /ws`

协议字段不再单独维护长文档。修改协议时必须更新 `protocol/src/lib.rs` 和序列化测试；需要说明语义时，在本文只保留高层约束。

WebSocket 当前推送：

- `track_list_item_changed`
- `track_detail_changed`
- `track_live_view_changed`
- `account_summary_changed`

`/debug/...` 接口是排查入口，不是稳定产品协议。

## 文档维护规则

- README 只保留项目入口、启动方式和文档导航。
- 本文保留当前架构、从零构建路径、边界和运行语义。
- 不再保留历史 plan/spec 作为主文档；过时讨论直接删除。
- 如果某个文档不能明确回答“从零如何构建当前系统”或“当前实现如何工作”，默认不保留。
