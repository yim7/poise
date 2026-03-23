use std::{collections::HashMap, path::PathBuf, sync::Arc};

use anyhow::{Result, bail};
use chrono::{SecondsFormat, Utc};

use crate::{
    integrations::binance::{
        BinanceTransport, PositionMode, PositionSnapshot, PositionSnapshotState,
    },
    protocol::{
        DEFAULT_INSTANCE_ID, ExchangeOrderRules, OpenOrder, OpenOrdersSource, RuntimeState,
        SystemEvent,
    },
    storage::PersistedRuntime,
};

const POSITION_QTY_TOLERANCE: f64 = 1e-9;
const POSITION_PRICE_TOLERANCE: f64 = 1e-6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeMode {
    Paper,
    Testnet,
    Mainnet,
}

impl RuntimeMode {
    fn from_binance_settings(enabled: bool, env_name: &str) -> Self {
        if !enabled {
            return Self::Paper;
        }

        if env_name.eq_ignore_ascii_case("mainnet") {
            Self::Mainnet
        } else {
            Self::Testnet
        }
    }

    fn db_directory(self) -> &'static str {
        match self {
            Self::Paper => "paper",
            Self::Testnet => "testnet",
            Self::Mainnet => "mainnet",
        }
    }

    pub fn is_mainnet(self) -> bool {
        matches!(self, Self::Mainnet)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupConfig {
    pub runtime_mode: RuntimeMode,
    pub instance_id: String,
    pub db_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StartupExchangeState {
    pub position: PositionSnapshotState,
    pub open_orders: Option<Vec<OpenOrder>>,
    pub order_rules: Option<ExchangeOrderRules>,
    pub position_mode: PositionMode,
}

impl StartupExchangeState {
    pub fn position_snapshot(&self) -> Option<&PositionSnapshot> {
        self.position.as_ref()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum StartupDecision {
    Continue,
    Pause { code: &'static str, message: String },
    Refuse { code: &'static str, message: String },
}

#[derive(Debug, Clone, PartialEq)]
pub struct StartupReport {
    pub exchange: StartupExchangeState,
    pub decision: StartupDecision,
}

impl StartupConfig {
    pub fn from_env() -> Result<Self> {
        Self::from_pairs(std::env::vars())
    }

    pub fn from_pairs<I, K, V>(pairs: I) -> Result<Self>
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let env: HashMap<String, String> = pairs
            .into_iter()
            .map(|(key, value)| (key.as_ref().to_string(), value.as_ref().to_string()))
            .collect();

        let enabled = parse_bool(env.get("GRID_PLATFORM_BINANCE_ENABLED"));
        let env_name = env
            .get("GRID_PLATFORM_BINANCE_ENV")
            .map(String::as_str)
            .unwrap_or("testnet");
        let runtime_mode = RuntimeMode::from_binance_settings(enabled, env_name);

        if matches!(runtime_mode, RuntimeMode::Mainnet)
            && !is_explicit_mainnet_opt_in(env.get("GRID_PLATFORM_ALLOW_MAINNET"))
        {
            bail!("GRID_PLATFORM_ALLOW_MAINNET=1 is required to enable mainnet");
        }

        let instance_id = env
            .get("GRID_PLATFORM_INSTANCE_ID")
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
            .unwrap_or(DEFAULT_INSTANCE_ID)
            .to_string();
        let db_path = PathBuf::from(".data")
            .join(runtime_mode.db_directory())
            .join(format!("{instance_id}.db"));

        Ok(Self {
            runtime_mode,
            instance_id,
            db_path,
        })
    }
}

impl StartupReport {
    pub async fn collect(
        runtime_mode: RuntimeMode,
        symbol: &str,
        persisted: &PersistedRuntime,
        transport: Arc<dyn BinanceTransport>,
    ) -> Result<Self> {
        let mut exchange = collect_startup_signed_exchange_state(symbol, transport.clone()).await?;
        if !runtime_mode.is_mainnet()
            || (exchange.position.is_available() && exchange.open_orders.is_some())
        {
            exchange.order_rules = transport.fetch_exchange_info(symbol).await?.order_rules;
        }
        let decision = reconcile_startup(runtime_mode, persisted, &exchange)?;
        Ok(Self { exchange, decision })
    }

    pub fn apply_to(&self, mut runtime: PersistedRuntime) -> PersistedRuntime {
        match &self.exchange.position {
            PositionSnapshotState::Unavailable => {}
            PositionSnapshotState::Flat => {
                runtime.snapshot.runtime.position_qty = 0.0;
                runtime.snapshot.runtime.position_avg_price = 0.0;
                runtime.snapshot.runtime.unrealized_pnl = 0.0;
                runtime.snapshot.runtime.realized_pnl = 0.0;
            }
            PositionSnapshotState::Position(position) => {
                runtime.snapshot.runtime.position_qty = position.qty;
                runtime.snapshot.runtime.position_avg_price = position.avg_price;
                runtime.snapshot.runtime.unrealized_pnl = position.unrealized_pnl;
                runtime.snapshot.runtime.realized_pnl = position.realized_pnl;
            }
        }

        match &self.exchange.open_orders {
            Some(open_orders) => {
                runtime.snapshot.execution.open_orders = open_orders
                    .iter()
                    .filter(|order| is_strategy_managed_order(order))
                    .cloned()
                    .collect();
                runtime.snapshot.execution.exchange_open_orders = open_orders.clone();
                runtime.snapshot.execution.exchange_open_orders_source =
                    OpenOrdersSource::ExchangeLive;
            }
            None => {
                runtime.snapshot.execution.exchange_open_orders.clear();
                runtime.snapshot.execution.exchange_open_orders_source =
                    OpenOrdersSource::Unavailable;
            }
        }

        runtime.snapshot.strategy.config.exchange_rules = self.exchange.order_rules.clone();

        match &self.decision {
            StartupDecision::Continue => {}
            StartupDecision::Pause { code, message } => {
                runtime.snapshot.runtime.strategy_state = "paused".into();
                prepend_system_event(
                    &mut runtime.system_events,
                    SystemEvent {
                        level: "error".into(),
                        source: "startup".into(),
                        code: Some((*code).into()),
                        message: message.clone(),
                        created_at: now_utc(),
                    },
                );
            }
            StartupDecision::Refuse { code, message } => {
                runtime.snapshot.runtime.strategy_state = "paused".into();
                prepend_system_event(
                    &mut runtime.system_events,
                    SystemEvent {
                        level: "error".into(),
                        source: "startup".into(),
                        code: Some((*code).into()),
                        message: message.clone(),
                        created_at: now_utc(),
                    },
                );
            }
        }

        runtime
    }
}

pub async fn collect_startup_exchange_state(
    symbol: &str,
    transport: Arc<dyn BinanceTransport>,
) -> Result<StartupExchangeState> {
    let mut exchange = collect_startup_signed_exchange_state(symbol, transport.clone()).await?;
    exchange.order_rules = transport.fetch_exchange_info(symbol).await?.order_rules;
    Ok(exchange)
}

async fn collect_startup_signed_exchange_state(
    symbol: &str,
    transport: Arc<dyn BinanceTransport>,
) -> Result<StartupExchangeState> {
    Ok(StartupExchangeState {
        position_mode: transport.fetch_position_mode().await?,
        position: transport.fetch_position_snapshot(symbol).await?,
        open_orders: transport.fetch_open_orders(symbol).await?,
        order_rules: None,
    })
}

pub fn reconcile_startup(
    runtime_mode: RuntimeMode,
    persisted: &PersistedRuntime,
    exchange: &StartupExchangeState,
) -> Result<StartupDecision> {
    if runtime_mode.is_mainnet()
        && (!exchange.position.is_available() || exchange.open_orders.is_none())
    {
        return Ok(StartupDecision::Refuse {
            code: "STARTUP_MAINNET_SIGNED_STATE_UNAVAILABLE",
            message: "mainnet startup requires signed position and open-order snapshots".into(),
        });
    }

    if exchange.position_mode == PositionMode::Hedge {
        return Ok(StartupDecision::Pause {
            code: "STARTUP_BINANCE_HEDGE_MODE_UNSUPPORTED",
            message: "binance hedge mode is enabled; switch account to one-way mode".into(),
        });
    }

    if startup_positions_mismatch(&persisted.snapshot.runtime, &exchange.position) {
        return Ok(StartupDecision::Pause {
            code: "STARTUP_RECONCILE_POSITION_MISMATCH",
            message: "exchange position differs from persisted runtime state".into(),
        });
    }

    if exchange.open_orders.as_deref().is_some_and(|open_orders| {
        startup_open_orders_mismatch(
            &persisted.snapshot.execution.exchange_open_orders,
            open_orders,
        )
    }) {
        return Ok(StartupDecision::Pause {
            code: "STARTUP_RECONCILE_OPEN_ORDERS_MISMATCH",
            message: "exchange open orders differ from persisted exchange state".into(),
        });
    }

    Ok(StartupDecision::Continue)
}

fn startup_positions_mismatch(runtime: &RuntimeState, position: &PositionSnapshotState) -> bool {
    let persisted_has_position = runtime.position_qty.abs() > POSITION_QTY_TOLERANCE;

    match position {
        PositionSnapshotState::Unavailable => false,
        PositionSnapshotState::Flat => persisted_has_position,
        PositionSnapshotState::Position(position) => {
            if !persisted_has_position {
                return true;
            }

            (runtime.position_qty - position.qty).abs() > POSITION_QTY_TOLERANCE
                || (runtime.position_avg_price - position.avg_price).abs()
                    > POSITION_PRICE_TOLERANCE
        }
    }
}

fn startup_open_orders_mismatch(persisted: &[OpenOrder], exchange: &[OpenOrder]) -> bool {
    normalize_open_orders(persisted) != normalize_open_orders(exchange)
}

fn is_strategy_managed_order(order: &OpenOrder) -> bool {
    order.client_order_id.starts_with("grid_") || order.client_order_id.starts_with("reduce_only_")
}

fn normalize_open_orders(orders: &[OpenOrder]) -> Vec<String> {
    let mut normalized = orders
        .iter()
        .map(|order| {
            format!(
                "{}|{}|{}|{:.10}|{:.10}|{:.10}|{}",
                order.order_id,
                order.client_order_id,
                order.side.to_ascii_uppercase(),
                order.price,
                order.qty,
                order.filled_qty,
                order.status.to_ascii_uppercase(),
            )
        })
        .collect::<Vec<_>>();
    normalized.sort_unstable();
    normalized
}

fn parse_bool(value: Option<&String>) -> bool {
    value
        .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(false)
}

fn is_explicit_mainnet_opt_in(value: Option<&String>) -> bool {
    value.is_some_and(|value| value == "1")
}

fn prepend_system_event(events: &mut Vec<SystemEvent>, event: SystemEvent) {
    events.insert(0, event);
    while events.len() > 50 {
        events.pop();
    }
}

fn now_utc() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{RuntimeMode, StartupConfig};

    #[test]
    fn infers_mainnet_mode_and_default_db_path() {
        let config = StartupConfig::from_pairs([
            ("GRID_PLATFORM_BINANCE_ENABLED", "1"),
            ("GRID_PLATFORM_BINANCE_ENV", "mainnet"),
            ("GRID_PLATFORM_ALLOW_MAINNET", "1"),
        ])
        .expect("startup config");

        assert_eq!(config.runtime_mode, RuntimeMode::Mainnet);
        assert_eq!(config.instance_id, "local");
        assert_eq!(config.db_path, PathBuf::from(".data/mainnet/local.db"));
    }

    #[test]
    fn rejects_mainnet_without_explicit_allow_flag() {
        let error = StartupConfig::from_pairs([
            ("GRID_PLATFORM_BINANCE_ENABLED", "1"),
            ("GRID_PLATFORM_BINANCE_ENV", "mainnet"),
        ])
        .expect_err("mainnet must require explicit allow flag");

        assert!(error.to_string().contains("GRID_PLATFORM_ALLOW_MAINNET=1"));
    }

    #[test]
    fn rejects_mainnet_when_allow_flag_is_true_instead_of_one() {
        let error = StartupConfig::from_pairs([
            ("GRID_PLATFORM_BINANCE_ENABLED", "1"),
            ("GRID_PLATFORM_BINANCE_ENV", "mainnet"),
            ("GRID_PLATFORM_ALLOW_MAINNET", "true"),
        ])
        .expect_err("mainnet should only allow explicit value 1");

        assert!(error.to_string().contains("GRID_PLATFORM_ALLOW_MAINNET=1"));
    }

    #[test]
    fn rejects_mainnet_when_allow_flag_is_yes_instead_of_one() {
        let error = StartupConfig::from_pairs([
            ("GRID_PLATFORM_BINANCE_ENABLED", "1"),
            ("GRID_PLATFORM_BINANCE_ENV", "mainnet"),
            ("GRID_PLATFORM_ALLOW_MAINNET", "yes"),
        ])
        .expect_err("mainnet should only allow explicit value 1");

        assert!(error.to_string().contains("GRID_PLATFORM_ALLOW_MAINNET=1"));
    }
}
