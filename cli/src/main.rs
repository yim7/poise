//! `poise`：一个实例的命令行入口。
//!
//! `serve` 是常驻进程：驱动运行中的策略，并在本机提供网页控制台。新建、预览、
//! 确认启动、暂停、恢复、停止都在网页上做，命令行只留 `check`、`status`、`stop`、
//! `instances` 几个辅助命令。连测试网还是正式网由启动文件决定（[`instance`]），
//! 实例按名字在根目录下找（[`home`]）。
//!
//! 会写数据库、会向交易所下单撤单的命令（`serve`、`stop`）先取实例锁：
//! 一个实例同时只允许一个这样的进程。

mod feishu;
mod home;
mod instance;
mod report;
mod serve;
mod service;

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
    /// 实例名（在 ~/.poise 或 POISE_HOME 下找），或者实例目录的路径；目录里放着启动文件
    /// poise.toml。
    #[arg(long, global = true, value_name = "名字或目录")]
    instance: Option<String>,
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
    /// 列出根目录下的实例：网络、监听地址和账户。
    Instances,
    /// 往启动文件里的飞书机器人发一条测试消息，确认提醒能送到。
    NotifyTest,
    /// 常驻运行：交给 launchd，登录后自动启动、崩溃后自动拉起。
    #[command(subcommand)]
    Service(ServiceCommand),
}

#[derive(Subcommand)]
enum ServiceCommand {
    /// 把当前程序和网页控制台复制进实例目录，装上并启动；已装过就换成当前版本。
    Install {
        /// 两步之间的间隔秒数。
        #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u64).range(1..=3600))]
        step_interval_secs: u64,
        /// 网页控制台的静态文件目录（前端 `npm run build` 的产物），会复制进实例目录。
        #[arg(long, value_name = "目录", default_value = DEFAULT_WEB_ROOT)]
        web_root: PathBuf,
    },
    /// 停掉并卸下。
    Uninstall,
    /// launchd 里的状态。
    Status,
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

    let root = home::root_from_env()?;
    if let Command::Instances = cli.command {
        println!("实例根目录：{}", root.display());
        for line in home::describe(&root) {
            println!("{line}");
        }
        return Ok(());
    }
    let directory = home::resolve(cli.instance.as_deref(), &root)?;
    let instance = Instance::load(&directory)?;
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
        Command::NotifyTest => notify_test(&instance).await,
        Command::Service(command) => match command {
            ServiceCommand::Install {
                step_interval_secs,
                web_root,
            } => service::install(&instance, &directory, &web_root, step_interval_secs),
            ServiceCommand::Uninstall => service::uninstall(&directory),
            ServiceCommand::Status => service::status(&directory),
        },
        Command::Instances => unreachable!("handled before loading an instance"),
    }
}

async fn notify_test(instance: &Instance) -> Result<()> {
    let Some(webhook) = instance.startup().feishu_webhook() else {
        anyhow::bail!("启动文件里没有 `feishu_webhook`");
    };
    let notice = poise_runtime::Notice::new(
        "test",
        "提醒测试",
        "这是一条测试消息：之后失效、自动暂停、兜底单触发、保证金率越线、驱动连续失败和反复撤挂都会推到这里。",
    );
    feishu::send_now(webhook.expose(), &instance.label(), &notice).await?;
    println!("已发送，飞书确认收到");
    Ok(())
}

fn parse_instrument(symbol: &str) -> Result<Instrument> {
    Instrument::try_new(symbol).with_context(|| format!("`{symbol}` is not a valid instrument"))
}

fn init_logging(verbose: bool) {
    let default = if verbose { "debug" } else { "info" };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default));
    // 常驻运行时日志写进文件：不是终端就不带颜色控制符。
    let terminal = std::io::IsTerminal::is_terminal(&std::io::stdout());
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_ansi(terminal)
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

    /// `--instance` 可以不写在命令行上（解析时不报错）：找不到实例时由 [`super::home::resolve`]
    /// 报错并列出已有实例。写了就原样留给它：名字或路径。
    #[test]
    fn the_instance_is_a_name_or_a_path() {
        let cli = Cli::try_parse_from(["poise", "serve"]).unwrap();
        assert_eq!(cli.instance, None);

        let cli = Cli::try_parse_from(["poise", "--instance", "testnet", "check"]).unwrap();
        assert_eq!(cli.instance.as_deref(), Some("testnet"));

        let cli = Cli::try_parse_from(["poise", "instances"]).unwrap();
        assert!(matches!(cli.command, Command::Instances));
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
