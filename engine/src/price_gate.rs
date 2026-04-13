use serde::{Deserialize, Serialize};

use crate::executor::OrderRole;
use crate::ports::ExecutionQuote;

pub const MAX_MARK_BOOK_DIVERGENCE_BPS: u32 = 300;
pub const RECOVER_MARK_BOOK_DIVERGENCE_BPS: u32 = 150;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubmitPurpose {
    AutoReconcile,
    ManualRiskReduction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PriceExecutionBlockReason {
    MissingExecutionQuote,
    MarkBookDivergence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PriceExecutionGate {
    Open,
    ManualRiskReductionOnly { reason: PriceExecutionBlockReason },
    NoSubmit { reason: PriceExecutionBlockReason },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkingOrderGateAction {
    Keep,
    Cancel,
}

pub fn evaluate_price_execution_gate(
    previous: PriceExecutionGate,
    mark_price: Option<f64>,
    quote: Option<ExecutionQuote>,
) -> PriceExecutionGate {
    let Some(quote) = quote else {
        return PriceExecutionGate::NoSubmit {
            reason: PriceExecutionBlockReason::MissingExecutionQuote,
        };
    };

    let Some(divergence_bps) = divergence_bps(mark_price, quote) else {
        return PriceExecutionGate::Open;
    };

    let recover_threshold = f64::from(RECOVER_MARK_BOOK_DIVERGENCE_BPS);
    let enter_threshold = f64::from(MAX_MARK_BOOK_DIVERGENCE_BPS);

    let is_recovering_divergence = matches!(
        previous,
        PriceExecutionGate::ManualRiskReductionOnly {
            reason: PriceExecutionBlockReason::MarkBookDivergence
        }
    );

    if is_recovering_divergence {
        if divergence_bps > recover_threshold {
            return PriceExecutionGate::ManualRiskReductionOnly {
                reason: PriceExecutionBlockReason::MarkBookDivergence,
            };
        }
        return PriceExecutionGate::Open;
    }

    if divergence_bps >= enter_threshold {
        return PriceExecutionGate::ManualRiskReductionOnly {
            reason: PriceExecutionBlockReason::MarkBookDivergence,
        };
    }

    PriceExecutionGate::Open
}

pub fn allows_submit(gate: PriceExecutionGate, purpose: SubmitPurpose) -> bool {
    match gate {
        PriceExecutionGate::Open => true,
        PriceExecutionGate::ManualRiskReductionOnly { .. } => {
            matches!(purpose, SubmitPurpose::ManualRiskReduction)
        }
        PriceExecutionGate::NoSubmit { .. } => false,
    }
}

pub fn allows_auto_replace(gate: PriceExecutionGate) -> bool {
    matches!(gate, PriceExecutionGate::Open)
}

pub fn gate_block_reason(gate: PriceExecutionGate) -> Option<PriceExecutionBlockReason> {
    match gate {
        PriceExecutionGate::Open => None,
        PriceExecutionGate::ManualRiskReductionOnly { reason }
        | PriceExecutionGate::NoSubmit { reason } => Some(reason),
    }
}

pub fn gate_from_block_reason(reason: Option<PriceExecutionBlockReason>) -> PriceExecutionGate {
    match reason {
        None => PriceExecutionGate::Open,
        Some(PriceExecutionBlockReason::MissingExecutionQuote) => PriceExecutionGate::NoSubmit {
            reason: PriceExecutionBlockReason::MissingExecutionQuote,
        },
        Some(PriceExecutionBlockReason::MarkBookDivergence) => {
            PriceExecutionGate::ManualRiskReductionOnly {
                reason: PriceExecutionBlockReason::MarkBookDivergence,
            }
        }
    }
}

pub fn restore_gate_from_snapshot(
    reason: Option<PriceExecutionBlockReason>,
    mark_price: Option<f64>,
    best_bid: Option<f64>,
    best_ask: Option<f64>,
) -> PriceExecutionGate {
    match reason {
        Some(reason) => gate_from_block_reason(Some(reason)),
        None => evaluate_price_execution_gate(
            PriceExecutionGate::Open,
            mark_price,
            execution_quote(best_bid, best_ask),
        ),
    }
}

pub fn working_order_gate_action(
    gate: PriceExecutionGate,
    role: OrderRole,
) -> WorkingOrderGateAction {
    match gate {
        PriceExecutionGate::Open => WorkingOrderGateAction::Keep,
        PriceExecutionGate::ManualRiskReductionOnly { .. }
        | PriceExecutionGate::NoSubmit { .. } => match role {
            OrderRole::IncreaseInventory => WorkingOrderGateAction::Cancel,
            OrderRole::DecreaseInventory => WorkingOrderGateAction::Keep,
        },
    }
}

fn divergence_bps(mark_price: Option<f64>, quote: ExecutionQuote) -> Option<f64> {
    let mark_price = mark_price?;
    if !mark_price.is_finite() || mark_price <= f64::EPSILON {
        return None;
    }

    let book_mid = (quote.best_bid + quote.best_ask) / 2.0;
    if !book_mid.is_finite() || book_mid <= f64::EPSILON {
        return None;
    }

    Some(((mark_price - book_mid).abs() / mark_price) * 10_000.0)
}

fn execution_quote(best_bid: Option<f64>, best_ask: Option<f64>) -> Option<ExecutionQuote> {
    Some(ExecutionQuote {
        best_bid: best_bid?,
        best_ask: best_ask?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quote(best_bid: f64, best_ask: f64) -> ExecutionQuote {
        ExecutionQuote { best_bid, best_ask }
    }

    #[test]
    fn price_gate_returns_no_submit_when_quote_is_missing() {
        let gate = evaluate_price_execution_gate(PriceExecutionGate::Open, Some(100.0), None);

        assert_eq!(
            gate,
            PriceExecutionGate::NoSubmit {
                reason: PriceExecutionBlockReason::MissingExecutionQuote,
            }
        );
    }

    #[test]
    fn price_gate_returns_manual_risk_reduction_only_when_mark_book_diverges() {
        let gate = evaluate_price_execution_gate(
            PriceExecutionGate::Open,
            Some(100.0),
            Some(quote(95.0, 95.0)),
        );

        assert_eq!(
            gate,
            PriceExecutionGate::ManualRiskReductionOnly {
                reason: PriceExecutionBlockReason::MarkBookDivergence,
            }
        );
    }

    #[test]
    fn price_gate_reopens_after_divergence_recovers() {
        let blocked = PriceExecutionGate::ManualRiskReductionOnly {
            reason: PriceExecutionBlockReason::MarkBookDivergence,
        };

        let still_blocked =
            evaluate_price_execution_gate(blocked, Some(100.0), Some(quote(97.0, 97.0)));
        let reopened = evaluate_price_execution_gate(blocked, Some(100.0), Some(quote(99.0, 99.0)));

        assert_eq!(still_blocked, blocked);
        assert_eq!(reopened, PriceExecutionGate::Open);
    }
}
