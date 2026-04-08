use std::sync::Arc;

use chrono::{DateTime, Utc};
use poise_engine::ports::AccountCapacitySnapshot;
use poise_engine::runtime::AccountCapacityConstraint;
use poise_engine::track::Instrument;
use tokio::sync::{Mutex, OwnedMutexGuard};

#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct AccountMarginBlock {
    pub increase_blocked: bool,
    pub blocked_reason: Option<String>,
    pub blocked_at: Option<DateTime<Utc>>,
}

#[derive(Default)]
pub struct AccountMarginGuardStore {
    snapshots_by_instrument:
        std::sync::Mutex<std::collections::HashMap<Instrument, AccountCapacitySnapshot>>,
    block: std::sync::Mutex<AccountMarginBlock>,
}

impl AccountMarginGuardStore {
    pub(crate) fn replace_snapshots(
        &self,
        snapshots: std::collections::HashMap<Instrument, AccountCapacitySnapshot>,
    ) {
        let mut stored_snapshots = self.snapshots_by_instrument.lock().unwrap();
        stored_snapshots.extend(snapshots);
    }

    pub(crate) fn update_snapshot(
        &self,
        instrument: Instrument,
        snapshot: AccountCapacitySnapshot,
    ) {
        self.snapshots_by_instrument
            .lock()
            .unwrap()
            .insert(instrument, snapshot);
    }

    pub(crate) fn activate_insufficient_margin(
        &self,
        _instrument: &Instrument,
        reason: impl Into<String>,
        blocked_at: DateTime<Utc>,
    ) {
        let reason = reason.into();
        *self.block.lock().unwrap() = AccountMarginBlock {
            increase_blocked: true,
            blocked_reason: Some(reason),
            blocked_at: Some(blocked_at),
        };
    }

    pub(crate) fn constraint_for(&self, instrument: &Instrument) -> AccountCapacityConstraint {
        let snapshot = self
            .snapshots_by_instrument
            .lock()
            .unwrap()
            .get(instrument)
            .cloned();
        let block = self.block.lock().unwrap().clone();

        AccountCapacityConstraint {
            increase_blocked: block.increase_blocked,
            blocked_reason: block.blocked_reason,
            max_increase_notional: snapshot.map(|snapshot| snapshot.max_increase_notional),
        }
    }
}

impl poise_application::AccountCapacityGuard for AccountMarginGuardStore {
    fn constraint_for(&self, instrument: &Instrument) -> AccountCapacityConstraint {
        self.constraint_for(instrument)
    }
}

#[derive(Default)]
pub struct TrackReconcileGuards {
    locks: Mutex<std::collections::HashMap<String, Arc<Mutex<()>>>>,
}

impl TrackReconcileGuards {
    pub async fn lock(&self, track_id: &str) -> OwnedMutexGuard<()> {
        let lock = {
            let mut locks = self.locks.lock().await;
            Arc::clone(
                locks
                    .entry(track_id.to_string())
                    .or_insert_with(|| Arc::new(Mutex::new(()))),
            )
        };

        lock.lock_owned().await
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use poise_engine::track::{Instrument, Venue};

    use super::*;

    #[test]
    fn account_margin_guard_tracks_single_exchange_block_without_venue_bucket() {
        let store = AccountMarginGuardStore::default();
        let instrument = Instrument::new(Venue::Binance, "BTCUSDT");

        store.activate_insufficient_margin(&instrument, "insufficient margin", Utc::now());
        let constraint = store.constraint_for(&instrument);

        assert!(constraint.increase_blocked);
        assert!(store.block.lock().unwrap().increase_blocked);
    }
}
