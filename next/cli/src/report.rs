//! 三个只读或收尾的命令：`check`、`status`、`stop`。
//!
//! 都只打印结论，不打印私钥；启动校验里最要紧的一句是「由私钥推出的地址确实在
//! 这个账户已授权的 API 钱包列表里」。

use anyhow::{Context, Result};
use chrono::Utc;
use poise_runtime::{
    Exchange, Runtime, StartupCheck, StartupWarning, StoredOrder, StrategyStatus, validate_startup,
};
use poise_strategy::instrument::{Instrument, PriceRule};

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
/// 这是启动闸门，会真正下单的命令（`grid`）在动手之前都先走这一步——
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

    // `stop` 会把本标的上所有策略一起暂停，所以别让这里看起来只有一个。
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
    println!(
        "状态        {}",
        match stored.status {
            StrategyStatus::Running => "运行中",
            StrategyStatus::Paused => "已暂停",
        }
    );
    println!("建立时间    {}", stored.created_at);
    println!("成交同步点  {}", stored.fills_synced_through);
    println!(
        "合约规则    报价规则 {} 数量精度 {} 最低金额 {}",
        price_rule(stored.spec.price_rule()),
        stored.spec.quantity_step(),
        stored.spec.min_order_notional()
    );
    println!(
        "网格        买价 {} 到 {}，间距 {}，价差 {}，上限 {}",
        stored.config.long.price_low,
        stored.config.long.price_high,
        stored.config.long.grid_spacing,
        stored.config.long.grid_spread,
        stored.config.long.max_position
    );

    for (index, cell) in store.load_cell_states(strategy_id)?.iter().enumerate() {
        println!(
            "  格 {index} {} 待买回 {}",
            if cell.enabled {
                "已启用"
            } else {
                "未启用"
            },
            cell.to_buy_back
        );
    }

    let orders = store.load_orders(strategy_id)?;
    println!("本策略的订单 {} 张", orders.len());
    for order in &orders {
        print_order(order);
    }

    let working = instance.exchange().working_orders(instrument).await?;
    println!("交易所上属于本系统的挂单 {} 张", working.len());
    for order in &working {
        println!(
            "  {} 价 {} 量 {} 已成交 {} 状态 {:?}",
            order.client_order_id,
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
    // 撤单会写数据库，也要先取实例锁：否则会和正在跑的 `grid` 抢同一批订单。
    let _lock = instance.lock()?;

    let runtime = Runtime::new(instance.exchange().clone(), instance.open_store()?);
    let ids = runtime
        .store()
        .load_strategy_ids(instrument)
        .context("cannot read the strategies in this database")?;
    if ids.is_empty() {
        println!("{} 上还没有策略", instrument.symbol());
        return Ok(());
    }

    for strategy_id in ids {
        let canceled = runtime.pause(&strategy_id, Utc::now()).await?;
        println!("策略 {strategy_id} 已暂停，撤销 {} 张订单", canceled.len());
    }

    // 收尾核对：交易所上不该再剩本系统的挂单。
    let working = instance.exchange().working_orders(instrument).await?;
    println!("交易所上还有 {} 张属于本系统这个标的的挂单", working.len());
    for order in working {
        println!(
            "  {} 价 {} 量 {} 状态 {:?}",
            order.client_order_id, order.limit_price, order.quantity, order.status
        );
    }
    let position = instance.exchange().position(instrument).await?;
    println!("仓位        {position}");
    Ok(())
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
        "  {} 格 {} {} 价 {} 量 {} 已成交 {} 状态 {:?} 交易所单号 {}",
        order.record.client_order_id,
        order.record.cell_index,
        order.record.purpose.as_str(),
        order.record.limit_price,
        order.record.quantity,
        order.record.cumulative_filled,
        order.record.status,
        order.exchange_order_id.as_deref().unwrap_or("-"),
    );
}
