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

use poise_strategy::planning::WaitReason;

use crate::api;
use crate::lock::InstanceLock;
use crate::notify::Notice;
use crate::runtime::{FUNDING_RATE_REFRESH, Runtime, RuntimeError};
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
        let funding = tokio::spawn(refresh_funding(self.runtime.clone()));
        let held = tokio::spawn(read_archived_positions(self.runtime.clone()));
        let served = axum::serve(self.listener, router)
            .with_graceful_shutdown(shutdown)
            .await;
        restoring.abort();
        funding.abort();
        held.abort();
        Ok(served?)
    }
}

/// 定时重读资金费率（第 7.6 节），和交易的驱动分开跑：读接口慢或者超时，不能拖住撤单和
/// 止损检查。成不成功都按同一个节拍，失败了下一轮再读。
async fn refresh_funding(runtime: Arc<Runtime>) {
    let period = FUNDING_RATE_REFRESH
        .to_std()
        .expect("the funding refresh period is positive");
    let mut ticker = tokio::time::interval(period);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        if let Err(error) = runtime.refresh_funding_rates().await {
            tracing::warn!(%error, "cannot refresh the funding rates");
        }
    }
}

/// 每分钟读一次已归档标的上仍有的仓位（第 7.7 节第 8 条）。只读、不下单，和交易的驱动
/// 分开跑；失败了下一轮再读，读数超过 5 分钟没更新页面就不再显示。
async fn read_archived_positions(runtime: Arc<Runtime>) {
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(60));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        if let Err(error) = runtime.read_archived_positions().await {
            tracing::warn!(%error, "cannot read the positions left on archived instruments");
        }
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
        .hold_for_restore(strategy_id, "进程重启，正在收尾重启前的挂单。")
        .await
}

/// 每一轮驱动所有该驱动的策略。
///
/// 除了运行中的，还有「恢复中」「暂停中」「停止中」：它们各自还没走完，收尾、撤单和
/// 补记成交都要靠驱动推进。
///
/// 各个策略并发走步：一步里有十几次串行的交易所请求，单次超时 15 秒，一个策略在交易所
/// 半挂起时能占住好几分钟；串行驱动的话，其它策略的失效价检查、保证金率撤单都要排在
/// 它后面（第 7.5、7.6 节）。同一个策略仍然同一时刻只走一步（[`Runtime::step`] 的占用
/// 标记），资金预检有自己的锁，所以并发不会让两个策略共用一份额度。
///
/// 一个策略出错不拖住别的策略：记下来，下一轮再来。策略自己被暂停之后就不再是
/// 「运行中」，下一轮自然不会再驱动它。
///
/// `pending` 是启动时没能落成「恢复中」的策略。它们还是「运行中」，身上可能带着重启前
/// 挂出去的单：每一轮先重试落状态，落成之前绝不驱动——驱动就是在旧单之上再挂一遍。
async fn drive(runtime: Arc<Runtime>, interval: Duration, mut pending: Vec<StrategyId>) {
    let mut troubles = Troubles::default();
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
            Ok(drivable) => {
                troubles.observe_service(&runtime, None);
                drivable
            }
            Err(error) => {
                tracing::error!(%error, "cannot list the strategies to drive");
                troubles.observe_service(&runtime, Some(format!("读不出要驱动的策略（{error}）")));
                continue;
            }
        };
        let mut steps = Vec::new();
        for strategy_id in drivable {
            if pending.contains(&strategy_id) {
                tracing::error!(
                    strategy = %strategy_id,
                    "not driven: it is still not held back from the last restart"
                );
                troubles.observe(
                    &runtime,
                    &strategy_id,
                    Some("重启后没能落到恢复中，没有驱动".to_string()),
                );
                continue;
            }
            let runtime = runtime.clone();
            steps.push(tokio::spawn(async move {
                let outcome = runtime.step(&strategy_id).await;
                (strategy_id, outcome)
            }));
        }
        for step in steps {
            match step.await {
                Ok((strategy_id, Ok(report))) => {
                    tracing::debug!(
                        strategy = %strategy_id,
                        status = report.status.as_str(),
                        placed = report.placed.len(),
                        canceled = report.canceled.len(),
                        "step done"
                    );
                    let trouble = report
                        .plan
                        .waiting
                        .iter()
                        .find(|reason| is_trouble(**reason))
                        .map(|reason| wait_label(*reason).to_string());
                    troubles.observe(&runtime, &strategy_id, trouble);
                }
                // 用户操作正在用、或者排队等着用这个策略的驱动权：让它先走，这一轮跳过。
                Ok((strategy_id, Err(RuntimeError::Busy(_)))) => tracing::debug!(
                    strategy = %strategy_id,
                    "step skipped: the strategy is being driven by a console request"
                ),
                Ok((strategy_id, Err(error))) => {
                    tracing::error!(strategy = %strategy_id, %error, "step failed");
                    troubles.observe(&runtime, &strategy_id, Some(error.to_string()));
                }
                // 一步里 panic 了：只影响这个策略的这一轮，下一轮照常再驱动。
                Err(error) => tracing::error!(%error, "a step panicked"),
            }
        }
    }
}

/// 驱动出问题多久才提醒：交易所偶尔慢一两步是常事，连着 5 分钟就说明止损检查也可能失灵。
const TROUBLE_ALERT_AFTER: chrono::Duration = chrono::Duration::minutes(5);

/// 这几种等待一直不消失，说明读不到交易所的数据，而不是在正常等价格。
fn is_trouble(reason: WaitReason) -> bool {
    matches!(
        reason,
        WaitReason::StaleMarketData
            | WaitReason::MarginRatioUnavailable
            | WaitReason::OrderResultUnknown
            | WaitReason::FillsPending
            | WaitReason::PositionUnstable
    )
}

fn wait_label(reason: WaitReason) -> &'static str {
    match reason {
        WaitReason::StaleMarketData => "行情过期",
        WaitReason::MarginRatioUnavailable => "保证金率读不到",
        WaitReason::OrderResultUnknown => "订单结果待查",
        WaitReason::FillsPending => "成交待对账",
        WaitReason::PositionUnstable => "仓位数据还在变化",
        _ => "等待中",
    }
}

/// 各策略（和整个驱动循环）从什么时候开始一直出问题、提醒过没有。
#[derive(Default)]
struct Troubles {
    since: std::collections::HashMap<StrategyId, (chrono::DateTime<chrono::Utc>, bool)>,
    service: Option<(chrono::DateTime<chrono::Utc>, bool)>,
}

impl Troubles {
    /// 整个驱动循环出问题（读不出策略列表）：这时没有任何策略在被驱动。
    fn observe_service(&mut self, runtime: &Runtime, trouble: Option<String>) {
        let now = runtime.now();
        match trouble {
            Some(what) => {
                let (since, alerted) = self.service.get_or_insert((now, false));
                if !*alerted && now - *since >= TROUBLE_ALERT_AFTER {
                    *alerted = true;
                    runtime.notify(Notice::new(
                        "service:trouble",
                        "poise 驱动连续 5 分钟出问题",
                        format!("{what}。所有策略都没在驱动，兜底止损单仍在交易所上。"),
                    ));
                }
            }
            None => {
                if let Some((_, true)) = self.service.take() {
                    runtime.notify(Notice::new("service:trouble", "poise 驱动已恢复正常", ""));
                }
                self.service = None;
            }
        }
    }

    /// 记下这一步有没有出问题：连续出问题超过 [`TROUBLE_ALERT_AFTER`] 提醒一次，好了之后再
    /// 提醒一次。
    fn observe(&mut self, runtime: &Runtime, strategy_id: &StrategyId, trouble: Option<String>) {
        let now = runtime.now();
        let instrument = runtime
            .store()
            .load_strategy(strategy_id)
            .map(|stored| stored.instrument.to_string())
            .unwrap_or_else(|_| strategy_id.to_string());
        match trouble {
            Some(what) => {
                let (since, alerted) = self
                    .since
                    .entry(strategy_id.clone())
                    .or_insert((now, false));
                if !*alerted && now - *since >= TROUBLE_ALERT_AFTER {
                    *alerted = true;
                    runtime.notify(Notice::new(
                        format!("{strategy_id}:trouble"),
                        format!("{instrument} 驱动连续 5 分钟出问题"),
                        format!("{what}。这段时间里失效价检查和挂单调整可能都没在做，兜底止损单仍在交易所上。"),
                    ));
                }
            }
            None => {
                if let Some((_, true)) = self.since.remove(strategy_id) {
                    runtime.notify(Notice::new(
                        format!("{strategy_id}:trouble"),
                        format!("{instrument} 驱动已恢复正常"),
                        "",
                    ));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use chrono::{Duration, TimeZone, Utc};
    use poise_strategy::instrument::{Fees, Instrument, InstrumentSpec};
    use poise_strategy::units::{Notional, Price, Quantity, Rate};

    use super::Troubles;
    use crate::clock::ManualClock;
    use crate::notify::RecordingNotifier;
    use crate::store::{Store, StrategyId};
    use crate::{Runtime, RuntimeOptions, SimulatedAccount, SimulatedExchange};

    /// 连续出问题满 5 分钟才提醒一次，好了再说一声；中间好过一次就重新计时。
    #[test]
    fn trouble_is_reported_after_five_minutes_and_once_more_when_it_clears() {
        let start = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let btc = Instrument::try_new("BTC").unwrap();
        let spec = InstrumentSpec::new(
            btc.clone(),
            Price::parse("1").unwrap(),
            Quantity::parse("0.001").unwrap(),
            Notional::parse("10").unwrap(),
        )
        .unwrap();
        let clock = Arc::new(ManualClock::new(start));
        let notices = RecordingNotifier::new();
        let runtime = Runtime::with_clock(
            Arc::new(SimulatedExchange::new(
                btc,
                Price::parse("100").unwrap(),
                start,
            )),
            Arc::new(SimulatedAccount::new(
                spec,
                Fees::new(Rate::ZERO, Rate::ZERO),
            )),
            Store::open_in_memory().unwrap(),
            RuntimeOptions::default(),
            clock.clone(),
        )
        .with_notifier(notices.clone());
        let strategy = StrategyId::new();
        let mut troubles = Troubles::default();

        troubles.observe(&runtime, &strategy, Some("行情过期".to_string()));
        clock.advance(Duration::minutes(3));
        troubles.observe(&runtime, &strategy, None);
        clock.advance(Duration::minutes(1));
        troubles.observe(&runtime, &strategy, Some("行情过期".to_string()));
        clock.advance(Duration::minutes(4));
        troubles.observe(&runtime, &strategy, Some("行情过期".to_string()));
        assert!(notices.notices().is_empty(), "中间好过一次，重新计时");

        clock.advance(Duration::minutes(1));
        troubles.observe(&runtime, &strategy, Some("行情过期".to_string()));
        troubles.observe(&runtime, &strategy, Some("行情过期".to_string()));
        troubles.observe(&runtime, &strategy, None);

        let titles = notices.titles();
        assert_eq!(titles.len(), 2, "{titles:?}");
        assert!(titles[0].ends_with("驱动连续 5 分钟出问题"), "{titles:?}");
        assert!(notices.notices()[0].body.starts_with("行情过期"));
        assert!(titles[1].ends_with("驱动已恢复正常"), "{titles:?}");
    }
}
