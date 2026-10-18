//! `poise`：一个实例的命令行入口。
//!
//! `serve` 是常驻进程：驱动运行中的策略，并在本机提供网页控制台。新建、预览、
//! 确认启动、暂停、恢复、停止都在网页上做，命令行只留 `check`、`status`、`stop`
//! 三个辅助命令。连测试网还是正式网由启动文件决定（[`instance`]）。
//!
//! 会写数据库、会向交易所下单撤单的命令（`serve`、`stop`）先取实例锁：
//! 一个实例同时只允许一个这样的进程。

mod instance;
mod report;
mod serve;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use poise_runtime::ServeOptions;
use poise_strategy::instrument::Instrument;
use tracing_subscriber::EnvFilter;

use crate::instance::Instance;

#[derive(Parser)]
#[command(
    name = "poise",
    about = "Poise 网格策略的命令行入口；连测试网还是正式网由实例目录里的启动文件决定"
)]
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
    /// 常驻服务：驱动运行中的策略，并在本机提供网页控制台。
    Serve(ServeArgs),
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

#[derive(Args)]
struct ServeArgs {
    /// 两步之间的间隔秒数。
    #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u64).range(1..=3600))]
    step_interval_secs: u64,
    /// 网页控制台的静态文件目录（前端 `npm run build` 的产物）。
    #[arg(long, value_name = "目录", default_value = DEFAULT_WEB_ROOT)]
    web_root: PathBuf,
    /// 覆盖启动文件里的 `listen`，只在本机地址冲突时用；仍然只接受本机地址。
    #[arg(long, value_name = "地址")]
    listen: Option<String>,
}

/// 缺省就是仓库里的前端产物：源码树里直接 `poise serve` 就能打开控制台。
const DEFAULT_WEB_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../console/dist");

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_logging(cli.verbose);

    let instance = Instance::load(&cli.instance)?;
    match cli.command {
        Command::Check => report::check(&instance).await,
        Command::Serve(args) => {
            let options = ServeOptions {
                step_interval: Duration::from_secs(args.step_interval_secs),
                web_root: Some(args.web_root),
            };
            serve::run(&instance, options, args.listen).await
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

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{Cli, Command};

    /// 常驻服务的节拍可以调，缺省 5 秒。
    #[test]
    fn serve_takes_a_step_interval() {
        for (arguments, expected) in [
            (vec!["poise", "serve"], 5),
            (
                vec![
                    "poise",
                    "--instance",
                    "/tmp/i",
                    "serve",
                    "--step-interval-secs",
                    "9",
                ],
                9,
            ),
        ] {
            let cli = Cli::try_parse_from(arguments).unwrap();
            let Command::Serve(args) = cli.command else {
                panic!("`serve` must parse into the serve command");
            };
            assert_eq!(args.step_interval_secs, expected);
        }
    }

    /// `grid` 没有了：跑策略现在是 `serve` 加网页控制台的事。
    #[test]
    fn grid_is_gone() {
        assert!(Cli::try_parse_from(["poise", "grid"]).is_err());
    }

    /// 地址缺省用启动文件里的；命令行可以覆盖，覆盖值走同一套本机地址检查。
    #[test]
    fn serve_can_override_the_listen_address() {
        let cli = Cli::try_parse_from(["poise", "serve"]).unwrap();
        let Command::Serve(args) = cli.command else {
            panic!("`serve` must parse into the serve command");
        };
        assert!(args.listen.is_none());

        let cli = Cli::try_parse_from(["poise", "serve", "--listen", "127.0.0.1:18080"]).unwrap();
        let Command::Serve(args) = cli.command else {
            panic!("`serve` must parse into the serve command");
        };
        assert_eq!(args.listen.as_deref(), Some("127.0.0.1:18080"));
    }
}
