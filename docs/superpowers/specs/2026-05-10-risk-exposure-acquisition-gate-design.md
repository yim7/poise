# 风险暴露获取门控设计

## 背景

当前自动策略会根据价格带曲线持续计算目标敞口。只要当前敞口和目标敞口存在足够偏离，系统就会通过 `CatchUp` 或 `CurveMaker` 追随目标。这个行为在降低风险时是需要的，但在增加风险时可能导致高位加仓、低位加空，暴露获得成本不理想。

本设计的目标是：增加风险暴露时，要求价格先给出成本优势；降低风险暴露时，保持立即响应。

## 核心术语

- `curve_target`：策略曲线在当前价格下给出的真实目标敞口。
- `allowed_target`：成本优势规则当前允许执行到的目标敞口。
- `backlog`：`curve_target - allowed_target`，表示尚未获得成本优势、暂不允许执行的风险暴露。
- `anchor`：上一次启动、释放或重置风险获取门控时的价格和曲线目标。

`curve_target` 继续由策略曲线负责；`allowed_target` 由风险暴露获取门控负责；executor 只能执行到 `allowed_target`，不能直接追 `curve_target` 中尚未释放的部分。

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
initial_ratio = 0.3
advantage_steps = 2.0
min_release_steps = 1.0
max_release_steps = 4.0
catchup_ratio = 0.25
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
| `initial_ratio` | `0.3` | 启动或从零建立方向性仓位时，先允许建立目标仓位的比例。 |
| `advantage_steps` | `2.0` | 价格需要让曲线目标从 anchor 继续走出多少个最小调仓单位，才算获得一次成本优势。 |
| `min_release_steps` | `1.0` | 每次释放的最小步长倍数。 |
| `max_release_steps` | `4.0` | 每次释放的最大步长倍数。 |
| `catchup_ratio` | `0.25` | 根据当前 backlog 动态计算释放量的比例。 |

派生量：

```text
advantage_units = min_rebalance_units * advantage_steps
base_step_units = min_rebalance_units * min_release_steps
max_step_units = min_rebalance_units * max_release_steps
release_units = clamp(backlog_units * catchup_ratio, base_step_units, max_step_units)
release_units = min(release_units, backlog_units)
```

其中 `*_steps` 和 `initial_ratio`、`catchup_ratio` 都是无量纲参数；它们不能和 `curve_target`、`min_rebalance_units` 混用为乘积，除非其中一边是倍数或比例。

## 启动建仓

启动时不直接追满 `curve_target`，也不完全等待。系统先允许建立一部分基础风险暴露：

```text
target_units = abs(curve_target)
ratio_units = target_units * initial_ratio

if target_units < min_rebalance_units:
    initial_units = target_units
else:
    initial_units = max(ratio_units, min_rebalance_units)
    initial_units = min(initial_units, target_units)

allowed_target = sign(curve_target) * initial_units
```

示例：

```text
curve_target = +5.0
min_rebalance_units = 0.5
initial_ratio = 0.3

allowed_target = +1.5
backlog = +3.5
```

剩余 backlog 不按来源拆分。它不是固定欠仓，也不会自动补齐；后续每次价格继续给出成本优势时，只释放一部分。

## 增加风险

如果 `curve_target` 在 `allowed_target` 外侧，说明仍有未释放的风险暴露。价格必须从 anchor 继续朝增加该方向风险的方向移动，并让曲线目标至少多走出 `advantage_units`，才释放一次。

加多：

```text
curve_target > allowed_target
current_curve_target >= anchor_curve_target + advantage_units
```

加空：

```text
curve_target < allowed_target
current_curve_target <= anchor_curve_target - advantage_units
```

释放时：

```text
backlog_units = abs(curve_target - allowed_target)
release_units = clamp(backlog_units * catchup_ratio, base_step_units, max_step_units)
release_units = min(release_units, backlog_units)
allowed_target = move_toward(allowed_target, curve_target, release_units)
anchor_price = current_price
anchor_curve_target = current_curve_target
```

新的价格位置产生的曲线新增需求也进入同一个 backlog，不单独立即释放。也就是说，每到一次更优价格，只释放一次预算；如果 `allowed_target` 仍未追到 `curve_target`，剩余差值继续等待下一次成本优势。

## 降低风险

降低风险不释放新的 backlog。

如果 `curve_target` 仍在 `allowed_target` 外侧，只缩小 backlog，不改变 `allowed_target`：

```text
allowed_target = +1.5
curve_target 从 +5.0 回到 +4.0
allowed_target 仍是 +1.5
backlog 从 +3.5 缩小到 +2.5
```

只有当 `curve_target` 回到 `allowed_target` 内侧，才立即降低风险：

```text
allowed_target = +1.5
curve_target = +1.0
allowed_target -> +1.0
```

这样不会因为小幅反弹就快速平掉已经用成本优势获得的仓位。

如果 `allowed_target` 和 `curve_target` 异号，不能直接把 `allowed_target` 调整到 `curve_target`。这同时包含“降低旧方向风险”和“增加新方向风险”。第一版必须先把旧方向风险降到 0：

```text
allowed_target = +1.5
curve_target = -1.0
allowed_target -> 0.0
```

实际仓位回到 0 后，再按新方向的启动建仓规则处理 `curve_target = -1.0`。这样不会在一次重新评估里绕过新方向的成本优势门控。

## 时间语义

第一版不设置时间过期。

backlog 只由价格和曲线目标变化驱动：

- 价格继续给出成本优势：释放一次。
- 价格回到降低风险方向：backlog 缩小，必要时立即降风险。
- 价格横盘：backlog 保留但不释放。

不引入时间过期，是为了保持规则目标单一：没有成本优势就不自动释放风险暴露。

## CatchUp 语义

`CatchUp` 只能追 `allowed_target` 内的缺口。

```text
current_exposure = +1.0
allowed_target = +1.5
curve_target = +5.0

CatchUp 只能补到 +1.5
不能追到 +5.0
```

如果 `allowed_target` 被释放后，实际仓位还没跟上，`CatchUp` 可以按现有 `min_rebalance_units` 规则补齐 `allowed_target` 内的缺口。

如果 `allowed_target == current_exposure`，但仍存在下一次成本优势释放预算，系统仍需要进入 executor 规划 `CurveMaker`。不能因为没有 `CatchUp` 缺口就跳过 executor，否则等待阶段无法提前挂出下一次优势价限价单。

## CurveMaker 语义

`CurveMaker` 不应被简单禁用。它应该成为获取成本优势的主要方式，但不能绕过风险暴露获取门控。

增加风险时，`CurveMaker` 只允许挂下一次成本优势释放单：

```text
backlog = curve_target - allowed_target
release_units = next_release_units(abs(backlog))
advantage_units = min_rebalance_units * advantage_steps

下一次优势目标：
  加多：anchor_curve_target + advantage_units
  加空：anchor_curve_target - advantage_units
```

`CurveMaker` 可以在下一次优势目标对应的触发价附近挂限价单，但总风险暴露不能超过本次 `release_units`。

示例：

```text
allowed_target = +1.5
curve_target = +5.0
backlog = +3.5
release_units = +0.875
anchor_curve_target = +5.0
advantage_units = +1.0

下一次优势目标 = +6.0
CurveMaker 最多挂 +0.875 的买入风险获取单
```

不允许：

- 按完整 backlog 连续挂多档增加风险单。
- 挂出超过下一次 `release_units` 的增加风险单。
- 保留已经不符合当前门控预算的增加风险 `CurveMaker`。

降低风险方向的 `CurveMaker` 或 reduce-only 行为不受风险获取门控限制，继续保持响应。

## 状态归属

`core/src/strategy.rs` 继续只负责曲线目标和价格带判断，不包含成本优势逻辑。

`engine` 层拥有风险暴露获取门控，因为它需要同时理解：

- 当前仓位和自动状态。
- 曲线目标与价格。
- 风控 cap 后的目标。
- executor 能否规划 `CatchUp` 和 `CurveMaker`。

建议在 `engine` 内引入聚焦模块或聚焦类型，负责把 `curve_target` 过滤为 `allowed_target`，并输出给 executor 的上下文。调用方不应自己判断 backlog、anchor、释放步长或 `CurveMaker` 预算。

## 考虑过的方案

### 只用 `min_rebalance_units`

优点是简单。缺点是它只减少小幅交易，不能表达“增加风险需要成本优势”，也无法约束 `CurveMaker` 绕过。

### 只在 reconciler 压低目标

优点是改动小。缺点是 `CurveMaker` 仍可能提前挂完整未来档位，实际仓位可以绕过 `allowed_target`。

### 禁用增加风险 `CurveMaker`

优点是安全，容易实现。缺点是削弱了 `CurveMaker` 的核心价值，无法利用限价单在优势价格自动成交。

### 选定方案：目标门控 + 下一次释放 `CurveMaker`

该方案把“是否允许增加风险”集中在一个 owner 中，同时让 `CurveMaker` 服务成本优势获取，而不是绕过它。代价是 executor 需要理解门控给出的增加风险预算。

## 验收场景

启动建仓：

- 当前仓位为 0，`curve_target = +5`，`initial_ratio = 0.3` 时，`allowed_target = +1.5`。
- 剩余 `+3.5` 进入 backlog，不立即追满。

优势释放：

- `anchor_curve_target = +5`，`advantage_units = 1` 时，只有当当前曲线目标达到 `+6` 或更高，才释放一次。
- 当前 backlog 为 `+3.5`，`catchup_ratio = 0.25`，`base_step = 0.5`，`max_step = 2.0` 时，释放 `+0.875`。

新增需求进入 backlog：

- 价格跌到新位置后，`curve_target` 从 `+5` 变成 `+6`，新增 `+1` 不立即全量释放，而是进入统一 backlog。

降低风险：

- `allowed_target = +1.5`，`curve_target` 从 `+5` 回到 `+4` 时，不减仓。
- `allowed_target = +1.5`，`curve_target` 回到 `+1` 时，立即把 `allowed_target` 降到 `+1`。
- `allowed_target = +1.5`，`curve_target` 回到 `-1` 时，先把 `allowed_target` 降到 `0`，不直接开空。

`CatchUp`：

- `current_exposure = +1`，`allowed_target = +1.5`，`curve_target = +5` 时，只允许追到 `+1.5`。

`CurveMaker`：

- 存在 backlog 时，只允许下一次优势位置的一步释放单。
- 不允许挂出完整 backlog 的增加风险 maker 单。
- 降风险 maker 或 reduce-only 行为不被该门控阻止。

## 未纳入第一版

- 时间过期策略。
- 多档未来释放 `CurveMaker`。
- 按不同市场波动率动态调整 `advantage_units`。
- 按成本价或成交均价反向校准释放节奏。
