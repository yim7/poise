use std::collections::BTreeSet;
use std::sync::Arc;

use anyhow::Result;
use poise_application::TrackQueryStore;
use poise_core::track::{Instrument, TrackId};
use poise_engine::ledger::TrackPnlRecord;
use poise_engine::ports::AccountPort;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecentFillCoverage {
    Recorded,
    Missing,
    Unkeyed,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RecentFillAuditItem {
    pub record: TrackPnlRecord,
    pub coverage: RecentFillCoverage,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct RecentFillsAudit {
    pub records_seen: usize,
    pub records_recorded: usize,
    pub records_missing: usize,
    pub records_unkeyed: usize,
    pub items: Vec<RecentFillAuditItem>,
}

pub(crate) struct RecentFillsAuditor {
    account: Arc<dyn AccountPort>,
    query_store: Arc<dyn TrackQueryStore>,
}

impl RecentFillsAuditor {
    pub(crate) fn new(
        account: Arc<dyn AccountPort>,
        query_store: Arc<dyn TrackQueryStore>,
    ) -> Self {
        Self {
            account,
            query_store,
        }
    }

    pub(crate) async fn audit_recent_fills(
        &self,
        track_id: &TrackId,
        instrument: &Instrument,
    ) -> Result<RecentFillsAudit> {
        let records = self
            .account
            .get_recent_track_pnl_records(instrument)
            .await?;
        let source_keys = records
            .iter()
            .filter_map(normalized_source_key)
            .collect::<Vec<_>>();
        let local_source_keys = if source_keys.is_empty() {
            BTreeSet::new()
        } else {
            self.query_store
                .list_track_pnl_source_keys(track_id, &source_keys)
                .await?
                .into_iter()
                .collect::<BTreeSet<_>>()
        };

        let mut audit = RecentFillsAudit {
            records_seen: records.len(),
            ..RecentFillsAudit::default()
        };
        for record in records {
            let coverage = match normalized_source_key(&record) {
                Some(source_key) if local_source_keys.contains(&source_key) => {
                    audit.records_recorded += 1;
                    RecentFillCoverage::Recorded
                }
                Some(_) => {
                    audit.records_missing += 1;
                    RecentFillCoverage::Missing
                }
                None => {
                    audit.records_unkeyed += 1;
                    RecentFillCoverage::Unkeyed
                }
            };
            audit.items.push(RecentFillAuditItem { record, coverage });
        }

        Ok(audit)
    }
}

fn normalized_source_key(record: &TrackPnlRecord) -> Option<String> {
    record
        .source_key
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use anyhow::Result;
    use chrono::{TimeZone, Utc};
    use poise_application::{TrackMutationStore, TrackQueryStore};
    use poise_core::track::{Instrument, TrackId, Venue};
    use poise_core::types::Side;
    use poise_engine::ledger::TrackPnlRecord;
    use poise_engine::ports::{AccountCapacitySnapshot, AccountPort, UserDataEvent};
    use poise_storage::sqlite::SqliteStorage;
    use tokio::sync::mpsc;

    use super::{RecentFillCoverage, RecentFillsAuditor};

    #[tokio::test]
    async fn recent_fills_audit_marks_recorded_missing_and_unkeyed_records() {
        let repository = Arc::new(SqliteStorage::in_memory().unwrap());
        let track_id = TrackId::new("btc-core");
        let instrument = Instrument::new(Venue::Okx, "BTC-USD-SWAP");
        let recorded = trade_record(
            instrument.clone(),
            Some("okx:fills:recorded"),
            Some("recorded"),
        );
        TrackMutationStore::insert_track_pnl_record(&*repository, &track_id, &recorded)
            .await
            .unwrap();

        let missing = trade_record(
            instrument.clone(),
            Some("okx:fills:missing"),
            Some("missing"),
        );
        let unkeyed = trade_record(instrument.clone(), None, Some("unkeyed"));
        let account = Arc::new(FakeRecentFillsAccount {
            records: vec![recorded, missing, unkeyed],
        });
        let auditor =
            RecentFillsAuditor::new(account, repository.clone() as Arc<dyn TrackQueryStore>);

        let audit = auditor
            .audit_recent_fills(&track_id, &instrument)
            .await
            .unwrap();

        assert_eq!(audit.records_seen, 3);
        assert_eq!(audit.records_recorded, 1);
        assert_eq!(audit.records_missing, 1);
        assert_eq!(audit.records_unkeyed, 1);
        assert_eq!(audit.items[0].coverage, RecentFillCoverage::Recorded);
        assert_eq!(audit.items[1].coverage, RecentFillCoverage::Missing);
        assert_eq!(audit.items[2].coverage, RecentFillCoverage::Unkeyed);
    }

    fn trade_record(
        instrument: Instrument,
        source_key: Option<&str>,
        trade_id: Option<&str>,
    ) -> TrackPnlRecord {
        TrackPnlRecord::trade(
            instrument,
            Utc.with_ymd_and_hms(2026, 6, 17, 1, 2, 3).unwrap(),
            "okx:fills".to_string(),
            source_key.map(str::to_string),
            Some("order-1".to_string()),
            trade_id.map(str::to_string),
            Side::Sell,
            62_000.0,
            0.2,
            0.0001,
            0.00001,
            "BTC",
        )
    }

    struct FakeRecentFillsAccount {
        records: Vec<TrackPnlRecord>,
    }

    #[async_trait::async_trait]
    impl AccountPort for FakeRecentFillsAccount {
        async fn get_account_capacity_snapshot(
            &self,
            _instrument: &Instrument,
        ) -> Result<AccountCapacitySnapshot> {
            Ok(AccountCapacitySnapshot {
                max_increase_notional: 1_000_000.0,
            })
        }

        async fn get_recent_track_pnl_records(
            &self,
            _instrument: &Instrument,
        ) -> Result<Vec<TrackPnlRecord>> {
            Ok(self.records.clone())
        }

        async fn subscribe_user_data(&self) -> Result<mpsc::Receiver<UserDataEvent>> {
            let (_sender, receiver) = mpsc::channel(1);
            Ok(receiver)
        }
    }
}
