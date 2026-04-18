use std::collections::HashMap;
use std::future::Future;

use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use poise_engine::ports::{AccountCapacitySnapshot, UserDataEvent};
use poise_engine::track::Instrument;
use tokio::sync::mpsc;
use tokio::time::sleep;

use super::{
    RuntimeStartupCapacityMode, RuntimeStartupDefinition, STARTUP_RETRY_ATTEMPTS,
    STARTUP_RETRY_DELAY, ServerRuntime, exchange_state,
};

struct TrackStartupSeed {
    track_id: String,
    position: poise_engine::observation::PositionObservation,
    open_orders: Vec<poise_engine::observation::OrderObservation>,
}

pub(super) async fn complete_startup(
    runtime: &ServerRuntime,
    receiver: &mut mpsc::Receiver<UserDataEvent>,
    startup_cutoff: DateTime<Utc>,
) -> Result<()> {
    let mut account_capacity_snapshots: HashMap<Instrument, AccountCapacitySnapshot> =
        HashMap::new();
    let mut track_seeds = Vec::new();

    for track in &runtime.startup_definitions {
        let instrument = track.instrument().clone();
        let position = retry_startup_step("get_position", || {
            runtime.execution.get_position(&instrument)
        })
        .await?;
        let open_orders = retry_startup_step("get_open_orders", || {
            runtime.execution.get_open_orders(&instrument)
        })
        .await?;
        let account_capacity_snapshot = probe_startup_account_capacity(runtime, track).await?;

        let required_additional_notional = track.required_additional_notional(position.qty);
        if required_additional_notional > account_capacity_snapshot.max_increase_notional {
            return Err(anyhow!(
                "insufficient account margin for configured max_notional on track `{}`: required {}, available {}",
                track.track_id().as_str(),
                required_additional_notional,
                account_capacity_snapshot.max_increase_notional
            ));
        }

        account_capacity_snapshots.insert(instrument, account_capacity_snapshot);
        track_seeds.push(TrackStartupSeed {
            track_id: track.track_id().as_str().to_string(),
            position: exchange_state::position_observation(&position),
            open_orders: open_orders
                .iter()
                .map(exchange_state::order_observation)
                .collect(),
        });
    }

    runtime
        .state
        .account_margin_guard
        .replace_snapshots(account_capacity_snapshots);

    for seed in track_seeds {
        runtime
            .state
            .reconcile
            .observation_service
            .sync_exchange_state(&seed.track_id, seed.position, seed.open_orders)
            .await?;
    }

    replay_startup_user_data(runtime, receiver, startup_cutoff).await?;
    seed_startup_pending_submit_effects(runtime).await
}

async fn probe_startup_account_capacity(
    runtime: &ServerRuntime,
    track: &RuntimeStartupDefinition,
) -> Result<AccountCapacitySnapshot> {
    match track.startup_capacity_mode() {
        RuntimeStartupCapacityMode::AvailableBalanceTimesLeverage { leverage } => {
            let summary = retry_startup_step("get_account_summary", || {
                runtime.account_summary.get_account_summary()
            })
            .await?;
            Ok(AccountCapacitySnapshot {
                max_increase_notional: summary.available * *leverage as f64,
            })
        }
        RuntimeStartupCapacityMode::AccountCapacitySnapshot => {
            let instrument = track.instrument().clone();
            retry_startup_step("get_account_capacity_snapshot", || {
                runtime.account.get_account_capacity_snapshot(&instrument)
            })
            .await
        }
    }
}

pub(super) async fn replay_startup_user_data(
    runtime: &ServerRuntime,
    receiver: &mut mpsc::Receiver<UserDataEvent>,
    startup_cutoff: DateTime<Utc>,
) -> Result<()> {
    let mut buffered_events = Vec::new();
    while let Ok(event) = receiver.try_recv() {
        buffered_events.push(event);
    }

    buffered_events.sort_by_key(|event| event.event_time);
    for event in buffered_events {
        if event.event_time > startup_cutoff {
            let instrument = event.instrument().clone();
            let Some(track_id) = runtime
                .state
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
            exchange_state::apply_user_data_event(
                &runtime.state.reconcile,
                runtime.execution.as_ref(),
                &track_id,
                event,
            )
            .await
            .map_err(super::mutate_error)?;
        }
    }

    Ok(())
}

async fn seed_startup_pending_submit_effects(runtime: &ServerRuntime) -> Result<()> {
    let startup_pending_submit_effects = runtime
        .state
        .reconcile
        .effect_store
        .list_all_pending_submit_effects()
        .await?;
    runtime
        .state
        .reconcile
        .submit_preflight
        .seed_startup_pending_submit_effects(
            startup_pending_submit_effects
                .into_iter()
                .map(|effect| effect.effect_id),
        )
        .await;
    Ok(())
}

pub(super) async fn retry_startup_step<T, F, Fut>(
    step_name: &'static str,
    mut operation: F,
) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let mut last_error = None;

    for attempt in 0..STARTUP_RETRY_ATTEMPTS {
        match operation().await {
            Ok(value) => return Ok(value),
            Err(error) => {
                if attempt + 1 == STARTUP_RETRY_ATTEMPTS {
                    return Err(error);
                }
                tracing::warn!(
                    step = step_name,
                    attempt = attempt + 1,
                    max_attempts = STARTUP_RETRY_ATTEMPTS,
                    "startup step failed: {error}"
                );
                last_error = Some(error);
            }
        }

        sleep(STARTUP_RETRY_DELAY).await;
    }

    Err(last_error.unwrap_or_else(|| anyhow!("startup step `{step_name}` failed")))
}
