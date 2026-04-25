use std::sync::Arc;

use poise_engine::observation::MarketObservation;
use poise_engine::ports::MarketDataTick;
use tokio::sync::watch;
use tokio::task::{JoinHandle, JoinSet};

use super::ServerRuntime;

pub(super) fn spawn_market_task(
    runtime: &ServerRuntime,
    shutdown_rx: watch::Receiver<bool>,
) -> JoinHandle<()> {
    let state = runtime.state.clone();
    let market_data = Arc::clone(&runtime.market_data);
    let market_data_health_state = Arc::clone(&runtime.market_data_health_state);

    tokio::spawn(async move {
        let tracks = state
            .reconcile
            .observation_service
            .track_instruments()
            .await;
        tracing::info!("starting market data task for {} tracks", tracks.len());
        let mut workers = JoinSet::new();

        for track in tracks {
            if *shutdown_rx.borrow() {
                break;
            }

            let instrument = track.instrument.clone();
            match market_data.subscribe_prices(&instrument).await {
                Ok(mut receiver) => {
                    tracing::info!(
                        "subscribed market data for track {} ({})",
                        track.id,
                        instrument.symbol
                    );
                    let state = state.clone();
                    let market_data_health_state = Arc::clone(&market_data_health_state);
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
                                        .observation_service
                                        .observe_market(&track.id, observation)
                                        .await
                                    {
                                        Ok(_) => {
                                            let _ = state
                                                .live_view_notifications
                                                .send(track.id.clone());
                                            market_data_health_state.mark_dirty(&track.id);
                                        }
                                        Err(error) => {
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
                    tracing::warn!(
                        "failed to subscribe market data for {}: {error}",
                        instrument.symbol
                    );
                }
            }
        }

        while let Some(result) = workers.join_next().await {
            if let Err(error) = result {
                tracing::warn!("market worker join error: {error}");
            }
        }
    })
}
