use anyhow::{Context, Result, anyhow, ensure};
use poise_core::instrument::{InstrumentSpec, PositionBasis};
use poise_core::track::{Instrument, TrackDefinition};
use poise_engine::ports::{AccountSummarySnapshot, ExchangePorts, MarketDataPort};
use poise_protocol::{
    ConfigDryRunAccountView, ConfigDryRunCapacityView, ConfigDryRunResponse, ConfigDryRunTrackView,
    InstrumentView, TrackPositionQuantityUnitView,
};

use crate::config::Config;
use crate::config_explain::{TrackConfigExplanation, explain_track_config};
use crate::exchange_startup::build_track_leverage_index;

pub(crate) async fn run_config_dry_run_with_ports(
    config: &Config,
    exchange_ports: ExchangePorts,
) -> Result<ConfigDryRunResponse> {
    let track_leverage_index = build_track_leverage_index(&config.tracks)?;
    let track_definitions = config.track_definitions()?;
    let account = exchange_ports
        .account_summary()
        .get_account_summary()
        .await
        .context("failed to load dry-run account summary")?;
    let metadata = exchange_ports.metadata();
    let market_data = exchange_ports.market_data();
    let mut tracks = Vec::new();

    for track in &track_definitions {
        let info = metadata
            .get_exchange_info(track.instrument())
            .await
            .with_context(|| {
                format!(
                    "failed to load dry-run symbol metadata for track `{}` symbol `{}`",
                    track.track_id().as_str(),
                    track.instrument().symbol
                )
            })?;
        let explanation = explain_track_config(track, &info.instrument_spec)?;
        let leverage = track_leverage_index
            .get(track.track_id())
            .copied()
            .ok_or_else(|| {
                anyhow!(
                    "missing startup leverage for track `{}`",
                    explanation.track_id
                )
            })?;
        let capacity = estimate_capacity(
            track,
            &info.instrument_spec,
            &account,
            leverage,
            market_data.as_ref(),
        )
        .await?;
        tracks.push(project_track_explanation(
            explanation,
            track.instrument(),
            leverage,
            capacity,
        ));
    }

    Ok(ConfigDryRunResponse {
        account: project_account(account),
        tracks,
        warnings: Vec::new(),
    })
}

fn project_account(account: AccountSummarySnapshot) -> ConfigDryRunAccountView {
    ConfigDryRunAccountView {
        valuation_asset: account.valuation_asset().as_str().to_string(),
        equity: account.equity().value(),
        available: account.available().map(|amount| amount.value()),
        available_by_asset: account
            .available_by_asset()
            .iter()
            .map(|(asset, value)| (asset.as_str().to_string(), *value))
            .collect(),
        unrealized_pnl: account.unrealized_pnl().map(|amount| amount.value()),
        observed_at: account.observed_at().to_rfc3339(),
    }
}

fn project_track_explanation(
    explanation: TrackConfigExplanation,
    instrument: &Instrument,
    leverage: u32,
    capacity: Option<ConfigDryRunCapacityView>,
) -> ConfigDryRunTrackView {
    ConfigDryRunTrackView {
        track_id: explanation.track_id,
        instrument: InstrumentView {
            venue: instrument.venue.as_str().to_string(),
            symbol: explanation.symbol,
        },
        leverage,
        position_qty_unit: quantity_unit(&explanation.position_basis),
        position_qty_per_unit: explanation.position_qty_per_unit,
        unit_notional: explanation.unit_notional,
        unit_notional_asset: explanation.unit_notional_asset,
        quantity_step: explanation.quantity_step,
        min_quantity: explanation.min_quantity,
        min_notional: explanation.min_notional,
        effective_max_notional: explanation.effective_max_notional,
        loss_limit_asset: explanation.loss_limit_asset,
        daily_loss_limit: explanation.daily_loss_limit,
        total_loss_limit: explanation.total_loss_limit,
        capacity,
    }
}

fn quantity_unit(position_basis: &PositionBasis) -> TrackPositionQuantityUnitView {
    match position_basis {
        PositionBasis::BaseAsset => TrackPositionQuantityUnitView::BaseAsset,
        PositionBasis::Contracts { .. } => TrackPositionQuantityUnitView::Contracts,
    }
}

async fn estimate_capacity(
    track: &TrackDefinition,
    instrument_spec: &InstrumentSpec,
    account: &AccountSummarySnapshot,
    leverage: u32,
    market_data: &dyn MarketDataPort,
) -> Result<Option<ConfigDryRunCapacityView>> {
    let mark_price = market_data
        .get_mark_price(track.instrument())
        .await
        .with_context(|| {
            format!(
                "failed to load dry-run mark price for track `{}` symbol `{}`",
                track.track_id().as_str(),
                track.instrument().symbol
            )
        })?;
    let Some(mark_price) = mark_price else {
        if matches!(
            instrument_spec.position_basis(),
            PositionBasis::Contracts { .. }
        ) {
            return Err(anyhow!(
                "missing mark price for inverse dry-run capacity on `{}`",
                track.instrument().symbol
            ));
        }
        return Ok(None);
    };
    ensure!(
        mark_price.is_finite() && mark_price > 0.0,
        "invalid mark price for dry-run capacity on `{}`: got {}",
        track.instrument().symbol,
        mark_price
    );

    let available_asset = instrument_spec.settlement_asset().as_str().to_string();
    let available = match instrument_spec.position_basis() {
        PositionBasis::Contracts { .. } => account
            .available_for_asset(&available_asset)
            .with_context(|| {
                format!(
                    "missing available balance for settlement asset `{}`",
                    available_asset
                )
            })?,
        PositionBasis::BaseAsset => {
            match account.available_for_asset(&available_asset).or_else(|| {
                (account.valuation_asset() == instrument_spec.notional_asset())
                    .then(|| account.available().map(|amount| amount.value()))
                    .flatten()
            }) {
                Some(available) => available,
                None => return Ok(None),
            }
        }
    };
    ensure!(
        available.is_finite(),
        "invalid available balance for `{}`: got {}",
        available_asset,
        available
    );
    let available = available.max(0.0);

    let (estimated_max_position_qty, estimated_max_notional, estimated_max_notional_asset) =
        match instrument_spec.position_basis() {
            PositionBasis::BaseAsset => {
                let estimated_max_notional = available * leverage as f64;
                (
                    estimated_max_notional / mark_price,
                    estimated_max_notional,
                    instrument_spec.notional_asset().as_str().to_string(),
                )
            }
            PositionBasis::Contracts { contract_value } => {
                let contract_notional = contract_value.value();
                let estimated_contracts =
                    available * mark_price * leverage as f64 / contract_notional;
                (
                    estimated_contracts,
                    estimated_contracts * contract_notional,
                    instrument_spec.notional_asset().as_str().to_string(),
                )
            }
        };

    Ok(Some(ConfigDryRunCapacityView {
        available,
        available_asset,
        mark_price,
        leverage,
        estimated_max_position_qty,
        estimated_max_notional,
        estimated_max_notional_asset,
    }))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use anyhow::{Result, anyhow};
    use chrono::{TimeZone, Utc};
    use poise_core::instrument::{Amount, Asset, InstrumentSpec};
    use poise_core::track::{Instrument, Venue};
    use poise_engine::ports::{
        AccountCapacitySnapshot, AccountPort, AccountSummaryPort, AccountSummarySnapshot,
        ExchangeInfo, ExchangeOpenOrderSnapshot, ExchangePorts, ExecutionPort, MarketDataPort,
        MarketDataTick, MetadataPort, OrderReceipt, OrderRequest, Position, RecentTrackPnlRecords,
    };
    use poise_protocol::TrackPositionQuantityUnitView;
    use tokio::sync::mpsc;

    use super::run_config_dry_run_with_ports;

    #[tokio::test]
    async fn dry_run_loads_metadata_and_account_without_starting_runtime_streams() {
        let config = crate::config::parse_config(
            r#"
[exchange]
venue = "okx"
api_key = "demo-key"
api_secret = "demo-secret"
passphrase = "demo-passphrase"

[[tracks]]
track_id = "btc-core"
symbol = "BTC-USD-SWAP"
lower_price = 60000.0
upper_price = 70000.0
long_exposure_units = 4.0
short_exposure_units = 6.0
notional_per_unit = 300.0
leverage = 3
daily_loss_limit = 0.01
total_loss_limit = 0.03
"#,
        )
        .unwrap();
        let exchange = Arc::new(RecordingDryRunExchange::default());
        let response = run_config_dry_run_with_ports(
            &config,
            ExchangePorts::new(
                exchange.clone(),
                exchange.clone(),
                exchange.clone(),
                exchange.clone(),
                exchange.clone(),
            ),
        )
        .await
        .unwrap();

        assert_eq!(
            exchange.calls(),
            vec![
                "account_summary",
                "metadata:BTC-USD-SWAP",
                "mark_price:BTC-USD-SWAP"
            ]
        );
        assert_eq!(response.account.available_by_asset["BTC"], 0.25);
        assert_eq!(response.tracks.len(), 1);
        let track = &response.tracks[0];
        assert_eq!(track.track_id, "btc-core");
        assert_eq!(track.instrument.venue, "okx");
        assert_eq!(track.instrument.symbol, "BTC-USD-SWAP");
        assert_eq!(track.leverage, 3);
        assert_eq!(
            track.position_qty_unit,
            TrackPositionQuantityUnitView::Contracts
        );
        assert_eq!(track.position_qty_per_unit, 3.0);
        assert_eq!(track.unit_notional_asset, "USD");
        assert_eq!(track.loss_limit_asset, "BTC");
        let capacity = track.capacity.as_ref().unwrap();
        assert_eq!(capacity.available, 0.25);
        assert_eq!(capacity.available_asset, "BTC");
        assert_eq!(capacity.mark_price, 60_000.0);
        assert_eq!(capacity.leverage, 3);
        assert_eq!(capacity.estimated_max_position_qty, 450.0);
        assert_eq!(capacity.estimated_max_notional, 45_000.0);
        assert_eq!(capacity.estimated_max_notional_asset, "USD");
        assert!(response.warnings.is_empty());
    }

    #[tokio::test]
    async fn dry_run_reports_missing_inverse_ct_val_metadata() {
        let config = inverse_config_with_notional_per_unit(300.0);
        let exchange = Arc::new(RecordingDryRunExchange::with_metadata_error(
            "missing ctVal/contract_notional",
        ));

        let error = run_config_dry_run_with_ports(
            &config,
            ExchangePorts::new(
                exchange.clone(),
                exchange.clone(),
                exchange.clone(),
                exchange.clone(),
                exchange.clone(),
            ),
        )
        .await
        .unwrap_err();
        let error = format!("{error:#}");

        assert!(error.contains("BTC-USD-SWAP"));
        assert!(error.contains("ctVal"));
        assert!(error.contains("contract_notional"));
    }

    #[tokio::test]
    async fn dry_run_reports_position_qty_below_minimum_unit() {
        let config = inverse_config_with_notional_per_unit(50.0);
        let exchange = Arc::new(RecordingDryRunExchange::default());

        let error = run_config_dry_run_with_ports(
            &config,
            ExchangePorts::new(
                exchange.clone(),
                exchange.clone(),
                exchange.clone(),
                exchange.clone(),
                exchange.clone(),
            ),
        )
        .await
        .unwrap_err()
        .to_string();

        assert!(error.contains("BTC-USD-SWAP"));
        assert!(error.contains("position_qty_per_unit"));
        assert!(error.contains("0.5"));
        assert!(error.contains("min_qty"));
        assert!(error.contains("1"));
    }

    fn inverse_config_with_notional_per_unit(notional_per_unit: f64) -> crate::config::Config {
        crate::config::parse_config(&format!(
            r#"
[exchange]
venue = "okx"
api_key = "demo-key"
api_secret = "demo-secret"
passphrase = "demo-passphrase"

[[tracks]]
track_id = "btc-core"
symbol = "BTC-USD-SWAP"
lower_price = 60000.0
upper_price = 70000.0
long_exposure_units = 4.0
short_exposure_units = 6.0
notional_per_unit = {notional_per_unit}
leverage = 3
daily_loss_limit = 0.01
total_loss_limit = 0.03
"#
        ))
        .unwrap()
    }

    struct RecordingDryRunExchange {
        calls: Mutex<Vec<String>>,
        instrument_spec: Mutex<InstrumentSpec>,
        metadata_error: Option<String>,
        mark_price: Mutex<Option<f64>>,
    }

    impl Default for RecordingDryRunExchange {
        fn default() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                instrument_spec: Mutex::new(inverse_spec()),
                metadata_error: None,
                mark_price: Mutex::new(Some(60_000.0)),
            }
        }
    }

    impl RecordingDryRunExchange {
        fn with_metadata_error(error: impl Into<String>) -> Self {
            Self {
                metadata_error: Some(error.into()),
                ..Self::default()
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }

        fn record(&self, call: impl Into<String>) {
            self.calls.lock().unwrap().push(call.into());
        }
    }

    #[async_trait::async_trait]
    impl AccountSummaryPort for RecordingDryRunExchange {
        async fn get_account_summary(&self) -> Result<AccountSummarySnapshot> {
            self.record("account_summary");
            Ok(crate::test_support::test_account_summary_snapshot(
                "USD",
                0.3,
                Some(0.25),
                &[("BTC", 0.25)],
                Some(0.0),
                Utc.with_ymd_and_hms(2026, 6, 17, 8, 0, 0).unwrap(),
            ))
        }
    }

    #[async_trait::async_trait]
    impl MetadataPort for RecordingDryRunExchange {
        async fn get_exchange_info(&self, instrument: &Instrument) -> Result<ExchangeInfo> {
            self.record(format!("metadata:{}", instrument.symbol));
            if let Some(error) = &self.metadata_error {
                return Err(anyhow!(error.clone()));
            }
            Ok(ExchangeInfo {
                instrument_spec: self.instrument_spec.lock().unwrap().clone(),
            })
        }

        async fn get_server_time(&self) -> Result<chrono::DateTime<Utc>> {
            Ok(Utc::now())
        }
    }

    #[async_trait::async_trait]
    impl ExecutionPort for RecordingDryRunExchange {
        async fn submit_order(
            &self,
            _req: OrderRequest,
        ) -> poise_engine::ports::ExecutionResult<OrderReceipt> {
            panic!("dry-run must not submit orders")
        }

        async fn cancel_order(
            &self,
            _instrument: &Instrument,
            _order_id: &str,
        ) -> poise_engine::ports::ExecutionResult<OrderReceipt> {
            panic!("dry-run must not cancel orders")
        }

        async fn cancel_all(
            &self,
            _instrument: &Instrument,
        ) -> poise_engine::ports::ExecutionResult<()> {
            panic!("dry-run must not cancel orders")
        }

        async fn get_position(
            &self,
            _instrument: &Instrument,
        ) -> poise_engine::ports::ExecutionResult<Position> {
            panic!("dry-run must not load positions in task 3.2")
        }

        async fn get_open_orders(
            &self,
            _instrument: &Instrument,
        ) -> poise_engine::ports::ExecutionResult<ExchangeOpenOrderSnapshot> {
            panic!("dry-run must not load open orders")
        }
    }

    #[async_trait::async_trait]
    impl AccountPort for RecordingDryRunExchange {
        async fn get_account_capacity_snapshot(
            &self,
            _instrument: &Instrument,
        ) -> Result<AccountCapacitySnapshot> {
            panic!("dry-run capacity estimation is introduced in task 3.3")
        }

        async fn get_recent_track_pnl_records(
            &self,
            _instrument: &Instrument,
            _required_window: &poise_engine::ports::PnlHistoryWindow,
        ) -> Result<RecentTrackPnlRecords> {
            Ok(RecentTrackPnlRecords::Unsupported)
        }

        async fn subscribe_user_data(
            &self,
        ) -> Result<mpsc::Receiver<poise_engine::ports::UserDataEvent>> {
            panic!("dry-run must not subscribe user data")
        }
    }

    #[async_trait::async_trait]
    impl MarketDataPort for RecordingDryRunExchange {
        async fn subscribe_prices(
            &self,
            _instrument: &Instrument,
        ) -> Result<mpsc::Receiver<MarketDataTick>> {
            panic!("dry-run must not subscribe market data")
        }

        async fn get_mark_price(&self, _instrument: &Instrument) -> Result<Option<f64>> {
            self.record("mark_price:BTC-USD-SWAP");
            Ok(*self.mark_price.lock().unwrap())
        }
    }

    fn inverse_spec() -> InstrumentSpec {
        let instrument = Instrument::new(Venue::Okx, "BTC-USD-SWAP");
        let usd = Asset::try_new("USD").unwrap();
        InstrumentSpec::try_inverse(
            instrument,
            Asset::try_new("BTC").unwrap(),
            usd.clone(),
            Asset::try_new("BTC").unwrap(),
            Amount::new(100.0, usd.clone()).unwrap(),
            Default::default(),
            0.1,
            1.0,
            1.0,
            Amount::new(0.0, usd).unwrap(),
        )
        .unwrap()
    }
}
