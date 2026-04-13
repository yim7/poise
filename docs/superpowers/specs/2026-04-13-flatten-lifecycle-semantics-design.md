# 策略层 flatten 生命周期语义设计

## 背景

当前代码里仍然混用了两套不同阶段的语义：

- 早期策略层设计使用 `out_of_band_policy = reduce_only`
- 后来手动命令增加了 `Flatten`
- 但手动 `Flatten` 和自动带外平仓都继续复用 `TrackStatus::ReducingOnly`

这和当前已经确认的产品语义不一致：

- 带外自动平仓应该叫 `flatten`
- 自动带外平仓在价格回到带内后，应该恢复 `Active`
- 手动 `Flatten` 不是自动恢复语义，它应该保持人工目标覆盖，直到 `Resume`

同时还要保留一个明确边界：

- 交易所订单层的 `OrderRequest.reduce_only` 仍然保留
- 它是订单属性，不是策略生命周期语义

本设计处理同一组带外生命周期语义：

- `freeze`
- `hold`
- `flatten`
- `terminate`

以及它们在运行时状态、协议投影和界面语义中的一致表达。  
本设计不改交易所订单层的 `reduce_only` 字段。

## 目标

- 把策略层 `reduce_only` 正式替换为 `flatten`
- 把自动带外平仓状态和手动平仓状态拆开
- 让回带内后的自动恢复规则只作用于自动带外平仓
- 保持手动 `Flatten` 的持久化目标覆盖语义
- 删除旧策略语义的双命名，不保留 `reduce_only` 兼容 alias

## 非目标

- 不改交易所订单 `reduce_only=true` 的执行语义
- 不新增市价平仓链路
- 不做旧快照、旧协议、旧配置的双写兼容

## 最终设计

### 语义分层

系统里与“减仓 / 平仓”相关的概念拆成 3 层：

1. 策略层带外策略
2. 运行时生命周期状态
3. 交易所订单属性

这 3 层不能再共用同一个名字。

### 策略层带外策略

`OutOfBandPolicy` 的稳定值改为：

- `Freeze`
- `Hold`
- `Flatten`
- `Terminate`

语义：

- `Freeze`
  - 离开区间后冻结当前目标
  - 不主动去风险
  - 回带内后自动恢复
- `Hold`
  - 离开区间后冻结当前目标
  - 不主动去风险
  - 不自动恢复，需要人工动作
- `Flatten`
  - 离开区间后把目标设为 `0`
  - 回带内后自动恢复
- `Terminate`
  - 离开区间后把目标设为 `0`
  - 进入终态，不再恢复

这里的 `Flatten` 只定义目标仓位语义，不定义执行层是否用限价单还是市价单。

### 生命周期状态

`TrackStatus` 的稳定值改为：

- `WaitingMarketData`
- `Active`
- `Frozen`
- `Holding`
- `Flattening`
- `ManualFlattening`
- `Terminated`
- `Paused`

语义：

- `Flattening`
  - 来源只能是带外 `out_of_band_policy = flatten`
  - 目标仓位为 `0`
  - 回带内后自动恢复 `Active`
- `ManualFlattening`
  - 来源只能是手动 `Flatten`
  - 必须伴随 `manual_target_override = Some(Exposure(0.0))`
  - 不会因为价格回带而自动恢复
  - 只能由 `Resume` 清除

### 状态迁移规则

#### `freeze` 与 `hold`

带外冻结类策略保持两个不同恢复规则：

- `Freeze`
  - 带外时保留冻结目标
  - 回带内后自动恢复 `Active`
- `Hold`
  - 带外时保留冻结目标
  - 回带内后继续保持 `Holding`
  - 只能由人工 `Resume` 恢复

这次要求把当前 `Holding` 误自动恢复的问题一并修正，避免同一组带外策略继续存在半新半旧语义。

#### 自动带外 `flatten`

当 `band_status(strategy_price, config)` 返回带外，且 `out_of_band_policy = Flatten`：

- `desired_exposure = Exposure(0.0)`
- `new_status = Some(TrackStatus::Flattening)`

当价格重新回到带内：

- `TrackStatus::Flattening -> TrackStatus::Active`
- 然后按带内曲线重新计算 `desired_exposure`

#### 手动 `Flatten`

手动命令 `Flatten` 的语义固定为：

- `manual_target_override = Some(Exposure(0.0))`
- `status = TrackStatus::ManualFlattening`
- 后续每次 reconcile 优先使用 override target

即使价格已经回到带内：

- 仍继续保持目标 `0`
- 不自动恢复 `Active`

#### `Resume`

`Resume` 在“手动平仓生效中”时的语义：

- 清除 `manual_target_override`
- 不再保留 `ManualFlattening`
- 立即按当前 `strategy_price` 走正常 reconcile

结果规则：

- 若存在 live `strategy_price`，由正常 reconcile 决定恢复后的 `desired_exposure` 和状态
- 若当前没有 live `strategy_price`，恢复到 `WaitingMarketData`
- `Resume` 不会直接把带外 `Flattening` 当作手动状态处理
- `Resume` 也负责从 `Holding` 恢复正常策略控制

#### `Terminate`

手动 `Terminate` 和自动带外 `Terminate` 共用同一个终态：

- `status = Terminated`
- `desired_exposure = Exposure(0.0)`
- 清除 `manual_target_override`
- 后续不再恢复

### `manual_target_override` 的关系

这次不新增新的持久化控制位，继续使用现有：

```rust
manual_target_override: Option<Exposure>
```

但增加一条一致性要求：

- `TrackStatus::ManualFlattening` 必须与 `manual_target_override = Some(Exposure(0.0))` 配对出现
- `TrackStatus::Flattening` 不依赖 `manual_target_override`

也就是说：

- `manual_target_override` 继续表达“人工目标覆盖”
- `ManualFlattening` 负责表达“当前正处于人工平仓生命周期”

### 对外协议与投影

对外稳定语义同步改为：

- `TrackStatus::Flattening -> "flattening"`
- `TrackStatus::ManualFlattening -> "manual_flattening"`
- `OutOfBandPolicy::Flatten -> "flatten"`

以下旧策略语义不再出现在稳定协议中：

- `reducing_only`
- `reduce_only`（仅指策略层/生命周期层）

`projector` 里的命令可用性也同步改成：

- `Resume` 在 `Paused`、`Holding` 或 `ManualFlattening` 时可用
- `Flatten` 在非 `Terminated` 时可用
- 自动带外 `Flattening` 不额外开放 `Resume`

这里增加一条边界：

- `Resume` 的对外可用性只依赖 `TrackStatus`
- `manual_target_override` 只用于 engine 内部控制语义和一致性检查
- `projector` 和 TUI 不再直接根据 `manual_target_override` 推导命令可用性

### TUI 与展示语义

列表页和详情页都使用新的生命周期语义：

- 自动带外状态显示 `flattening`
- 手动命令状态显示 `manual_flattening`

如果列表页需要为了列宽做短标签，那只是视图层展示缩写，不改变协议值和内部状态名。

### 风控与订单层边界

以下内容保持不变：

- `OrderRequest.reduce_only`
- executor 内部“减仓单 -> reduce_only=true”的映射
- 账户保证金保护里“允许减风险路径继续执行”的判断

这些地方的 `reduce_only` 指的是订单提交属性，不是生命周期状态，不参与这次重命名。

### 兼容策略

本项目当前处于探索阶段，这次不保留过渡兼容：

- 不接受 `out_of_band_policy = "reduce_only"` 作为新配置
- 不保留 `TrackStatus::ReducingOnly`
- 不做协议双值兼容
- 不为旧快照增加别名反序列化

如果本地还有旧数据库、旧快照或旧配置：

- 直接删除或重建
- 不把兼容逻辑写进运行时主路径

## 影响范围

- `core`
  - `OutOfBandPolicy`
- `engine`
  - `TrackStatus`
  - `reconciler`
  - `manager`
  - `snapshot / persisted_runtime`
- `application`
  - read model 传递的新状态值
- `protocol`
  - 对外枚举值和字符串
- `server`
  - projector 的状态映射和命令可用性
- `tui`
  - dashboard / instance 的状态文案
- 用户文档
  - `README.md`
  - `docs/protocol-contract.md`

## 验收标准

1. 配置层只接受 `out_of_band_policy = "flatten"`，不再接受 `"reduce_only"`
2. 带外 `flatten` 时，目标为 `0`，状态为 `Flattening`
3. `Flattening` 在回带内后自动恢复 `Active`
4. 手动 `Flatten` 后，状态为 `ManualFlattening`，且 `manual_target_override = 0`
5. `ManualFlattening` 在回带内后仍保持目标 `0`
6. `Holding` 在回带内后不自动恢复，且只能由 `Resume` 恢复
7. `Resume` 可以从 `ManualFlattening` 或 `Holding` 恢复正常策略控制
8. `Terminate` 继续只使用一个 `Terminated` 终态
9. 对外协议和 TUI 不再出现策略层 `reducing_only / reduce_only`
10. 交易所订单层 `OrderRequest.reduce_only` 及相关测试保持不变
11. `Resume` 的对外可用性只依赖 `TrackStatus`，不依赖 `manual_target_override`
