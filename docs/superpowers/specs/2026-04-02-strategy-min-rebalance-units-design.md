# Strategy Min Rebalance Units Design

**背景**

当前系统已经在执行域补上了一层交易所最小下单门槛：

- `quantity_step`
- `min_qty`
- `min_notional`

这解决了“交易所根本不会接受的碎单”问题，但还没有解决另一层问题：

- 某些调仓虽然满足交易所门槛
- 但从策略角度看仍然过小、过碎
- 在连续 tick 下会产生很多小幅调仓意图

当前策略目标是连续函数，见 [../../../core/src/strategy.rs](../../../core/src/strategy.rs)。  
只要 `target_exposure` 和 `current_exposure` 存在细小差异，系统就可能继续尝试进入执行路径。

## 目标

- 为策略增加一层独立于交易所限制的“最小调仓单位”。
- 减少满足交易所门槛、但从策略角度仍然过碎的小调仓。
- 保持 `target_exposure` 继续表达原始策略目标，不把策略目标和执行抑制混在一起。
- 不破坏现有 `Working / SubmitPending / Empty` 的执行状态机语义。

## 非目标

- 不把这次设计扩展成按绝对金额配置的门槛。
- 不在本次引入按总容量比例的门槛。
- 不改变现有 `replacement gate` 的价格改善语义。
- 不改动 `submit recovery` 的主语义。
- 不新增时间窗口防抖。

## 问题定义

现在有两层不同的“是否值得下单”判断：

1. **交易所门槛**
   - 订单是否可被交易所接受
2. **策略门槛**
   - 这次调仓是否足够大，值得策略实际执行

当前代码只有第一层，没有第二层。  
因此会出现：

- 订单虽然“可下”
- 但仓位变化仍然过小
- 导致策略在连续价格变化下产生很多细碎调仓

## 候选方向

### 方向 A：按绝对金额设置最小调仓门槛

做法：

- 配置一个最小调仓金额，例如低于 `50 USDT` 不调。

优点：

- 直观。
- 和“订单金额太小”这个感受接近。

缺点：

- 用户反馈这种口径不方便。
- 与现有 `exposure` 语义不统一。
- 不同价格区间和不同策略下解释成本更高。

### 方向 B：按总容量比例设置最小调仓门槛

做法：

- 配置一个比例，例如低于总容量 `5%` 不调。

优点：

- 大策略自然门槛更大。
- 容易压住大仓位下的小碎单。

缺点：

- 同样的 `inventory_gap` 在不同配置下行为会变化，理解成本更高。
- 与现有 `current_exposure / target_exposure / inventory_gap` 的统一口径不完全一致。

### 方向 C：按固定 `exposure unit` 设置最小调仓门槛

做法：

- 配置 `min_rebalance_units`
- 当 `abs(target_exposure - current_exposure) < min_rebalance_units` 时，不生成新的策略调仓目标单

优点：

- 与现有 `exposure` 语义完全统一。
- 最容易解释和测试。
- 不会因为策略总容量变化而让门槛语义漂移。

缺点：

- 对非常大容量的策略，固定 `0.5 unit` 可能仍偏小。

## 设计结论

选择方向 C。

第一版使用固定 `exposure unit` 门槛：

- 新增配置项：`min_rebalance_units`
- 默认值：`0.5`

理由：

- 它和当前系统最核心的策略语义保持一致：
  - `current_exposure`
  - `target_exposure`
  - `inventory_gap`
- 不需要重新引入另一套金额口径或容量比例口径。
- 适合先快速稳定“细碎调仓”问题。

如果后续验证发现大容量策略下 `0.5 unit` 仍然太碎，再考虑升级为：

- `max(固定 unit 门槛, 容量比例门槛)`

但这不属于第一版范围。

## 核心设计

### 1. 配置模型

在 `TrackConfig` 中新增字段：

```rust
pub min_rebalance_units: f64
```

约束：

- 必须 `>= 0.0`
- 必须是有限数值，拒绝 `NaN` / `inf`
- 默认值 `0.5`

语义：

- 仅用于策略级最小调仓判断
- 不替代交易所门槛

### 2. 判断落点

该门槛属于策略语义，但不放在 `reconciler`，也不放在 `manager`。

落点放在 `executor planning`。

原因：

- `reconciler` 负责产出原始策略目标，不应该改写或抹平 `target_exposure`
- `manager` 不应该复制 `Working / SubmitPending / Empty` 的执行状态机知识
- `executor planning` 已经负责把“目标 -> 真实订单 / NoOp / CancelOrder”收敛成单一执行语义

因此顺序变成：

1. `reconciler` 继续正常计算原始 `target_exposure`
2. `executor planning` 先检查策略门槛：
   - `abs(target_exposure - current_exposure) < min_rebalance_units`
3. 若未达到策略门槛，不生成新的 `desired_order`
4. 若达到策略门槛，再继续走现有交易所门槛判断：
   - `quantity_step`
   - `min_qty`
   - `min_notional`

这意味着系统最终有两层门槛：

1. **策略门槛**：`min_rebalance_units`
2. **交易所门槛**：真实订单 floor

#### 2.1 门槛等号语义

本方案固定采用：

- `abs(target_exposure - current_exposure) < min_rebalance_units`
  - 抑制执行
- `abs(target_exposure - current_exposure) >= min_rebalance_units`
  - 允许继续进入执行判断

也就是说，**等于门槛时视为“值得调仓”**。

#### 2.2 数值稳定性约定

`exposure` 和 `min_rebalance_units` 都是浮点数，实现时必须使用统一的门槛比较辅助函数，而不是在多个调用点直接裸比较。

要求：

- 对外语义仍以上面的 `< / >=` 为准
- 允许实现层引入一个很小的内部容差，只用于避免浮点抖动和 flaky 测试
- 该容差不得改变“等于门槛时允许执行”这一对外契约

也就是说：

- 容差是实现细节
- 不是第三套新的业务门槛

### 3. 对现有 slot 状态机的影响

在 `executor planning` 中，“小于策略门槛”只意味着：

- 不生成新的 `desired_order`

后续仍沿用现有 diff 语义，而不是新增旁路规则。

这里的关键约束是：

- “小于策略门槛”只决定**不生成新的 `desired_order`**
- 不改变 `diff_desired_orders(desired_order = None, current_slot)` 已有的状态机边界
- 如果实现里需要为 `SubmitPending` 增加保留语义，也必须明确在该分支中表达，不能通过改写 `target_exposure` 或 `manager` 侧旁路逻辑实现

#### `Empty`

- 返回 `NoOp`

#### `Working`

- 如果当前已有工作单，而新的策略目标已经低于策略门槛，则应按现有 `desired_order = None` 语义处理：
  - 产出 `CancelOrder`

原因：

- 既然策略认为这次调仓已经不值得继续执行，就不应继续保留旧调仓单。
- 这不是新的特例，而是沿用现有 `desired_order = None` 的工作单处理语义。

#### `SubmitPending`

- 如果当前 slot 仍是 `SubmitPending`，则必须保留 pending slot，返回 `NoOp`

原因：

- 不能因为门槛变化把 in-flight submit 静默抹掉
- 后续 receipt / live order / recovery 仍需要能够吸收这张单

这条语义必须继续保留。

在 `submit recovery` 路径里也必须保持同样约束：

- recovery 复算当前计划时，必须使用同一份 `min_rebalance_units`
- 如果当前计划因为策略门槛或交易所 floor 不再生成新的 submit，且匹配的 `SubmitPending` slot 仍存在，则应等待 exchange state，而不是再次 submit 或清掉 pending slot
- 如果该 slot 已经被 startup / periodic sync 清掉，则 stale pending effect 应正常 `Superseded`，避免 effect 永远停在 pending

### 3.1 大于策略门槛但小于交易所 floor

如果：

- `abs(target_exposure - current_exposure) >= min_rebalance_units`
- 但换算成真实订单后，仍然低于交易所门槛

则仍按现有执行层交易所 floor 语义处理，而不是回退成“策略门槛未命中”。

要求：

- `target_exposure` 仍保留原始策略目标
- 不生成新的 `SubmitOrder`
- 三种 slot 继续按 `desired_order = None` 的既有分支处理：
  - `Empty`：`NoOp`
  - `Working`：`CancelOrder`
  - `SubmitPending`：保留 pending slot，`NoOp`

也就是说：

- 策略门槛决定“这次调仓值不值得进入执行”
- 交易所 floor 决定“这次执行在交易所侧是否可形成真实订单”

两者必须按顺序叠加，不能混成同一层判断。

### 4. `target_exposure` 与事件语义

该设计不改变 `target_exposure` 的语义。

要求：

- `target_exposure` 继续表示原始策略目标
- 小于策略门槛时，不得把 `target_exposure` 改写成 `current_exposure`
- 如果策略目标确实变化，`ExposureTargetChanged` 仍应照常发出

也就是说：

- “策略目标变化了”
- “执行器选择暂时不下单”

这两个事实要继续分开表达。

### 5. 与现有 `replacement gate` 的关系

`min_rebalance_units` 不替代 `replacement gate`。

两者职责不同：

- `min_rebalance_units`
  - 决定“这次策略调仓是否大到值得进入执行”
- `replacement gate`
  - 决定“已有同方向工作单时，是否值得为了更好价格而改挂”

因此顺序上：

1. 先由 `min_rebalance_units` 判断是否要进入本次调仓
2. 若进入执行，再由 `replacement gate` 判断是否需要替换已有订单

## 测试策略

至少覆盖以下行为：

1. `inventory_gap < min_rebalance_units` 且当前无 slot
   - 返回 `NoOp`
   - 不生成新 `SubmitOrder`

2. `inventory_gap < min_rebalance_units` 且当前有 `Working`
   - 仍按现有语义产出 `CancelOrder`

3. `inventory_gap < min_rebalance_units` 且当前有 `SubmitPending`
   - 返回 `NoOp`
   - 保留 pending slot

4. 小于策略门槛但原始策略目标确实变化时
   - `target_exposure` 保持原始策略目标
   - `ExposureTargetChanged` 仍正常发出

5. 大于策略门槛但小于交易所 floor 时
   - `target_exposure` 保持原始策略目标
   - 不生成新的 `SubmitOrder`
   - `Empty / Working / SubmitPending` 分别遵循既有 `desired_order = None` 语义
   - 不允许两层门槛混淆

6. `min_rebalance_units = 0.0`
   - 退化为关闭策略门槛
   - 行为与当前仅靠交易所 floor 的版本一致

## 风险与权衡

### 成本

- 配置模型新增一个策略字段
- executor planning 多一层判断

### 收益

- 明确抑制“策略上仍然太碎”的调仓
- 保持 `target_exposure` 语义稳定
- 不把执行状态机知识扩散到 `manager` 或 `reconciler`

### 已知限制

- 固定 `0.5 unit` 对非常大容量策略可能仍偏小
- 第一版不解决“门槛随容量变化”的问题

## 后续扩展

如果第一版仍无法满足大容量策略，可以在未来扩展为：

```text
effective_min_rebalance_units =
max(min_rebalance_units, total_capacity_units * min_rebalance_ratio)
```

但扩展前提是：

- 继续保持 `target_exposure` 语义不变
- 继续把执行判断留在 `executor planning`
