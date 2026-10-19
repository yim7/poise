//! 三个辅助命令：`check`、`status`、`stop`。
//!
//! 都只打印结论，不打印私钥；启动校验里最要紧的一句是「由私钥推出的地址确实在
//! 这个账户已授权的 API 钱包列表里」。

use anyhow::{Context, Result};
use chrono::Utc;
use poise_runtime::{
    Exchange, StartupCheck, StartupWarning, StoredOrder, StrategyStatus, validate_startup,
};
use poise_strategy::Strategy;
use poise_strategy::instrument::{Instrument, PriceRule};
use poise_strategy::invalidation::reduction_sold;
use poise_strategy::order::OrderRecord;
use poise_strategy::side::Direction;

use crate::instance::Instance;

/// `check`：只做启动校验，不下单。
pub async fn check(instance: &Instance) -> Result<()> {
    println!("启动文件    {}", instance.startup().path().display());
    println!("网络        {}", instance.startup().network().as_str());
    validate(instance).await?;
    Ok(())
}

/// 交易所那一侧的启动校验（设计第 2.0 节）：账户模式、API 钱包授权、到期提醒。
///
/// 这是启动闸门，会真正下单的命令（`serve`）在动手之前都先走这一步——
/// 账户不是 Unified、或私钥推出的地址不在已授权列表里，就一张单也不该发出去。
///
/// `stop` 刻意不走这一步：出问题时撤单是唯一要做的事，不能被一次读接口失败挡住。
/// `status` 只读，也不需要。
pub async fn validate(instance: &Instance) -> Result<StartupCheck> {
    let path = instance.startup().path();
    let check = validate_startup(instance.startup(), instance.account(), Utc::now())
        .await
        .with_context(|| format!("启动校验没有通过（{}）", path.display()))?;

    println!(
        "启动校验通过：账户模式 {}，API 钱包 {}（已授权，有效期至 {}），可用 USDC {}",
        check.account_mode.as_str(),
        check.api_wallet_address,
        check.api_wallet_valid_until,
        check.available_collateral
    );
    for warning in &check.warnings {
        match warning {
            StartupWarning::ApiWalletExpiresSoon {
                address,
                valid_until,
                remaining_days,
            } => println!(
                "提醒        API 钱包 {address} 还有 {remaining_days} 天到期（{valid_until}），请提前续期"
            ),
        }
    }
    Ok(check)
}

/// 策略状态、本地订单、交易所上的挂单、仓位和可用余额。
pub async fn status(instance: &Instance, instrument: &Instrument) -> Result<()> {
    let store = instance.open_store()?;
    let ids = store.load_strategy_ids(instrument)?;
    let Some(strategy_id) = ids.first() else {
        println!("{} 上还没有策略", instrument.symbol());
        return Ok(());
    };

    // `stop` 会把本标的上所有还能动的策略一起暂停，所以别让这里看起来只有一个。
    if ids.len() > 1 {
        println!(
            "提醒        本标的上还有 {} 个策略，`stop` 会把它们一起暂停：{}",
            ids.len() - 1,
            ids[1..]
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(" ")
        );
    }

    let stored = store.load_strategy(strategy_id)?;
    println!("策略        {strategy_id}");
    println!("标的        {}", stored.instrument.symbol());
    println!("状态        {}", stored.status.label());
    println!("建立时间    {}", stored.created_at);
    println!("成交同步点  {}", stored.fills_synced_through);
    println!(
        "合约规则    报价规则 {} 数量精度 {} 最低金额 {}",
        price_rule(stored.spec.price_rule()),
        stored.spec.quantity_step(),
        stored.spec.min_order_notional()
    );
    let orders = store.load_orders(strategy_id)?;
    // 观点失效卖出的数量不进账本：和网页的格子表一样，按「先网格、后主仓」扣掉再显示。
    let records: Vec<OrderRecord> = orders.iter().map(|order| order.record.clone()).collect();
    let strategy = Strategy::new(stored.config.clone(), stored.spec.clone(), stored.fees)?;
    let state =
        strategy.after_invalidation(&store.load_state(strategy_id)?, &reduction_sold(&records));
    for direction in Direction::ALL {
        let Some(side) = stored.config.side(direction) else {
            continue;
        };
        println!(
            "{direction} 网格    {} 到 {}，间距 {}，价差 {}，上限 {}",
            side.price_low, side.price_high, side.grid_spacing, side.grid_spread, side.max_position
        );
        let Some(side_state) = state.side(direction) else {
            continue;
        };
        for (index, cell) in side_state.cells().iter().enumerate() {
            // 待买回量 `S` 是还差多少才建满这一格：多头侧是买入，空头侧是卖出开空。
            let pending = match direction {
                Direction::Long => "待买回",
                Direction::Short => "待开空",
            };
            println!(
                "  格 {index} {} {pending} {}",
                if cell.enabled {
                    "已启用"
                } else {
                    "未启用"
                },
                cell.to_buy_back
            );
        }
    }

    println!("本策略的订单 {} 张", orders.len());
    for order in &orders {
        print_order(order);
    }

    let working = instance.exchange().working_orders(instrument).await?;
    println!("交易所上这个标的的挂单 {} 张", working.len());
    for order in &working {
        println!(
            "  {} {:?} 价 {} 量 {} 已成交 {} 状态 {:?}",
            cloid_label(order),
            order.side,
            order.limit_price,
            order.quantity,
            order.cumulative_filled,
            order.status
        );
    }

    let position = instance.exchange().position(instrument).await?;
    let mark = instance.exchange().mark_price(instrument).await?;
    let available = instance.account().available_collateral().await?;
    println!("仓位        {position}");
    println!("标记价      {mark}");
    println!("可用 USDC   {available}");
    Ok(())
}

/// 紧急停止：暂停本标的上的全部策略，撤掉本系统在场的订单（第 9.4 节的暂停）。
///
/// 常驻服务在跑时实例锁在它手里，这里拿不到；这时改走服务的本机接口，让服务自己去
/// 暂停、撤单——同一把锁、同一份状态，不会和它抢订单。地址取服务写在数据目录里的
/// 实际监听地址，不取启动文件里的 `listen`。服务没在跑时才直接打开数据库操作。
///
/// 失效中的策略不动：它正在用只减仓单止损，停止会把它归档、放弃剩下的减仓（第 2.4 节）。
/// 真要放弃，去网页上点停止。
pub async fn stop(instance: &Instance, instrument: &Instrument) -> Result<()> {
    // 撤单会写数据库，先取实例锁：否则会和正在跑的 `serve` 抢同一批订单。
    let found = match instance.lock() {
        Ok(lock) => {
            let outcome = stop_locally(instance, instrument).await;
            drop(lock);
            outcome?
        }
        Err(lock_error) => {
            let listen =
                running_service_address(instance).with_context(|| format!("{lock_error:#}"))?;
            println!(
                "实例锁被占着（{}），常驻服务应当正在运行：改走它的本机接口 http://{listen}",
                lock_error.root_cause()
            );
            stop_via_api(&listen.to_string(), instrument)
                .await
                .with_context(|| format!("{lock_error:#}"))?
        }
    };
    // 这个标的上没有策略：本系统在这里没下过单，不用再去交易所核对。
    if found == Found::Nothing {
        println!("{} 上还没有策略", instrument.symbol());
        return Ok(());
    }

    // 收尾核对：交易所上不该再剩本系统的挂单。
    // 只看本系统下的单（带 cloid）：用户手动下的单不归 `stop` 管。
    let working: Vec<_> = instance
        .exchange()
        .working_orders(instrument)
        .await?
        .into_iter()
        .filter(|order| order.client_order_id.is_some())
        .collect();
    println!("交易所上还有 {} 张属于本系统这个标的的挂单", working.len());
    for order in &working {
        println!(
            "  {} 价 {} 量 {} 状态 {:?}",
            cloid_label(order),
            order.limit_price,
            order.quantity,
            order.status
        );
    }
    let position = instance.exchange().position(instrument).await?;
    println!("仓位        {position}");
    Ok(())
}

/// 常驻服务写在数据目录里的实际监听地址。没有这个文件就不去猜：按启动文件里的 `listen`
/// 去连，可能连到同一端口上的别的服务。
fn running_service_address(instance: &Instance) -> Result<std::net::SocketAddr> {
    let path = instance.listen_file();
    let text = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "cannot read `{}`, where the running service records its address",
            path.display()
        )
    })?;
    let address: std::net::SocketAddr = text
        .trim()
        .parse()
        .with_context(|| format!("`{}` does not hold an address", path.display()))?;
    if !address.ip().is_loopback() {
        anyhow::bail!(
            "`{}` holds {address}, which is not a local address",
            path.display()
        );
    }
    Ok(address)
}

/// 这个标的上有没有找到策略。
#[derive(Debug, PartialEq, Eq)]
enum Found {
    Nothing,
    Strategies,
}

/// 服务没在跑：直接打开数据库，逐个策略处理。
///
/// 一个策略出错不中断后面的：紧急命令要把能撤的都撤掉，最后再把失败的报出来。
async fn stop_locally(instance: &Instance, instrument: &Instrument) -> Result<Found> {
    let runtime = instance.runtime()?;
    let ids = runtime
        .store()
        .load_strategy_ids(instrument)
        .context("cannot read the strategies in this database")?;
    if ids.is_empty() {
        return Ok(Found::Nothing);
    }

    let mut failures = Vec::new();
    for strategy_id in ids {
        let status = match runtime.store().load_strategy(&strategy_id) {
            Ok(stored) => stored.status,
            Err(error) => {
                println!("策略 {strategy_id} 读不出来：{error}");
                failures.push(strategy_id);
                continue;
            }
        };
        let outcome = match action_for(status) {
            Action::Skip => {
                println!("策略 {strategy_id} {}，跳过", status.label());
                continue;
            }
            Action::KeepReducing => {
                println!(
                    "策略 {strategy_id} {}，正在用只减仓单减仓，不打断；要放弃请在网页上点停止",
                    status.label()
                );
                continue;
            }
            // 恢复中、暂停中都没有新单在场外等着，暂停只是把它们收尾的进度往前推一步。
            Action::Pause => runtime
                .pause(&strategy_id)
                .await
                .map(|canceled| format!("已暂停，撤销 {} 张订单", canceled.len())),
            // 已经点过停止的走停止那条路：暂停不收「停止中」，而它要的正是把撤单收尾
            // 推进一步，和停止是同一件事。
            Action::Stop => runtime
                .stop(&strategy_id)
                .await
                .map(|canceled| format!("停止中，撤销 {} 张订单", canceled.len())),
        };
        match outcome {
            Ok(message) => println!("策略 {strategy_id} {message}"),
            Err(error) => {
                println!("策略 {strategy_id} 处理失败：{error}");
                failures.push(strategy_id);
            }
        }
    }
    if failures.is_empty() {
        return Ok(Found::Strategies);
    }
    anyhow::bail!(
        "{} 个策略没处理成功：{}",
        failures.len(),
        failures
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(" ")
    )
}

/// 服务在跑：通过它的本机接口暂停本标的上的策略。
///
/// 接口和网页用的是同一套（`runtime/src/api.rs`）：先列策略，再对本标的上每个活动策略
/// 发暂停或停止，和本地路径的决定一样。
async fn stop_via_api(listen: &str, instrument: &Instrument) -> Result<Found> {
    let base = format!("http://{listen}/api");
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("cannot build the HTTP client")?;
    let strategies: Vec<serde_json::Value> = http
        .get(format!("{base}/strategies"))
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .with_context(|| format!("cannot reach the running service at http://{listen}"))?
        .json()
        .await
        .context("the running service returned an unreadable strategy list")?;

    let mut found = Found::Nothing;
    let mut failures = Vec::new();
    for strategy in strategies {
        if strategy["instrument"].as_str() != Some(instrument.symbol()) {
            continue;
        }
        let Some(strategy_id) = strategy["strategy_id"].as_str() else {
            continue;
        };
        found = Found::Strategies;
        let status = strategy["status"].as_str().unwrap_or("");
        let (status, action) = match status_from_api(status) {
            Some(status) => (status, action_for(status)),
            None => {
                println!("策略 {strategy_id} 状态 `{status}` 认不出来，跳过");
                failures.push(strategy_id.to_string());
                continue;
            }
        };
        let path = match action {
            Action::Skip => {
                println!("策略 {strategy_id} {}，跳过", status.label());
                continue;
            }
            Action::KeepReducing => {
                println!(
                    "策略 {strategy_id} {}，正在用只减仓单减仓，不打断；要放弃请在网页上点停止",
                    status.label()
                );
                continue;
            }
            Action::Pause => "pause",
            Action::Stop => "stop",
        };
        let response = http
            .post(format!("{base}/strategies/{strategy_id}/{path}"))
            .send()
            .await;
        let outcome = async {
            let body: serde_json::Value = response?.error_for_status()?.json().await?;
            Ok::<_, reqwest::Error>(body)
        }
        .await;
        match outcome {
            Ok(body) => println!(
                "策略 {strategy_id} {}，撤销 {} 张订单",
                body["status"]
                    .as_str()
                    .and_then(status_from_api)
                    .map_or("状态未知", StrategyStatus::label),
                body["canceled"].as_array().map_or(0, Vec::len)
            ),
            Err(error) => {
                println!("策略 {strategy_id} 处理失败：{error}");
                failures.push(strategy_id.to_string());
            }
        }
    }
    if failures.is_empty() {
        return Ok(found);
    }
    anyhow::bail!(
        "{} 个策略没处理成功：{}",
        failures.len(),
        failures.join(" ")
    )
}

/// 接口上的状态字符串和数据库里是同一套（`StrategyStatus::as_str`）。
fn status_from_api(value: &str) -> Option<StrategyStatus> {
    serde_json::from_value(serde_json::Value::String(value.to_string())).ok()
}

/// `stop` 对每个策略做的事。
///
/// 这个映射跟着运行时那边的契约走：暂停只收运行中、恢复中、暂停中、已暂停，已经点过
/// 停止的策略再点暂停会被拒（`NotRunning`），而它要的本来就是继续收尾，推进一步的是
/// 停止那条路。
#[derive(Debug, PartialEq, Eq)]
enum Action {
    /// 归档的没有挂单，草稿还没启动：都不用动。
    Skip,
    /// 撤销挂单、留在暂停：仓位保留。
    Pause,
    /// 已经点过停止：把撤单收尾推进一步。
    Stop,
    /// 失效中、平仓中：正在止损减仓或平仓，紧急命令不打断它。停止会归档并放弃剩下的减仓，
    /// 那是用户要明确做的决定，不该由一条「撤掉挂单」的命令顺手做掉。
    KeepReducing,
}

fn action_for(status: StrategyStatus) -> Action {
    match status {
        StrategyStatus::Stopped
        | StrategyStatus::Invalidated
        | StrategyStatus::Closed
        | StrategyStatus::Draft => Action::Skip,
        StrategyStatus::Stopping => Action::Stop,
        StrategyStatus::Invalidating | StrategyStatus::Closing => Action::KeepReducing,
        StrategyStatus::Running
        | StrategyStatus::Restoring
        | StrategyStatus::Pausing
        | StrategyStatus::Paused => Action::Pause,
    }
}

/// 报价规则的一句话说明。
fn price_rule(rule: PriceRule) -> String {
    match rule {
        PriceRule::Fixed(tick) => format!("固定步长 {tick}"),
        PriceRule::SignificantFigures {
            max_figures,
            step_floor,
        } => format!("最多 {max_figures} 位有效数字，步长不小于 {step_floor}"),
    }
}

fn print_order(order: &StoredOrder) {
    println!(
        "  {} {} {} 价 {} 量 {} 已成交 {} 状态 {:?} 交易所单号 {}",
        order.record.client_order_id,
        order.record.unit,
        order.record.purpose.as_str(),
        order.record.limit_price,
        order.record.quantity,
        order.record.cumulative_filled,
        order.record.status,
        order.exchange_order_id.as_deref().unwrap_or("-"),
    );
}

/// 挂单的客户端订单 ID；用户手动下的单没有它。
fn cloid_label(order: &poise_runtime::ExchangeOrder) -> String {
    order
        .client_order_id
        .as_ref()
        .map_or_else(|| "（手动下单）".to_string(), ToString::to_string)
}

#[cfg(test)]
mod tests {
    use super::{Action, action_for};
    use poise_runtime::StrategyStatus;

    /// 每个状态下 `stop` 该走哪条路。
    ///
    /// 「停止中」走的是停止：点过停止的策略再点暂停会被运行时拒掉。「失效中」不动：
    /// 它正在止损减仓，停止会归档并放弃剩下的减仓，紧急撤单的命令不该顺手做掉这件事。
    #[test]
    fn stop_advances_a_stopping_strategy_and_leaves_an_invalidating_one_alone() {
        let expected = [
            (StrategyStatus::Draft, Action::Skip),
            (StrategyStatus::Stopped, Action::Skip),
            (StrategyStatus::Invalidated, Action::Skip),
            (StrategyStatus::Running, Action::Pause),
            (StrategyStatus::Restoring, Action::Pause),
            (StrategyStatus::Pausing, Action::Pause),
            (StrategyStatus::Paused, Action::Pause),
            (StrategyStatus::Stopping, Action::Stop),
            (StrategyStatus::Invalidating, Action::KeepReducing),
            (StrategyStatus::Closing, Action::KeepReducing),
            (StrategyStatus::Closed, Action::Skip),
        ];
        for (status, action) in expected {
            assert_eq!(action_for(status), action, "{}", status.label());
        }
    }

    /// 接口上的状态字符串就是数据库里那一套，认不出的返回 `None` 而不是猜。
    #[test]
    fn api_statuses_map_back_to_the_lifecycle_statuses() {
        assert_eq!(
            super::status_from_api("invalidating"),
            Some(StrategyStatus::Invalidating)
        );
        assert_eq!(
            super::status_from_api("running"),
            Some(StrategyStatus::Running)
        );
        assert_eq!(super::status_from_api("archived"), None);
    }
}
