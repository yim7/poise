# Track Protection State Model Design

## 背景

当前 track 运行时把几类不同知识平铺在一起：

- 策略在当前价格下想要的目标
- 带外保护策略及其恢复规则
- 亏损触发后的处理
- 手动覆盖命令
- 执行层安全门

这带来了 3 个持续扩大的问题：

- `desired_exposure` 同时像计算结果和持久化状态
- `TrackStatus` 同时承载生命周期、保护模式和人工覆盖语义
- 带外恢复、亏损终止、执行限制的知识分散在多个层里，容易继续靠补丁字段和特殊分支演进

这份设计稿的目标不是再补一个字段，而是把不同类型的知识拆到各自拥有者下面。

## 目标

- 把 `policy`、运行时 `state`、领域 `outcome` 明确拆开
- 让 `desired_exposure` 回到派生值
- 把带外保护定义为显式 policy，而不是隐式散落的规则
- 把亏损触发定义为默认终止，而不是 `Cap(0)`
- 让 engine 私有状态不再泄漏到 read model、projector 和 server 测试夹具
- 保持执行安全门独立，不并入主状态机

## 非目标

- 本设计不引入新的价格止损线或 trailing stop 机制
- 本设计不改变执行器下单方式
- 本设计不把 `TrackState` 暴露成对外协议或 server/projector 的输入抽象
- 本设计不把缺盘口、价格偏离、market data stale 全部并入主状态机

## 设计结论

### 1. 先区分 4 类对象

系统里需要明确区分 4 类对象：

1. `Policy`
2. `StrategyIntent`
3. `Runtime Track State`
4. `ExecutionGate` 和对外投影

它们不是一层东西，不能继续混在同一组平铺字段里。

### 2. `Policy` 表达静态决策

`Policy` 表达“应该如何保护”和“应该如何恢复”，不表达当前是否已经触发。

带外相关 policy 建议显式建模为：

```rust
enum BandProtectionPolicy {
    Freeze { recover: BandRecoverPolicy },
    Hold,
    Flatten { recover: BandRecoverPolicy },
    Terminate,
}

enum BandRecoverPolicy {
    BackInBand,
    PriceConfirm { bps: u32 },
}
```

设计要求：

- `freeze / hold / flatten / terminate` 的选择属于 policy
- 回带内价格确认的参数属于 policy
- policy 本身不记录 breach side
- 亏损处理当前固定为 `terminate`，先不额外抽一个只有单一变体的 `LossPolicy`

ownership 建议：

- `core::strategy` 拥有 `BandProtectionPolicy`
- `core::strategy` 拥有 `BandRecoverPolicy`
- `engine` 只消费这些 policy，并维护运行时 guard

这样后续调整确认阈值时，不会把同一知识拆到配置、引擎逻辑和存储 3 处。时间确认或非对称确认先作为未来扩展，不在第一版公开配置形状里提前占位。

### 3. `StrategyIntent` 只负责策略目标

`StrategyIntent` 只回答一个问题：

- 在没有保护干预时，当前价格下策略希望得到什么目标仓位

它只由策略本身和市场输入决定，不携带持久化 guard，不表达人工覆盖，也不负责解释亏损终止。

`desired_exposure` 是 `StrategyIntent` 经过保护层和执行层处理后的最终派生结果，不是主状态的一部分。

### 4. `TrackState` 才是 source of truth

真正需要持久化、恢复和命令控制的是完整的运行时 `TrackState`，而不是 `desired_exposure`。

建议主状态结构：

```rust
enum TrackState {
    WaitingMarketData,
    Running(ControlState),
    Paused { suspended: ControlState },
    Terminated { cause: TerminationCause },
}

enum ControlState {
    Automatic(AutoState),
    Manual(ManualState),
}

enum AutoState {
    FollowingBand,
    Frozen { target_anchor: Exposure, guard: ReentryGuard },
    Holding { target_anchor: Exposure },
    Flattening { guard: ReentryGuard },
}

enum ManualState {
    Flattened,
    TargetOverride { target: Exposure },
}
```

这里的 `TrackState` 才是 source of truth。  
对外展示的 `TrackStatus` 以后应当从它派生，而不是继续作为主状态。

设计要求：

- `ManualState::Flattened` 和 `TargetOverride { target }` 必须分开
- 不再用 `target + reason` 的组合表达手动 flatten
- 像“`FlattenCommand` 配非零 target”这样的无效组合，应通过状态形状直接设计掉
- `target_anchor` 是进入 `Frozen` / `Holding` 前最后一个经过 risk 批准的目标仓位
- 如果当次没有可用的 risk-approved target，`target_anchor` 才回退为当前仓位
- `target_anchor` 不是当前实际仓位，也不是 executor active-round anchor
- `Frozen` 每次 reconcile 继续以 `target_anchor` 作为目标，恢复确认通过后清除 `target_anchor + guard` 并重新跟随当前策略目标
- `Holding` 每次 reconcile 继续以 `target_anchor` 作为目标，只能通过 resume 或人工命令清除
- risk cap 可以压低本轮派生目标，但不能反向改写已经采样的 `target_anchor`

### 5. `ReentryGuard` 只保存运行时记忆

自动带外价格确认的恢复条件由显式 guard 持有，但 guard 只保存运行时记忆，不保存 policy 参数。

建议形状：

```rust
struct ReentryGuard {
    boundary: BandBoundary,
}
```

语义要求：

- `boundary` 记录最初从哪一侧触发带外保护
- 第一版只保存价格确认需要的 breach side
- 任何自动回带内确认的运行态都必须携带 guard；第一版包括 `Frozen` 和 `Flattening`
- 如果未来增加时间确认，再同 task 引入对应的时间戳字段、时间源、离开确认区重置语义和恢复后清理语义

关键边界：

- `bps` 这类确认阈值属于 `BandRecoverPolicy`
- `boundary` 属于 `ReentryGuard`

也就是说：

- policy 决定“怎样才算确认”
- guard 决定“当前观察到了什么”

这样可以避免把配置和运行时记忆混成一个结构。

### 6. 带外保护和带外终止的关系

带外不再被当成纯策略细节，而是被定义为一种可配置保护。

对应关系：

- `Freeze` policy -> `Running(Automatic(Frozen { target_anchor, guard }))`
- `Hold` policy -> `Running(Automatic(Holding { target_anchor }))`
- `Flatten` policy -> `Running(Automatic(Flattening { guard }))`
- `Terminate` policy -> `Terminated { cause: TerminationCause::Band(BandTerminationCause::OutOfRange) }`

这意味着：

- 带外 `terminate` 不属于 `AutoState`
- 它是生命周期终态

### 7. 亏损默认终止，但 risk 只返回 risk 自己的结果

亏损触发不再定义为“压到 0 并等待恢复”，而是默认终止。

但 `core::risk` 不应直接返回顶层 `TerminationCause`。  
更合理的是让 risk 只返回自己的领域结果：

```rust
enum RiskOutcome {
    Allow { target: Exposure },
    Cap { target: Exposure },
    Terminate(RiskTerminationCause),
}

enum RiskTerminationCause {
    DailyLossLimit,
    TotalLossLimit,
}
```

然后由 engine/controller 做映射：

```rust
enum TerminationCause {
    ManualCommand,
    Band(BandTerminationCause),
    Risk(RiskTerminationCause),
}

enum BandTerminationCause {
    OutOfRange,
}
```

第一版对应关系：

- `max_notional` 超限 -> `Cap`
- `daily_loss_limit` 触发 -> `Terminate(DailyLossLimit)`
- `total_loss_limit` 触发 -> `Terminate(TotalLossLimit)`

设计要求：

- risk 层不需要知道 `ManualCommand`
- risk 层不需要知道 `BandOutOfRange`
- risk 层第一版不处理账户容量不足；账户容量不足归属于 `ExecutionGate / AccountCapacityGate`
- controller 负责把 risk 结果映射为顶层生命周期变化

### 8. 手动 flatten 不是 terminate

手动 `Flatten` 仍然保留为可恢复的人为覆盖：

```rust
ControlState::Manual(ManualState::Flattened)
```

手动设置目标则是：

```rust
ControlState::Manual(ManualState::TargetOverride { target })
```

手动 `Terminate` 则直接进入：

```rust
TrackState::Terminated {
    cause: TerminationCause::ManualCommand,
}
```

这三者必须继续分开：

- 手动 `Flatten`：还活着，只是人工压到 0，可 `resume`
- 手动 `TargetOverride`：还活着，以人工目标覆盖自动控制
- 手动 `Terminate`：生命周期结束，不可 `resume`

额外约束：

- `SetTarget(0)` 要么被拒绝，要么规范化成 `ManualState::Flattened`
- 不应同时保留“值为 0 的 target override”和“manual flatten”两种外部语义

### 9. `ExecutionGate` 不进入主状态机

以下情况继续留在执行安全层：

- 缺少盘口
- `mark_price` 与盘口偏离过大
- market data stale
- recovery anomaly
- 账户容量不足或 account margin guard 阻止加仓

它们负责回答：

- 现在能不能自动下单
- 是否只允许减风险
- 是否进入 `attention_required`

它们不应和带外保护、亏损终止混进同一套持久化控制状态。

建议保留独立接口，例如：

```rust
enum ExecutionGate {
    Open,
    ReduceOnly { reason: ExecutionGateReason },
    NoSubmit { reason: ExecutionGateReason },
}

enum ExecutionGateReason {
    MissingOrderBook,
    PriceDislocated,
    MarketDataStale,
    RecoveryAnomaly,
    AccountCapacityInsufficient {
        required_notional: f64,
        available_notional: f64,
    },
}

struct AccountCapacityGateInput {
    current: Exposure,
    approved_target: Exposure,
    unit_notional: f64,
    available_notional: Option<f64>,
}
```

`ExecutionGateReason` 应只有一份事件可见词汇，由共享事件契约拥有；engine 的 execution gate 决策直接使用它，不再额外定义一套同形 `engine::ExecutionGateReason` 再做转换函数。

账户容量的 owner 应明确为 `AccountCapacityGate`：

- 输入：当前仓位、risk 批准后的目标、单位名义价值、账户可用容量快照
- 输出：`ExecutionGate::Open`、`ReduceOnly` 或 `NoSubmit`
- 事件/诊断：使用 execution/account gate 语义，例如 `ExecutionGateApplied` 或 execution diagnostics
- 禁止：继续用 `RiskOutcome`、`RiskState.account_capacity_constraint` 或 `RiskDenied` 表达账户容量不足

这样可以保留“亏损 / max_notional 是 risk”，“账户当前是否允许加仓是 execution gate”的边界。

### 10. 持久化边界

engine 私有运行时状态应当被完整持久化，但不应继续作为共享快照顶层零散字段扩散。

这里的持久化根对象应当直接对齐 source of truth：

- `TrackState` 是 source of truth
- storage 也应持久化完整 `TrackState`，或与它等价的一份 engine 私有 `runtime_state`
- `ControlState` 是 `TrackState` 的一部分，不应单独成为另一套“真实状态”

推荐方向：

- 把 engine 私有主状态收敛为单独的 `track_state` 或 `runtime_state`
- 在 storage 中以独立持久化块保存，例如 `track_state_json` 或 `runtime_state_json`
- 由 engine restore 时恢复为内部状态机
- query / projector 默认只消费派生出的展示状态

这样可以避免未来新增：

- 时间确认
- 非对称确认
- 更多终止原因

时继续修改大量共享顶层结构和测试夹具。

也可以理解成：

- `TrackState` 决定生命周期和控制模式
- `desired_exposure`、`TrackStatus`、命令可用性都是从它派生
- storage 负责把这份主状态原样保存，而不是只保存其中一部分再靠恢复时重建其余语义

### 11. 对外展示

对外协议和 read model 继续保留简单状态名，但它们应当是派生值，例如：

- `WaitingMarketData`
- `Active`
- `Frozen`
- `Holding`
- `Flattening`
- `ManualFlattening`
- `Paused`
- `Terminated`

其中：

- `Flattening` 对应 `Running(Automatic(Flattening { .. }))`
- `ManualFlattening` 对应 `Running(Manual(Flattened))`
- `Terminated` 对应任何 `Terminated { cause: ... }`

read model 不需要感知 `ReentryGuard` 内部字段，除非后续明确需要对外展示。

额外边界：

- `TrackState` 可以出现在 engine runtime、engine snapshot、持久化恢复和 application 的单一适配层
- `TrackRuntimeReadState` 是 application 内部 read adapter，不应作为跨 crate public interface re-export
- `TrackRuntimeReadState` 不应携带完整 `TrackState`
- server / projector / server 测试夹具只消费 `TrackReadModel` 或等价的 application public projection，不直接接触 `TrackRuntimeReadState`
- server / projector / server 测试夹具不应构造 `AutoState`、`ManualState`、`ReentryGuard`
- server 生产代码和 server 测试都不应直接 import 或接收 `TrackRuntimeSnapshot`
- server/runtime 测试如果需要 durable seed，应通过 application test-support 或服务 API 获取公开 `TrackReadModel` / projector fixture，不直接接触 engine 私有 snapshot
- 如果需要测试 `TrackState -> TrackStatus`，测试应放在 application 适配层，而不是 server projector 层

### 12. 恢复语义

恢复规则固定如下：

- `Paused` 可以 `resume`
- `ManualState::Flattened` 可以 `resume`
- `Holding` 可以 `resume`
- 自动带外 `Frozen` 和 `Flattening` 不靠 `resume` 恢复，而靠 `BandRecoverPolicy + ReentryGuard`
- `Terminated` 不能用普通 `resume` 恢复

如果未来需要支持亏损后的重新启用，应使用单独命令，例如：

- `reactivate_after_loss`
- 或 `reset_after_termination`

而不是复用 `resume`。

## 模块 ownership

建议模块边界如下：

- `core::strategy`
  - 带内曲线和 band 语义
  - `BandProtectionPolicy`
  - `BandRecoverPolicy`
- `core::risk`
  - 风险阈值评估
  - `RiskOutcome`
  - `RiskTerminationCause`
- `core::events`
  - `DomainEvent`
  - `ExecutionGateReason` 这类事件可见 payload 词汇
- `engine/execution_gate`
  - `ExecutionGate`
  - `AccountCapacityGate`
  - 账户容量快照到执行门决策的映射
- `engine/controller`
  - `TrackState`
  - `ControlState`
  - `ReentryGuard`
  - policy 消费和状态迁移
  - risk outcome 到 `TerminationCause` 的映射
- `storage`
  - `TrackState` / `runtime_state` 的完整持久化和恢复
- `application/read_adapter`
  - `TrackState` / `runtime_state` 到 `TrackStatus` / `TrackReadModel` 的唯一适配
  - 内部 `TrackRuntimeReadState` 只作为 application 私有过渡对象
- `query / projector`
  - `TrackReadModel`、`TrackStatus` 等对外派生展示

这套分工的关键是：

- 每层只知道自己的领域结果
- 顶层生命周期语义只在 controller 汇合
- 存储负责完整保存 engine 私有状态，但不要求所有读模型都理解它

## 实现分组

建议按知识边界分组实现，而不是按时间顺序拆共享接口：

1. `core` 先新增 policy / outcome 类型和纯 helper，但不改变跨 crate 共享配置字段
2. `TrackConfig.out_of_band_policy` 这种共享配置形状，必须和所有消费者迁移放在同一个实现 task
3. `TrackRuntimeSnapshot` 根接口切到 `runtime_state` 时，只允许在 engine、storage、application 持久化和 application read adapter 边界内流动；server 直接 snapshot 消费必须在同一 task 改成 application 公开 API
4. `RiskState.account_capacity_constraint` / `RiskDenied` 这类账户容量表达，必须在同一实现 task 迁移到 `ExecutionGate / AccountCapacityGate` 语义
5. application 负责唯一的 `TrackState / runtime_state -> TrackRuntimeReadState -> TrackReadModel / TrackStatus` 适配，其中 `TrackRuntimeReadState` 仅限 application 内部
6. server / projector 只消费公开 read model，不消费 engine 内部状态

这样可以避免临时桥接、双轨字段和内部状态向上层泄漏。

## 验收标准

这套设计落地后，应满足：

- 带外响应策略和恢复规则由显式 policy 拥有
- 自动带外 `Frozen` / `Flattening` 的恢复条件由 `BandRecoverPolicy + ReentryGuard` 共同决定
- `ReentryGuard` 保存运行时记忆，而不是配置阈值
- `Frozen` / `Holding` 使用语义明确的 `target_anchor`，并定义采样、清除和 risk cap 交互规则
- 手动 `flatten` 和手动目标覆盖由不同状态变体表达
- 亏损触发后默认进入终止态，而不是可自动恢复的压零态
- risk 层不直接返回顶层生命周期原因
- `resume` 不再承担亏损恢复语义
- 执行安全门不进入主状态机
- 账户容量不足不再通过 risk 命名表达，而是由 `AccountCapacityGate` 输出执行门决策
- `TrackState` 作为 source of truth 与持久化根对象保持一致
- engine 私有状态不再以零散顶层字段泄漏到共享 snapshot 接口
- `TrackState` 不泄漏到 server/projector/read model 的公开输入结构
- `TrackRuntimeReadState` 不成为跨 crate 公共接口，server 只消费 `TrackReadModel` 或等价公开投影
- server 生产代码不直接 import 或接收 `TrackRuntimeSnapshot`
- execution gate reason 只有一份事件可见类型，不在 core 和 engine 之间重复定义再转换
- 跨 crate 共享字段的形状变更不允许单独落在一个 task 里，必须和所有直接消费者一起迁移
