# State Bootstrap Boundary 设计

## 背景

当前 `--rebuild-state` 已经能满足用户语义：

- 默认启动时，如果本地快照和当前配置不一致，服务拒绝启动
- 加 `--rebuild-state` 时，备份旧 SQLite，丢弃旧本地快照，再按“交易所真实仓位与挂单 + 新配置”重建本地状态

但是这套实现有两个结构问题：

1. `server/src/assembly.rs` 同时承担了平台装配和状态库生命周期管理
2. `assemble(config, rebuild_state: bool)` 暴露了语义过薄的布尔参数

这会把“状态接管策略”的知识继续堆进装配层，后续如果再增加别的启动策略，复杂度会继续扩散。

## 目标

- 保留现有 `--rebuild-state` 的用户语义
- 让平台装配重新只负责装配
- 让“状态接管 / 状态重建”成为单独模块的职责
- 去掉裸 `bool` 参数，改成显式启动策略

## 非目标

- 不改变 `--rebuild-state` 的外部行为
- 不引入新的状态后端抽象
- 不支持只重建单个 `track`
- 不修改 runtime 启动后接管交易所真实状态的主流程

## 当前问题

### 1. 特殊运维流程混入通用装配

当前 `server/src/assembly.rs` 既负责：

- 校验 config
- 创建交易所 adapter
- 组装 `ServerPlatform`

又负责：

- 打开 SQLite
- 检查 persisted snapshot 与 config 是否匹配
- 备份旧库
- 删除 `-wal` / `-shm`
- 重建 repository

这让装配层知道了过多与 SQLite 生命周期相关的知识。

### 2. 启动接口语义过薄

`assemble(config, rebuild_state: bool)` 需要调用方记住：

- `false` 表示严格模式
- `true` 表示检测到不一致时备份旧库并重建本地状态

这类 uncommon case 不应该通过裸布尔传播。

## 备选方案

### 方案 A：只把 `bool` 改成 enum

- 把 `assemble(config, bool)` 改成 `assemble(config, StateBootstrapMode)`
- 其余逻辑仍保留在 `server/src/assembly.rs`

优点：

- 改动最小
- 调用点语义更清楚

缺点：

- 主要耦合问题仍在
- `assembly` 继续承担状态库生命周期细节

### 方案 B：拆出状态启动模块，并把接口改成显式策略

- 新增 `server/src/state_bootstrap.rs`
- 由该模块负责状态库检查、备份、重建
- `main.rs` 只解析 CLI，产出显式启动策略
- `assembly.rs` 只接收已准备好的 repository，负责平台装配

优点：

- 状态接管知识单点归属
- `assembly` 回到单一职责
- uncommon case 被收敛到显式模块和显式类型

缺点：

- 比方案 A 多一个模块

### 方案 C：进一步抽象出状态后端 service

- 在方案 B 基础上，再抽象统一状态后端接口

优点：

- 长期扩展空间更大

缺点：

- 当前需求不足以支撑
- 容易过度设计

## 结论

采用 **方案 B：拆出状态启动模块，并把接口改成显式策略**。

## 设计

### 启动策略类型

在启动入口使用显式类型，例如：

- `StateBootstrapMode::Strict`
- `StateBootstrapMode::Rebuild`

语义定义：

- `Strict`
  - 如果 persisted snapshot 与当前 config 不一致，直接报错退出
- `Rebuild`
  - 如果不一致，先备份旧 SQLite，再删除旧本地快照，用新库继续启动

## 模块边界

### `server/src/main.rs`

职责：

- 解析 `--config`
- 解析 `--rebuild-state`
- 把 CLI 选项映射到 `StateBootstrapMode`
- 调用状态启动模块准备 repository
- 将 repository 交给装配层

不负责：

- 直接操作 SQLite 文件
- 拼装 mismatch 细节
- 平台内部装配

### `server/src/state_bootstrap.rs`

职责：

- 根据 `Config` 计算默认 SQLite 路径
- 打开本地 `SqliteStorage`
- 检查 persisted snapshot 与当前 config / instrument 是否匹配
- 在 `Rebuild` 模式下备份旧库并重建
- 在 `Strict` 模式下返回结构化 mismatch 错误

它拥有以下知识：

- 什么叫“状态不一致”
- 如何安全备份 SQLite 主文件和 sidecar 文件

不负责：

- 交易所 adapter 创建
- runtime 组装
- HTTP / WS 启动
- CLI 文案渲染

### `server/src/assembly.rs`

职责：

- 校验 track 唯一性
- 校验运行环境与交易所凭证
- 创建交易所 adapter
- 把 exchange、market data、repository、clock 组装为 `ServerPlatform`

不再负责：

- 选择状态启动策略
- 备份或删除 SQLite 文件
- 检查 snapshot / config mismatch

## 接口形状

建议把接口收成两层：

1. `state_bootstrap::prepare_state_repository(config, mode) -> Arc<dyn StateStore>`
2. `assembly::assemble(config, repository) -> ServerPlatform`

其中：

- `StateStore`
  - 是组合接口，表达“可用于 server 启动装配的状态仓库”
  - 覆盖当前真正需要的能力：`StateRepositoryPort + TrackReadRepositoryPort`
  - 当前实现仍然可以由 `SqliteStorage` 提供
  - 但 `SqliteStorage` 这个落地类型不再泄露到 `main` 和 `assembly`

这样 common path 很直接：

- `main` 负责把“用户要怎么启动”翻译成策略
- `state_bootstrap` 负责把“该用哪份本地状态”准备好
- `assembly` 只关心“已有 repository，开始装配平台”

这样还能保证：

- `state_bootstrap` 拥有“如何准备本地状态仓库”的知识
- `assembly` 不需要知道当前仓库的具体落地类型
- 后续如果状态准备过程调整，不会把 `SqliteStorage` 细节继续扩散到更多调用点

之所以不用 `PreparedStateRepository` 包装类型，是因为如果它只包一层 trait object，而不增加新的行为或约束，就会退化成浅包装。这里直接用有语义的组合接口更直接，也更不容易长成 pass-through 模块。

## 错误语义

在 `Strict` 模式下，如果发现 mismatch，`state_bootstrap` 应返回结构化错误，例如：

- `db_path`
- `mismatches`
- `suggested_action`

其中每个 mismatch 至少包含：

- `track_id`
- expected config
- persisted config
- instrument 差异（如有）

CLI 层再把这个结构化错误渲染成用户提示，例如包含：

- SQLite 路径
- 差异详情
- 建议命令：`--rebuild-state`

这样状态规则和展示文案不会耦合在一个模块里；如果以后 TUI、HTTP 或自动化脚本也要复用这套检查，仍然可以消费同一份结构化结果。

## `--rebuild-state` 的运行语义

`--rebuild-state` 不表示“完全从零开始”，而是：

1. 放弃旧的本地快照
2. 保留交易所当前真实状态作为事实来源
3. 按新配置重新建立本地运行时
4. 启动阶段再通过现有 `startup_sync` 接管真实持仓和挂单

也就是：

- 接管旧仓位
- 不继承旧本地策略快照
- 由新配置重新解释交易所当前状态

## 测试策略

### `server/src/main.rs`

- 解析 `--config`
- 解析 `--rebuild-state`
- 拒绝未知参数

### `server/src/state_bootstrap.rs`

- 严格模式下，无 mismatch 时返回 repository
- 严格模式下，有 mismatch 时返回结构化错误
- 重建模式下，有 mismatch 时会备份旧库并创建新库
- 重建时删除 `-wal` / `-shm`

### `server/src/assembly.rs`

- 不再测试 SQLite 文件生命周期细节
- 只测试装配本身：exchange、抽象 repository、runtime 组装是否正确

## 预期结果

完成后，启动链路会变成：

1. `main.rs` 解析 CLI
2. `state_bootstrap.rs` 准备状态仓库
3. `assembly.rs` 组装平台
4. `runtime.start()` 接管交易所真实状态

这样复杂度会集中在正确的层：

- 启动策略复杂度留在状态启动模块
- 平台装配复杂度留在装配层
- runtime 恢复复杂度继续留在 runtime / write side

## 设计复评

按 `software-design-philosophy-review` 复看，这个方案的关键改进是：

- 去掉了 `assembly` 中的 `special-general mixture`
- 避免用裸布尔暴露 uncommon case
- 让“状态接管策略”拥有单一知识归属

当前没有看到更明显的复杂度泄漏点。只要实现时保持 `state_bootstrap.rs` 不继续吸收 CLI 解析和 runtime 装配职责，这个边界会比现状稳定得多。
