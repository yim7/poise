# K6 回放 / paper / testnet 验证手册

本文档固化 K6 当前可重复执行的验证链路，包括：

- 本地 replay 场景回放
- paper fill 模拟与命令闭环
- fake service / fake transport 集成验证
- testnet 市场流接入下的最小 smoke 流程

## 1. Replay 输入格式

当前 replay 使用 JSON 场景文件，最小格式如下：

```json
{
  "name": "buy fill then flatten",
  "steps": [
    {
      "type": "market",
      "last_price": 99.5,
      "mark_price": 99.5,
      "emitted_at": "2025-01-01T00:00:01Z"
    },
    {
      "type": "command",
      "command": "flatten_now",
      "command_id": "cmd_flatten_replay",
      "expect_status": "completed"
    }
  ],
  "assertions": {
    "position_qty": 0.0,
    "open_order_count": 0,
    "recent_fill_count": 2,
    "last_command_status": "completed"
  }
}
```

字段说明：

- `name`：场景名
- `steps`：按顺序执行的步骤
- `market`：把价格事件送进内核；若当前 execution adapter 是 paper 模式，会在这一步做穿价成交模拟
- `command`：向内核提交控制命令，并等待终态 ack；可选 `expect_status`
- `assertions`：场景结束后对关键快照字段做最小断言

当前仓库内的最小夹具在 [`../service/tests/fixtures/replay_buy_then_flatten.json`](../service/tests/fixtures/replay_buy_then_flatten.json)。

## 2. 本地验证命令

先跑 K6 相关最小矩阵：

```bash
cargo test -p grid-platform-service --test paper_execution --test replay_runner --test fake_transport_chain
```

再跑现有 paper 模式端到端链路：

```bash
cargo test -p grid-platform-tui --test local_paper_e2e
```

最后跑全量回归：

```bash
cargo test
```

## 3. Testnet 最小 Smoke 流程

这条流程的目标是验证：

- 服务端能接上 Binance Futures testnet 市场流
- 市场事件能进入内核并驱动运行态
- control plane 命令在 testnet 市场流环境下仍然闭环
- 重启后运行态和最近命令结果能恢复

当前 smoke 关注的是“testnet 行情 + 本地 paper execution”的最小闭环，不把真实签名下单耦合进当前阶段。

### 3.1 启动服务

```bash
export GRID_PLATFORM_BINANCE_ENABLED=1
export GRID_PLATFORM_BINANCE_ENV=testnet
export GRID_PLATFORM_BINANCE_SYMBOL=XAUUSDT
export GRID_PLATFORM_SERVICE_DB_PATH=.tmp/k6-testnet-smoke-fresh.db

cargo run -p grid-platform-service
```

说明：

- `GRID_PLATFORM_BINANCE_API_KEY` 当前不是必填；未配置时用户流保持未接入，但市场流 smoke 仍然可跑
- 若要验证用户流状态展示，可额外配置 `GRID_PLATFORM_BINANCE_API_KEY`
- 若重复执行 smoke，请删除旧的 DB 文件或换一个新路径，避免把上一次 smoke 的运行态带进本轮验证

### 3.2 检查运行态

```bash
curl http://127.0.0.1:8000/runtime/snapshot
```

确认下面几项：

- `runtime.env` 是 `testnet`
- `connection.http_available` 是 `true`
- `connection.ws_connected` 是 `true`
- `runtime.session_state` 不再停留在 `syncing`
- `connection.last_heartbeat_at` 不为空
- `runtime.last_price` 或 `runtime.mark_price` 已经变成非零值

### 3.3 跑命令闭环

先执行停单和清理：

```bash
curl -X POST http://127.0.0.1:8000/commands/pause -H 'content-type: application/json' -d '{"command_id":"cmd_pause_smoke"}'
curl -X POST http://127.0.0.1:8000/commands/cancel-all -H 'content-type: application/json' -d '{"command_id":"cmd_cancel_smoke"}'
curl -X POST http://127.0.0.1:8000/commands/flatten-now -H 'content-type: application/json' -d '{"command_id":"cmd_flatten_smoke"}'
```

再调用一次快照：

```bash
curl http://127.0.0.1:8000/runtime/snapshot
```

确认下面几项：

- `execution.last_command_ack_event` 已刷新
- `execution.recent_commands` 按最新命令顺序记录
- `runtime.strategy_state` 是 `paused`
- `cancel-all` 后 `execution.open_orders` 为空
- `flatten-now` 后 `runtime.position_qty` 回到 `0`

最后恢复策略：

```bash
curl -X POST http://127.0.0.1:8000/commands/resume -H 'content-type: application/json' -d '{"command_id":"cmd_resume_smoke"}'
curl http://127.0.0.1:8000/runtime/snapshot
```

确认下面几项：

- `runtime.strategy_state` 回到 `running`
- `execution.last_command_ack_event.command_id` 变成 `cmd_resume_smoke`
- `execution.open_orders` 再次出现按最新中心价生成的网格挂单

### 3.4 重启恢复

停掉服务，再用相同环境变量重新启动一次，然后再次检查：

```bash
curl http://127.0.0.1:8000/runtime/snapshot
```

确认下面几项：

- 最近一次 `last_command_ack_event` 仍能恢复
- `recent_commands` 里还能看到刚才的 smoke 命令
- 运行态没有回退到初始样例值

## 4. 当前验收证据

当前 K6 已固化的直接证据包括：

- [`../service/tests/paper_execution.rs`](../service/tests/paper_execution.rs)：paper fill 规则
- [`../service/tests/replay_runner.rs`](../service/tests/replay_runner.rs)：replay 场景回放
- [`../service/tests/fake_transport_chain.rs`](../service/tests/fake_transport_chain.rs)：fake transport 驱动的服务端成交链路
- [`../tui/tests/local_paper_e2e.rs`](../tui/tests/local_paper_e2e.rs)：paper 模式控制面端到端链路
