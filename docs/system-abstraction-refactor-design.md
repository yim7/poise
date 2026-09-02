# 系统抽象重构设计

> 状态：实施中，S1–S5 已完成，S6 正在实施。
>
> 文档角色：本文定义本次系统抽象重构要做的改动和验收标准；[system-overview.md](system-overview.md) 在重构完成前继续描述当前系统，是当前实现的唯一长期依据。[exchange-abstraction-research.md](exchange-abstraction-research.md) 只保留调研证据，不定义最终架构。
>
> 生命周期：全部验收通过后，把已实现的结果合并进 `system-overview.md`，再删除本文和调研文档。实现没有完成前，不提前改写当前系统说明。

## 1. 为什么重构

当前系统的主要问题不是功能缺失，而是一次 track 更新被多个 owner 共同解释：

- engine 中的 `TrackManager` 和 `TrackRuntime` 先原地修改状态，再用 `TrackMutationFrame` 表示前后差异和 rollback 边界；
- application 的 `MutationExecutor` 负责锁、持久化、effect 和通知，外面又有 observation、command、effect 等多个 service 转发调用；
- 静态定义同时存在于 `TrackDefinitionRegistry` 和 manager/runtime；
- submit 的 binding、preflight、queue、coordinator 和 unknown-outcome 处理跨 engine、application、server 分布；
- exchange ports 位于 engine，server 又复制成 `RuntimePorts`；
- `ExchangeRules`、`native_qty`、裸 `f64` 和 venue/symbol 特判共同承担标的与单位换算；
- read model 仍会从 definition、runtime 和 rules 重新组合业务值。

主导复杂度信号是：

- `change amplification`：一次状态或单位变化需要同时修改多个 crate；
- `cognitive load`：必须理解 mutation、rollback、registry、service、queue 和 server runner 才能判断一次更新是否已经提交；
- `unknown unknowns`：相同事实有多个投影或计算入口，很难确认某处修改会不会破坏恢复、对账或读模型。

本次重构的目标不是增加一套更正式的框架，而是减少 owner、公共接口和需要同时记住的概念。

## 2. 本次改动范围

本次重构完成后，系统应满足以下结构：

1. application 只用 `TradingRuntime::handle(track_id, input)` 接受所有会改变 track 的输入。
2. engine 从不可变当前状态计算一个完整的 `TrackTransition`，不做 I/O，不原地修改 committed state。
3. `TradingRuntime` 独占串行化、持久化、安装下一状态、session effect queue 和通知顺序。
4. submit 生命周期完全归 application；server 只启动 runner 和转交事实。
5. 交易所能力通过五个窄 port 暴露，port 位于 application 与 adapter 的 seam，不再属于 engine。
6. `InstrumentSpec` 成为合约事实、精度、canonical quantity、notional 和 base exposure 换算的唯一 owner。
7. 账户估值和 PNL cashflow 都携带资产；不同资产不能用裸数值相加。
8. 查询只组装 committed runtime snapshot 和持久历史，不重新运行业务计算。
9. 删除已被替代的 frame、registry、浅 service、重复 port bundle 和 server submit owner。
10. 不兼容重构前的数据库 schema、旧字段或旧接口；删除迁移、双读/双写和 legacy 推断路径，旧实例需要显式重新初始化。

这不是一次性横向替换所有 crate。实现按后文的纵向切片推进，每个切片都必须保持系统可运行、可测试，并在引入新入口的同一切片删除对应旧入口。

## 3. 明确不改的内容

本次重构不改变以下已经成立的业务语义：

- 单实例连接单交易所，`track_id` 是稳定业务身份，`venue + symbol` 在实例内唯一；
- `policy_target -> risk_approved_target -> acquisition_frontier -> execution_target` 四层目标链；
- risk acquisition 的 fresh-session 保守初始化、advantage release、stale release、跨零和优先降风险规则；
- pause、resume、flatten、terminate 和带外策略语义；
- `SubmitOutcomeUnknown` 期间禁止提交替代订单；
- REST position/open-orders 快照受 exchange observation revision 和 effect queue revision 保护；
- fresh session 不恢复旧 bindings、执行进度、gate、timer 或 effect queue；
- `track_effects` 只是 best-effort 诊断 journal，不是 outbox；
- HTTP 路由和 WebSocket 事件集合；
- curve-immediate benchmark、delayed decision replay 和 evaluation 的业务含义；
- 当前支持的四个 venue，不在本次增加 spot、期权、划转、提现或多 venue runtime；
- 当前继续使用 `f64`，先用单位边界和统一归一化消除混用，不在本次引入 decimal 依赖。

如果重构过程中必须改变以上任一语义，应先单独更新本文并重新评审，不能把它混成“内部重构”。

## 4. 比较过的设计方向

### 方向 A：只缩小当前类型的可见性

保留 `TrackManager + TrackMutationFrame + MutationExecutor`，只把 service、registry 和 server helper 设为私有或合并文件。

优点是改动小。缺点是最关键的原地 mutation、持久化失败 rollback 和多个状态 owner 仍然存在；调用方变少了，但提交协议没有变简单。这只能作为中间整理，不能作为最终结构。

### 方向 B：`TrackStep` 原地推进

让 runtime 暴露 `step(input)`，内部修改当前状态并返回 events/effects。

它少一个候选状态类型，但“step”没有说明结果是否已经持久化，也不能消除数据库失败时的 snapshot 和 rollback。这个名字还容易被误解为 runner 阶段、队列 item 或状态机节点。

### 方向 C：不可变 `TrackTransition` + application 提交

engine 接收当前状态和输入，返回候选下一状态、durable events 和 effects；application 在持久化成功后一次安装结果。

它新增一个 engine-private 临时值，但可以删除 mutation frame、diff 推断、rollback、多个 mutation service 以及 server 对提交步骤的了解。新增概念少于被删除概念，且可直接对纯转换做测试。

选择方向 C。

## 5. 概念预算

本次只保留以下新增或重新定义的概念：

| 名称 | 性质 | 是否跨模块公开 | 准确职责 |
| --- | --- | --- | --- |
| `TradingRuntime` | application 运行时对象 | 对 server 公开 | 持有全部 tracks 和 ports，串行处理输入并提交结果 |
| `TrackInput` | application 到 engine 的输入契约 | application / engine | 表示一个已经进入共享语义的事实、命令或 effect 结果 |
| `TrackTransition` | engine 临时值 | engine 内部 | 把候选下一状态、events 和 effects 绑定成一次纯计算结果 |
| `TrackSessionState` | engine 运行时状态 | engine 内部，查询只见快照 | 当前进程内一个 track 的完整 committed 状态 |
| `InstrumentSpec` | core 深模块 | core / engine / application | 持有不可变合约事实并提供全部 canonical 换算 |
| `PositionQty` | 带语义数值 | 共享业务边界 | 表示该 instrument 约定的 canonical position quantity |
| `Amount` | 带资产数值 | 共享业务边界 | 防止不同资产的账户金额和 cashflow 被直接相加 |
| `ConnectedExchange` | server 私有装配记录 | 否 | 确保一次 venue 分支产生匹配的 ports 和 startup control |

下面这些只表示职责划分，不实现成新 class、service 或 DTO：

- 不可变定义、durable facts、session state、exchange facts 四类状态；
- transition 计算、commit、effect dispatch 三段职责；
- query side 与 write side 的区别。

明确不新增：`TrackStep`、每类输入一个 service、每类状态一个 repository、通用 actor 框架、通用 state-machine framework、公开 `ConnectedExchange`、第二个 runtime port bundle，以及 `NotionalAmount / BaseAmount / ValuedAmount / AssetAmount` 等一组平行 wrapper。当前只用一个 `Amount`，只有实际类型错误仍无法被阻止时才继续细分。

## 6. 目标模块与接口

### 6.1 模块归属

| 模块 | 重构后的职责 |
| --- | --- |
| `core/` | 领域身份、静态定义、风险限制、领域事件、`InstrumentSpec` 及纯计算 |
| `engine/` | 单 track 纯 transition、目标链、risk acquisition、executor、恢复判定、回放 |
| `application/` | `TradingRuntime`、提交协议、session effect queue、exchange/persistence ports、queries |
| `storage/` | application persistence ports 的 SQLite adapter |
| `exchanges/*` | 私有 wire protocol 与共享事实的双向转换 |
| `server/` | 配置、一次装配、HTTP/WS、把外部输入送进 runtime、启动和停止 runner |
| `protocol/` | 对外 DTO，只表达 committed read model |
| `tui/` | 消费协议，不做领域重算 |

### 6.2 唯一写入口

```rust
impl TradingRuntime {
    async fn handle(
        &self,
        track_id: &TrackId,
        input: TrackInput,
    ) -> Result<TrackUpdateReceipt>;
}
```

`TrackInput` 至少覆盖：

- market / mark price observation；
- position / order / fill / PNL fact；
- user command；
- fresh-session bootstrap fact；
- reconciliation result；
- effect dispatch result；
- 需要进入 track 决策的 account / health fact。

不要求每个输入 variant 拥有单独 public method。为了 HTTP 调用方便可以有薄的参数解析函数，但最终只能调用 `handle`，且不能在外部插入额外提交步骤。

`TrackUpdateReceipt` 只说明已经提交的结果，例如最新 decision revision 和可见 snapshot 是否变化。通知由 runtime 内部发布，receipt 不能暴露候选 state、未入队 effect，也不能让调用方继续决定是否 commit 或 publish。

### 6.3 engine 纯转换

```rust
fn transition(
    current: &TrackSessionState,
    definition: &TrackDefinition,
    instrument: &InstrumentSpec,
    input: TrackInput,
) -> Result<TrackTransition>;

struct TrackTransition {
    next_state: TrackSessionState,
    events: Vec<DomainEvent>,
    effects: Vec<TrackEffect>,
}
```

`TrackTransition` 只存在于一次 `handle` 调用中。它不是持久化记录、queue item、运行阶段或协议 DTO。

同样的 current state、definition、instrument 和 input 必须得到同样结果。引擎禁止读取数据库、系统时间、随机数、网络或全局可变状态；这些数据先变成输入。引擎也不持有 exchange port。

第一版允许完整 clone 一个 track state。只有性能数据证明 clone 是瓶颈时，才在不改变接口语义的前提下引入 copy-on-write 或内部优化。

### 6.4 查询入口

查询保留单独的 `TradingQueries`。它从 `TradingRuntime` 取得 committed snapshot，并与 repository 中的 history 组装 read model。

查询侧不能：

- 重新计算四层目标或 `DecisionReason`；
- 从 quantity 自行计算 notional / exposure；
- 用当前 metadata 重算历史记录；
- 把 read model 写回 runtime；
- 依赖第二份 `TrackDefinitionRegistry`。

## 7. 状态和提交标准

### 7.1 四类状态

| 类别 | 内容 | 恢复方式 |
| --- | --- | --- |
| 不可变定义 | `TrackDefinition`、当前 session 的 `InstrumentSpec` | 配置 + 交易所 metadata |
| durable facts | identity、durable control、events、PNL/fee/funding、账户监控基线 | SQLite |
| session state | decision、executor、bindings、risk gate、recovery、行情和仓位观察 | fresh session 重建 |
| exchange facts | 实际仓位、open orders、fills、实际费用和 funding | 交易所查询和流 |

四类状态是判断 owner 的标准，不生成四套对象或 repository。

### 7.2 一次更新的提交顺序

1. `TradingRuntime` 取得 track 的串行锁并读取 committed state。
2. engine 纯计算 `TrackTransition`；当前状态保持不变。
3. durable events 和 durable control 变化在一个 SQLite transaction 中写入。
4. 持久化成功后，在同一临界区安装 `next_state` 并把 effects 追加到 session queue。
5. 释放锁后发布 committed read-model notification。
6. effect runner 在锁内认领 effect，释放锁后调用交易所，再把结果作为新的 `TrackInput` 送回 `handle`。

第一版在 SQLite 写入期间保持该 track 的串行锁。它牺牲同一 track 的写并发，换取明显的顺序和更少的版本协议；任何交易所网络 I/O 都不得持锁。

错误标准：

- transition 失败：无持久化、无状态变化、无 effect；
- durable write 失败：current state 未变，不需要 rollback；
- 安装后通知失败：业务状态不回滚，health / warning 暴露通知问题；
- effect journal 失败：不回滚状态或 queue，journal 仍是 best-effort；
- exchange call 确定失败：结果回送 engine，由状态机决定下一动作；
- outcome unknown：保持提交所有权，禁止自动替代 submit。

### 7.3 revisions

- `decision_revision` 属于 committed track state，只在新的 decision 成功提交时递增，候选 transition 不可见。
- `exchange_observation_revision` 属于 application 的进程内并发保护，不进入候选 `TrackSessionState`。一个订单或仓位事实通过基本格式校验、被 runtime 接受时就推进；后续业务 transition 或 durable write 失败也不能让它回退。
- session effect queue revision 同样由 application 管理，在 effect 入队、认领或裁决时推进。REST position/open-orders 请求返回时，只有 observation revision 和 queue revision 都与发起前一致，快照才可应用。

## 8. Effect 和 submit 标准

职责固定为：

- engine 拥有 order intent、`ClientOrderKey` 和候选 binding；
- application queue 独占 `queued / dispatching / outcome_unknown / resolving / completed` 生命周期；
- adapter 拥有 wire ID 编码和协议结果映射；
- server 只启动 runner，不拥有 submit business state。

`ClientOrderKey` 在重试、REST/WS 对账和 fresh-session 恢复中保持业务身份稳定。adapter 可以按交易所字符集和长度生成 wire representation，但不能让 wire 格式成为共享领域规范。

`SubmitOutcomeUnknown` 后必须同时保留 effect、client order identity 和 binding ownership。只有完整 open-orders / position snapshot 或后续 order fact 能解除。`track_effects` journal 不能作为“已经提交”或“应该补发”的证据。

重构完成后，`server::SubmitPreflight`、`server::SubmitCoordinator` 和 server 中理解上述 lifecycle 的分支应删除。runner 可以由 server 启动，但 lifecycle 代码必须位于 application。

## 9. 交易所和单位标准

### 9.1 五个能力 port

- `ExecutionPort`：submit、cancel、cancel all、position、完整 open orders；
- `MarketDataPort`：市场数据订阅；
- `AccountPort`：recent track PNL 与 account user data；
- `AccountSummaryPort`：账户估值、逐资产余额和容量输入；
- `MetadataPort`：instrument metadata 与 exchange time。

port traits 从 engine 移到 application seam。engine 的依赖图中不能出现 async exchange port、adapter 或 server 类型。

port 的语义验收：

- open orders 完成全部分页后才能构造 complete snapshot；不能证明时返回 error 或 explicit incomplete；
- cancel ACK 只表示 accepted，terminal status 必须来自权威查询或流；
- recent PNL 请求携带 required window，响应携带 covered window 和 completeness；空记录不等于 unsupported，也不自动等于 complete；
- capacity 使用显式 startup leverage 或交易所实际 leverage，禁止 port 内置隐藏 `10x`；
- submit/cancel 的确定失败与 outcome unknown 使用不同结果；
- 缺少 base、quote、settlement、contract value、tick、lot 或其他必要 metadata 时 fail closed。

### 9.2 一次装配

`server::assembly` 只在一处穷举 `ExchangeConfig`，构造私有的：

```rust
struct ConnectedExchange {
    venue: Venue,
    ports: ExchangePorts,
    startup_control: Arc<dyn ExchangeStartupControl>,
}
```

这个形状只说明配对约束，不要求 adapter 返回该 server 类型。如果私有元组更简单，可以不创建命名 struct。assembly 把 ports 直接移动给 `TradingRuntime`，startup control 只供启动准备使用；不再构造 `RuntimePorts` 或让 server runtime 分别复制五个 owner。

### 9.3 `InstrumentSpec`

`Instrument { venue, symbol }` 只标识路由。合约事实与计算统一由 `core::InstrumentSpec` 拥有：

```rust
struct InstrumentSpec {
    instrument: Instrument,
    base_asset: Asset,
    quote_asset: Asset,
    notional_asset: Asset,
    settlement_asset: Asset,
    position_basis: PositionBasis,
    price_rule: PriceRule,
    quantity_rule: QuantityRule,
    min_notional: Amount,
}

enum PositionBasis {
    BaseAsset,
    Contracts { contract_value: Amount },
}

struct PositionQty(f64);

struct Amount {
    value: f64,
    asset: Asset,
}
```

字段可保持私有。调用方只通过以下语义操作使用它：

```text
normalize_price
normalize_position_qty
position_qty_for_notional
notional_from_position_qty
base_exposure_from_position_qty
```

唯一换算规则：

- 线性合约的 canonical position quantity 是 base asset amount；OKX linear 的 wire contracts 在 adapter 边界转换一次；
- 反向合约的 canonical position quantity 是 contracts，`contract_value` 明确每张合约的面值和资产；
- order、position、open order、fill 和持久 quantity 在同一 instrument 上使用同一个 canonical unit；
- `TrackDefinition::notional_per_unit` 只通过 `InstrumentSpec` 换算，不保留固定 `notional / price` 的旁路；
- `DesiredOrder -> NormalizedOrder -> WireOrder` 中，价格和数量只由 `InstrumentSpec` normalize 一次；adapter 只做可逆编码和验证；
- wire quantity 和 venue-specific symbol/settlement 规则不能离开 adapter；
- adapter 如果无法无损表达 normalized order，应返回错误；如果交易所接受值不同，receipt 返回实际 canonical 值。

完成迁移后删除 `ExchangeRules`、`QuantityKind` 的混合语义、`native_qty` 公共命名、`Instrument::quote_asset` 的 venue 猜测，以及 `TrackDefinition` 中绕过 spec 的 base-quantity 公式。

### 9.4 Amount、账户和 PNL

必须显式区分 base、quote、notional 和 settlement asset。

- account-wide equity、available、unrealized PNL 携带同一个 valuation asset；
- `available_by_asset` 保留逐资产原生数量，不能累加成一个无单位数字；
- realized PNL、trading fee 和 funding 分别持久化 `Amount` 与 source identity；
- 实际 cashflow 只来自交易所事实，`InstrumentSpec` 不推导；
- fee-rate estimate 是有来源的策略输入，不进入 spec，也不替代实际 fee；
- 不同 asset 的 cashflow 只能通过带 source 和 timestamp 的 FX valuation 转换后聚合。

缺少可靠 FX 时，所有原始 cashflow 仍然持久化，aggregate 明确为 `Incomplete` 并列出 unvalued amounts。不得把未知 fee 当零，不得丢弃已知 realized PNL。loss guard 面对 incomplete aggregate 时禁止增加风险，但允许 cancel、reduce-risk 和数据修复；有可靠估值或保守上界后才解除。

持久 quantity 同时保存当时的 canonical position unit；contract quantity 额外保存当时的 contract value 和 asset。旧记录不能用当前 metadata 重算历史 PNL。

## 10. 按模块需要发生的改动

### `core/`

- 增加最小 `Asset`、`Amount`、`PositionQty` 和 `InstrumentSpec` 深模块；
- 把合约换算从 `TrackDefinition` / `Instrument` 的 venue 特判移入 spec；
- 保留 track definition、策略、风险和领域事件，不引入 port 或 session state。

### `engine/`

- 把完整 track session state 变成可从不可变输入生成下一值的内部状态；
- 由一个 engine transition 函数组合 reconciler、executor 和 recovery 的结果，产出一个 `TrackTransition`；
- 删除 `TrackMutationFrame`、rollback、基于前后 diff 判断 durable write 的协议；
- 将 exchange/persistence ports 移出 engine；
- replay 和 evaluation 改用同一 transition 与 `InstrumentSpec`。

### `application/`

- 建立唯一 `TradingRuntime::handle` 和只读 `TradingQueries`；
- 直接持有唯一 track map，合并 `TrackDefinitionRegistry` 与 `TrackManager` 的索引职责；
- 把 mutation commit、session effect queue、submit lifecycle 和 notification 变成一个内部协议；
- 删除 observation / command / effect / submit 等只做转发的 service；
- 定义 exchange ports 和 persistence ports。

### `storage/`

- 保持只实现 application persistence port；
- 为 quantity unit、contract value、cashflow asset 和 account valuation asset 增加必要 schema；
- 只接受当前 schema；不在启动时迁移、重命名或猜测旧表和旧字段，schema 不匹配时明确失败并要求重新初始化实例。

### `exchanges/*`

- 从 metadata 构造完整、验证后的 `InstrumentSpec`；
- 在 adapter 内完成 canonical quantity 与 wire quantity 的唯一转换；
- 为 REST position/open orders、WS order/position、fills 建立 round-trip 契约；
- recent PNL 返回 required/covered window 和 completeness；
- 保持协议、签名、分页、账户模式和 wire DTO 私有。

### `server/`

- assembly 只做一次 venue 分派；
- server task 把外部事实转换为 `TrackInput` 并调用 runtime；
- 删除 `RuntimePorts`、`SubmitPreflight`、`SubmitCoordinator` 和对 track mutation 顺序的了解；
- HTTP/WS 只使用 commands、queries 和 committed notifications。

### `protocol/`、`tui/` 和工具

- protocol 对 amount、asset、PNL completeness 和 canonical unit 做必要投影；
- TUI 不重新计算目标、notional 或 PNL；
- workbench 使用 `InstrumentSpec` 和同一 transition/evaluation 路径，不保留 TypeScript 计算 fallback。

## 11. 落地切片

每个切片都从真实输入经过 engine、application 和可观察输出形成闭环，不能先横向铺一批空类型。

| ID | 切片 | 状态 | 实现提交 |
| --- | --- | --- | --- |
| S1 | 不可变 transition 与唯一更新入口 | 已完成 | `0f3a936`、`75b58b7`、`2d0992f`、`6427e46` |
| S2 | effect / submit ownership | 已完成 | `7f714ba` |
| S3 | OKX linear `InstrumentSpec` | 已完成 | `f913d39`、`7f019c7`、`de37b11`、`10ca055` |
| S4 | OKX inverse 与其他 venue | 已完成 | `1833c7c`、`ddb2e9a`、`a9613e1` |
| S5 | 账户估值与 PNL cashflow | 已完成 | `63cbf90`、`da3a3e1`、`2d94796` |
| S6 | queries、装配和最终删除 | 实施中（definition/query ownership、历史兼容清理、exchange port 归属、单一 venue 装配已完成） | `2ab98ac`、`1f29019`、`c4f300f`、`b90298b` |

### 切片 1：不可变 transition 与唯一更新入口

- 用现有类型先建立 `TradingRuntime::handle`；
- engine 直接返回候选 session state、events 和 effects；
- commands、market observation、order fact 各迁移一条代表性路径；
- 删除 `TrackMutationFrame`、rollback、`MutationExecutor` 和对应浅 service；
- 保持目标链、控制状态和 notification 测试通过。

### 切片 2：effect / submit ownership

- session queue 和 submit lifecycle 进入 application；
- effect runner 的交易所结果统一回送 `handle`；
- outcome unknown、stable client key、reconciliation revision 建立端到端测试；
- 删除 server preflight / coordinator 和旧 effect owner。

### 切片 3：OKX linear `InstrumentSpec`

- 贯通 metadata -> spec -> desired/normalized order -> wire contracts -> REST/WS canonical facts -> persistence/read model；
- 校验 `ctVal`、tick、lot、min quantity 和 min notional；
- 同一切片删除 OKX linear 的旧 `ExchangeRules` 消费入口。

### 切片 4：OKX inverse 与其他 venue

- 先覆盖 inverse contract value、contracts、base exposure、notional 和 settlement；
- 再依次迁移 Binance、Bybit、Hyperliquid；
- 每个 adapter 都运行相同的数值、normalize 和 REST/WS round-trip 契约；
- 最后删除共享层所有 symbol/settlement 猜测和剩余 `ExchangeRules`。

### 切片 5：账户估值与 PNL cashflow

- account-wide amount 与 per-asset balance 分离；
- realized PNL、fee、funding 分别带 asset 持久化；
- 实现 complete / incomplete aggregation 与 loss-guard 行为；
- recent PNL 加入 requested/covered window 和 completeness；
- 当前 schema 的持久化与恢复通过测试。

### 切片 6：queries、装配和最终删除

- queries 只组装 committed snapshot 和 persisted history；
- 删除 definition registry、RuntimePorts 和重复 projector 计算；
- 删除旧 schema 迁移、legacy identity 接纳和其他仅为过渡存在的代码；
- assembly 只保留一个 venue 分支和私有连接记录；
- 完成跨 crate 验收后再更新 `system-overview.md`，删除本文与调研文档。

## 12. 准确验收标准

只有以下全部成立，才算本次重构完成。

### 12.1 结构验收

- 所有会改变 track 的入口最终只调用 `TradingRuntime::handle`；没有调用方能分步执行 persist、install 或 enqueue。
- engine 不依赖 Tokio I/O、exchange port、persistence port、server 或 adapter。
- source 中不存在 `TrackMutationFrame` 和 track rollback API。
- source 中不存在 `TrackDefinitionRegistry`、`RuntimePorts`、`SubmitPreflight`、`SubmitCoordinator`。
- observation / command / effect service 如果只做转发已经删除；保留的模块必须拥有独立知识。
- `TrackTransition` 不含 frame，不跨出 engine，也不被持久化或投影到协议。
- server 不直接修改 engine state，不读取 session effect lifecycle。

### 12.2 状态与失败验收

- 对 transition、durable write、state install、notification、exchange dispatch 每个失败点都有测试。
- durable write 失败后 committed state、decision revision 和 effect queue 均不变化。
- notification 或 effect journal 失败不会回滚已经提交的业务状态。
- exchange network I/O 不持有 track lock。
- REST 对账只有 observation revision 与 queue revision 同时匹配时才能应用。
- fresh session 不恢复旧 bindings、gate、timer 或 queue。
- outcome unknown 时不会出现同一 intent 的替代 submit。

### 12.3 业务语义验收

- 四层目标链与 `DecisionReason` 来自同一 committed transition。
- pause/resume/flatten/terminate、带外策略、fresh acquisition、advantage/stale release、跨零和优先降风险测试与重构前行为一致。
- read model、protocol、TUI 不重算目标或执行原因。
- decision replay 使用与在线 engine 相同的 transition 路径。

### 12.4 交易所与单位验收

- 每个启用 instrument 在进入 runtime 前都有完整且验证通过的 `InstrumentSpec`。
- 共享 source 中不存在根据 venue/symbol 猜 quote/settlement 的逻辑。
- `ExchangeRules` 和绕过 spec 的 quantity/notional/base-exposure 计算入口已经删除。
- order、position、open order、fill、PNL record 的 quantity 在同一 instrument 上使用同一 canonical unit。
- OKX linear contracts/base、OKX inverse contracts/notional，以及 Binance、Bybit、Hyperliquid 都有固定数值和 round-trip 测试。
- metadata 缺失、单位不匹配、无法落在 wire step 或低于最小订单时 fail closed。
- open-orders 完整性、cancel accepted 与 terminal 的区别、recent PNL 覆盖窗口和显式 leverage 都有 adapter contract tests。
- 不同 asset 的 account amount 或 PNL cashflow 不能直接聚合；incomplete 聚合会阻止增加风险而不阻止降风险。

### 12.5 持久化与恢复验收

- identity、durable control、events、PNL cashflow 和 account baseline 可以从 SQLite 恢复。
- session state 与 effect queue 不从 `track_effects` journal 恢复。
- 历史 quantity 的 unit 和 contract value 含义不会被当前 metadata 改写。
- SQLite 只接受当前 schema；旧 schema 不迁移、不推断，启动明确失败并要求重新初始化。
- PNL backfill 的 source key 幂等，coverage incomplete 会进入 readiness blocker。

### 12.6 验证范围

- 每个切片先运行直接相关的最小测试；
- 改动跨 crate 后运行受影响 crate 测试；
- 最终运行 workspace tests、格式化和 lint；
- 使用代表性 fixture 对四个 venue 做 config、metadata、order normalize、REST/WS mapping、recovery 和 PNL coverage 验收；
- 最终 `rg` 检查上述应删除的旧类型、迁移入口和旁路公式，项目中不再保留对应实现或迁移测试。

## 13. 更新唯一依据的门槛

满足以下条件后，才更新 [system-overview.md](system-overview.md)：

1. 第 12 节全部验收通过；
2. 当前代码中已经不存在需要在旧架构与新架构之间解释的双轨；
3. `git diff` 和测试结果证明文档描述的是已实现行为，而不是目标设想；
4. 将本文中最终成立的模块、状态、交易所和单位语义合并进 `system-overview.md`；
5. 删除本文和 [exchange-abstraction-research.md](exchange-abstraction-research.md)，README 仍只指向一个长期系统文档。

在此之前：

- `system-overview.md` 回答“当前系统如何工作”；
- 本文回答“这次重构要改成什么，以及完成标准是什么”；
- 调研文档只回答“这些交易所和单位结论来自哪些证据”。

三者角色不同，不能相互替代。
