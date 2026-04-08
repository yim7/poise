use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use super::ServerRuntime;

pub(super) fn spawn_account_task(
    runtime: &ServerRuntime,
    mut shutdown_rx: watch::Receiver<bool>,
) -> JoinHandle<()> {
    let state = runtime.state.clone();
    let refresh_interval = runtime.account_refresh_interval;
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(refresh_interval);
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        break;
                    }
                }
                _ = interval.tick() => {
                    if let Err(error) = state.account_monitor.refresh_once().await {
                        tracing::warn!("account monitor refresh failed: {error}");
                    }
                }
            }
        }
    })
}
