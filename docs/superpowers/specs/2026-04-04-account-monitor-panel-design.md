# 账户监控面板设计

## 背景

当前 `Poise` 的 TUI 完全围绕 `track` 展开：

- 列表页只展示轨道生命周期、执行状态、仓位和 PnL，见 [`tui/src/views/dashboard.rs`](../../../tui/src/views/dashboard.rs)
- 详情页也只展示单个轨道的状态、策略、执行、统计和活动，见 [`tui/src/views/instance.rs`](../../../tui/src/views/instance.rs)
- 协议层只有 `track` 列表、详情、命令和事件，没有账户级读模型，见 [`protocol/src/lib.rs`](../../../protocol/src/lib.rs)
- 服务端持久化只有 `track_snapshots`、`track_events` 和 `track_effects`，没有账户级状态表，见 [`storage/src/schema.rs`](../../../storage/src/schema.rs)

用户最初考虑的是“净值曲线”。进一步讨论后，约束变得明确：

- 账户总资产曲线在 Binance App 里已经很方便看
- 如果要在 Poise 内复刻“1 年、按天、剔除外部资金流、纳入未实现盈亏”的交易净值曲线，需要补一整条历史重建链路，首版成本偏高
- 当前更有价值的是把账户风险放进 Poise 的值守语境，让账户状态和轨道状态出现在同一屏

因此，这次不做历史净值曲线，改做一个轻量的账户监控面板。

## 目标

- 在 `Dashboard` 首页展示账户级摘要
- 让值守时能同时看到“账户整体风险”和“各轨道状态”
- 首版提供账户总资产、可用余额、未实现盈亏、当日变化和风险信号
- 风险信号支持默认阈值，也支持配置覆盖
- 保持现有 TUI 交互模型，不新增页面和快捷键

## 非目标

- 不实现历史净值曲线
- 不实现账户级分页、独立详情页或新视图模式
- 不把账户状态拆分到每个 `track`
- 不在这次引入长历史表、历史导入任务或 Binance 异步下载接口
- 不修改现有策略、执行和风控逻辑

## 关键约束

- 账户监控是账户级能力，配置和数据边界不能挂到单个 `track`
- 首版数据应尽量来自 Binance 当前账户接口，不依赖复杂历史重建
- `day_change` 需要跨服务重启保持一致，因此至少要持久化“当日基准值”和“最近一次成功账户摘要”
- 配置必须有默认值；整段配置缺失时，系统仍可启动并启用默认阈值
- 阈值如果非法或顺序颠倒，启动阶段直接报错，不做静默修正

## 方案比较

### 方案 A：历史净值曲线

做法：

- 新增账户级时间序列
- 接 Binance 历史接口或下载接口
- 在 TUI 里渲染账户曲线

优点：

- 视觉上完整
- 能回答“这一段时间账户怎么走”

缺点：

- 与 Binance App 功能重合
- 为了得到有意义的交易净值，需要处理外部资金流和历史重建
- 首版实现成本远高于当前值守需求

结论：

- 不采用

### 方案 B：账户摘要数字卡

做法：

- 在 `Dashboard` 顶部新增账户摘要区块
- 展示账户关键数值，不做风险等级

优点：

- 成本低
- 能快速把账户信息带进 TUI

缺点：

- 只显示数字，不足以支持风险判断
- 用户仍需要自己解释“这个数字是不是异常”

结论：

- 不采用

### 方案 C：账户摘要数字卡 + 风险信号

做法：

- 在 `Dashboard` 顶部新增独立 `Account` 区块
- 展示摘要数字
- 同时计算并展示账户级风险信号和原因摘要

优点：

- 直接服务值守场景
- 不重复做 Binance App 已经擅长的长历史展示
- 能和 `track` 状态形成同一屏的风险上下文

缺点：

- 需要补账户级协议、服务和持久化边界
- 需要定义一套账户信号规则和配置验证

结论：

- 采用

## 最终设计

### 1. 首页新增 `Account` 区块

`Account` 区块放在 `Dashboard` 顶部，轨道表格之前。

理由：

- 账户状态是全局上下文，应该先于单个轨道展示
- 如果放到单个实例详情页，账户级信号会错误地依附某个 `track`
- 首页展示更符合值守路径：先判断账户是否异常，再看哪条轨道有问题

首版不新增页面，不新增快捷键，不引入聚焦切换。用户进入 `Dashboard` 就能看到账户区块。

### 2. 区块内容

区块分两行展示：

- 第一行：`equity / available / unrealized pnl / day change`
- 第二行：`risk signal / reason / day base at / updated at`

字段定义：

- `equity`
  - 当前账户总资产
- `available`
  - 当前可用余额
- `unrealized pnl`
  - 当前未实现盈亏
- `day change`
  - 当前 `equity` 相对“当日基准值”的变化
- `risk signal`
  - 账户风险等级，取 `normal / attention / critical`
- `reason`
  - 当前命中的异常原因摘要
- `day base at`
  - 当前自然日基准值建立时间
- `updated at`
  - 最近一次成功刷新账户摘要的时间

当账户数据暂时不可用时：

- 区块仍保留
- 数值显示最近一次成功值或 `-`
- `updated at` 显示最后成功刷新时间
- 不因为单次拉取失败直接把账户标记为风险异常

### 3. 数据口径

首版账户摘要直接采用 Binance USDⓈ-M Futures `GET /fapi/v3/account` 的账户汇总口径，不自行重算账户总资产。

字段映射固定为：

- `equity`
  - 映射 Binance `totalMarginBalance`
- `available`
  - 映射 Binance `availableBalance`
- `unrealized pnl`
  - 映射 Binance `totalUnrealizedProfit`

原因：

- 三个字段都来自同一个账户摘要响应
- `totalMarginBalance` 已经包含钱包余额和未实现盈亏，避免本地再拼装出第二套口径
- 首版不需要按持仓逐个汇总未实现盈亏

补充说明：

- 单资产模式下，以上字段沿用 Binance 的单资产口径
- 多资产模式下，以上字段沿用 Binance 返回的 USD 汇总口径
- Poise 只展示交易所返回值，不在首版做额外折算

### 4. 风险信号规则

首版只做三类信号：

- `day_change`
- `available`
- `unrealized_pnl`

#### `day_change`

定义：

- `current_equity` 相对当日基准值的变化比例

当日基准值规则：

- `AccountMonitor` 在每个自然日内记录一次基准快照
- 基准快照取该自然日第一次成功拉到的账户摘要
- 同一天内后续刷新复用该基准
- 跨自然日后重新建立新基准
- 服务重启后从本地恢复当前自然日的基准值和基准建立时间

说明：

- 首版不引入历史回填，因此无法稳定得到“午夜整点基准”
- 这里保留“当天第一次成功快照”口径，但把这套时序复杂度封装在 `AccountMonitor` 内部
- 对外显式暴露 `day_base_at`，避免调用方把 `day_change` 误解成交易所官方日收益

自然日时区固定为：

- `Asia/Shanghai`

理由：

- 用户当前值守环境就在该时区
- 固定时区比“跟随服务器本地时区”更稳定，可避免部署机器变化导致日切换漂移
- 首版先固定一个明确口径，不再为此引入额外时区配置

默认阈值：

- `<= -3.0%` 为 `attention`
- `<= -5.0%` 为 `critical`

#### `available`

定义：

- `available / equity`

默认阈值：

- `<= 30.0%` 为 `attention`
- `<= 15.0%` 为 `critical`

#### `unrealized_pnl`

定义：

- `unrealized_pnl / equity`

默认阈值：

- `<= -5.0%` 为 `attention`
- `<= -10.0%` 为 `critical`

说明：

- 首版只把大额浮亏视为风险
- 大额浮盈不触发风险异常

#### 极端值退化规则

当 `equity <= 0` 时：

- `available / equity` 和 `unrealized_pnl / equity` 不再计算比例
- `day_change` 显示为 `-`
- 账户 `risk_signal` 直接置为 `critical`
- `reason` 至少包含 `equity <= 0`

理由：

- 这是极端账户状态，继续按比例运算没有稳定意义
- 直接标记为 `critical` 比静默显示空值更符合风险语义

#### 聚合规则

- 三条规则分别独立计算等级
- 账户最终 `risk_signal` 取最高等级
- `reason` 只展示命中的异常项，使用紧凑摘要
- 例如：`day_change -4.2%, available 12.8%`

### 5. 配置模型

新增服务端顶层配置段：

```toml
[account_monitor]
day_change_attention_pct = -3.0
day_change_critical_pct = -5.0
available_ratio_attention_pct = 30.0
available_ratio_critical_pct = 15.0
unrealized_loss_attention_pct = -5.0
unrealized_loss_critical_pct = -10.0
```

默认值策略：

- 整个 `[account_monitor]` 缺失：全部回落默认值
- 只配置部分字段：其余字段回落默认值

启动校验：

- 首版 3 个指标的危险方向都朝“更小的数值”发展
- 因此所有指标都要求 `attention` 阈值大于或等于 `critical` 阈值
- 例如：
  - `day_change_attention_pct = -3.0`，`day_change_critical_pct = -5.0`
  - `available_ratio_attention_pct = 30.0`，`available_ratio_critical_pct = 15.0`
- 所有百分比阈值必须是有限数值
- 校验失败直接拒绝启动

配置边界：

- `account_monitor` 属于服务端全局配置
- 不能挂在 `[[tracks]]` 下

### 6. 服务端与协议边界

新增账户级深模块 `AccountMonitor`。

`AccountMonitor` 独占以下知识：

- Binance 账户摘要拉取
- 当日基准值建立与恢复
- 风险信号计算
- 新旧摘要 diff
- UI 通知触发

除 `AccountMonitor` 之外，其他层不得自行重算 `day_change`、`risk_signal` 或基准值。

`AccountMonitor` 对外只暴露中性的当前摘要读取接口，例如：

- `current_summary()`

`AccountMonitor` 不暴露任何带 `dashboard` 语义的接口，也不直接向 UI 层暴露订阅接口。

新增账户级内部读模型 `AccountReadModel`，负责承载：

- 当前摘要数值
- 当日基准值
- `day_base_at`
- `day_change`
- 聚合后的 `risk_signal`
- `reason`
- `updated_at`

`AccountReadModel` 是纯派生读模型：

- 只在 `AccountMonitor` 内按当前规则即时计算
- 不作为持久化结构直接落库
- 不承担跨版本兼容责任

新增 `AccountProjector`，负责把 `AccountReadModel` 投影成协议层 `AccountSummaryView`。

账户读路径固定为：

- `AccountMonitor.current_summary() -> AccountReadModel`
- `AccountProjector.project_summary(...) -> AccountSummaryView`

这里不新增第二套 `AccountQueryService`。原因是：

- 现有 `TrackQueryService` 的职责是从持久化快照、事件和 effects 组装 track 读侧
- 账户侧的当前状态、基准值和风险规则本来就由 `AccountMonitor` 独占
- 再包一层独立 query service 只会形成浅层转发；真正需要复用的是 `AccountProjector`

边界划分：

- Binance 适配层负责读取账户当前摘要
- server 负责建立账户级内部读模型、计算风险等级和维护当日基准
- server 边界层通过 `AccountProjector` 输出协议 DTO
- protocol 负责定义账户摘要 DTO 和事件
- TUI 负责在 `Dashboard` 顶部渲染 `Account` 区块

对外协议首版固定为：

- `GET /account`
- 保留现有 `GET /tracks`
- 保留现有 `GET /tracks/:id`
- 复用现有 `GET /ws`
- WebSocket 事件 `account_summary_changed`

原因：

- 账户摘要本身是独立于 `track` 的账户级资源
- 继续沿用当前服务端以资源为单位组织 HTTP 接口的风格
- 不重复引入一个同时承载账户和 track 列表的重叠读侧
- 不额外引入第二条 WebSocket 连接

WebSocket 承载方式固定为：

- `/ws` 继续作为唯一实时订阅入口
- 现有只承载 `track` 事件的消息结构升级为统一事件外壳 `StreamEvent`
- `StreamEvent` 不再带顶层 `track_id`
- 统一事件外壳固定为：

```rust
pub enum StreamEvent {
    TrackListItemChanged {
        track_id: String,
        item: TrackListItemView,
    },
    TrackDetailChanged {
        track_id: String,
        detail: Box<TrackDetailView>,
    },
    AccountSummaryChanged {
        summary: AccountSummaryView,
    },
}
```

- `track_id` 只出现在 track 相关事件体里
- `account_summary_changed` 不携带空的或伪造的 `track_id`

这次是探索阶段，允许直接演进协议，不保留旧的只面向 `track` 的 WebSocket 消息外壳。

Exchange port 边界固定为：

- 新增账户级读取方法，例如 `get_account_summary()`
- 保留现有 `get_account_capacity_snapshot(symbol)`，继续服务逐 symbol 的容量与风控校验
- 两者职责不同，前者服务账户级读侧，后者服务交易执行前的风险边界

Server 通知边界固定为：

- 现有 `TrackInternalNotification` 升级为统一服务端通知抽象 `ServerNotification`
- `TrackWriteService` 和 `AccountMonitor` 都只向这条统一服务端通知流发布变化
- `websocket` 只消费这一条统一通知流，不直接知道账户轮询细节
- 账户变化不再拥有第二套独立订阅机制

统一服务端通知抽象固定为：

```rust
pub enum ServerNotification {
    TrackChanged {
        track_id: TrackId,
    },
    AccountChanged,
}
```

说明：

- 服务端内部通知只表达“哪个读侧对象需要重投影”
- 它不直接携带 UI DTO
- `websocket` 收到 `TrackChanged` 后重投影对应 track 事件
- `websocket` 收到 `AccountChanged` 后重投影账户摘要事件

### 7. 刷新与交互

账户摘要刷新采用“启动立即拉取 + 固定间隔轮询”的策略。

职责边界固定为：

- `runtime` 负责账户轮询任务的生命周期、定时触发和 shutdown
- `runtime` 只调用 `AccountMonitor.refresh_once()`
- `AccountMonitor.refresh_once()` 独占账户拉取、基准维护、风险计算、diff、持久化和通知条件

刷新规则：

- 服务启动时先立即触发一次 `refresh_once()`
- 启动完成后，由 runtime 独立按固定周期触发 `refresh_once()`
- 首版轮询间隔固定为 `5s`
- 账户刷新不依赖任何 `track` 事件

推送规则：

- 首次成功刷新后，发布一次 `account_summary_changed`
- 后续只有在摘要值、风险等级或原因摘要发生变化时才发布 `account_summary_changed`
- 单次刷新失败不发布风险事件，只保留最近一次成功摘要
- 如果连续失败，TUI 仍显示最近一次成功值和其 `updated at`

TUI 启动顺序调整为：

1. 请求 `account`
2. 请求 `tracks`
3. 请求当前选中 `track` 的详情
4. 订阅 WebSocket

后续更新依赖 WebSocket 推送刷新账户区块，不新增额外交互。

`Account` 区块不跟随选中的 `track` 变化。它始终表达账户整体状态。

### 8. 持久化策略

首版不建长历史表，只持久化 `AccountMonitorState`：

- `trading_day`
- `baseline_equity`
- `baseline_captured_at`
- `last_observed_account_snapshot`

其中 `last_observed_account_snapshot` 只保存原始账户快照字段，例如：

- `equity`
- `available`
- `unrealized_pnl`
- `observed_at`

目的：

- 让 `day_change` 在服务重启后保持连续
- 让账户接口临时失败时仍能展示最近一次成功值
- 让 `AccountReadModel` 在启动恢复时按当前规则重新计算，而不是复用旧版派生结果

不做：

- 账户历史时间序列
- 多日归档
- 净值曲线重建

### 9. TUI 表现原则

- `Account` 区块风格与现有 `track` 信号保持一致
- `risk_signal` 使用与 `track` 状态相同的视觉等级语义
- `normal` 不抢视线；`attention` 和 `critical` 需要明显可扫
- `reason` 保持紧凑，不展开成长文

## 模块职责

### `server/config`

- 定义 `account_monitor` 配置结构
- 提供默认值
- 负责配置合法性校验

### `exchanges/binance`

- 新增账户摘要读取能力
- 只暴露当前账户摘要，不承载信号计算

### `server/account_monitor`

- 实现 `AccountMonitor` 深模块
- 负责单次刷新、基准维护、风险计算、diff、持久化和通知条件

### `server/account_projector`

- 把 `AccountReadModel` 投影成 `AccountSummaryView`
- 供 `http` 和 `websocket` 共用，避免双份协议装配逻辑

### `server/query`

- 继续负责现有 `track` 查询
- 不承载账户读侧；账户读侧所有权留在 `AccountMonitor`

### `server/notifications`

- 把 `track` 与 `account` 变化统一进一条服务端通知流
- 供 `websocket` 消费

### `storage`

- 新增账户监控所需的最小持久化结构
- 支持读取和保存当日基准值与最近一次原始账户快照
- 不承担账户风险规则或协议投影

### `protocol`

- 新增账户摘要 DTO
- 新增统一流事件外壳中的 `account_summary_changed`

### `tui`

- 启动时拉取账户摘要
- 在 `Dashboard` 顶部渲染 `Account` 区块
- 响应账户摘要更新事件并刷新视图

## 风险与取舍

- 首版不做历史曲线，牺牲了趋势可视化，换取更低的实现成本和更高的值守价值
- `day_change` 使用“当日首次成功摘要”作为基准，口径简单稳定，但不等同于交易所官方日收益定义
- 单次账户拉取失败不触发风险异常，会降低误报，但也意味着“接口失联”本身不是账户风险信号
- 账户级信号和轨道级信号会同时出现在首页，需要控制视觉权重，避免账户区块盖住轨道表格

## 验收标准

- `Dashboard` 顶部新增账户监控区块
- 区块展示 `equity / available / unrealized pnl / day change / risk signal / reason / updated at`
- 区块展示 `day base at`
- `Account` 区块不依赖当前选中的 `track`
- 三类风险信号按默认阈值正确工作
- `[account_monitor]` 支持整段缺失和字段级默认值
- 非法阈值配置会在启动阶段报错
- 服务重启后，同一自然日内 `day_change` 基准保持一致
- TUI 通过独立 `account` 资源加载账户摘要，不复制 `tracks` 资源
- 账户接口临时失败时，区块仍可展示最近一次成功值或空值，不误报风险异常
- 相关协议、服务、存储和 TUI 测试覆盖新行为
