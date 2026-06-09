use anyhow::{Result, ensure};
use poise_core::risk::LossGuardSnapshot;

use crate::ledger::TrackPnlStats;
use crate::runtime::RiskState;

pub fn build_loss_guard_snapshot(
    pnl_stats: &TrackPnlStats,
    risk_state: &RiskState,
) -> LossGuardSnapshot {
    build_loss_guard_snapshot_for_asset(pnl_stats, risk_state, None)
        .expect("loss guard snapshot without asset check should be infallible")
}

pub fn build_loss_guard_snapshot_for_asset(
    pnl_stats: &TrackPnlStats,
    risk_state: &RiskState,
    settlement_asset: Option<&str>,
) -> Result<LossGuardSnapshot> {
    if let (Some(expected), Some(actual)) = (settlement_asset, pnl_stats.pnl_asset.as_deref()) {
        ensure!(
            actual == expected,
            "pnl asset `{actual}` does not match settlement asset `{expected}`"
        );
    }

    Ok(LossGuardSnapshot {
        net_realized_pnl_today: pnl_stats.net_realized_pnl_today(),
        net_realized_pnl_cumulative: pnl_stats.net_realized_pnl_cumulative(),
        unrealized_pnl: risk_state.unrealized_pnl,
    })
}

#[cfg(test)]
mod tests {
    use chrono::NaiveDate;

    use super::*;

    #[test]
    fn loss_guard_snapshot_accepts_matching_pnl_asset() {
        let pnl_stats = TrackPnlStats {
            pnl_utc_day: NaiveDate::from_ymd_opt(2026, 4, 8).unwrap(),
            pnl_asset: Some("BTC".to_string()),
            gross_realized_pnl_today: -0.01,
            gross_realized_pnl_cumulative: -0.02,
            ..TrackPnlStats::default()
        };

        let snapshot = build_loss_guard_snapshot_for_asset(
            &pnl_stats,
            &RiskState {
                unrealized_pnl: -0.03,
            },
            Some("BTC"),
        )
        .unwrap();

        assert_eq!(snapshot.net_realized_pnl_today, -0.01);
        assert_eq!(snapshot.unrealized_pnl, -0.03);
    }

    #[test]
    fn loss_guard_snapshot_rejects_mismatched_pnl_asset() {
        let pnl_stats = TrackPnlStats {
            pnl_utc_day: NaiveDate::from_ymd_opt(2026, 4, 8).unwrap(),
            pnl_asset: Some("USDT".to_string()),
            ..TrackPnlStats::default()
        };

        let error = build_loss_guard_snapshot_for_asset(
            &pnl_stats,
            &RiskState {
                unrealized_pnl: 0.0,
            },
            Some("BTC"),
        )
        .unwrap_err();

        assert!(error.to_string().contains("settlement asset"));
    }
}
