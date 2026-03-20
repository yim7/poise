# TUI 首次快照空态设计

## 背景

当前仓库里有两处会把“示例数据”当成默认启动状态：

- `tui` 启动时先用本地 `sample` 状态渲染首帧
- `service` 默认内存 bootstrap 也会返回一份 `RuntimeSnapshot::sample()`

这会导致系统在没有真实运行态数据时，仍然显示看起来像真实事实的内容，例如：

- 持仓数量与持仓均价
- 未实现盈亏与已实现盈亏
- 两笔 `NEW` 挂单
- 一笔 recent fill

问题不在于“界面太空”，而在于“界面把未知状态伪装成已知事实”。对于运维终端，这种误导比空白更危险。

## 问题定义

当前启动链路同时缺少两种区分：

1. `首次快照尚未拿到`
2. `快照已拿到，但当前业务确实为空`

这两个状态现在都可能被 `sample` 掩盖，导致用户无法判断：

- 当前只是还没同步到服务端
- 服务端当前没有真实数据
- 还是系统已经确认当前为空仓、空挂单、空成交

## 目标

- 默认启动时不再展示任何伪造的持仓、挂单、PnL、成交数据
- 明确区分 `等待首次快照`、`首次快照失败重试中`、`首次快照已就绪`
- 在首次快照未就绪前，保留 TUI 页面框架与连接进度，但禁用运行态操作
- 当快照已拿到且数据确实为空时，界面显示真实空态，而不是等待态
- `RuntimeSnapshot::sample()` 只保留给测试与渲染快照基线，不再进入真实启动链路

## 非目标

- 本次不重做 TUI 页面布局
- 本次不扩展新的控制面接口
- 本次不依赖 WebSocket 增量事件来拼装首次业务状态
- 本次不引入复杂启动向导或整页阻塞式 loading 页面
- 本次不处理“进入 `Ready` 之后再次失联”的完整页面重构，只保持现有退化与重连语义

## 方案选择

本次采用“强状态式空态”方案。

对比过的 3 个方向如下：

### 1. 强状态式空态

- 首次快照未就绪时，顶部明确显示 `WAITING SNAPSHOT` 或 `SNAPSHOT FAILED`
- 业务面板只显示状态文案，不显示任何业务数值
- 运行态操作全部禁用

这是推荐方案，因为它能最明确地区分“未知”和“已知为空”。

### 2. 轻量占位式

- 保留原始表格与数字布局
- 数值统一显示 `--`
- 列表显示空表头和占位行

这个方案实现简单，但信号不够强，容易让用户把“未知”看成“当前为空”。

### 3. 启动阻塞页

- 首次快照未返回前，不进入正常页面
- 单独显示一整页连接与加载状态

这个方案语义很绝对，但太重，会打断现有页签、帮助和面板结构，也不利于运维观察。

## 设计摘要

本次设计分成两层：

1. `service`
   - 默认 bootstrap 运行态改成“已知空态”
   - 不再把 `RuntimeSnapshot::sample()` 用作真实启动数据
2. `tui`
   - 启动后进入 `WaitingFirstSnapshot`
   - 首次快照失败后进入 `SnapshotRetrying`
   - 首次快照成功后进入 `Ready`
   - 只有进入 `Ready` 之后才展示业务数据与开放运行态操作

核心原则只有两条：

- 未知不能显示成事实
- 首次可用业务状态必须由 HTTP 快照建立

## 服务端启动语义

### 1. 默认 bootstrap 改为空业务态

默认内存模式与 SQLite 首次建库模式都不再从 `RuntimeSnapshot::sample()` 派生运行态，而是使用新的空 bootstrap 语义：

- `position_qty = 0.0`
- `position_avg_price = 0.0`
- `unrealized_pnl = 0.0`
- `realized_pnl = 0.0`
- `open_orders = []`
- `recent_fills = []`
- `pending_commands = []`
- `recent_commands = []`

这表示“服务端当前已知没有持仓、没有挂单、没有成交历史”，而不是“给一个像真的例子凑页面”。

### 2. 市场与连接字段保持可观测，但不伪造业务事实

bootstrap 运行态仍可保留系统级元信息，例如：

- `symbol`
- `env`
- `session_state`
- 连接健康相关字段

但不应凭空构造：

- 最新价
- 标记价
- 风险使用量
- 虚构成交与挂单

如果某些字段当前协议层必须返回数值，则其默认值只能表达“当前无已知业务事实”，不能借用示例行情填充。

### 3. `sample` 只保留测试用途

`RuntimeSnapshot::sample()` 与 `AppState::sample()` 仍可存在，但只用于：

- 单元测试
- 渲染快照测试
- 协议测试

真实启动路径不得再依赖这些函数。

## TUI 状态模型

TUI 新增一个首次快照状态枚举：

- `WaitingFirstSnapshot`
- `SnapshotRetrying { last_error, retry_count, retry_in_ms }`
- `Ready`

状态流转如下：

1. 启动进入 `WaitingFirstSnapshot`
2. 发送首次 `/runtime/snapshot` 请求
3. 请求成功，进入 `Ready`
4. 请求失败，进入 `SnapshotRetrying`
5. 等待退避时间后重试
6. 任意一次成功都进入 `Ready`

约束如下：

- 在从未进入过 `Ready` 之前，不能回退到任何 `sample` 或“默认业务态”
- 在 `Ready` 之前，WebSocket 可以连接，但不能单靠增量事件解锁业务渲染
- 首次业务可用态必须由 HTTP 快照建立

## 渲染策略

### 总体原则

- 保留现有页签、面板边框、焦点和底栏结构
- 不保留任何业务数值、订单表头、成交表头或命令时间线项
- 用明确状态文案替代“空表 + 空数字”

### 顶部状态栏

在 `WaitingFirstSnapshot` 与 `SnapshotRetrying` 阶段：

- 仍显示产品名 `GRID PLATFORM`
- 不显示依赖业务快照的运行态摘要，例如 `XAUUSDT testnet running`
- 主状态改为：
  - `WAITING SNAPSHOT`
  - `SNAPSHOT FAILED`
- 次级信息显示连接过程，而不是业务健康，例如：
  - `requesting /runtime/snapshot`
  - `retry in 2s`

### Dashboard 页面

各面板在 `WaitingFirstSnapshot` 阶段显示：

- `Execution Focus`
  - `等待首次快照`
  - `运行态数据尚未初始化`
- `Open Orders`
  - `等待首次快照`
  - `挂单视图尚未初始化`
- `Recent Fills`
  - `等待首次快照`
  - `成交视图尚未初始化`
- `Risk + Alerts`
  - `等待首次快照`
  - `风控视图尚未初始化`
- `Market + Health`
  - `HTTP 正在请求首次快照`
  - `WS 等待首次快照完成后接管增量更新`
- `Command Timeline`
  - `等待首次快照`
  - `命令时间线尚未初始化`

在 `SnapshotRetrying` 阶段：

- 所有面板第一行统一改为 `获取首次快照失败`
- 只有 `Market + Health` 面板展示错误摘要与下一次重试时间
- 其他面板第二行统一显示 `正在等待下一次重试`

### 其他页面

`Grid`、`Market`、`Events` 页面遵循同一原则：

- 保留页面结构
- 不显示任何业务行项目
- 面板正文统一显示“等待首次快照”或“获取首次快照失败”
- 不出现容易被误解成真实空态的表头、计数或数值

`Help` 页面保持可访问，因为它不依赖运行态数据。

### 已进入 `Ready` 后的真实空态

一旦首次快照成功，页面恢复现有正常渲染逻辑。

此时：

- `0`
- 空列表
- `暂无挂单`
- `暂无成交`

都表示服务端已经确认的真实状态，而不再是占位。

## 交互限制

在 `WaitingFirstSnapshot` 与 `SnapshotRetrying` 阶段：

- 允许：
  - 切页
  - 切焦点
  - 打开帮助
  - 退出
- 禁止：
  - `p`
  - `r`
  - `c`
  - `f`
  - `s`

用户触发被禁用操作时：

- 不打开确认框
- 直接显示短 toast

文案如下：

- `WaitingFirstSnapshot`
  - `首次快照未就绪，操作已禁用`
- `SnapshotRetrying`
  - `首次快照获取失败，等待重试后再操作`

`Ready` 后恢复现有操作行为。

## 重试策略

首次快照失败后的重试建议复用现有 WebSocket 重连节奏：

- 第 1 次失败后等待 1 秒
- 第 2 次失败后等待 2 秒
- 第 3 次失败后等待 4 秒
- 后续上限 8 秒

理由：

- 保持现有系统语义一致
- 用户更容易理解状态栏中的 `retry in ...`
- 不需要为 snapshot 再引入第二套退避规则

重试期间：

- 顶部状态栏显示剩余退避时间
- `Market + Health` 显示最近一次错误摘要
- 不把失败信息写入命令时间线
- 不生成伪系统事件来占满事件页面

## 实现边界

本次实现建议限定在以下边界内：

### `service`

- 为真实启动链路提供空 bootstrap 运行态
- 移除 `PersistedRuntime::*_bootstrap()` 对 `RuntimeSnapshot::sample()` 的依赖
- 保留 `sample` 供测试使用

### `tui`

- 增加首次快照状态模型
- 新增空态渲染分支
- 在首次快照未就绪前禁用运行态操作
- 为首次快照失败补充重试 effect 与倒计时展示

不需要新增控制面接口，也不需要修改 `snapshot + incremental events` 总模型。

## 测试策略

遵循“测试先行”原则，先补验收与状态测试，再改实现。

### 1. `tui` 状态流转测试

在 `tui/src/store.rs` 增加测试：

- 启动默认处于 `WaitingFirstSnapshot`
- 首次快照失败后进入 `SnapshotRetrying`
- 重试成功后进入 `Ready`
- `WaitingFirstSnapshot` 与 `SnapshotRetrying` 期间提交运行态命令不会发出 effect
- 被禁用操作会写入正确 toast

### 2. `tui` 渲染快照测试

在 `tui/src/render.rs` 增加快照基线：

- `dashboard_waiting_snapshot`
- `dashboard_snapshot_retrying`
- 视需要补 `market_waiting_snapshot`

重点验证：

- 不出现任何 `sample` 业务数字
- 不出现伪造订单与成交
- 顶部状态栏与空态文案清晰可见

### 3. `service` bootstrap 测试

在 `service` 增加测试覆盖：

- 默认内存 bootstrap 返回空业务态，而不是 `sample`
- SQLite 首次 bootstrap 返回空业务态，而不是 `sample`
- `sample` 仍可被测试调用，但不进入真实启动链路

### 4. 端到端验收

至少覆盖下面两条：

1. 服务端不可用
   - TUI 首屏显示 `WAITING SNAPSHOT`
   - 失败后切到 `SNAPSHOT FAILED`
   - 页面不出现任何持仓、PnL、挂单示例值
2. 服务端可用但为空 bootstrap
   - TUI 首次快照成功后进入 `Ready`
   - 页面显示真实空态，不显示任何示例挂单、成交和盈亏

## 验收标准

- 默认启动不再显示示例持仓、示例 PnL、示例挂单、示例成交
- 首次快照未返回时，页面明确显示等待态
- 首次快照失败时，页面明确显示失败重试态
- 首次快照未就绪前，运行态操作全部禁用
- 首次快照成功后，页面一次性切换到真实运行态
- 当真实快照内容为空时，界面显示真实空态，而不是等待态或示例态

## 风险与取舍

- 如果当前协议中的某些数值字段无法表达“未知”，服务端空 bootstrap 只能返回保守的零值；这要求 TUI 在 `Ready` 态中继续根据上下文区分“真实空态”和“未同步到市场数据”的文案。
- 本次没有引入新的协议字段来描述每个子视图的独立就绪度，目的是先把“首次快照前绝不展示假数据”这条主线落地。
- `sample` 仍会保留在测试中，因此需要明确约束真实启动链路，避免后续回归时再次误用。
