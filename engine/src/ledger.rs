use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use crate::observation::OrderObservation;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TrackLedgerState {
    pub realized_pnl_day: Option<NaiveDate>,
    pub gross_realized_pnl_today: f64,
    pub gross_realized_pnl_cumulative: f64,
    pub trading_fee_cumulative: f64,
    pub funding_fee_cumulative: f64,
    #[serde(default)]
    pub unresolved_gaps: Vec<LedgerGapRecord>,
}

impl TrackLedgerState {
    pub fn from_legacy_realized(
        realized_pnl_day: Option<NaiveDate>,
        gross_realized_pnl_today: f64,
        gross_realized_pnl_cumulative: f64,
    ) -> Self {
        Self {
            realized_pnl_day,
            gross_realized_pnl_today,
            gross_realized_pnl_cumulative,
            ..Self::default()
        }
    }

    pub fn is_empty(&self) -> bool {
        self.realized_pnl_day.is_none()
            && self.gross_realized_pnl_today.abs() <= f64::EPSILON
            && self.gross_realized_pnl_cumulative.abs() <= f64::EPSILON
            && self.trading_fee_cumulative.abs() <= f64::EPSILON
            && self.funding_fee_cumulative.abs() <= f64::EPSILON
            && self.unresolved_gaps.is_empty()
    }

    pub fn apply_delta(&mut self, trading_day: NaiveDate, delta: &LedgerDelta) {
        match delta {
            LedgerDelta::GrossRealizedPnl(amount) => self.apply_gross_realized_pnl(trading_day, *amount),
            LedgerDelta::TradingFee(amount) => {
                self.trading_fee_cumulative += amount;
            }
            LedgerDelta::FundingFee(amount) => {
                self.funding_fee_cumulative += amount;
            }
        }
    }

    pub fn apply_gross_realized_pnl(&mut self, trading_day: NaiveDate, amount: f64) {
        if self.realized_pnl_day != Some(trading_day) {
            self.realized_pnl_day = Some(trading_day);
            self.gross_realized_pnl_today = 0.0;
        }
        if amount.abs() > f64::EPSILON {
            self.gross_realized_pnl_today += amount;
            self.gross_realized_pnl_cumulative += amount;
        }
    }

    pub fn record_gap(&mut self, gap: LedgerGapRecord) {
        self.unresolved_gaps.push(gap);
    }

    pub fn net_realized_pnl(&self) -> f64 {
        self.gross_realized_pnl_cumulative - self.trading_fee_cumulative + self.funding_fee_cumulative
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum LedgerDelta {
    GrossRealizedPnl(f64),
    TradingFee(f64),
    FundingFee(f64),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecutionLedgerUpdate {
    pub order_update: OrderObservation,
    #[serde(default)]
    pub ledger_deltas: Vec<LedgerDelta>,
    #[serde(default)]
    pub ledger_gaps: Vec<LedgerGapRecord>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LedgerAdjustmentEvent {
    #[serde(default)]
    pub ledger_deltas: Vec<LedgerDelta>,
    #[serde(default)]
    pub ledger_gaps: Vec<LedgerGapRecord>,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackLedgerEvent {
    Execution(ExecutionLedgerUpdate),
    Adjustment(LedgerAdjustmentEvent),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LedgerGapRecord {
    pub gap_key: String,
    pub reason: String,
    pub observed_at: DateTime<Utc>,
    pub source: String,
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    #[test]
    fn apply_gross_realized_pnl_rolls_daily_window() {
        let mut ledger = TrackLedgerState::from_legacy_realized(
            Some(NaiveDate::from_ymd_opt(2026, 3, 24).unwrap()),
            12.5,
            17.5,
        );

        ledger.apply_gross_realized_pnl(NaiveDate::from_ymd_opt(2026, 3, 25).unwrap(), -5.0);

        assert_eq!(ledger.realized_pnl_day, NaiveDate::from_ymd_opt(2026, 3, 25));
        assert!((ledger.gross_realized_pnl_today + 5.0).abs() < f64::EPSILON);
        assert!((ledger.gross_realized_pnl_cumulative - 12.5).abs() < f64::EPSILON);
    }

    #[test]
    fn record_gap_preserves_existing_entries() {
        let mut ledger = TrackLedgerState::default();
        ledger.record_gap(LedgerGapRecord {
            gap_key: "gap-1".into(),
            reason: "unsupported_commission_asset".into(),
            observed_at: Utc.with_ymd_and_hms(2026, 3, 29, 8, 0, 0).unwrap(),
            source: "binance:order_trade_update".into(),
        });
        ledger.record_gap(LedgerGapRecord {
            gap_key: "gap-2".into(),
            reason: "missing_symbol".into(),
            observed_at: Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap(),
            source: "binance:account_update".into(),
        });

        assert_eq!(ledger.unresolved_gaps.len(), 2);
    }
}
