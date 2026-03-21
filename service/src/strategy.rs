use crate::protocol::{
    GridConfig, GridLevel, GridLevelState, GridSide, RiskState, RuntimeState, StrategyState,
    StrategyStatus,
};

const EPSILON: f64 = 1e-9;

pub fn validate_config(config: &GridConfig) -> Result<(), String> {
    if config.levels_per_side == 0 {
        return Err("levels_per_side must be greater than 0".into());
    }
    if config.spacing_bps <= 0.0 {
        return Err("spacing_bps must be greater than 0".into());
    }
    if config.quantity_per_level <= 0.0 {
        return Err("quantity_per_level must be greater than 0".into());
    }
    if config.max_position_qty <= 0.0 {
        return Err("max_position_qty must be greater than 0".into());
    }
    if config.rebuild_threshold_bps <= 0.0 {
        return Err("rebuild_threshold_bps must be greater than 0".into());
    }
    Ok(())
}

pub fn reconcile(
    runtime: &RuntimeState,
    risk: &RiskState,
    previous: &StrategyState,
) -> StrategyState {
    let config = if validate_config(&previous.config).is_ok() {
        previous.config.clone()
    } else {
        GridConfig::default()
    };
    let market_price = market_price(runtime);
    let reference_price = if previous.rebuild_reference_price > EPSILON {
        previous.rebuild_reference_price
    } else if previous.center_price > EPSILON {
        previous.center_price
    } else {
        market_price
    };
    let drift_bps = price_drift_bps(market_price, reference_price);
    let occupied_count = occupied_levels(runtime.position_qty, config.quantity_per_level);

    if drift_bps >= config.rebuild_threshold_bps {
        if occupied_count > 0 || risk.breaker_engaged || runtime.strategy_state == "paused" {
            let mut levels = build_levels(reference_price, &config);
            apply_level_states(
                &mut levels,
                runtime.position_qty,
                occupied_count,
                GridLifecycleMode::PendingRebuild,
            );
            let (lower_bound, upper_bound) = bounds(&levels);
            return StrategyState {
                config,
                status: StrategyStatus::PendingRebuild,
                center_price: reference_price,
                lower_bound,
                upper_bound,
                rebuild_reference_price: reference_price,
                pending_rebuild_reason: Some(pending_rebuild_reason(
                    drift_bps,
                    occupied_count > 0,
                    risk.breaker_engaged,
                    runtime.strategy_state == "paused",
                )),
                levels,
            };
        }

        let mut levels = build_levels(market_price, &config);
        apply_level_states(
            &mut levels,
            runtime.position_qty,
            occupied_count,
            GridLifecycleMode::Normal,
        );
        let (lower_bound, upper_bound) = bounds(&levels);
        return StrategyState {
            config,
            status: strategy_status(occupied_count),
            center_price: market_price,
            lower_bound,
            upper_bound,
            rebuild_reference_price: market_price,
            pending_rebuild_reason: None,
            levels,
        };
    }

    let mut levels = build_levels(reference_price, &config);
    apply_level_states(
        &mut levels,
        runtime.position_qty,
        occupied_count,
        GridLifecycleMode::Normal,
    );
    let (lower_bound, upper_bound) = bounds(&levels);
    StrategyState {
        config,
        status: strategy_status(occupied_count),
        center_price: reference_price,
        lower_bound,
        upper_bound,
        rebuild_reference_price: reference_price,
        pending_rebuild_reason: None,
        levels,
    }
}

#[derive(Clone, Copy)]
enum GridLifecycleMode {
    Normal,
    PendingRebuild,
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

fn strategy_status(occupied_count: usize) -> StrategyStatus {
    if occupied_count > 0 {
        StrategyStatus::Occupied
    } else {
        StrategyStatus::Active
    }
}

fn pending_rebuild_reason(
    drift_bps: f64,
    occupied: bool,
    breaker_engaged: bool,
    paused: bool,
) -> String {
    if occupied {
        return format!(
            "price drift {:.1}bps while inventory is still occupied",
            drift_bps
        );
    }
    if breaker_engaged {
        return format!("price drift {:.1}bps while breaker is engaged", drift_bps);
    }
    if paused {
        return format!("price drift {:.1}bps while strategy is paused", drift_bps);
    }
    format!("price drift {:.1}bps exceeded rebuild threshold", drift_bps)
}

fn price_drift_bps(current_price: f64, reference_price: f64) -> f64 {
    if reference_price.abs() <= EPSILON {
        return 0.0;
    }
    ((current_price - reference_price).abs() / reference_price) * 10_000.0
}

fn occupied_levels(position_qty: f64, quantity_per_level: f64) -> usize {
    if quantity_per_level <= EPSILON {
        return 0;
    }
    (position_qty.abs() / quantity_per_level).ceil() as usize
}

fn build_levels(center_price: f64, config: &GridConfig) -> Vec<GridLevel> {
    let mut levels = Vec::with_capacity((config.levels_per_side * 2) as usize);

    for step in 1..=config.levels_per_side {
        let multiplier = (config.spacing_bps * step as f64) / 10_000.0;
        levels.push(GridLevel {
            level_id: format!("buy_{step:02}"),
            side: GridSide::Buy,
            price: round_price(center_price * (1.0 - multiplier)),
            quantity: config.quantity_per_level,
            state: GridLevelState::Active,
            client_order_id: Some(format!("grid_buy_{step:02}")),
            order_id: Some(format!("ord_buy_{step:02}")),
        });
    }

    for step in 1..=config.levels_per_side {
        let multiplier = (config.spacing_bps * step as f64) / 10_000.0;
        levels.push(GridLevel {
            level_id: format!("sell_{step:02}"),
            side: GridSide::Sell,
            price: round_price(center_price * (1.0 + multiplier)),
            quantity: config.quantity_per_level,
            state: GridLevelState::Active,
            client_order_id: Some(format!("grid_sell_{step:02}")),
            order_id: Some(format!("ord_sell_{step:02}")),
        });
    }

    levels
}

fn apply_level_states(
    levels: &mut [GridLevel],
    position_qty: f64,
    occupied_count: usize,
    mode: GridLifecycleMode,
) {
    match position_qty.partial_cmp(&0.0) {
        Some(std::cmp::Ordering::Greater) => mark_levels(levels, GridSide::Buy, occupied_count),
        Some(std::cmp::Ordering::Less) => mark_levels(levels, GridSide::Sell, occupied_count),
        _ => {}
    }

    if matches!(mode, GridLifecycleMode::PendingRebuild) {
        for level in levels.iter_mut() {
            if level.state == GridLevelState::Active {
                level.state = GridLevelState::PendingRebuild;
            }
        }
    }
}

fn mark_levels(levels: &mut [GridLevel], side: GridSide, occupied_count: usize) {
    let mut remaining = occupied_count;
    for level in levels.iter_mut().filter(|level| level.side == side) {
        if remaining == 0 {
            break;
        }
        level.state = GridLevelState::Occupied;
        remaining -= 1;
    }
}

fn bounds(levels: &[GridLevel]) -> (f64, f64) {
    let lower = levels
        .iter()
        .map(|level| level.price)
        .reduce(f64::min)
        .unwrap_or_default();
    let upper = levels
        .iter()
        .map(|level| level.price)
        .reduce(f64::max)
        .unwrap_or_default();
    (lower, upper)
}

fn round_price(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::RiskLevel;

    fn runtime_state() -> RuntimeState {
        RuntimeState {
            symbol: "XAUUSDT".into(),
            env: "paper".into(),
            session_state: "regular".into(),
            strategy_state: "running".into(),
            last_price: 100.0,
            mark_price: 100.0,
            position_qty: 0.0,
            position_avg_price: 0.0,
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

    fn previous_state() -> StrategyState {
        StrategyState {
            config: GridConfig::default(),
            status: StrategyStatus::Active,
            center_price: 100.0,
            lower_bound: 0.0,
            upper_bound: 0.0,
            rebuild_reference_price: 100.0,
            pending_rebuild_reason: None,
            levels: Vec::new(),
        }
    }

    #[test]
    fn validate_config_rejects_zero_levels() {
        let config = GridConfig {
            levels_per_side: 0,
            ..GridConfig::default()
        };

        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn reconcile_marks_levels_pending_rebuild_when_paused_blocks_rebuild() {
        let mut runtime = runtime_state();
        runtime.strategy_state = "paused".into();
        runtime.last_price = 102.0;
        runtime.mark_price = 102.0;

        let risk = risk_state();
        let mut previous = previous_state();
        previous.config.rebuild_threshold_bps = 50.0;

        let strategy = reconcile(&runtime, &risk, &previous);

        assert_eq!(strategy.status, StrategyStatus::PendingRebuild);
        assert!(
            strategy
                .pending_rebuild_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("paused"))
        );
        assert!(
            strategy
                .levels
                .iter()
                .all(|level| level.state == GridLevelState::PendingRebuild)
        );
    }

    #[test]
    fn reconcile_marks_buy_levels_occupied_for_long_inventory() {
        let mut runtime = runtime_state();
        runtime.position_qty = 0.15;
        runtime.position_avg_price = 99.0;

        let strategy = reconcile(&runtime, &risk_state(), &previous_state());

        assert_eq!(strategy.status, StrategyStatus::Occupied);
        assert_eq!(
            strategy
                .levels
                .iter()
                .filter(|level| level.side == GridSide::Buy)
                .filter(|level| level.state == GridLevelState::Occupied)
                .count(),
            2
        );
        assert_eq!(
            strategy
                .levels
                .iter()
                .filter(|level| level.side == GridSide::Sell)
                .filter(|level| level.state == GridLevelState::Occupied)
                .count(),
            0
        );
        assert!(strategy.pending_rebuild_reason.is_none());
    }
}
