# TUI 详情页重画设计

## 背景

当前 `poise-tui` 的详情页已经能展示基础状态、策略、执行、统计和活动，但存在几个明显问题：

- 状态不够醒目，`attention_required` 需要进入 `Execution` 区块后才看得完整
- `Strategy` 信息不完整，像 `min_rebalance_units` 这类直接影响行为判断的参数没有进入详情页
- 版块权重平均，主阅读路径不清楚，重要信息和背景资料混在一起
- `Commands` 单独占块，但实际日常操作较少，不值得压缩更重要的信息空间

这使得详情页更像原始字段堆叠，而不是一个能快速判断“现在发生什么、策略是什么、系统为什么这样做”的读侧界面。

## 目标

- 让状态和异常在进入详情页后第一眼可见
- 让详情页补齐关键策略参数，避免需要回到配置文件对照
- 让页面阅读顺序符合日常判断路径：先看状态，再看全貌，再看策略和执行细节
- 保持现有快捷键和命令语义，不把详情页做成重操作面板
- 保持实现适配 `ratatui`，不引入复杂交互模式

## 非目标

- 不修改 `pause`、`resume` 等命令语义
- 不新增详情页分页、Tab 或 modal
- 不重做 dashboard 列表布局
- 不在这次顺手调整策略或执行逻辑本身

## 方案比较

### 方案 A：决策优先页

做法：

- 顶部先给出最强状态摘要
- 中间用 `Overview` 和完整 `Strategy` 解释当前上下文
- `Execution` 负责解释系统当前动作和原因
- `Commands` 降级为提示，不再占据主区块

优点：

- 同时满足“信息更完整”和“状态更醒目”
- 阅读路径符合用户日常判断顺序
- 不需要引入新的交互模型

缺点：

- 需要重排详情页布局，并补 detail 读模型字段

结论：

- 采用

### 方案 B：配置优先页

做法：

- 把 `Strategy` 放到最前并扩成主区块
- 状态和执行退到后面

优点：

- 参数最完整，适合做配置核对

缺点：

- 日常使用时，最先需要看的状态和异常被埋后

结论：

- 不采用

### 方案 C：执行优先页

做法：

- 顶部大块突出执行状态、attention 和命令
- 策略和统计退后

优点：

- 异常最显眼

缺点：

- 会继续稀释策略信息完整性，不符合这次目标

结论：

- 不采用

## 最终设计

### 1. 页面骨架

详情页改成下面 6 个主区块，按顺序阅读：

1. `Status`
2. `Overview`
3. `Strategy`
4. `Execution`
5. `Statistics`
6. `Trace`

其中 `Commands` 不再单独保留为主区块，而是固定压缩成 `Status` 区块中的一行提示。

### 2. Status 区块

`Status` 作为详情页顶部摘要区，承担“第一眼判断”的职责。

展示规则：

- 有 `attention_required` 时，第一行直接显示高亮告警
- 文案至少包含：`execution status`、告警数量或原因摘要、`inventory_gap`、`gap_age`
- 无异常时，第一行显示正常摘要，例如 `active | open | gap 0.5000 | 1 active slot`
- 第二行展示上下文：`track id / symbol / venue / updated_at`
- 如果当前有可用命令，在区块底部补一行紧凑提示，例如 `commands: p pause | r resume`

这样用户进入详情页时，不需要向下扫描就能知道是否异常、当前差多少、能否立刻处理。

### 3. Overview 区块

`Overview` 用成组字段表达当前全貌，而不是单个散点行。

建议布局：

- 一行身份和生命周期：`id / symbol / venue / lifecycle`
- 一行价格上下文：`reference / mark / index`
- 一行仓位上下文：`current exposure / desired exposure / delta`

其中：

- `delta` 继续复用现有方向信号语法
- 正值、负值、零值使用现有颜色体系表达加仓、减仓和持平

### 4. Strategy 区块

`Strategy` 从当前的 4 个字段扩成完整策略摘要，并分成两组展示。

第一组：带宽与形状

- `lower_price`
- `upper_price`
- `shape_family`
- `out_of_band_policy`

第二组：容量与执行门槛

- `long_exposure_units`
- `short_exposure_units`
- `notional_per_unit`
- `min_rebalance_units`

这样用户可以直接在详情页里回答：

- 这条 track 的带宽是什么
- 多空容量是多少
- 每单位对应多少名义金额
- 多小的目标变化会触发下一次执行动作

### 5. Execution 区块

`Execution` 改成“摘要 + 原因 + 明细”的层次。

建议内容顺序：

- `state`
- `execution_status`
- `active_slot_count`
- `replacement_gate`
- `attention_reasons`
- `slots`

展示原则：

- attention 仍显示在这里，但不再是唯一异常入口
- `replacement_gate` 用一句可读文案表达，不直接暴露原始结构噪音
- `slots` 继续保留现有订单明细，但放在区块后半部

### 6. Statistics 区块

`Statistics` 保持现有统计口径，但排版更适合扫读。

至少展示：

- `total_pnl`
- `realized_pnl`
- `max_inventory_gap_abs`
- `max_gap_age_ms`
- `stats_started_at`

其中 `total_pnl` 和 `realized_pnl` 继续使用与列表一致的方向信号表达。

### 7. Trace 区块

底部不再把 `Activity` 和 `Diagnostics` 视为两个彼此竞争高度的主区块，而是统一定义成一个 `Trace` 区。

职责：

- `Activity` 仍是默认主内容，承担最近变化和线索追踪职责
- `Diagnostics` 继续受现有开关控制，但只在 `Trace` 区内部占用空间

约束：

- `Trace` 不承担主状态表达
- `Trace` 永远只占页面剩余空间，不反向挤压顶部核心区块
- 未开启 diagnostics 时，`Trace` 只渲染 `Activity`
- 开启 diagnostics 时，`Trace` 在内部再决定如何分配 `Activity` 和 `Diagnostics`
- 无内容时明确显示空态

### 8. 布局退化规则

为避免布局复杂度散落到渲染分支，详情页按可用高度分成 3 种固定模式：

#### 模式选择契约

- 布局模式只由详情页 body 区的可用高度决定
- 这里的 body 区是指顶部 `Status` 栏和底部 `Keys` 栏之外，传给 `instance` 视图的绘制区域
- 模式选择不依赖内容条数，不根据 `activity`、`diagnostics` 或 `slots` 的多少动态切换
- `diagnostics` 开关只影响 `Trace` 区内部如何分配空间，不参与布局模式选择
- 模式判断必须集中在单一布局策略函数中完成，由它产出当前模式和各区块约束；其余渲染逻辑只消费这个结果，不再各自写高度判断分支

这样可以保证：

- 新增字段时，只需要调整一个地方的布局阈值和对应测试
- `Trace` 内容增长不会反向改变顶部核心区块的布局模式
- 实现不会重新退化成散落的 magic number 判断

#### 标准模式

- 独立渲染 `Status`、`Overview`、`Strategy`、`Execution`、`Statistics`、`Trace`
- `Trace` 使用全部剩余空间
- 开启 diagnostics 时，只在 `Trace` 内部分割 `Activity` 和 `Diagnostics`

#### 紧凑模式

- 仍保留 6 个主区块
- `Statistics` 压缩成单行摘要
- `Strategy` 保持两组语义，但每组压缩成更紧凑的单行或双行表达
- `Trace` 继续使用剩余空间，并优先保留最近几条 `Activity`

#### 最小模式

- 只保证 `Status`、`Overview`、`Strategy`、`Execution` 四个核心区块独立可见
- `Statistics` 收叠进 `Overview` 的末行摘要，不再保留独立区块
- `Trace` 整体隐藏
- diagnostics 开关状态保留，但在最小模式下不额外占用布局空间

区块优先级固定为：

1. `Status`
2. `Overview`
3. `Strategy`
4. `Execution`
5. `Statistics`
6. `Trace`

任何高度不足的退化都必须按这个优先级发生，而不是继续追加新的固定高度分支。

## 协议与投影变更

当前 `TrackDetailView.strategy` 只包含：

- `lower_price`
- `upper_price`
- `shape_family`
- `out_of_band_policy`

这不足以支撑完整策略区块，因此需要扩展 detail 读模型。

### 1. `protocol`

扩展 `GridStrategyView`，新增：

- `long_exposure_units`
- `short_exposure_units`
- `notional_per_unit`
- `min_rebalance_units`

这次协议只扩展原始且稳定的策略事实，不新增只为展示服务的派生值。

### 2. `server`

详情投影在生成 `TrackDetailView` 时同步补齐上述字段，保证 TUI 只消费稳定读模型，不依赖 `core::TrackConfig` 内部细节。

## TUI 实现边界

这次实现限定在：

- `protocol`：扩展 detail 里的 `GridStrategyView`
- `server`：补齐 detail projector 和相关 HTTP / WebSocket 测试夹具
- `tui/src/views/instance.rs`：重排详情页布局和渲染逻辑
- `tui` fixture / 视图测试：锁住新的页面结构、布局退化规则和策略字段展示

不改：

- 命令处理流程
- dashboard 主列表布局
- diagnostics 独立开关行为

## 测试策略

先补验收测试，再做实现。

至少覆盖：

1. 协议 fixture 能反序列化扩展后的 `strategy` 字段
2. 服务端 detail 投影会带出完整策略字段
3. TUI 详情页会先显示顶部 `Status` 摘要
4. `attention_required` 时，顶部状态和执行区都会显示异常语义
5. `Strategy` 区块会显示 `min_rebalance_units`、容量和名义金额字段
6. 可用命令只作为紧凑提示显示，不再单独渲染成主区块
7. diagnostics 开启后只占用 `Trace` 区，不新增独立主区块
8. 小终端高度下会按 `标准 / 紧凑 / 最小` 规则退化
9. 布局模式只由 body 区高度决定，且模式判断集中在单一布局策略函数中

## 验收标准

- 进入详情页后，顶部第一屏能直接看到当前状态和异常
- `Strategy` 区块能完整看到关键策略参数，包含 `min_rebalance_units`
- 详情页阅读路径变成“状态 -> 全貌 -> 策略 -> 执行 -> 统计 -> Trace”
- `Commands` 不再单独占据主区块
- 小终端高度下，布局会按固定优先级退化，不新增散落的高度分支
- 布局模式只随 body 区高度变化，不随内容条数和 diagnostics 内容量抖动
- 新布局和新增字段都有测试覆盖
