//! 最小做多网格：把命令行参数变成网格配置，然后一步接一步地驱动运行时。
//!
//! 本期没有主仓、不限速（任务 02 的范围），所以「网格」就是一个价格范围、一个间距、
//! 每格一份数量。为了在测试网上跑得动，参数用「距标记价万分之几」表示，
//! 实际的报价精度、数量精度和最低金额都按交易所给的合约规则算。

use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{Duration as ChronoDuration, Utc};
use poise_runtime::{Exchange, ResumeOutcome, Runtime, StrategyId, StrategyStatus};
use poise_strategy::Strategy;
use poise_strategy::config::{ConfigError, SideConfig, StrategyConfig};
use poise_strategy::instrument::{Fees, Instrument, InstrumentSpec};
use poise_strategy::planning::WaitReason;
use poise_strategy::units::{Price, PriceRounding, Quantity};

use crate::instance::Instance;

#[derive(Debug, Clone)]
pub struct GridParams {
    pub instrument: Instrument,
    /// 格数。
    pub cells: usize,
    /// 网格间距，按标记价的万分之几。
    pub spacing_bps: i64,
    /// 网格价差（每格卖价比买价高多少），按标记价的万分之几。
    pub spread_bps: i64,
    /// 每格数量；缺省取刚好满足最低订单金额的数量。
    pub quantity: Option<Quantity>,
}

/// 一步之间的等待。
#[derive(Debug, Clone, Copy)]
pub struct LoopOptions {
    pub interval: Duration,
    pub once: bool,
}

/// 跑网格：没有策略就建一个，有就按第 12 节的恢复流程接着跑。
pub async fn run(instance: &Instance, params: GridParams, options: LoopOptions) -> Result<()> {
    // 一个实例只允许一个写进程：两个进程同时驱动会各自下单，同一格重复买入。
    // 锁一直持有到命令结束（`_lock` 不能省成 `_`）。
    let _lock = instance.lock()?;

    // 启动闸门（设计第 2.0 节）：账户模式不对、或私钥没被授权，就不要开始下单。
    crate::report::validate(instance).await?;

    let account = instance.account();
    let runtime = Runtime::new(instance.exchange().clone(), instance.open_store()?);

    let strategy_id = match runtime
        .store()
        .load_strategy_ids(&params.instrument)?
        .first()
    {
        Some(strategy_id) => {
            let strategy_id = strategy_id.clone();
            println!("接着已有的策略跑：{strategy_id}");
            resume(&runtime, &strategy_id).await?;
            strategy_id
        }
        None => {
            // 费率、合约规则和标记价只有新建策略时才用得上：恢复流程不该被
            // 这几个读接口拖住（它们取不到时会直接让命令失败）。
            let fees = account
                .fees()
                .await
                .context("cannot read the account fee rates")?;
            let spec = account
                .instrument_spec(&params.instrument)
                .await
                .context("cannot read the contract rules")?;
            let mark = instance
                .exchange()
                .mark_price(&params.instrument)
                .await
                .context("cannot read the mark price")?;
            let strategy = plan_minimum(&params, &spec, &fees, mark)?;
            print_grid(&strategy, mark);
            let strategy_id = runtime.create_strategy(
                strategy.config().clone(),
                spec.clone(),
                fees,
                Utc::now(),
            )?;
            println!("新策略：{strategy_id}");
            strategy_id
        }
    };

    drive(instance, &runtime, &strategy_id, options).await
}

/// 参数、合约规则和标记价 → 一个已经通过校验的策略。
///
/// 返回 [`Strategy`] 而不是配置，是因为调用方要显示格子划分；配置可以从它取回。
pub fn plan(
    params: &GridParams,
    spec: &InstrumentSpec,
    fees: &Fees,
    mark: Price,
) -> Result<Strategy, ConfigError> {
    Strategy::new(config(params, spec, mark), spec.clone(), *fees)
}

/// 没给每格数量时，按「刚好合规」来配。
///
/// 先按最小数量步长试一次；配置校验会算出这一格至少需要多少（含两条腿的余量），
/// 用它再算一次就够——那个值是按最严格的一格算的，对其它格也成立。
/// 这样 CLI 不必复制策略里的最低金额规则。
pub fn plan_minimum(
    params: &GridParams,
    spec: &InstrumentSpec,
    fees: &Fees,
    mark: Price,
) -> Result<Strategy, ConfigError> {
    let explicit = params.quantity;
    let mut params = params.clone();
    params.quantity = Some(explicit.unwrap_or_else(|| spec.quantity_step()));

    match plan(&params, spec, fees, mark) {
        Err(ConfigError::CellBelowMinimumOrderNotional {
            required_quantity, ..
        }) if explicit.is_none() => {
            params.quantity = Some(required_quantity);
            plan(&params, spec, fees, mark)
        }
        result => result,
    }
}

fn config(params: &GridParams, spec: &InstrumentSpec, mark: Price) -> StrategyConfig {
    let spacing = spec.normalize_price(
        fraction_of(mark, params.spacing_bps),
        PriceRounding::Nearest,
    );
    let spread = spec.normalize_price(fraction_of(mark, params.spread_bps), PriceRounding::Nearest);
    // 最高一格挂在标记价下面一个间距：买单不越价，落在盘口上等。
    let price_high = spec.normalize_price(mark.saturating_sub(spacing), PriceRounding::Nearest);
    // 想要正好 `cells` 格：跨度落在 (cells − 1) × d 与 cells × d 之间。
    let steps = params.cells as i128 - 1;
    let span = spacing.units() as i128 * steps + spacing.units() as i128 / 2;
    let price_low = spec.normalize_price(
        Price::from_units((price_high.units() as i128 - span).max(1) as i64),
        PriceRounding::Down,
    );

    let per_cell = params.quantity.unwrap_or(spec.quantity_step());
    let position_units = (per_cell.units() as i128 * params.cells as i128).min(i64::MAX as i128);

    StrategyConfig {
        instrument: params.instrument.clone(),
        long: SideConfig {
            price_low,
            price_high,
            max_position: Quantity::from_units(position_units as i64),
            grid_spacing: spacing,
            grid_spread: spread,
        },
    }
}

/// 标记价的万分之 `bps`。
fn fraction_of(price: Price, bps: i64) -> Price {
    Price::from_units(((price.units() as i128 * bps as i128) / 10_000) as i64)
}

/// 按第 12 节恢复：等可能落地的订单过期，撤掉遗留挂单，再走一步。
async fn resume(runtime: &Runtime, strategy_id: &StrategyId) -> Result<()> {
    loop {
        match runtime.resume(strategy_id, Utc::now()).await? {
            ResumeOutcome::WaitingForExpiry { until } => {
                let wait =
                    (until - Utc::now()).max(ChronoDuration::zero()) + ChronoDuration::seconds(1);
                println!("还有可能落地的订单，等到 {until} 之后再恢复");
                tokio::time::sleep(wait.to_std().unwrap_or(Duration::from_secs(1))).await;
            }
            ResumeOutcome::Resumed(report) => {
                // `report` 是恢复之后走的那一步，恢复时撤掉的遗留挂单不在里面。
                println!(
                    "恢复完成（遗留挂单已在恢复时撤掉）：这一步撤单 {} 张，下单 {} 张，补记成交 {} 笔",
                    report.canceled.len(),
                    report.placed.len(),
                    report.applied_fills
                );
                return Ok(());
            }
        }
    }
}

/// 一步接一步地驱动。收到停止信号时撤销本策略全部在场订单。
async fn drive(
    instance: &Instance,
    runtime: &Runtime,
    strategy_id: &StrategyId,
    options: LoopOptions,
) -> Result<()> {
    loop {
        match runtime.step(strategy_id, Utc::now()).await {
            Ok(report) => println!(
                "{} 状态 {} 仓位 {} 应有 {} 下单 {} 撤单 {} 成交 {} 行情 {} 等待 {}",
                Utc::now().format("%H:%M:%S"),
                if report.status == StrategyStatus::Paused {
                    "已暂停"
                } else {
                    "运行中"
                },
                report.position,
                report.expected_position,
                report.placed.len(),
                report.canceled.len(),
                report.applied_fills,
                report.plan.market_price,
                waiting(&report.plan.waiting),
            ),
            // 这一步没走完不代表策略坏了：结果未知的订单要靠后面的核对才有结论，
            // 所以这里只报错，不停下来。
            Err(error) => eprintln!("这一步失败：{error}"),
        }

        if options.once {
            return Ok(());
        }

        // 策略被暂停了（多半是运行时发现仓位对不上）就不要再转下去了：
        // 再走也只会一直报同一个错，用户看不到「策略已经停了」这个事实。
        // 实例锁保证不会同时有另一个进程在改这个策略。
        if stopped(runtime, strategy_id)? {
            println!("策略不再运行，退出；确认仓位后可以用 `poise grid` 接着跑");
            print_remaining(instance, runtime, strategy_id).await?;
            return Ok(());
        }
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                println!("收到停止信号，撤销本策略全部在场订单");
                let canceled = runtime.pause(strategy_id, Utc::now()).await?;
                println!("已撤销 {} 张订单", canceled.len());
                print_remaining(instance, runtime, strategy_id).await?;
                return Ok(());
            }
            _ = tokio::time::sleep(options.interval) => {}
        }
    }
}

/// 策略是不是已经不在运行了。看的是库里存的状态。
fn stopped(runtime: &Runtime, strategy_id: &StrategyId) -> Result<bool> {
    let stored = runtime.store().load_strategy(strategy_id)?;
    Ok(stored.status != StrategyStatus::Running)
}

/// 停止之后核对：交易所上还有没有属于本系统这个标的的挂单。
async fn print_remaining(
    instance: &Instance,
    runtime: &Runtime,
    strategy_id: &StrategyId,
) -> Result<()> {
    let stored = runtime.store().load_strategy(strategy_id)?;
    let working = instance
        .exchange()
        .working_orders(stored.spec.instrument())
        .await?;
    println!("交易所上还有 {} 张属于本系统这个标的的挂单", working.len());
    for order in working {
        println!(
            "  {} 价 {} 量 {} 状态 {:?}",
            order.client_order_id, order.limit_price, order.quantity, order.status
        );
    }
    Ok(())
}

fn waiting(reasons: &[WaitReason]) -> String {
    if reasons.is_empty() {
        return "-".to_string();
    }
    reasons
        .iter()
        .map(|reason| format!("{reason:?}"))
        .collect::<Vec<_>>()
        .join(",")
}

fn print_grid(strategy: &Strategy, mark: Price) {
    println!(
        "网格 {} 共 {} 格，标记价 {}，最高一格买价 {}",
        strategy.spec().instrument().symbol(),
        strategy.grid().len(),
        mark,
        strategy.config().long.price_high
    );
    for cell in strategy.grid().cells() {
        println!(
            "  第 {} 格 买 {} 卖 {} 数量 {}",
            cell.index, cell.buy_price, cell.sell_price, cell.quantity
        );
    }
}

#[cfg(test)]
mod tests {
    use poise_strategy::instrument::{Fees, Instrument, InstrumentSpec};
    use poise_strategy::units::{Notional, Price, Quantity, Rate};

    use poise_strategy::config::ConfigError;

    use super::{GridParams, config, plan, plan_minimum};

    fn btc_spec() -> InstrumentSpec {
        InstrumentSpec::new(
            Instrument::try_new("BTC").unwrap(),
            Price::parse("1").unwrap(),
            Quantity::parse("0.00001").unwrap(),
            Notional::parse("10").unwrap(),
        )
        .unwrap()
    }

    fn params(cells: usize, quantity: Option<&str>) -> GridParams {
        GridParams {
            instrument: Instrument::try_new("BTC").unwrap(),
            cells,
            spacing_bps: 20,
            spread_bps: 40,
            quantity: quantity.map(|value| Quantity::parse(value).unwrap()),
        }
    }

    fn fees() -> Fees {
        Fees::new(
            Rate::parse("0.00045").unwrap(),
            Rate::parse("0.00015").unwrap(),
        )
    }

    #[test]
    fn the_price_range_follows_the_mark_price() {
        let spec = btc_spec();
        let mark = Price::parse("95000").unwrap();

        let config = config(&params(2, None), &spec, mark);

        // 间距和价差按万分之几取整到报价精度。
        assert_eq!(config.long.grid_spacing, Price::parse("190").unwrap());
        assert_eq!(config.long.grid_spread, Price::parse("380").unwrap());
        // 最高一格在标记价下面一个间距。
        assert_eq!(config.long.price_high, Price::parse("94810").unwrap());
        assert!(config.long.price_high < mark);
    }

    #[test]
    fn the_grid_has_the_requested_cell_count_and_rests_below_the_mark() {
        let spec = btc_spec();
        let mark = Price::parse("95000").unwrap();

        let strategy = plan_minimum(&params(2, None), &spec, &fees(), mark).unwrap();

        let cells = strategy.grid().cells();
        assert_eq!(cells.len(), 2);
        assert_eq!(cells[0].buy_price, Price::parse("94810").unwrap());
        assert_eq!(cells[1].buy_price, Price::parse("94620").unwrap());
        assert!(cells.iter().all(|cell| cell.buy_price < mark));
        // 缺省数量每格一样，而且每格都顶得住交易所的最低订单金额。
        assert_eq!(cells[0].quantity, cells[1].quantity);
        assert!(
            cells
                .iter()
                .all(|cell| spec.meets_minimum_order(cell.quantity, cell.buy_price))
        );
        assert_eq!(
            strategy.config().long.max_position,
            Quantity::from_units(cells[0].quantity.units() * 2)
        );
    }

    #[test]
    fn a_given_quantity_is_used_as_is() {
        let spec = btc_spec();
        let mark = Price::parse("95000").unwrap();

        let strategy = plan(&params(1, Some("0.001")), &spec, &fees(), mark).unwrap();

        assert_eq!(strategy.grid().len(), 1);
        assert_eq!(
            strategy.grid().cells()[0].buy_price,
            Price::parse("94810").unwrap()
        );
        assert_eq!(
            strategy.grid().cells()[0].quantity,
            Quantity::parse("0.001").unwrap()
        );
    }

    #[test]
    fn a_quantity_below_the_minimum_order_amount_is_refused_not_silently_raised() {
        let spec = btc_spec();
        let mark = Price::parse("95000").unwrap();

        let error = plan(&params(2, Some("0.00001")), &spec, &fees(), mark).unwrap_err();

        assert!(
            matches!(error, ConfigError::CellBelowMinimumOrderNotional { .. }),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn a_grid_that_cannot_beat_the_fees_is_refused() {
        let spec = btc_spec();
        let mark = Price::parse("95000").unwrap();
        let mut params = params(2, None);
        // 价差只有 10 美元，扣掉两条腿的 taker 费率后不赚钱（费率是万分之 4.5）。
        params.spread_bps = 1;

        let error = plan_minimum(&params, &spec, &fees(), mark).unwrap_err();

        assert!(
            error.to_string().contains("profitable"),
            "unexpected error: {error}"
        );
    }
}
