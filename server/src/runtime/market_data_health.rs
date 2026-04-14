use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::sync::{Notify, watch};
use tokio::task::JoinHandle;

use crate::server_context::ReconcileState;

use super::ServerRuntime;

#[derive(Default)]
pub(crate) struct MarketDataHealthState {
    dirty_tracks: Mutex<HashSet<String>>,
    notify: Notify,
}

impl MarketDataHealthState {
    pub(crate) fn mark_dirty(&self, track_id: &str) {
        self.dirty_tracks
            .lock()
            .unwrap()
            .insert(track_id.to_string());
        self.notify.notify_one();
    }

    fn take_dirty(&self) -> HashSet<String> {
        std::mem::take(&mut *self.dirty_tracks.lock().unwrap())
    }

    async fn wait(&self) {
        self.notify.notified().await;
    }
}

pub(super) fn spawn_market_data_health_task(
    runtime: &ServerRuntime,
    mut shutdown_rx: watch::Receiver<bool>,
) -> JoinHandle<()> {
    let state = runtime.state.clone();
    let clock = Arc::clone(&runtime.clock);
    let market_data_health_state = Arc::clone(&runtime.market_data_health_state);
    let max_sleep_interval = runtime.market_data_health_max_sleep_interval;

    tokio::spawn(async move {
        let tracks = state
            .reconcile
            .observation_service
            .track_instruments()
            .await;
        let mut deadlines = HashMap::new();
        for track in &tracks {
            update_deadline(
                &state.reconcile,
                clock.as_ref(),
                &track.id,
                max_sleep_interval,
                &mut deadlines,
            )
            .await;
        }

        loop {
            if *shutdown_rx.borrow() {
                break;
            }

            apply_dirty_tracks(
                &state.reconcile,
                clock.as_ref(),
                &market_data_health_state,
                max_sleep_interval,
                &mut deadlines,
            )
            .await;

            let now = clock.now();
            let due_track_ids = collect_due_track_ids(&deadlines, now);
            if !due_track_ids.is_empty() {
                refresh_due_tracks(
                    &state.reconcile,
                    clock.as_ref(),
                    &due_track_ids,
                    max_sleep_interval,
                    &mut deadlines,
                )
                .await;
                continue;
            }

            if let Some(sleep_for) = next_sleep_interval(&deadlines, now, max_sleep_interval) {
                tokio::select! {
                    biased;
                    changed = shutdown_rx.changed() => {
                        if changed.is_err() || *shutdown_rx.borrow() {
                            break;
                        }
                    }
                    _ = market_data_health_state.wait() => {}
                    _ = tokio::time::sleep(sleep_for) => {}
                }
            } else {
                tokio::select! {
                    biased;
                    changed = shutdown_rx.changed() => {
                        if changed.is_err() || *shutdown_rx.borrow() {
                            break;
                        }
                    }
                    _ = market_data_health_state.wait() => {}
                }
            }
        }
    })
}

async fn apply_dirty_tracks(
    state: &ReconcileState,
    clock: &dyn poise_engine::ports::ClockPort,
    market_data_health_state: &MarketDataHealthState,
    max_sleep_interval: Duration,
    deadlines: &mut HashMap<String, DateTime<Utc>>,
) {
    for track_id in market_data_health_state.take_dirty() {
        update_deadline(state, clock, &track_id, max_sleep_interval, deadlines).await;
    }
}

async fn refresh_due_tracks(
    state: &ReconcileState,
    clock: &dyn poise_engine::ports::ClockPort,
    due_track_ids: &[String],
    max_sleep_interval: Duration,
    deadlines: &mut HashMap<String, DateTime<Utc>>,
) {
    for track_id in due_track_ids {
        if let Err(error) = state
            .observation_service
            .refresh_market_data_health(track_id)
            .await
        {
            tracing::warn!("failed to refresh market data health for {track_id}: {error}");
            deadlines.insert(
                track_id.clone(),
                retry_deadline(clock.now(), max_sleep_interval),
            );
            continue;
        }

        update_deadline(state, clock, track_id, max_sleep_interval, deadlines).await;
    }
}

async fn update_deadline(
    state: &ReconcileState,
    clock: &dyn poise_engine::ports::ClockPort,
    track_id: &str,
    max_sleep_interval: Duration,
    deadlines: &mut HashMap<String, DateTime<Utc>>,
) {
    match state
        .observation_service
        .market_data_health_deadline(track_id)
        .await
    {
        Ok(Some(deadline)) => {
            deadlines.insert(track_id.to_string(), deadline);
        }
        Ok(None) => {
            deadlines.remove(track_id);
        }
        Err(error) => {
            tracing::warn!("failed to load market data health deadline for {track_id}: {error}");
            deadlines.insert(
                track_id.to_string(),
                retry_deadline(clock.now(), max_sleep_interval),
            );
        }
    }
}

fn collect_due_track_ids(
    deadlines: &HashMap<String, DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Vec<String> {
    deadlines
        .iter()
        .filter(|(_, deadline)| **deadline <= now)
        .map(|(track_id, _)| track_id.clone())
        .collect()
}

fn next_sleep_interval(
    deadlines: &HashMap<String, DateTime<Utc>>,
    now: DateTime<Utc>,
    max_sleep_interval: Duration,
) -> Option<Duration> {
    let nearest_deadline = deadlines.values().min().copied()?;
    let remaining = nearest_deadline.signed_duration_since(now);
    if remaining <= chrono::Duration::zero() {
        return Some(Duration::ZERO);
    }

    let remaining = remaining.to_std().unwrap_or(max_sleep_interval);
    Some(remaining.min(max_sleep_interval))
}

fn retry_deadline(now: DateTime<Utc>, max_sleep_interval: Duration) -> DateTime<Utc> {
    match chrono::Duration::from_std(max_sleep_interval) {
        Ok(interval) => now + interval,
        Err(_) => now + chrono::Duration::seconds(1),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::time::Duration;

    use super::{MarketDataHealthState, next_sleep_interval};
    use chrono::{TimeZone, Utc};

    #[test]
    fn market_data_health_state_coalesces_dirty_track_ids() {
        let state = MarketDataHealthState::default();

        state.mark_dirty("BTCUSDT");
        state.mark_dirty("BTCUSDT");
        state.mark_dirty("ETHUSDT");

        assert_eq!(
            state.take_dirty(),
            HashSet::from(["BTCUSDT".to_string(), "ETHUSDT".to_string()])
        );
        assert!(state.take_dirty().is_empty());
    }

    #[test]
    fn next_sleep_interval_returns_none_without_deadlines() {
        assert_eq!(
            next_sleep_interval(
                &HashMap::new(),
                Utc.with_ymd_and_hms(2026, 4, 15, 8, 0, 0).unwrap(),
                Duration::from_secs(1),
            ),
            None
        );
    }
}
