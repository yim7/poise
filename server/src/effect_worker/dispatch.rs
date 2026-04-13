use std::collections::HashSet;

use anyhow::Result;
use poise_application::PersistedTrackEffect;
use poise_engine::transition::TrackEffect;

use super::{Cancellation, EffectWorker};

pub(super) async fn run_once(worker: &EffectWorker) -> Result<()> {
    let mut seen_effects = HashSet::new();

    loop {
        if *worker.shutdown_rx.borrow() {
            break;
        }

        let Some(effect) = worker
            .state
            .reconcile
            .effect_store
            .list_dispatchable_effects()
            .await?
            .into_iter()
            .find(|effect| !seen_effects.contains(&effect.effect_id))
        else {
            break;
        };
        let effect_id = effect.effect_id.clone();
        if let Err(error) = worker.process_effect(effect).await {
            tracing::warn!("failed to process persisted effect: {error}");
        }
        seen_effects.insert(effect_id);
    }

    Ok(())
}

pub(super) async fn process_effect(
    worker: &EffectWorker,
    persisted: PersistedTrackEffect,
) -> Result<()> {
    match persisted.effect {
        TrackEffect::SubmitOrder {
            ref request,
            ref desired_exposure,
            ..
        } => {
            worker
                .execute_submit(&persisted, request.clone(), desired_exposure.clone())
                .await
        }
        TrackEffect::CancelOrder {
            ref instrument,
            ref order_id,
        } => {
            worker
                .execute_cancellation(
                    &persisted,
                    Cancellation::One {
                        instrument: instrument.clone(),
                        order_id: order_id.clone(),
                    },
                )
                .await
        }
        TrackEffect::CancelAll { ref instrument } => {
            worker
                .execute_cancellation(
                    &persisted,
                    Cancellation::All {
                        instrument: instrument.clone(),
                    },
                )
                .await
        }
        TrackEffect::NoOp => {
            worker
                .state
                .effect_service
                .complete_effect_succeeded(persisted.track_id.as_str(), &persisted.effect_id)
                .await?;
            Ok(())
        }
    }
}
