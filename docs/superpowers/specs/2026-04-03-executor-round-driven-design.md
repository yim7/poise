# 执行器轮次驱动设计

**日期：** 2026-04-03

本文在 [2026-03-29-inventory-executor-architecture-design.md](2026-03-29-inventory-executor-architecture-design.md) 和 [2026-04-02-strategy-min-rebalance-units-design.md](2026-04-02-strategy-min-rebalance-units-design.md) 的基础上，进一步收紧执行器边界，解决当前“价格触发后持续重算、链路有延迟时容易一直调整”的结构性问题。

## 1. 背景

当前系统已经具备这些能力：

- `reconciler` 能持续根据最新 `reference_price` 计算目标仓位
- `executor` 能根据目标仓位和当前仓位生成单槽位执行计划
- `min_rebalance_units` 已经从“停手阈值”改成“触发下一次执行动作的最小目标变化”
- `slot` 已经承担订单生命周期和恢复归属

但当前执行器仍然保留了明显的 tick-driven 特征：

- 每次新价格都会产出新的期望执行输入
- 规划结果仍接近“此刻最优的一张限价单”
- 发单、回执回写、live order 同步存在链路延迟
- 延迟一旦出现，系统容易持续认为“当前挂单已经落后，应再次调整”

这说明当前执行器缺少一个明确的一等语义：

- 当前这一轮执行承诺是什么

## 2. 问题定义

当前 `target_exposure` 在系统里混合了两种语义：

1. 系统当前希望仓位去到哪里
2. 当前执行器这一轮承诺去到哪里

这会导致几个直接问题：

1. “最新目标变化”容易被直接翻译成“应该立刻换单”
2. `planning` 和 `recovery` 都在维护“是否继续当前执行”的判断知识
3. `mode` 目前更像诊断标签，还不是执行策略 owner
4. `slot` 虽然已经负责订单生命周期，但没有明确挂到“执行轮次”之下

## 3. 设计目标

### 3.1 主目标

- 把“当前系统想去哪”和“当前执行承诺去哪”拆开
- 把执行器从 tick-driven 收紧为 round-driven
- 让 `mode` 真正参与执行决策，而不只是诊断标签
- 保留 `slot` 作为订单生命周期 owner
- 在第一阶段不引入 `lane`

### 3.2 非目标

- 这次不引入多 `lane`
- 这次不引入多 `slot` 执行编排
- 这次不改为 `MARKET`
- 这次不在 UI 先暴露新字段
- 这次不重做 server / effect worker 边界

## 4. 备选方向

### 4.1 方向 A：继续当前结构，让 `mode` 直接渗透到 `planning / recovery`

做法：

- 保持当前 `target_exposure` 含义不变
- 在现有逻辑里增加 `match mode`

优点：

- 改动最小

缺点：

- `mode` 会散落到多个文件
- “当前执行承诺”仍然没有独立 owner
- 结构上仍然接近 tick-driven

### 4.2 方向 B：直接引入 `round + lane`

做法：

- 一次性把执行拆成多段仓位执行
- `mode` 再控制 lane 的激进度或 lane 数量

优点：

- 长期能力最强

缺点：

- 当前阶段复杂度明显过高
- 会同时引入分仓、分槽位、部分成交再分配、lane 恢复等新问题

### 4.3 方向 C：第一阶段先引入 `desired_exposure + active_round`，继续单 `slot`

做法：

- 外层运行态把当前系统目标明确命名为 `desired_exposure`
- 执行器新增 `active_round`
- 当前执行计划围绕 `active_round.target_exposure` 和单个 `inventory_core` 槽位运行
- `mode` 变成 round 级策略

优点：

- 直接解决当前一阶问题
- 抽象足够清楚
- 改动面可控

缺点：

- 仍然先保留单槽位，后续若要分层执行还要继续扩展

### 4.4 决策

选择方向 C。

原因：

- 当前最缺的是“执行轮次”抽象，不是“多 lane”
- 它能把复杂度集中到 executor 内部
- 它是把 `mode` 变成真正策略 owner 的最小可行路径

## 5. 核心术语

### 5.1 `desired_exposure`

系统当前希望仓位去到哪里。

它替代当前外层运行态里的 `target_exposure` 主语义。

特点：

- 可以每个 tick 更新
- 它表达当前系统真值
- 它不等于“当前必须立刻按它改挂”

第一阶段对外协议仍保留字段名 `target_exposure`，但这里明确：

- 协议层 `target_exposure` 只投影 `desired_exposure`
- 它不得表示 `ExecutionRound.target_exposure`
- `server` / `projector` / `query` 若继续使用旧字段名，也只能表达“系统当前希望去哪”

### 5.2 `active_round`

执行器当前这一轮执行承诺。

它回答：

- 当前执行器正在把仓位往哪里拉
- 当前这一轮是什么激进度
- 当前这一轮是否还应该继续

### 5.3 `ExecutionRound.target_exposure`

当前执行轮次承诺去到哪里。

它和 `desired_exposure` 不同：

- `desired_exposure` 可以频繁变化
- `ExecutionRound.target_exposure` 只有在开启新 round 或切换 round 时才变化

第一阶段明确：

- `ExecutionRound.target_exposure` 是执行层 target 的唯一 owner
- 当前 `WorkingOrder.target_exposure` 不再保留
- slot 或 order 若需要展示或判断当前 round target，只能读取 `active_round.target_exposure`

如果恢复时出现“存在非空 slot，但不存在 `active_round`”：

- 这是恢复异常
- 不允许从 slot 或 order 反推 target owner
- 必须先显式重建或终止当前 round

### 5.4 `slot`

订单生命周期容器。

它继续负责：

- `Empty / SubmitPending / Working`
- 订单事实的持久化和恢复归属

它不再拥有目标语义，只作为 round 的执行载体。

## 6. 状态模型

第一阶段建议把状态收敛成：

```rust
pub struct TrackRuntime {
    pub desired_exposure: Option<Exposure>,
    pub executor_state: ExecutorState,
}

pub struct ExecutorState {
    pub active_round: Option<ExecutionRound>,
    pub slots: Vec<ExecutionSlot>,
    pub diagnostics: ExecutorDiagnostics,
    pub stats: ExecutionStats,
}

pub struct ExecutionRound {
    pub target_exposure: Exposure,
    pub mode: ExecutionMode,
    pub started_at: DateTime<Utc>,
    pub last_plan_at: Option<DateTime<Utc>>,
    pub last_progress_at: Option<DateTime<Utc>>,
}
```

第一阶段继续保留单个固定 `inventory_core` 槽位。

配套约束：

- `WorkingOrder` 只保留交易所订单事实和执行角色，不再持有 `target_exposure`
- 若实现阶段需要兼容旧快照迁移，也只能在迁移层临时读取旧字段，不得把它继续作为决策输入

## 7. Owner 边界

### 7.1 `reconciler`

只拥有：

- `desired_exposure`

不拥有：

- 当前执行轮次
- 撤改单判断
- 当前订单是否过期

### 7.2 `executor`

只拥有：

- 执行规划流程编排
- 把 `round_policy` 结果应用到运行态
- 基于当前 round 和 `mode` 生成报价与替换计划
- 产出最终 `NoOp / Cancel / Submit` effect

### 7.3 `round_policy`

单点拥有：

- 当前 round 是否仍然有效
- 当前 round 是否应 `Start / Continue / Switch / Finish`
- 何时升级或降级 `mode`

它的输入至少包括：

- `current_exposure`
- `desired_exposure`
- `active_round`
- 当前 slot 摘要
- `observed_at`

这里的“至少包括”描述的是第一阶段完成后的稳定形态。
若在实现顺序上先落 `round_policy`、后落 `active_round` 运行态，过渡期要求是：

- `RoundPolicyInput.active_round` 必须显式建模为 `Option`
- 在 `active_round` 尚未进入 runtime / snapshot 的提交里，这个字段只能为 `None`
- 该阶段若仍需要沿用旧执行锚点，只允许在 `round_policy` 内部临时读取或适配
- `planning`、`recovery`、`manager` 不得因为 `active_round` 还未落地而各自补一套替代字段或局部规则
- 等 `active_round` 落地后，必须在同一个 task 内把执行 target 一次性切到 `active_round.target_exposure`

第一阶段把输入继续收紧为一个共享构造入口：

- `RoundPolicyInput` 只能通过统一工厂构造，例如 `round_policy_input_from_state(...)`
- `planning`、`recovery` 不允许各自再拼一份 slot 摘要或补一组局部字段
- 第一阶段 `RoundPolicySlotSummary` 只暴露：
  - `slot`
  - `phase`
  - `working_side`
  - `working_price`
  - `working_quantity`
- 除这个摘要外，`round_policy` 不直接读取 slot / order 明细

它的输出至少包括：

- `RoundDecision`
- 下一状态下应使用的 `ExecutionMode`

`planning` 和 `recovery` 不允许各自再实现一套 round 有效性判断，只能消费同一份 `round_policy` 决策结果。

第一阶段明确：

- `round_policy` 是 round 生命周期和 `mode` 变化的唯一决策 owner
- `executor` 不得再直接实现“是否开启 / 继续 / 切换 / 结束 round”的局部规则
- 若 `executor` 内部需要以模块形式组织，`round_policy` 也只能作为 executor 的内部子模块存在，不能再暴露第二份同语义 owner

### 7.4 `slot`

只拥有：

- 订单生命周期
- 订单事实归属
- 恢复时的 live order 认领

### 7.5 `manager`

只负责串联：

- 先得到 `desired_exposure`
- 再调用 executor 规划

它不推断执行语义。

## 8. 结构不变量

第一阶段至少保持这些不变量：

1. `desired_exposure` 可以独立于 `active_round.target_exposure` 更新
2. 只要存在 `SubmitPending` 或 `Working` slot，就必须存在 `active_round`
3. 没有 `active_round` 时，全部 slot 必须为空态
4. 当前 slot 内工作单默认服务于 `active_round.target_exposure`
5. `desired_exposure` 更新时，不允许直接改写 slot，必须先经过 round decision
6. recovery 必须先恢复 slot 事实，再决定 round 是继续、结束还是重建
7. `ExecutionRound.target_exposure` 是执行 target 的唯一 owner；slot 和 order 不得保留第二份决策副本
8. `planning` 和 `recovery` 必须消费同一份 `round_policy` 结果，不允许各自实现 round 有效性规则
9. `round_policy` 是 round 生命周期的唯一决策 owner；executor 只负责应用结果与生成 effect
10. 协议层 `target_exposure` 只投影 `desired_exposure`，不得表示 `ExecutionRound.target_exposure`
11. `RoundPolicyInput` 只能由共享工厂构造，`planning` 和 `recovery` 不得各自维护摘要拼装逻辑

## 9. 决策流程

第一阶段的执行流程改成：

1. `reconciler` 计算最新 `desired_exposure`
2. executor 读取当前 `active_round` 和 `slots`
3. executor 调用共享 `round_policy` 做 `round decision`
4. `round decision` 只回答：
   - `StartRound`
   - `ContinueRound`
   - `SwitchRound`
   - `FinishRound`
5. executor 根据当前 round 的 `mode` 计算单槽位报价策略
6. executor 对比当前 slot 事实，生成 `NoOp / Cancel / Submit`

这一步的关键变化是：

- 新价格到来后，系统先问“当前 round 还成立吗”
- 不再先问“是不是应该立刻换一张最新限价单”

## 10. `mode` 的职责

第一阶段保留 `Passive / Rebalance / CatchUp`，但把它们收紧为 round 级策略。

它们决定：

- 当前 working order 的报价容忍度
- 当前 round 内是否允许替换订单
- 多久算 stale
- 当前 round 何时应升级到更激进的 mode

它们不决定：

- `desired_exposure`
- 是否存在 round
- 是否引入 lane
- slot 生命周期记账

### 10.1 `Passive`

- 允许一定程度的报价滞后
- 优先继续当前 round
- 优先减少不必要撤改单

### 10.2 `Rebalance`

- 缩小报价滞后容忍度
- 更积极地认定当前 working order 已过期
- 收敛优先级高于 `Passive`

### 10.3 `CatchUp`

- 使用最短容忍时间
- 允许更激进的限价行为
- 目标是尽快把库存拉回安全区

## 11. Recovery 原则

Recovery 不再把“当前请求是否匹配最新计划请求”作为第一视角，也不单独拥有 round 有效性判断。

第一阶段 recovery 应按下面顺序工作：

1. 根据 live orders 恢复 slot 事实
2. 调用共享 `round_policy` 判断这些 slot 是否仍然从属于当前 `active_round`
3. 若当前 round 仍然有效，则继续当前 round
4. 若当前 round 已失效，则结束或切换 round

恢复时首先面对的是：

- 当前市场上到底有哪些订单事实

而不是：

- 现在又重新算出了一张什么最新理想订单

## 12. 为什么第一阶段不引入 `lane`

`lane` 解决的是“如何把一轮执行再拆成多段仓位”。

这不是当前一阶问题。当前一阶问题是：

- `desired_exposure` 和当前执行承诺没有分开

如果现在直接引入 `lane`，会同时引入：

- 分仓逻辑
- lane 间部分成交分配
- lane 恢复归属
- lane 级 UI 和诊断

这会在当前阶段显著增加认知负担和改动面。

因此第一阶段明确不引入 `lane`，只做：

- `desired_exposure`
- `active_round`
- 单个 `inventory_core` slot

## 13. 验收重点

第一阶段至少要能证明这些行为：

1. `desired_exposure` 可以继续随新价格更新，但不会自动改写 `active_round.target_exposure`
2. 当前 round 未失效时，小幅价格变化不会触发持续改挂
3. 当前 round 失效时，executor 会明确地切换 round，而不是在旧 round 上继续堆补丁
4. `mode` 会真实影响 round 内的替换和 stale 判断
5. recovery 先恢复 slot，再恢复或重建 round

## 14. 后续阶段

第二阶段再讨论：

- 是否引入 `lane`
- 是否让不同 `mode` 生成不同数量的 lane
- 是否把 UI 暴露到 round 和 mode 层

第一阶段不提前承诺这些结构。
