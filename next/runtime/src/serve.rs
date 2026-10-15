//! 常驻服务（第 12 节）。
//!
//! 一个实例就是一个进程：它拿住实例锁、恢复运行中的策略、按节拍驱动它们，
//! 并在启动文件给的本地地址上提供网页用的 HTTP 接口。
//!
//! 服务本身不做任何判定：一步该不该下单由 [`Runtime`] 决定，这里只管什么时候走。

use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::time::MissedTickBehavior;

use crate::api;
use crate::lock::InstanceLock;
use crate::runtime::{Runtime, RuntimeError};
use crate::store::StrategyId;

/// 常驻服务的参数。
#[derive(Debug, Clone)]
pub struct ServeOptions {
    /// 每轮驱动之间的间隔。
    pub step_interval: Duration,
    /// 网页控制台的静态文件目录；没有就只提供接口。
    pub web_root: Option<PathBuf>,
}

impl Default for ServeOptions {
    fn default() -> Self {
        Self {
            step_interval: Duration::from_secs(5),
            web_root: None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Runtime(#[from] RuntimeError),
    /// 控制台没有登录，只允许监听本机地址。
    #[error(
        "`{0}` is not a local address: the console has no login, so it must not be reachable from the network"
    )]
    NotLocal(SocketAddr),
}

/// 已经绑好地址、拿住实例锁的服务。
///
/// 监听地址只来自启动文件里的 `listen`：不额外开放别的地址，也不对外网开放。
pub struct Server {
    runtime: Arc<Runtime>,
    listener: TcpListener,
    options: ServeOptions,
    /// 实例锁：进程活着就一直握着，析构时释放。
    _lock: InstanceLock,
}

impl Server {
    pub async fn bind(
        runtime: Arc<Runtime>,
        lock: InstanceLock,
        listen: SocketAddr,
        options: ServeOptions,
    ) -> Result<Self, ServeError> {
        if !listen.ip().is_loopback() {
            return Err(ServeError::NotLocal(listen));
        }
        let listener = TcpListener::bind(listen).await?;
        tracing::info!(listen = %listener.local_addr()?, "serving the web console");
        Ok(Self {
            runtime,
            listener,
            options,
            _lock: lock,
        })
    }

    pub fn local_addr(&self) -> Result<SocketAddr, ServeError> {
        Ok(self.listener.local_addr()?)
    }

    /// 恢复运行中的策略，然后一直服务到 `shutdown` 完成。
    ///
    /// 恢复必须在开始驱动之前做完：重启前挂出去的单还没撤，先走一步就会在旧单之上
    /// 再挂一遍。
    ///
    /// 但恢复不能挡住网页：先把监听开起来，再一个一个恢复。恢复本身只是把状态落到
    /// 「恢复中」，真正的等待和撤单交给驱动循环一轮一轮推（第 12 节）。某个策略恢复
    /// 不成功只是它自己不能下新单（网页上看得到原因），别的策略和整个控制台照常。
    pub async fn run<F: Future<Output = ()> + Send + 'static>(
        self,
        shutdown: F,
    ) -> Result<(), ServeError> {
        let runtime = self.runtime.clone();
        let interval = self.options.step_interval;
        let router = match self.options.web_root.clone() {
            Some(root) => api::router_with_web(self.runtime.clone(), root),
            None => api::router(self.runtime.clone()),
        };
        let restoring = tokio::spawn(async move { restore(runtime, interval).await });
        let served = axum::serve(self.listener, router)
            .with_graceful_shutdown(shutdown)
            .await;
        restoring.abort();
        Ok(served?)
    }
}

/// 进程重启后把运行中的策略逐个落到「恢复中」，然后按节拍驱动它们（第 12 节）。
///
/// 遗留订单还有可能落地时不能下单：撤单和下单之间隔着一段时间，订单落地了我们不知道，
/// 就会重复下单。所以恢复分两步走——先落到「恢复中」，这个状态一张新单都不下；再由
/// 驱动循环一轮一轮地等过期、撤单、补记成交，收干净了才转成「运行中」。
///
/// 一个策略恢复不成功不拖住别的策略，也不影响控制台：它留在「恢复中」，网页上看得到
/// 原因，下一轮继续试。整份恢复做完才进入驱动循环。
async fn restore(runtime: Arc<Runtime>, interval: Duration) {
    let to_restore = match runtime.to_restore() {
        Ok(to_restore) => to_restore,
        Err(error) => {
            tracing::error!(%error, "cannot list the strategies to restore");
            Vec::new()
        }
    };
    let mut pending = Vec::new();
    for strategy_id in to_restore {
        if let Err(error) = hold_back(&runtime, &strategy_id).await {
            tracing::error!(
                strategy = %strategy_id,
                %error,
                "cannot hold this strategy back; it must not be driven until it is"
            );
            pending.push(strategy_id);
        }
    }
    drive(runtime, interval, pending).await;
}

/// 把一个策略落到「恢复中」：重启前挂出去的单还没撤，落成之前不能驱动它。
async fn hold_back(runtime: &Runtime, strategy_id: &StrategyId) -> Result<(), RuntimeError> {
    tracing::info!(
        strategy = %strategy_id,
        "holding this strategy back until its leftover orders are cleared"
    );
    runtime
        .hold_for_restore(strategy_id, "进程重启，正在收尾重启前挂出去的单。")
        .await
}

/// 每一轮驱动所有该驱动的策略。
///
/// 除了运行中的，还有「恢复中」「暂停中」「停止中」：它们各自还没走完，收尾、撤单和
/// 补记成交都要靠驱动推进。
///
/// 一个策略出错不拖住别的策略：记下来，下一轮再来。策略自己被暂停之后就不再是
/// 「运行中」，下一轮自然不会再驱动它。
///
/// `pending` 是启动时没能落成「恢复中」的策略。它们还是「运行中」，身上可能带着重启前
/// 挂出去的单：每一轮先重试落状态，落成之前绝不驱动——驱动就是在旧单之上再挂一遍。
async fn drive(runtime: Arc<Runtime>, interval: Duration, mut pending: Vec<StrategyId>) {
    let mut ticker = tokio::time::interval(interval);
    // 一轮花的时间超过间隔时不连着补跑：补跑只会把积压的行情一次性用掉。
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        if !pending.is_empty() {
            let mut still = Vec::new();
            for strategy_id in pending {
                if let Err(error) = hold_back(&runtime, &strategy_id).await {
                    tracing::error!(
                        strategy = %strategy_id,
                        %error,
                        "cannot hold this strategy back; it must not be driven until it is"
                    );
                    still.push(strategy_id);
                }
            }
            pending = still;
        }
        let drivable = match runtime.drivable() {
            Ok(drivable) => drivable,
            Err(error) => {
                tracing::error!(%error, "cannot list the strategies to drive");
                continue;
            }
        };
        for strategy_id in drivable {
            if pending.contains(&strategy_id) {
                tracing::error!(
                    strategy = %strategy_id,
                    "not driven: it is still not held back from the last restart"
                );
                continue;
            }
            match runtime.step(&strategy_id).await {
                Ok(report) => tracing::debug!(
                    strategy = %strategy_id,
                    status = report.status.as_str(),
                    placed = report.placed.len(),
                    canceled = report.canceled.len(),
                    "step done"
                ),
                // 接口那边正在驱动同一个策略：它走的就是这一步，这一轮跳过就是了。
                // 网页每隔几秒刷新一次详情，撞上是常事，不是失败。
                Err(RuntimeError::Busy(_)) => tracing::debug!(
                    strategy = %strategy_id,
                    "step skipped: the strategy is being driven by a console request"
                ),
                Err(error) => tracing::error!(strategy = %strategy_id, %error, "step failed"),
            }
        }
    }
}
