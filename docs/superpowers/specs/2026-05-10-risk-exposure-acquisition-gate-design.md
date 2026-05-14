# 风险暴露获取门控设计

## 背景

当前自动策略会根据价格带曲线持续计算目标敞口。只要当前敞口和目标敞口存在足够偏离，系统就会通过 `CatchUp` 或 `CurveMaker` 追随目标。这个行为在降低风险时是需要的，但在增加风险时可能导致高位加仓、低位加空，暴露获得成本不理想。

本设计的目标是：增加风险暴露时，要求价格先给出成本优势；降低风险暴露时，保持立即响应。

## 核心术语

- `desired_exposure`：策略曲线在当前价格下给出的理论目标敞口。它是减仓规划的依据。
- `risk_release_frontier`：风险暴露获取门控已经释放的新增风险边界。它限制继续增加风险，但不是减仓目标。
- `execution_target_exposure`：executor 当前应该追随的目标敞口。它由 `desired_exposure`、`risk_release_frontier` 和 `current_exposure` 派生。
- `backlog`：`desired_exposure - risk_release_frontier`，表示尚未释放的新增风险暴露。
- `anchor`：上一次启动、释放或重置风险获取门控时的价格和曲线目标。

`desired_exposure` 继续由策略曲线负责；`risk_release_frontier` 由风险暴露获取门控负责；executor 策略只追 `execution_target_exposure`，不需要知道 target 是普通曲线目标还是门控后的执行目标。

## 配置参数

`risk_acquisition` 是每个 track 的参数组，不是全局配置。它默认启用；如果某个 track 没写该子表，系统使用默认值。

配置必须写在对应的 `[[tracks]]` 后面，使用 `[tracks.risk_acquisition]` 子表。不要使用 `risk_acquisition = { ... }` 行内对象形式。

完整示例：

```toml
[[tracks]]
track_id = "btc-core"
symbol = "BTCUSDT"
lower_price = 90000.0
upper_price = 110000.0
long_exposure_units = 8.0
short_exposure_units = 8.0
notional_per_unit = 375.0
min_rebalance_units = 0.5
shape_family = "linear"
out_of_band_policy = "freeze"
max_notional = 3000.0
leverage = 10
daily_loss_limit = 120.0
total_loss_limit = 500.0

[tracks.risk_acquisition]
initial_ratio = 0.5
advantage_steps = 2.0
min_release_steps = 1.0
max_release_steps = 4.0
catchup_ratio = 0.25
stale_release_minutes = 60.0
```

如果有多个 `[[tracks]]`，每个 track 都可以有自己的 `[tracks.risk_acquisition]` 子表；子表归属于它前面最近的那个 `[[tracks]]`。

`out_of_band_policy` 仍然保持单字段形式，因为它是“选择一种带外策略”的枚举：

```toml
out_of_band_policy = "freeze"
out_of_band_policy = "flatten"
out_of_band_policy = { flatten = { trigger = { flatten_confirm = { bps = 500 } }, recover = "back_in_band" } }
```

`risk_acquisition` 是一组同时生效的调参项，所以使用子表。

默认值：

| 字段 | 默认值 | 含义 |
| --- | ---: | --- |
| `initial_ratio` | `0.5` | 启动或从零建立方向性仓位时，先允许建立目标仓位的比例。 |
| `advantage_steps` | `2.0` | 价格需要让曲线目标从 anchor 继续走出多少个最小调仓单位，才算获得一次成本优势。 |
| `min_release_steps` | `1.0` | 每次释放的最小步长倍数。 |
| `max_release_steps` | `4.0` | 每次释放的最大步长倍数。 |
| `catchup_ratio` | `0.25` | 根据当前 backlog 动态计算释放量的比例。 |
| `stale_release_minutes` | `60.0` | 同一个 anchor 等待达到这个分钟数后，即使价格还没走到优势阈值，也释放一批 backlog；设为 `0` 表示关闭时间释放。 |

派生量：

```text
advantage_units = min_rebalance_units * advantage_steps
base_step_units = min_rebalance_units * min_release_steps
max_step_units = min_rebalance_units * max_release_steps
release_units = clamp(backlog_units * catchup_ratio, base_step_units, max_step_units)
release_units = min(release_units, backlog_units)
```

其中 `*_steps` 和 `initial_ratio`、`catchup_ratio` 都是无量纲参数；它们不能和 `desired_exposure`、`min_rebalance_units` 混用为乘积，除非其中一边是倍数或比例。

## 启动建仓

启动时不直接追满 `desired_exposure`，也不完全等待。系统先允许建立一部分基础风险暴露：

```text
target_units = abs(desired_exposure)
ratio_units = target_units * initial_ratio

if target_units < min_rebalance_units:
    initial_units = target_units
else:
    initial_units = max(ratio_units, min_rebalance_units)
    initial_units = min(initial_units, target_units)

risk_release_frontier = sign(desired_exposure) * initial_units
```

示例：

```text
desired_exposure = +5.0
min_rebalance_units = 0.5
initial_ratio = 0.5

risk_release_frontier = +2.5
backlog = +2.5
```

剩余 backlog 不按来源拆分。它不是固定欠仓，也不会自动补齐；后续每次价格继续给出成本优势时，只释放一部分。

## 增加风险

如果 `desired_exposure` 在 `risk_release_frontier` 外侧，说明仍有未释放的风险暴露。系统满足任一条件时释放一次：价格从 anchor 继续朝增加该方向风险的方向移动，并让曲线目标至少多走出 `advantage_units`；或同一个 anchor 等待达到 `stale_release_minutes`。

加多：

```text
desired_exposure > risk_release_frontier
current_curve_target >= anchor_curve_target + advantage_units
```

加空：

```text
desired_exposure < risk_release_frontier
current_curve_target <= anchor_curve_target - advantage_units
```

每次 reconcile 先确认上一段释放是否已经被实际仓位吸收：

```text
long:  current_exposure >= risk_release_frontier
short: current_exposure <= risk_release_frontier
```

如果实际仓位已经到达或穿过释放边界，且没有超过 `desired_exposure`，就把 `risk_release_frontier` 推进到 `current_exposure`。这只是承认已经拿到的仓位，不算一次新的释放，也不会重置 anchor。

释放时：

```text
backlog_units = abs(desired_exposure - risk_release_frontier)
release_units = clamp(backlog_units * catchup_ratio, base_step_units, max_step_units)
release_units = min(release_units, backlog_units)
risk_release_frontier = move_toward(risk_release_frontier, desired_exposure, release_units)
anchor_price = current_price
anchor_curve_target = current_curve_target
anchor_started_at = current_time
```

新的价格位置产生的曲线新增需求也进入同一个 backlog，不单独立即释放。也就是说，每到一次更优价格，只释放一次预算；如果 `risk_release_frontier` 仍未追到 `desired_exposure`，剩余差值继续等待下一次成本优势。

如果实际仓位还没到达上一批 `risk_release_frontier`，系统先让 `CatchUp` 补齐到当前 `execution_target_exposure`，不继续释放下一批 backlog。

## 降低风险

降低风险不释放新的 backlog。

如果 `desired_exposure` 仍在 `risk_release_frontier` 外侧，只缩小 backlog，不改变 `risk_release_frontier`：

```text
risk_release_frontier = +1.5
desired_exposure 从 +5.0 回到 +4.0
risk_release_frontier 仍是 +1.5
backlog 从 +3.5 缩小到 +2.5
```

只有当 `desired_exposure` 回到 `risk_release_frontier` 内侧，才退出门控并立即按理论目标降低风险：

```text
risk_release_frontier = +1.5
desired_exposure = +1.0
execution_target_exposure -> +1.0
```

这样不会因为小幅反弹就快速平掉已经用成本优势获得的仓位。

如果 `risk_release_frontier` 和 `desired_exposure` 异号，不能直接把执行目标调整到 `desired_exposure`。这同时包含“降低旧方向风险”和“增加新方向风险”。第一版必须先把旧方向风险降到 0：

```text
risk_release_frontier = +1.5
desired_exposure = -1.0
execution_target_exposure -> 0.0
```

实际仓位回到 0 后，再按新方向的启动建仓规则处理 `desired_exposure = -1.0`。这样不会在一次重新评估里绕过新方向的成本优势门控。

## 时间语义

默认设置时间释放：`stale_release_minutes = 60.0`。

backlog 主要由价格和曲线目标变化驱动，但同一个 anchor 等待过久时允许释放一批，避免价格长期没有走到优势阈值导致目标永远追不满：

- 价格继续给出成本优势：释放一次。
- 同一个 anchor 等待达到 `stale_release_minutes`：释放一次。
- 价格回到降低风险方向：backlog 缩小，必要时立即降风险。
- 价格横盘：backlog 保留；达到时间阈值后释放一批。

时间释放和价格优势释放使用同一套 `release_units` 计算，不另设数量规则。每次释放都会重置 `anchor_price`、`anchor_curve_target` 和 `anchor_started_at`。如果 `stale_release_minutes = 0`，时间释放关闭，只保留价格优势释放。

## CatchUp 语义

`CatchUp` 只追 `execution_target_exposure`，不直接理解风险门控上下文。

```text
current_exposure = +1.0
risk_release_frontier = +1.5
desired_exposure = +5.0
execution_target_exposure = +1.5

CatchUp 只能追到 +1.5，不能追到 +5.0
```

如果实际仓位已经穿过 `risk_release_frontier`，但没有超过 `desired_exposure`，`execution_target_exposure` 等于当前仓位：

```text
desired_exposure = +5.0
risk_release_frontier = +1.5
current_exposure = +2.0
execution_target_exposure = +2.0
```

这不会触发减仓，也不会继续加仓。后续新的释放仍然要等待价格优势或时间释放。

## CurveMaker 语义

`CurveMaker` 只保留降低风险方向的价值：提前在理论曲线减仓位置挂 reduce-only maker 单。

第一版不规划任何 increase maker。增加风险统一交给 `CatchUp` 追随 `execution_target_exposure`，避免 maker 挂单绕过风险释放边界。

降低风险方向的 `CurveMaker` 必须基于 `desired_exposure` 判断，而不是基于 `risk_release_frontier`。这样不会因为实际仓位超过释放边界、但仍未超过理论目标，就过早减掉优势价格拿到的仓位。

## 状态归属

`core/src/strategy.rs` 继续只负责曲线目标和价格带判断，不包含成本优势逻辑。

`engine` 层拥有风险暴露获取门控，因为它需要同时理解：

- 当前仓位和自动状态。
- 曲线目标与价格。
- 风控 cap 后的目标。
- executor 能否规划 `CatchUp` 和 `CurveMaker`。

`risk_exposure_gate` 负责维护 `risk_release_frontier`、anchor 和 release 节奏。`reconciler` 保留理论 `desired_exposure`，并把门控状态交给 executor。executor 顶层负责把 `current_exposure`、`desired_exposure` 和可选 `risk_release_frontier` 转成 `execution_target_exposure`；具体策略只追这个 target。

## 考虑过的方案

### 只用 `min_rebalance_units`

优点是简单。缺点是它只减少小幅交易，不能表达“增加风险需要成本优势”，也无法约束 `CurveMaker` 绕过。

### 只在 reconciler 压低 desired

优点是改动小。缺点是 `desired_exposure` 会同时表示理论目标和执行目标，减仓 maker 会误把风险释放边界当成减仓目标。

### 禁用增加风险 `CurveMaker`

优点是安全，容易实现。缺点是增加风险主要依赖 `CatchUp`，可能牺牲一部分 maker 成本优势。

### 选定方案：风险释放边界 + 执行目标

该方案把“是否允许增加风险”集中在 `risk_exposure_gate`，把“当前应该追什么目标”集中在 executor 顶层，具体策略不理解门控上下文。代价是 executor 需要同时接收理论目标和释放边界，以便 `CatchUp` 与 reduce maker 使用不同依据。

## 验收场景

启动建仓：

- 当前仓位为 0，`desired_exposure = +5`，`initial_ratio = 0.5` 时，`risk_release_frontier = +2.5`。
- 剩余 `+2.5` 进入 backlog，不立即追满。

优势释放：

- `anchor_curve_target = +5`，`advantage_units = 1` 时，只有当当前曲线目标达到 `+6` 或更高，才释放一次。
- 当前 backlog 为 `+3.5`，`catchup_ratio = 0.25`，`base_step = 0.5`，`max_step = 2.0` 时，释放 `+0.875`。

新增需求进入 backlog：

- 价格跌到新位置后，`desired_exposure` 从 `+5` 变成 `+6`，新增 `+1` 不立即全量释放，而是进入统一 backlog。

降低风险：

- `risk_release_frontier = +1.5`，`desired_exposure` 从 `+5` 回到 `+4` 时，不减仓。
- `risk_release_frontier = +1.5`，`desired_exposure` 回到 `+1` 时，退出门控并按 `+1` 减仓。
- `risk_release_frontier = +1.5`，`desired_exposure` 回到 `-1` 时，先把执行目标降到 `0`，不直接开空。

`CatchUp`：

- `current_exposure = +1`，`risk_release_frontier = +1.5`，`desired_exposure = +5` 时，只允许追到 `+1.5`。
- `current_exposure = +2`，`risk_release_frontier = +1.5`，`desired_exposure = +5` 时，不减仓，frontier 推进到 `+2`。

`CurveMaker`：

- 不规划 increase maker。
- reduce maker 只基于 `desired_exposure`，且必须 reduce-only。

## 未纳入第一版

- 时间过期策略。
- 多档未来释放 `CurveMaker`。
- 按不同市场波动率动态调整 `advantage_units`。
- 按成本价或成交均价反向校准释放节奏。
