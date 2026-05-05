# Poise 当前系统说明

本文只记录当前实现仍成立的系统事实。历史 plan、旧 spec、旧协议说明和阶段性评审不再作为文档保留；如果设计继续变化，应直接更新本文或 README。

## 事实源

- 架构事实源是代码和测试，本文只做当前边界摘要。
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

## 配置与身份

服务端只支持单实例连接单交易所。`[exchange]` 决定当前实例的 `venue`，`[[tracks]]` 不再配置 venue。

当前重要约束：

- `exchange.venue` 当前支持 `binance`、`bybit`、`hyperliquid` 和 `okx`。
- Binance / Bybit 使用 `api_key` 和 `api_secret`；Hyperliquid 使用 `private_key` 和 `wallet_address`，可选 `vault_address`；OKX 使用 `api_key`、`api_secret` 和 `passphrase`。
- `track_id` 是显式配置的稳定业务标识。
- `Instrument` 由 `exchange.venue()` 和 track `symbol` 组成。
- `symbol` 是当前交易所的合约标识：Binance / Bybit 使用 `BTCUSDT` 这类合约符号，Hyperliquid perpetuals 使用 `BTC`、`ETH` 这类 coin 名称，OKX SWAP 使用 `BTC-USDT-SWAP` 这类 instrument id。
- 同一实例内 `track_id` 必须唯一。
- 同一实例内 `Instrument { venue, symbol }` 必须唯一。
- `leverage` 是 server-owned startup-only 配置，不进入 `TrackDefinition` 或 `TrackConfig`。
- 未配置 `leverage` 时默认 `10`。

当前交易所接入只覆盖 Poise 运行需要的合约能力。Hyperliquid 适配器只接入 perpetuals，不提供 spot、提现、划转、TWAP 或 vault 运维功能；可选 `vault_address` 只作为 Hyperliquid action 签名上下文进入适配器内部。Hyperliquid standard mode 使用 perps `clearinghouseState.withdrawable` 作为可用保证金口径；unified account / portfolio margin 使用 `spotClearinghouseState` 的 USDC 可用余额作为启动容量口径。OKX 适配器只接入 `SWAP` 永续合约，REST 覆盖规则查询、账户摘要、持仓、open orders、下单、撤单、cancel all 和启动期杠杆设置；WebSocket 覆盖 ticker、mark price、订单更新、成交 PNL、持仓更新、断线重连和重订阅。OKX 当前只支持 `cross` 保证金模式和 `net` 持仓模式，不提供 spot、期权、划转、提现或资金账户操作。

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
4. 在 `server/src/exchange_startup.rs` 增加启动期窄控制：`SymbolLeverageSetter` 用于按 track 设置 startup-only leverage；`StartupCapacityProbe` 的选择也在这里处理。Binance 使用交易所 account capacity snapshot；Bybit / Hyperliquid / OKX 使用 account summary 的 available balance 乘以 track startup leverage。
5. 补最小测试：配置解析、`connected::tests::`、关键 mapper、`exchange_startup::tests::`，以及必要的 `assembly::tests::`。如果新增交易所只改某个 crate，优先跑该 crate 的最小测试，再按影响面扩大。

## 启动与运行时

启动主路径：

1. `server/src/main.rs` 读取 `--instance-dir` 下的 `config.toml`。
2. `state_bootstrap::prepare_state_repository(...)` 初始化 SQLite，构造 `TrackDefinitionRegistry`，并检查当前配置与持久业务状态是否兼容。
3. `assembly::assemble(...)` 构造交易所连接，按 track 执行 startup-only 杠杆设置，加载交易所规则，并把每个 `TrackDefinition` 注册进 `TrackManager`。
4. `RuntimeStartupDefinition` 把 `TrackDefinition` 和 startup leverage 组合成 server runtime 内部输入。
5. `StartupCapacityProbe` 在启动恢复时按当前交易所策略计算可增加名义金额。
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

持久化边界保存业务状态和事件，不保存完整当前配置定义。读侧需要静态定义时，从当前配置构造出的 `TrackDefinitionRegistry` 读取。

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
- 本文保留当前架构、边界和运行语义。
- 不再保留历史 plan/spec 作为主文档；过时讨论直接删除。
- 如果某个文档不能明确回答“当前实现如何工作”，默认不保留。
