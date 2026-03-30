# Poise / Track 全量改名设计

## 1. 背景

产品名已经确定为 `Poise`，但当前仓库仍大量保留旧命名：

- 仓库目录仍是 `grid-platform`
- workspace 和 7 个 crate 仍使用 `grid-*`
- 领域主对象仍使用 `Grid*` / `grid_*`
- 配置、HTTP 路由、协议 DTO、SQLite schema、测试夹具和文档仍围绕 `grid`

这会造成两个问题：

1. 对外产品名、代码名和协议名分裂，阅读和沟通成本高。
2. `grid` 已经不能准确描述当前系统。当前系统的顶层运行对象更接近“可持续运行、可控制、可持久化的价格带库存执行轨道”，不是传统网格。

因此本次改名目标不是“补一层品牌文案”，而是一次无兼容层的全量收敛：产品名统一为 `Poise`，运行对象统一为 `track`。

## 2. 目标

- 把产品名统一为 `Poise`
- 把运行对象从 `grid` 统一改为 `track`
- 让 package、binary、类型、配置、协议、数据库、测试和文档使用同一套命名
- 不保留旧命名兼容层
- 完成后，仓库内不再把 `grid` 当作当前主线系统的一等命名

## 3. 非目标

- 本次不引入新的策略术语层，例如 `band`
- 本次不改策略数学模型，只做命名收敛
- 本次不顺手重画架构边界或拆分额外 crate
- 本次不保留 `grid_*` 到 `track_*` 的双写、别名或兼容解析
- 本次不为了统一而把 `core/`、`engine/`、`storage/` 这类职责目录改成带品牌前缀的目录名

## 4. 命名决策

### 4.1 产品名

- 产品名：`Poise`
- 仓库根目录：`trading-lab/poise`

### 4.2 运行对象

顶层运行对象统一命名为 `track`。

选择 `track` 的原因：

- 比 `grid` 更符合当前“持续运行、持续收敛库存偏差”的语义
- 比 `strategy` 更具体，不会把静态策略语义和运行实例混在一起
- 比 `band` 更适合承载命令、快照、事件和运行态

### 4.3 保留的通用职责目录

以下目录保留原职责名，不改成品牌前缀目录：

- `core/`
- `engine/`
- `storage/`
- `protocol/`
- `exchanges/binance/`
- `server/`
- `tui/`

原因：

- 这些目录名本身不是旧产品名，而是职责名
- 改成 `poise-core/` 这类目录只会增加路径噪音
- 一致性主要靠 package 名、binary 名和代码命名完成，不靠目录名前缀完成

## 5. 命名映射

### 5.1 仓库与 package

- `grid-platform` -> `poise`
- `grid-core` -> `poise-core`
- `grid-engine` -> `poise-engine`
- `grid-storage` -> `poise-storage`
- `grid-protocol` -> `poise-protocol`
- `grid-binance` -> `poise-binance`
- `grid-server` -> `poise-server`
- `grid-tui` -> `poise-tui`

### 5.2 领域类型

- `GridId` -> `TrackId`
- `GridRuntime` -> `TrackRuntime`
- `GridRuntimeSnapshot` -> `TrackRuntimeSnapshot`
- `GridManager` -> `TrackManager`
- `GridConfig` -> `TrackConfig`
- `GridStatus` -> `TrackStatus`
- `GridCommand` -> `TrackCommand`
- `GridObservation` -> `TrackObservation`
- `GridTransition` -> `TrackTransition`
- `GridEffect` -> `TrackEffect`

说明：

- 只要类型的主语是当前系统顶层运行对象，就统一从 `Grid*` 改成 `Track*`
- 纯通用类型或基础值对象不因本次改名而改名

### 5.3 字段与变量

- `grid_id` -> `track_id`
- `grid_ids` -> `track_ids`
- `grid` -> `track`
- `grids` -> `tracks`

### 5.4 配置

- `[[grids]]` -> `[[tracks]]`
- `grid_id = "..."` -> `track_id = "..."`

其他策略字段暂时保留：

- `lower_price`
- `upper_price`
- `long_exposure_units`
- `short_exposure_units`
- `notional_per_unit`

### 5.5 HTTP / WebSocket / DTO

- `/grids` -> `/tracks`
- `/grids/:id` -> `/tracks/:id`
- `/grids/:id/commands` -> `/tracks/:id/commands`

DTO 同步改名：

- `GridListResponse` -> `TrackListResponse`
- `GridListItemView` -> `TrackListItemView`
- `GridDetailView` -> `TrackDetailView`
- `GridCommandRequest` -> `TrackCommandRequest`
- `GridCommandAccepted` -> `TrackCommandAccepted`
- `GridStreamEvent` -> `TrackStreamEvent`

JSON 线协议字段同步改名：

- `grid_id` -> `track_id`

### 5.6 存储

SQLite schema 同步改名：

- `grid_snapshots` -> `track_snapshots`
- `grid_effects` -> `track_effects`
- `domain_events` -> `track_events`
- 所有 `grid_id` 列 -> `track_id`

说明：

- 因为本次不保留兼容层，旧数据文件不做在线迁移
- 旧本地数据库默认视为不可复用；新版本按新 schema 重新初始化

### 5.7 二进制与运行入口

- `grid-server` -> `poise-server`
- `grid-tui` -> `poise-tui`

相关默认命名同步改：

- `.data/<environment>/grid-server.sqlite` -> `.data/<environment>/poise-server.sqlite`
- `grid-server.toml` 这类测试临时文件名 -> `poise-server.toml`
- TUI 标题和日志前缀统一改为 `Poise`

### 5.8 环境变量

环境变量统一切到 `POISE_*`：

- `GRID_PLATFORM_BASE_URL` -> `POISE_BASE_URL`
- `GRID_PLATFORM_WS_URL` -> `POISE_WS_URL`
- `GRID_TUI_WS_URL` -> `POISE_TUI_WS_URL`

本次不保留旧环境变量兼容读取。

## 6. 文档与文件名策略

### 6.1 当前文档

README、配置示例、当前协议文档、当前架构 spec、当前主线 plan 中的主引用统一改为 `Poise` / `track`。

### 6.2 历史文档

历史 spec / plan 允许保留原文件名中的时间戳和旧主题词，但其正文中涉及“当前主线对象”的部分应在本次批量替换时同步收敛为 `Poise` / `track`。

判断原则：

- 仍被 README 或当前主线文档直接引用的，视为“当前文档”，要同步改标题、正文和链接
- 只作为历史记录存在、且不再被主线入口引用的，可以保留旧文件名，不要求全仓库逐个重命名

这样做的原因是：

- 避免把一次命名收敛扩成大规模历史文件路径重写
- 保留时间线可读性
- 让当前入口文档先达到一致

## 7. 实施顺序

### 7.1 第一阶段：编译层命名收敛

- 改 workspace package 名
- 改 7 个 crate 的 package 名和相互依赖
- 改 binary 名
- 改 import 路径和 `Cargo.lock`

目标：

- 先恢复 workspace 可编译

### 7.2 第二阶段：领域与协议收敛

- 改 `Grid*` / `grid_*` 到 `Track*` / `track_*`
- 改配置结构、HTTP 路由、协议 DTO、JSON 字段
- 改测试夹具和请求示例

目标：

- 让代码主语、接口主语和配置主语一致

### 7.3 第三阶段：存储与运行入口收敛

- 改 SQLite 表名、列名、索引名
- 改默认数据库文件名
- 改环境变量名
- 改 README、docs、示例配置和测试临时文件名

目标：

- 清理所有用户可见和持久化命名残留

### 7.4 第四阶段：仓库根目录切换

最后再把仓库根目录从：

- `trading-lab/grid-platform`

改成：

- `trading-lab/poise`

这一动作放到最后，不在当前活跃 Codex 工作区中间执行。

原因：

- 当前 Codex 线程和工作区提示缓存显式绑定旧绝对路径
- 中途改目录容易让当前线程和工作区关联失效
- 更稳妥的方式是：先在当前目录完成 spec / plan，真正执行重命名时关闭旧工作区，再用新目录重开

## 8. 风险与处理

### 8.1 编译断点多

package 名、类型名、字段名和路由名会形成大面积编译错误。

处理方式：

- 严格按阶段推进
- 先收 package / import，再收领域类型，再收协议和存储

### 8.2 无兼容层带来的破坏性变更

旧配置、旧 URL、旧环境变量、旧数据库文件都不能继续工作。

处理方式：

- 在 README 和变更说明中明确写清
- 验收按“新命名全通、新命名唯一路径”执行

### 8.3 历史文档噪音

历史 plan / spec 数量多，如果全部改文件名，成本高且噪音大。

处理方式：

- 优先保证当前入口文档统一
- 历史文档以正文替换和入口去旧名为主，不追求全量文件名重写

## 9. 验收标准

满足以下条件才算完成：

- workspace 和 7 个 crate 的 package 名全部切到 `poise-*`
- 二进制名全部切到 `poise-*`
- 代码中的顶层运行对象全部从 `Grid*` / `grid_*` 切到 `Track*` / `track_*`
- 配置键、HTTP 路由、协议字段全部切到 `track`
- SQLite 表名和列名全部切到 `track`
- README、当前协议文档、当前架构 spec 和主线引用文档不再把当前系统称为 `grid-platform` 或顶层对象 `grid`
- `cargo test` 在新命名下通过
- 仓库根目录最终切到 `trading-lab/poise`

## 10. 当前结论

本次改名不做半步方案，也不保留兼容。

统一后的命名基线是：

- 产品名：`Poise`
- 仓库根目录：`poise`
- package / binary：`poise-*`
- 运行对象：`track`
- 配置、协议、存储、测试和文档全部以 `track` 为当前主语

这套命名能准确表达当前系统是一个以 `Poise` 为产品名、以 `track` 为顶层运行对象的策略执行系统，而不是传统网格平台。
