# 近期计划

本文档只描述最近 1 到 3 个里程碑的执行顺序、测试矩阵和验收标准。
长期阶段目标与依赖关系统一放在 [`roadmap.md`](roadmap.md)。
更细的当前任务拆分统一放在 [`../TODO.md`](../TODO.md)。

## 1. 当前基线

当前仓库已经完成：

- Rust 双 crate 基线
- 协议模型与兼容文档
- TUI 可用骨架
- 本地用户侧 E2E 基线

当前最重要的判断是：

- 近期主线是把 `service` 做成真正的内核
- 开发顺序按测试优先推进
- `tui` 当前主要职责是验证协议、恢复和用户链路，不是继续扩展页面功能

## 2. 测试优先执行原则

后续开发按三层测试一起推进，不采用“先写实现，再补测试”的方式。

### 2.1 三层测试分工

- 内核测试：覆盖命令生命周期、状态流转、超时和恢复逻辑。
- 接口与协议测试：覆盖 HTTP / WS envelope、路由行为和协议字段兼容。
- 本地 E2E：覆盖 `service -> transport -> tui -> 用户动作 -> service` 的关键闭环。

### 2.2 当前必保的本地验收路径

- 冷启动后成功拉取 `snapshot`
- WebSocket 连通并接收增量事件
- 模拟行情 tick 后 TUI 状态更新
- `pause / resume / flatten` 命令进入时间线并收到结果
- 重连成功后重新拉取 `snapshot`

### 2.3 完成定义

一个功能项只有在满足下面条件后才算完成：

- 验收测试已先定义或同步更新
- 相关单测、集成测试和本地 E2E 通过
- 对外协议和客户端体验没有无意回退

## 3. 里程碑 K4：执行闭环与命令语义做实

### 目标

把命令从“控制面已接收”推进到“真实执行已完成/失败/超时”的闭环语义。

### 先补的测试

1. 为执行适配层补 fake execution transport 测试。
2. 为 `pause / resume / cancel-all / flatten-now / shutdown-after-flatten` 补命令终态测试。
3. 为幂等、重试和超时补内核测试。
4. 扩展本地 E2E，覆盖撤单、平仓、重连后命令终态恢复。

### 实施任务

1. 建立执行适配层边界，并把下单、撤单、查单、成交回报统一接线。
2. 定义 `command_id` 与 `client_order_id / order_id / trade_id` 的关联模型。
3. 让命令生命周期统一输出 `accepted / ack / failed / timed_out`。
4. 让 `open orders`、`fills` 和命令时间线共享同一组执行事实。
5. 把执行失败原因、超时原因和幂等命中原因纳入审计和 WebSocket 事件。
6. 让 TUI 时间线和事件页能看到真实执行结果，而不是只看控制面回包。

### K4 验收标准

- 命令不再只是内存状态切换
- `ack / failure / timeout` 有明确业务意义
- open orders 与 fills 可与命令轨迹关联
- K4 对应测试矩阵全部通过

### 当前状态

- 已完成 execution adapter 显式接口、异步 `accepted / ack / failed / timed_out`、命令关联链路、执行重试与 TUI 联动
- `pause / resume` 已通过策略执行 gate 真实作用于“是否继续新增网格挂单”
- K4 验收条件已满足

## 4. 里程碑 K5：网格策略与风控

### 目标

让 `service` 进入真正的网格运行与风险控制阶段。

### 先补的测试

1. 为网格配置校验补单元测试。
2. 为网格状态机和重建逻辑补状态流转测试。
3. 为最大仓位、止损、单日亏损和 breaker 触发补风险测试。
4. 为 TUI `Grid` 和风险态展示补回归快照。

### 实施任务

1. 定义网格配置 schema 和运行参数。
2. 实现 `active / occupied / pending_rebuild` 状态模型。
3. 建立网格层生成与重建规则。
4. 把策略输出接到执行适配层，而不是直接改订单镜像。
5. 接入风险阈值、breaker 和风险事件。
6. 让 TUI `Grid` 页面展示策略状态而不是单纯订单列表。

### K5 验收标准

- `service` 内部有真实策略状态机
- `Grid` 页面显示的是策略模型而非订单镜像
- 风险事件可观测、可操作
- K5 对应测试矩阵全部通过

### 当前状态

- 已完成 `strategy` 协议模型、网格配置 schema、层级生成、`active / occupied / pending_rebuild` 状态流转
- 已完成最大仓位、止损、单日亏损与 breaker 的统一评估、`risk_alert` 广播与持久化
- 已完成 TUI `Grid / Dashboard / Events` 对策略态、风险阈值和操作建议的联动展示
- K5 验收条件已满足

## 5. 里程碑 K6：回放 / paper / testnet 验证

### 目标

建立可重复验证链路，把系统从“能运行”推进到“能被证明”。

### 先补的测试

1. 为 replay runner 和 paper fill 逻辑补场景测试。
2. 为 fake service / fake transport 集成链路补测试。
3. 为 testnet 下单、撤单、恢复流程补最小冒烟验证。

### 实施任务

1. 实现 replay runner 和录制/回放输入格式。
2. 实现 paper execution 与 fill 模拟规则。
3. 建立 fake service / fake transport 测试夹具。
4. 跑通 testnet 的最小命令执行与恢复闭环。
5. 输出运维与验证手册。

### K6 验收标准

- 本地可重复回放
- paper 与 testnet 可跑通关键操作
- 关键路径有端到端验证证据
- K6 对应测试矩阵全部通过

### 当前状态

- 已完成 replay JSON 输入格式、`service::replay` runner 与最小场景夹具
- 已完成 paper fill 模拟规则，并在市场价穿价时更新挂单、成交与仓位
- 已完成 fake transport 驱动的服务端验证链路，并补上“首个有效市场价前不提前挂单”的回归测试
- 已输出 [`k6-validation.md`](k6-validation.md) 手册，固化 replay / paper / testnet smoke 的验证命令与检查项
- 已手工跑通 testnet 市场流下的 `pause -> cancel-all -> flatten-now -> resume -> restart` 最小闭环
- K6 验收条件已满足

## 6. 里程碑 K7：Web UI 就绪与多实例预备

### 目标

让控制面在不破坏现有 TUI 兼容性的前提下，补齐 Web UI 需要的查询模型、实例边界和能力描述。

### 先补的测试

1. 为 Web 查询接口补 HTTP 路由测试，覆盖 `runtime / orders / fills / alerts / commands`。
2. 为关键列表补分页、过滤和排序测试。
3. 为控制面能力接口补实例、认证边界和 WebSocket 能力测试。

### 实施任务

1. 保留现有 `/runtime/snapshot`、`/orders/open` 等 TUI 兼容接口。
2. 新增 `/query/runtime`、`/query/orders`、`/query/fills`、`/query/alerts`、`/query/commands`。
3. 为列表接口统一补 `items + pagination + filters + sort` 响应模型。
4. 为 `commands` 和 `alerts` 明确默认排序与可选排序规则。
5. 通过 `/control-plane/capabilities` 暴露 `instance_id`、局域网部署模式、简单 token 边界、WebSocket 鉴权策略、endpoint 分组和最小 Web 控制面能力。

### K7 验收标准

- Web UI 可直接消费统一查询模型，不必复用 TUI 的数组型旧接口
- 关键列表具备分页、过滤和排序约束
- `instance_id`、局域网部署模式、认证边界和 WebSocket 策略有明确输出
- K7 对应接口测试全部通过

### 当前状态

- 已新增 `/query/runtime`、`/query/orders`、`/query/fills`、`/query/alerts`、`/query/commands`
- 已为 orders / fills / alerts / commands 补分页、过滤和排序响应模型
- 已通过 `/control-plane/capabilities` 输出 `instance_id`、局域网部署模式、简单 token 边界、WebSocket `runtime_stream` 订阅和 endpoint 分组
- 已补控制面验收测试，覆盖查询模型、排序规则与能力接口
- K7 验收条件已满足

## 7. 当前并行方式

当前没有正在执行的并行主线，等待下一里程碑确认。

当前不建议直接扩展新的交易功能，下一阶段应先明确 K8 目标，再决定是继续控制面扩展还是回到交易能力建设。
