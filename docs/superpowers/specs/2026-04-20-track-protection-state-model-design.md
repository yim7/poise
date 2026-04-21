# Track Protection State Model Design

## 背景

前一版设计把几个不同层次的知识混在了一起：

- 带外策略名称同时表达动作、恢复条件和运行阶段
- `freeze` 和 `flatten` 的运行时语义没有分清“当前动作”和“后续升级路径”
- `BandRecoverPolicy` 同时挂在 `freeze` 和 `flatten` 上，导致 `freeze` 也被迫承载价格确认这类并不自然的复杂度
- `Holding` 语义介于自动保护和人工控制之间，价值不明确

这会持续带来两个问题：

- 认知负担高：读代码的人很难快速回答“当前为什么是 `frozen`”“什么时候会真正平到 0”“恢复确认到底在保护哪一步”
- 变更扩散：只要调整带外保护，就会同时触碰 policy、runtime state、恢复规则、对外展示

这次设计的目标是重新梳理语义边界，而不是继续在旧状态机上补字段。

## 目标

- 保留 `freeze / flatten / terminate` 这三个对外策略名
- 删除 `hold`
- 明确区分：
  - 策略配置想表达什么
  - 运行时当前处在什么阶段
  - 对外状态如何投影
- 让 `freeze` 成为真正的“无成本等待回归”
- 让 `flatten` 成为“先等待，再在更外侧保护触发带真正退到 0，并按确认规则自动恢复”
- 维持当前 risk 语义：亏损触发直接 `terminate`

## 非目标

- 本设计不引入时间确认
- 本设计不引入新的 trailing stop 或额外止损线
- 本设计不改变执行器下单方式
- 本设计不把 `TrackState` 暴露到协议层
- 本设计不在第一版加入“反复 `flatten` 自动转 `terminate`”；这仍由现有 risk 预算负责

## 设计结论

### 1. `BandProtectionPolicy` 只表达静态保护决策

自动带外策略保留三个显式选项：

```rust
enum BandProtectionPolicy {
    Freeze,
    Flatten {
        trigger_bps: u32,
        recover: BandRecoverPolicy,
    },
    Terminate,
}

enum BandRecoverPolicy {
    BackInBand,
    PriceConfirm { bps: u32 },
}
```

语义：

- `Freeze`
  - 价格一出主带就进入保护
  - 不主动减仓到 `0`
  - 回到主带后立即恢复
- `Flatten`
  - 价格一出主带不会立刻平仓，而是先进入等待阶段
  - 只有继续朝带外方向走，穿过更外侧的触发带，才真正进入 `flattening`
  - 真正 `flattening` 后，目标压到 `0`
  - 后续按 `recover` 自动恢复
- `Terminate`
  - 价格一出主带就直接终止

这里的关键是：

- `trigger_bps` 表示“外侧触发确认”，决定什么时候真正执行 `flatten`
- `recover` 表示“内侧恢复确认”，决定 `flatten` 之后什么时候允许自动恢复

两者都属于 `flatten` policy，但负责的是不同阶段的不同决策，不能合成一个参数。

### 2. `freeze` 和 `flatten` 还是两条独立策略

虽然 `flatten` 的前置阶段在行为上看起来像“冻结”，但它不应和 `freeze` 策略本身合并。

原因：

- `freeze` 想表达的是“只等待回归，不升级到平仓”
- `flatten` 想表达的是“先等待，如果继续恶化再退到 0”
- 如果把两者合成一条组合策略，配置层会失去直接表达“我只想 freeze”的能力

所以：

- `freeze` 是独立策略
- `flatten` 内部可以包含一个“等待确认是否真正 flatten”的阶段

### 3. `TrackState` 继续是 source of truth

完整运行态仍然是唯一主状态：

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
    Frozen {
        target_anchor: Exposure,
    },
    FlattenPending {
        target_anchor: Exposure,
        boundary: BandBoundary,
    },
    Flattening {
        boundary: BandBoundary,
    },
}

enum ManualState {
    Flattened,
    TargetOverride { target: Exposure },
}
```

和上一版相比，这里有三点变化：

- 删除 `Holding`
- `Frozen` 不再携带 `guard`
- 新增 `FlattenPending`

### 4. `Frozen`、`FlattenPending`、`Flattening` 的职责分开

#### `Frozen`

只用于 `freeze` 策略。

语义：

- 进入时采样 `target_anchor`
- 保护期间每次 reconcile 都继续以 `target_anchor` 作为目标
- 一旦价格回到主带内，立即回到 `FollowingBand`

`Frozen` 不需要 `guard`，因为它不需要记忆 breach side，也不需要价格确认恢复。

#### `FlattenPending`

只用于 `flatten` 策略的前置阶段。

语义：

- 价格刚出主带时进入
- 当前目标仍保持 `target_anchor`
- 记录最初带外方向 `boundary`
- 如果价格回到主带内但还没触发真正 `flatten`，直接回到 `FollowingBand`
- 如果价格继续朝同一方向走，穿过外侧 `trigger_bps` 触发带，才进入 `Flattening`
- 如果价格还没回主带就从另一侧再次带外，旧的 pending 必须立即丢弃，并按当前观察到的带外方向重建新的 `FlattenPending`

这说明：

- `FlattenPending` 在动作上像冻结
- 但它不是 `freeze` 策略
- 它是 `flatten` 策略内部的等待阶段

#### `Flattening`

只用于 `flatten` 策略真正生效后的阶段。

语义：

- 目标固定为 `0`
- 保留最初触发方向 `boundary`
- 后续按 `BandRecoverPolicy` 自动恢复

### 5. `target_anchor` 的定义收窄

`target_anchor` 只属于“保护期间继续维持原目标”的状态：

- `Frozen`
- `FlattenPending`

定义：

- 它是进入保护状态前最后一个经过 risk 批准的目标仓位
- 如果当次没有可用的 risk-approved target，才回退为当前仓位
- 它不是当前实际仓位
- 它不是 executor active-round anchor
- risk cap 可以压低本轮派生目标，但不能反向改写已采样的 `target_anchor`

`Flattening` 不再需要 `target_anchor`，因为它的目标已经固定为 `0`

### 6. `BandBoundary` 只属于 `flatten` 路径

上一版把“恢复确认需要的运行时记忆”抽成了通用 `ReentryGuard`。
这次重新梳理后，真正需要 `boundary` 的只有 `flatten` 路径：

- `FlattenPending` 需要知道价格是从哪一侧带外，才能判断是否进一步穿过外侧触发带
- `Flattening` 需要知道价格是从哪一侧触发，才能按同侧的 `price_confirm` 做恢复判断

而 `freeze` 并不需要这些信息。

因此：

- 不再保留通用 `ReentryGuard`
- `boundary` 直接成为 `FlattenPending` 和 `Flattening` 的状态字段

这样能减少一个泛化过度、但实际只被 `flatten` 使用的中间抽象。

### 7. `flatten.trigger_bps` 和 `recover` 的精确定义

对于主带 `[lower_price, upper_price]`：

```rust
band_width = upper_price - lower_price
trigger_distance = band_width * trigger_bps / 10_000
recover_distance = band_width * recover_bps / 10_000
```

#### 外侧触发确认

仅用于 `FlattenPending -> Flattening`

- `boundary = Below`
  - 当 `price <= lower_price - trigger_distance` 时进入 `Flattening`
- `boundary = Above`
  - 当 `price >= upper_price + trigger_distance` 时进入 `Flattening`

#### 内侧恢复确认

仅用于 `Flattening -> FollowingBand`

- `BackInBand`
  - 价格回到 `[lower_price, upper_price]` 就恢复
- `PriceConfirm { bps }`
  - `boundary = Below`
    - 当 `price >= lower_price + recover_distance` 时恢复
  - `boundary = Above`
    - 当 `price <= upper_price - recover_distance` 时恢复

所以：

- `trigger_bps` 防止“刚出带一点点就立刻止损”
- `recover` 防止“刚回带内一点点就立刻重开”

### 8. 对外状态投影保持简单

对外公开状态继续保持少量稳定值，不暴露 `FlattenPending`：

- `FollowingBand` -> `active`
- `Frozen` -> `frozen`
- `FlattenPending` -> `frozen`
- `Flattening` -> `flattening`

同时，非生命周期的中性展示文案也不再复用 `hold/holding` 这组词，避免和已删除的自动保护语义重新混淆。
- `ManualState::Flattened` -> `manual_flattening`
- `Paused` -> `paused`
- `Terminated` -> `terminated`

理由：

- `FlattenPending` 虽然在内部语义上属于 `flatten` 策略
- 但对外行为上它仍然是“暂停增加风险、等待确认”
- 没必要为此新增 `flatten_pending` 这种公开生命周期名

策略意图由 `strategy.out_of_band_policy` 解释，运行阶段由 `status.lifecycle` 解释，两者不混在一个枚举值里。

### 9. 手动命令继续独立于自动保护策略

手动命令保持这三种：

- 手动 `flatten` -> `Running(Manual(Flattened))`
- 手动 `target override` -> `Running(Manual(TargetOverride { target }))`
- 手动 `terminate` -> `Terminated { cause: ManualCommand }`

这里继续坚持：

- 手动 `flatten` 不是自动 `flatten`
- 手动 `terminate` 和自动 `terminate` 共享同一个终态，区别只在 `TerminationCause`

### 10. risk 仍然直接拥有最终止损

risk 语义不改：

- `max_notional` 超限 -> `Cap`
- `daily_loss_limit` 触发 -> `Terminate(DailyLossLimit)`
- `total_loss_limit` 触发 -> `Terminate(TotalLossLimit)`

也就是说：

- 自动 `flatten` 负责“临时规避风险并按条件自动恢复”
- risk 负责“真实亏损已经不能继续运行”

“反复 `flatten` 是否最终自动转 `terminate`”是未来增强项，不进入第一版状态机。第一版继续依赖现有 risk 预算吸收边缘来回止损造成的真实损耗。

## 模块 ownership

### `core::strategy`

拥有：

- `BandProtectionPolicy`
- `BandRecoverPolicy`
- `BandBoundary`
- 与 `flatten.trigger_bps`、`recover` 有关的纯函数判定 helper

不拥有：

- `target_anchor`
- `FlattenPending`
- 手动命令状态
- risk terminate 语义

### `engine`

拥有：

- `TrackState`
- `AutoState`
- `ManualState`
- `target_anchor`
- `FlattenPending` / `Flattening` 的运行时演进
- 从 policy 到 runtime state 的状态迁移

### `core::risk`

拥有：

- `RiskOutcome`
- `RiskTerminationCause`

不拥有：

- 自动 `flatten`
- 自动 `freeze`
- 顶层 `TerminationCause`

### `application / server`

继续只消费：

- 公开 `TrackReadModel`
- 公开生命周期投影
- 配置后的 `out_of_band_policy`

不接触：

- `TrackState`
- `AutoState`
- `ManualState`
- `target_anchor`
- `FlattenPending`

## 配置形状

稳定配置形状改为：

```toml
out_of_band_policy = { freeze = {} }
out_of_band_policy = { flatten = {
  trigger_bps = 500,
  recover = { price_confirm = { bps = 500 } }
} }
out_of_band_policy = { terminate = {} }
```

默认值建议：

- 如果用户完全不配 `out_of_band_policy`，默认仍为 `freeze`
- 但默认值只是省略配置时的便捷行为，不再暗含 `freeze` 自带 `recover`

## 测试重点

新的测试重点应围绕以下语义锁定：

- `freeze` 一出主带就进入 `Frozen`
- `Frozen` 回主带立刻恢复
- `flatten` 一出主带先进入 `FlattenPending`
- `FlattenPending` 回主带直接恢复，不会平到 `0`
- `FlattenPending` 继续向外穿过 `trigger_bps` 触发带，才进入 `Flattening`
- `Flattening` 的 `BackInBand / PriceConfirm` 恢复按 `boundary` 判定
- `FlattenPending` 对外仍投影成 `frozen`
- risk 触发仍直接 `terminate`

## 后续扩展

这次设计为以后预留了两个明确扩展点，但都不在第一版内：

1. `Flattening` 后基于运行数据决定是否自动升级为 `terminate`
2. `BandRecoverPolicy` 增加时间确认

只有在真实运行数据证明需要时，才继续往这两个方向扩展。当前版本先把语义和 ownership 定稳。
