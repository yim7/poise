use std::sync::Arc;

use chrono::{DateTime, Utc};
use poise_engine::ports::UserDataEvent;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use super::{ServerRuntime, apply_user_data_event};

pub(super) fn spawn_user_task(
    runtime: &ServerRuntime,
    mut receiver: mpsc::Receiver<UserDataEvent>,
    startup_cutoff: DateTime<Utc>,
    mut shutdown_rx: watch::Receiver<bool>,
) -> JoinHandle<()> {
    let state = runtime.state.clone();
    let execution = Arc::clone(&runtime.execution);

    tokio::spawn(async move {
        loop {
            if *shutdown_rx.borrow() {
                break;
            }

            let event = tokio::select! {
                biased;
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        break;
                    }
                    continue;
                }
                event = receiver.recv() => event,
            };

            let Some(event) = event else {
                break;
            };

            if event.event_time <= startup_cutoff {
                continue;
            }

            let instrument = event.instrument().clone();
            let Some(track_id) = state
                .reconcile
                .observation_service
                .resolve_track_id(&instrument)
                .await
            else {
                tracing::warn!(
                    "received user data for unknown instrument {}:{}",
                    instrument.venue.as_str(),
                    instrument.symbol
                );
                continue;
            };
            if let Err(error) = apply_user_data_event(
                &state.reconcile,
                execution.as_ref(),
                &track_id,
                event,
            )
            .await
            {
                tracing::warn!(
                    "failed to apply user data update for {}: {}",
                    instrument.symbol,
                    error.message()
                );
                continue;
            }
        }
    })
}
