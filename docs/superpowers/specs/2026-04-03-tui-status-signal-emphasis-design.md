# TUI 状态与方向信号强化设计

## 背景

当前 `poise-tui` 已经能展示实例生命周期、执行状态、仓位和 PnL，但视觉层级还不够清楚：

- 顶部 `Status` 栏只显示固定文案 `Poise`，没有承载实际状态
- 底部 `Keys` 栏混放了快捷键和运行消息，职责不清
- 列表页仓位只显示当前值，看不出是在加仓还是减仓
- 列表页没有 PnL 列，无法快速扫出盈亏方向
- 详情页虽然有执行状态和 PnL 数字，但异常状态、仓位变化和盈亏方向都不够醒目

这让 TUI 更像“原始数据面板”，而不是可快速判断风险和方向的操作界面。

## 目标

- 让顶部 `Status` 栏真正承载全局状态
- 让列表页一眼看出执行异常、加仓/减仓和盈亏方向
- 让详情页用和列表一致的视觉语法表达执行异常、仓位方向和 PnL
- 保持 TUI 风格统一，不引入和现有布局冲突的新交互

## 非目标

- 不修改命令语义和键位映射
- 不修改策略、执行或风险计算逻辑
- 不引入新的面板切换模式
- 不为了这次优化重做整个 TUI 布局系统

## 关键约束

- 这是 TUI，不是图形界面，视觉强化必须适配 `ratatui`
- 用户偏好更强表现力，因此可以使用背景色、高亮块和 Unicode 方向箭头
- 列表页新增 PnL 展示需要协议和服务端投影一起补齐

## 方案比较

### 方案 A：异常块 + 方向箭头

做法：

- 执行异常使用独立高亮块或高亮徽标
- 仓位变化和 PnL 都统一成“箭头 + 数值 + 颜色”
- 顶部状态栏显示全局运行态和选中实例异常摘要

优点：

- 异常、方向、盈亏三类信息的视觉语法统一
- 在 TUI 中可读性和表现力平衡较好
- 可以局部强调，不会把整行全部染色

缺点：

- 需要补一些样式辅助函数和单元格渲染逻辑

结论：

- 采用

### 方案 B：全面胶囊标签化

做法：

- 执行状态、仓位、PnL 都做成短标签块

优点：

- 风格统一

缺点：

- TUI 横向空间紧，列表列宽容易被压缩

结论：

- 不采用

### 方案 C：整行背景染色

做法：

- 异常状态带动整行背景变化，仓位和 PnL 只做文字强化

优点：

- 异常最显眼

缺点：

- 容易和选中态冲突
- 列表页可读性会下降

结论：

- 不采用

## 最终设计

### 1. 顶部状态栏职责调整

当前顶部 `Status` 栏改为真正的全局状态栏，底部 `Keys` 栏只保留快捷键帮助。

顶部状态栏按优先级显示：

1. 最近系统状态消息，例如 `websocket connected`、`startup failed: ...`
2. 当前选中实例的执行异常摘要，例如 `! execution anomaly on btc-core`
3. 当前视图和选中实例标识，例如 `dashboard | btc-core | BTCUSDT`

如果存在状态消息和异常摘要，两者用分隔符拼接，保证顶部栏总是承载实时状态。

### 2. 列表页统一信号语法

列表页列结构调整为：

- `ID`
- `Symbol`
- `Lifecycle`
- `Execution`
- `Exposure`
- `PnL`

`Last Price` 从列表移除，保留到详情页展示。理由是列表优先承担风险和方向扫描。

字段展示规则：

- `Execution`
  - 正常：显示 `open` / `paused` / `closed`
  - 异常：显示紧凑警示文案 `! ATTN open`，并用暖色背景块强化
- `Exposure`
  - 基于 `target - current` 计算方向
  - 加仓：冷色 `↑ +0.2500`
  - 减仓：暖色 `↓ -0.2500`
  - 持平：灰色 `→ 0.0000`
- `PnL`
  - 列表显示总 PnL
  - 正值：绿色 `↑ +1245.30`
  - 负值：红色 `↓ -245.30`
  - 零值：灰色 `→ 0.00`

### 3. 详情页统一信号语法

详情页保持现有版块结构，但在以下位置加强视觉：

- `Overview`
  - `reference/exposure` 改成带方向语法的仓位摘要
- `Statistics`
  - `Total PnL` 和 `Realized PnL` 都改成方向箭头格式
- `Execution`
  - 当 `execution_status = attention_required` 时，在区块最前面显示高亮异常块
  - 文案统一为 `! ATTENTION REQUIRED`
  - 第二行显示 `alerts: ...`
  - 正常状态不显示异常块

列表和详情复用同一套箭头和颜色语法，避免两处表达不一致。

### 4. 协议边界

为支持列表页新增 `PnL` 列，`TrackListItemView` 新增简要统计字段：

```rust
pub struct TrackListItemView {
    pub id: String,
    pub instrument: InstrumentView,
    pub lifecycle: GridLifecycleView,
    pub reference_price: Option<f64>,
    pub exposure: ExposureSummaryView,
    pub execution: ExecutionBadgeView,
    pub statistics: GridStatisticsView,
}
```

这里直接复用 `GridStatisticsView`，原因是列表当前只需要 `total_pnl`，但复用现有统计结构可以避免再造一个只含一两个字段的浅包装类型。

服务端 projector 在投影列表项时同步填充统计字段，TUI 列表直接消费。

## 模块职责

### `protocol`

- 定义列表所需的新增统计字段
- 保持详情结构不变

### `server/projector`

- 把 `TrackReadModel` 的累计已实现盈亏和未实现盈亏投影到列表项统计字段

### `tui/theme`

- 提供执行异常、仓位方向、PnL 方向、顶部状态栏等样式辅助

### `tui/views`

- `mod.rs` 负责把顶部状态栏和底部快捷键职责分开
- `dashboard.rs` 负责列表页的方向摘要和异常徽标
- `instance.rs` 负责详情页异常块、方向仓位和 PnL 表达

## 风险与取舍

- 列表去掉 `Last Price` 会减少一个行情数字，但换来更关键的风险和方向信息
- Unicode 箭头依赖终端字体；如果个别终端回退不佳，仍会比纯文本更直观
- 列表复用 `GridStatisticsView` 会携带比当前所需更多字段，但比新增一个只含 `total_pnl` 的类型更简单

## 验收标准

- 顶部 `Status` 栏不再只显示固定文案，而是显示实际全局状态
- 底部 `Keys` 栏只显示快捷键帮助
- 列表页可以看出执行异常、加仓/减仓方向和总 PnL 方向
- 详情页异常状态、仓位方向和 PnL 方向使用与列表一致的视觉语法
- 相关协议、服务端投影和 TUI 测试都覆盖新行为
