use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use poise_core::track::Instrument;
use poise_core::types::Side;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackPnlRecordKind {
    Trade,
    Funding,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackPnlRecord {
    pub instrument: Instrument,
    pub occurred_at: DateTime<Utc>,
    pub kind: TrackPnlRecordKind,
    pub source: String,
    pub source_key: Option<String>,
    pub order_id: Option<String>,
    pub trade_id: Option<String>,
    pub side: Option<Side>,
    pub price: Option<f64>,
    pub qty: Option<f64>,
    pub realized_pnl: f64,
    pub trading_fee: f64,
    pub funding_fee: f64,
}

impl TrackPnlRecord {
    #[allow(clippy::too_many_arguments)]
    pub fn trade(
        instrument: Instrument,
        occurred_at: DateTime<Utc>,
        source: String,
        source_key: Option<String>,
        order_id: Option<String>,
        trade_id: Option<String>,
        side: Side,
        price: f64,
        qty: f64,
        realized_pnl: f64,
        trading_fee: f64,
    ) -> Self {
        Self {
            instrument,
            occurred_at,
            kind: TrackPnlRecordKind::Trade,
            source,
            source_key,
            order_id,
            trade_id,
            side: Some(side),
            price: Some(price),
            qty: Some(qty),
            realized_pnl,
            trading_fee,
            funding_fee: 0.0,
        }
    }

    pub fn funding(
        instrument: Instrument,
        occurred_at: DateTime<Utc>,
        source: String,
        source_key: Option<String>,
        funding_fee: f64,
    ) -> Self {
        Self {
            instrument,
            occurred_at,
            kind: TrackPnlRecordKind::Funding,
            source,
            source_key,
            order_id: None,
            trade_id: None,
            side: None,
            price: None,
            qty: None,
            realized_pnl: 0.0,
            trading_fee: 0.0,
            funding_fee,
        }
    }

    pub fn trade_summary(
        instrument: Instrument,
        occurred_at: DateTime<Utc>,
        source: String,
        source_key: Option<String>,
        trade_id: Option<String>,
        realized_pnl: f64,
        trading_fee: f64,
    ) -> Self {
        Self {
            instrument,
            occurred_at,
            kind: TrackPnlRecordKind::Trade,
            source,
            source_key,
            order_id: None,
            trade_id,
            side: None,
            price: None,
            qty: None,
            realized_pnl,
            trading_fee,
            funding_fee: 0.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackPnlStats {
    pub pnl_utc_day: NaiveDate,
    pub gross_realized_pnl_today: f64,
    pub gross_realized_pnl_cumulative: f64,
    pub trading_fee_today: f64,
    pub trading_fee_cumulative: f64,
    pub funding_fee_today: f64,
    pub funding_fee_cumulative: f64,
}

impl Default for TrackPnlStats {
    fn default() -> Self {
        Self {
            pnl_utc_day: NaiveDate::from_ymd_opt(1970, 1, 1).expect("valid epoch date"),
            gross_realized_pnl_today: 0.0,
            gross_realized_pnl_cumulative: 0.0,
            trading_fee_today: 0.0,
            trading_fee_cumulative: 0.0,
            funding_fee_today: 0.0,
            funding_fee_cumulative: 0.0,
        }
    }
}

impl TrackPnlStats {
    pub fn normalize_utc_day(&mut self, utc_day: NaiveDate) {
        self.ensure_utc_day(utc_day);
    }

    pub fn ensure_utc_day(&mut self, utc_day: NaiveDate) {
        if self.pnl_utc_day != utc_day {
            self.pnl_utc_day = utc_day;
            self.gross_realized_pnl_today = 0.0;
            self.trading_fee_today = 0.0;
            self.funding_fee_today = 0.0;
        }
    }

    pub fn net_realized_pnl_today(&self) -> f64 {
        self.gross_realized_pnl_today - self.trading_fee_today + self.funding_fee_today
    }

    pub fn net_realized_pnl_cumulative(&self) -> f64 {
        self.gross_realized_pnl_cumulative - self.trading_fee_cumulative
            + self.funding_fee_cumulative
    }

    pub fn net_realized_pnl(&self) -> f64 {
        self.net_realized_pnl_cumulative()
    }

    pub fn apply_record(&mut self, record: &TrackPnlRecord) {
        if record.occurred_at.date_naive() == self.pnl_utc_day {
            self.gross_realized_pnl_today += record.realized_pnl;
            self.trading_fee_today += record.trading_fee;
            self.funding_fee_today += record.funding_fee;
        }
        self.gross_realized_pnl_cumulative += record.realized_pnl;
        self.trading_fee_cumulative += record.trading_fee;
        self.funding_fee_cumulative += record.funding_fee;
    }
}

impl TrackPnlStats {
    pub fn is_empty(&self) -> bool {
        self.gross_realized_pnl_today.abs() <= f64::EPSILON
            && self.gross_realized_pnl_cumulative.abs() <= f64::EPSILON
            && self.trading_fee_today.abs() <= f64::EPSILON
            && self.trading_fee_cumulative.abs() <= f64::EPSILON
            && self.funding_fee_today.abs() <= f64::EPSILON
            && self.funding_fee_cumulative.abs() <= f64::EPSILON
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    #[test]
    fn apply_gross_realized_pnl_rolls_utc_daily_window() {
        let mut stats = TrackPnlStats {
            pnl_utc_day: NaiveDate::from_ymd_opt(2026, 3, 24).unwrap(),
            gross_realized_pnl_today: 12.5,
            gross_realized_pnl_cumulative: 17.5,
            ..TrackPnlStats::default()
        };

        stats.ensure_utc_day(NaiveDate::from_ymd_opt(2026, 3, 25).unwrap());
        stats.apply_record(&TrackPnlRecord::trade_summary(
            Instrument::new(poise_core::track::Venue::Binance, "BTCUSDT"),
            Utc.with_ymd_and_hms(2026, 3, 25, 8, 0, 0).unwrap(),
            "test".into(),
            None,
            None,
            -5.0,
            0.0,
        ));

        assert_eq!(
            stats.pnl_utc_day,
            NaiveDate::from_ymd_opt(2026, 3, 25).unwrap()
        );
        assert!((stats.gross_realized_pnl_today + 5.0).abs() < f64::EPSILON);
        assert!((stats.gross_realized_pnl_cumulative - 12.5).abs() < f64::EPSILON);
    }

    #[test]
    fn normalize_utc_day_is_the_single_rollover_owner() {
        let mut stats = TrackPnlStats {
            pnl_utc_day: NaiveDate::from_ymd_opt(2026, 4, 8).unwrap(),
            gross_realized_pnl_today: 120.0,
            gross_realized_pnl_cumulative: 500.0,
            trading_fee_today: 5.0,
            trading_fee_cumulative: 30.0,
            funding_fee_today: -2.0,
            funding_fee_cumulative: -11.0,
            ..TrackPnlStats::default()
        };

        stats.normalize_utc_day(NaiveDate::from_ymd_opt(2026, 4, 9).unwrap());

        assert_eq!(
            stats.pnl_utc_day,
            NaiveDate::from_ymd_opt(2026, 4, 9).unwrap()
        );
        assert_eq!(stats.gross_realized_pnl_today, 0.0);
        assert_eq!(stats.trading_fee_today, 0.0);
        assert_eq!(stats.funding_fee_today, 0.0);
        assert_eq!(stats.gross_realized_pnl_cumulative, 500.0);
        assert_eq!(stats.trading_fee_cumulative, 30.0);
        assert_eq!(stats.funding_fee_cumulative, -11.0);
    }

    #[test]
    fn net_realized_pnl_today_includes_today_fees_and_funding() {
        let mut stats = TrackPnlStats {
            pnl_utc_day: NaiveDate::from_ymd_opt(2026, 4, 8).unwrap(),
            ..TrackPnlStats::default()
        };
        stats.apply_record(&TrackPnlRecord::trade_summary(
            Instrument::new(poise_core::track::Venue::Binance, "BTCUSDT"),
            Utc.with_ymd_and_hms(2026, 4, 8, 8, 0, 0).unwrap(),
            "test".into(),
            None,
            None,
            120.0,
            5.0,
        ));
        stats.apply_record(&TrackPnlRecord::funding(
            Instrument::new(poise_core::track::Venue::Binance, "BTCUSDT"),
            Utc.with_ymd_and_hms(2026, 4, 8, 9, 0, 0).unwrap(),
            "test".into(),
            None,
            -2.0,
        ));

        assert!((stats.net_realized_pnl_today() - 113.0).abs() < f64::EPSILON);
    }

    #[test]
    fn utc_day_rollover_resets_today_fee_fields_but_keeps_cumulative_values() {
        let mut stats = TrackPnlStats {
            pnl_utc_day: NaiveDate::from_ymd_opt(2026, 4, 8).unwrap(),
            ..TrackPnlStats::default()
        };
        stats.apply_record(&TrackPnlRecord::trade_summary(
            Instrument::new(poise_core::track::Venue::Binance, "BTCUSDT"),
            Utc.with_ymd_and_hms(2026, 4, 8, 8, 0, 0).unwrap(),
            "test".into(),
            None,
            None,
            120.0,
            5.0,
        ));
        stats.ensure_utc_day(NaiveDate::from_ymd_opt(2026, 4, 9).unwrap());
        stats.apply_record(&TrackPnlRecord::trade_summary(
            Instrument::new(poise_core::track::Venue::Binance, "BTCUSDT"),
            Utc.with_ymd_and_hms(2026, 4, 9, 8, 0, 0).unwrap(),
            "test".into(),
            None,
            None,
            10.0,
            0.0,
        ));

        assert!((stats.trading_fee_today - 0.0).abs() < f64::EPSILON);
        assert!((stats.trading_fee_cumulative - 5.0).abs() < f64::EPSILON);
    }

    #[test]
    fn apply_record_only_updates_amounts() {
        let record = TrackPnlRecord::trade_summary(
            Instrument::new(poise_core::track::Venue::Binance, "ETHBTC"),
            Utc.with_ymd_and_hms(2026, 4, 8, 8, 0, 0).unwrap(),
            "test".into(),
            None,
            None,
            0.002,
            0.00001,
        );
        let mut stats = TrackPnlStats {
            pnl_utc_day: NaiveDate::from_ymd_opt(2026, 4, 8).unwrap(),
            ..TrackPnlStats::default()
        };

        stats.apply_record(&record);

        assert_eq!(stats.gross_realized_pnl_cumulative, 0.002);
        assert_eq!(stats.trading_fee_cumulative, 0.00001);
    }
}
