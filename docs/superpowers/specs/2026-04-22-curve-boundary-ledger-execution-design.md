# 执行器重新设计：单曲线 Boundary Ledger 架构

**日期：** 2026-04-22
**基线：** `main` @ `2cb4c60`

本文给出一份新的统一设计，用来替换这一天内分叉出来的几套执行器方案。目标不是继续在 `round + slot` 上打补丁，也不是把执行器直接做成一台固定形态的做市机，而是把系统收敛到少数几个稳定真值上，让后续执行策略在同一条曲线下并存。

相关文件：

- 当前运行态定义：[`../../../engine/src/runtime.rs`](../../../engine/src/runtime.rs)
- 当前执行器入口：[`../../../engine/src/executor/mod.rs`](../../../engine/src/executor/mod.rs)
- 当前恢复逻辑：[`../../../engine/src/executor/recovery.rs`](../../../engine/src/executor/recovery.rs)
- 当前订单回报吸收：[`../../../engine/src/executor/recording.rs`](../../../engine/src/executor/recording.rs)
- 曲线与带外策略：[`../../../core/src/strategy.rs`](../../../core/src/strategy.rs)
- 保护状态模型：[2026-04-20-track-protection-state-model-design.md](2026-04-20-track-protection-state-model-design.md)

## 1. 目标

这次重构只解决一个问题：

> 在单一曲线 `desired(price)` 已经确定的前提下，怎样让执行器既能支持多种执行策略，又保持正确性、可观测性和可恢复性。

这里的“多种策略”只指执行策略，不指多条上游曲线。项目基础保持不变：

- 上游仍然只有一条曲线
- 曲线仍然定义“价格到这里，应持有什么仓位”
- 执行层只负责把这条曲线落到交易所

## 2. 现有问题

当前 `round + slot` 模型的根问题不是代码多，而是主语义放错了。

它把系统中心放在：

- 当前这一轮要追哪个 `desired_exposure`
- 当前哪一个 slot 上有哪一张单

这会让很多本该属于“库存变化记账”的知识，退化成订单控制补丁：

- 什么时候换单
- 什么时候算 stale
- 什么时候从被动挂单切到追单
- crash 后怎么把“那一张单”接回来

一旦要支持提前挂单、多种执行方式、部分成交、保护态过滤，这些知识就会同时泄漏到 planner、recording、recovery 和读模型。

## 3. 第一性原理

执行系统里真正稳定的事实只有三个：

1. 曲线 `desired(price)`
2. 物理仓位 `current_exposure`
3. 交易所上的订单与成交回报

从这三个事实往下推，执行器真正需要管理的，不是 round，也不是 slot，而是：

> 哪些边界操作还没完成、已经完成了多少、当前由什么执行方式在覆盖它们。

这里的“边界操作”指：

- 曲线上两个相邻仓位层级之间的一条边界
- 以及沿这条边界向上或向下的一次操作

因此新的主骨架必须围绕 `boundary + progress + binding` 来设计，而不是围绕“订单位置”来设计。

## 4. 设计原则

### 4.1 真值尽量少

核心状态里只保留：

- 单一曲线
- 物理仓位
- 边界进度
- live order bindings

以下信息全部降级为派生或策略输出：

- `inventory_gap`
- `mode`
- `core/resting` 展示分组
- stale 年龄统计

### 4.2 事实和执行方式分开

系统必须显式区分四件事：

- `boundary`：一条相邻仓位层级之间的边界
- `progress`：这条边界自本轮账本锚点以来累计执行了多少
- `policy`：用什么方式去覆盖某个方向的边界操作
- `binding`：交易所上哪张单正在服务它

这四层不能再混成一层状态机。

### 4.3 订单不是主语义

订单只是市场上的实现，不是真值。

系统不再围绕：

- `slot`
- `round`
- “这张单就是这段语义”

来设计。

### 4.4 恢复与正常运行使用同一套语义

恢复不再拥有一套特供领域模型。

正常运行怎样理解 `boundary`、`progress` 和 `binding`，恢复就按同一套规则重建。

### 4.5 配置变更只允许一种规则

为了降低复杂度，第一阶段明确选择：

> 任意 `TrackConfig` / 曲线 profile revision 变化，都开启一套新的 boundary ledger。

也就是：

- 新 revision 到来时，`ledger_anchor_exposure = current_exposure`
- 旧 revision 的 boundary progress 不做 remap
- 不承诺跨 revision 复用已完成边界操作

这条规则故意保守，但只有这一条，避免热更、回放、恢复各自补兼容逻辑。

## 5. 四层架构

新的执行器固定为下面四层：

```text
curve
  -> levels / boundaries
  -> boundary progress ledger
  -> execution policy
  -> live order binding
```

### 5.1 Curve

`Curve` 不是新对象，本质上仍然来自：

- `TrackConfig`
- `strategy::desired_exposure(price, config)`

它继续回答一件事：

> 在价格 `p` 下，理想仓位是多少。

这层不直接产出订单。

### 5.2 Levels / Boundaries

这是曲线离散化后的静态层。

它回答：

- 这条曲线在 exposure 轴上被切成哪些稳定层级
- 相邻层级之间有哪些边界
- 每条边界的触发价格和步长是多少

### 5.3 Boundary Progress Ledger

这是新的执行内核。

它回答：

- 每条边界自账本锚点以来，向上执行了多少、向下执行了多少
- 在当前价格下，这条边界应当处于上侧还是下侧
- 该边界两个方向各还剩多少可执行量
- 当前哪些边界操作已经被某个执行策略覆盖

### 5.4 Execution Policy

执行策略不拥有目标，只拥有“覆盖某个方向边界操作的方法”。

第一阶段支持的 policy 类型：

- `CurveMakerPolicy`
- `CatchUpPolicy`
- `ManualOverridePolicy`
- `FlattenPolicy`

后续可以继续增加，但都必须消费同一个 boundary progress ledger。

### 5.5 Live Order Binding

这层才和交易所订单对接。

它回答：

- 当前有哪些 live order
- 每张单正在覆盖哪些边界操作
- 当前订单生命周期处于什么状态
- 回报到来时，如何回写边界进度

## 6. Level 与 Boundary 模型

### 6.1 Level

最小离散单位按仓位增量切，而不是按价格格子切。

先把 exposure 轴按 `level_step` 切成稳定层级：

- 第一阶段 `level_step = config.min_rebalance_units`
- 若末端有不足一个单位的残余量，允许形成更小的最后一段

例如：

- `...`
- `-1`
- `0`
- `+1`
- `+2`
- `...`

Level 只是仓位刻度，不直接对应订单。

### 6.2 Boundary

每对相邻 level 之间形成一条无向边界。

例如：

- `boundary(0, +1)`
- `boundary(+1, +2)`
- `boundary(-2, -1)`

每条边界都支持两个方向的操作：

- `Up`：从较低 exposure 走向较高 exposure
- `Down`：从较高 exposure 走向较低 exposure

这两个方向不是两条独立真值，只是同一条边界上的两种操作视角。

### 6.3 Boundary 的生成

给定：

- `level_step = config.min_rebalance_units`
- 曲线支持的 exposure 上下界

离散化算法是：

1. 从曲线支持的最小 exposure 到最大 exposure，切出一组稳定 level
2. 每对相邻 level 之间形成一条 boundary
3. 每条 boundary 记录：
   - `lower_exposure`
   - `upper_exposure`
   - `trigger_price`
   - `step_size`
4. 若最末端不足一个 `level_step`，允许形成最后一条更小的残余边界

例如：

- 曲线范围 `[-8, +8]`
- `level_step = 1`

则生成：

- `boundary(-8, -7)`
- `...`
- `boundary(-1, 0)`
- `boundary(0, +1)`
- `boundary(+1, +2)`
- `...`
- `boundary(+7, +8)`

这条规则自然支持仓位回撤：

- 价格下行建出多头仓位时，执行 `boundary(0, +1).Up`、`boundary(+1, +2).Up`
- 价格回升减少多头仓位时，执行 `boundary(+5, +6).Down`、`boundary(+4, +5).Down`
- 做空后的回补，也同理使用负侧边界的 `Up`

### 6.4 触发价格的反解

每条 boundary 只有一个 `trigger_price`，它表示：

> 曲线刚好跨过这条边界时对应的价格。

纯函数形状为：

```rust
pub fn trigger_price_for_boundary(
    boundary_upper: Exposure,
    config: &TrackConfig,
) -> f64;
```

定义：

- 对 `boundary(lower, upper)`，`trigger_price` 就是使 `desired_exposure(price, config) == upper` 的那一个价格
- `Up` 和 `Down` 都共用这一个边界价格，只是代表向边界上侧或下侧移动

当前三种 `ShapeFamily` 在带内都保持单调，因此第一阶段用单调反解即可：

- 在 `[lower_price, upper_price]` 上对 `desired_exposure(price, config)` 做二分搜索
- 搜索目标是 `desired_exposure(price, config) - upper = 0`
- 精度满足一个价格 tick 的一半即可

`BoundaryBlueprint.trigger_price` 保留反解得到的连续值；真正下单时再按 `exchange_rules` 做价格离散化。

### 6.5 `BoundaryId` 与 `BoundaryBlueprint`

第一阶段定义：

```rust
pub struct BoundaryId {
    pub profile_revision: ProfileRevision,
    pub lower_exposure_bp: i64,
    pub upper_exposure_bp: i64,
}

pub struct BoundaryBlueprint {
    pub id: BoundaryId,
    pub lower_exposure: Exposure,
    pub upper_exposure: Exposure,
    pub trigger_price: f64,
    pub step_size: f64,
}

pub enum BoundaryDirection {
    Up,
    Down,
}
```

其中：

- `profile_revision` 来自 `TrackConfig` 的稳定版本指纹
- `_bp` 表示 exposure 经稳定精度缩放后的整数
- 第一阶段不做跨 revision remap，所以 `profile_revision` 必须进入 `BoundaryId`

`BoundaryBlueprint` 是纯派生结构：

- 它来自曲线与离散规则
- 不持久化到 snapshot
- 它不直接携带订单侧或订单角色

订单侧、订单角色和具体数量方向，都是在 `(boundary, direction)` 的查询视角上派生的。

## 7. Boundary Progress Ledger

### 7.1 核心职责

ledger 维护的不是“这一档有没有一张单”，而是：

- 每条边界自账本锚点以来向上累计执行了多少
- 每条边界自账本锚点以来向下累计执行了多少
- 当前这条边界的有效穿越量是多少
- 在当前价格下，哪个方向是 `due`，哪个方向是 `future`
- 当前哪些边界操作已被某个 binding 覆盖

### 7.2 `BoundaryProgress`

第一阶段把每条边界的最小持久化事实定义为：

```rust
pub struct BoundaryProgress {
    pub cumulative_up_qty: f64,
    pub cumulative_down_qty: f64,
}
```

这两个字段都表示：

- 自本轮 `ledger_anchor_exposure` 建立以来
- 沿该边界某个方向累计吸收了多少成交

### 7.3 有效穿越量与剩余量

账本锚点不是目录生成起点，而是“本轮 ledger 从哪一个物理仓位开始记账”的起点。

因此，对任意 `boundary(lower, upper)`，都先从 `ledger_anchor_exposure` 派生一个锚点侧状态：

```text
anchor_crossed_qty =
  if ledger_anchor_exposure >= upper - exposure_epsilon
  then step_size
  else 0
```

再计算当前有效穿越量：

```text
effective_crossed_qty =
  anchor_crossed_qty
  + cumulative_up_qty
  - cumulative_down_qty
```

在正常路径下，必须保持：

```text
0 <= effective_crossed_qty <= step_size
```

然后再派生两个方向当前还剩多少可执行量：

```text
up_remaining = step_size - effective_crossed_qty
down_remaining = effective_crossed_qty
```

这意味着：

- 若锚点时已经在边界上侧，则初始就天然拥有 `down_remaining = step_size`
- 若锚点时在边界下侧，则初始天然拥有 `up_remaining = step_size`
- 价格往返穿越同一条边界时，不需要再拆成两个方向状态后额外做配对合并

### 7.4 `due` 的判定

`due / future` 是纯派生语义，不持久化。

第一阶段定义：

```text
exposure_epsilon = max(level_step * 0.01, quantity_step_as_exposure)
```

其中 `quantity_step_as_exposure` 表示交易所最小数量步长映射到 exposure 后的精度。

对当前价格 `strategy_price`，先计算：

```text
spot_target = desired_exposure(strategy_price, config)
```

对任意 `boundary(lower, upper)`，先判断当前目标应在边界哪一侧：

```text
target_is_upper = spot_target >= upper - exposure_epsilon
```

再派生方向状态：

- 若 `target_is_upper`：
  - `Up` 为 `due`，当且仅当 `up_remaining > 0`
  - `Down` 为 `future`
- 若 `target_is_upper = false`：
  - `Down` 为 `due`，当且仅当 `down_remaining > 0`
  - `Up` 为 `future`

等价地，也可以理解成：

- `Up`：当前价格已经穿过这条 boundary 的触发价格并应停留在上侧
- `Down`：当前价格已经回穿这条 boundary 的触发价格并应停留在下侧

### 7.5 持久化状态

```rust
pub struct BoundaryLedgerState {
    pub profile_revision: ProfileRevision,
    pub ledger_anchor_exposure: Exposure,
    pub progress: Vec<BoundaryProgressEntry>,
}

pub struct BoundaryProgressEntry {
    pub boundary_id: BoundaryId,
    pub progress: BoundaryProgress,
}
```

这里只持久化最小非派生事实：

- `profile_revision`
- `ledger_anchor_exposure`
- 每条边界自锚点以来的双向累计成交

以下信息不持久化，由当前价格、当前 bindings 和 blueprint 派生：

- 每条边界两个方向的 `due / future`
- 当前是否已被覆盖
- 当前剩余量

### 7.6 `expected_exposure`

为了做账实校验，ledger 可以派生：

```text
expected_exposure =
  ledger_anchor_exposure
  + Σ(cumulative_up_qty - cumulative_down_qty)
```

但这里有一个硬约束：

> `expected_exposure` 只用于一致性校验，不作为第二套“调整后仓位真值”参与 planner 决策。

也就是说：

- 物理真值始终只有 `current_exposure`
- planner 不从 `expected_exposure` 反推出新的目标
- 当 `current_exposure` 与 `expected_exposure` 明显偏离时，进入 anomaly / recovery 路径

## 8. Binding 模型

### 8.1 为什么需要独立 binding

这次重构最重要的边界之一是：

> 边界进度和订单生命周期必须拆开。

否则：

- `PartiallyFilled`
- `Canceling`
- maker 单未完成后由 aggressive 单接管
- 几条相邻边界操作聚合成一张追单

这些现实路径都会把知识重新打散到多个模块。

### 8.2 `BoundaryOperation`

policy 和 binding 选择的对象不是“整条边界”，而是：

> 某条边界上的某个方向操作。

第一阶段定义：

```rust
pub struct BoundaryOperation {
    pub boundary_id: BoundaryId,
    pub direction: BoundaryDirection,
}
```

它只是查询视角，不是新的持久化真值。

### 8.3 `LiveOrderBinding`

```rust
pub struct LiveOrderBinding {
    pub binding_id: String,
    pub proposal_key: BindingProposalKey,
    pub allocations: Vec<BindingOperationAllocation>,
    pub request: OrderRequest,
    pub desired_exposure: Exposure,
    pub submit_purpose: SubmitPurpose,
    pub order_id: Option<String>,
    pub status: BindingStatus,
    pub policy_state: BindingPolicyState,
}

pub enum BindingStatus {
    SubmitPending,
    Working,
    CancelPending,
    Terminal,
}

pub enum BindingPolicyState {
    Stateless,
    CurveMaker {
        due_grace_started_at: Option<DateTime<Utc>>,
    },
}
```

这里：

- `proposal_key` 是 diff 用的稳定匹配键，等于 `(policy, ordered_operation_keys)`
- `allocations` 表示这张单正在覆盖哪一组边界操作，以及每个操作分到的 exposure 数量
- `order_id` 是交易所 live order id；提交前可以为空
- `policy_state` 只保存 owner policy 私有的运行态，不进入通用 binding 语义
- 边界进度由 fills 吸收后再统一回写

第一阶段只有 `CurveMakerPolicy` 使用私有状态：

- 当 maker binding 覆盖的操作进入 `due`，且 `due_grace_started_at` 为空时，置为 `now`
- 只要该 maker binding 覆盖的操作回到 `future`，则清空 `due_grace_started_at`
- binding 被 replace、终止或被别的 policy 抢占时，新的 binding 重新从 `None` 开始

### 8.4 progress 回写规则

progress 回写采用两阶段：

1. 先把交易所回报吸收到 binding 上，更新 binding 的订单生命周期状态
2. 再把已确认完成的 `allocations` 按顺序回写到 boundary progress

当一个 binding 覆盖多条边界操作时，部分成交按 **操作顺序贪心分配**，不按比例分配。

第一阶段的代码只在交易所回报明确进入 `Filled` 终态时回写对应 allocations；若后续 adapter 提供累计成交量，仍按这里的顺序贪心规则把增量吸收到同一套 boundary progress。

第一阶段固定规则：

- 对 `CurveMakerPolicy`，顺序就是该 binding 内操作的自然顺序
- 对 `CatchUpPolicy`，顺序是“更靠近当前价格、已更早 due 的操作在前”

例如：

- binding 覆盖 `boundary(0, +1).Up` 与 `boundary(+1, +2).Up`
- 总量 `2.0`
- 本次累计成交 `1.3`

则回写为：

- `boundary(0, +1).Up` 完成 `1.0`
- `boundary(+1, +2).Up` 完成 `0.3`

### 8.5 binding 终止后的进度

binding 进入 `CancelPending` 或 `Terminal`，不会回滚已经写入 boundary progress 的已成交部分。

也就是说：

- binding 生命周期结束，只表示“这张单不再覆盖这些边界操作”
- 已写入的边界进度继续保留
- 未完成的剩余量会在下一轮被新的 binding 继续覆盖

### 8.6 BoundaryOperation 与 Binding 的关系

第一阶段选择一个保守但清楚的约束：

> 任一 `BoundaryOperation` 在任一时刻最多只能被一个 active binding 覆盖。

但一个 binding 可以覆盖：

- 一条边界操作
- 或若干条相邻、同方向边界操作

这样：

- `CurveMakerPolicy` 可以选择一条边界操作一张单
- `CatchUpPolicy` 可以把多条 overdue 操作聚合成一张 aggressive 单

而不需要改变边界本身的语义。

## 9. Execution Policy

### 9.1 Policy 的职责

policy 不直接改账本，只做两件事：

1. 从 ledger view 中挑选自己可以覆盖的边界操作
2. 生成一组期望的 `BindingProposal`

也就是说，policy 输出的是：

- 想覆盖哪些边界操作
- 用什么价格、数量和挂单方式覆盖

### 9.2 第一阶段 policy 集合

#### `CurveMakerPolicy`

职责：

- 在当前价格附近提前摆出未来边界操作
- 默认按每侧最近 `N` 条未覆盖 future 操作工作
- 第一阶段 `N = 3`

这里保留前面第三版方案的核心价值：

- 有一个声明式的目标挂单集合
- 执行器按 diff 保持交易所状态趋近这个集合

但这个集合只属于 maker policy，不再是系统真值。

#### `CatchUpPolicy`

职责：

- 覆盖已 `due` 但未完成的边界操作
- 对 stale 的 maker binding 做升级
- 允许把多条相邻 `due` 操作聚合成一张 aggressive 单

第一阶段固定一个内部参数：

```text
curve_maker_grace_ms = 60_000
```

接管规则：

- 操作已 `due`
- 操作仍有 `remaining`
- 当前只被 `CurveMakerPolicy` 的 maker binding 覆盖
- 且该 binding 的 `BindingPolicyState::CurveMaker { due_grace_started_at }` 已持续超过 `curve_maker_grace_ms`

满足这四条时，`CatchUpPolicy` 才会抢占 `CurveMakerPolicy`。

#### `ManualOverridePolicy`

职责：

- 在 `ManualState::TargetOverride` / `ManualState::Flattened` 下接管执行
- 暂停其他 policy

#### `FlattenPolicy`

职责：

- 在 `Frozen / FlattenPending / Flattening` 等保护态下，只覆盖风险减少方向的边界操作

### 9.3 Policy 优先级

为了避免同一边界操作被多个 policy 同时抢占，第一阶段固定优先级：

```text
ManualOverride > Flatten > CatchUp > CurveMaker
```

高优 policy 可以抢占低优 policy 对同一边界操作的覆盖权。

### 9.4 Policy 仲裁机制

policy 不是并行各算各的，再做合并；第一阶段采用**按优先级串行仲裁**：

1. 先构建一份 `LedgerView`
2. 再构建一份临时的 `CoverageReservation`
3. 按优先级依次运行 policy
4. 每个 policy 只能选择当前尚未被更高优 policy 预留的边界操作
5. policy 产出 proposal 后，立即写入 `CoverageReservation`

特殊规则：

- `ManualOverridePolicy` 一旦激活，直接独占本轮执行，后续 policy 全部跳过
- `FlattenPolicy` 和 `CatchUpPolicy` 可以抢占已有的 `CurveMakerPolicy` 覆盖
- `CurveMakerPolicy` 只能选择仍未被覆盖的 future 操作，不会反向抢占高优 policy

第一阶段明确支持这种抢占路径：

> 若某个边界操作当前由 `CurveMakerPolicy` 的 maker binding 覆盖，但本轮已变成 overdue 并被 `CatchUpPolicy` 选中，则 diff 阶段应撤掉原 maker binding，并换成新的 catch-up binding。

这里的 `overdue` 第一阶段正式定义为：

- 操作已 `due`
- 操作仍有 `remaining`
- 且满足 catch-up 的接管规则

## 10. Reconcile 流程

统一执行入口形状：

```rust
pub fn plan(input: PlanInput<'_>) -> PlanOutput;
```

内部顺序固定为：

1. **构建 profile revision**
   从当前 `TrackConfig` 得到 `ProfileRevision`

2. **处理 revision 切换**
   若 revision 与持久化 ledger 不一致：
   - 清空旧 `BoundaryLedgerState.progress`
   - `ledger_anchor_exposure = current_exposure`
   - 清空旧 bindings

3. **派生 level / boundary catalog**
   由曲线生成 `Vec<BoundaryBlueprint>`

4. **吸收真实回报**
   用 live order updates / fills 更新 bindings，并把已完成量回写到 boundary progress

5. **做账实校验**
   派生 `expected_exposure`
   若与 `current_exposure` 偏差超限，进入 `RecoveryAnomaly::ExpectedExposureMismatch`

6. **构建 ledger view**
   计算每条 boundary 的：
   - `effective_crossed_qty`
   - `up_remaining`
   - `down_remaining`
   - 哪个方向 `due`
   - 哪个方向已被覆盖

7. **运行 policy**
   按优先级生成 `BindingProposal` 集合

8. **binding diff**
   对比：
   - 当前 active bindings
   - 本轮期望 bindings

   产出：
   - `SubmitOrder`
   - `CancelOrder`
   - 必要时的 replace

9. **派生读模型**
   最后才派生：
   - `inventory_gap`
   - `mode`
   - core/resting 展示分组

### 10.1 Binding Proposal 与 diff 规则

`binding diff` 的输入不是裸订单，而是 policy 产出的期望 binding 集合：

```rust
pub struct BindingProposal {
    pub policy: PolicyKind,
    pub operations: BoundaryOperationSelection,
    pub request: OrderRequest,
}
```

第一阶段的匹配键固定为：

```text
proposal_key = (policy, ordered_operation_keys)
```

diff 规则：

1. `proposal_key` 匹配且请求参数在容差内一致 → `NoOp`
2. `proposal_key` 匹配但请求参数已偏离 policy 的替换规则 → `CancelAndReplace`
3. active binding 的操作与更高优 proposal 冲突 → `CancelOrder`
4. proposal 无匹配 active binding → `SubmitOrder`
5. active binding 无匹配 proposal → `CancelOrder`

### 10.2 Policy 自己决定报价稳定性

不同 policy 的报价稳定性不同，替换规则由 policy 自己定义：

- `CurveMakerPolicy`
  - 价格锚定 boundary 的 `trigger_price`
  - 先按 `exchange_rules` 做确定性离散化
  - 曲线与 revision 不变时，quote 漂移本身不会触发换单
- `CatchUpPolicy`
  - 价格锚定当前 `ExecutionQuote`
  - Buy 用 `best_ask`，Sell 用 `best_bid`
  - 聚合 quantity = 所选操作的剩余量之和
  - 报价变动是否触发 replace，按 catch-up policy 的 aggressiveness 规则决定

也就是说：

- maker 单天然更稳定
- catch-up 单天然更跟随当前盘口

两者都走同一个 diff 框架，但不共享一套“全局换价逻辑”。

## 11. Recovery

恢复沿用和正常路径同一套语义，不额外再造领域模型。

### 11.1 快照恢复

如果 snapshot 中的：

- `profile_revision`
- `BoundaryLedgerState`
- `bindings`

都与当前 config revision 一致，则直接还原。

### 11.2 revision 不一致

如果 config revision 不一致：

- 不做 remap
- 直接开启新 ledger
- `ledger_anchor_exposure = current_exposure`
- `progress = {}`
- `bindings = {}`

这是第一阶段刻意选择的单一规则。

### 11.3 live order 认领

恢复时优先按 binding 快照认领 live order：

- `client_order_id` 命中已知 binding
- 或 `order_id` 命中已知 binding

若无 id backing，可做受限结构匹配：

- `side`
- `price ± tick tolerance`
- `qty ± step tolerance`
- 且只能在当前 active binding 候选上唯一匹配

否则进入：

- `UnknownLiveOrder`
- `AmbiguousLiveOrder`

恢复不会凭 live order 虚构边界进度。

## 12. 与旧概念的关系

| 旧概念 | 新架构中的位置 |
|---|---|
| `desired_exposure` 标量 | 仍保留，但只是曲线在当前价格下的读数 |
| `ExecutionRound` | 删除，不再作为核心状态 |
| `ExecutionMode` | 删除，不再持久化；若仍需展示则由 policy 派生 |
| `ExecutionSlot` | 删除，订单不再是主语义容器 |
| `core/resting` | 仅作为 policy 或读模型投影，不是内核状态 |
| `TargetOrderBook` | 降级为 `CurveMakerPolicy` 的策略投影 |
| `claim/progress ledger` | 被 `boundary progress ledger` 取代 |

## 13. 第一阶段范围

为了避免再次做成补丁工程，第一阶段只做这几个明确边界：

1. 单一曲线，不支持多曲线合成
2. boundary 按 `min_rebalance_units` 离散
3. 任一 `BoundaryOperation` 最多一个 active binding
4. 配置 revision 变化时整本账重开，不做 remap
5. `CurveMakerPolicy` 与 `CatchUpPolicy` 同时存在
6. `TargetOrderBook` 只作为 maker policy 的内部投影
7. 外部接口继续保持：
   - `ExecutionAction`
   - `ports::OrderRequest` / `OrderReceipt` / `OrderStatus`

## 14. 明确不做

- 不保留 `round + slot` 作为过渡骨架
- 不让 `TargetOrderBook` 直接成为系统真值
- 不把 boundary progress 和订单生命周期压成一层状态机
- 不支持跨 config revision 的 boundary remap
- 不在第一阶段暴露 boundary/binding 内部枚举到 protocol
- 不为不同 policy 引入独立预算 owner

## 15. 验收重点

最终实现至少要能证明：

1. `current_exposure` 始终是唯一物理仓位真值
2. boundary progress 与 binding 生命周期是两个独立维度
3. `CurveMakerPolicy` 可以提前挂出未来边界操作对应的单
4. `CatchUpPolicy` 可以接管 overdue / stale 操作，且允许聚合多条操作
5. 同一 `BoundaryOperation` 不会被多个 active binding 同时覆盖
6. recovery 不会凭 live order 虚构 boundary progress
7. config revision 变化时只有一种行为：重开新账本
8. 读模型里的 `core/resting/mode/gap` 都是派生信息，不重新进入核心状态

## 16. 结论

这份设计把执行器收敛到一条清晰主线：

- 曲线是真值
- level / boundary 是静态离散层
- boundary progress ledger 是执行内核
- policy 是执行手段
- order binding 是交易所实现

这样既保留了“提前埋单、声明式 diff、曲线驱动做市”的能力，也避免把系统锁死成固定形态的单一做市机，更适合后续在同一条曲线下继续增加执行策略而不反复改核心语义。
