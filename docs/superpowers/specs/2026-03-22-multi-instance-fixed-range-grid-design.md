# 多标的固定区间网格设计

本文档定义 `service` 与 `tui` 的下一阶段演进方案：服务端通过单个环境配置文件启动多标的网格实例，客户端新增实例列表与切换能力，其余详情页继续保持当前单实例视角。

本方案面向“固定区间中性网格”这一明确目标，不引入自动搬移区间、同标的多策略叠加、TUI 创建网格等额外能力。

## 1. 目标

- 让 `service` 在单个进程内启动多个标的实例
- 每个标的实例使用固定区间网格配置
- 配置入口改为“一环境一 TOML 文件”
- `tui` 增加实例列表与实例切换能力
- `Dashboard / Grid / Market / Events` 继续保持单实例详情页模式
- 保持每个标的的运行态、订单、风控、持久化彼此隔离

## 2. 非目标

本次明确不做：

- 同一标的同时运行多套网格
- 自动搬移区间或自动重建区间
- 在 `tui` 中创建、编辑或删除网格
- 一个页面同时并排展示多个实例
- 把现有内核直接改造成“单个大聚合状态里包含全部标的”
- 把 SQLite 路径、内部数量参数暴露给用户配置

## 3. 当前现状与约束

当前系统仍然是单实例模型：

- `RuntimeSnapshot` 只有一份 `runtime / execution / risk / strategy`，见 [`service/src/protocol.rs`](../../../../service/src/protocol.rs)
- 当前网格参数是内部实现视角的 `spacing_bps / levels_per_side / quantity_per_level / max_position_qty / rebuild_threshold_bps`
- 当前网格层生成逻辑是以中心价为轴的对称买卖层，见 [`service/src/strategy.rs`](../../../../service/src/strategy.rs)
- 当前控制面命令没有实例目标字段
- 当前 `tui` 的 `Grid` 视图直接读取单个 `state.strategy`，见 [`tui/src/selectors.rs`](../../../../tui/src/selectors.rs)

用户目标已经明确为：

- 启动可配，而不是先做交互式建网格
- 一个环境使用一个配置文件
- 用户按标的理解实例，不填写额外 `instance_id`
- 网格是固定区间中性网格
- 启动时价格若在区间外，实例进入等待态；价格回到区间后自动开始运行

这些约束意味着：不能继续简单复用“中心对称网格”的用户语义，而需要引入“固定区间梯子”策略模型。

## 4. 用户配置模型

### 4.1 配置文件入口

每个环境一个 TOML 文件，例如：

- `configs/testnet.toml`
- `configs/mainnet.toml`

建议通过 `service` CLI 新增 `--config <path>` 指定配置文件。第一版可继续保留当前无配置文件的单实例启动路径，用于兼容现有本地验证与回归测试。

### 4.2 配置样例

```toml
environment = "testnet"
default_symbol = "BTCUSDT"

[[instances]]
symbol = "BTCUSDT"

[instances.range]
lower_price = 90000
upper_price = 110000
grid_levels = 6
max_position_notional = 3000

[[instances]]
symbol = "ETHUSDT"

[instances.range]
lower_price = 2200
upper_price = 2800
grid_levels = 8
max_position_notional = 4000
```

### 4.3 字段语义

- `environment`
  - 当前文件对应的运行环境，例如 `testnet` 或 `mainnet`
  - 用于展示、配置自校验、默认数据目录隔离
- `default_symbol`
  - `tui` 默认选中的标的
  - 可选；未填写时回退到第一个 `instances[].symbol`
- `instances[].symbol`
  - 标的唯一键
  - 用户配置文件中不再出现 `instance_id`
  - 服务内部以规范化后的 `symbol` 作为实例注册键
- `instances[].range.lower_price`
  - 固定区间下边界
- `instances[].range.upper_price`
  - 固定区间上边界
- `instances[].range.grid_levels`
  - 整个区间的总价格档位数，包含上下边界
  - 不要求偶数
- `instances[].range.max_position_notional`
  - 单边满仓允许的最大名义金额
  - 第一版按区间中点价换算为最大持仓数量

### 4.4 不暴露给用户的内容

以下内容属于实现细节，第一版不出现在配置文件中：

- `instance_id`
- `db_path`
- `spacing_bps`
- `quantity_per_level`
- `levels_per_side`
- `max_position_qty`
- `rebuild_threshold_bps`

### 4.5 默认数据目录

每个实例的 SQLite 路径由服务自动推导，不要求用户填写。建议规则为：

- `.data/<environment>/<symbol-lowercase>.db`

示例：

- `.data/testnet/btcusdt.db`
- `.data/testnet/ethusdt.db`

## 5. 服务端运行模型

### 5.1 多实例注册表

第一版不直接把现有内核改造成多标的大聚合状态，而是在进程外层新增注册表，例如 `ApplicationRegistry`。其职责为：

- 读取并校验配置文件
- 为每个 `symbol` 创建独立的 `Application`
- 为每个实例准备独立的存储与恢复上下文
- 按 `symbol` 路由 HTTP、WebSocket 和命令
- 汇总实例列表供 `tui` 展示

每个实例内部继续保留现有单实例边界：

- 一份 `RuntimeSnapshot`
- 一套 `engine / read model / event stream`
- 一套订单视图、风控视图与系统事件
- 一份独立的 SQLite

### 5.2 Binance 接入边界

第一版配置文件只负责实例与网格参数，不承载 API Key、Secret 等敏感信息。共享账号凭证与可选的 Binance endpoint override 仍沿用现有服务级配置方式。

当服务在配置文件模式下启动时：

- `environment` 取自配置文件
- `symbol` 取自实例配置
- 凭证仍来自服务级环境变量或等价现有入口

### 5.3 单实例与多实例兼容

第一版建议保留两条启动路径：

- 现有单实例启动路径：继续兼容当前本地回归与最小使用方式
- 新配置文件启动路径：主线方案，支持多标的实例

新 `tui` 统一基于实例列表接口工作。即使服务只启动一个实例，`tui` 也仍按“列表里只有一个实例”的方式工作。

## 6. 固定区间梯子模型

### 6.1 核心语义

固定区间梯子与当前“中心对称网格”不同：

- 用户配置的是固定区间，而不是中心价与单边层数
- `grid_levels` 表示整个区间的总价格档位数
- 启动时实际激活的买卖档位数量，取决于当前价格落在区间中的位置

中性网格在这里表示“配置区间本身不带方向偏置”，不表示“启动瞬间买卖挂单数量必须对称”。

### 6.2 价格档位生成

给定：

- `lower_price`
- `upper_price`
- `grid_levels`

档位生成公式为：

```text
step = (upper_price - lower_price) / (grid_levels - 1)
price_i = lower_price + step * i
```

其中 `i` 取值范围为 `0..grid_levels-1`。

示例：

- `lower_price = 90`
- `upper_price = 110`
- `grid_levels = 6`

得到的价格档位为：

- `90`
- `94`
- `98`
- `102`
- `106`
- `110`

### 6.3 挂单方向判定

给定当前市场价 `mark_price` 或等价首选价格后，按如下规则生成活动档位：

- 档位价格严格低于当前价：目标方向为 `buy`
- 档位价格严格高于当前价：目标方向为 `sell`
- 档位价格等于当前价：该档位不挂单

这样可以避免服务启动瞬间出现贴价单或跨价单。

### 6.4 数量换算

第一版按区间中点价换算数量：

```text
midpoint_price = (lower_price + upper_price) / 2
max_position_qty = max_position_notional / midpoint_price
quantity_per_level = max_position_qty / (grid_levels - 1)
```

使用 `grid_levels - 1` 作为分母的原因是：在极端情况下，当前价可能靠近上边界或下边界，此时单边最多可能累计成交 `grid_levels - 1` 个档位。

### 6.5 状态模型

建议保留 `runtime.strategy_state` 作为运维生命周期字段，例如：

- `running`
- `paused`

同时重定义 `strategy.status` 为梯子运行状态：

- `waiting_market_price`
- `waiting_range_entry`
- `active`
- `occupied`

并新增：

- `strategy.status_reason: Option<String>`

用于表达等待原因，例如：

- “等待首个市场价格”
- “当前价格 112000.00 高于上边界 110000.00”

状态优先级建议如下：

- 只要当前实例已有库存，占用态优先显示为 `occupied`
- `waiting_range_entry` 仅用于“当前无库存且价格在区间外”
- 当实例已占仓但价格跑出区间时，仍显示 `occupied`，同时通过 `status_reason` 说明“当前价格已出区间，已停止新增挂单”

### 6.6 启动与区间内外行为

第一版运行规则：

- 无市场价：进入 `waiting_market_price`
- 有市场价但在区间外且无库存：进入 `waiting_range_entry`
- 价格处于区间内时，边界按“包含上下边界”处理
- 价格进入区间且实例处于 `running`：自动开始挂网格
- 价格再次跑出区间：取消该实例的全部策略挂单；若无库存则进入 `waiting_range_entry`，若有库存则保持 `occupied`
- 第一版不自动搬移区间

### 6.7 风控与库存占用

第一版仍然保留现有风控闭环、`pause / resume / cancel-all / flatten` 等运维能力，但要改成实例作用域。

`occupied` 语义继续表示：

- 当前实例已经持有由网格成交形成的库存
- TUI 网格页仍可展示哪些档位已变成库存占用态

## 7. 协议与控制面设计

### 7.1 实例列表接口

新增：

- `GET /instances`

返回内容建议包含：

- `environment`
- `default_symbol`
- `items[]`

`items[]` 每项建议至少包含：

- `symbol`
- `operator_state`
- `strategy_status`
- `status_reason`
- `last_price`
- `lower_price`
- `upper_price`
- `risk_level`
- `unacked_alerts`
- `pending_commands`

第一版实例列表通过 HTTP 拉取，不额外提供多实例聚合 WebSocket。

### 7.2 实例作用域接口

现有单实例接口扩展为按 `symbol` 作用域访问：

- `GET /instances/{symbol}/runtime/snapshot`
- `GET /instances/{symbol}/orders/open`
- `GET /instances/{symbol}/fills/recent`
- `GET /instances/{symbol}/risk/events`
- `GET /instances/{symbol}/system/events`
- `GET /instances/{symbol}/query/runtime`
- `GET /instances/{symbol}/query/orders`
- `GET /instances/{symbol}/query/fills`
- `GET /instances/{symbol}/query/alerts`
- `GET /instances/{symbol}/query/commands`
- `POST /instances/{symbol}/commands/pause`
- `POST /instances/{symbol}/commands/resume`
- `POST /instances/{symbol}/commands/cancel-all`
- `POST /instances/{symbol}/commands/flatten-now`
- `POST /instances/{symbol}/commands/shutdown-after-flatten`
- `GET /instances/{symbol}/ws`

### 7.3 兼容策略

为降低现有本地脚本与 README 迁移成本，第一版可保留现有顶层单实例接口，作为 `default_symbol` 的兼容别名。

约束如下：

- 新 `tui` 不再使用旧接口
- 旧接口仅作为兼容入口
- 当服务运行在多实例模式时，旧接口始终指向 `default_symbol`

### 7.4 符号规范

第一版建议：

- 配置文件中的 `symbol` 使用交易所标准大写形式
- 服务内部以规范化后的大写 `symbol` 作为实例键
- `tui` 不自行拼接符号值，而是复用 `/instances` 返回的 `symbol`

## 8. TUI 设计

### 8.1 总体原则

`tui` 的目标不是改成多实例总控面板，而是在保持现有详情页结构不变的前提下，增加“实例列表 + 当前实例切换”。

### 8.2 启动流程

新的启动流程建议为：

1. 拉取 `GET /instances`
2. 选择 `default_symbol`，若为空则取列表第一个
3. 拉取 `GET /instances/{symbol}/runtime/snapshot`
4. 连接 `GET /instances/{symbol}/ws`
5. 进入现有 `Dashboard / Grid / Market / Events` 详情页

### 8.3 切换流程

用户切换实例时：

1. 关闭当前实例的 WebSocket
2. 拉取新实例的 `runtime/snapshot`
3. 建立新实例的 `ws`
4. 用新实例快照覆盖本地状态
5. 保持当前 page 不变，仅切换数据源

### 8.4 实例列表交互

第一版建议新增一个轻量列表面板或模态：

- 快捷键：`i`
- 内容：`symbol`、运行状态、区间摘要、风险等级、未确认告警数
- `Enter`：切换到选中实例
- `Esc`：关闭列表

实例列表在以下时机刷新即可：

- `tui` 启动时
- 用户打开实例列表时
- 用户手动切换完成后

第一版不要求实例列表实时跟随全部实例状态变化。

### 8.5 详情页保持不变

以下页面继续沿用当前单实例布局与渲染逻辑：

- `Dashboard`
- `Grid`
- `Market`
- `Events`
- `Help`

必要改动仅限于：

- header 增加当前 `symbol`
- help/footer 增加实例切换快捷键说明
- 等待态文案支持显示“等待首个价格”或“等待进入区间”

## 9. 配置校验与错误处理

### 9.1 文件级校验

- `environment` 必填
- `instances` 至少包含 1 个实例
- 若填写 `default_symbol`，必须存在于 `instances`

### 9.2 实例级校验

- `symbol` 不能为空
- 同一文件内 `symbol` 必须唯一
- `lower_price > 0`
- `upper_price > lower_price`
- `grid_levels >= 2`
- `max_position_notional > 0`

### 9.3 启动失败策略

第一版不做“部分实例成功、部分实例失败”。配置文件只要有一个实例校验失败，整个服务启动失败并给出清晰错误信息。

### 9.4 等待态不是错误

以下情况不应视为启动失败：

- 尚未收到首个市场价格
- 当前价格暂时不在配置区间内

它们应体现在实例状态中，而不是进程退出。

## 10. 测试设计

本项目要求测试先行，因此第一步应先补验收测试，再实现功能。

### 10.1 服务端验收测试

至少覆盖以下场景：

- 配置文件解析成功
- 配置文件字段非法时启动失败
- 固定区间梯子价格生成正确
- 启动时无行情进入 `waiting_market_price`
- 启动时价格在区间外进入 `waiting_range_entry`
- 价格进入区间后自动开始挂单
- 价格跑出区间后自动撤掉不应存在的挂单并回到等待态
- `/instances` 返回多实例摘要
- `/instances/{symbol}/...` 路由命中正确实例
- 多实例状态、订单、风险、持久化互不串扰
- `pause / resume / cancel-all / flatten` 在实例作用域内生效

### 10.2 TUI 验收测试

至少覆盖以下场景：

- 启动先拉实例列表，再拉默认实例快照
- `default_symbol` 缺省时回退到第一个实例
- 切换实例时会重拉快照并重建对应 WebSocket
- 切换后 `Dashboard / Grid / Market / Events` 正确显示新实例内容
- 实例列表面板能展示摘要并完成切换
- 等待态页面和网格页文案正确

### 10.3 快照与回归

建议为以下内容补快照或等价回归：

- 实例列表面板
- 等待首个价格状态
- 等待进入区间状态
- 切换实例后的 header / footer 提示

最终验收要求：

- `cargo test -p grid-platform-service` 通过
- `cargo test -p grid-platform-tui` 通过
- `cargo test` 通过

## 11. 实施顺序建议

建议按以下顺序实现：

1. 新增配置文件模型与解析校验测试
2. 在 `service` 中引入多实例注册表
3. 实现固定区间梯子模型与对应测试
4. 接入实例列表与实例作用域 HTTP / WebSocket 路由
5. 改造 `tui` 启动流程与实例切换流程
6. 补实例列表、等待态与切换回归测试
7. 更新 [`README.md`](../../../../README.md) 与相关运行文档
8. 运行全量测试并同步任务清单

## 12. 风险与控制

### 12.1 策略模型切换风险

从“中心对称网格”切到“固定区间梯子”是这次最大的语义变化。

控制方式：

- 先写策略验收测试
- 用明确的档位生成公式和等待态规则锁定行为
- 第一版不引入自动搬移区间

### 12.2 路由与状态隔离风险

多实例最容易出现“命令发到了错误实例”或“读到了别的实例数据”。

控制方式：

- 所有新接口显式带 `symbol`
- 注册表内部只允许通过 `symbol` 命中实例
- 为多实例隔离场景补专门回归测试

### 12.3 TUI 复杂度膨胀

若把实例列表也做成实时多实例监控，会显著扩大范围。

控制方式：

- 第一版实例列表只做 HTTP 刷新，不做聚合订阅
- 详情页继续维持单实例模型

## 13. 结论

本方案采用“一环境一配置文件 + 服务端多实例注册表 + 固定区间梯子 + TUI 实例列表切换”的组合设计。它直接对应用户的配置心智，并最大化复用当前单实例详情页与现有服务端边界。第一版只解决固定区间中性网格的多标的启动与查看问题，不引入自动搬移区间、同标的多策略叠加或 TUI 写入口，从而把实现范围控制在可验证、可回归的尺度内。
