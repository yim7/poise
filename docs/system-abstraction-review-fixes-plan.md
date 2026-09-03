# 系统抽象复审修复计划

本文是本轮修复期间的临时任务清单。验收完成后删除本文，并把仍成立的系统语义更新到 `system-overview.md`。

## 范围与标准

本轮只修复已经复核确认的问题，不增加兼容、迁移或双读逻辑。SQLite schema 发生变化时仍采用精确 schema 校验；旧实例数据库不兼容，直接重新初始化。

测试使用已经确认的公共 seam：交易所 `ExecutionPort`、`TradingRuntime::handle` 和 engine `TrackTransition`。每个任务先补失败测试，再做最小实现；通过直接相关测试后提交，并将 commit SHA 回写本文，才开始下一项。

## 设计决定

- 完整订单分页由各 adapter 内部负责。调用方只得到完整结果或错误，不接触 cursor、`after` 或交易所分页模型。
- canonical position quantity 在共享业务边界使用 `PositionQty`；wire 数字仍由 adapter 私有代码使用 `f64`。
- PNL 记录自身携带发生时的 quantity unit、base asset 或 contract value。storage 不再用写入时的当前 `InstrumentSpec` 推断历史语义。
- `TradingRuntime` 先在候选状态应用并验证 PNL，再持久化；持久化成功后安装候选状态，消除“已落库但内存拒绝”的部分提交。
- engine transition 使用显式时间输入，并显式声明需要提交的 facts/effects；删除按前后状态字段排除来推断 durable write 的协议。
- Hyperliquid 保证金失败后的容量查询不参与重新授权，因此删除这条无效的 `AccountPort` 能力和隐藏 `10x`，不新增杠杆参数。

## 任务清单

- [x] F1：Bybit 与 OKX open orders 完成全部分页；覆盖多页、结束页和游标不前进/请求失败场景。Commit：`1f67dd2`
- [x] F2：PNL 异资产记录通过 `TradingRuntime::handle` 原子进入 SQLite 和 committed state，并保持 `Incomplete` 聚合。Commit：`a689d25`
- [ ] F3：共享 order、position、observation 与 PNL quantity 边界统一使用 `PositionQty`，adapter wire 转换保持私有。Commit：待完成
- [ ] F4：PNL fact 保存发生时 quantity 语义，storage 不读取当前 metadata 补写历史字段。Commit：待完成
- [ ] F5：时间成为 engine 输入，market transition 删除字段排除 diff 和手工状态恢复。Commit：待完成
- [ ] F6：删除 Hyperliquid 隐藏 `10x` 及无效的运行期 capacity port 查询。Commit：待完成
- [ ] F7：运行 workspace 验收、复审最终 diff，更新 `system-overview.md` 并删除本文。Commit：待完成
