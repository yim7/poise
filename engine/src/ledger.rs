use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use poise_core::instrument::{Amount, Asset, PositionQty};
use poise_core::track::Instrument;
use poise_core::types::Side;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackPnlRecordKind {
    Trade,
    Funding,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackPnlCashflowKind {
    RealizedPnl,
    TradingFee,
    Funding,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TrackPnlCashflow {
    Trade {
        realized_pnl: Amount,
        trading_fee: Option<Amount>,
    },
    Funding {
        funding: Amount,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TrackPnlQuantityUnit {
    BaseAsset { asset: Asset },
    Contracts { contract_value: Amount },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackPnlQuantity {
    qty: PositionQty,
    unit: TrackPnlQuantityUnit,
}

impl TrackPnlQuantity {
    pub fn base_asset(qty: PositionQty, asset: Asset) -> Self {
        Self {
            qty,
            unit: TrackPnlQuantityUnit::BaseAsset { asset },
        }
    }

    pub fn contracts(qty: PositionQty, contract_value: Amount) -> Result<Self, String> {
        if contract_value.value() <= f64::EPSILON {
            return Err(format!(
                "contract value must be positive, got {}",
                contract_value.value()
            ));
        }
        Ok(Self {
            qty,
            unit: TrackPnlQuantityUnit::Contracts { contract_value },
        })
    }

    pub fn qty(&self) -> PositionQty {
        self.qty
    }

    pub fn unit(&self) -> &TrackPnlQuantityUnit {
        &self.unit
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackPnlRecord {
    pub instrument: Instrument,
    pub occurred_at: DateTime<Utc>,
    pub source: String,
    pub source_key: Option<String>,
    pub order_id: Option<String>,
    pub trade_id: Option<String>,
    pub side: Option<Side>,
    pub price: Option<f64>,
    pub quantity: Option<TrackPnlQuantity>,
    pub cashflow: TrackPnlCashflow,
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
        quantity: TrackPnlQuantity,
        realized_pnl: Amount,
        trading_fee: Option<Amount>,
    ) -> Self {
        Self {
            instrument,
            occurred_at,
            source,
            source_key,
            order_id,
            trade_id,
            side: Some(side),
            price: Some(price),
            quantity: Some(quantity),
            cashflow: TrackPnlCashflow::Trade {
                realized_pnl,
                trading_fee,
            },
        }
    }

    pub fn funding(
        instrument: Instrument,
        occurred_at: DateTime<Utc>,
        source: String,
        source_key: Option<String>,
        funding: Amount,
    ) -> Self {
        Self {
            instrument,
            occurred_at,
            source,
            source_key,
            order_id: None,
            trade_id: None,
            side: None,
            price: None,
            quantity: None,
            cashflow: TrackPnlCashflow::Funding { funding },
        }
    }

    pub fn trade_summary(
        instrument: Instrument,
        occurred_at: DateTime<Utc>,
        source: String,
        source_key: Option<String>,
        trade_id: Option<String>,
        realized_pnl: Amount,
        trading_fee: Option<Amount>,
    ) -> Self {
        Self {
            instrument,
            occurred_at,
            source,
            source_key,
            order_id: None,
            trade_id,
            side: None,
            price: None,
            quantity: None,
            cashflow: TrackPnlCashflow::Trade {
                realized_pnl,
                trading_fee,
            },
        }
    }

    pub fn kind(&self) -> TrackPnlRecordKind {
        match self.cashflow {
            TrackPnlCashflow::Trade { .. } => TrackPnlRecordKind::Trade,
            TrackPnlCashflow::Funding { .. } => TrackPnlRecordKind::Funding,
        }
    }

    pub fn realized_pnl(&self) -> Option<&Amount> {
        match &self.cashflow {
            TrackPnlCashflow::Trade { realized_pnl, .. } => Some(realized_pnl),
            TrackPnlCashflow::Funding { .. } => None,
        }
    }

    pub fn trading_fee(&self) -> Option<&Amount> {
        match &self.cashflow {
            TrackPnlCashflow::Trade { trading_fee, .. } => trading_fee.as_ref(),
            TrackPnlCashflow::Funding { .. } => None,
        }
    }

    pub fn funding_amount(&self) -> Option<&Amount> {
        match &self.cashflow {
            TrackPnlCashflow::Funding { funding } => Some(funding),
            TrackPnlCashflow::Trade { .. } => None,
        }
    }

    pub fn primary_asset(&self) -> &Asset {
        match &self.cashflow {
            TrackPnlCashflow::Trade { realized_pnl, .. } => realized_pnl.asset(),
            TrackPnlCashflow::Funding { funding } => funding.asset(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UnvaluedCashflow {
    pub kind: TrackPnlCashflowKind,
    pub amount: Option<Amount>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum PnlAggregateCompleteness {
    #[default]
    Complete,
    Incomplete {
        unvalued_cashflows: Vec<UnvaluedCashflow>,
    },
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct PnlAggregate {
    pub asset: Option<Asset>,
    pub valued_amount: f64,
    pub completeness: PnlAggregateCompleteness,
}

impl PnlAggregate {
    pub fn complete_value(&self) -> Option<f64> {
        matches!(self.completeness, PnlAggregateCompleteness::Complete)
            .then_some(self.valued_amount)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackPnlView {
    pub asset: Asset,
    pub gross_realized_pnl_today: f64,
    pub gross_realized_pnl_cumulative: f64,
    pub trading_fee_today: f64,
    pub trading_fee_cumulative: f64,
    pub funding_fee_today: f64,
    pub funding_fee_cumulative: f64,
    pub net_realized_pnl_cumulative: PnlAggregate,
    pub unrealized_pnl: f64,
    pub total_pnl: PnlAggregate,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackPnlStats {
    pub pnl_utc_day: NaiveDate,
    pub pnl_asset: Option<Asset>,
    pub gross_realized_pnl_today: f64,
    pub gross_realized_pnl_cumulative: f64,
    pub trading_fee_today: f64,
    pub trading_fee_cumulative: f64,
    pub funding_fee_today: f64,
    pub funding_fee_cumulative: f64,
    pub unvalued_cashflows_today: Vec<UnvaluedCashflow>,
    pub unvalued_cashflows_cumulative: Vec<UnvaluedCashflow>,
}

impl Default for TrackPnlStats {
    fn default() -> Self {
        Self {
            pnl_utc_day: NaiveDate::from_ymd_opt(1970, 1, 1).expect("valid epoch date"),
            pnl_asset: None,
            gross_realized_pnl_today: 0.0,
            gross_realized_pnl_cumulative: 0.0,
            trading_fee_today: 0.0,
            trading_fee_cumulative: 0.0,
            funding_fee_today: 0.0,
            funding_fee_cumulative: 0.0,
            unvalued_cashflows_today: Vec::new(),
            unvalued_cashflows_cumulative: Vec::new(),
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
            self.unvalued_cashflows_today.clear();
        }
    }

    pub fn net_realized_pnl_today(&self) -> PnlAggregate {
        self.aggregate(
            self.gross_realized_pnl_today - self.trading_fee_today + self.funding_fee_today,
            &self.unvalued_cashflows_today,
        )
    }

    pub fn net_realized_pnl_cumulative(&self) -> PnlAggregate {
        self.aggregate(
            self.gross_realized_pnl_cumulative - self.trading_fee_cumulative
                + self.funding_fee_cumulative,
            &self.unvalued_cashflows_cumulative,
        )
    }

    pub fn net_realized_pnl(&self) -> PnlAggregate {
        self.net_realized_pnl_cumulative()
    }

    pub fn view(&self, default_asset: &Asset, unrealized_pnl: f64) -> TrackPnlView {
        let asset = self
            .pnl_asset
            .clone()
            .unwrap_or_else(|| default_asset.clone());
        let mut net_realized_pnl_cumulative = self.net_realized_pnl_cumulative();
        if net_realized_pnl_cumulative.asset.is_none() {
            net_realized_pnl_cumulative.asset = Some(asset.clone());
        }
        let total_pnl = PnlAggregate {
            asset: Some(asset.clone()),
            valued_amount: net_realized_pnl_cumulative.valued_amount + unrealized_pnl,
            completeness: net_realized_pnl_cumulative.completeness.clone(),
        };

        TrackPnlView {
            asset,
            gross_realized_pnl_today: self.gross_realized_pnl_today,
            gross_realized_pnl_cumulative: self.gross_realized_pnl_cumulative,
            trading_fee_today: self.trading_fee_today,
            trading_fee_cumulative: self.trading_fee_cumulative,
            funding_fee_today: self.funding_fee_today,
            funding_fee_cumulative: self.funding_fee_cumulative,
            net_realized_pnl_cumulative,
            unrealized_pnl,
            total_pnl,
        }
    }

    pub fn apply_record(&mut self, record: &TrackPnlRecord) {
        self.apply_cashflow_fact(record.occurred_at, &record.cashflow)
    }

    pub fn apply_cashflow_fact(&mut self, occurred_at: DateTime<Utc>, cashflow: &TrackPnlCashflow) {
        let primary_asset = match cashflow {
            TrackPnlCashflow::Trade { realized_pnl, .. } => realized_pnl.asset(),
            TrackPnlCashflow::Funding { funding } => funding.asset(),
        };
        let target_asset = self
            .pnl_asset
            .get_or_insert_with(|| primary_asset.clone())
            .clone();
        let is_today = occurred_at.date_naive() == self.pnl_utc_day;
        match cashflow {
            TrackPnlCashflow::Trade {
                realized_pnl,
                trading_fee,
            } => {
                self.apply_amount(
                    &target_asset,
                    TrackPnlCashflowKind::RealizedPnl,
                    Some(realized_pnl),
                    is_today,
                );
                self.apply_amount(
                    &target_asset,
                    TrackPnlCashflowKind::TradingFee,
                    trading_fee.as_ref(),
                    is_today,
                );
            }
            TrackPnlCashflow::Funding { funding } => self.apply_amount(
                &target_asset,
                TrackPnlCashflowKind::Funding,
                Some(funding),
                is_today,
            ),
        }
    }

    fn apply_amount(
        &mut self,
        target_asset: &Asset,
        kind: TrackPnlCashflowKind,
        amount: Option<&Amount>,
        is_today: bool,
    ) {
        let Some(amount) = amount else {
            let unvalued = UnvaluedCashflow { kind, amount: None };
            if is_today {
                self.unvalued_cashflows_today.push(unvalued.clone());
            }
            self.unvalued_cashflows_cumulative.push(unvalued);
            return;
        };
        if amount.asset() != target_asset {
            let unvalued = UnvaluedCashflow {
                kind,
                amount: Some(amount.clone()),
            };
            if is_today {
                self.unvalued_cashflows_today.push(unvalued.clone());
            }
            self.unvalued_cashflows_cumulative.push(unvalued);
            return;
        }

        match kind {
            TrackPnlCashflowKind::RealizedPnl => {
                if is_today {
                    self.gross_realized_pnl_today += amount.value();
                }
                self.gross_realized_pnl_cumulative += amount.value();
            }
            TrackPnlCashflowKind::TradingFee => {
                if is_today {
                    self.trading_fee_today += amount.value();
                }
                self.trading_fee_cumulative += amount.value();
            }
            TrackPnlCashflowKind::Funding => {
                if is_today {
                    self.funding_fee_today += amount.value();
                }
                self.funding_fee_cumulative += amount.value();
            }
        }
    }

    fn aggregate(&self, valued_amount: f64, unvalued: &[UnvaluedCashflow]) -> PnlAggregate {
        PnlAggregate {
            asset: self.pnl_asset.clone(),
            valued_amount,
            completeness: if unvalued.is_empty() {
                PnlAggregateCompleteness::Complete
            } else {
                PnlAggregateCompleteness::Incomplete {
                    unvalued_cashflows: unvalued.to_vec(),
                }
            },
        }
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
            && self.unvalued_cashflows_today.is_empty()
            && self.unvalued_cashflows_cumulative.is_empty()
            && self.pnl_asset.is_none()
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn amount(value: f64, asset: &str) -> Amount {
        Amount::new(value, Asset::try_new(asset).unwrap()).unwrap()
    }

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
            amount(-5.0, "USDT"),
            Some(amount(0.0, "USDT")),
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
            amount(120.0, "USDT"),
            Some(amount(5.0, "USDT")),
        ));
        stats.apply_record(&TrackPnlRecord::funding(
            Instrument::new(poise_core::track::Venue::Binance, "BTCUSDT"),
            Utc.with_ymd_and_hms(2026, 4, 8, 9, 0, 0).unwrap(),
            "test".into(),
            None,
            amount(-2.0, "USDT"),
        ));

        assert_eq!(stats.net_realized_pnl_today().complete_value(), Some(113.0));
        assert_eq!(stats.pnl_asset.as_ref().map(Asset::as_str), Some("USDT"));
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
            amount(120.0, "USDT"),
            Some(amount(5.0, "USDT")),
        ));
        stats.ensure_utc_day(NaiveDate::from_ymd_opt(2026, 4, 9).unwrap());
        stats.apply_record(&TrackPnlRecord::trade_summary(
            Instrument::new(poise_core::track::Venue::Binance, "BTCUSDT"),
            Utc.with_ymd_and_hms(2026, 4, 9, 8, 0, 0).unwrap(),
            "test".into(),
            None,
            None,
            amount(10.0, "USDT"),
            Some(amount(0.0, "USDT")),
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
            amount(0.002, "BTC"),
            Some(amount(0.00001, "BTC")),
        );
        let mut stats = TrackPnlStats {
            pnl_utc_day: NaiveDate::from_ymd_opt(2026, 4, 8).unwrap(),
            ..TrackPnlStats::default()
        };

        stats.apply_record(&record);

        assert_eq!(stats.gross_realized_pnl_cumulative, 0.002);
        assert_eq!(stats.trading_fee_cumulative, 0.00001);
        assert_eq!(stats.pnl_asset.as_ref().map(Asset::as_str), Some("BTC"));
    }

    #[test]
    fn apply_record_keeps_unvalued_cashflow_and_marks_aggregate_incomplete() {
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
            amount(120.0, "USDT"),
            Some(amount(0.001, "BNB")),
        ));

        let aggregate = stats.net_realized_pnl_today();

        assert_eq!(aggregate.complete_value(), None);
        assert_eq!(aggregate.valued_amount, 120.0);
        assert_eq!(
            aggregate.completeness,
            PnlAggregateCompleteness::Incomplete {
                unvalued_cashflows: vec![UnvaluedCashflow {
                    kind: TrackPnlCashflowKind::TradingFee,
                    amount: Some(amount(0.001, "BNB")),
                }],
            }
        );
    }

    #[test]
    fn pnl_view_owns_total_and_default_asset_semantics() {
        let stats = TrackPnlStats {
            gross_realized_pnl_cumulative: 120.0,
            trading_fee_cumulative: 5.0,
            ..TrackPnlStats::default()
        };
        let view = stats.view(&Asset::try_new("USDT").unwrap(), -15.0);

        assert_eq!(view.asset.as_str(), "USDT");
        assert_eq!(
            view.net_realized_pnl_cumulative.complete_value(),
            Some(115.0)
        );
        assert_eq!(view.total_pnl.complete_value(), Some(100.0));
    }

    #[test]
    fn apply_record_treats_missing_trading_fee_as_incomplete() {
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
            amount(120.0, "USDT"),
            None,
        ));

        assert_eq!(stats.net_realized_pnl_today().complete_value(), None);
        assert_eq!(
            stats.unvalued_cashflows_today,
            vec![UnvaluedCashflow {
                kind: TrackPnlCashflowKind::TradingFee,
                amount: None,
            }]
        );
    }

    #[test]
    fn trade_record_owns_occurrence_time_quantity_semantics() {
        let quantity =
            TrackPnlQuantity::contracts(PositionQty::new(30.0).unwrap(), amount(100.0, "USD"))
                .unwrap();
        let record = TrackPnlRecord::trade(
            Instrument::new(poise_core::track::Venue::Okx, "BTC-USD-SWAP"),
            Utc.with_ymd_and_hms(2026, 4, 8, 8, 0, 0).unwrap(),
            "test".into(),
            Some("trade:1".into()),
            Some("order:1".into()),
            Some("1".into()),
            Side::Sell,
            60_000.0,
            quantity.clone(),
            amount(0.001, "BTC"),
            Some(amount(0.00001, "BTC")),
        );

        assert_eq!(record.quantity, Some(quantity));
        assert!(matches!(
            record.quantity.as_ref().unwrap().unit(),
            TrackPnlQuantityUnit::Contracts { contract_value }
                if contract_value.value() == 100.0 && contract_value.asset().as_str() == "USD"
        ));
    }
}
