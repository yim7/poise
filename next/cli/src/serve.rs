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
use poise_runtime::{ServeOptions, Server};

use crate::instance::Instance;

/// 起服务并一直跑到 `Ctrl-C`。
///
/// `listen` 是命令行给的覆盖值，缺省用启动文件里的 `listen`。
pub async fn run(instance: &Instance, options: ServeOptions, listen: Option<String>) -> Result<()> {
    let lock = instance.lock()?;

    // 启动闸门：账户不是 Unified、或私钥推出的地址不在已授权列表里，就此打住（第 2.0 节）。
    crate::report::validate(instance).await?;

    let text = listen.unwrap_or_else(|| instance.startup().listen().to_string());
    let listen = listen_address(&text)?;
    let runtime = Arc::new(instance.runtime()?);

    let server = Server::bind(runtime, lock, listen, options)
        .await
        .with_context(|| format!("cannot listen on `{listen}`"))?;
    println!("网页控制台  http://{}", server.local_addr()?);
    println!("停止        按 Ctrl-C");

    server
        .run(async {
            if let Err(error) = tokio::signal::ctrl_c().await {
                tracing::error!(%error, "cannot listen for Ctrl-C; stopping now");
            }
        })
        .await?;

    println!("已停止");
    Ok(())
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
