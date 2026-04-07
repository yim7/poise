# Instance Dir 隔离设计

## 背景

当前运行态把多个本应按实例隔离的东西隐式挂在仓库当前工作目录下：

- SQLite 默认路径是 `.data/<environment>/poise-server.sqlite`
- 脚本日志默认路径是 `.logs/...`
- 本地真实配置通常也放在仓库 `configs/` 下

这套做法在“单账号、单实例、单目录”时勉强可用，但一旦出现下面这些场景，就会立刻暴露结构问题：

- 同一个仓库目录下同时跑多个 Binance 账号
- 同一个 `environment` 下同时跑多套 mainnet / testnet 配置
- 某个实例使用 `--rebuild-state` 时误伤另一个实例的本地状态
- 真实配置文件散落在仓库里，难以判断某份配置到底对应哪个数据库和日志

根因不是 SQLite 不能存多个 `track`。根因是“实例边界”没有成为一等概念，数据库、日志、配置和服务地址知识被拆散在工作目录、环境变量和脚本默认值里。

## 目标

- 引入显式实例目录 `instance dir`，让每个运行实例拥有稳定且独立的本地边界
- 让一个实例的配置、数据库、日志和备份天然归属于同一个目录
- 让 `--rebuild-state` 的作用域严格收敛到当前实例
- 删除按调用方命名的环境变量，只保留实例级服务地址语义
- 删除 `paper` 这层旧脚本命名，避免把实例模型和历史本地联调命名混在一起
- 让“多账号、多实例并行运行”成为默认可安全操作的路径

## 非目标

- 不改变现有 `track` 业务语义
- 不引入账号级数据库 schema 变更
- 不支持一个实例目录承载多个配置文件
- 不保留当前旧环境变量和旧路径约定的兼容层
- 不把源码工作区复制成多个运行副本作为正式方案

## 当前问题

### 1. 运行产物隐式依赖仓库工作目录

数据库路径当前由 `Config::default_db_path()` 按相对路径推导：

- `.data/<environment>/poise-server.sqlite`

这意味着两个 mainnet 账号只要从同一个仓库目录启动，就会默认打到同一个 SQLite。

### 2. 配置与数据库、日志没有显式归属关系

当前真实配置通常放在仓库 `configs/*.local.toml` 中，但数据库和日志路径由别的规则决定。调用方需要同时记住：

- 当前用了哪份配置
- 当前从哪个目录启动
- 当前 `.data` 和 `.logs` 实际落在哪

这会带来明显的认知负担，也让误操作很难避免。

### 3. 环境变量语义被调用方污染

当前存在：

- `POISE_BASE_URL`
- `POISE_HEALTH_BASE_URL`
- `POISE_WS_URL`
- `POISE_TUI_WS_URL`

这些名字描述的是“谁在使用地址”，而不是“实例暴露什么地址”。对用户真正需要记住的实例语义来说，这是多余复杂度。

### 4. `paper` 旧命名已经失真

当前存在：

- `run-paper-server.sh`
- `run-paper-tui.sh`
- `start-paper-zellij.sh`
- `.logs/paper`
- `poise-paper` session

但运行时真正支持的环境是：

- `testnet`
- `mainnet`

这里的 `paper` 已经不是运行模式，只是历史联调脚本遗留命名。继续保留只会制造误解，尤其是在引入实例目录之后。

## 备选方案

### 方案 A：继续依赖不同工作目录或多个 git worktree

做法：

- 不改程序接口
- 依赖调用方用不同目录启动不同实例

优点：

- 实现最少

问题：

- 隔离规则仍然是隐式知识
- 手工 `cargo run`、脚本、自动化入口容易回到共享目录
- 把“实例隔离”错误地绑定到“代码副本隔离”

结论：不采用。

### 方案 B：只增加 `runtime dir`

做法：

- 增加显式运行目录
- 数据库和日志从该目录派生
- 配置文件仍然放在仓库 `configs/`

优点：

- 比当前方案清楚
- 可以先解决数据库和日志串用问题

问题：

- 配置与运行产物仍然分离
- 仍然需要额外记忆“这份配置对应哪个 runtime dir”

结论：不采用。

### 方案 C：引入 `instance dir`，每个实例目录只承载一个配置和全部本地运行产物

做法：

- 引入显式 `instance dir`
- 配置固定为 `<instance_dir>/config.toml`
- 数据库、日志、重建备份全部从 `instance dir` 派生
- 实例服务地址环境变量统一为 `POISE_BASE_URL`
- 脚本、日志目录和 session 名称去掉 `paper` 命名

优点：

- 实例边界单点归属
- 用户不再需要记忆路径组合规则
- `--rebuild-state` 的影响范围天然正确

问题：

- 需要调整 CLI、脚本和文档

结论：采用。

## 设计结论

正式引入 `instance dir` 作为运行实例根目录。

`instance dir` 的职责是承载某一个 Poise 实例的全部本地状态，不承载源码，不承担代码版本隔离职责。

一个实例目录只允许包含：

- 一个 `config.toml`
- 一个 `.data/`
- 一个 `.logs/`
- 该实例运行过程中生成的备份文件

仓库本身只保留 demo 配置，不再鼓励把真实运行配置长期放在仓库 `configs/` 中。

## 目录结构

建议实例目录结构如下：

```text
~/poise-instances/account-a/
  config.toml
  .data/
    mainnet/
      poise-server.sqlite
  .logs/
    poise-server.log
    poise-tui.log
    health-probe.log

~/poise-instances/account-b/
  config.toml
  .data/
    mainnet/
      poise-server.sqlite
  .logs/
    poise-server.log
    poise-tui.log
    health-probe.log
```

这里的 `mainnet` / `testnet` 仍然保留在 `.data/<environment>/` 下，不是为了表达多实例，而是为了保持环境语义和现有状态重建逻辑一致。

实例隔离由 `instance dir` 提供；环境语义由 `environment` 提供。两者职责不同，不互相替代。

## 接口设计

### Server CLI

`poise-server` 改为显式接收：

- `--instance-dir <path>`

约定：

- 配置文件路径固定为 `<instance-dir>/config.toml`
- 数据库根路径固定从 `<instance-dir>/.data/` 派生
- 所有状态重建备份也固定落在该实例目录内

不再要求调用方同时显式传：

- `--config`
- 数据库路径
- 日志目录

服务端只需要知道实例目录；实例目录内部的标准结构由程序负责解释。

### 配置模型

配置文件仍保留现有 TOML 结构，但其物理位置从仓库 `configs/*.local.toml` 切换为实例目录内的固定文件：

- `<instance-dir>/config.toml`

`Config` 不新增 `db_path` 字段。

原因：

- `db_path` 只是实例目录内部路径布局的一部分
- 如果把它暴露到配置层，会重新把“运行产物落点”知识泄露给调用方
- 当前问题的正确 owner 是实例目录规则，而不是配置字段

### 数据库路径

数据库路径从：

- `.data/<environment>/poise-server.sqlite`

改为：

- `<instance-dir>/.data/<environment>/poise-server.sqlite`

对应重建备份路径也一并移动到该目录下。

### 日志路径

脚本和工具默认日志路径统一为：

- `<instance-dir>/.logs/poise-server.log`
- `<instance-dir>/.logs/poise-tui.log`
- `<instance-dir>/.logs/health-probe.log`

日志目录不再默认写入仓库根下的 `.logs/paper`。

### 环境变量

实例服务地址环境变量统一为：

- `POISE_BASE_URL`

语义：

- 当前实例暴露的 HTTP 服务基地址

删除：

- `POISE_HEALTH_BASE_URL`
- `POISE_WS_URL`
- `POISE_TUI_WS_URL`

原因：

- 这些变量表达的是调用方视角，而不是实例本身的稳定语义
- 当前协议下 WebSocket 路径固定为 `/ws`
- 没有证据表明当前需要把 WebSocket 地址当成独立配置项

TUI 和 health probe 都只接受 `POISE_BASE_URL`。

TUI 内部 WebSocket 地址固定从 `POISE_BASE_URL` 推导：

- `http://host:port` -> `ws://host:port/ws`
- `https://host:port` -> `wss://host:port/ws`

## 模块职责

### `server`

拥有：

- 实例目录解析
- 从实例目录加载 `config.toml`
- 从实例目录派生数据库路径
- 状态重建备份仍然只操作当前实例目录内文件

不拥有：

- 脚本层日志文件命名策略
- 多实例编排策略

### `scripts`

拥有：

- 接收实例目录参数或环境变量
- 把实例目录作为所有脚本的统一运行入口
- 保证日志落到实例目录
- 用仓库代码运行程序，但不再把仓库目录当成运行产物根目录

不拥有：

- 自己推导数据库路径
- 自己解释配置文件结构

### `README` / 运维文档

拥有：

- 明确说明“一个账号一个实例目录”
- 给出实例目录示例结构
- 给出 mainnet / testnet 多实例启动示例

不再暗示：

- 真实配置文件长期放在仓库 `configs/`
- 共享一个仓库目录就是合理运行方式

## 行为语义

### 1. 多账号隔离

两个 mainnet 账号只要使用不同的 `instance dir`，即使：

- 使用相同的 `environment = "mainnet"`
- 使用相同的仓库源码
- 使用相同的二进制

它们的数据库、日志和重建备份也不会互相影响。

### 2. `--rebuild-state`

`--rebuild-state` 的语义不变，但作用域从“当前工作目录下的环境数据库”收敛为：

- 当前 `instance dir` 下该 `environment` 的数据库

这样可以定义掉“另一个账号的本地状态被意外重建”这个错误。

### 3. 手工排查

用户只需要进入实例目录，就能看到：

- 当前配置
- 当前数据库
- 当前日志
- 当前重建备份

排查路径不再跨仓库目录和脚本环境变量来回跳转。

### 4. 脚本入口统一切换

这次切换不是只改 server CLI，再让脚本继续拼旧参数。

脚本要同步切到实例目录模型：

- `scripts/run-paper-server.sh`
- `scripts/run-paper-tui.sh`
- `scripts/probe-health.sh`
- `scripts/start-paper-zellij.sh`

统一规则：

- 脚本接收 `POISE_INSTANCE_DIR`
- server 脚本调用 `poise-server --instance-dir <path>`
- TUI 和 health probe 从实例目录读出统一的实例地址约定
- 日志默认写入 `<instance-dir>/.logs/`
- 脚本名、zellij layout 名和默认 session 名不再带 `paper`
- dry-run 输出也必须体现实例目录，而不是仓库根目录的相对路径

这样调用方不需要再分别知道：

- 配置文件在哪里
- 日志目录在哪里
- 当前数据库会落到哪里
- 各脚本之间该传哪些不同名的地址变量

## 迁移策略

因为项目仍处于探索阶段，这次不提供兼容层。

直接迁移到新规则：

- server CLI 删除旧入口，改用 `--instance-dir`
- 脚本删除旧变量和旧默认值，并统一改为实例目录入口
- 文档改写到实例目录模型

如果用户仍想保留旧仓库内的 local 配置，可以手工把文件移动到实例目录中的 `config.toml`。

旧脚本和旧命名不保留兼容别名，直接替换为实例目录语义的新名称。

## 测试要求

需要新增或调整的验收测试包括：

- `server`：`--instance-dir` 能正确加载 `<instance-dir>/config.toml`
- `server`：数据库路径从实例目录派生，而不是当前仓库目录
- `server`：`--rebuild-state` 只影响当前实例目录下的数据库
- `scripts/run-paper-server.sh`：dry-run 输出 `--instance-dir` 和实例目录内的 server 日志路径
- `scripts/run-paper-tui.sh`：dry-run 输出实例目录内的 TUI 日志路径和统一的 `POISE_BASE_URL`
- `scripts/probe-health.sh`：dry-run 输出实例目录内的 health 日志路径和统一的 `POISE_BASE_URL`
- `scripts/start-paper-zellij.sh`：导出统一的 `POISE_INSTANCE_DIR`、`POISE_BASE_URL`，不再导出旧变量
- `tui`：只从 `POISE_BASE_URL` 推导 WebSocket 地址
- `tui` / 脚本：不存在 `POISE_WS_URL`、`POISE_TUI_WS_URL`、`POISE_HEALTH_BASE_URL` 的依赖残留
- 文档与脚本：不存在 `paper` 旧命名残留，例如脚本名、session 名、日志目录名、layout 文件名

## 风险

### 1. 调用入口一次性变化较大

这次会同时改：

- CLI
- 脚本
- README
- 测试

但这些变化都围绕同一个边界，不是分散需求；集中处理更容易保证语义一致。

### 2. 仍有少量代码默认依赖当前工作目录

需要系统性排查：

- 数据库路径推导
- 日志默认值
- 测试 fixture 中的相对路径

否则会留下局部仍写到仓库根目录的残留行为。

## 总结

这次设计的核心不是“给数据库换个路径”，而是把“运行实例”收敛成一个真正可理解、可迁移、可重建的单位。

采用 `instance dir` 之后：

- `workspace` 只负责源码
- `instance dir` 只负责实例状态
- `POISE_BASE_URL` 成为唯一实例服务地址语义

这样可以在不修改业务规则的前提下，定义掉当前多账号 mainnet 共用本地状态的结构性风险。
