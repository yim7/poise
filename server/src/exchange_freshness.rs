use std::collections::HashMap;
use std::sync::Mutex;

use poise_engine::transition::TrackEffect;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExchangeFreshnessReason {
    FilledAwaitingSync,
    UnabsorbedOrderUpdate,
    SubmitOutcomeUnknown,
    CancelOutcomeUnknown,
}

#[derive(Debug, Clone)]
struct TrackFreshnessState {
    generation: u64,
    last_reason: ExchangeFreshnessReason,
}

#[derive(Debug, Clone)]
pub struct ExchangeFreshnessSyncToken {
    track_id: String,
    generation: u64,
}

#[derive(Default)]
pub struct ExchangeFreshness {
    inner: Mutex<HashMap<String, TrackFreshnessState>>,
}

impl ExchangeFreshness {
    pub async fn mark_stale(&self, track_id: &str, reason: ExchangeFreshnessReason) {
        let mut inner = self.inner.lock().unwrap();
        let next_generation = inner
            .get(track_id)
            .map(|state| state.generation + 1)
            .unwrap_or(1);
        inner.insert(
            track_id.to_string(),
            TrackFreshnessState {
                generation: next_generation,
                last_reason: reason,
            },
        );
    }

    pub async fn prepare_sync(&self, track_id: &str) -> ExchangeFreshnessSyncToken {
        let generation = self
            .inner
            .lock()
            .unwrap()
            .get(track_id)
            .map(|state| state.generation)
            .unwrap_or(0);
        ExchangeFreshnessSyncToken {
            track_id: track_id.to_string(),
            generation,
        }
    }

    pub async fn clear_if_current(&self, token: ExchangeFreshnessSyncToken) {
        let mut inner = self.inner.lock().unwrap();
        let should_clear = inner
            .get(token.track_id.as_str())
            .map(|state| state.generation == token.generation)
            .unwrap_or(false);
        if should_clear {
            inner.remove(token.track_id.as_str());
        }
    }

    pub async fn is_stale(&self, track_id: &str) -> bool {
        self.inner.lock().unwrap().contains_key(track_id)
    }

    pub async fn requires_sync_before_effect(&self, track_id: &str, effect: &TrackEffect) -> bool {
        if !self.is_stale(track_id).await {
            return false;
        }

        matches!(
            effect,
            TrackEffect::SubmitOrder { .. } | TrackEffect::CancelOrder { .. }
        )
    }
}

#[cfg(test)]
mod tests {
    use poise_core::types::{Exposure, Side};
    use poise_engine::ports::OrderRequest;
    use poise_engine::track::{Instrument, Venue};
    use poise_engine::transition::TrackEffect;

    use super::{ExchangeFreshness, ExchangeFreshnessReason};

    fn test_instrument() -> Instrument {
        Instrument {
            venue: Venue::Binance,
            symbol: "BTCUSDT".to_string(),
        }
    }

    fn submit_effect() -> TrackEffect {
        TrackEffect::SubmitOrder {
            request: OrderRequest {
                instrument: test_instrument(),
                side: Side::Buy,
                price: 100_000.0,
                quantity: 0.1,
                client_order_id: "freshness-test-submit".to_string(),
                reduce_only: false,
            },
            desired_exposure: Exposure(1.0),
        }
    }

    fn cancel_effect() -> TrackEffect {
        TrackEffect::CancelOrder {
            instrument: test_instrument(),
            order_id: "freshness-test-order".to_string(),
        }
    }

    #[tokio::test]
    async fn freshness_is_fresh_by_default() {
        let freshness = ExchangeFreshness::default();

        assert!(!freshness.is_stale("btc-core").await);
    }

    #[tokio::test]
    async fn mark_stale_sets_track_state_until_cleared() {
        let freshness = ExchangeFreshness::default();

        freshness
            .mark_stale("btc-core", ExchangeFreshnessReason::FilledAwaitingSync)
            .await;

        let token = freshness.prepare_sync("btc-core").await;
        freshness.clear_if_current(token).await;

        assert!(!freshness.is_stale("btc-core").await);
    }

    #[tokio::test]
    async fn stale_track_blocks_submit_and_cancel_effects() {
        let freshness = ExchangeFreshness::default();
        freshness
            .mark_stale("btc-core", ExchangeFreshnessReason::UnabsorbedOrderUpdate)
            .await;

        assert!(
            freshness
                .requires_sync_before_effect("btc-core", &submit_effect())
                .await
        );
        assert!(
            freshness
                .requires_sync_before_effect("btc-core", &cancel_effect())
                .await
        );
    }

    #[tokio::test]
    async fn fresh_track_allows_submit_and_cancel_effects() {
        let freshness = ExchangeFreshness::default();

        assert!(
            !freshness
                .requires_sync_before_effect("btc-core", &submit_effect())
                .await
        );
        assert!(
            !freshness
                .requires_sync_before_effect("btc-core", &cancel_effect())
                .await
        );
    }

    #[tokio::test]
    async fn clear_if_current_does_not_erase_newer_stale_fact() {
        let freshness = ExchangeFreshness::default();
        freshness
            .mark_stale("btc-core", ExchangeFreshnessReason::FilledAwaitingSync)
            .await;
        let token = freshness.prepare_sync("btc-core").await;

        freshness
            .mark_stale("btc-core", ExchangeFreshnessReason::SubmitOutcomeUnknown)
            .await;
        freshness.clear_if_current(token).await;

        assert!(freshness.is_stale("btc-core").await);
    }

    #[tokio::test]
    async fn mark_stale_replaces_reason_with_newer_fact() {
        let freshness = ExchangeFreshness::default();
        freshness
            .mark_stale("btc-core", ExchangeFreshnessReason::FilledAwaitingSync)
            .await;
        freshness
            .mark_stale("btc-core", ExchangeFreshnessReason::CancelOutcomeUnknown)
            .await;

        let inner = freshness.inner.lock().unwrap();
        let state = inner.get("btc-core").expect("track should stay stale");

        assert_eq!(
            state.last_reason,
            ExchangeFreshnessReason::CancelOutcomeUnknown
        );
    }
}
