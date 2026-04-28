# Track 分层概念迁移计划

> 执行规则：每个任务先补或确认验收测试，再实现；验收通过后立即提交，并把 commit SHA 回写到对应任务。

**目标：** 明确 track 相关概念的层级归属，把配置文件规格、静态定义和运行态对象分开，减少 `Configured` / `Prepared` / `Definition` 等相近名字带来的认知负担。

**核心迁移方向：**

```text
server::config::TrackSpec
  -> core::track::TrackDefinition
      -> engine::runtime::TrackRuntime
```

## 分层规则

- `core` 拥有纯领域概念和不变量：`TrackId`、`Venue`、`Instrument`、`TrackDefinition`、策略和风险校验。
- `engine` 拥有运行时状态机和交易决策：`TrackRuntime`、`TrackManager`、`TrackEffect`、`TrackTransition`、executor / reconciler。
- `application` 拥有用例边界和投影：service、session queue、journal port、read model 组装、definition registry。
- `server` 拥有进程入口和外部世界：TOML config schema、exchange wiring、HTTP/WebSocket、runtime task 装配。
- `storage` 和 `protocol` 分别拥有持久化实现和外部 API DTO，不拥有领域定义。

## 目标命名

- `server::config::TrackSpec`：用户在 `[[tracks]]` 中声明的配置规格，允许包含 `leverage` 等 server startup 字段。
- `core::track::TrackDefinition`：已补默认值、已校验的静态 track 定义。
- `application::TrackDefinitionRegistry`：application 用例层的一组静态定义索引，服务 query/startup/read model。
- `engine::runtime::TrackRuntime`：真实运行态对象，表示一个活着的 track 实例。

## core 迁移清单

**必须迁移到 core：**

- `TrackId`：基础领域标识，不属于运行引擎。
- `Venue`：交易场所标识，配置、协议、exchange wiring 和 engine 都会引用。
- `Instrument`：交易标的标识，由 `Venue + symbol` 组成，不属于运行态。
- `TrackDefinition`：静态 track 定义，包含 `TrackId`、`Instrument`、`TrackConfig`、`max_notional`、`LossLimits`、`tick_timeout_secs`。
- 静态定义相关计算：`curve_max_notional`、`effective_max_notional`、`required_additional_notional(position_qty)`、`exposure_from_position_qty(position_qty)`；优先作为 `TrackDefinition` 方法或 core 内部 helper，而不是保留 application wrapper。

**已经在 core，保持不动：**

- `TrackConfig`
- `ShapeFamily`
- `BandProtectionPolicy`
- `LossLimits`
- `Exposure`
- `ExchangeRules`

**不迁移到 core：**

- `TrackSpec`：TOML schema，属于 server。
- `TrackDefinitionInput`：暂不引入；当前只有 `TrackSpec` 一个构造入口，新增 input 只是把字段搬到另一个 struct literal。
- `TrackRuntime` / `TrackManager`：运行态和状态机，属于 engine。
- `TrackEffect` / `TrackTransition`：运行后产生的 effect 和转场结果，属于 engine。
- `TrackReadModel` / protocol view：读模型和外部 API DTO，属于 application / protocol。
- `TrackDefinitionRegistry`：先保留在 application；只有当多个下层 crate 需要同一组定义索引不变量时，再考虑下沉。

**当前已满足的前提，不作为迁移任务：**

- `Instrument` 已经表示完整标的身份：`venue + symbol`。
- `venue` 已经不用于运行时动态选择 exchange port；exchange port 由 `server::config::ExchangeConfig` 构造。
- `venue` 已经用于完整身份、日志/持久化自描述、事件匹配和未来多 venue 扩展。
- `TrackDefinition.instrument.venue` 已经来自 service-level `ExchangeConfig::venue()`，而不是 track-level 配置字段。
- 因此本计划不包含任何 `Instrument` 结构或语义改动；后续如果迁移到 core，只移动归属和 import 路径。

## 非目标

- 不把 `TrackRuntime` 改名成 `Track`。
- 不删除 `Instrument.venue`。
- 不改变交易行为、风控行为、startup 恢复行为和 effect 派发语义。
- 不改变 HTTP/WebSocket protocol 字段名。
- 不把 `server::config::TrackSpec` 下沉到 core；它是 TOML schema，不是领域定义。
- 不为了迁移保留长期 alias；如果某个任务中需要临时兼容，必须在同一任务内删除。

## Task 1：收束 server 配置侧命名为 TrackSpec

**目的：** 让配置文件条目的名字表达“用户声明的规格”，避免 server 侧再出现 `TrackDefinition` 这种容易和领域定义混淆的名字。

**文件：**

- `server/src/config.rs`
- `server/src/exchange_startup.rs`
- `server/src/assembly.rs`
- `server/src/state_bootstrap.rs`
- `server/src/main.rs`

**步骤：**

- [x] 将 `TrackFileDefinition` / `TrackEntry` / 测试 alias 统一为 `TrackSpec`。
- [x] 将测试 fixture helper 统一为 `track_spec`。
- [x] 确认 `TrackSpec` 仍只服务 config schema 和 startup-only 字段读取。
- [x] 搜索确认 server 代码中不再出现配置侧 `TrackDefinition` / `TrackEntry` / `TrackFileDefinition`。

**最小验收命令：**

- `cargo test -p poise-server config::tests::`
- `cargo test -p poise-server exchange_startup::tests::`

**执行记录：**

- 2026-04-28：当前工作区已完成，未提交。
- 验收：`cargo test -p poise-server config::tests::`
- 验收：`cargo test -p poise-server exchange_startup::tests::`

**Commit SHA：** `24f6a6c`

## Task 2：把 track 标识和交易标的类型下沉到 core

**目的：** `TrackId`、`Venue`、`Instrument` 是基础领域标识，不属于 engine 运行时。先下沉这些低风险类型，为后续 `TrackDefinition` 下沉做依赖准备。本任务只改变类型归属和 import 路径，不改变 `Instrument { venue, symbol }` 的结构和语义。

**文件：**

- `core/src/track.rs`
- `core/src/lib.rs`
- `engine/src/track.rs`
- workspace 中所有引用 `poise_engine::track::{TrackId, Venue, Instrument}` 的 Rust 文件

**步骤：**

- [x] 在 `core` 中定义并导出 `TrackId`、`Venue`、`Instrument`。
- [x] 将全 workspace 引用迁移到 `poise_core::track::{TrackId, Venue, Instrument}`。
- [x] 删除 engine 中对应类型定义，不保留长期 re-export。
- [x] 保留 engine 中真正属于运行/执行层的 track 相关类型。

**最小验收命令：**

- `cargo test -p poise-core track::tests::`
- `cargo test -p poise-engine track::tests::`
- `cargo test -p poise-application track_definition::tests::`
- `cargo test -p poise-server config::tests::`

**执行记录：**

- 2026-04-28：已完成。
- 验收：`cargo test -p poise-core track::tests::`
- 验收：`cargo test -p poise-engine track::tests::`
- 验收：`cargo test -p poise-application track_definition::tests::`
- 验收：`cargo test -p poise-server config::tests::`
- 额外确认：`cargo test -p poise-server assembly::tests::track_instrument_uses_service_exchange_venue`
- 检查：`git diff --check`

**Commit SHA：** `b7a50b2`

## Task 3：把静态 TrackDefinition 下沉到 core

**目的：** 让“一个 track 是什么”由 core 统一拥有，删除 application 中 `ConfiguredTrackInput` / `ConfiguredTrackDefinition` 这组过程式命名。

**文件：**

- `core/src/track.rs`
- `application/src/track_definition.rs`
- `application/src/lib.rs`
- `server/src/config.rs`
- `server/src/state_bootstrap.rs`
- `server/src/assembly.rs`
- `server/src/test_support.rs`

**步骤：**

- [x] 在 `core::track` 中定义 `TrackDefinition`，字段包括 `track_id`、`instrument`、`track_config`、`max_notional`、`loss_limits`、`tick_timeout_secs`。
- [x] 在 `TrackDefinition` 上提供 `try_new(track_id, instrument, track_config, max_notional, loss_limits, tick_timeout_secs)`，接收已经分组好的领域对象，不接 15 个 TOML 原始字段。
- [x] 将默认值补齐和校验逻辑移动到 `TrackDefinition::try_new`。
- [x] 将 `server::config::TrackSpec::to_configured_input` 改为 `to_track_definition`，由它负责 TOML schema 字段到领域对象的映射，并调用 `TrackDefinition::try_new`。
- [x] 删除 `ConfiguredTrackInput` 和 `ConfiguredTrackDefinition`。
- [x] 评估并删除浅层 `TrackReadDefinition` / `TrackStartupDefinition`；read model 和 startup 优先直接使用 `core::TrackDefinition` 或其语义方法。
- [x] 如果某个 projection 必须保留，必须说明它隐藏了什么信息，而不是只复制字段。
- [x] 将 `PreparedTrackRegistry` 改名为 `TrackDefinitionRegistry`，并存储 `core::TrackDefinition`。

**最小验收命令：**

- `cargo test -p poise-core track::tests::`
- `cargo test -p poise-application track_definition::tests::`
- `cargo test -p poise-application query_service::tests::`
- `cargo test -p poise-server config::tests::`
- `cargo test -p poise-server exchange_startup::tests::`

**执行记录：**

- 2026-04-28：已完成；未保留 read/startup projection。
- 验收：`cargo test -p poise-core track::tests::`
- 验收：`cargo test -p poise-application track_definition::tests::`
- 验收：`cargo test -p poise-application query_service::tests::`
- 验收：`cargo test -p poise-server config::tests::`
- 验收：`cargo test -p poise-server exchange_startup::tests::`
- 额外确认：`cargo test -p poise-application read_model::tests::`
- 额外确认：`cargo test -p poise-server assembly::tests::track_instrument_uses_service_exchange_venue`
- 额外确认：`cargo test -p poise-server state_bootstrap::tests::`
- 额外确认：`cargo test -p poise-server --test startup_preparation`
- 检查：`git diff --check`

**Commit SHA：** `1b7fa97`

## Task 4：让 engine 构造入口接收 TrackDefinition

**目的：** 先把 engine 的 track 创建边界改成接收静态定义，减少 `add_track` / `TrackRuntime::new` 的参数散落，但暂不强行折叠 `TrackRuntime` 内部字段。

**文件：**

- `engine/src/manager.rs`
- `engine/src/runtime.rs`
- `application/src/mutation_executor.rs`
- `server/src/assembly.rs`
- `server/src/test_support.rs`
- server runtime 测试 fixture

**步骤：**

- [ ] 将 `TrackManager::add_track` / `add_track_with_tick_timeout_secs` 改为接收 `TrackDefinition` 和 `ExchangeRules`。
- [ ] 将 `TrackRuntime::new` / `with_tick_timeout_secs` 改为接收 `TrackDefinition`。
- [ ] 保持 `TrackRuntime` 的 `id()`、`instrument()`、`config()`、`max_notional()`、`loss_limits()` 等方法，避免调用方了解 definition 内部结构。
- [ ] 更新 server/application 的装配和测试 fixture，避免继续散传 definition 字段。

**最小验收命令：**

- `cargo test -p poise-engine manager::tests::`
- `cargo test -p poise-engine runtime::tests::`
- `cargo test -p poise-application mutation_executor::tests::`
- `cargo test -p poise-server assembly::tests::runtime_state_exposes_observation_and_account_paths_only`

**Commit SHA：** 待执行后回写

## Task 5：让 TrackRuntime 内部持有 TrackDefinition

**目的：** 在构造边界稳定后，再把 `TrackRuntime` 内部重复展开的静态字段折叠为一个 definition 字段，明确 runtime = static definition + dynamic state。

**文件：**

- `engine/src/runtime.rs`
- `engine/src/manager.rs`
- `engine/src/reconciler.rs`
- `engine/src/executor/*`
- engine 测试 fixture

**步骤：**

- [ ] 将 `TrackRuntime` 的 `id`、`instrument`、`config`、`max_notional`、`loss_limits` 折叠为 `definition: TrackDefinition`。
- [ ] 为高频计算保留 `TrackRuntime` 方法，不让调用方写 `track.definition.track_config`。
- [ ] 更新 reconciler / manager / runtime 内部直接字段访问。
- [ ] 对测试中直接修改 `track.config`、`track.max_notional`、`track.loss_limits` 的 fixture 增加专门 builder/helper，避免生产结构为测试暴露可变字段。

**最小验收命令：**

- `cargo test -p poise-engine reconciler::tests::`
- `cargo test -p poise-engine manager::tests::`
- `cargo test -p poise-engine runtime::tests::`
- `cargo test -p poise-engine executor::tests::`

**Commit SHA：** 待执行后回写

## Task 6：清理旧命名和文档

**目的：** 删除旧概念描述，避免后续继续按 `Configured` / `Prepared` / engine-owned definition 的心智模型开发。

**文件：**

- `docs/superpowers/specs/2026-04-09-track-definition-runtime-boundary-design.md`
- `docs/superpowers/plans/2026-04-09-track-definition-runtime-boundary.md`
- `docs/superpowers/specs/2026-04-18-runtime-startup-bootstrap-boundary-design.md`
- `docs/superpowers/plans/2026-04-18-runtime-startup-bootstrap-boundary.md`
- 本计划文件

**步骤：**

- [ ] 更新仍有参考价值的设计文档，把 `ConfiguredTrackDefinition` / `TrackPreparedDefinition` 替换为新的分层模型。
- [ ] 不机械改早期历史记录；只更新仍被当作当前设计依据的文档。
- [ ] 搜索确认生产代码中不再有 `ConfiguredTrackDefinition`、`ConfiguredTrackInput`、`PreparedTrackRegistry`、`TrackPreparedDefinition`。
- [ ] 回写 Task 1-5 的 commit SHA。

**最小验收命令：**

- `rg -n "ConfiguredTrackDefinition|ConfiguredTrackInput|PreparedTrackRegistry|TrackPreparedDefinition" application/src core/src engine/src server/src storage/src`
- `cargo test -p poise-core track::tests::`
- `cargo test -p poise-application track_definition::tests::`
- `cargo test -p poise-server config::tests::`

**Commit SHA：** 待执行后回写

## 并行性

- Task 1 可以独立执行。
- Task 2 必须在 Task 3 前完成。
- Task 3 必须在 Task 4 前完成。
- Task 4 必须在 Task 5 前完成。
- Task 6 的文档盘点可以先并行做只读分析，但实际落文档应在 Task 3-5 命名稳定后完成。

## 设计检查点

- 如果迁移过程中出现新增 `TrackDefinitionInput` 的冲动，先确认是否已经有第二个非 server 构造入口；否则不要引入。
- 如果 `TrackRuntime` 折叠 definition 后导致大量 `track.definition.*` 泄漏，停止并补 runtime 语义方法。
- 如果迁移过程中有人试图删除 `Instrument.venue` 或改成裸 `Symbol`，停止；这不属于本计划。
- 如果某个类型只是在 application 和 core 间转发字段，优先删除，不新增 wrapper。
- 如果迁移要求 protocol 或 storage schema 改名，必须单独确认；本计划默认不碰外部协议和持久化 schema。
