use poise_core::risk::LossGuardSnapshot;

use crate::ledger::TrackPnlStats;
use crate::runtime::RiskState;

pub fn build_loss_guard_snapshot(
    pnl_stats: &TrackPnlStats,
    risk_state: &RiskState,
) -> LossGuardSnapshot {
    LossGuardSnapshot {
        net_realized_pnl_today: pnl_stats.net_realized_pnl_today(),
        net_realized_pnl_cumulative: pnl_stats.net_realized_pnl_cumulative(),
        unrealized_pnl: risk_state.unrealized_pnl,
    }
}
