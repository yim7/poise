use poise_protocol::{AccountSummaryView, RiskSignalView};

use poise_application::{AccountReadModel, AccountRiskSignal};

pub struct AccountProjector;

impl AccountProjector {
    pub fn new() -> Self {
        Self
    }

    pub fn project_summary(&self, model: &AccountReadModel) -> AccountSummaryView {
        AccountSummaryView {
            equity: Some(model.equity),
            available: Some(model.available),
            unrealized_pnl: Some(model.unrealized_pnl),
            day_change_pct: model.day_change_pct,
            risk_signal: project_risk_signal(model.risk_signal),
            reason: model.reason.clone(),
            day_base_at: Some(model.day_base_at.to_rfc3339()),
            updated_at: Some(model.updated_at.to_rfc3339()),
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

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use poise_protocol::{AccountSummaryView, RiskSignalView};

    use super::AccountProjector;
    use poise_application::{AccountReadModel, AccountRiskSignal};

    #[test]
    fn projects_account_read_model_to_summary_view() {
        let projector = AccountProjector::new();
        let model = AccountReadModel {
            equity: 12_500.0,
            available: 9_000.0,
            unrealized_pnl: -350.0,
            baseline_equity: 12_800.0,
            day_base_at: Utc.with_ymd_and_hms(2026, 4, 4, 0, 0, 1).unwrap(),
            day_change_pct: Some(-2.75),
            risk_signal: AccountRiskSignal::Attention,
            reason: Some("day_change -2.75%".to_string()),
            updated_at: Utc.with_ymd_and_hms(2026, 4, 4, 1, 23, 45).unwrap(),
        };

        assert_eq!(
            projector.project_summary(&model),
            AccountSummaryView {
                equity: Some(12_500.0),
                available: Some(9_000.0),
                unrealized_pnl: Some(-350.0),
                day_change_pct: Some(-2.75),
                risk_signal: RiskSignalView::Attention,
                reason: Some("day_change -2.75%".to_string()),
                day_base_at: Some("2026-04-04T00:00:01+00:00".to_string()),
                updated_at: Some("2026-04-04T01:23:45+00:00".to_string()),
            }
        );
    }
}
