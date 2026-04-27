# Boundary Policy 统一重构设计

**日期：** 2026-04-26
**基线：** `main` @ `774fe28`

相关文档：

- [2026-04-22-curve-boundary-ledger-execution-design.md](2026-04-22-curve-boundary-ledger-execution-design.md)
- [../plans/2026-04-22-boundary-ledger-executor.md](../plans/2026-04-22-boundary-ledger-executor.md)

## 1. 背景

当前 boundary ledger executor 已经完成了 `round + slot` 到 `boundary / ledger / binding / policy` 的迁移，但 Normal 模式下仍保留两套 policy 协议：

- `CatchUpPolicy` 负责 due boundary，通常聚合多个 due 操作为一张 aggressive 单。
- `CurveMakerPolicy` 负责未来 boundary，通常每个 operation 一张 passive maker 单。

这两套 policy 本身都不复杂，复杂度主要来自它们之间的交互：

- `CoverageReservation` 负责跨 policy 防止重复覆盖。
- `effective_maker_owner_indexes` 让 CurveMaker 在某些情况下等价覆盖 CatchUp。
- `replaceable_active_owner_indexes` 让 CatchUp 可以抢占 maker。
- `preexisting_cancel_pending_operations` 依赖 `plan()` 内部调用顺序，避免刚进入 grace 的 maker 阻塞同轮 CatchUp。

这些概念让 Normal planning 的正确性依赖“先拍快照、再推进 grace、再跑 CatchUp、再跑 CurveMaker、最后 reconcile”的执行顺序。它能工作，但调用顺序已经变成隐藏协议。

## 2. 设计目标

本次重构目标是降低 Normal planning 的概念数量，而不是改变交易行为。

目标：

- 由一个 `BoundaryPolicy` 拥有 Normal 模式下的 boundary 覆盖决策。
- 调用层不再知道 CatchUp 与 CurveMaker 的内部优先级协议。
- 调用层不再维护 `CoverageReservation` 或 `preexisting_cancel_pending_operations`。
- cancel-pending 仍然存在，但由 policy/reconciliation 内部解释。
- 保留现有生产行为：
  - passive maker：每个 future operation 一张单。
  - aggressive catch-up：due operations 可以聚合为一张单。
  - maker 进入 due 后先经历 grace，再被 aggressive 替换。
  - 已经 cancel-pending 的旧 owner 阻止同一 operation 重复提交。

非目标：

- 不删除 boundary progress ledger。
- 不改变 order fill 与 position update 的时序模型。
- 不改变外部 `ExecutionAction` / `OrderRequest` / effect 边界。
- read model 只做展示投影，不继续拥有 binding 专用 enum。
- 不改变 ManualOverride / ReduceOnly 的外层优先级。

## 3. 复杂度信号

主要复杂度信号是 cognitive load。

维护者必须同时记住：

- CatchUp 先于 CurveMaker。
- CurveMaker 被 due grace 推进后会先变成 cancel-pending。
- 但“本轮刚变成 cancel-pending”的 maker 又不应该阻塞同轮 CatchUp。
- 某些 active maker 可以等价覆盖 CatchUp，某些则需要被 CatchUp 替换。
- `CoverageReservation` 只描述 desired binding 之间的覆盖，不描述 existing binding。

这些知识应该由一个更深的 Normal boundary policy 吸收，而不是留给 `plan()` 的执行顺序。

## 4. 设计方向比较

### 方向 A：保留双 policy，只补注释和测试

优点：

- 改动最小。
- 当前行为最稳定。

缺点：

- 只是给隐藏协议加说明，没有减少协议本身。
- 后续新增执行策略仍要理解 reservation、effective owner、replace owner 的交互。

结论：适合作为第一批保护性清理，不是最终设计。

### 方向 B：只删除 `CoverageReservation`

优点：

- 能减少一部分跨 policy 状态。

缺点：

- effective maker owner、replaceable active owner、preexisting cancel-pending 顺序仍然存在。
- 只是删除一个症状，Normal planning 仍由多个浅层协议拼起来。

结论：不单独采用。

### 方向 C：引入统一 `BoundaryPolicy`

优点：

- Normal 模式下只有一个模块拥有 boundary 覆盖决策。
- due/passive/aggressive/grace/cancel-pending 的解释在一个边界内完成。
- `plan()` 只需要准备 ledger view、调用 policy、应用 binding reconciliation。

缺点：

- 需要补充验收测试锁住现有行为，避免把执行语义误改成“每操作一单”或“所有操作都聚合”。

结论：采用。

## 5. 目标抽象

### 5.1 模块 ownership

- `boundary.rs` 继续拥有 boundary identity、方向、触发价格和离散化。
- `ledger.rs` 继续拥有 remaining、due、expected exposure。
- `binding.rs` 继续拥有 live order binding、状态、proposal key、回报吸收所需状态。
- `policy.rs` 拥有 Normal 模式下的 boundary 覆盖决策：
  - 选择 future maker operations。
  - 选择 due catch-up operations。
-  - 在同一次选择中隐藏 due operation 对 future maker selection 的预留关系。
- `planning.rs` 只做编排：
  - 建立 ledger view。
  - 应用全局 gate。
  - 调用 policy。
  - policy 输出 Normal 模式 desired binding。
  - 根据 policy 的 reconciliation decision 执行复用、取消、替换或提交。

### 5.2 简化后的接口

目标接口形态：

```rust
pub struct PolicyPlanningInput<'a> {
    pub view: &'a BoundaryLedgerView,
    pub boundaries: &'a [BoundaryBlueprint],
    pub instrument: &'a Instrument,
    pub config: &'a TrackConfig,
    pub exchange_rules: &'a ExchangeRules,
    pub base_qty_per_unit: f64,
    pub min_rebalance_units: f64,
    pub current_exposure: &'a Exposure,
    pub desired_exposure: &'a Exposure,
    pub execution_quote: Option<ExecutionQuote>,
    pub submit_purpose: SubmitPurpose,
    pub exposure_epsilon: f64,
    pub curve_maker_levels_per_side: usize,
}

pub fn plan_policy_bindings(
    context: PolicyContext,
    input: &PolicyPlanningInput<'_>,
) -> Vec<DesiredBinding>;

pub fn classify_binding_reconciliation(...) -> BindingReconciliationDecision;
```

`BoundaryPolicy` 不再只是 operation selector。它拥有 Normal 模式的 coverage 决策，包括：

- CatchUp due operation 聚合。
- CurveMaker per-operation passive binding。
- passive maker 是否还能覆盖 CatchUp。
- maker grace 过期后是否允许 aggressive replacement。
- cancel-pending owner 是否阻止重复提交。

`planning.rs` 只保留外层 context 分流、price gate、ledger anomaly、以及把 reconciliation decision 转成 cancel/submit effect 的机械执行。

### 5.3 cancel-pending 的新归属

cancel-pending 不是要删除的概念。它仍然表示“旧 owner 正在取消，不能重复提交同一 operation”。

重构后的规则：

- 如果 active maker 过 grace，本轮 `BoundaryPolicy` 可以产出 aggressive operation；reconciliation 根据 desired binding 取消旧 maker owner。
- 如果旧 owner 已经是上一轮遗留的 `CancelPending`，policy/reconciliation 必须阻止重复提交。
- 调用层不再创建 `preexisting_cancel_pending_operations` 快照。

也就是说，保留运行状态，删除外层时序协议。

## 6. 需要保留的行为

重构前后必须保持：

- ManualOverride > ReduceOnly > Normal 的外层优先级不变。
- Normal 内 aggressive due 覆盖优先于 passive future maker。
- Catch-up gap 方向过滤不变。
- Curve maker 每侧最多保留 `CURVE_MAKER_LEVELS_PER_SIDE` 个 future operation。
- Passive maker 的 trigger price 与 reduce-only 语义不变。
- Due aggressive 的 execution price、聚合 quantity、min-notional 过滤不变。
- 已 cancel-pending 的 operation 不会重复 submit。
- 本轮刚进入 grace replacement 的 maker 可以被同轮 aggressive binding 替换。

## 7. Progress ledger 为什么暂缓删除

从数学上看，连续相邻 boundary 的 remaining 可以由 `current_exposure` 推导。但当前 Binance user data path 中：

- `ORDER_TRADE_UPDATE` 只产生 order observation。
- `ACCOUNT_UPDATE` 才产生 position update。

两者是独立 websocket 消息，到达顺序不保证。因此在 fill 已到但 position 未到的窗口内，progress ledger 仍然承担“已成交覆盖量”的会话内真值。如果直接删除 ledger，planner 可能用旧 `current_exposure` 重复覆盖。

删除 progress ledger 需要先完成独立 spike：让 fill 与 post-fill exposure 在 application 层形成同一个事实输入，或证明 adapter 层能提供等价保证。

## 8. 验收原则

本次重构通过以下方式验收：

- 先补行为测试，再改结构。
- 重构后删除 `CoverageReservation`，并把 existing owner 是否可覆盖或可替换的判断收进 reconciliation 私有规则，不再以双 policy 交互协议暴露。
- `plan()` 中不再有 `preexisting_cancel_pending_operations` 快照。
- 当前 Normal 模式行为测试保持通过。
- 新增测试覆盖：
  - terminal binding 清理。
  - anchor middle branch。
  - sell-side curve maker。
  - partial fill 后后续 binding 覆盖剩余量。
  - active binding drift budget 容差。
  - due maker grace 后同轮 aggressive replacement。
  - preexisting cancel-pending 阻止重复 submit。
