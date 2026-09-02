use std::sync::Arc;

use poise_application::TrackInput;
use poise_application::exchange::MarketDataTick;
use poise_core::track::TrackId;
use poise_engine::observation::MarketObservation;
use tokio::sync::watch;
use tokio::task::{JoinHandle, JoinSet};

use super::{RuntimeHealthComponent, ServerRuntime};

pub(super) fn spawn_market_task(
    runtime: &ServerRuntime,
    shutdown_rx: watch::Receiver<bool>,
) -> JoinHandle<()> {
    let state = runtime.state.clone();
    let market_data = runtime.exchange.market_data();
    let market_data_health_state = Arc::clone(&runtime.market_data_health_state);
    let runtime_health = Arc::clone(&runtime.state.runtime_health);

    tokio::spawn(async move {
        let tracks = state.reconcile.runtime.track_instruments().await;
        runtime_health.register_track_scopes(
            RuntimeHealthComponent::MarketData,
            tracks.iter().map(|track| track.id.clone()).collect(),
        );
        tracing::info!("starting market data task for {} tracks", tracks.len());
        let mut workers = JoinSet::new();

        for track in tracks {
            if *shutdown_rx.borrow() {
                break;
            }

            let instrument = track.instrument.clone();
            match market_data.subscribe_prices(&instrument).await {
                Ok(mut receiver) => {
                    runtime_health
                        .record_track_success_now(RuntimeHealthComponent::MarketData, &track.id);
                    tracing::info!(
                        "subscribed market data for track {} ({})",
                        track.id,
                        instrument.symbol
                    );
                    let state = state.clone();
                    let market_data_health_state = Arc::clone(&market_data_health_state);
                    let runtime_health = Arc::clone(&runtime_health);
                    let mut worker_shutdown_rx = shutdown_rx.clone();
                    workers.spawn(async move {
                        loop {
                            if *worker_shutdown_rx.borrow() {
                                break;
                            }

                            tokio::select! {
                                biased;
                                changed = worker_shutdown_rx.changed() => {
                                    if changed.is_err() || *worker_shutdown_rx.borrow() {
                                        break;
                                    }
                                }
                                tick = receiver.recv() => {
                                    let Some(tick) = tick else {
                                        runtime_health.record_track_error_now(
                                            RuntimeHealthComponent::MarketData,
                                            &track.id,
                                            format!(
                                                "market data receiver closed for track {} ({})",
                                                track.id,
                                                instrument.symbol
                                            ),
                                        );
                                        tracing::warn!(
                                            "market data receiver closed for track {} ({})",
                                            track.id,
                                            instrument.symbol
                                        );
                                        break;
                                    };

                                    let observation = match tick {
                                        MarketDataTick::ExecutionQuote(tick) => {
                                            MarketObservation::ExecutionQuote {
                                                execution_quote: tick.execution_quote,
                                            }
                                        }
                                        MarketDataTick::MarkPrice(tick) => {
                                            MarketObservation::MarkPrice {
                                                mark_price: tick.mark_price,
                                            }
                                        }
                                    };

                                    match state
                                        .reconcile
                                        .runtime
                                        .handle(
                                            &TrackId::new(&track.id),
                                            TrackInput::Market(observation),
                                        )
                                        .await
                                    {
                                        Ok(_) => {
                                            runtime_health.record_track_success_now(
                                                RuntimeHealthComponent::MarketData,
                                                &track.id,
                                            );
                                            let _ = state
                                                .live_view_notifications
                                                .send(track.id.clone());
                                            market_data_health_state.mark_dirty(&track.id);
                                        }
                                        Err(error) => {
                                            runtime_health.record_track_error_now(
                                                RuntimeHealthComponent::MarketData,
                                                &track.id,
                                                format!(
                                                    "track `{}` ({}): {error}",
                                                    track.id, instrument.symbol
                                                ),
                                            );
                                            tracing::warn!(
                                                "failed to apply market data update for {}: {}",
                                                instrument.symbol,
                                                error
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    });
                }
                Err(error) => {
                    runtime_health.record_track_error_now(
                        RuntimeHealthComponent::MarketData,
                        &track.id,
                        format!("track `{}` ({}): {error}", track.id, instrument.symbol),
                    );
                    tracing::warn!(
                        "failed to subscribe market data for {}: {error}",
                        instrument.symbol
                    );
                }
            }
        }

        while let Some(result) = workers.join_next().await {
            if let Err(error) = result {
                runtime_health.record_error_now(
                    RuntimeHealthComponent::MarketData,
                    format!("market data worker join error: {error}"),
                );
                tracing::warn!("market worker join error: {error}");
            }
        }
    })
}
