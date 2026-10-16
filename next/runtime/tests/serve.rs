//! 常驻服务（第 12 节）：实例锁、重启恢复、按节拍驱动、只监听本地地址。

mod common;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use common::{Console, at, console_over, console_with, long_grid_config};
use poise_runtime::{
    InstanceLock, ManualClock, Runtime, RuntimeOptions, ServeError, ServeOptions, Server, Store,
    StrategyStatus,
};
use poise_strategy::units::{Price, Quantity};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::oneshot;

/// 起一个服务，返回它绑的地址和停止开关。数据目录是临时的，服务活着时锁在它手里。
async fn serve(console: &Console, interval: Duration) -> Running {
    serve_runtime(console.runtime.clone(), interval).await
}

async fn serve_runtime(runtime: Arc<Runtime>, interval: Duration) -> Running {
    let directory = tempfile::tempdir().unwrap();
    let lock = InstanceLock::acquire(directory.path()).unwrap();
    let server = Server::bind(
        runtime,
        lock,
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
        ServeOptions {
            step_interval: interval,
            ..ServeOptions::default()
        },
    )
    .await
    .unwrap();
    let address = server.local_addr().unwrap();
    let (stop, stopped) = oneshot::channel();
    let task = tokio::spawn(async move {
        server
            .run(async move {
                let _ = stopped.await;
            })
            .await
            .unwrap();
    });
    Running {
        address,
        stop,
        task,
        _directory: directory,
    }
}

/// 用真正的套接字发一个 GET，返回整个响应。
///
/// 测试里没有 HTTP 客户端，服务这边要验的又正是「套接字上还答不答话」，
/// 所以直接写一个最小请求。
async fn get(address: SocketAddr, path: &str) -> String {
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let request = format!("GET {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    response
}

struct Running {
    address: SocketAddr,
    stop: oneshot::Sender<()>,
    task: tokio::task::JoinHandle<()>,
    _directory: tempfile::TempDir,
}

impl Running {
    async fn shutdown(self) {
        let _ = self.stop.send(());
        self.task.await.unwrap();
    }
}

/// 服务只监听启动文件给的那个本地地址。
#[tokio::test]
async fn the_server_listens_only_on_the_given_local_address() {
    let console = console_with(Store::open_in_memory().unwrap());
    let running = serve(&console, Duration::from_millis(50)).await;

    assert!(
        running.address.ip().is_loopback(),
        "bound to {}",
        running.address
    );
    running.shutdown().await;
}

/// 控制台没有登录，只监听本机地址：给一个对外地址就拒绝，不绑上去。
#[tokio::test]
async fn the_server_refuses_a_public_address() {
    let console = console_with(Store::open_in_memory().unwrap());
    let directory = tempfile::tempdir().unwrap();
    let lock = InstanceLock::acquire(directory.path()).unwrap();

    let refused = Server::bind(
        console.runtime.clone(),
        lock,
        SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
        ServeOptions {
            step_interval: Duration::from_millis(50),
            ..ServeOptions::default()
        },
    )
    .await;

    assert!(
        matches!(refused, Err(ServeError::NotLocal(_))),
        "a console without a login must not be reachable from the network"
    );
}

/// 服务活着的时候实例锁在它手里：同一个数据目录起不了第二个实例。
#[tokio::test]
async fn the_server_holds_the_instance_lock() {
    let console = console_with(Store::open_in_memory().unwrap());
    let running = serve(&console, Duration::from_millis(50)).await;

    assert!(
        InstanceLock::acquire(running._directory.path()).is_err(),
        "a second instance must not get the same data directory"
    );
    running.shutdown().await;
}

/// 重启之后：先撤掉遗留挂单，再按保存的状态重新挂单（第 12 节，B19）。
#[tokio::test]
async fn b19_the_server_restores_running_strategies_before_driving_them() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("poise.sqlite");
    let exchange = common::exchange();
    let account = common::account();

    // 第一个进程：建策略、挂单，然后退出。
    let first = console_over(
        Store::open(&path).unwrap(),
        exchange.clone(),
        account.clone(),
        Arc::new(ManualClock::new(at(0))),
    );
    let draft_id = first.runtime.create_draft(long_grid_config()).unwrap();
    let preview = first.runtime.preview_draft(&draft_id).await.unwrap();
    first
        .runtime
        .start_draft(&draft_id, preview.token.as_deref().unwrap())
        .await
        .unwrap();
    first.clock.set(at(1));
    first.runtime.step(&draft_id).await.unwrap();
    assert_eq!(exchange.place_count(), 20);
    let old: Vec<_> = exchange
        .orders()
        .into_iter()
        .filter_map(|order| order.client_order_id)
        .collect();
    drop(first);

    // 进程不在的时候行情跌了，一格整份成交。
    exchange
        .fill(&old[0], Quantity::parse("0.05").unwrap(), at(10))
        .unwrap();
    exchange.set_market_price(Price::parse("94500").unwrap(), at(10));

    // 第二个进程：同一个交易所、同一个数据库，时钟已经走过 expiresAfter。
    let second = console_over(
        Store::open(&path).unwrap(),
        exchange.clone(),
        account,
        Arc::new(ManualClock::new(at(20))),
    );
    let running = serve(&second, Duration::from_millis(50)).await;

    let restored = wait_for(|| {
        let exchange = exchange.clone();
        let old = old.clone();
        async move {
            exchange.place_count() > 20
                && exchange.orders().iter().all(|order| {
                    order
                        .client_order_id
                        .as_ref()
                        .is_none_or(|client_order_id| !old.contains(client_order_id))
                        || !order.status.is_working()
                })
        }
    })
    .await;
    assert!(restored, "the leftover orders must be replaced on startup");
    // 离线期间的成交也补记了。
    assert_eq!(
        second
            .runtime
            .state(&draft_id)
            .unwrap()
            .side(poise_strategy::side::Direction::Long)
            .unwrap()
            .cell(poise_strategy::grid::CellIndex(0))
            .unwrap()
            .to_buy_back,
        Quantity::ZERO
    );

    running.shutdown().await;
}

/// 恢复没走通只影响这一个策略：它留在「恢复中」不下新单，网页上看得到原因，
/// 后台一轮一轮继续试，试通了就接着跑。
///
/// 恢复失败不能拖住控制台，也不能让整个进程退出——一次撤单被交易所拒收只是这一轮没走通，
/// 用户还要能打开网页看到发生了什么。
#[tokio::test]
async fn b19_a_strategy_that_cannot_be_restored_stays_visible_and_keeps_retrying() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("poise.sqlite");
    let exchange = common::exchange();
    let account = common::account();

    // 第一个进程：建策略、挂单，然后退出。
    let first = console_over(
        Store::open(&path).unwrap(),
        exchange.clone(),
        account.clone(),
        Arc::new(ManualClock::new(at(0))),
    );
    let draft_id = first.runtime.create_draft(long_grid_config()).unwrap();
    let preview = first.runtime.preview_draft(&draft_id).await.unwrap();
    first
        .runtime
        .start_draft(&draft_id, preview.token.as_deref().unwrap())
        .await
        .unwrap();
    first.clock.set(at(1));
    first.runtime.step(&draft_id).await.unwrap();
    assert_eq!(exchange.place_count(), 20);
    drop(first);

    // 第二个进程：交易所一直拒收撤单，恢复走不通。拒收要一直持续到下面看完「恢复中」
    // 为止：后台每 20 毫秒重试一次，只拒一次的话，GET 赶到之前它可能已经恢复了。
    exchange.reject_next_cancels(usize::MAX, "rate limited");
    let runtime = Arc::new(Runtime::with_clock(
        exchange.clone(),
        account,
        Store::open(&path).unwrap(),
        RuntimeOptions::default(),
        Arc::new(ManualClock::new(at(20))),
    ));
    let running = serve_runtime(runtime.clone(), Duration::from_millis(20)).await;

    // 恢复没走通：状态留在「恢复中」，原因写在快照里。
    let stuck = wait_for(|| {
        let runtime = runtime.clone();
        let draft_id = draft_id.clone();
        async move {
            runtime.status(&draft_id).unwrap() == StrategyStatus::Restoring
                && runtime
                    .detail(&draft_id)
                    .unwrap()
                    .note
                    .is_some_and(|note| note.contains("恢复失败"))
        }
    })
    .await;
    assert!(
        stuck,
        "a recovery that cannot finish must stay visible as `restoring`"
    );
    // 恢复中一张新单都不下，遗留挂单还在交易所上。
    assert_eq!(exchange.place_count(), 20, "no new orders while restoring");

    // 控制台照常答话，并且如实说这个策略在恢复中、为什么。
    let response = get(running.address, &format!("/api/strategies/{draft_id}")).await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.contains("\"status\":\"restoring\""), "{response}");
    assert!(response.contains("恢复失败"), "{response}");

    // 交易所恢复正常，后台继续重试：撤单成功，遗留挂单换掉，策略接着跑。
    exchange.reject_next_cancels(0, "");
    let recovered = wait_for(|| {
        let exchange = exchange.clone();
        let runtime = runtime.clone();
        let draft_id = draft_id.clone();
        async move {
            exchange.place_count() > 20
                && runtime.status(&draft_id).unwrap() == StrategyStatus::Running
        }
    })
    .await;
    assert!(
        recovered,
        "the background must keep retrying until it works"
    );

    running.shutdown().await;
}

/// 停止中的策略由常驻服务收尾：撤不干净时不归档，后台每一轮继续撤、继续对账，
/// 撤干净了才归档。
#[tokio::test]
async fn a_stopping_strategy_is_settled_by_the_background() {
    let console = console_with(Store::open_in_memory().unwrap());
    let draft_id = console.runtime.create_draft(long_grid_config()).unwrap();
    let preview = console.runtime.preview_draft(&draft_id).await.unwrap();
    console
        .runtime
        .start_draft(&draft_id, preview.token.as_deref().unwrap())
        .await
        .unwrap();
    console.clock.set(at(1));
    console.runtime.step(&draft_id).await.unwrap();

    // 有一张单的撤单回执丢在路上：我们不知道它撤掉了没有，状态停在「停止中」。
    console.exchange.unknown_next_cancel("timed out");
    let canceled = console.runtime.stop(&draft_id).await.unwrap();
    assert_eq!(canceled.len(), 19);
    assert_eq!(
        console.runtime.status(&draft_id).unwrap(),
        StrategyStatus::Stopping
    );
    // 交易所上那张单其实已经不在场了：撤单请求到了，只是回执丢了。
    let leftover = console
        .runtime
        .orders(&draft_id)
        .unwrap()
        .into_iter()
        .find(|order| order.record.is_working())
        .unwrap()
        .record
        .client_order_id;
    assert!(console.exchange.cancel_externally(&leftover));

    let running = serve(&console, Duration::from_millis(20)).await;
    let archived = wait_for(|| {
        let runtime = console.runtime.clone();
        let draft_id = draft_id.clone();
        async move { runtime.status(&draft_id).unwrap() == StrategyStatus::Stopped }
    })
    .await;

    assert!(
        archived,
        "the background must keep settling a stopping strategy"
    );
    running.shutdown().await;
}

/// 运行中的策略按节拍被驱动：服务起来之后不用谁去点，订单自己挂上。
#[tokio::test]
async fn the_server_drives_running_strategies_on_a_timer() {
    let directory = tempfile::tempdir().unwrap();
    let console = console_with(Store::open(directory.path().join("poise.sqlite")).unwrap());
    let draft_id = console.runtime.create_draft(long_grid_config()).unwrap();
    let preview = console.runtime.preview_draft(&draft_id).await.unwrap();
    console
        .runtime
        .start_draft(&draft_id, preview.token.as_deref().unwrap())
        .await
        .unwrap();
    assert_eq!(console.exchange.place_count(), 0);

    let running = serve(&console, Duration::from_millis(20)).await;

    let placed = wait_for(|| {
        let exchange = console.exchange.clone();
        async move { exchange.place_count() >= 20 }
    })
    .await;
    assert!(
        placed,
        "the running strategy must be driven without being asked"
    );

    running.shutdown().await;
}

/// 暂停的策略不该被服务驱动：暂停之后不会自己挂回来。
#[tokio::test]
async fn the_server_does_not_drive_paused_strategies() {
    let directory = tempfile::tempdir().unwrap();
    let console = console_with(Store::open(directory.path().join("poise.sqlite")).unwrap());
    let draft_id = console.runtime.create_draft(long_grid_config()).unwrap();
    let preview = console.runtime.preview_draft(&draft_id).await.unwrap();
    console
        .runtime
        .start_draft(&draft_id, preview.token.as_deref().unwrap())
        .await
        .unwrap();
    console.clock.set(at(1));
    console.runtime.step(&draft_id).await.unwrap();
    console.clock.set(at(2));
    console.runtime.pause(&draft_id).await.unwrap();
    let placed = console.exchange.place_count();

    let running = serve(&console, Duration::from_millis(20)).await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    assert_eq!(console.exchange.place_count(), placed);
    assert!(
        console
            .exchange
            .orders()
            .iter()
            .all(|order| !order.status.is_working())
    );
    running.shutdown().await;
}

/// 等到条件成立，最多等 5 秒。
async fn wait_for<F, Fut>(mut check: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        if check().await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}
