# Track Session Runtime Fresh-Start 设计

**日期：** 2026-04-23
**基线：** `codex/boundary-ledger-executor`

相关文档：

- 边界账本执行器设计：[2026-04-22-curve-boundary-ledger-execution-design.md](2026-04-22-curve-boundary-ledger-execution-design.md)
- Track definition 与 runtime 边界：[2026-04-09-track-definition-runtime-boundary-design.md](2026-04-09-track-definition-runtime-boundary-design.md)
- Runtime 启动 bootstrap 边界：[2026-04-18-runtime-startup-bootstrap-boundary-design.md](2026-04-18-runtime-startup-bootstrap-boundary-design.md)

相关代码：

- Track definition：[`../../../application/src/track_definition.rs`](../../../application/src/track_definition.rs)
- runtime：[`../../../engine/src/runtime.rs`](../../../engine/src/runtime.rs)
- manager：[`../../../engine/src/manager.rs`](../../../engine/src/manager.rs)
- startup bootstrap：[`../../../server/src/runtime/startup_bootstrap.rs`](../../../server/src/runtime/startup_bootstrap.rs)
- user task：[`../../../server/src/runtime/user_data.rs`](../../../server/src/runtime/user_data.rs)

## 1. 问题

当前实现虽然已经把执行器主语义切到 `boundary ledger + binding`，但重启后的启动路径仍然保留了旧设计的惯性：

- 启动仍然在旧 `TrackRuntime` 上做局部 reset，而不是从新会话重新构建
- startup cleanup 和新 session 构建还混在同一条流程里
- 旧会话 effect、follow-up retirement、旧 targeting 结果和旧 executor 进度没有彻底从语义上作废
- 启动 replay 与 steady-state 交接仍然容易按时间顺序拼接，而不是按明确阶段交接

这说明系统现在还没有真正实现：

> 重启后开启一个新的执行会话，只保留定义和业务历史，不保留上一会话的本地执行状态。

## 2. 目标

这次设计只解决一件事：

> 把 `track` 的跨重启保留数据与会话执行态彻底分开，并把启动流程改成显式的 `cleanup -> fresh session bootstrap -> steady-state handoff` 三阶段。

具体目标：

- 旧本地执行状态在重启后全部作废
- inherited order 只属于 cleanup 阶段，不参与新 session 语义构建
- 新 session runtime 只由定义、持久业务状态、交易所真实仓位和当前有效市场数据构建
- startup 结束后，steady-state runtime 不再保留任何 startup 专用知识

## 3. 非目标

- 不改变单曲线 boundary-ledger 执行器内核
- 不恢复旧 `round + slot` 语义
- 不让交易所 live order 重新成为本地 binding 的恢复真值
- 不保留旧 session 的 pending / executing work
- 不在这一轮引入多曲线或多账户预算模型

## 4. 设计原则

### 4.1 只保留真正跨重启有意义的数据

重启后保留的数据必须满足两个条件：

- 它在新会话里仍然有业务意义
- 它不是上一会话的本地推导物

因此，跨重启应保留的是：

- `TrackDefinition`
- `TrackControlState`
- `TrackLedgerState`

不应保留的是：

- bindings
- pending / executing effects
- follow-up retirements
- recovery anomaly
- desired exposure
- active risk cap
- boundary ledger progress
- startup cleanup filters
- 任何旧 session 的本地身份或中间态

### 4.2 交易所旧订单只属于 cleanup，不属于 session bootstrap

inherited open orders 的作用只有两个：

- 确认当前标的上存在旧订单
- 在启动阶段把它们清理掉

它们不是新 session runtime 的输入，也不参与本地 binding 恢复。

### 4.3 新 session 只接受外部真值

新 session 的构建输入固定为：

- `TrackDefinition`
- `TrackControlState`
- `TrackLedgerState`
- `FreshSessionExternalInputs`

这里的“当前交易所真实仓位”是唯一物理仓位真值，仍然表现为 `current_exposure`。

`FreshSessionExternalInputs` 是启动阶段查询到的当前外部事实，不是旧 session 状态。第一阶段定义为：

```rust
pub struct FreshSessionExternalInputs {
    pub current_exposure: Exposure,
    pub market_data: Option<CurrentMarketData>,
    pub exchange_rules: ExchangeRules,
}

pub struct CurrentMarketData {
    pub strategy_price: f64,
    pub mark_price: Option<f64>,
    pub execution_quote: ExecutionQuote,
    pub observed_at: DateTime<Utc>,
}
```

其中：

- `current_exposure`：来自交易所当前真实仓位
- `market_data`：当前有效行情；如果没有，或缺少执行报价，则新 session 进入 `WaitingMarketData`
- `exchange_rules`：当前标的的交易规则，包括 price tick、quantity step、min qty、min notional 和手续费参数

`exchange_rules` 不是会话状态，也不是从旧 runtime 恢复出来的字段。它必须由 startup 阶段从配置或交易所规则缓存的单一 owner 读取，并作为 `TrackRuntime::fresh_start(...)` 的显式输入。

本轮不引入新的账户级动态预算系统。若未来需要把可用保证金、账户风险占用或跨 track 容量纳入下单约束，应新增独立的外部约束输入，而不是复用旧 `CapacityBudget` 或旧 runtime 字段。

### 4.4 startup 是显式阶段，不是 steady-state 的特例

startup 有明确入口和出口：

- 入口前，不运行正常 reconcile
- 出口后，不再保留 startup cleanup 的忽略规则或专用状态

这条原则要求 startup 和 steady-state 的交接必须是完整的 phase handoff，而不是多处时间过滤的拼接。

## 5. 数据边界

### 5.1 `TrackDefinition`

`TrackDefinition` 是跨重启保留的定义层输入。它回答：

> 这条 track 是什么，以及它在长期配置上允许做什么。

第一阶段定义为：

```rust
pub struct TrackDefinition {
    pub id: TrackId,
    pub instrument: Instrument,
    pub config: TrackConfig,
    pub max_notional: f64,
    pub loss_limits: LossLimits,
}

pub struct LossLimits {
    pub daily_loss_limit: f64,
    pub total_loss_limit: f64,
}
```

这里的语义边界是：

- `config`：曲线与执行几何参数
- `max_notional`：track 级仓位限制
- `loss_limits`：亏损保护阈值

不再使用 `CapacityBudget` 这种把仓位限制和亏损限制混在一起的结构。

### 5.2 `max_notional` 的语义

`TrackConfig` 已经给出曲线天然最大仓位：

- `curve_long_max_notional = long_exposure_units * notional_per_unit`
- `curve_short_max_notional = short_exposure_units * notional_per_unit`
- `curve_max_notional = max(curve_long_max_notional, curve_short_max_notional)`

`max_notional` 不是曲线派生值，而是 track 级显式上限。最终有效仓位限制是：

```text
effective_max_notional = min(curve_max_notional, max_notional)
```

也就是说：

- 曲线给出理论上限
- `max_notional` 给出配置上限
- 两者取更保守的那个

### 5.3 `TrackControlState`

`TrackControlState` 表示会影响新会话行为的持久控制状态。它不属于执行中间态，也不属于定义层。

它必须是一个封闭集合，第一阶段定义为：

```rust
pub enum TrackControlState {
    Enabled {
        mode: PersistedControlMode,
    },
    Paused {
        resume_mode: PersistedControlMode,
    },
    Terminated {
        cause: TerminationCause,
    },
}

pub enum PersistedControlMode {
    Automatic,
    ManualFlatten,
    ManualTargetOverride {
        target: Exposure,
    },
}
```

这类状态必须跨重启保留，否则重启后会把产品层控制语义丢掉。

`TrackControlState` 的唯一写入来源是产品控制命令或对应的持久业务事件，例如：

- 创建或启用 track：写入 `Enabled { mode: Automatic }`
- 暂停 track：写入 `Paused { resume_mode }`
- 恢复 track：写入 `Enabled { mode: resume_mode }`
- 终止 track：写入 `Terminated { cause }`
- 手动 flatten：写入 `Enabled { mode: ManualFlatten }`
- 手动目标仓位：写入 `Enabled { mode: ManualTargetOverride { target } }`
- 回到自动模式：写入 `Enabled { mode: Automatic }`

startup 只能读取已经持久化的 `TrackControlState`，不能从上一会话的 `TrackState`、runtime snapshot 或 session transient state 推导它。

这里明确不保留当前 `TrackState` 里的会话瞬时状态：

- `WaitingMarketData`
- `Frozen`
- `FlattenPending`
- `Flattening`

这些状态都必须在新 session 中根据当前配置、当前价格和当前仓位重新计算。

如果实现阶段需要一次性迁移旧数据，迁移脚本可以临时定义旧 `TrackState` 到 `TrackControlState` 的转换规则，但这不是 startup runtime 的恢复路径，也不应留在 fresh-session bootstrap 里。

### 5.4 `TrackLedgerState`

`TrackLedgerState` 表示 track 级已实现账本真值。它不只是 pnl 数值，还包含 daily / cumulative fee、funding 和未补平的 ledger gap。

第一阶段至少需要包含：

- `ledger_utc_day`
- `gross_realized_pnl_today`
- `gross_realized_pnl_cumulative`
- `trading_fee_today`
- `trading_fee_cumulative`
- `funding_fee_today`
- `funding_fee_cumulative`
- `unresolved_gaps`

可以表示为：

```rust
pub struct TrackLedgerState {
    pub ledger_utc_day: NaiveDate,
    pub gross_realized_pnl_today: f64,
    pub gross_realized_pnl_cumulative: f64,
    pub trading_fee_today: f64,
    pub trading_fee_cumulative: f64,
    pub funding_fee_today: f64,
    pub funding_fee_cumulative: f64,
    pub unresolved_gaps: Vec<LedgerGapRecord>,
}
```

这里不把下列值持久化成独立真值：

- `net_realized_pnl_today`
- `net_realized_pnl_cumulative`
- `total_pnl`
- `unrealized_pnl`

其中：

- `net_realized_pnl_today = gross_realized_pnl_today - trading_fee_today + funding_fee_today`
- `net_realized_pnl_cumulative = gross_realized_pnl_cumulative - trading_fee_cumulative + funding_fee_cumulative`
- `unrealized_pnl` 由当前仓位和当前价格或账户快照重新得到
- `total_pnl = net_realized_pnl_cumulative + unrealized_pnl`

也就是说，`TrackLedgerState` 只保存 track 级已实现账本真值，不保存会随当前行情变化的浮动值。

这里的 `today` 明确按 UTC 解释，而不是本地时区或交易所各自口径。

也就是说：

- `ledger_utc_day` 是所有 `*_today` 字段所属的 UTC 日 bucket
- `gross_realized_pnl_today / trading_fee_today / funding_fee_today` 永远只表示 `ledger_utc_day` 这一天的值
- cumulative 字段表示跨日累计值，不随 UTC 日切归零
- `unresolved_gaps` 表示账本里尚未补平的记账缺口记录，不是 pnl 数值本身

`TrackLedgerState` 必须有单一 owner，负责：

- 解释 UTC 日边界
- 在跨日时 rollover `ledger_utc_day`
- 把所有 `*_today` 字段归零并保留 cumulative 字段
- 追加 ledger delta 和 unresolved gap
- 提供 net realized pnl 的派生接口

这套规则不能分散到 startup、risk guard、projector 或 storage mapper 多处各自解释。

在 fresh-session bootstrap 之前，必须先由这个 owner 用当前 UTC 日期对 `TrackLedgerState` 做标准化：

- 若 `ledger_utc_day == current_utc_day`，原样保留
- 若 `ledger_utc_day != current_utc_day`，先 rollover 到新的 `ledger_utc_day`，再把标准化后的结果交给 fresh-session bootstrap

### 5.5 Track session runtime

Track session runtime 是概念边界，对应当前代码里的 `TrackRuntime` 类型，不新增同名包装类型。它回答：

> 在这次进程生命周期里，执行器当前如何根据最新外部事实工作。

它包括：

- 当前仓位视图
- 当前 live quote / market data
- boundary ledger
- bindings
- runtime-only risk projection
- pending / executing work
- recovery anomaly

重启后它整体作废，不参与恢复。

## 6. fresh session 的构造规则

### 6.1 新 session 输入

新 session 只能由以下输入构造：

- `TrackDefinition`
- `TrackControlState`
- `TrackLedgerState`
- `FreshSessionExternalInputs`

不允许读取上一会话的：

- executor snapshot
- binding 列表
- pending submit hints
- boundary progress
- startup cleanup filter

### 6.2 新 session 初始状态

给定当前真实仓位 `current_exposure`，新 session runtime 的执行内核初始化为：

```text
boundary ledger:
  profile_revision = profile_revision_for_config(config)
  ledger_anchor_exposure = current_exposure
  progress = empty

bindings = empty
recent_terminal_orders = empty
recovery_anomaly = none
desired_exposure = none
active_risk_cap = none
```

若此时没有当前有效市场数据：

- runtime 进入 `WaitingMarketData`
- 不沿用上一会话的 `strategy_price`
- 不沿用上一会话的 `desired_exposure`

### 6.3 `Executing` 语义

`Executing` 不是跨重启保留的持久工作语义。

因此 fresh-session 的规则是：

- 旧会话遗留的 `Pending` effect 全部 `Superseded`
- 旧会话遗留的 `Executing` effect 也全部 `Superseded`
- 旧会话遗留的 `follow_up_retirements` 全部删除

本次设计不允许留下“上一会话 admitted 但未完成”的 effect 状态继续阻塞新会话批次。

## 7. startup 三阶段

### 7.1 Phase A: `InheritedOrderCleanup`

职责：

- 查询当前标的的 open orders
- 若存在 inherited orders，则对当前标的执行 `cancel_all(instrument)`
- 等待当前标的 open orders 清空

输入：

- `instrument`
- execution port

输出：

- cleanup 完成
- 一个只存在于 startup 阶段内部的 `CleanupTracker`

`CleanupTracker` 持有：

- 本次 startup cleanup 创建的 cleanup identity 集合
- 每个 cleanup identity 当前是否已经解析为终态
- startup replay 期间需要忽略的预期 cleanup update

它是 startup 私有 owner，不进入 steady-state runtime。

### 7.2 Phase B: `FreshSessionBootstrap`

职责：

- 清空旧会话本地执行状态
- 读取 `FreshSessionExternalInputs`
- 调用 `TrackRuntime::fresh_start(...)` 构建新 session runtime

这个阶段只负责调用构造入口，不拥有“如何从定义与外部真值构造新 runtime”的规则。构造规则由 `TrackRuntime::fresh_start(...)` 自己拥有，不允许分散到 startup bootstrap、manager、mutation executor 和测试夹具里。

### 7.3 Phase C: `SteadyStateHandoff`

职责：

- 建立 cleanup barrier
- 取得候选 steady-state cutoff
- 把 `event_time <= candidate_cutoff` 的 buffered user-data 完整 replay 给 startup phase
- 若 replay 后 cleanup 状态又发生变化，则重新等待 barrier 并取得新的 cutoff
- 然后把 receiver 移交给 steady-state user task

steady-state user task 只处理：

```text
event_time > steady_state_cutoff
```

startup replay 和 steady-state task 之间不允许有时间空窗，也不允许重叠消费。

这里的前提不是“cleanup 已经发起”，而是：

> cleanup 相关的旧订单回报已经在 startup phase 内被完整吸收或解析完毕。

## 8. Startup replay 与 steady-state 交接

### 8.1 正确语义

startup replay 必须消费到最终交接边界，而不是消费“当前缓冲区里已经有的事件”。

因此不允许采用这种做法：

- `try_recv` 一遍当前缓冲区
- 之后再取新的 cutoff
- steady-state 再按新的 cutoff 丢事件

因为这会在 replay 结束与最终 cutoff 取得之间留下事件空窗。

### 8.2 唯一允许的交接方式

第一阶段只允许一种交接模型：startup phase 独占 receiver，反复建立 barrier，直到 cleanup quiesced 和 cutoff drain 在同一次循环里同时成立。

1. startup phase 独占 user-data receiver
2. `InheritedOrderCleanup` 发起当前标的 cleanup
3. startup phase 持续消费 user-data，并把 cleanup 相关 update 交给 `CleanupTracker`
4. 等待 `CleanupTracker` 达到 quiesced
5. startup phase 查询最终外部真值：
   - 当前仓位
   - 当前标的 `ExchangeRules`
   - 当前 open orders
   - 当前有效市场数据
6. 调用 `TrackRuntime::fresh_start(...)` 构建新 session runtime
7. 取得候选 `steady_state_cutoff`
8. startup phase replay / drain 所有 `event_time <= steady_state_cutoff` 的 buffered event
9. 若第 8 步吸收到 cleanup 相关 update、改变了 `CleanupTracker` 或发现新的 cleanup gap，则回到第 4 步
10. 若第 8 步完成后 `CleanupTracker` 仍然 quiesced，且 `TrackRuntime` 仍由最新外部真值构建，则把 receiver 移交给 steady-state user task

steady-state user task 只按通用 cutoff 过滤：

```text
event_time > steady_state_cutoff
```

它不接收 cleanup identity，也不持有 cleanup filter。

这个模型必须满足：

- `CleanupTracker` 在最终 drain 之后仍然 quiesced
- startup cleanup 产生的预期 order update 只在 startup replay 内忽略
- steady-state user task 不保留 startup cleanup 专用过滤规则
- steady-state 只知道 cutoff，不知道 cleanup identity

`CleanupTracker.quiesced` 的第一阶段定义为同时满足：

1. 当前标的最新 open-order snapshot 已经为空
2. 每个 cleanup identity 都已经在 startup phase 内解析为以下之一，并完成 ledger 影响处理：
   - `Canceled`
   - `Filled`
   - `Expired`
   - `Rejected`
   - `AbsentAfterEmptySnapshotWithGap`

其中 `AbsentAfterEmptySnapshotWithGap` 表示：

- 这张 inherited order 没有再出现在当前标的最新空 snapshot 里
- 但 startup phase 没有拿到足够信息确认它的完整终态或 ledger delta
- 因此必须向 `TrackLedgerState.unresolved_gaps` 追加一条 cleanup gap

也就是说，cleanup 不能用“订单已经不在 open orders 里”静默吞掉可能存在的成交、手续费或资金费影响。无法确定的部分必须显式进入账本 gap。

只有当 `CleanupTracker` 在最终 cutoff drain 之后仍然 quiesced，steady-state handoff 才允许发生。

## 9. 模块 owner

### 9.1 `application/src/track_definition.rs`

owner：

- `TrackDefinition`
- `LossLimits`
- `max_notional` 语义

### 9.2 持久业务状态 owner

需要新增 application-owned 模块，明确拥有：

- `TrackControlState`

它不能继续混在旧 session runtime snapshot 里表达。

### 9.3 持久存储边界 owner

需要新增或重写 storage/application 边界，明确拥有：

- `TrackPersistentState`
- `TrackControlState` 的读写
- `TrackLedgerState` 的读写

第一阶段的形状是：

```rust
pub struct TrackPersistentState {
    pub track_id: TrackId,
    pub control_state: TrackControlState,
    pub ledger_state: TrackLedgerState,
}
```

fresh-session startup 只能从这个持久状态 owner 读取跨重启真值。

`TrackRuntimeSnapshot`、`PersistedRuntimeCodec` 和旧 `track_snapshots` 表不能继续作为 fresh-session 的恢复输入。实现时可以删除旧 snapshot 存储，也可以临时保留为调试/审计输出，但必须满足：

- startup 不读取旧 runtime snapshot 来恢复执行语义
- `TrackControlState` 和 `TrackLedgerState` 不通过旧 runtime snapshot 间接暴露
- 任何保留的 snapshot 都不能成为新 session 构造规则的 owner

### 9.4 `engine/src/ledger.rs`

owner：

- `TrackLedgerState`
- `ledger_utc_day` 的 rollover
- gross / fee / funding 的 daily / cumulative 记账
- `unresolved_gaps`
- net realized pnl 的派生接口

它不能继续被当成“session-only runtime 状态”，而应作为跨重启保留的 track 级账本真值。

### 9.5 `server/src/runtime/startup_bootstrap.rs`

owner：

- inherited-order cleanup 阶段
- startup replay
- startup 与 steady-state 的交接编排

这里的阶段名是 `startup_bootstrap.rs` 内部流程边界，不要求新增 public type。该模块不拥有新 session runtime 的内部构造规则，只负责阶段编排。

### 9.6 `TrackRuntime::fresh_start`

现有 `TrackRuntime` 自己拥有 fresh-session 构造规则，通过一个唯一入口表达：

```rust
impl TrackRuntime {
    pub fn fresh_start(
        definition: TrackDefinition,
        control_state: TrackControlState,
        ledger_state: TrackLedgerState,
        external_inputs: FreshSessionExternalInputs,
        started_at: DateTime<Utc>,
    ) -> Self
}
```

这个方法负责从 `TrackDefinition + TrackControlState + TrackLedgerState + FreshSessionExternalInputs` 构造新的 `TrackRuntime`。

`FreshSessionExternalInputs` 必须显式携带当前真实仓位、当前有效市场数据和当前标的 `ExchangeRules`。`TrackRuntime::fresh_start(...)` 不允许读取旧 runtime、旧 snapshot、manager 全局字段或 startup cleanup state 来补齐缺失输入。

startup 只调用这个方法，不内联持有这套构造规则。除非后续构造逻辑复杂到 `TrackRuntime` 无法清晰承载，否则不引入额外 factory / bootstrapper 类型，也不新增 `TrackSessionRuntime` 包装类型。

## 10. 读模型与业务历史

重启后需要保留的是业务历史，而不是旧 session 执行态。

例如：

- realized pnl 统计
- 累计手续费
- 历史事件
- 产品控制状态

这些信息应该继续由读模型和持久业务状态使用，但不允许反向变成新 session 执行内核的恢复输入，除非它们本来就是定义层或风险真值的一部分。

## 11. 验收标准

### 11.1 fresh-session

- 重启后不会延续任何旧会话本地 effect，包括 `Pending` 和 `Executing`
- 重启后不会恢复旧 binding、旧 boundary progress、旧 desired target
- 新 session 的 boundary ledger anchor 等于当前真实仓位

### 11.2 startup 阶段

- inherited orders 只影响 cleanup，不参与新 session runtime 构建
- startup cleanup 规则不会泄漏到 steady-state user task
- startup replay 与 steady-state handoff 没有丢事件窗口

### 11.3 定义边界

- `TrackDefinition` 不再使用 `CapacityBudget`
- `max_notional` 与 `loss_limits` 分开表达
- 风险模块消费 `LossLimits`
- 仓位上限逻辑消费 `config + max_notional`

## 12. 实施方向

这份设计会对应一份单独的实施计划：

- 先拆定义层和持久业务状态
- 再重写 fresh-session 构造规则
- 再重写 startup 三阶段和交接边界

不再在旧 `TrackRuntimeSnapshot` 上做“多清几个字段”的局部修补。
