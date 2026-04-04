# Strategy Min Rebalance Units 设计

## 背景

第一版 `min_rebalance_units` 已经落地，但当前语义是：

- 当 `abs(desired_exposure - current_exposure) < min_rebalance_units` 时，不再开始新的调仓
- 一旦已经进入执行生命周期，执行器仍然会继续追逐每次最新的 `desired_exposure`

这会带来一个明显副作用：

- 刚启动或大 gap 阶段，系统会先进入调仓
- 调仓进行中，如果价格连续变化，最新目标会持续轻微漂移
- `SubmitPending` 会被不断 supersede
- `Working` 会被不断 cancel / replace
- 最后只是碰巧在 gap 收敛到阈值以内时停下

也就是说，第一版实现的是“停手阈值”，没有实现“执行中防抖”。

## 目标

- 避免执行过程中为了追求绝对精准调仓而频繁撤单重挂
- 保持系统仍然逐步逼近最新策略目标，而不是永久停在旧目标
- 保持 `desired_exposure` 继续表达最新原始策略目标
- 保持 `reconciler`、`manager`、`executor` 的知识边界清晰
- 让“执行锚点 + 门槛比较”成为 executor 内的单一事实来源

## 非目标

- 不新增第二个独立门槛参数
- 不把这次设计扩展成按绝对金额或总容量比例配置
- 不引入基于时间窗口的防抖
- 不改变现有交易所 floor 语义
- 不改变现有 replacement gate 的“价格改善是否值得改挂”语义

## 问题定义

当前系统把 `min_rebalance_units` 解释成：

- 当前仓位与最新策略目标之间，是否还值得继续调仓

但用户真正需要的是：

- 最新策略目标是否已经变化到值得触发**下一次执行动作**

这里的“下一次执行动作”包括：

- 在没有活动单时，开始一轮新的调仓
- 在存在 `SubmitPending` 时，是否 supersede 当前 pending submit
- 在存在 `Working` 时，是否 cancel / replace 当前工作单

如果仍然把门槛只绑定在 `current_exposure -> latest_target` 上，就会继续出现：

- 调仓已经开始
- 最新目标只变了一点
- 但执行器仍然不断打断当前生命周期去追最新 target

问题不在于“系统会不会停”，而在于“系统在停之前太频繁地改执行计划”。

## 备选方向

### 方向 A：保持现有语义，只调大 `min_rebalance_units`

做法：

- 继续把门槛定义为 `abs(desired_exposure - current_exposure)`
- 通过更大的固定阈值减少频繁调仓

优点：

- 改动最小

缺点：

- 不能解决“已经开始执行后仍频繁 supersede / cancel-replace”这个核心问题
- 只是把第一笔调仓推迟，不能改变执行中的追逐语义

### 方向 B：新增第二个参数，专门控制执行中的改单门槛

做法：

- 保留当前 `min_rebalance_units`
- 再增加一个类似 `min_reprice_units` / `min_retarget_units`

优点：

- 语义最细

缺点：

- 新增一套用户心智模型
- 配置和测试成本上升
- 第一版需求还不足以支撑第二个门槛参数

### 方向 C：重定义 `min_rebalance_units`，让它表示“触发下一次执行动作的最小目标变化”

做法：

- 不再只看 `current_exposure -> latest_target`
- 而是根据当前是否已有活动生命周期，选择不同的锚点来判断是否值得触发下一次执行动作

优点：

- 不新增接口复杂度
- 直接解决频繁 supersede / cancel-replace 的核心问题
- 仍与现有 `exposure` 口径统一

缺点：

- 相比第一版，`min_rebalance_units` 的定义发生变化

## 设计结论

选择方向 C。

`min_rebalance_units` 的新定义是：

- **触发下一次执行动作所需的最小目标变化量**

这意味着：

- 没有活动单时，它决定“要不要开始新一轮调仓”
- 有活动单时，它决定“要不要打断当前这轮并改执行计划”

## 核心设计

### 1. 配置模型

配置字段保持不变：

```rust
pub min_rebalance_units: f64
```

默认值仍为：

- `0.5`

校验规则保持不变：

- `>= 0.0`
- 必须是有限数值

变化只发生在**语义**，不是字段形状。

### 1.1 迁移说明

这不是字段形状变更，而是**配置语义变更**。

旧语义更接近：

- `current_exposure -> latest_target` 的停手阈值

新语义改为：

- 触发下一次执行动作的最小目标变化

因此，已有配置里的同一个数值，例如 `0.5`，在第二版中的效果会是：

- 没有活动生命周期时，仍然决定是否开始一轮新调仓
- 有活动生命周期时，不再要求系统持续追逐每次最新 target
- 相比第一版，会更少触发 `Superseded` 和 `CancelReplace`

调参含义也随之变化：

- 如果希望更频繁跟随最新目标，应调低该值
- 如果希望执行更稳、减少 lifecycle 抖动，应调高该值

这条迁移说明必须同步体现在：

- `README.md`
- 用户可见配置示例

### 2. 参考点与锚点

executor planning 不再总是使用：

- `current_exposure`
- `latest desired_exposure`

来判断是否值得进入下一次执行动作。

而是先定义一个**执行锚点**：

#### 2.1 没有活动生命周期时

如果 inventory core slot 当前是 `Empty`：

- 执行锚点 = `current_exposure`

此时语义是：

- 最新策略目标相对当前仓位的变化，是否足够大，值得开始一轮新调仓

#### 2.2 存在活动生命周期时

如果 inventory core slot 当前是：

- `SubmitPending`
- `Working`

则执行锚点 = 当前 slot 已记录的 `working_order.desired_exposure`

此时语义是：

- 最新策略目标相对“当前这轮正在执行的目标”是否已经变化得足够大，值得打断当前生命周期并开始下一次执行动作

#### 2.3 生命周期结束后

当 slot 回到 `Empty`：

- 执行锚点重新退回 `current_exposure`

这保证系统行为是：

- 一轮一轮推进
- 每轮结束后重新对齐最新目标
- 而不是在一轮执行中持续追逐每次最新 target

### 3. 门槛判断语义

定义：

```text
trigger_delta = abs(latest_desired_exposure - execution_anchor)
```

规则固定为：

- `trigger_delta < min_rebalance_units`
  - 不触发下一次执行动作
- `trigger_delta >= min_rebalance_units`
  - 允许进入下一次执行动作判断

等号语义保持：

- **等于门槛时允许执行**

### 3.1 单一 owner：`rebalance trigger` 策略

“执行锚点是什么”和“这次 target 漂移是否已经大到值得触发下一次执行动作”必须由 executor 内的单一策略抽象拥有，不能由：

- `planning`
- `submit recovery`

各自再实现一遍。

建议落点：

- `engine/src/executor/rebalance_trigger.rs`

它拥有以下知识：

- 当前 slot 是否存在活动生命周期
- 不同 slot 状态下该使用哪种锚点
- `trigger_delta` 的计算方式
- `min_rebalance_units` 的统一比较语义与容差

对外只暴露一个很小的结果，例如：

- 当前锚点
- 当前 `trigger_delta`
- 是否允许触发下一次执行动作

然后：

- `planning` 消费该结果，决定 `NoOp / Submit / CancelReplace`
- `submit recovery` 消费同一结果，决定 `AwaitExchangeState / Superseded / Proceed`

这样做的原因是：

- 后续如果门槛、锚点、浮点容差再调整，只需要改一处
- 不会出现 planning 和 recovery 语义漂移
- 调用方不必理解 uncommon case 的细节

### 4. 对不同 slot 状态的影响

#### 4.1 `Empty`

如果当前没有活动 slot：

- `trigger_delta < min_rebalance_units`
  - `NoOp`
  - 不生成新的 `SubmitOrder`
- `trigger_delta >= min_rebalance_units`
  - 继续按最新策略目标进入现有 planning 路径

#### 4.2 `SubmitPending`

如果当前 slot 是 `SubmitPending`：

- `abs(latest_target - anchored_target) < min_rebalance_units`
  - 不 supersede 当前 pending submit
  - 保留 pending slot
  - 如果当前恢复的就是这张仍然有效的 pending submit，本轮继续 `Proceed`
  - 不因为小幅 target 漂移改成 replacement submit

- `abs(latest_target - anchored_target) >= min_rebalance_units`
  - 允许 supersede 当前 pending submit
  - 再按最新目标决定是否产生新的 submit

也就是说：

- 小幅 target 漂移不应该把 pending submit 一直打掉重来
- 也不应该把当前仍然有效的 pending submit 错误降级成 `AwaitExchangeState`

#### 4.3 `Working`

如果当前 slot 是 `Working`：

- `abs(latest_target - anchored_target) < min_rebalance_units`
  - 不因为目标轻微漂移而 cancel / replace 当前工作单
  - 当前这轮继续执行

- `abs(latest_target - anchored_target) >= min_rebalance_units`
  - 才允许进入正常的 replacement / cancel-replace 判断

这条语义是这次设计最关键的变化：

- 小幅目标漂移不再等同于“当前工作单已经过时”

### 5. 与交易所 floor 的关系

顺序调整为：

1. 先用 `min_rebalance_units` 判断是否值得触发下一次执行动作
2. 只有值得触发时，才继续走真实订单计算
3. 然后再应用交易所 floor：
   - `quantity_step`
   - `min_qty`
   - `min_notional`

这意味着：

- 在活动生命周期内，如果 target 漂移还没超过策略门槛，系统不会因为最新 target 重新计算订单并触发交易所 floor 分支
- 只有在确实值得开启下一次执行动作时，才重新生成针对最新目标的真实订单

### 6. 与 `desired_exposure` 的关系

`desired_exposure` 的语义不变：

- 它仍然表示最新原始策略目标

不允许因为当前执行生命周期被锚定就把：

- `track.desired_exposure`
- `ExposureTargetChanged`

改写成“当前执行目标”。

系统要同时表达两个事实：

1. 最新策略目标是什么
2. 当前执行生命周期围绕哪个目标在推进

第一个属于 `reconciler / manager`，第二个属于 `executor`

补充一条和运行态直接相关的结果：

- `SubmitPending` 的当前 effect 在 drift 仍位于 active anchor 门槛内时，会继续执行当前请求
- `Working` 的当前工作单在同样条件下继续挂着
- 因此运行态看到的效果是：小幅 target 漂移不会在 effect worker 中制造连续 supersede / cancel-replace

### 7. 与 `replacement gate` 的关系

`min_rebalance_units` 和 `replacement gate` 的职责变成：

1. `min_rebalance_units`
   - 决定是否值得触发下一次执行动作
2. `replacement gate`
   - 在已经允许开启下一次执行动作后，再决定当前工作单是否值得为了价格改善而改挂

也就是说：

- `min_rebalance_units` 先挡住“目标只漂了一点，不值得打断当前生命周期”
- `replacement gate` 再处理“既然值得重新规划，这次改挂是否真的划算”

这条顺序在 `resume_track` 场景里也保持一致：

- 如果恢复激活后，最新策略目标相对当前活动锚点的漂移仍低于 `min_rebalance_units`
- 系统不会重新进入 replacement gate 推导 `RoundedMatch`
- 而是继续沿用当前活动生命周期

## 模块边界

### `reconciler`

继续只负责：

- 产出最新原始策略目标

不感知执行锚点。

### `manager`

继续只负责：

- 保存最新 `desired_exposure`
- 把最新策略目标传给 executor

不新增执行中防抖旁路。

### `executor`

executor 统一拥有以下知识：

- 当前是否已有活动生命周期
- 当前生命周期的执行锚点是什么
- `min_rebalance_units` 该如何决定“是否触发下一次执行动作”

其中：

- `planning` 负责把 trigger 决策映射成当前轮次的执行动作
- `submit recovery` 负责把同一 trigger 决策映射成 `AwaitExchangeState / Superseded / Proceed`

但这两条路径都不重新定义锚点与门槛比较规则。

## 测试策略

至少覆盖以下行为：

1. `Empty` 且 `abs(latest_target - current_exposure) < min_rebalance_units`
   - `NoOp`
   - 不生成新 submit

2. `Working` 且 `abs(latest_target - anchored_target) < min_rebalance_units`
   - 保留 working order
   - 不 cancel / replace

3. `SubmitPending` 且 `abs(latest_target - anchored_target) < min_rebalance_units`
   - 不 supersede pending submit
   - 保留 pending slot

4. `Working` 或 `SubmitPending` 且 `abs(latest_target - anchored_target) >= min_rebalance_units`
   - 允许进入下一次执行动作语义

5. 生命周期结束后
   - 如果 `abs(latest_target - current_exposure) >= min_rebalance_units`
     - 允许开始下一轮
   - 如果 `< min_rebalance_units`
     - 停止继续调仓

6. `desired_exposure` 仍保持最新原始策略目标
   - 不因执行锚点而被改写

7. planning 与 recovery 使用同一份 trigger 决策来源
   - 不允许两边各自维护一套锚点与门槛比较 helper

## 风险与权衡

### 成本

- `min_rebalance_units` 的定义相比第一版发生变化
- planning / recovery 需要共享“执行锚点”语义

### 收益

- 直接解决频繁 supersede / cancel-replace 的核心问题
- 不新增第二个配置参数
- 保持“逐步逼近目标”而不是“每一刻都追到最新目标”

### 已知限制

- 这次仍然是固定 `0.5 unit`
- 如果后续还需要区分“开始调仓门槛”和“执行中改单门槛”，再考虑拆成两个参数

## 外部文档要求

由于 `min_rebalance_units` 的对外语义已经从“停手阈值”变成“触发下一次执行动作的最小目标变化”，README 和用户可见配置说明必须同步更新，至少解释：

- 无活动单时的参考点是 `current_exposure`
- 有活动生命周期时的参考点是当前执行目标
- 该参数不再表示“永远追最新 target 直到 gap 小于门槛”
