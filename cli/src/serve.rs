//! `serve`：一个实例的常驻进程（设计第 12 节）。
//!
//! 它先取实例锁，再做启动校验——两步都在动手之前：已经有另一个进程在跑同一个实例，
//! 或者账户模式不对、私钥没被授权，就一张单也不该发出去。先取锁也让「同一个实例只能
//! 有一个写进程」这件事不依赖任何一次网络请求。
//!
//! 起来之后它做两件事：按节拍驱动运行中的策略，在本机地址上提供网页控制台。
//! 停止用 Ctrl-C：收到信号后先停止接新请求，再退出。

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use poise_runtime::{Notice, Notifier, ServeOptions, Server, StartupWarning};

use crate::feishu::FeishuNotifier;
use crate::instance::Instance;

/// 起服务并一直跑到 `Ctrl-C`。
///
/// `listen` 是命令行给的覆盖值，缺省用启动文件里的 `listen`。
pub async fn run(instance: &Instance, options: ServeOptions, listen: Option<String>) -> Result<()> {
    let lock = instance.lock()?;

    // 启动闸门：账户不是 Unified、或私钥推出的地址不在已授权列表里，就此打住（第 2.0 节）。
    let check = crate::report::validate(instance).await?;

    let text = listen.unwrap_or_else(|| instance.startup().listen().to_string());
    let listen = listen_address(&text)?;
    let notifier: Option<Arc<FeishuNotifier>> = match instance.startup().feishu_webhook() {
        Some(webhook) => Some(FeishuNotifier::spawn(
            webhook.expose().to_string(),
            instance.label(),
        )?),
        None => None,
    };
    let mut runtime = instance.runtime()?;
    if let Some(notifier) = &notifier {
        runtime = runtime.with_notifier(notifier.clone());
    }
    let runtime = Arc::new(runtime);

    let server = Server::bind(runtime.clone(), lock, listen, options)
        .await
        .with_context(|| format!("cannot listen on `{listen}`"))?;
    let bound = server.local_addr()?;
    // 写下实际监听的地址：`poise stop` 在服务运行时按它去连（见 `LISTEN_FILE_NAME`）。
    let listen_file = instance.listen_file();
    std::fs::write(&listen_file, format!("{bound}\n"))
        .with_context(|| format!("cannot write `{}`", listen_file.display()))?;
    println!("网页控制台  http://{bound}");
    println!("停止        按 Ctrl-C");
    println!(
        "提醒        {}",
        if notifier.is_some() {
            "推到飞书"
        } else {
            "没配置（启动文件里写 feishu_webhook 才推）"
        }
    );
    // 重启提醒：常驻运行时进程被拉起来，用户靠它知道服务断过。
    let live = runtime.drivable().map(|ids| ids.len()).unwrap_or_default();
    let mut body = format!("{live} 个策略交给它驱动，控制台 http://{bound}");
    for warning in &check.warnings {
        match warning {
            StartupWarning::ApiWalletExpiresSoon { remaining_days, .. } => {
                body.push_str(&format!(
                    "\nAPI 钱包还有 {remaining_days} 天到期，请提前续期。"
                ));
            }
        }
    }
    runtime.notify(Notice::new("service:start", "poise 已启动", body));

    let served = server
        .run(async {
            shutdown_signal().await;
        })
        .await;
    // 进程崩掉时这个文件会留下；`stop` 只在实例锁被占着（服务确实在跑）时才读它。
    let _ = std::fs::remove_file(&listen_file);
    if let Some(notifier) = &notifier {
        notifier.notify(Notice::new(
            "service:stop",
            "poise 已停止",
            "服务正常退出：策略不再被驱动，交易所上的兜底止损单仍然挂着。",
        ));
        // 后台任务还要把这一条送出去。
        notifier.flush(std::time::Duration::from_secs(5)).await;
    }
    served?;

    println!("已停止");
    Ok(())
}

/// 等到 Ctrl-C 或 SIGTERM：launchd 停服务发的是 SIGTERM。
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! {
                    result = tokio::signal::ctrl_c() => if let Err(error) = result {
                        tracing::error!(%error, "cannot listen for Ctrl-C");
                    },
                    _ = terminate.recv() => {}
                }
                return;
            }
            Err(error) => tracing::error!(%error, "cannot listen for SIGTERM"),
        }
    }
    if let Err(error) = tokio::signal::ctrl_c().await {
        tracing::error!(%error, "cannot listen for Ctrl-C; stopping now");
    }
}

/// 启动文件里的 `listen`。
///
/// 只接受本机地址：控制台没有登录，能连上就能下单撤单，不该让同一网络里的别人碰到。
fn listen_address(text: &str) -> Result<SocketAddr> {
    let address: SocketAddr = text
        .parse()
        .with_context(|| format!("`listen = \"{text}\"` is not an address like 127.0.0.1:8000"))?;
    if !address.ip().is_loopback() {
        bail!(
            "`listen = \"{text}\"` is not a local address; the console has no login and must not be reachable from the network"
        );
    }
    Ok(address)
}

#[cfg(test)]
mod tests {
    use super::listen_address;

    #[test]
    fn accepts_a_local_address() {
        assert_eq!(
            listen_address("127.0.0.1:8000").unwrap().to_string(),
            "127.0.0.1:8000"
        );
        assert!(listen_address("[::1]:8000").unwrap().ip().is_loopback());
    }

    #[test]
    fn refuses_an_address_others_can_reach() {
        for text in ["0.0.0.0:8000", "192.168.1.10:8000"] {
            let error = listen_address(text).unwrap_err();
            assert!(
                error.to_string().contains("local address"),
                "unexpected error for `{text}`: {error}"
            );
        }
    }

    #[test]
    fn refuses_something_that_is_not_an_address() {
        let error = listen_address("localhost:8000").unwrap_err();
        assert!(
            format!("{error:#}").contains("is not an address"),
            "unexpected error: {error:#}"
        );
    }
}
