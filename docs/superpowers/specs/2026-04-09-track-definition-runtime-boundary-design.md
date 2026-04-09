# Track Definition 与 Runtime 边界设计

## 背景

当前 `track` 的 definition 与 persisted runtime 还混在一起：

- `server/src/config.rs` 同时承担 TOML 形状、默认值展开和业务语义
- `TrackRuntimeSnapshot` 既是 restore artifact，又带着 `instrument/config`
- query 侧通过 `TrackBudgetCatalog` 临时拼 definition 与 runtime
- bootstrap 直接比较 persisted snapshot 中的 `instrument/config`

这会把同一份知识分散在 `server`、`application`、`engine`、`storage` 多层，导致：

- definition 没有单一 owner
- runtime snapshot 既服务恢复，又被读侧直接消费
- budget、tick timeout、restore gating 的语义混在一起
- 兼容旧 persisted 状态时容易长出多套 fallback

## 目标

- 让 definition、persisted runtime、bootstrap、query 各自拥有单一 owner
- 让 `TrackRuntimeSnapshot` 只表达 runtime 事实，不再混入 definition
- 让 query 侧只消费显式读模型输入，不直接依赖 restore artifact
- 让 restore gating 与恢复后约束同步成为两条清晰语义
- 保持旧 persisted 状态可恢复，但兼容入口只能有一个

## 非目标

- 不改变外部 HTTP / WebSocket 协议
- 不引入新的持久化 definition 表
- 不让 `engine` 反向依赖 `application`
- 不让预算变化触发强制 rebuild

## 结论

采用以下边界：

1. `poise-server` 只拥有配置文件形状
2. `poise-application` 拥有 normalized definition、registry、query 读源
3. `poise-engine` 拥有 restore primitives、runtime seed、post-restore constraints 和 persisted codec
4. `poise-storage` 只持久化 runtime 事实
5. `server::state_bootstrap` 是唯一编排点，负责从 config 走到 query-ready repository

## 配置输入边界

### `server/src/config.rs`

`poise-server` 只拥有 raw 配置文件类型：

- `TrackFileDefinition`

职责：

- 表达 TOML / `serde` 形状
- 做最小的字段读取和格式错误提示

不负责：

- 业务默认值展开
- 风控语义校验
- 生成 query / runtime 使用的 definition 对象

### `application/src/track_definition.rs`

`poise-application` 拥有两层语义输入：

- `ConfiguredTrackInput`
  - 不带文件格式知识
  - 只是 application 侧的原始输入 DTO
- `ConfiguredTrackDefinition`
  - 由 `ConfiguredTrackInput` 规范化后得到
  - 负责默认值展开和语义校验

规范化路径固定为：

`TrackFileDefinition -> ConfiguredTrackInput -> ConfiguredTrackDefinition`

默认值、预算推导、合法性校验都只在 `ConfiguredTrackDefinition::try_from_input(...)` 这一处定义。

## Definition Owner

### `TrackPreparedDefinition`

`TrackPreparedDefinition` 是完整 normalized definition 的 owner。

它至少包含：

- `track_id`
- `instrument`
- `track_config`
- `budget`
- `tick_timeout_secs`
- `restore_revision`

职责：

- 作为运行期内关于某个 track definition 的唯一完整对象
- 对外投影 query 所需的 `TrackReadDefinition`
- 对外投影 engine 所需的 `TrackRuntimeSeed`
- 对外投影 engine 所需的 `PostRestoreConstraints`

`TrackReadDefinition` 只是 query 用的读侧投影，不是完整 definition。

### `PreparedTrackRegistry`

`PreparedTrackRegistry` 是运行期内 definition 集合的唯一 owner。

职责：

- 保存全部 `TrackPreparedDefinition`
- 通过 `track_id` 提供稳定查询
- 向 bootstrap、query、assembly 暴露同一份 definition 事实

不负责：

- 构造完整 runtime snapshot
- 直接做 persisted state 读取

## Engine Owner

### `engine/src/persisted_runtime.rs`

`poise-engine` 拥有以下恢复相关类型：

- `TrackRestoreRevision`
- `TrackRuntimeSeed`
- `PostRestoreConstraints`
- `PersistedRuntimeCodec`

其中：

- `TrackRestoreRevision`
  - 只表达“persisted runtime 能否安全恢复”
  - 只由真正影响恢复语义的字段决定
- `TrackRuntimeSeed`
  - 表达从 definition 启动一个全新 runtime 所需的最小输入
- `PostRestoreConstraints`
  - 表达恢复后立即生效、但不参与 restore gating 的运行约束
- `PersistedRuntimeCodec`
  - 是唯一 legacy persisted runtime 兼容入口

### `TrackRestoreRevision` 语义

`TrackRestoreRevision` 只由以下字段决定：

- `instrument`
- `track_config`

它明确不包含：

- `budget`
- `tick_timeout_secs`

原因：

- `budget` 和 `tick_timeout_secs` 变化后，persisted runtime 仍可恢复
- 它们应在恢复后通过 `PostRestoreConstraints` 生效，而不是触发 mismatch

## Runtime Snapshot 边界

### `engine/src/snapshot.rs`

`TrackRuntimeSnapshot` 改成 runtime-only artifact。

保留：

- 当前持仓、目标持仓、observed 状态、execution 状态、ledger、risk、margin guard 等 runtime 事实
- `restore_revision`

移除：

- `instrument`
- `track_config`
- `budget`
- 任何 definition 派生字段

这样 `TrackRuntimeSnapshot` 只服务于：

- persisted runtime 恢复
- runtime 状态保存

它不再直接作为 query 侧输入。

## Query 边界

### `application/src/track_read_source.rs`

query 侧新增两个 application-owned 类型：

- `TrackRuntimeReadState`
- `TrackReadSource`

其中：

- `TrackRuntimeReadState`
  - 是 query 真正需要的 runtime 投影
  - 不直接暴露完整 `TrackRuntimeSnapshot`
- `TrackReadSource`
  - 组合 `TrackReadDefinition`
  - `TrackRuntimeReadState`
  - events / effects / updated_at

`TrackQueryService` 的职责改成：

- 从 `PreparedTrackRegistry` 读取 `TrackPreparedDefinition`
- 从 `TrackQueryStore` 读取 persisted runtime 与相关读侧记录
- 组装 `TrackReadSource`

`TrackReadModel` 只从 `TrackReadSource` 构造。

## Bootstrap 边界

### `server/src/state_bootstrap.rs`

`state_bootstrap` 是唯一编排点，负责：

1. 读取 `TrackFileDefinition`
2. 映射为 `ConfiguredTrackInput`
3. 规范化为 `ConfiguredTrackDefinition`
4. 构造 `PreparedTrackRegistry`
5. 读取 persisted runtime
6. 通过 `PersistedRuntimeCodec::decode(...)` 解码旧状态
7. 比较 `restore_revision`
8. 对缺失 persisted runtime 的 track 写入初始 runtime
9. 对已恢复 runtime 应用 `PostRestoreConstraints`
10. 持久化必要的 runtime 调整
11. 返回 query-ready repositories 与 registry

### Strict / Rebuild 语义

- `Strict`
  - 若旧 persisted runtime 的 `restore_revision` 与当前 definition 不匹配，则报 mismatch
  - 若 config 中新增 track，但数据库里还没有它的 persisted track presence 记录，不算 mismatch；应补写初始 runtime
  - 若某个 track 已有 persisted track presence 记录，但 runtime snapshot 丢失，则视为 persisted state 损坏并报 mismatch
  - 若数据库中存在 persisted track presence 或 runtime，但当前 config 已删除该 track，则报 mismatch
- `Rebuild`
  - 发现 mismatch 时，按现有 rebuild 规则重建本地状态，并清理不在当前 config 中的旧 persisted track

### Mismatch Payload

`StateBootstrapError::PersistedStateMismatch` 的 detail 需要和 runtime-only 边界一起重定义。

正式约束：

- mismatch payload 不能再要求 persisted state 提供 `instrument` / `track_config`
- `main` 和其他调用方只能消费 bootstrap 暴露的结构化 mismatch 事实

推荐形状：

- `RestoreRevisionMismatch { expected_revision, actual_revision }`
- `PersistedTrackMissingRuntime`
- `PersistedTrackMissingFromConfig`

其中 `track_id` 继续由外层 `PersistedStateMismatch` 提供。

这样：

- `state_bootstrap` 只暴露新边界下真实可得的诊断事实
- `main` 的 CLI 文案不需要再理解 persisted definition 细节
- storage 不会为了错误展示而继续保留旧 definition 字段

### 预算变化语义

`budget` 变化不参与 `restore_revision` 比较。

但 bootstrap 返回前必须执行一次 engine-owned 的恢复后约束同步：

- 调用 `TrackRuntime::apply_post_restore_constraints(...)`
- 只收敛 `desired_exposure` 与其他运行约束状态
- 不直接修改 `current_exposure`
- 不依赖最新市场价格
- 不替代正常 reconcile

这保证：

- persisted runtime 可以恢复
- 恢复后的 runtime 不会继续沿用过期预算语义

## Storage 边界

### `storage/src/sqlite.rs`

`poise-storage` 只持久化 runtime 事实：

- runtime snapshot
- events
- effects
- account monitor
- 最小的 persisted track presence 记录

它不持久化完整 definition，也不持久化单独的 `track_definitions` 表。

persisted track presence 记录只表达：

- 某个 `track_id` 是否已经在本地状态中初始化过

它不表达：

- definition 内容
- budget
- `track_config`
- query 展示字段

这个最小记录只为 `Strict` 启动语义服务，用来区分：

- 新增 track，尚未初始化
- 已存在 track，但 persisted runtime 丢失

### Storage 不变量

persisted track presence 不能由 bootstrap 单独写入。

正式约束：

- 初始 runtime seed 写入时，presence 记录与 runtime snapshot 必须在同一个 SQLite 事务内提交
- 后续任意 runtime snapshot 持久化时，也必须在同一个事务内保证对应 track 的 presence 记录存在
- 不允许暴露“先注册 presence，再单独写 runtime”的两步接口

这样可以避免：

- 启动中断后只留下 presence
- 严格模式把半写入状态误判成 persisted state 损坏
- bootstrap 额外承担存储一致性知识

所有 persisted runtime 的读取都先经过：

- `poise_engine::persisted_runtime::PersistedRuntimeCodec::decode(...)`

不允许：

- SQLite 路径自己做一套 legacy fallback
- JSON snapshot 路径自己做另一套 legacy fallback

## Legacy 兼容

兼容旧 persisted runtime 时，单一入口固定为：

- `PersistedRuntimeCodec::decode(...)`

职责：

- 兼容旧 JSON snapshot
- 兼容旧 SQLite 行
- 把 legacy `realized_pnl_*` 等旧字段统一回填为当前 ledger / runtime 结构

调用方只接收规范化后的 runtime snapshot，不再自己判断 fallback 条件。

## 模块职责总结

### `poise-server`

负责：

- 配置文件解析
- raw config 到 application input 的机械映射
- bootstrap 编排
- 平台装配

不负责：

- definition 语义默认值
- restore revision 计算
- legacy runtime codec
- 从 persisted state 反推出旧 definition 内容

### `poise-application`

负责：

- normalized definition
- registry
- query 读侧 source

不负责：

- persisted runtime 兼容解码
- runtime 默认值细节

### `poise-engine`

负责：

- restore revision
- runtime seed
- post-restore constraints
- runtime-only snapshot
- legacy persisted runtime codec

### `poise-storage`

负责：

- runtime 事实的 SQLite 持久化
- 不拥有 definition 语义

## 结果

完成后应满足：

- definition 只有一个运行期 owner：`PreparedTrackRegistry`
- persisted runtime 只有一个恢复 owner：`PersistedRuntimeCodec`
- query 不再直接消费 `TrackRuntimeSnapshot`
- bootstrap 返回的 repository 从创建起就是 query-ready
- presence 与 runtime snapshot 之间没有可持久化的半写入裂缝
- `Strict` 模式能区分“新增 track 尚未初始化”和“已知 track 的 runtime 丢失”
- `Strict` 模式会把“不在当前 config 中的旧 persisted track”视为 mismatch
- mismatch payload 只依赖 revision / presence / runtime 缺失这组事实
- `TrackBudgetCatalog`、`track_definitions` 这类已废弃路径不再恢复
