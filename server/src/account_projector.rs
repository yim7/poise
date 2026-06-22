use poise_protocol::{
    AccountAnalysisView, AccountAssetExposureView, AccountAssetQuantitySourceView,
    AccountHedgeLikeView, AccountSummaryView, AccountTrackAnalysisView, InstrumentView,
    RiskSignalView,
};

use poise_application::{
    AccountAnalysisReadModel, AccountAssetExposureReadModel, AccountAssetQuantitySource,
    AccountHedgeLikeReadModel, AccountReadModel, AccountRiskSignal, AccountTrackAnalysisReadModel,
};

pub struct AccountProjector;

impl AccountProjector {
    pub fn new() -> Self {
        Self
    }

    pub fn project_summary_with_analysis(
        &self,
        model: Option<&AccountReadModel>,
        analysis: Option<&AccountAnalysisReadModel>,
    ) -> AccountSummaryView {
        AccountSummaryView {
            equity: model.map(|model| model.equity),
            available: model.map(|model| model.available),
            unrealized_pnl: model.map(|model| model.unrealized_pnl),
            day_change_pct: model.and_then(|model| model.day_change_pct),
            risk_signal: model
                .map(|model| project_risk_signal(model.risk_signal))
                .unwrap_or_default(),
            reason: model.and_then(|model| model.reason.clone()),
            day_base_at: model.map(|model| model.day_base_at.to_rfc3339()),
            updated_at: model.map(|model| model.updated_at.to_rfc3339()),
            analysis: analysis.map(project_account_analysis),
        }
    }
}

fn project_risk_signal(signal: AccountRiskSignal) -> RiskSignalView {
    match signal {
        AccountRiskSignal::Normal => RiskSignalView::Normal,
        AccountRiskSignal::Attention => RiskSignalView::Attention,
        AccountRiskSignal::Critical => RiskSignalView::Critical,
    }
}

fn project_account_analysis(source: &AccountAnalysisReadModel) -> AccountAnalysisView {
    AccountAnalysisView {
        tracks: source.tracks.iter().map(project_account_track).collect(),
        total_contracts: source.total_contracts,
        total_signed_usd_notional: source.total_signed_usd_notional,
        total_abs_usd_notional: source.total_abs_usd_notional,
        base_exposures: source
            .base_exposures
            .iter()
            .map(project_account_asset_exposure)
            .collect(),
        hedge_like: source
            .hedge_like
            .iter()
            .map(project_account_hedge_like)
            .collect(),
    }
}

fn project_account_track(source: &AccountTrackAnalysisReadModel) -> AccountTrackAnalysisView {
    AccountTrackAnalysisView {
        track_id: source.track_id.clone(),
        instrument: InstrumentView {
            venue: source.instrument.venue.as_str().to_string(),
            symbol: source.instrument.symbol.clone(),
        },
        settlement_asset: source.settlement_asset.clone(),
        native_quantity: source.native_quantity,
        contract_count: source.contract_count,
        signed_usd_notional: source.signed_usd_notional,
        abs_usd_notional: source.abs_usd_notional,
        estimated_base_asset: source.estimated_base_asset.clone(),
        estimated_base_exposure: source.estimated_base_exposure,
        pnl_asset: source.pnl_asset.clone(),
    }
}

fn project_account_asset_exposure(
    source: &AccountAssetExposureReadModel,
) -> AccountAssetExposureView {
    AccountAssetExposureView {
        asset: source.asset.clone(),
        quantity: source.quantity,
    }
}

fn project_account_hedge_like(source: &AccountHedgeLikeReadModel) -> AccountHedgeLikeView {
    AccountHedgeLikeView {
        asset: source.asset.clone(),
        contract_base_exposure: source.contract_base_exposure,
        account_asset_quantity: source.account_asset_quantity,
        account_asset_quantity_source: source
            .account_asset_quantity_source
            .map(project_account_asset_quantity_source),
        net_base_exposure: source.net_base_exposure,
    }
}

fn project_account_asset_quantity_source(
    source: AccountAssetQuantitySource,
) -> AccountAssetQuantitySourceView {
    match source {
        AccountAssetQuantitySource::AccountSummaryAvailableByAsset => {
            AccountAssetQuantitySourceView::AccountSummaryAvailableByAsset
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use chrono::{TimeZone, Utc};
    use poise_core::track::{Instrument, Venue};
    use poise_protocol::{
        AccountAnalysisView, AccountAssetExposureView, AccountAssetQuantitySourceView,
        AccountHedgeLikeView, AccountSummaryView, AccountTrackAnalysisView, InstrumentView,
        RiskSignalView,
    };

    use super::AccountProjector;
    use poise_application::{
        AccountAnalysisReadModel, AccountAssetExposureReadModel, AccountAssetQuantitySource,
        AccountHedgeLikeReadModel, AccountReadModel, AccountRiskSignal,
        AccountTrackAnalysisReadModel,
    };

    #[test]
    fn projects_account_read_model_to_summary_view() {
        let projector = AccountProjector::new();
        let model = AccountReadModel {
            equity: 12_500.0,
            available: 9_000.0,
            available_by_asset: BTreeMap::from([("BTC".to_string(), 0.2)]),
            unrealized_pnl: -350.0,
            baseline_equity: 12_800.0,
            day_base_at: Utc.with_ymd_and_hms(2026, 4, 4, 0, 0, 1).unwrap(),
            day_change_pct: Some(-2.75),
            risk_signal: AccountRiskSignal::Attention,
            reason: Some("day_change -2.75%".to_string()),
            updated_at: Utc.with_ymd_and_hms(2026, 4, 4, 1, 23, 45).unwrap(),
        };

        assert_eq!(
            projector.project_summary_with_analysis(Some(&model), None),
            AccountSummaryView {
                equity: Some(12_500.0),
                available: Some(9_000.0),
                unrealized_pnl: Some(-350.0),
                day_change_pct: Some(-2.75),
                risk_signal: RiskSignalView::Attention,
                reason: Some("day_change -2.75%".to_string()),
                day_base_at: Some("2026-04-04T00:00:01+00:00".to_string()),
                updated_at: Some("2026-04-04T01:23:45+00:00".to_string()),
                analysis: None,
            }
        );
    }

    #[test]
    fn projects_account_analysis_to_summary_view() {
        let projector = AccountProjector::new();
        let model = AccountReadModel {
            equity: 12_500.0,
            available: 9_000.0,
            available_by_asset: BTreeMap::from([("BTC".to_string(), 0.2)]),
            unrealized_pnl: -350.0,
            baseline_equity: 12_800.0,
            day_base_at: Utc.with_ymd_and_hms(2026, 4, 4, 0, 0, 1).unwrap(),
            day_change_pct: Some(-2.75),
            risk_signal: AccountRiskSignal::Attention,
            reason: Some("day_change -2.75%".to_string()),
            updated_at: Utc.with_ymd_and_hms(2026, 4, 4, 1, 23, 45).unwrap(),
        };
        let analysis = AccountAnalysisReadModel {
            tracks: vec![AccountTrackAnalysisReadModel {
                track_id: "btc-coin".to_string(),
                instrument: Instrument::new(Venue::Okx, "BTC-USD-SWAP"),
                settlement_asset: "BTC".to_string(),
                native_quantity: -30.0,
                contract_count: Some(-30.0),
                signed_usd_notional: -3000.0,
                abs_usd_notional: 3000.0,
                estimated_base_asset: Some("BTC".to_string()),
                estimated_base_exposure: Some(-0.03),
                pnl_asset: "BTC".to_string(),
            }],
            total_contracts: -30.0,
            total_signed_usd_notional: -3000.0,
            total_abs_usd_notional: 3000.0,
            base_exposures: vec![AccountAssetExposureReadModel {
                asset: "BTC".to_string(),
                quantity: -0.03,
            }],
            hedge_like: vec![AccountHedgeLikeReadModel {
                asset: "BTC".to_string(),
                contract_base_exposure: -0.03,
                account_asset_quantity: Some(0.2),
                account_asset_quantity_source: Some(
                    AccountAssetQuantitySource::AccountSummaryAvailableByAsset,
                ),
                net_base_exposure: Some(0.17),
            }],
        };

        let summary = projector.project_summary_with_analysis(Some(&model), Some(&analysis));

        assert_eq!(
            summary.analysis,
            Some(AccountAnalysisView {
                tracks: vec![AccountTrackAnalysisView {
                    track_id: "btc-coin".to_string(),
                    instrument: InstrumentView {
                        venue: "okx".to_string(),
                        symbol: "BTC-USD-SWAP".to_string(),
                    },
                    settlement_asset: "BTC".to_string(),
                    native_quantity: -30.0,
                    contract_count: Some(-30.0),
                    signed_usd_notional: -3000.0,
                    abs_usd_notional: 3000.0,
                    estimated_base_asset: Some("BTC".to_string()),
                    estimated_base_exposure: Some(-0.03),
                    pnl_asset: "BTC".to_string(),
                }],
                total_contracts: -30.0,
                total_signed_usd_notional: -3000.0,
                total_abs_usd_notional: 3000.0,
                base_exposures: vec![AccountAssetExposureView {
                    asset: "BTC".to_string(),
                    quantity: -0.03,
                }],
                hedge_like: vec![AccountHedgeLikeView {
                    asset: "BTC".to_string(),
                    contract_base_exposure: -0.03,
                    account_asset_quantity: Some(0.2),
                    account_asset_quantity_source: Some(
                        AccountAssetQuantitySourceView::AccountSummaryAvailableByAsset,
                    ),
                    net_base_exposure: Some(0.17),
                }],
            })
        );
    }
}
