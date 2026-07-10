# Poise 系统构建说明

本文是 `docs/` 下唯一长期文档。它回答两个问题：如果从零构建 Poise，应该按什么闭环和边界搭起来；以及当前实现有哪些仍成立的运行语义。它是架构叙事和构建顺序，不替代代码里的 schema、trait 和测试。历史 plan、旧 spec、旧协议说明和阶段性评审不再作为文档保留；如果设计继续变化，应直接更新本文或 README。

## 事实源

- 架构约束和构建顺序由本文叙述；具体字段、接口和行为细节以代码和测试为准。
- 配置 schema 的事实源是 `server/src/config.rs`。
- HTTP / WebSocket DTO 的事实源是 `protocol/src/lib.rs` 和对应序列化测试。
- HTTP 路由事实源是 `server/src/http.rs`，WebSocket 推送事实源是 `server/src/websocket.rs`。
- 策略曲线、风险计算和 curve-immediate benchmark 的事实源是 `core/src/strategy.rs`、`core/src/risk.rs`、`core/src/track.rs` 和 `core/src/replay.rs`。
- 在线决策、执行器、decision replay 和 evaluation 的事实源是 `engine/src/reconciler.rs`、`engine/src/manager.rs`、`engine/src/runtime.rs`、`engine/src/executor/`、`engine/src/decision_replay.rs` 和 `engine/src/decision_evaluation.rs`。

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
2. 单 track 决策闭环：在 `engine/` 建 `TrackRuntime`、`reconciler` 和最小 `manager`。输入是静态定义、当前仓位、策略价格和风险状态；`reconciler` 输出四层目标与 target-side gate 结果，executor 生成 order plan，`manager` 再组合两者，提交一份带 revision 和 `DecisionReason` 的 `InventoryDecisionSnapshot`。此时不接真实交易所，先用测试证明自动、暂停、手动、带外、风险 cap 和 acquisition gate 的目标计算成立。
3. 执行规划闭环：在 `engine/src/executor/` 建 `ExecutorState`、交易规则输入、binding ledger 和 policy。executor 只接收 risk-approved target、final execution target、盘口和 exchange rules，不读取 acquisition frontier，也不重新计算 backlog；输出是 `TrackEffect` 和执行原因，不是直接下单。测试要覆盖 catch up、reduce-only maker、订单复用、撤单、恢复和异常状态。
4. mutation 闭环：用 `TrackMutationFrame` 把一次决策前后的 runtime 状态包起来，`manager` 负责提交或回滚当前进程状态，并保证 decision、executor state、事件和 effects 来自同一次规划。验收点是：没有 effect 的 live-only tick 不做持久写；有事件、effect 或 durable 控制状态变化时才进入 application 写路径。
5. application / storage 闭环：`application/` 定义 mutation、query、read model、当前 session effect queue 和持久化 port；`storage/` 只实现这些 port。SQLite 持久化 track identity、durable 控制状态、领域事件、best-effort effect journal 和 PNL 明细；读模型从当前 config registry、live runtime view 和持久记录投影出来。effect journal 只服务审计与诊断，不承担派发或恢复。
6. server 装配闭环：`server/` 读取实例目录配置，构造 `TrackDefinitionRegistry`，准备 SQLite，连接交易所 ports，设置启动期杠杆，加载交易规则，把 tracks 注册进 `TrackManager`，再启动 runtime task 和 HTTP / WebSocket。验收点是 config、assembly、startup preparation 和 projector 的局部测试。
7. 交易所闭环：每个 `exchanges/<venue>/` 先在私有模块完成签名、REST / WebSocket DTO、symbol 规则、账户模式和错误映射，再通过 `connected.rs` 输出 `ExchangePorts`。新增交易所只把 venue 分支接到 core/server/assembly，不把交易所私有字段上移到共享领域层。
8. 协议和界面闭环：`protocol/` 定义 wire DTO，`server` projector 负责从 read model 投影，`tui/` 只消费 HTTP / WebSocket。TUI 可以合并 `track_detail_changed` 和 `track_live_view_changed`，但只能展示同一份 committed decision，不能从零散字段重新计算目标或原因。
9. 工具闭环：`engine::decision_evaluation` 是两条 lane 比较和评估指标的唯一 owner，内部组合 `core` benchmark 与真实 manager decision replay。Workbench 的 Tauri Rust 层只构造 rules fixture、场景和时间窗口，调用 evaluator，并把结果投影为工具 DTO；为图表采样曲线时也复用 `core` API。TypeScript 只负责表单、异步调用、图表坐标和展示，不复制曲线、数量、risk acquisition 或指标公式。Workbench 的 rules fixture 和模拟成交模型是显式评估假设，不是 live runtime、交易所 metadata 或持久化事实。

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
- `AccountPort`：账户容量快照、recent track PNL capability 和 user data 订阅。
- `AccountSummaryPort`：账户摘要，用于 account monitor 和部分启动资金策略。
- `MetadataPort`：交易规则和交易所时间。

`ExecutionPort` 必须区分确定失败与结果未知。`SubmitOutcomeUnknown` 只表示请求已经越过 dispatch 边界，交易所可能接受了订单，但本地没有权威 receipt；它不能被当成普通失败后直接提交替代订单。当前 session 会保留 submit effect、client order identity 和 binding 所有权，把 exchange freshness 标为需要对账，并通过完整 position/open-orders 快照或后续订单事实裁决。只有权威事实解决这笔 submit 后，队列才能释放它；`track_effects` journal 不参与这条所有权协议。

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
6. `ServerRuntime::start()` 先验证 recent PNL 恢复能力，订阅 user data，清理 inherited open orders，再从持久控制状态、PNL 明细和交易所当前仓位、规则、余额及启动窗口内事件建立 fresh session；随后启动 market data、user data、recovery、effect worker、account monitor、PNL backfill 和 health 相关 task。

启动遇到持久状态与当前配置不兼容时会失败，操作者应显式处理实例目录或数据库。

启动恢复是 fresh-session rebuild，不是 warm restart。旧 bindings、执行进度、price gate、recovery anomaly、acquisition frontier、release anchor、stale timer 和当前 session effect queue 全部作废；`track_effects` 中旧 journal 行不会重新入队或重放。交易所仓位和订单始终从交易所重新确认。

fresh session 的第一笔有效策略行情采用保守 acquisition 初始化：如果 risk-approved target 要增加当前真实仓位的风险，acquisition frontier 从当前仓位开始；如果目标降低风险，则立即允许减到 risk-approved target；跨零目标先只允许平到 `0`。这次初始化不使用 `initial_ratio`，也不会因为重启授予新增风险。`initial_ratio` 只用于同一进程 session 中后来形成的普通 acquisition episode。stale 计时从 fresh session 的第一笔有效决策开始，因此进程离线时间不计入 `stale_release_minutes`。

配置 dry-run 是启动前只读检查，入口是 `poise-server --instance-dir <dir> --dry-run`。它读取同一份 `config.toml`，连接交易所只读 port，加载账户摘要、symbol metadata 和可用的 mark price，然后输出 JSON 格式的 `ConfigDryRunResponse`。dry-run 不初始化 SQLite，不执行 startup-only 杠杆设置，不下单，不订阅 market data 或 user data，也不启动 runtime task。输出中的配置解释和容量估算只是当前配置与交易所 metadata/account snapshot 的读模型，不写入持久化，也不作为 engine、PNL 或恢复流程的事实源。

dry-run 对每个 track 解释 native quantity 语义：base 合约按价格带中心把 `notional_per_unit` 换算为 base quantity；inverse 合约按 `ctVal/contract_notional` 换算为 contracts，并把 unit 面值解释为 USD。loss limit 资产来自当前交易规则的 settlement asset。inverse 容量估算使用 settlement asset 可用余额、mark price、startup leverage 和 contract notional 估算最大 contracts 与 USD 面值；缺 `ctVal/contract_notional`、缺 settlement asset 余额、缺 mark price，或 unit native quantity 低于 `min_qty` 时，dry-run 应给出包含 track/symbol、字段名和计算值的错误。

运行中的 position/open-orders REST 对账使用进程内 revision guard。发起组合查询前同时捕获 track 的 `exchange_observation_revision` 和 current-session effect queue revision；网络请求返回后，只有两个 revision 都未变化才允许应用。如果期间 WebSocket 或 effect 生命周期已经写入更新，旧 REST 快照会被丢弃并重新查询，有限次重试仍无法得到同一 revision 的完整快照时显式失败。这样 REST 慢响应不能覆盖更新的 WS 仓位、订单事实或仍待裁决的 submit 所有权。

## 策略与执行语义

### 回放与评估

`core::replay` 是 curve-immediate benchmark，不是在线决策回放。它不连接交易所、不读取持久化，也不运行 manager、risk acquisition 或 executor；每个价格点直接计算曲线目标，变化达到 `min_rebalance_units` 时按简化 taker 成交假设立即跟到目标。它复用 `ExchangeRules` 的 native quantity、notional 和 settlement fee 换算，用于回答“如果立即跟随曲线，仓位、换手、名义和费用基准是什么”。对于 inverse contracts，unit native quantity 来自 `notional_per_unit / contract_notional`，手续费换算到 settlement asset。

`engine::decision_replay` 才是 delayed decision path 的确定性 runner。它把带时间戳的行情、仓位、订单事实和 restart action 输入真实 `TrackManager -> reconciler -> executor` 路径，再用小型确定性交换所解释公开 effects。每个 step 报告 manager 真正 committed 的 `InventoryDecisionSnapshot`、submit/cancel/fill、实际 release trigger 和模拟仓位；稳定 order reference 只属于报告语义。`ImmediateFullAtOrderPrice` 只会在订单已经 marketable 时合成全成，`NeverFill` 只保留 working order；两者都不是对排队、滑点或成交概率的预测。

`engine::decision_evaluation` 用同一份 `TrackDefinition`、`ExchangeRules`、初始仓位和时间窗口组合 curve-immediate benchmark 与 delayed decision replay，统一计算 tracking error、backlog、authorized-but-unfilled gap、release、成交价格、换手、费用、最大名义、保证金和 restart sensitivity。Workbench 的 Rust 后端负责构造明确的 rules fixture 与场景、调用 evaluator 并投影 DTO；TypeScript 不提供计算 fallback。工具结果是离线评估，不反向写入 runtime 或 persistence。

### 在线决策链

每次有效决策都使用同一条语义链：

```text
policy_target
  -> risk_approved_target
  -> acquisition_frontier
  -> execution_target
  -> execution constraints
  -> order plan
```

- `policy_target` 表示策略曲线、带外策略或人工/终止控制当前希望达到的理论目标。
- `risk_approved_target` 是当前风险与控制路径最终允许的目标。自动策略路径会在 `policy_target` 上应用 risk cap 和 loss guard；人工或终止快路径可以直接使用对应控制目标。它保留“策略想要什么”与“当前允许什么”的差异。
- `acquisition_frontier` 是当前 session 已经释放的新增风险边界。没有 active acquisition gate 时它为空，表示不额外限制 risk-approved target。
- `execution_target` 是结合当前真实仓位、risk-approved target 和 acquisition frontier 后，executor 本轮可以追随的 final target。account capacity、price gate、恢复状态和 venue minimum 是后续 execution constraints；它们可以阻止 submit，但不偷偷改写这四层目标。

ownership 固定如下：

- `core::strategy` 拥有曲线公式；`engine::reconciler` 是四层目标和 risk acquisition 的唯一计算 owner。
- executor 只接收 `risk_approved_target` 与 `execution_target`，结合当前仓位、盘口、exchange rules 和已有 bindings 生成 order plan 与执行原因；它不读取 acquisition frontier，也不重算 backlog。
- manager 把 reconciler 结果和 executor 结果组合成一次 `InventoryDecisionSnapshot`，选择一个主 `DecisionReason`，并在提交该次 runtime/executor 变化时让 decision revision 严格递增一次。
- `TrackRuntimeView`、application read model、server projector、protocol、TUI 和 Server live inspector 只投影 committed snapshot。它们不能从其他零散字段、risk acquisition 诊断或 binding 状态重新运行决策链。等待行情、行情 stale 或暂停时，对外目标不可用，并通过 `DecisionReason` 显式说明。

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

- `risk_acquisition` 默认启用，只约束增加风险暴露；降低风险暴露时直接重新评估并优先执行到 risk-approved target。
- fresh session 的保守初始化不使用 `initial_ratio`；它只承认当前真实仓位或立即降低风险。初始化完成后，同一 session 中后来出现、且没有既有 gate state 的普通 acquisition episode 才计算 initial allowance：目标小于 `min_rebalance_units` 时 allowance 等于目标，否则取 `target_units * initial_ratio` 与 `min_rebalance_units` 的较大值。初始 frontier 再取 allowance 与已有同向仓位的较大值，并受 target 限制；只有 frontier 超出当前仓位的部分才算新增 release，所以已有仓位已经覆盖 allowance 时 release 可以为 `0`。
- 增加风险暴露后，系统记录 `release_anchor_price` 和 `release_anchor_target`。只有 risk-approved target 相对锚点继续走出 `advantage_steps * min_rebalance_units`，才释放一部分 backlog。
- 如果在线 stale 等待达到 `stale_release_minutes`（默认 60 分钟），即使价格没有走到优势阈值，也允许释放一部分 backlog；设为 `0` 表示关闭时间释放。fresh session 不继承旧 timer，离线时间不参与等待。
- 每次释放量由 backlog、`catchup_ratio`、`min_release_steps` 和 `max_release_steps` 共同决定：先按 backlog 比例计算，再限制在最小/最大释放单位之间。
- 运行时通过 committed decision 的 `acquisition_frontier` 和独立 acquisition diagnostics 公开 `backlog_units`、`next_advantage_price`、`next_release_units` 等观测字段；这些字段只解释当前 decision，不能作为 read model 重算目标的输入。

风险释放不可变式：

- `reconciler` 先计算 `policy_target` 和 `risk_approved_target`，再让 risk acquisition gate 计算 `acquisition_frontier`。gate 不覆盖前两层目标。
- 没有 acquisition frontier 时，`execution_target = risk_approved_target`。
- acquisition frontier 和 risk-approved target 异号时，先把 frontier 投影到 `0` 作为新方向释放起点，再继续走同一套 advantage/stale 释放逻辑；如果本轮没有释放，`execution_target = 0`。
- frontier 已经进入 risk-approved target 方向时，单向净持仓模式可以让 `CatchUp` 直接追到 frontier；这一笔订单的前半段会平掉旧方向残余仓位，后半段才进入已经释放的新方向风险，因此不能标成 reduce-only。
- risk-approved target 回到 frontier 内侧时，会把 frontier 一起往风险更低方向压回去；active gate 被压到 0 后，后续同方向增长不会重复获得该 episode 的初始释放，只能继续靠价格优势或 stale 时间释放。
- risk-approved target 仍在 frontier 外侧，且当前仓位还没到达 frontier 时，execution target 最多到 frontier，不继续释放下一段 backlog。
- 当前仓位到达或穿过 frontier，但没有超过 risk-approved target 时，frontier 推进到当前仓位；这是承认已经获得的仓位，不算新的 release，也不重置 anchor。
- 当前仓位超过 risk-approved target 时，execution target 回到 risk-approved target，优先降低风险。
- 增加风险不规划 increase maker，统一由 `CatchUp` 追随 `execution_target`；`CurveMaker` 只保留 risk-approved target 降低风险方向的 reduce-only maker。

后续释放量计算：

```text
backlog_units = abs(risk_approved_target - acquisition_frontier)
release_units = clamp(backlog_units * catchup_ratio, min_release_steps * min_rebalance_units, max_release_steps * min_rebalance_units)
release_units = min(release_units, backlog_units)
```

价格优势释放和时间释放使用同一套 `release_units`。只要还有同方向 backlog，且价格优势满足，就把 frontier 向 risk-approved target 推进，并重置 `release_anchor_price`、`release_anchor_target` 和 `stale_since`；因此价格优势释放会让 stale 重新计时。时间释放不要求当前仓位先吸收上一段 frontier，不更新价格优势锚点，只重置 `stale_since`，所以下一次时间释放会重新等待完整在线周期。frontier 推进后由 `CatchUp` 追新的 `execution_target`；当前仓位到达或穿过 frontier 时，gate 先 ratchet 承认已获得的仓位。价格继续移动产生的新 risk-approved target 变化只进入同一个 backlog，不单独立即释放。

风险释放示例：

```text
current_exposure = +3.9
policy_target = +4.263
risk_approved_target = +4.263
acquisition_frontier = +3.9
backlog_units = +0.363
execution_target = +3.9
```

这表示策略与风险目标都已经到 `+4.263`，但当前 session 只释放到 `+3.9`。executor 当前保持 `+3.9`；等价格优势或在线时间释放把 acquisition frontier 往外推进后，manager 才会提交新的 execution target，`CatchUp` 再追随它。

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

持久化边界保存可跨进程恢复的业务事实，以及用于审计和诊断的历史记录。诊断记录不会因此自动成为恢复输入。持久化层不保存完整当前配置定义，也不把 read model 当成事实源；读侧需要静态定义时，从当前配置构造出的 `TrackDefinitionRegistry` 读取。

当前持久化事实：

- `track_identity`：保存 `track_id`、`venue`、`symbol` 和 `bound_at` 的最小稳定绑定。启动在连接交易所和修改交易所状态前校验它；同一 `track_id` 不能把旧控制状态或 PNL 历史重新解释成另一 instrument。
- `track_control_state`：只保存可跨进程恢复的控制状态，例如自动、暂停、手动目标和终止。自动运行中的 `RiskExposureGated`、冻结确认、flatten pending 等运行阶段在写入持久控制状态时折回对应的 durable 控制模式。
- `track_events`：保存领域事件，用于审计和读侧更新。
- `track_effects`：best-effort effect journal，保存计划派发和结果的诊断投影、重试次数及错误摘要。它不是当前 session effect queue、可靠 outbox 或交易所订单事实；写入失败不会回滚已经提交的业务状态，旧行也不参与启动恢复或订单重放。当前进程中的 journal 写入失败通过日志、diagnostics 和 readiness warning 暴露，因此 journal 本身可能不完整。
- `track_pnl_records`：保存可归属到 track 的成交盈亏、交易手续费和资金费明细。
- `persisted_track_presence`：读模型辅助表，只用于 listing 和 updated-at 元数据。
- `account_monitor_state`：账户监控的跨进程基线，包括交易日、基线权益和基线捕获时间；不保存当前账户摘要。

当前进程事实：

- `TrackMutationFrame` 是一次 mutation 的当前进程快照，用于提交、回滚和 durable-write 判断；它不是持久文档。
- `ExecutorState`、binding ledger、recovery anomaly、risk exposure gate、fresh acquisition marker、`InventoryDecisionSnapshot`、decision revision、acquisition frontier、release anchor、stale timer 和 live market fields 属于 runtime 当前会话状态。它们不持久化；需要展示时从 live runtime 投影，重启后根据 durable 控制状态、当前配置、交易所快照和新行情重新建立。
- `SessionEffectQueue` 是当前进程内唯一的 effect 派发事实，随 fresh session 新建；它不从 `track_effects` 恢复。
- 当前账户摘要、`available_by_asset` 和 hedge-like account asset 数量来自启动后的 `AccountSummaryPort` 刷新或订阅更新，不从 `account_monitor_state` 恢复。
- `TrackRuntimeView`、read model 和 protocol DTO 都是投影结果。它们可以被查询和推送，但不能反向作为 engine 或 persistence 的事实源。

读模型链路：

```text
TrackDefinitionRegistry + TrackRuntimeView + persisted event/journal projections
  -> application read model
  -> server projector
  -> protocol DTO
```

`TrackRuntimeView` 已经包含 manager committed decision 的投影；application read model、`server` projector 和 protocol DTO 只复制同一 revision 的四层 target 与 reason，不从其他 runtime 字段重算。`server` projector 不直接暴露 engine 内部运行时结构，HTTP / WebSocket 对外只推投影后的读模型。

Track PNL 统计只把本地可归属到 track 的明细作为事实来源：

- `track_pnl_records` 记录每笔成交已实现盈亏、交易手续费、PNL 资产，以及可归属到 symbol/track 的资金费。
- `pnl_asset` 是 PNL 明细的一部分，来自交易所成交、费用或合约结算资产语义；运行时和读模型统计从明细聚合得到该资产。公开读模型只在没有任何 PNL 明细时，才用当前 track 的 settlement asset 作为展示 fallback。
- 非当前 track settlement asset 计价的手续费不能混入本地 track PNL 统计；adapter 应拒绝或跳过这类明细，而不是把不同资产相加。
- `TrackPnlStats` 是运行时和读模型使用的即时统计结果，不是持久化真值；启动和查询时可以从 `track_pnl_records` 重新聚合。
- 当日 PNL 窗口按当前 UTC 日展示，明细是否进入当日统计由它自己的发生时间决定。
- 订单成交更新只更新订单/执行器状态；PNL 明细作为独立记录写入，避免订单状态和财务统计互相隐藏规则。
- 资金费如果无法归属到某个 track，就不进入 track PNL 统计。
- HTTP / WebSocket 公开读模型使用 `pnl` 字段，不再用旧的 `ledger` 命名承载 PNL 统计。

recent PNL capability 是 `RecentTrackPnlRecords::Available(records) | Unsupported`，空历史必须表达为 `Available([])`，不能伪装成不支持。当前矩阵为：

| Venue | recent PNL capability | 当前内容 |
| --- | --- | --- |
| Hyperliquid | `Available` | recent fills 与可归属 funding |
| OKX | `Available` | recent fills 与可归属 funding bills |
| Binance | `Unsupported` | 不把空集合伪装成成功 |
| Bybit | `Unsupported` | 不把空集合伪装成成功 |

Available venue 的 recent fills 和 funding 都映射为 `TrackPnlRecord`，不进入第二套 PNL 事实表。runtime 的 PNL backfill task 使用交易所 recent records 补写本地缺失明细，并通过 `/health` 暴露最近完成时间、写入数量、跳过数量和最近错误；PNL capability 或 backfill 仍为 unknown 或出现关键异常时，也会成为 `/readiness` blocker。重复回补依赖 source key 保持幂等。`GET /debug/tracks/:id/recent-fills-audit` 只比较交易所最近 trade records 和本地 `track_pnl_records` 的覆盖情况，是排查投影，不写事实；`Unsupported` 也是成功的 capability 结论，查询或本地存储错误才是接口错误。启动后的 recent fills audit 如果发现本地缺失，会写入 `PnlAuditMissingRecords` 诊断事件；该事件说明“启动时发现缺失”，不替代 PNL 明细，也不表示缺失一定尚未被后续 backfill 修复。

## HTTP / WebSocket

当前公开入口：

- `GET /health`
- `GET /readiness`
- `GET /account`
- `GET /tracks`
- `GET /tracks/:id`
- `POST /tracks/:id/commands`
- `GET /debug/tracks/:id/diagnostics`
- `GET /debug/tracks/:id/recent-fills-audit`
- `GET /ws`

协议字段不再单独维护长文档。修改协议时必须更新 `protocol/src/lib.rs` 和序列化测试；需要说明语义时，在本文只保留高层约束。

`/health` 保留进程、track 和 runtime task 的观测语义；task 的 `Unknown` 状态会如实展示，但不会仅因此改变既有 health 结论。`/readiness` 是更严格的当前交易条件投影：关键 runtime task 或 PNL 状态为 `Unknown` / `Degraded`，以及 track 等待行情、行情陈旧、price execution gate、recovery anomaly、需要重新对账的 exchange freshness 或账户保证金 gate 时，返回 `ready_to_trade=false` 和 blockers。账户监控的 `Critical` 状态阻止交易，`Attention` 状态只进入 warnings。effect journal 写入失败也只进入 warnings，不恢复旧 effect、不回滚业务状态，也不把 journal 升级为 outbox。readiness 只读取当前内存和读模型快照，不持久化，也不重新运行 engine 状态机。

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

当前不拆分长期模块：架构边界、数量单位、PNL 语义、启动与运行时、配置 dry-run 和 replay 继续放在本文。只有当某类内容明显有独立读者、独立维护节奏或篇幅过长时，才拆成长期模块。候选模块包括 OKX 值守操作手册、配置 dry-run 使用手册等操作型材料；它们必须从本文有明确入口，声明自己只记录操作步骤，不重复维护 `ExchangeRules`、PNL、启动或策略语义。任何拆出的长期模块都要说明事实归属，并在本文保留边界摘要和去重规则。
