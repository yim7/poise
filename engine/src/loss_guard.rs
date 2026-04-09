use poise_core::risk::LossGuardSnapshot;

use crate::ledger::TrackLedgerState;
use crate::runtime::RiskState;

pub fn build_loss_guard_snapshot(
    ledger_state: &TrackLedgerState,
    risk_state: &RiskState,
) -> LossGuardSnapshot {
    LossGuardSnapshot {
        net_realized_pnl_today: ledger_state.net_realized_pnl_today(),
        net_realized_pnl_cumulative: ledger_state.net_realized_pnl_cumulative(),
        unrealized_pnl: risk_state.unrealized_pnl,
    }
}
