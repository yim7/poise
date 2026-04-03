# Track Activity / Diagnostics 分层设计

## 背景

当前 `TrackDetailView.activity` 由服务端把 `recent_track_events` 和 `recent_effects` 合并后直接投影得到，定义见 [../../protocol-contract.md](../../protocol-contract.md)。

这个设计把两类语义混在了一起：

- 用户应关注的活动
- 内部诊断上有价值、但不适合直接给用户看的信息

`DomainEvent::ExposureTargetChanged` 是当前最明显的问题点。它表达的是策略目标变化，内部排查时有价值，但在用户视角里容易被误读为“即将发单”或“执行器正在动作”，而且频率高，放进 `Activity` 会干扰真正重要的执行活动。

## 设计目标

- 保留底层 `track_events` 和 `effects`，不丢内部诊断信息
- 让对外协议明确区分“用户活动”和“内部诊断”
- 让客户端可以显式选择是否进入 debug 视角，但不让客户端自己理解或过滤原始领域事件
- 保持主用户协议稳定，把内部诊断保留在单独 debug 边界内
- 暂不引入通用订阅系统或消息分类系统

## 非目标

- 不改变 engine 的事件生成规则
- 不改变事件持久化模型
- 不让 `/ws` 支持按消息类型订阅
- 不在本次设计里加入复杂的权限模型

## 当前边界问题

当前最主要的复杂度信号是认知负担。

问题不在于一条具体日志，而在于 `activity` 这个接口同时暴露了两种不同目的的信息。客户端看到 `activity` 后，无法从接口本身知道哪些是用户可理解的活动，哪些只是内部计算痕迹。

如果只在 `poise-tui` 里过滤 `ExposureTargetChanged`，会带来额外问题：

- HTTP 和 WebSocket 返回的 `activity` 语义不变，只是某个客户端偷偷隐藏
- 以后新增客户端时，需要重复维护相同过滤规则
- “哪些事件属于用户活动”这个知识泄漏到多个客户端

## 方案比较

### 方案 A：只在 TUI 过滤

做法：

- 服务端协议保持不变
- `poise-tui` 渲染 `Activity` 时跳过 `ExposureTargetChanged`

优点：

- 改动最小

缺点：

- `activity` 的协议语义仍然混乱
- 不同客户端可能显示不一致
- 过滤规则泄漏到客户端

结论：

- 不采用

### 方案 B：在主 `TrackDetailView` 里直接增加 `diagnostics`

做法：

- `TrackDetailView.activity` 只承载面向用户的活动流
- `TrackDetailView.diagnostics` 承载内部诊断信息
- 服务端 projector 负责事件分类
- 客户端只负责决定默认显示哪一块

优点：

- 接口语义清楚
- 分类知识集中在服务端投影层
- 客户端可以按场景选择显示，而不需要理解原始事件

缺点：

- 会把“内部诊断”直接固化到主用户协议
- 大多数客户端即使不用 diagnostics，也要面对新的公共字段
- diagnostics 的稳定性边界容易模糊，后续演进成本高

结论：

- 不采用

### 方案 C：稳定用户详情 + 单独 debug diagnostics 入口

做法：

- `TrackDetailView.activity` 保持面向用户的稳定活动流
- 新增显式 debug 命名空间下的接口暴露 diagnostics
- 服务端 projector 负责分类
- 客户端如果进入 debug 模式，再显式加载 diagnostics

优点：

- 主用户协议保持干净
- diagnostics 不会提前冻结成所有客户端都要理解的稳定字段
- 客户端仍然可以选择是否显示 diagnostics

缺点：

- 比“直接加字段”多一个接口边界
- 需要明确 debug 接口是非稳定契约

结论：

- 采用

### 方案 D：引入通用订阅分类系统

做法：

- `/ws` 允许客户端声明订阅哪些类型的数据流
- 服务端按订阅类别选择推送

优点：

- 长期看最灵活

缺点：

- 当前问题规模不足以支撑这套复杂度
- 会同时扩大协议、连接管理和客户端状态机的改动面

结论：

- 当前阶段不采用

## 最终设计

### 1. 协议分层

`TrackDetailView` 继续只承载稳定的用户详情，不新增 `diagnostics` 字段。

建议形状：

```rust
pub struct TrackDetailView {
    pub identity: GridIdentityView,
    pub status: GridStatusPanelView,
    pub strategy: GridStrategyView,
    pub market: GridMarketView,
    pub position: GridPositionView,
    pub statistics: GridStatisticsView,
    pub execution: GridExecutionView,
    pub activity: Vec<GridActivityItemView>,
    pub available_commands: Vec<GridCommandView>,
}
```

另行新增 debug 专用接口：

- `GET /debug/tracks/:id/diagnostics` -> `TrackDiagnosticsView`

建议形状：

```rust
pub struct TrackDiagnosticsView {
    pub items: Vec<TrackDiagnosticItemView>,
}

pub struct TrackDiagnosticItemView {
    pub ts: String,
    pub message: String,
    pub level: ActivityLevelView,
}
```

这里显式使用单独的 `TrackDiagnosticItemView`，即使字段暂时与 `GridActivityItemView` 一致，也不复用同一个协议类型。这样可以把“用户活动”和“调试诊断”明确建成两个抽象，避免后续语义重新混在一起。

### 2. 稳定性边界

- `TrackDetailView.activity`
  - 稳定用户协议的一部分
  - 面向用户理解和日常操作
  - 文案和分类应尽量稳定
- `TrackDiagnosticsView`
  - debug 专用接口
  - 非稳定、best-effort 诊断视图
  - 允许随版本演进内容、文案、保留策略和分类细节
  - 不作为自动化、告警或外部集成的稳定契约

这个边界必须写入协议文档，避免实现时把 debug 数据再次当成稳定产品接口。

同时，非稳定属性不只写在文档里，也直接编码在接口边界里：

- 主用户协议继续使用 `/tracks/...`
- debug 诊断接口统一放在 `/debug/...`

这样调用方仅从路径就能看出它不是和主产品详情同级的稳定接口。

### 3. 模块职责

- `engine`
  - 继续只负责产生原始 `DomainEvent` 和 `TrackEffect`
  - 不知道哪些事件该出现在用户活动，哪些该进入诊断区
- `server/read_model`
  - 继续承载 `recent_track_events` 和 `recent_effects`
  - 不做用户语义分类
- `server/event presentation classifier` 或等价分类层
  - 单独拥有“哪些信息属于 activity，哪些属于 diagnostics”的分类知识
  - 输入底层 `recent_track_events` / `recent_effects`
  - 输出已经分区的表示，例如 `activity_events`、`diagnostic_events`
- `server/projector`
  - 只负责把已经分到 `activity` 的项目渲染成稳定 `TrackDetailView.activity`
- `server/debug projector` 或等价的 debug 查询层
  - 只负责把已经分到 `diagnostics` 的项目渲染成 `TrackDiagnosticsView`
- `protocol`
  - 主协议暴露稳定用户视图
  - debug 协议暴露非稳定诊断视图
- `tui`
  - 默认只显示 `activity`
  - 进入 debug 模式时，显式请求并显示 `diagnostics`

关键约束：

- 分类规则只能有一个所有者
- 稳定投影和 debug 投影可以是两个出口，但不能各自维护一套事件分类判断

这里允许实现上仍放在同一个 Rust 文件里，但语义上必须把“分类”与“渲染”视为不同职责，避免后续新增事件时出现两边都要同步修改分类表。

### 4. 分类原则

初版不靠枚举黑名单堆规则，而是定义分类原则：

- `activity`
  - 用户无需理解 engine 内部细节也能读懂
  - 能帮助用户判断当前执行状态、风险状态或系统是否需要关注
  - 与 effect 状态、风险拒绝、替换门槛、带内外状态变化等直接相关
- `diagnostics`
  - 主要服务于排查和调试
  - 如果没有 engine 内部背景，普通用户容易误读
  - 高频内部计算痕迹、目标漂移、规划过程信息默认放这里

默认判定规则：

- 如果一条信息需要解释 engine 内部计算过程才能读懂，默认进入 `diagnostics`
- 如果一条信息是用户判断系统行为的直接依据，进入 `activity`

实现约束：

- 上述判定规则由单一 classifier 执行
- `projector` 和 debug 查询层只消费分类结果，不重复做 `match` 分支判定

### 5. 首批分类规则

首个明确规则如下：

- `DomainEvent::ExposureTargetChanged` 进入 `diagnostics`
- 与执行结果、风险拒绝、替换门槛、策略状态变化直接相关、且用户应理解的活动，继续进入 `activity`
- `recent_effects` 默认继续进入 `activity`

初版不追求一次把所有事件都重新分组，只先把明确误导用户的高频内部事件移到 `diagnostics`。

### 6. WebSocket 语义

`/ws` 仍然只推 `track_list_item_changed` 和 `track_detail_changed`。

本次不为 diagnostics 增加 websocket 推送，也不增加订阅协商。

原因是：

- 当前需求只是避免把内部目标变化混入用户活动
- diagnostics 的主要使用场景是显式 debug，而不是常驻高频消费
- 如果未来确实出现需要实时看 diagnostics 的场景，再单独设计 debug websocket，而不是把订阅复杂度提前引入主链路

如果未来增加 debug websocket，也应放在独立 debug 命名空间下，而不是复用主 `/ws`。

### 7. TUI 默认行为

`poise-tui` 默认仍只渲染 `Activity` 区块，不新增新的主界面噪声。

如果需要内部排查，可以增加显式的 diagnostics 入口，例如：

- 详情页新增 `Diagnostics` 面板
- 或者通过 debug 开关切换显示

触发方式建议是按需加载，而不是和主详情一起返回。这样普通用户路径不承担 debug 复杂度。

## 为什么这是更干净的边界

这个方案把复杂度压回了服务端的投影与 debug 查询边界。

客户端不再需要知道：

- `ExposureTargetChanged` 是什么
- 哪些 `DomainEvent` 该隐藏
- 哪些活动属于“用户活动”，哪些属于“内部诊断”

客户端只需要知道：

- `activity` 默认展示给用户
- debug 模式下可以额外请求 `diagnostics`

这样后续新增类似事件时，只改服务端分类规则，而不是修改每个客户端的过滤逻辑。

## 风险与后续演进

### 风险

- 初版只移动 `ExposureTargetChanged`，其他边界模糊的事件仍可能需要后续再分类
- diagnostics 初版仍是时间线文案，如果未来需要更强结构化排查能力，可能要再演进

### 后续可选演进

- 为 `diagnostics` 增加显式显示开关
- 把部分诊断信息做成结构化字段，而不是纯文案列表
- 当确实出现多客户端、多订阅场景时，再评估是否需要通用订阅系统

## 验收标准

- `TrackDetailView.activity` 保持稳定、面向用户的活动语义
- diagnostics 不进入主 `TrackDetailView`
- 存在单独 debug diagnostics 入口，且路径和文档都明确其非稳定属性
- activity / diagnostics 的分类规则只有一个所有者
- `ExposureTargetChanged` 不再进入用户 `activity`
- 底层 `track_events` 仍然保留该事件
- `poise-tui` 默认行为不再向普通用户展示这类高频目标变化
