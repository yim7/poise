# TODO

当前 TODO 只跟踪最近 1 到 3 个里程碑的可执行任务。
阶段目标与顺序见 [`docs/roadmap.md`](docs/roadmap.md)，近期策略与验收标准见 [`docs/plan.md`](docs/plan.md)。

## 当前状态

- 当前主线：待确认下一里程碑
- 并行预研：无
- 最近完成：`K7` Web UI 就绪与多实例预备；`K6` 回放 / paper / testnet 验证；`K5` 网格策略与风控；服务端 CLI 已接入 `clap` 并支持 `--help / --version`；TUI `Ctrl-C` 误触发 `cancel-all` 已修复；TUI 服务流与行情流重连状态文案已区分；TUI 订单视图已拆分为“策略订单”和“交易所挂单”，并为执行快照补上 `open_orders_source` 来源语义；Binance testnet 已接入真实 `exchange_open_orders` 启动同步与用户流订单更新
- 最近验证：`cargo fmt --check`、`cargo test -p grid-platform-service`、`cargo test -p grid-platform-tui`、`cargo test` 已通过；订单来源协议兼容、Binance 真实挂单同步、TUI 双轴状态模型，以及本地 E2E service 启动并发超时回归已复核，无新的阻断项

## 首次快照空态验收

- [x] 服务端默认 bootstrap 返回已知空业务态，不再走 `sample`
- [x] TUI 启动默认进入 `WaitingFirstSnapshot`
- [x] 首次快照失败后进入 `SnapshotRetrying` 并自动退避重试
- [x] 首次快照未就绪前禁用 `p / r / c / f / s` 操作并显示正确 toast
- [x] `Help` 页在 bootstrap 期间保持可访问
- [x] 首次快照成功后进入 `Ready`，空 bootstrap 显示真实空态
- [x] 渲染快照已覆盖 waiting / retrying / help bootstrap 页面
- [x] 本地 E2E 已覆盖 waiting 禁用操作与首次成功后的真实空态

## 验收补缺

- [x] 修复 paper 模式在持仓已建立且后续只有行情波动时 `runtime.unrealized_pnl` 不刷新
- [x] 为 paper 模式持仓后的连续行情更新补回归测试
- [x] 修复空价格 replay `market` step 复用旧价导致的幽灵成交
- [x] 统一 reduce-only 成交数量与已实现盈亏口径
- [x] 修复 TUI 将 `Ctrl-C` 误判为 `cancel-all` 的问题
- [x] 为 `Ctrl-C` 退出与带修饰键快捷键补回归测试
- [x] 区分 TUI 服务控制面与行情流的重连状态文案
- [x] 为连接状态标签补回归测试
- [x] 区分 TUI 策略订单与交易所挂单视图
- [x] 为执行快照补 `open_orders_source` 并兼容旧 payload / 旧快照
- [x] 接入 Binance 真实交易所挂单同步到 `exchange_open_orders`
- [x] 修复本地 E2E 并发启动 service 时的 Cargo 构建锁超时

## 已完成里程碑

- [x] `K1` 服务端内核重构
- [x] `K2` 持久化与恢复
- [x] `K3` 市场与账户接入准备

## K4 执行闭环与命令语义做实

### K4.1 测试先补齐

- [x] 为 execution transport 增加 fake adapter 测试
- [x] 为 `pause / resume / cancel-all / flatten-now / shutdown-after-flatten` 增加命令终态测试
- [x] 为执行失败、超时和幂等命中增加内核测试
- [x] 扩展本地 E2E，覆盖撤单、平仓和重连后终态恢复
- [x] 扩展协议测试，覆盖命令结果与订单/成交关联字段

### K4.2 执行适配层与关联模型

- [x] 建立 execution adapter 模块边界
- [x] 明确下单、撤单、查单、成交回报的适配接口
- [x] 定义 `command_id -> client_order_id -> order_id -> trade_id` 关联模型
- [x] 让 open orders、fills 和 recent commands 共用同一执行事实来源

### K4.3 命令真实语义

- [x] 让 `pause` 变成“停止新增策略下单”
- [x] 让 `resume` 变成“恢复策略下单能力”
- [x] 让 `cancel-all` 变成“等待现有挂单全部清空”
- [x] 让 `flatten-now` 变成“提交 reduce-only 平仓并等待仓位归零”
- [x] 让 `shutdown-after-flatten` 建立在 `flatten-now` 完成之后

### K4.4 终态、超时与审计

- [x] 统一 `accepted / ack / failed / timed_out` 的状态流转
- [x] 记录执行失败原因、超时原因和幂等命中原因
- [x] 让持久化审计与 WebSocket 事件使用同一终态语义
- [x] 补充执行重试与重放保护策略

### K4.5 TUI 联动

- [x] 时间线展示真实执行结果与失败原因
- [x] 订单区与成交区能反查对应命令
- [x] 事件页展示执行失败和超时细节
- [x] 为执行闭环后的关键页面补快照验证

### K4.6 收尾与验收

- [x] 跑通 `cargo test`
- [x] 复核 K4 验收标准
- [x] 更新 `docs/plan.md` 与本文件状态
- 验收结论：K4 已验收通过

## K5 网格策略与风控

### K5.1 测试先补齐

- [x] 为网格配置校验增加单元测试
- [x] 为网格状态机和重建逻辑增加状态流转测试
- [x] 为最大仓位、止损、单日亏损和 breaker 增加风险测试
- [x] 为 TUI `Grid` 与风险展示增加快照回归

### K5.2 策略模型

- [x] 定义网格配置 schema
- [x] 定义 `active / occupied / pending rebuild` 状态模型
- [x] 实现网格层生成规则
- [x] 实现网格重建规则
- [x] 把策略输出接到执行层，而不是直接改订单镜像

### K5.3 风控模型

- [x] 接入最大仓位限制
- [x] 接入止损阈值
- [x] 接入单日亏损限制
- [x] 接入 breaker 触发与解除逻辑
- [x] 统一风险事件落盘与广播

### K5.4 TUI 联动

- [x] `Grid` 页面展示策略状态机
- [x] Dashboard 展示核心风险阈值与 breaker 状态
- [x] Events 页面展示风险事件和操作建议
- [x] 为策略/风险联动后的页面补回归快照

### K5.5 收尾与验收

- [x] 跑通 `cargo test`
- [x] 复核 K5 验收标准
- [x] 更新 `docs/plan.md` 与本文件状态
- 验收结论：K5 已验收通过

## K6 回放 / paper / testnet 验证

### K6.1 测试先补齐

- [x] 为 replay runner 增加场景测试
- [x] 为 paper fill 逻辑增加成交模拟测试
- [x] 为 fake service / fake transport 集成链路增加测试
- [x] 为 testnet 下单、撤单、恢复增加最小冒烟清单

### K6.2 回放与 paper

- [x] 设计 replay 输入格式
- [x] 实现 replay runner
- [x] 实现 paper execution 和 fill 模拟规则
- [x] 建立录制或构造的市场事件流夹具

### K6.3 验证链路

- [x] 建立 fake service / fake transport 组合测试夹具
- [x] 跑通本地 replay 闭环
- [x] 跑通 paper 模式命令闭环
- [x] 跑通 testnet 最小执行闭环

### K6.4 运维与验收

- [x] 输出运维与验证手册
- [x] 固化验证命令和检查项
- [x] 跑通 `cargo test`
- [x] 复核 K6 验收标准
- [x] 更新 `docs/plan.md` 与本文件状态
- 验收结论：K6 已验收通过

## K7 Web UI 就绪与多实例预备

### K7.1 查询模型整理

- [x] 梳理 `runtime / orders / fills / alerts / commands` 的 Web 友好查询模型
- [x] 为关键列表设计分页参数
- [x] 为关键列表设计过滤参数
- [x] 为命令与风险事件设计查询排序规则

### K7.2 实例维度与认证边界

- [x] 预留 `instance_id`
- [x] 定义局域网部署模式
- [x] 设计简单认证与 token 边界
- [x] 明确 WebSocket 连接的鉴权策略

### K7.3 Web 客户端准备

- [x] 整理 Web UI 所需 endpoint 分组
- [x] 评估 WebSocket 订阅模型是否需要补充
- [x] 为 Web 客户端列出最小控制面能力清单

### K7.4 收尾与验收

- [x] 跑通 `cargo test -p grid-platform-service --test control_plane -- --nocapture`
- [x] 复核 K7 验收标准
- [x] 更新 `docs/plan.md` 与本文件状态
- 验收结论：K7 已验收通过

## 当前并行方式

- `K7` 主线：查询模型整理与 Web UI 预备边界

## 完成后必做

- [x] 更新 `docs/plan.md` 中对应里程碑状态
- [x] 更新本文件中的任务勾选状态
- [x] 记录最近一次验证结果
