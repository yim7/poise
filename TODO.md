# TODO

当前 TODO 只跟踪最近 1 到 3 个里程碑的可执行任务。
阶段目标与顺序见 [`docs/roadmap.md`](docs/roadmap.md)，近期策略与验收标准见 [`docs/plan.md`](docs/plan.md)。

## 当前状态

- 当前主线：`K6` 回放 / paper / testnet 验证
- 并行预研：`K7` Web UI 就绪与多实例预备
- 最近完成：`K4` 执行闭环与命令语义做实；`K5` 网格策略与风控
- 最近验证：`cargo test` 已通过，含 `service` / `tui` 全量测试；K5 的策略状态机、风险事件、协议契约、本地 E2E 与 TUI 快照回归均已跑通

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

- [ ] 为 replay runner 增加场景测试
- [ ] 为 paper fill 逻辑增加成交模拟测试
- [ ] 为 fake service / fake transport 集成链路增加测试
- [ ] 为 testnet 下单、撤单、恢复增加最小冒烟清单

### K6.2 回放与 paper

- [ ] 设计 replay 输入格式
- [ ] 实现 replay runner
- [ ] 实现 paper execution 和 fill 模拟规则
- [ ] 建立录制或构造的市场事件流夹具

### K6.3 验证链路

- [ ] 建立 fake service / fake transport 组合测试夹具
- [ ] 跑通本地 replay 闭环
- [ ] 跑通 paper 模式命令闭环
- [ ] 跑通 testnet 最小执行闭环

### K6.4 运维与验收

- [ ] 输出运维与验证手册
- [ ] 固化验证命令和检查项
- [ ] 跑通 `cargo test`
- [ ] 复核 K6 验收标准
- [ ] 更新 `docs/plan.md` 与本文件状态

## K7 Web UI 就绪与多实例预备

### K7.1 查询模型整理

- [ ] 梳理 `runtime / orders / fills / alerts / commands` 的 Web 友好查询模型
- [ ] 为关键列表设计分页参数
- [ ] 为关键列表设计过滤参数
- [ ] 为命令与风险事件设计查询排序规则

### K7.2 实例维度与认证边界

- [ ] 预留 `instance_id`
- [ ] 定义局域网部署模式
- [ ] 设计简单认证与 token 边界
- [ ] 明确 WebSocket 连接的鉴权策略

### K7.3 Web 客户端准备

- [ ] 整理 Web UI 所需 endpoint 分组
- [ ] 评估 WebSocket 订阅模型是否需要补充
- [ ] 为 Web 客户端列出最小控制面能力清单

## 当前并行方式

- `K6` 主线：回放 / paper / testnet 验证链路
- `K7` 侧线：查询模型整理与 Web UI 预备边界

## 完成后必做

- [x] 更新 `docs/plan.md` 中对应里程碑状态
- [x] 更新本文件中的任务勾选状态
- [x] 记录最近一次验证结果
