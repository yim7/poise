//! `poise`：一个实例的命令行入口。
//!
//! 本期只有测试网验证需要的四个命令：`check`、`grid`、`status`、`stop`；没有网页界面。
//! 启动文件写的网络是 `mainnet` 时，全部命令都拒绝运行（[`instance`]）。
//!
//! 网格参数用「距标记价万分之几」表达，精度和最低金额按交易所给的合约规则算；
//! 想在任何价位上跑，直接给 `--spacing-bps` / `--spread-bps` / `--quantity`。
//!
//! 会写数据库、会向交易所下单撤单的命令（`grid`、`stop`）先取实例锁：
//! 一个实例同时只允许一个这样的进程。

mod grid;
mod instance;
mod report;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use poise_strategy::instrument::Instrument;
use poise_strategy::units::Quantity;
use tracing_subscriber::EnvFilter;

use crate::instance::Instance;

#[derive(Parser)]
#[command(name = "poise", about = "Poise 网格策略的命令行入口（本期只连测试网）")]
struct Cli {
    /// 实例目录，里面放着启动文件 poise.toml。
    #[arg(long, global = true, value_name = "目录", default_value = ".")]
    instance: PathBuf,
    /// 打印调试日志。
    #[arg(long, global = true)]
    verbose: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 只做启动校验，不下任何单。
    Check,
    /// 运行最小做多网格；已经有策略就按恢复流程接着跑。
    Grid(GridArgs),
    /// 显示策略状态、订单、仓位和可用余额。
    Status(InstrumentArgs),
    /// 撤销这个标的上本系统全部在场订单。
    Stop(InstrumentArgs),
}

#[derive(Args)]
struct InstrumentArgs {
    /// 标的。
    #[arg(long, default_value = "BTC")]
    instrument: String,
}

/// `--spacing-bps` / `--spread-bps` 的上限：标记价的 100%。
///
/// 再大就不只是「网格很宽」，而是 `标记价 × bps` 会溢出 `Price` 的整数范围，
/// 算出来是个负数，后面的取整会直接 panic。
const MAX_BPS: i64 = 10_000;

#[derive(Args)]
struct GridArgs {
    #[command(flatten)]
    instrument: InstrumentArgs,
    /// 格数。
    #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(i64).range(1..=1_000))]
    cells: i64,
    /// 网格间距，按标记价的万分之几。上限是 100% 的标记价。
    #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(i64).range(1..=MAX_BPS))]
    spacing_bps: i64,
    /// 网格价差，按标记价的万分之几。上限是 100% 的标记价。
    #[arg(long, default_value_t = 40, value_parser = clap::value_parser!(i64).range(1..=MAX_BPS))]
    spread_bps: i64,
    /// 每格数量；缺省取刚好满足最低订单金额的数量。
    #[arg(long)]
    quantity: Option<String>,
    /// 两步之间的间隔秒数。
    #[arg(long, default_value_t = 5)]
    interval_secs: u64,
    /// 只走一步就退出。
    #[arg(long)]
    once: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_logging(cli.verbose);

    let instance = Instance::load(&cli.instance)?;
    match cli.command {
        Command::Check => report::check(&instance).await,
        Command::Grid(args) => {
            let instrument = parse_instrument(&args.instrument.instrument)?;
            let quantity = args
                .quantity
                .as_deref()
                .map(Quantity::parse)
                .transpose()
                .context("`--quantity` is not a number")?;
            let params = grid::GridParams {
                instrument,
                cells: args.cells as usize,
                spacing_bps: args.spacing_bps,
                spread_bps: args.spread_bps,
                quantity,
            };
            let options = grid::LoopOptions {
                interval: Duration::from_secs(args.interval_secs),
                once: args.once,
            };
            grid::run(&instance, params, options).await
        }
        Command::Status(args) => {
            let instrument = parse_instrument(&args.instrument)?;
            report::status(&instance, &instrument).await
        }
        Command::Stop(args) => {
            let instrument = parse_instrument(&args.instrument)?;
            report::stop(&instance, &instrument).await
        }
    }
}

fn parse_instrument(symbol: &str) -> Result<Instrument> {
    Instrument::try_new(symbol).with_context(|| format!("`{symbol}` is not a valid instrument"))
}

fn init_logging(verbose: bool) {
    let default = if verbose { "debug" } else { "info" };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}
