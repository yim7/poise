use crate::kernel::now_utc;
use crate::protocol::{GridConfig, RiskEvent, RiskLevel, RiskState, RuntimeState};

const EPSILON: f64 = 1e-9;

pub struct RiskEvaluation {
    pub state: RiskState,
    pub new_events: Vec<RiskEvent>,
}

pub fn evaluate(runtime: &RuntimeState, previous: &RiskState, config: &GridConfig) -> RiskEvaluation {
    let price = market_price(runtime);
    let max_notional = round_price(config.max_position_qty.abs() * price);
    let current_notional = round_price(runtime.position_qty.abs() * price);
    let total_pnl = runtime.realized_pnl + runtime.unrealized_pnl;

    let max_position_exceeded = runtime.position_qty.abs() > config.max_position_qty + EPSILON;
    let stop_loss_triggered = stop_loss_triggered(runtime, price, previous.stop_loss_pct);
    let daily_loss_breached = total_pnl <= previous.daily_loss_limit;
    let breaker_engaged = max_position_exceeded || stop_loss_triggered || daily_loss_breached;

    let usage_ratio = if max_notional > EPSILON {
        current_notional / max_notional
    } else {
        0.0
    };
    let risk_level = if breaker_engaged || usage_ratio >= 0.9 {
        RiskLevel::Danger
    } else if usage_ratio >= 0.75 || total_pnl <= previous.daily_loss_limit * 0.8 {
        RiskLevel::Warning
    } else if usage_ratio >= 0.5 || total_pnl <= previous.daily_loss_limit * 0.5 {
        RiskLevel::Watch
    } else {
        RiskLevel::Ok
    };

    let previous_max_position_exceeded =
        previous.max_position_exceeded || previous.current_notional > previous.max_notional + EPSILON;
    let mut new_events = Vec::new();
    if max_position_exceeded && !previous_max_position_exceeded {
        new_events.push(risk_event(
            RiskLevel::Danger,
            "MAX_POSITION_EXCEEDED",
            format!(
                "Position quantity {:.3} exceeded configured max {:.3}.",
                runtime.position_qty.abs(),
                config.max_position_qty
            ),
        ));
    }
    if stop_loss_triggered && !previous.stop_loss_triggered {
        new_events.push(risk_event(
            RiskLevel::Danger,
            "STOP_LOSS_TRIGGERED",
            format!(
                "Mark price {:.2} crossed the configured stop-loss threshold {:.1}%.",
                price,
                previous.stop_loss_pct
            ),
        ));
    }
    if daily_loss_breached && !previous.daily_loss_breached {
        new_events.push(risk_event(
            RiskLevel::Danger,
            "DAILY_LOSS_LIMIT_BREACHED",
            format!(
                "Combined daily PnL {:.2} breached the configured limit {:.2}.",
                total_pnl,
                previous.daily_loss_limit
            ),
        ));
    }
    if !breaker_engaged && previous.breaker_engaged {
        new_events.push(risk_event(
            RiskLevel::Watch,
            "BREAKER_RELEASED",
            "Breaker released after risk metrics returned inside configured limits.".into(),
        ));
    }

    let unacked_alerts = previous
        .unacked_alerts
        .saturating_add(new_events.len() as u32);

    RiskEvaluation {
        state: RiskState {
            current_notional,
            max_notional,
            daily_loss_limit: previous.daily_loss_limit,
            stop_loss_pct: previous.stop_loss_pct,
            risk_level,
            max_position_exceeded,
            stop_loss_triggered,
            daily_loss_breached,
            breaker_engaged,
            unacked_alerts,
        },
        new_events,
    }
}

fn risk_event(severity: RiskLevel, code: &str, message: String) -> RiskEvent {
    RiskEvent {
        severity,
        code: code.into(),
        message,
        created_at: now_utc(),
        acknowledged_at: None,
    }
}

fn market_price(runtime: &RuntimeState) -> f64 {
    if runtime.mark_price.abs() > EPSILON {
        runtime.mark_price
    } else if runtime.last_price.abs() > EPSILON {
        runtime.last_price
    } else {
        1.0
    }
}

fn stop_loss_triggered(runtime: &RuntimeState, mark_price: f64, stop_loss_pct: f64) -> bool {
    if runtime.position_qty.abs() <= EPSILON || runtime.position_avg_price.abs() <= EPSILON {
        return false;
    }

    let move_pct = if runtime.position_qty > 0.0 {
        ((mark_price - runtime.position_avg_price) / runtime.position_avg_price) * 100.0
    } else {
        ((runtime.position_avg_price - mark_price) / runtime.position_avg_price) * 100.0
    };

    move_pct <= -stop_loss_pct
}

fn round_price(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::evaluate;
    use crate::protocol::{GridConfig, RiskLevel, RiskState, RuntimeState};

    fn runtime_state() -> RuntimeState {
        RuntimeState {
            symbol: "BTCUSDT".into(),
            env: "paper".into(),
            session_state: "regular".into(),
            strategy_state: "running".into(),
            last_price: 100.0,
            mark_price: 100.0,
            position_qty: 0.0,
            position_avg_price: 100.0,
            unrealized_pnl: 0.0,
            realized_pnl: 0.0,
        }
    }

    fn risk_state() -> RiskState {
        RiskState {
            current_notional: 0.0,
            max_notional: 30.0,
            daily_loss_limit: -120.0,
            stop_loss_pct: 4.0,
            risk_level: RiskLevel::Ok,
            max_position_exceeded: false,
            stop_loss_triggered: false,
            daily_loss_breached: false,
            breaker_engaged: false,
            unacked_alerts: 0,
        }
    }

    #[test]
    fn evaluate_emits_multiple_new_rule_events_in_same_pass() {
        let mut runtime = runtime_state();
        runtime.position_qty = 0.5;
        runtime.unrealized_pnl = -130.0;

        let evaluation = evaluate(&runtime, &risk_state(), &GridConfig::default());
        let codes = evaluation
            .new_events
            .iter()
            .map(|event| event.code.as_str())
            .collect::<Vec<_>>();

        assert!(codes.contains(&"MAX_POSITION_EXCEEDED"));
        assert!(codes.contains(&"DAILY_LOSS_LIMIT_BREACHED"));
    }

    #[test]
    fn evaluate_emits_new_rule_event_even_when_breaker_is_already_engaged() {
        let mut previous = risk_state();
        previous.current_notional = 50.0;
        previous.max_notional = 30.0;
        previous.breaker_engaged = true;
        previous.risk_level = RiskLevel::Danger;
        previous.unacked_alerts = 1;

        let mut runtime = runtime_state();
        runtime.position_qty = 0.5;
        runtime.mark_price = 95.0;

        let evaluation = evaluate(&runtime, &previous, &GridConfig::default());
        let codes = evaluation
            .new_events
            .iter()
            .map(|event| event.code.as_str())
            .collect::<Vec<_>>();

        assert!(codes.contains(&"STOP_LOSS_TRIGGERED"));
    }
}
