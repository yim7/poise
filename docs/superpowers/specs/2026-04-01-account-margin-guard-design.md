# Account Margin Guard Design

**背景**

`-2019 Margin is insufficient` 现在暴露的是两个问题：

1. 启动时没有校验账号能力和配置上限是否匹配。
2. 运行时遇到交易所明确拒绝后，系统还会继续规划新的风险增加单，导致持续刷错。

当前风控只看 `max_notional`、日内盈亏和止损，不知道账号可用保证金，也没有账号级熔断语义。

## 目标

- 启动时提前发现“账号余额/杠杆不足以支撑配置最大仓位”。
- 运行时遇到 `-2019` 后，停止继续发会增加风险的单。
- `reduce_only`、撤单、平仓继续允许。
- 保持现有 engine/server 边界，不引入自动缩单或隐式改策略行为。

## 非目标

- 不做自动缩单重试。
- 不做多 track 之间的精确保证金分配器。
- 不把交易所余额细节直接泄漏进 executor planning。

## 设计结论

### 1. 新增账号级容量快照

在 `ExchangePort` 增加读取账号容量的能力，返回一个面向风控的精简结构，而不是直接暴露 Binance 原始响应。

建议结构：

```rust
pub struct AccountCapacitySnapshot {
    pub venue: Venue,
    pub available_balance: f64,
    pub total_wallet_balance: f64,
    pub max_increase_notional: f64,
    pub observed_at: DateTime<Utc>,
}
```

这里的 `max_increase_notional` 是“当前账号还能新增多少名义仓位”的统一表达。第一版允许 Binance 适配层内部用 `availableBalance * leverage` 或等价规则计算，只要对上层暴露的是稳定语义即可。

这是第一版的近似容量表达，不保证在双向持仓、组合保证金或维持保证金率快速变化时与交易所真实可开仓上限完全一致。后续如果容量公式升级，应该保持这个字段语义不变，而不是把 Binance 原始字段继续向上泄漏。

### 2. 启动预检只做硬失败校验

服务启动时，按账号读取一次 `AccountCapacitySnapshot`，并校验每个 track 的配置上限是否超过账号当前能力：

- `track_required_notional = budget.max_notional`
- 若 `track_required_notional > max_increase_notional`，启动失败或进入 `attention_required`

这层校验只回答一个问题：当前账号是否连配置上限都明显扛不住。

它不能代替运行时检查，因为余额、杠杆、已有仓位、其他挂单都会变化。

在多 track 共享同一保证金账户时，这个预检只是必要条件，不是充分条件。启动成功只表示“当前快照下没有明显超配”，不表示运行全程都一定有足够容量。

### 3. 运行时引入账号级“风险增加熔断”

在 server runtime 维护账号级 guard 状态，而不是每个 track 各自决定：

```rust
pub struct AccountMarginGuard {
    pub snapshot: Option<AccountCapacitySnapshot>,
    pub increase_blocked: bool,
    pub blocked_reason: Option<String>,
    pub blocked_at: Option<DateTime<Utc>>,
}
```

语义：

- 默认不阻断。
- 交易所返回 `-2019` 时，把对应账号标记为 `increase_blocked = true`。
- guard 激活后，所有会增加风险的 submit 都必须被拦下。
- `reduce_only` submit、撤单、已有仓位的减仓不受影响。

这个 guard 是账号级状态。`ExchangePort::get_account_capacity_snapshot(&Instrument)` 中的 `instrument` 参数只用于解析合约维度的容量信息或交易所规则，不表示每个 instrument 拥有独立保证金池。

这个结构是 server 侧的权威状态。第一版不要求把完整 `AccountMarginGuard` 直接持久化进每个 track snapshot。

engine 侧只应该看到一个裁剪后的只读约束视图，例如：

```rust
pub struct AccountCapacityConstraint {
    pub increase_blocked: bool,
    pub blocked_reason: Option<String>,
    pub max_increase_notional: Option<f64>,
}
```

它只表达“当前是否允许继续增加风险”，不负责保存账号级快照、阻断时间或恢复过程细节。

### 4. `-2019` 后的措施

`effect_worker` 在提交订单时如果收到 Binance `code=-2019`：

- 记录结构化原因 `insufficient_margin`
- 立刻触发账号容量重同步
- 把账号级 guard 置为 `increase_blocked`
- 当前 effect 标记为失败，保留清晰错误信息

第一版实现不应把“保证金不足”硬编码成仅有 `-2019` 一个错误码。更稳妥的做法是在 adapter 层把 Binance 的相关拒单码统一映射为内部原因 `insufficient_margin`，`effect_worker` 只依赖这个内部原因。

后续在 guard 解除前：

- 不再向交易所发送会增加风险的订单
- planner/reconcile 应该产出 `RiskDenied`
- UI/health 暴露 `attention_required`

### 5. 拦截点放在 reconcile，而不是只放在 effect worker

只在 `effect_worker` 下单前拦截不够，因为 planner 仍会持续生成新的 submit effect。

正确位置有两层：

- 第一层：`effect_worker` 处理真实交易所错误并触发 guard
- 第二层：`reconciler`/`manager` 在 guard 激活时直接 `RiskDenied`，不再生成新的风险增加单

这样才能同时解决：

- 不再继续打交易所接口
- 不再持续新增失败 effect

### 6. 风控判定规则

第一版只处理“是否允许增加风险”，不做精细分配。

规则：

- 若订单 `reduce_only=true`，直接允许
- 若账号 guard 激活，禁止任何增加风险的目标
- 若 guard 未激活但有新鲜 `AccountCapacitySnapshot`，当
  `required_increase_notional > max_increase_notional`
  时，返回 `RiskDenied { reason: "insufficient account margin" }`

其中：

- `required_increase_notional` 取本次目标相对当前仓位新增的名义金额
- 减少绝对仓位、平仓、反向中的减仓部分不应被拦

第一版金额继续沿用现有 `f64` 表示，保持与仓库现状一致；如果以后要做严格对账，再单独引入定点金额类型。

### 7. 恢复条件

guard 不能靠时间自动失效，必须靠新的账号快照解除。

解除条件：

- 成功拉到新的 `AccountCapacitySnapshot`
- 快照证明账号已有正的可用新增容量

恢复后再允许新的风险增加单。

恢复逻辑由 server 侧完整 guard 负责；engine 只消费恢复后的最新约束投影，不承担账号状态机职责。

## 边界与职责

### engine

- 定义面向风控的账号容量输入
- 在 reconcile 阶段把账号容量不足转成 `RiskDenied`
- 不直接依赖 Binance 错误码

### server

- 维护账号级 guard
- 在启动时执行预检
- 在 `-2019` 时更新 guard，并触发容量重同步
- 负责把完整 guard 投影成 engine 可消费的最小约束视图

### exchange adapter

- 负责从 Binance REST 读取账号信息
- 负责把交易所原始字段折叠成 `AccountCapacitySnapshot`

## 测试策略

需要覆盖四类验收：

1. 启动时账号容量不足，服务拒绝启动或进入 `attention_required`
2. guard 激活时，reconcile 不再产出风险增加的 `SubmitOrder`
3. guard 激活时，`reduce_only` 提交仍然允许
4. `-2019` 后触发 guard，再次 reconcile 变成 `RiskDenied` 而不是继续下单

另外补一条边界测试更稳：

5. server 持有完整 guard，但传给 engine/snapshot 的只有最小约束视图，不直接暴露账号级恢复细节

## 为什么不做自动缩单

自动缩单会把“账号容量不足”悄悄转换成“策略目标被改写”，带来两个问题：

- 策略行为不再稳定，难以解释
- 多个 track 共享账号时会出现容量抖动和互相抢占

第一版应该先做到“明确拒绝、明确告警、明确恢复”，而不是自动猜一个更小的单量。
