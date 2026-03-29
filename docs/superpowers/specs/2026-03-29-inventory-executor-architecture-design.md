# 库存执行器架构设计

**日期：** 2026-03-29

基于现有架构设计（见 [2026-03-24-grid-platform-architecture-design.md](2026-03-24-grid-platform-architecture-design.md)）和运行态边界收敛设计（见 [2026-03-27-grid-engine-runtime-internalization-design.md](2026-03-27-grid-engine-runtime-internalization-design.md)），把当前“库存目标直接翻成单笔挂单”的执行模型升级为独立库存执行器。

## 1. 背景

当前系统已经能稳定计算：

- 给定 `reference_price` 的 `target_exposure`
- 当前 `current_exposure`
- 基于风险和交易所规则的最小下单约束

但执行层仍然是单笔订单模型：

- `engine` 当前只维护一个 `pending_order`
- `reconciler` 直接把 `target_exposure - current_exposure` 翻成单笔 `SubmitOrder`
- 启动恢复、回执吸收、替单门槛都围绕单个 `pending_order` 展开

这会带来三个结构性问题：

1. 价格触发后如果挂单没有成交，系统没有“持续收敛库存偏差”的执行语义。
2. 执行层更像“单笔调仓器”，不是真正的库存控制执行器。
3. 恢复、重挂、观测吸收都缠在 `pending_order` 的局部补丁里，继续放大会显著增加维护成本。

本项目当前主语义已经明确为“库存管理”，不是传统网格。曲线负责定义目标库存，执行层负责把实际库存往目标库存拉回。网格式阶梯执行只是未来可能出现的一种执行方式，不是一期主模型。

## 2. 设计目标

### 2.1 主目标

- 明确拆分 `Inventory Policy` 与 `Inventory Executor`
- 让库存偏差具备持续执行语义，而不是单次触发语义
- 用工作订单集合替代单个 `pending_order`
- 恢复时先重建工作集，再重新规划执行，而不是依赖旧的单订单锚点补丁
- 保留现有曲线库存模型，不在这次改动 `core` 的策略形状
- 提升执行层可观测性，让运行时状态和量化指标都能解释“为什么这么执行”
- 提供稳定的量化评价口径，能在固定场景里比较迁移前后执行效果
- 支持在回放层和传统网格基线做 benchmark，而不是只看新执行器自身指标

### 2.2 非目标

- 这次不实现完整双边对称网格
- 这次不实现多 planner / 多执行器框架
- 这次不引入 `MARKET` 作为常规执行路径
- 这次不引入盘口依赖、波动率自适应、动态步长
- 这次不新增新的 HTTP / WebSocket 接口；如需观测字段，只在现有 detail / TUI 视图内扩展
- 这次不扩展新的策略族
- 这次不在生产运行时里同时跑“库存执行器 + 传统网格”双执行逻辑

## 3. 术语

### 3.1 `Inventory Policy`

上层库存策略。输入价格、曲线参数、风控状态，输出 `target_exposure`。它只回答“目标库存是多少”，不回答“应该挂什么单”。

### 3.2 `Inventory Executor`

下层执行器。输入 `target_exposure`、`current_exposure`、市场状态和执行状态，输出当前应维持的订单集合与执行动作。它只回答“怎样把实际库存往目标库存拉回去”。

### 3.3 `ExecutionMode`

执行器运行模式。一期固定为：

- `Passive`
- `Rebalance`
- `CatchUp`

它描述执行激进度，不描述未来可能存在的执行器类型。

### 3.4 `WorkingOrder`

执行器正在管理的一笔订单事实。它覆盖提交中、已挂出、部分成交、待撤等阶段，不等于交易所原始订单对象。

### 3.5 `DesiredOrders`

执行器本轮规划后希望市场上存在的订单集合。它是规划结果，不持久化，恢复后重新计算。

### 3.6 `ExecutionStats`

执行器累计统计信息。它是运行结果事实，用于量化评价迁移效果，和单轮 `DesiredOrders` 不同，需要持久化。

## 4. 候选方向与决策

### 4.1 方向 A：继续沿用单订单模型，只加强改单规则

优点：

- 改动最小

缺点：

- 核心抽象仍然是单笔订单
- 恢复与执行补丁会继续散在 `reconciler`、`manager`、worker
- 无法把“库存偏差持续收敛”变成系统一等语义

### 4.2 方向 B：独立库存执行器，使用分层执行模式

做法：

- `Inventory Policy` 继续输出 `target_exposure`
- 执行器维护 `working_orders`
- 执行器按 `Passive / Rebalance / CatchUp` 分层收敛库存偏差
- 每轮先生成 `DesiredOrders`，再对比当前工作集生成 effect

优点：

- 符合“库存管理”主语义
- 能把复杂度收回 `engine`
- 恢复、改挂、观测吸收都有稳定边界

缺点：

- 需要引入新的执行器运行态
- 要重写当前围绕 `pending_order` 的恢复模型

### 4.3 方向 C：直接做连续紧迫度执行器

优点：

- 理论上更细腻

缺点：

- 参数与行为解释都更难
- 一期验证成本太高

### 4.4 决策

选择方向 B。

原因：

- 它把“目标库存”和“订单执行”拆到不同层
- 它可以先做清晰的分层执行，再在以后考虑更细腻的内部评分
- 它最符合当前项目的探索重点：先把执行底座做对

## 5. 核心设计

### 5.1 `reconciler` 收窄为高层库存收敛

当前 [`engine/src/reconciler.rs`](../../../engine/src/reconciler.rs) 直接产出单笔订单 effect。改造后它只负责：

- 根据 `reference_price` 计算 `target_exposure`
- 应用风控裁剪
- 更新高层运行状态和相关领域事件

它不再直接决定：

- 单笔 `SubmitOrder`
- `CancelAll + SubmitOrder`
- 挂单替换门槛是否命中

### 5.2 引入 `executor_state`

[`engine/src/runtime.rs`](../../../engine/src/runtime.rs) 中的 `GridRuntime` 增加：

```rust
pub struct GridRuntime {
    ...
    pub target_exposure: Option<Exposure>,
    pub executor_state: ExecutorState,
}
```

`ExecutorState` 一期至少包含：

```rust
pub struct ExecutorState {
    pub mode: ExecutionMode,
    pub inventory_gap: Exposure,
    pub gap_started_at: Option<DateTime<Utc>>,
    pub last_reprice_at: Option<DateTime<Utc>>,
    pub working_orders: Vec<WorkingOrder>,
    pub last_execution_reason: Option<ExecutionReason>,
}
```

一期不把 `DesiredOrders` 持久化到 snapshot；它只存在于单轮规划过程中。

### 5.3 用 `WorkingOrder` 替代单个 `pending_order`

`WorkingOrder` 至少包含：

```rust
pub struct WorkingOrder {
    pub client_order_id: String,
    pub order_id: Option<String>,
    pub side: Side,
    pub price: f64,
    pub quantity: f64,
    pub role: OrderRole,
    pub status: OrderStatus,
    pub submitted_at: Option<DateTime<Utc>>,
    pub last_seen_at: Option<DateTime<Utc>>,
    pub slot: OrderSlot,
}
```

其中：

- `role` 用于表达这笔单在执行器里的职责
- `slot` 用于表达它在执行器工作集里的固定位置

一期不追求复杂铺单，但必须把“订单语义归属”从交易所字段中独立出来。

### 5.4 执行模式

一期模式固定为：

- `Passive`
  - 小偏差
  - 优先被动挂单
  - 允许偏差短时间存在
- `Rebalance`
  - 中等偏差，或被动挂单持续未收敛
  - 收敛优先级上升
  - 取消无关工作单，集中补库存
- `CatchUp`
  - 大偏差，或 `Rebalance` 持续超时
  - 允许更激进的限价执行
  - 目标是快速把库存拉回安全区

模式切换依据是：

- 库存偏差大小
- 偏差持续时间
- 工作订单是否过期或明显偏离应挂位置

模式切换不再简单等同于“价格是否再次触发”。

### 5.5 执行流程改成“先规划集合，再做 diff”

当前流程是：

- `inventory_gap -> 单笔 effect`

新流程改成：

1. `Inventory Policy` 输出 `target_exposure`
2. 执行器根据运行态计算 `inventory_gap`
3. 执行器决定 `ExecutionMode`
4. 执行器生成本轮 `DesiredOrders`
5. 执行器对比 `DesiredOrders` 与 `working_orders`
6. 生成定点 `cancel / submit` effect

这意味着：

- 系统不再默认使用 `CancelAll + SubmitOrder`
- 常规改挂只修改真正发生变化的订单
- `NoOp` 的判断依据不再只是“单个旧单是否还能凑合保留”

### 5.6 `desired_orders` 不持久化

这是本次设计的重要取舍。

持久化：

- `executor_state`
- `working_orders`
- 模式相关时间戳

不持久化：

- `DesiredOrders`

原因：

- `DesiredOrders` 是规划结果，不是执行事实
- 恢复后重新计算更稳
- 可以避免把未来执行逻辑固化到 snapshot 中

### 5.7 执行器必须同时输出“当前诊断”和“累计统计”

只知道当前 `working_orders` 还不够，执行器还需要可观测性模型来回答两类问题：

1. 当前为什么在这样执行
2. 迁移后相比旧模型到底改善了多少

因此一期增加两类可观测数据。

#### 当前诊断

当前诊断属于运行态视角，至少包括：

- `mode`
- `inventory_gap`
- `gap_age_ms`
- `working_order_count`
- `last_execution_reason`

它们用于解释“当前为什么是 `Passive / Rebalance / CatchUp`，为什么没有立即换单，为什么进入追赶模式”。

#### 累计统计

累计统计属于效果视角，至少包括：

- `requote_count`
- `catch_up_count`
- `working_order_submit_count`
- `working_order_cancel_count`
- `working_order_fill_count`
- `max_inventory_gap_abs`
- `max_gap_age_ms`

这些指标不追求理论最优，只要求口径稳定、跨重启可恢复，能在固定测试场景和实盘联调中量化比较行为变化。

#### 基准对比所需原始事实

为了支持和传统网格对照，运行时还必须稳定提供可回放的原始执行事实。第一版不要求把所有 benchmark 指标都持久化到主 snapshot，但至少要能从运行态 / 活动流 / effect 记录中稳定恢复这些事实：

- 库存偏差随时间的变化
- 工作订单提交、撤销、成交的计数与时间点
- 模式切换时间点
- 当前和累计已实现收益

这样 benchmark 层可以在不污染生产主逻辑的前提下，离线聚合出对比指标。

#### 投影边界

一期不新增新接口，只在现有 detail / TUI 上补充观测字段：

- `Execution` 区块展示当前诊断
- `Statistics` 区块展示累计统计

这样既不扩展接口数量，也能让执行器迁移具备可解释性和量化评价基础。

### 5.8 传统网格 benchmark 放在回放层，不放在主运行时

这次明确把 benchmark 设计为回放层能力，而不是生产运行时能力。

原因：

- 传统网格是对照基线，不是当前主执行逻辑
- 若把对照组塞进生产运行时，会把执行器边界和持久化边界一起做宽
- benchmark 需要的是“同一段输入、同一套撮合假设、两套执行逻辑的可重复比较”，这更适合测试 / 回放 harness

因此采用下面的分层：

- 主运行时负责提供原始执行事实和累计统计
- 回放 harness 负责喂入固定价格路径与固定成交规则
- benchmark 报告负责聚合并比较：
  - 当前库存执行器
  - 传统网格基线执行器

一期 benchmark 的推荐对比指标为：

- `mean_abs_inventory_gap`
- `max_inventory_gap_abs`
- `max_gap_age_ms`
- `submit_count`
- `cancel_count`
- `fill_count`
- `requote_count`
- `catch_up_count`
- `realized_pnl`
- `net_realized_pnl`

其中：

- 运行时负责产出原始事实
- benchmark 层负责从同一回放场景中聚合上述对比指标

这样能保证主运行时边界稳定，同时支持以后继续迭代对照组。

## 6. 恢复与持久化

### 6.1 恢复中心从单订单锚点改为工作集重建

当前恢复模型围绕单个 `pending_order` 的 `Submitting / receipt-backed` 锚点展开。改造后恢复中心变为：

- live position
- live open orders
- 已持久化的 `executor_state`

### 6.2 启动恢复顺序

启动时对每个 grid 的处理顺序固定为：

1. 吸收 live position，更新 `current_exposure`
2. 吸收 live open orders，重建 `working_orders`
3. 基于最新 `target_exposure`、`current_exposure`、`reference_price` 重新规划 `DesiredOrders`
4. 对比 `DesiredOrders` 与 `working_orders`
5. 生成需要的定点 `cancel / submit` effect

也就是说，恢复之后不是“尽量延续那一笔旧单”，而是“先恢复当前工作集，再重新规划当前应有工作集”。

### 6.3 `effect worker` 的边界收窄

[`server/src/effect_worker.rs`](../../../server/src/effect_worker.rs) 改造后只负责：

- 执行 `submit / cancel`
- 回写订单结果
- 更新 effect 状态

它不再负责：

- 执行策略判断
- 是否应该继续追价
- 是否应该触发整格重算

### 6.4 `CancelAll` 降级成异常工具

`CancelAll` 不再是常规替换路径。它只保留在：

- 启动异常恢复
- 工作集无法认领
- 人工修复
- 风控熔断

常规执行路径一律优先定点 `cancel / submit`。

## 7. 模块所有权

### 7.1 `core`

拥有：

- 曲线库存模型
- 风控模型
- 目标库存相关纯函数

不拥有：

- 执行模式
- 工作订单
- 替单与重报价逻辑

### 7.2 `engine`

拥有：

- `target_exposure` 计算
- `executor_state`
- `working_orders`
- 执行模式切换
- `DesiredOrders` 规划
- 工作集 diff 与 effect 生成

### 7.3 `server runtime`

拥有：

- 外部事实翻译
- 启动同步调度
- 调用写侧服务

不拥有：

- 执行器状态机规则
- 工作集合并规则

### 7.4 `effect worker`

拥有：

- 交易所 effect 执行
- 执行结果回写

不拥有：

- 执行策略决策
- 库存偏差判断

## 8. 一期边界

为了控制复杂度，一期显式限制为：

- 只有一个库存执行器实现
- 不做多 planner 抽象
- `working_orders` 总数上限先保持很小，优先验证工作集模型
- 常规路径先以“有助于库存收敛的订单”为主，不实现完整双边对称网格
- `CatchUp` 只允许更激进的限价行为，不引入 `MARKET`

## 9. 验收标准

1. 当 `target_exposure` 与 `current_exposure` 存在偏差时，系统会持续维护 `working_orders`，而不是只生成一次单笔挂单后停止收敛。
2. 小偏差时进入 `Passive`，偏差扩大或超时未收敛时可升级到 `Rebalance`，再扩大或再超时可升级到 `CatchUp`。
3. 部分成交、完全成交、撤单、拒单后，执行器会更新 `working_orders`，并基于新的库存偏差重新规划。
4. 启动恢复时，系统先吸收 live position 和 live open orders，重建 `working_orders`，再重新规划，不再依赖单个 `pending_order` 锚点补丁。
5. 常规改挂不使用 `CancelAll`，而是按工作集 diff 做定点 `cancel / submit`。
6. `effect worker` 不承担执行策略判断，只负责 effect 执行与回写。
7. 当 `DesiredOrders` 与 `working_orders` 等价时，系统返回 `NoOp`，避免无意义重挂。
8. 一期验收测试至少覆盖模式切换、部分成交、启动恢复、定点改挂和无变化 `NoOp` 这几条主路径。
9. detail / TUI 能直接看到当前执行模式、库存偏差、偏差持续时间、工作订单数量和最近一次执行原因。
10. detail / TUI 能看到稳定的累计统计，至少包含 `requote_count`、`catch_up_count`、提交/撤单/成交计数、`max_inventory_gap_abs` 和 `max_gap_age_ms`。
11. 存在一个固定回放场景，能在相同输入和相同成交规则下同时跑库存执行器与传统网格基线，并产出稳定 benchmark 报告。

## 10. 后续实现顺序

1. 先补验收测试，锁定执行器行为
2. 引入 `executor_state` 与 `working_orders`
3. 将执行规划从 `reconciler` 下移到执行器
4. 改造恢复链路和 effect worker
5. 清理旧的 `pending_order` 中心语义

这次设计的目标不是把执行做成最复杂，而是先把系统从“单笔调仓”推进到“库存执行器”。
