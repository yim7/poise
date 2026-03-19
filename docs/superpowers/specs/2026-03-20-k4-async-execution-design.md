# K4 异步执行终态设计

## 背景

当前 `service` 已经有 execution adapter、命令审计、协议关联字段和 TUI 展示，但 `cancel-all / flatten-now / shutdown-after-flatten` 仍然是在接收命令的同一轮里直接生成最终结果。这会让命令语义停留在“同步改快照”，和 K4 里“先 accepted，再等待真实执行事实落地”的目标不一致。

## 目标

把执行类命令从“同步产出终态”改成“先 accepted，再由内核异步收口 completed / failed / timed_out”，并让审计、WebSocket、恢复和 TUI 消费同一套终态语义。

## 非目标

- 这次不接真实交易所 API。
- 这次不引入复杂重试编排或回放框架。
- 这次不扩展新的 TUI 页面，只补现有链路的真实语义。

## 方案选择

采用“内核内异步执行状态机”方案：

- `submit_command()` 对执行类命令只负责登记 `accepted` 与 in-flight 状态。
- 真正的执行由后台任务调用 adapter。
- adapter 完成后把结果回投给内核，由内核统一写入运行态、审计、WebSocket 事件和恢复快照。
- 超时由服务端内核负责，不再只依赖 TUI 本地超时展示。

不采用“ACK 后让 TUI 强制拉 snapshot”的方案，因为它只能修正页面表现，不能修正服务端命令语义。

## 状态模型

新增一个服务端 in-flight 命令模型，至少包含：

- `command_id`
- `command`
- `accepted_at`
- `deadline_at`
- `state_token`

其中 `state_token` 用来保证超时检查和后台结果回投不会错误覆盖更新后的同名状态。

执行类命令采用单飞策略：

- 任意时刻最多只允许一个执行类命令处于 in-flight。
- 当已有 `cancel-all / flatten-now / shutdown-after-flatten` 在执行时，新的执行类命令不会排队，也不会替换旧命令。
- 新命令会立即收口为 `failed`，失败原因写明当前正在执行的命令。

这样可以避免多个执行命令并发改写 `open_orders`、`recent_fills`、`position_qty` 和 `strategy_state`。

状态流转如下：

1. 接收命令。
2. 若命中幂等记录，直接返回已知终态。
3. 若是 `pause / resume`，仍走本地即时完成。
4. 若是执行类命令，记录 `pending_commands=accepted`，返回 `CommandAccepted`。
5. 后台任务执行 adapter。
6. adapter 成功，内核应用执行结果并写入 `completed / failed / timed_out`。
7. deadline 先到时，由内核写入 `timed_out`，并清理 in-flight。
8. 若后台结果晚于 timeout 到达，则只记系统日志，不再覆盖终态。

`failed` 终态的来源统一定义为：

- adapter 返回业务失败
- adapter 调用返回错误
- 执行类命令撞上单飞限制
- `shutdown-after-flatten` 的子步骤未达到完成条件

## 命令语义

### `pause`

仍为本地命令，但语义改成“禁止策略继续发新单”的前置开关。当前仓库里尚未有真实策略发单链路，因此本次先把内核侧约束点补齐，后续 K5 策略逻辑接入时直接复用。

### `resume`

恢复策略发单能力，与 `pause` 使用同一约束位。

### `cancel-all`

accepted 后进入 in-flight，只有在 adapter 回传“挂单已清空”结果后才写入终态，并更新 `open_orders` 与关联字段。

### `flatten-now`

accepted 后进入 in-flight，只有在 adapter 回传“仓位归零 + reduce-only 成交事实”后才写入终态，并更新 `position_qty`、`recent_fills` 与关联字段。

### `shutdown-after-flatten`

这是一个更安全的复合终态。只有同时满足下面条件才算完成：

- 当前挂单已经清空
- 仓位已经归零
- reduce-only 成交事实已经写入
- `strategy_state` 已切到 `paused`

如果 flatten 失败、cancel-all 失败、或者超时，则整个命令以 `failed / timed_out` 收口，并保持策略状态不被误写成 `paused`。

## 超时与重放保护

服务端维护执行命令 deadline。deadline 到达后：

- 写入 `timed_out`
- 保留超时原因
- 清理 in-flight
- 通过同一协议广播 `CommandAck`

重放保护提升为：

- 先查 `recent_commands`
- 若窗口外已经落盘，查 SQLite 审计结果
- 命中后仍返回 `CommandAccepted`，并补发一个带“Idempotent hit”原因的 `CommandAck`，不重新发起执行

## 持久化与恢复

本次仍以“终态可恢复”为目标，不要求恢复未完成的 in-flight 执行。服务重启后：

- 已完成命令继续从 SQLite 恢复
- 重启时残留的 in-flight 命令不自动重放
- 如需继续执行，由上层重新下发命令

这样可以保持实现简单，同时避免把假执行器做成复杂调度器。

## 测试策略

先补测试，再改实现：

1. `service/tests/kernel_flow.rs`
   - 执行类命令先返回 `accepted`，随后异步收到 `completed`
   - 第二个执行类命令会因单飞限制立即 `failed`
   - adapter 失败会写入 `failed`
   - timeout 由服务端生成，而不是 TUI 本地生成
   - timeout 后晚到结果不会覆盖终态
   - 幂等命中可从持久化审计恢复
2. `service/tests/persistence_recovery.rs`
   - 命令终态与原因、关联字段继续可恢复
3. `tui/tests/local_paper_e2e.rs`
   - 执行命令经由 accepted -> ack 的真实链路
   - failure / timeout 由服务端终态驱动展示
   - timeout / reconnect 后 TUI 仍能看到服务端终态

## 风险与取舍

- 这次不会让 fake adapter 变成完整订单状态机，只做“异步完成 + 服务端 timeout”所需的最小能力。
- `pause / resume` 的真实业务意义会先体现在内核约束位上，等 K5 策略下单路径接入时再真正消费。
- 不恢复 in-flight 是有意取舍，避免在 K4 阶段引入更重的重放系统。
