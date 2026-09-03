use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;

use chrono::{Duration, Utc};
use poise_application::TrackQueryStore;
use poise_application::exchange::{
    AccountPort, PnlHistoryWindow, RecentPnlCompleteness, RecentTrackPnlRecords,
};
use poise_core::track::{Instrument, TrackId};
use poise_engine::ledger::{TrackPnlRecord, TrackPnlRecordKind};

const RECENT_PNL_LOOKBACK: Duration = Duration::hours(24);

pub(crate) fn required_recent_pnl_window(now: chrono::DateTime<Utc>) -> PnlHistoryWindow {
    PnlHistoryWindow::try_new(now - RECENT_PNL_LOOKBACK, now)
        .expect("fixed recent PNL lookback must form a valid window")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecentPnlAvailability {
    Available,
    Unsupported,
}

#[derive(Debug)]
pub(crate) enum RecentFillsAuditError {
    Exchange(anyhow::Error),
    Store(anyhow::Error),
}

impl fmt::Display for RecentFillsAuditError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exchange(error) => write!(formatter, "recent exchange PNL query failed: {error}"),
            Self::Store(error) => write!(formatter, "local PNL coverage query failed: {error}"),
        }
    }
}

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

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RecentFillsAudit {
    pub availability: RecentPnlAvailability,
    pub required_window: PnlHistoryWindow,
    pub covered_window: Option<PnlHistoryWindow>,
    pub completeness: RecentPnlCompleteness,
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
    ) -> std::result::Result<RecentFillsAudit, RecentFillsAuditError> {
        let required_window = required_recent_pnl_window(Utc::now());
        let batch = match self
            .account
            .get_recent_track_pnl_records(instrument, &required_window)
            .await
            .map_err(RecentFillsAuditError::Exchange)?
        {
            RecentTrackPnlRecords::Available(records) => records,
            RecentTrackPnlRecords::Unsupported => {
                return Ok(RecentFillsAudit {
                    availability: RecentPnlAvailability::Unsupported,
                    required_window,
                    covered_window: None,
                    completeness: RecentPnlCompleteness::Incomplete,
                    records_seen: 0,
                    records_recorded: 0,
                    records_missing: 0,
                    records_unkeyed: 0,
                    items: Vec::new(),
                });
            }
        };
        let completeness = if batch.covers(&required_window) {
            RecentPnlCompleteness::Complete
        } else {
            RecentPnlCompleteness::Incomplete
        };
        let covered_window = batch.covered_window;
        let records = batch
            .records
            .into_iter()
            .filter(|record| record.kind() == TrackPnlRecordKind::Trade)
            .collect::<Vec<_>>();
        let source_keys = records
            .iter()
            .filter_map(normalized_source_key)
            .collect::<Vec<_>>();
        let local_source_keys = if source_keys.is_empty() {
            BTreeSet::new()
        } else {
            self.query_store
                .list_track_pnl_source_keys(track_id, &source_keys)
                .await
                .map_err(RecentFillsAuditError::Store)?
                .into_iter()
                .collect::<BTreeSet<_>>()
        };

        let mut audit = RecentFillsAudit {
            availability: RecentPnlAvailability::Available,
            required_window,
            covered_window,
            completeness,
            records_seen: records.len(),
            records_recorded: 0,
            records_missing: 0,
            records_unkeyed: 0,
            items: Vec::new(),
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
    use poise_application::exchange::{AccountPort, RecentTrackPnlRecords, UserDataEvent};
    use poise_application::{TrackIdentityStore, TrackMutationStore, TrackQueryStore};
    use poise_core::instrument::{Amount, Asset};
    use poise_core::track::{Instrument, TrackId, Venue};
    use poise_core::types::Side;
    use poise_engine::ledger::TrackPnlRecord;
    use poise_storage::sqlite::SqliteStorage;
    use tokio::sync::mpsc;

    use super::{
        RecentFillCoverage, RecentFillsAuditError, RecentFillsAuditor, RecentPnlAvailability,
    };

    fn amount(value: f64, asset: &str) -> Amount {
        Amount::new(value, Asset::try_new(asset).unwrap()).unwrap()
    }

    #[tokio::test]
    async fn recent_fills_audit_marks_recorded_missing_and_unkeyed_records() {
        let repository = Arc::new(SqliteStorage::in_memory().unwrap());
        let track_id = TrackId::new("btc-core");
        let instrument = Instrument::new(Venue::Okx, "BTC-USD-SWAP");
        TrackIdentityStore::guard_track_identities(
            &*repository,
            &[(track_id.clone(), instrument.clone())],
        )
        .await
        .unwrap();
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
            response: FakeRecentFillsResponse::Available(vec![recorded, missing, unkeyed]),
        });
        let auditor =
            RecentFillsAuditor::new(account, repository.clone() as Arc<dyn TrackQueryStore>);

        let audit = auditor
            .audit_recent_fills(&track_id, &instrument)
            .await
            .unwrap();

        assert_eq!(audit.records_seen, 3);
        assert_eq!(audit.availability, RecentPnlAvailability::Available);
        assert_eq!(audit.records_recorded, 1);
        assert_eq!(audit.records_missing, 1);
        assert_eq!(audit.records_unkeyed, 1);
        assert_eq!(audit.items[0].coverage, RecentFillCoverage::Recorded);
        assert_eq!(audit.items[1].coverage, RecentFillCoverage::Missing);
        assert_eq!(audit.items[2].coverage, RecentFillCoverage::Unkeyed);
    }

    #[tokio::test]
    async fn recent_fills_audit_ignores_funding_records() {
        let repository = Arc::new(SqliteStorage::in_memory().unwrap());
        let track_id = TrackId::new("btc-core");
        let instrument = Instrument::new(Venue::Okx, "BTC-USD-SWAP");
        let account = Arc::new(FakeRecentFillsAccount {
            response: FakeRecentFillsResponse::Available(vec![TrackPnlRecord::funding(
                instrument.clone(),
                Utc.with_ymd_and_hms(2026, 6, 17, 8, 0, 0).unwrap(),
                "okx:bills".to_string(),
                Some("okx:bills:btc-usd-swap:bill-1".to_string()),
                amount(-0.0001, "BTC"),
            )]),
        });
        let auditor =
            RecentFillsAuditor::new(account, repository.clone() as Arc<dyn TrackQueryStore>);

        let audit = auditor
            .audit_recent_fills(&track_id, &instrument)
            .await
            .unwrap();

        assert_eq!(audit.records_seen, 0);
        assert!(audit.items.is_empty());
    }

    #[tokio::test]
    async fn recent_fills_audit_preserves_supported_empty_and_unsupported() {
        let repository = Arc::new(SqliteStorage::in_memory().unwrap());
        let track_id = TrackId::new("btc-core");
        let instrument = Instrument::new(Venue::Binance, "BTCUSDT");

        let available = RecentFillsAuditor::new(
            Arc::new(FakeRecentFillsAccount {
                response: FakeRecentFillsResponse::Available(Vec::new()),
            }),
            repository.clone() as Arc<dyn TrackQueryStore>,
        )
        .audit_recent_fills(&track_id, &instrument)
        .await
        .unwrap();
        let unsupported = RecentFillsAuditor::new(
            Arc::new(FakeRecentFillsAccount {
                response: FakeRecentFillsResponse::Unsupported,
            }),
            repository.clone() as Arc<dyn TrackQueryStore>,
        )
        .audit_recent_fills(&track_id, &instrument)
        .await
        .unwrap();
        let incomplete = RecentFillsAuditor::new(
            Arc::new(FakeRecentFillsAccount {
                response: FakeRecentFillsResponse::Incomplete(Vec::new()),
            }),
            repository.clone() as Arc<dyn TrackQueryStore>,
        )
        .audit_recent_fills(&track_id, &instrument)
        .await
        .unwrap();

        assert_eq!(available.availability, RecentPnlAvailability::Available);
        assert_eq!(available.records_seen, 0);
        assert_eq!(
            available.completeness,
            poise_application::exchange::RecentPnlCompleteness::Complete
        );
        assert!(available.covered_window.is_some());
        assert_eq!(unsupported.availability, RecentPnlAvailability::Unsupported);
        assert_eq!(unsupported.records_seen, 0);
        assert_eq!(
            incomplete.completeness,
            poise_application::exchange::RecentPnlCompleteness::Incomplete
        );
        assert_eq!(incomplete.covered_window, None);
    }

    #[tokio::test]
    async fn recent_fills_audit_keeps_transient_errors_separate_from_unsupported() {
        let repository = Arc::new(SqliteStorage::in_memory().unwrap());
        let error = RecentFillsAuditor::new(
            Arc::new(FakeRecentFillsAccount {
                response: FakeRecentFillsResponse::Error("temporary exchange outage".to_string()),
            }),
            repository as Arc<dyn TrackQueryStore>,
        )
        .audit_recent_fills(
            &TrackId::new("btc-core"),
            &Instrument::new(Venue::Binance, "BTCUSDT"),
        )
        .await
        .unwrap_err();

        assert_eq!(
            error.to_string(),
            "recent exchange PNL query failed: temporary exchange outage"
        );
    }

    #[tokio::test]
    async fn recent_fills_audit_classifies_local_coverage_query_failures_separately() {
        let instrument = Instrument::new(Venue::Okx, "BTC-USDT-SWAP");
        let error = RecentFillsAuditor::new(
            Arc::new(FakeRecentFillsAccount {
                response: FakeRecentFillsResponse::Available(vec![trade_record(
                    instrument.clone(),
                    Some("okx:fills:one"),
                    Some("one"),
                )]),
            }),
            Arc::new(FailingPnlSourceKeyStore),
        )
        .audit_recent_fills(&TrackId::new("btc-core"), &instrument)
        .await
        .unwrap_err();

        assert!(matches!(&error, RecentFillsAuditError::Store(_)));
        assert_eq!(
            error.to_string(),
            "local PNL coverage query failed: local source-key query unavailable"
        );
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
            poise_engine::ledger::TrackPnlQuantity::base_asset(
                poise_core::instrument::PositionQty::new(0.2).unwrap(),
                poise_core::instrument::Asset::try_new("BTC").unwrap(),
            ),
            amount(0.0001, "BTC"),
            Some(amount(0.00001, "BTC")),
        )
    }

    struct FakeRecentFillsAccount {
        response: FakeRecentFillsResponse,
    }

    enum FakeRecentFillsResponse {
        Available(Vec<TrackPnlRecord>),
        Incomplete(Vec<TrackPnlRecord>),
        Unsupported,
        Error(String),
    }

    struct FailingPnlSourceKeyStore;

    #[async_trait::async_trait]
    impl TrackQueryStore for FailingPnlSourceKeyStore {
        async fn list_recent_track_events(
            &self,
            _track_id: &TrackId,
            _limit: usize,
        ) -> Result<Vec<poise_application::StoredTrackEvent>> {
            unreachable!("not used by recent fills audit")
        }

        async fn list_recent_track_effects(
            &self,
            _track_id: &TrackId,
            _limit: usize,
        ) -> Result<Vec<poise_application::PersistedTrackEffect>> {
            unreachable!("not used by recent fills audit")
        }

        async fn load_track_control_state(
            &self,
            _track_id: &TrackId,
        ) -> Result<Option<poise_application::TrackControlState>> {
            unreachable!("not used by recent fills audit")
        }

        async fn load_track_pnl_stats(
            &self,
            _track_id: &TrackId,
            _pnl_utc_day: chrono::NaiveDate,
        ) -> Result<poise_engine::ledger::TrackPnlStats> {
            unreachable!("not used by recent fills audit")
        }

        async fn list_track_pnl_source_keys(
            &self,
            _track_id: &TrackId,
            _source_keys: &[String],
        ) -> Result<Vec<String>> {
            anyhow::bail!("local source-key query unavailable")
        }

        async fn load_track_updated_at(
            &self,
            _track_id: &TrackId,
        ) -> Result<Option<chrono::DateTime<Utc>>> {
            unreachable!("not used by recent fills audit")
        }
    }

    #[async_trait::async_trait]
    impl AccountPort for FakeRecentFillsAccount {
        async fn get_recent_track_pnl_records(
            &self,
            _instrument: &Instrument,
            required_window: &poise_application::exchange::PnlHistoryWindow,
        ) -> Result<RecentTrackPnlRecords> {
            match &self.response {
                FakeRecentFillsResponse::Available(records) => {
                    Ok(RecentTrackPnlRecords::Available(
                        poise_application::exchange::RecentTrackPnlBatch::complete(
                            required_window.clone(),
                            records.clone(),
                        ),
                    ))
                }
                FakeRecentFillsResponse::Incomplete(records) => {
                    Ok(RecentTrackPnlRecords::Available(
                        poise_application::exchange::RecentTrackPnlBatch::incomplete(
                            None,
                            records.clone(),
                        ),
                    ))
                }
                FakeRecentFillsResponse::Unsupported => Ok(RecentTrackPnlRecords::Unsupported),
                FakeRecentFillsResponse::Error(message) => anyhow::bail!(message.clone()),
            }
        }

        async fn subscribe_user_data(&self) -> Result<mpsc::Receiver<UserDataEvent>> {
            let (_sender, receiver) = mpsc::channel(1);
            Ok(receiver)
        }
    }
}
