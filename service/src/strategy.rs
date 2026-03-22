use crate::protocol::{
    GridConfig, GridLevel, GridLevelState, GridSide, RiskState, RuntimeState, StrategyState,
    StrategyStatus,
};

const EPSILON: f64 = 1e-9;

pub fn validate_config(config: &GridConfig) -> Result<(), String> {
    if !config.lower_price.is_finite() || !config.upper_price.is_finite() {
        return Err("lower_price and upper_price must be finite".into());
    }
    if config.lower_price >= config.upper_price {
        return Err("lower_price must be less than upper_price".into());
    }
    if config.grid_levels < 2 {
        return Err("grid_levels must be at least 2".into());
    }
    if !config.max_position_notional.is_finite() || config.max_position_notional <= 0.0 {
        return Err("max_position_notional must be greater than 0".into());
    }
    Ok(())
}

pub fn reconcile(
    runtime: &RuntimeState,
    _risk: &RiskState,
    previous: &StrategyState,
) -> StrategyState {
    let config = if validate_config(&previous.config).is_ok() {
        previous.config.clone()
    } else {
        GridConfig::default()
    };
    let lower_bound = round_price(config.normalize_price(config.lower_price));
    let upper_bound = round_price(config.normalize_price(config.upper_price));
    let center_price = round_price(config.midpoint_price());
    let occupied_count = occupied_levels(runtime.position_qty, config.quantity_per_level());

    let Some(market_price) = market_price(runtime) else {
        return StrategyState {
            config,
            status: StrategyStatus::WaitingMarketPrice,
            center_price,
            lower_bound,
            upper_bound,
            status_reason: Some("waiting for first market price".into()),
            levels: Vec::new(),
        };
    };

    let mut levels = build_levels(market_price, &config);
    apply_level_states(&mut levels, runtime.position_qty, occupied_count);

    let outside_range =
        market_price < config.lower_price - EPSILON || market_price > config.upper_price + EPSILON;
    let status = if outside_range && occupied_count == 0 {
        StrategyStatus::WaitingRangeEntry
    } else if occupied_count > 0 {
        StrategyStatus::Occupied
    } else {
        StrategyStatus::Active
    };
    let status_reason = if outside_range {
        Some(range_status_reason(
            market_price,
            lower_bound,
            upper_bound,
            occupied_count > 0,
        ))
    } else {
        None
    };

    StrategyState {
        config,
        status,
        center_price,
        lower_bound,
        upper_bound,
        status_reason,
        levels,
    }
}

fn market_price(runtime: &RuntimeState) -> Option<f64> {
    if runtime.mark_price.abs() > EPSILON {
        Some(runtime.mark_price)
    } else if runtime.last_price.abs() > EPSILON {
        Some(runtime.last_price)
    } else {
        None
    }
}

fn range_status_reason(
    market_price: f64,
    lower_bound: f64,
    upper_bound: f64,
    occupied: bool,
) -> String {
    if occupied {
        return format!(
            "current price {:.2} is outside configured range {:.2}-{:.2}; stopped placing new orders",
            market_price, lower_bound, upper_bound
        );
    }
    format!(
        "current price {:.2} is outside configured range {:.2}-{:.2}",
        market_price, lower_bound, upper_bound
    )
}

fn occupied_levels(position_qty: f64, quantity_per_level: f64) -> usize {
    if quantity_per_level <= EPSILON {
        return 0;
    }
    (position_qty.abs() / quantity_per_level).ceil() as usize
}

fn build_levels(market_price: f64, config: &GridConfig) -> Vec<GridLevel> {
    let step = (config.upper_price - config.lower_price) / (config.grid_levels - 1) as f64;
    let quantity = config.quantity_per_level();

    let mut levels = Vec::with_capacity(config.grid_levels as usize);
    for index in 0..config.grid_levels {
        let price = round_price(config.normalize_price(config.lower_price + step * index as f64));
        let side = if price < market_price - EPSILON {
            Some(GridSide::Buy)
        } else if price > market_price + EPSILON {
            Some(GridSide::Sell)
        } else {
            None
        };
        let Some(side) = side else {
            continue;
        };
        let step_id = index + 1;
        let side_label = match side {
            GridSide::Buy => "buy",
            GridSide::Sell => "sell",
        };
        levels.push(GridLevel {
            level_id: format!("{side_label}_{step_id:02}"),
            side,
            price,
            quantity,
            state: GridLevelState::Active,
            client_order_id: Some(format!("grid_{side_label}_{step_id:02}")),
            order_id: Some(format!("ord_{side_label}_{step_id:02}")),
        });
    }

    levels
}

fn apply_level_states(levels: &mut [GridLevel], position_qty: f64, occupied_count: usize) {
    match position_qty.partial_cmp(&0.0) {
        Some(std::cmp::Ordering::Greater) => mark_levels(levels, GridSide::Buy, occupied_count),
        Some(std::cmp::Ordering::Less) => mark_levels(levels, GridSide::Sell, occupied_count),
        _ => {}
    }
}

fn mark_levels(levels: &mut [GridLevel], side: GridSide, occupied_count: usize) {
    let mut indices = levels
        .iter()
        .enumerate()
        .filter(|(_, level)| level.side == side)
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    indices.sort_by(|left, right| match side {
        GridSide::Buy => levels[*right]
            .price
            .partial_cmp(&levels[*left].price)
            .unwrap_or(std::cmp::Ordering::Equal),
        GridSide::Sell => levels[*left]
            .price
            .partial_cmp(&levels[*right].price)
            .unwrap_or(std::cmp::Ordering::Equal),
    });
    for index in indices.into_iter().take(occupied_count) {
        levels[index].state = GridLevelState::Occupied;
    }
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
            status_reason: None,
            levels: Vec::new(),
        }
    }

    #[test]
    fn validate_config_rejects_invalid_range_levels() {
        let config = GridConfig {
            grid_levels: 1,
            ..GridConfig::default()
        };

        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn builds_range_ladder_with_inclusive_boundaries() {
        let levels = build_levels(100.0, &config());
        let prices: Vec<f64> = levels.iter().map(|level| level.price).collect();

        assert_eq!(prices, vec![90.0, 94.0, 98.0, 102.0, 106.0, 110.0]);
        assert_eq!(
            levels.iter().map(|level| level.side).collect::<Vec<_>>(),
            vec![
                GridSide::Buy,
                GridSide::Buy,
                GridSide::Buy,
                GridSide::Sell,
                GridSide::Sell,
                GridSide::Sell
            ]
        );
    }

    #[test]
    fn reconcile_waits_when_market_price_is_missing() {
        let mut runtime = runtime_state();
        runtime.last_price = 0.0;
        runtime.mark_price = 0.0;

        let strategy = reconcile(&runtime, &risk_state(), &previous_state());

        assert_eq!(strategy.status, StrategyStatus::WaitingMarketPrice);
        assert!(
            strategy
                .status_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("market price"))
        );
        assert!(strategy.levels.is_empty());
    }

    #[test]
    fn reconcile_waits_when_price_is_out_of_range_and_flat() {
        let mut runtime = runtime_state();
        runtime.last_price = 112.0;
        runtime.mark_price = 112.0;

        let strategy = reconcile(&runtime, &risk_state(), &previous_state());

        assert_eq!(strategy.status, StrategyStatus::WaitingRangeEntry);
        assert!(
            strategy
                .status_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("112.00"))
        );
    }

    #[test]
    fn reconcile_keeps_occupied_when_price_is_out_of_range_but_inventory_exists() {
        let mut runtime = runtime_state();
        runtime.last_price = 112.0;
        runtime.mark_price = 112.0;
        runtime.position_qty = 5.0;
        runtime.position_avg_price = 98.0;

        let strategy = reconcile(&runtime, &risk_state(), &previous_state());

        assert_eq!(strategy.status, StrategyStatus::Occupied);
        assert_eq!(
            strategy
                .levels
                .iter()
                .filter(|level| level.side == GridSide::Buy)
                .filter(|level| level.state == GridLevelState::Occupied)
                .count(),
            1
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
        assert!(
            strategy
                .status_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("outside configured range"))
        );
    }

    fn config() -> GridConfig {
        GridConfig {
            lower_price: 90.0,
            upper_price: 110.0,
            grid_levels: 6,
            max_position_notional: 3000.0,
            exchange_rules: None,
        }
    }
}
