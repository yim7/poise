//! 三个辅助命令：`check`、`status`、`stop`。
//!
//! 都只打印结论，不打印私钥；启动校验里最要紧的一句是「由私钥推出的地址确实在
//! 这个账户已授权的 API 钱包列表里」。

use anyhow::{Context, Result};
use chrono::Utc;
use poise_runtime::{
    Exchange, StartupCheck, StartupWarning, StoredOrder, StrategyStatus, validate_startup,
};
use poise_strategy::instrument::{Instrument, PriceRule};
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
    println!("状态        {}", status_name(stored.status));
    println!("建立时间    {}", stored.created_at);
    println!("成交同步点  {}", stored.fills_synced_through);
    println!(
        "合约规则    报价规则 {} 数量精度 {} 最低金额 {}",
        price_rule(stored.spec.price_rule()),
        stored.spec.quantity_step(),
        stored.spec.min_order_notional()
    );
    let state = store.load_state(strategy_id)?;
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

    let orders = store.load_orders(strategy_id)?;
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

/// 撤销全部策略在本标的上的在场订单（第 9.4 节的暂停）。
pub async fn stop(instance: &Instance, instrument: &Instrument) -> Result<()> {
    // 撤单会写数据库，也要先取实例锁：否则会和正在跑的 `serve` 抢同一批订单。
    let _lock = instance.lock()?;

    let runtime = instance.runtime()?;
    let ids = runtime
        .store()
        .load_strategy_ids(instrument)
        .context("cannot read the strategies in this database")?;
    if ids.is_empty() {
        println!("{} 上还没有策略", instrument.symbol());
        return Ok(());
    }

    for strategy_id in ids {
        let stored = runtime.store().load_strategy(&strategy_id)?;
        match action_for(stored.status) {
            // 归档的策略没有挂单，也不该被重新唤醒。
            Action::Skip => println!("策略 {strategy_id} {}，跳过", status_name(stored.status)),
            // 恢复中、暂停中都没有新单在场外等着，暂停只是把它们收尾的进度往前推一步。
            Action::Pause => {
                let canceled = runtime.pause(&strategy_id).await?;
                println!("策略 {strategy_id} 已暂停，撤销 {} 张订单", canceled.len());
            }
            // 已经点过停止的走停止那条路：暂停不收「停止中」，而它要的正是把撤单收尾
            // 推进一步，和停止是同一件事。
            Action::Stop => {
                let canceled = runtime.stop(&strategy_id).await?;
                println!("策略 {strategy_id} 停止中，撤销 {} 张订单", canceled.len());
            }
        }
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
}

fn action_for(status: StrategyStatus) -> Action {
    match status {
        StrategyStatus::Stopped | StrategyStatus::Draft => Action::Skip,
        StrategyStatus::Stopping => Action::Stop,
        StrategyStatus::Running
        | StrategyStatus::Restoring
        | StrategyStatus::Pausing
        | StrategyStatus::Paused => Action::Pause,
    }
}

/// 状态的中文说法。
fn status_name(status: StrategyStatus) -> &'static str {
    match status {
        StrategyStatus::Draft => "草稿",
        StrategyStatus::Running => "运行中",
        StrategyStatus::Restoring => "恢复中",
        StrategyStatus::Pausing => "暂停中",
        StrategyStatus::Paused => "已暂停",
        StrategyStatus::Stopping => "停止中",
        StrategyStatus::Stopped => "已归档",
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
    use super::{Action, action_for, status_name};
    use poise_runtime::StrategyStatus;

    /// 每个状态下 `stop` 该走哪条路。
    ///
    /// 「停止中」走的是停止：点过停止的策略再点暂停会被运行时拒掉，整条命令会在它这里
    /// 中断，排在后头的策略一张单都撤不掉。
    #[test]
    fn stop_advances_a_stopping_strategy_along_the_stop_path() {
        let expected = [
            (StrategyStatus::Draft, Action::Skip),
            (StrategyStatus::Stopped, Action::Skip),
            (StrategyStatus::Running, Action::Pause),
            (StrategyStatus::Restoring, Action::Pause),
            (StrategyStatus::Pausing, Action::Pause),
            (StrategyStatus::Paused, Action::Pause),
            (StrategyStatus::Stopping, Action::Stop),
        ];
        for (status, action) in expected {
            assert_eq!(action_for(status), action, "{}", status_name(status));
        }
    }
}
