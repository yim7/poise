# Hyperliquid 合约接入执行计划

目标：自研 Hyperliquid 合约交易所适配，只覆盖 Poise 运行需要的 perp 能力，不引入 Rust SDK 作为长期核心依赖。

边界：

- 只支持 Hyperliquid perpetuals，不支持 spot、vault、withdraw、transfer、builder fee、TWAP、HIP-3 部署资产。
- Hyperliquid 协议、签名、asset id、REST/WS 响应细节全部封装在 `exchanges/hyperliquid/`。
- `core`、`server` 和运行时只看到现有端口：`ExecutionPort`、`MarketDataPort`、`AccountSummaryPort`、`AccountPort`、`MetadataPort`、`SymbolLeverageSetter`。
- 签名实现以官方 Python SDK 和官方 API 文档为事实源；Rust 侧必须有固定样本测试。
- 配置字段为 `venue = "hyperliquid"`、`deployment`、`private_key`、`wallet_address`，可选 `vault_address` 后续只在 Hyperliquid crate 内处理。

验收总标准：

- `venue = "hyperliquid"` 的配置可以解析，并能构造 `Venue::Hyperliquid` 的 track。
- `exchanges/hyperliquid` 可以完成 meta、user state、open orders、下限价单、撤单、cancel all、update leverage、BBO/mark 行情、用户订单/成交/资金费事件的端口映射。
- 各模块有 TDD 测试覆盖，先跑最小相关测试，最终需要跑 Hyperliquid crate 和 server 相关测试。
- README 和 `docs/system-overview.md` 更新当前支持的交易所、配置和安全说明。

## 任务清单

- [x] Task 1: 配置边界和 workspace 接入
  - 文件：`Cargo.toml`、`core/src/track.rs`、`server/Cargo.toml`、`server/src/config.rs`、`server/src/assembly.rs`、`server/src/exchange_startup.rs`、`exchanges/hyperliquid/*`
  - 验收：`cargo test -p poise-core track::tests::venue_as_str_supports_hyperliquid` 和 `cargo test -p poise-server config::tests::parses_hyperliquid_exchange_config`
  - Commit SHA：`00095b9`

- [ ] Task 2: Hyperliquid 配置、端点和凭证校验
  - 文件：`exchanges/hyperliquid/src/config.rs`
  - 验收：`cargo test -p poise-hyperliquid config::tests::`
  - Commit SHA：

- [ ] Task 3: REST info 模型和 mapper
  - 文件：`exchanges/hyperliquid/src/rest/models.rs`、`exchanges/hyperliquid/src/mapper.rs`
  - 覆盖：`meta -> ExchangeInfo`、`userState -> Position/AccountSummary/Capacity`、`openOrders -> ExchangeOrder`
  - 验收：`cargo test -p poise-hyperliquid mapper::tests::`
  - Commit SHA：

- [ ] Task 4: L1 签名和 exchange action 编码
  - 文件：`exchanges/hyperliquid/src/signing.rs`、`exchanges/hyperliquid/src/rest/actions.rs`
  - 覆盖：order、cancel、updateLeverage 的固定 payload 签名样本；地址统一小写；nonce 与 vault marker 进入 action hash
  - 验收：`cargo test -p poise-hyperliquid signing::tests::`
  - Commit SHA：

- [ ] Task 5: REST client 写操作和只读查询
  - 文件：`exchanges/hyperliquid/src/rest/client.rs`
  - 覆盖：`meta`、`user_state`、`open_orders`、`submit_order`、`cancel_order`、`cancel_all`、`set_leverage`
  - 验收：`cargo test -p poise-hyperliquid rest::client::tests::`
  - Commit SHA：

- [ ] Task 6: Connected ports 和 server 装配
  - 文件：`exchanges/hyperliquid/src/connected.rs`、`exchanges/hyperliquid/src/startup_control.rs`、`server/src/assembly.rs`、`server/src/exchange_startup.rs`
  - 验收：`cargo test -p poise-hyperliquid connected::tests::` 和 `cargo test -p poise-server assembly::tests::`
  - Commit SHA：

- [ ] Task 7: WebSocket 行情和用户事件
  - 文件：`exchanges/hyperliquid/src/ws/*`
  - 覆盖：BBO/mark tick、order update、fills、funding、断线通知与重连订阅
  - 验收：`cargo test -p poise-hyperliquid ws::tests::`
  - Commit SHA：

- [ ] Task 8: 文档和最终验收
  - 文件：`README.md`、`docs/system-overview.md`
  - 验收：相关最小测试全部通过；必要时扩大到 `cargo test -p poise-hyperliquid`、`cargo test -p poise-server config::tests::`、`cargo test -p poise-server assembly::tests::`、`cargo test -p poise-server exchange_startup::tests::`
  - Commit SHA：
