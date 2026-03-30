# 库存执行器架构设计

**日期：** 2026-03-29

基于现有架构设计（见 [2026-03-24-grid-platform-architecture-design.md](2026-03-24-grid-platform-architecture-design.md)）和运行态边界收敛设计（见 [2026-03-27-grid-engine-runtime-internalization-design.md](2026-03-27-grid-engine-runtime-internalization-design.md)），把当前“库存目标直接翻成单笔挂单”的执行模型升级为独立库存执行器。

> **2026-03-30 落地说明**
>
> 本轮边界收紧已经完成，当前实现与本文对齐：
>
> - `slot` 生命周期统一由 `engine executor` transition 吸收
> - `submit recovery` 已并回执行器，server 侧只传递事实和回写结果
> - `write_service` 已从全局串行锁收紧为按 `grid` 串行
>
> 本次落地不改变主架构方向：
>
> - 保留 `slot`
> - 保留 `DesiredOrders` 不持久化
> - 不在本次改动引入 `actor`

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
- 用槽位工作集替代单个 `pending_order`
- 恢复时先重建槽位工作集，再重新规划执行，而不是依赖旧的单订单锚点补丁
- 保留现有曲线库存模型，不在这次改动 `core` 的策略形状
- 提升执行层可观测性，让运行时状态和稳定累计统计都能解释“为什么这么执行”
- 提供稳定的内部观测口径，支持本阶段验收和联调

### 2.2 非目标

- 这次不实现完整双边对称网格
- 这次不实现多 planner / 多执行器框架
- 这次不引入 `MARKET` 作为常规执行路径
- 这次不引入盘口依赖、波动率自适应、动态步长
- 这次不新增额外 endpoint，但允许重画现有 list / detail 读模型结构
- 这次不扩展新的策略族
- 这次不建立 replay benchmark；如需和传统网格做对照，放到下一阶段
- 这次不在生产运行时里同时跑“库存执行器 + 传统网格”双执行逻辑
- 这次不引入 per-grid actor；如需引入，只放到下一阶段的 server 侧时序收敛里讨论

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

执行器累计统计信息。它是运行结果事实，用于观测执行器是否稳定收敛，和单轮 `DesiredOrders` 不同，需要持久化。

```rust
pub struct ExecutionStats {
    pub started_at: DateTime<Utc>,
    pub max_inventory_gap_abs: Exposure,
    pub max_gap_age_ms: i64,
}
```

一期统计窗口固定为“当前 grid activation”：

- grid 创建或重新启用时重置
- 进程重启不重置
- `ExecutionStats` 至少要记录 `started_at`

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
    pub stats: ExecutionStats,
    pub slots: Vec<ExecutionSlot>,
    pub last_execution_reason: Option<ExecutionReason>,
}
```

一期不把 `DesiredOrders` 持久化到 snapshot；它只存在于单轮规划过程中。

### 5.3 用 `ExecutionSlot` 明确每个槽位的生命周期

当前设计里真正需要被持久化和恢复的，不是“无名订单列表”，而是“执行器有哪些固定槽位、每个槽位现在处于什么状态”。

因此 `ExecutorState` 里的核心对象不是松散的 `working_orders` 列表，而是具名槽位：

```rust
pub struct ExecutionSlot {
    pub slot: OrderSlot,
    pub state: SlotState,
    pub working_order: Option<WorkingOrder>,
}

pub enum SlotState {
    Empty,
    SubmitPending,
    Working,
}
```

其中：

- `slot` 是稳定身份
- `state` 是槽位生命周期
- `working_order` 是该槽位当前绑定的订单事实

执行器当前“工作集”定义为：所有非 `Empty` 槽位里的 `working_order` 集合。

### 5.4 槽位不变量

一期必须明确以下不变量：

1. 每个 `slot` 在任一时刻最多绑定一笔 `working_order`
2. `slot` 的生命周期只能由执行器推进，外部观测只提供事实，不直接改写槽位语义
3. `DesiredOrders` 必须先映射到具体 `slot`，再生成 `cancel / submit`
4. 恢复时先重建 `slot -> working_order` 关系，再做新一轮规划

这些不变量的目标是把恢复、撤单、重挂的复杂度压回执行器内部，避免 `manager`、`write_service`、`effect_worker` 各自推断合法状态。

因此一期还要再加一条实现约束：

- 提交请求、提交回执、live order 认领、终态清理这些事实都只能通过执行器 transition 吸收
- `manager`、`write_service`、`effect_worker` 只允许把事实交给执行器，不允许直接 `upsert / clear slot`

### 5.5 用 `WorkingOrder` 替代单个 `pending_order`

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

### 5.6 执行模式

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

### 5.7 执行流程改成“先规划集合，再做 diff”

当前流程是：

- `inventory_gap -> 单笔 effect`

新流程改成：

1. `Inventory Policy` 输出 `target_exposure`
2. 执行器根据运行态计算 `inventory_gap`
3. 执行器决定 `ExecutionMode`
4. 执行器生成本轮 `DesiredOrders`
5. 执行器先把 `DesiredOrders` 映射到具体 `slot`
6. 执行器对比 `DesiredOrders` 与当前槽位工作集
7. 生成定点 `cancel / submit` effect

这意味着：

- 系统不再默认使用 `CancelAll + SubmitOrder`
- 常规改挂只修改真正发生变化的订单
- `NoOp` 的判断依据不再只是“单个旧单是否还能凑合保留”

### 5.8 `desired_orders` 不持久化

这是本次设计的重要取舍。

持久化：

- `executor_state`
- 模式相关时间戳

不持久化：

- `DesiredOrders`

原因：

- `DesiredOrders` 是规划结果，不是执行事实
- 恢复后重新计算更稳
- 可以避免把未来执行逻辑固化到 snapshot 中

### 5.9 执行器必须同时输出“当前诊断”和“累计统计”

只知道当前槽位工作集还不够，执行器还需要可观测性模型来回答两类问题：

1. 当前为什么在这样执行
2. 当前执行是否在稳定收敛

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

- `max_inventory_gap_abs`
- `max_gap_age_ms`

这些指标不追求理论最优，只要求口径稳定、跨重启可恢复，能在本阶段验收测试和实盘联调中量化观察行为变化。

这里的“跨重启可恢复”含义固定为：

- 统计窗口延续同一个 grid activation
- 不是无限期累计历史最大值
- grid 重新启用后重新开始统计

#### 对外投影收窄为稳定摘要

当前诊断和累计统计里有两类信息：

1. 执行器内部语义
2. 对产品和运维稳定有意义的摘要

一期明确只把第二类放进主协议和 TUI 主视图。

主协议 / TUI 主视图允许投影：

- `execution_status`
- `inventory_gap`
- `gap_age_ms`
- `active_slot_count`
- `max_inventory_gap_abs`
- `max_gap_age_ms`
- `stats_started_at`

一期不把下面这些执行器内部概念固化进主协议：

- `mode`
- `last_execution_reason`

原因：

- 它对当前执行器实现很有用
- 但不是稳定的产品语义
- 未来只要执行模式命名或内部状态机调整，就会放大协议变更范围

因此这些字段保留在：

- 内部 snapshot / debug 能力

#### 投影边界

一期允许重画现有 list / detail 读模型，但要求它们直接围绕槽位工作集表达：

- list 视图展示 `execution_status` 和 `active_slot_count`，不再展示 `pending_order_count`
- detail 视图的 `Execution` 区块展示稳定执行摘要
- `execution_status` 使用稳定对外语义：
  - `normal`
  - `attention_required`
- detail 视图的 `Execution` 区块展示 `slots`
- `slots` 对外投影为稳定的 `ExecutionSlotView`
- `active_slot_count` 必须恒等于 `execution.slots.len()`
- `ExecutionSlotView` 至少包含：
  - `label`
  - `phase`
  - `intent`
  - `order`
- `order` 为可选对象，至少包含：
  - `side`
  - `price`
  - `quantity`
- `phase` 使用稳定视图语义：
  - `opening`
  - `working`
- `intent` 使用稳定业务语义：
  - `increase_inventory`
  - `decrease_inventory`
- `ExecutionSlotView` 不直接暴露内部 `SlotState`、`OrderRole` 或交易所订单状态枚举，也不要求与内部枚举一一对应
- `Statistics` 区块展示稳定累计统计
- 读模型不再保留 `execution.pending_order`

这样既不扩展接口数量，也能让执行器迁移具备可解释性和稳定观测基础。

### 5.10 下一阶段候选项

和传统网格的 replay benchmark 延后到下一阶段。

本阶段只要求主运行时支持：

- 当前诊断
- 稳定累计统计

不为离线对照回放额外引入：

- benchmark harness
- 传统网格基线执行器
- benchmark 专用验收项

per-grid actor 也延后到下一阶段。

它的定位固定为：

- 收敛 `server` 侧每个 `grid` 的时序与状态所有权
- 消除应用层跨 `grid` 的无谓串行化

它不负责：

- 取代 `slot`
- 改写执行器内部的 `DesiredOrders -> slot -> diff` 主模型

## 6. 恢复与持久化

### 6.1 恢复中心从单订单锚点改为槽位工作集重建

当前恢复模型围绕单个 `pending_order` 的 `Submitting / receipt-backed` 锚点展开。改造后恢复中心变为：

- live position
- live open orders
- 已持久化的 `executor_state`
- 已持久化且尚未终结的 `submit effect`，作为 `pending_submit_hints`

### 6.2 启动恢复顺序

启动时对每个 grid 的处理顺序固定为：

1. 吸收 live position，更新 `current_exposure`
2. 读取当前仍处于 pending 的 `submit effect`，提炼成 `pending_submit_hints`
3. 吸收 live open orders，并由执行器负责重建 `slot -> working_order` 关系
4. 基于最新 `target_exposure`、`current_exposure`、`reference_price` 重新规划 `DesiredOrders`
5. 对比 `DesiredOrders` 与当前槽位工作集，并对仍在 pending 的提交 effect 做去重
6. 生成需要的定点 `cancel / submit` effect

也就是说，恢复之后不是“尽量延续那一笔旧单”，而是“先恢复当前槽位工作集，再重新规划当前应有工作集”。

### 6.3 `submit recovery` 也属于执行器恢复语义

`submit recovery` 不是 `effect_worker`、`write_service`、`manager` 之间额外拼出来的一条旁路状态机。

它和启动恢复一样，本质上都是：

- 输入一组已持久化事实和交易所 live facts
- 由执行器判断当前槽位工作集应该怎样重建或保留
- 再决定当前 effect 应该继续等待、认领 live order、确认完成，还是被当前计划 supersede

因此本阶段要求：

- `submit recovery` 的判断逻辑下沉到 `engine executor`
- `effect_worker` 只负责拿到回执、live order、错误等事实，并调用写侧服务提交
- `write_service` 在每个 `grid` 的串行事务边界内统一持久化执行器 transition 与 effect 状态更新，不再拥有独立的 submit recovery 状态机
- `effect_service` 只提供 grid snapshot / pending effect 查询，不再承载恢复判断或任何写路径

### 6.4 恢复认领决策表

`server runtime` 只负责拉取 live facts，不负责判断订单该归属哪个槽位。

执行器对恢复认领输出统一的 `RecoveryResolution`：

```rust
pub enum RecoveryResolution {
    Rebuilt { state: ExecutorState },
    Anomaly {
        state: ExecutorState,
        anomaly: RecoveryAnomaly,
    },
}
```

其中：

- `RecoveryAnomaly` 只表达稳定异常类别，不夹带恢复动作
- `Anomaly.state` 表示执行器给出的异常恢复状态。当前阶段它必须清空无法确认归属的 `slots`，并设置 `recovery_anomaly`

槽位认领规则由执行器统一决定：

1. 一个 live order 仅匹配一个 slot
   - 认领到该 slot
2. 某个 slot 没有对应 live order
   - 清空该 slot，由后续重规划决定是否补单
3. 多个 live orders 同时匹配同一个 slot
   - 进入 `RecoveryAnomaly`
4. 某个 live order 无法匹配任何 slot
   - 进入 `RecoveryAnomaly`
5. 一个 live order 可能匹配多个 slots
   - 进入 `RecoveryAnomaly`

出现 `RecoveryAnomaly` 时，不做部分认领；启动异常恢复路径。

`submit receipt` 的回写也必须遵守同一条边界：

- receipt 只能认领已存在且唯一匹配的 slot
- 如果无法匹配 slot，写侧必须把它暴露成失败，不能静默 no-op 后继续把 effect 标成 `succeeded`

只要异常恢复路径仍未解除，对外 `execution_status` 必须保持为 `attention_required`。

模块责任固定为：

- `server runtime` 拉取 live facts
- `write_service` 拉取 `pending_submit_hints` 并提交 per-grid 写事务
- `engine executor` 产出 `RecoveryResolution`
- `effect_service` 只提供查询

### 6.5 `effect worker` 的边界收窄

[`server/src/effect_worker.rs`](../../../server/src/effect_worker.rs) 改造后只负责：

- 执行 `submit / cancel`
- 收集订单结果和失败事实
- 调用 `write_service` 提交写回

它不再负责：

- 执行策略判断
- 是否应该继续追价
- 是否应该触发整格重算
- 直接持久化 effect 状态

它也不再负责：

- 分类 `submit recovery` 的稳定结果
- 决定某个提交 effect 应该 `Proceed / Recovered / Superseded`

### 6.6 `CancelAll` 降级成异常工具

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
- `slots`
- 执行模式切换
- `DesiredOrders` 规划
- 槽位工作集 diff 与 effect 生成

### 7.3 `server runtime`

拥有：

- 外部事实翻译
- 启动同步调度
- 调用写侧服务

不拥有：

- 执行器状态机规则
- 工作集合并规则

### 7.4 `write_service`

拥有：

- 每个 `grid` 的持久化事务边界
- 每个 `grid` 的写侧串行控制
- `pending_submit_hints` 的读取与去重
- effect 状态与 grid mutation 的统一提交

并且要明确一个运行时不变量：

- 已持久化 effect 的状态写回依赖该 `grid` 已经加载到 write-side runtime
- 如果这个前提不成立，`write_service` 必须返回稳定、可诊断的 invariant violation
- `effect_worker` / `server runtime` 不得把这类错误包装成“交易所执行失败”

不拥有：

- 跨 `grid` 的全局串行化语义
- 执行器状态机规则
- `submit recovery` 的独立判断逻辑

这意味着：

- 同一个 `grid` 的 mutation 必须按顺序提交
- 不同 `grid` 的 mutation 不应该因为同一把全局锁互相阻塞

### 7.5 `effect worker`

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
- 活跃槽位总数上限先保持很小，优先验证工作集模型
- 常规路径先以“有助于库存收敛的订单”为主，不实现完整双边对称网格
- `CatchUp` 只允许更激进的限价行为，不引入 `MARKET`
- 不做 replay benchmark 及其配套回放 harness

## 9. 验收标准

1. 当 `target_exposure` 与 `current_exposure` 存在偏差时，系统会持续维护槽位工作集，而不是只生成一次单笔挂单后停止收敛。
2. 小偏差时进入 `Passive`，偏差扩大或超时未收敛时可升级到 `Rebalance`，再扩大或再超时可升级到 `CatchUp`。
3. 部分成交、完全成交、撤单、拒单后，执行器会更新对应槽位及其 `working_order`，并基于新的库存偏差重新规划。
4. 启动恢复时，系统先吸收 live position 和 live open orders，重建 `slot -> working_order` 关系，再重新规划，不再依赖单个 `pending_order` 锚点补丁。
5. 提交请求、提交回执、live order 认领和终态清理都通过执行器 transition 推进，`manager` / `write_service` / `effect_worker` 不再直接改写槽位。
6. `submit recovery` 由执行器统一判断并输出稳定结果，不再由 `effect_service` / `write_service` / `effect_worker` 组成旁路状态机。
7. 常规改挂不使用 `CancelAll`，而是按槽位工作集 diff 做定点 `cancel / submit`。
8. `effect worker` 不承担执行策略判断，只负责 effect 执行与回写。
9. 写侧对同一个 `grid` 串行提交，但不同 `grid` 不因为全局锁互相阻塞。
10. 当 `DesiredOrders` 与当前槽位工作集等价时，系统返回 `NoOp`，避免无意义重挂。
11. 一期验收测试至少覆盖模式切换、部分成交、启动恢复、submit recovery、定点改挂和无变化 `NoOp` 这几条主路径。
12. list / detail / TUI 读模型不再保留 `pending_order_count` 或 `execution.pending_order` 这类单订单兼容语义。
13. detail / TUI 能直接看到稳定执行摘要，至少包含库存偏差、偏差持续时间、工作订单数量、活跃槽位数量和统计窗口起点。
14. detail / TUI 能看到稳定的累计统计，至少包含 `max_inventory_gap_abs` 和 `max_gap_age_ms`，并能看到 `slots` 明细。

## 10. 后续实现顺序

1. 先补验收测试，锁定执行器行为
2. 引入 `executor_state` 与槽位工作集
3. 将执行规划从 `reconciler` 下移到执行器
4. 改造恢复链路和 effect worker
5. 清理旧的 `pending_order` 中心语义

这次设计的目标不是把执行做成最复杂，而是先把系统从“单笔调仓”推进到“库存执行器”。
