use anyhow::{Result, anyhow};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use grid_core::types::{Exposure, Side};

// ── Exchange types ──

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrderRequest {
    pub symbol: String,
    pub side: Side,
    pub price: f64,
    pub quantity: f64,
    pub client_order_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrderReceipt {
    pub order_id: String,
    pub client_order_id: String,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Position {
    pub symbol: String,
    pub qty: f64,
    pub avg_price: f64,
    pub unrealized_pnl: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpenOrder {
    pub symbol: String,
    pub order_id: String,
    pub client_order_id: String,
    pub side: Side,
    pub price: f64,
    pub qty: f64,
    pub realized_pnl: f64,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PriceTick {
    pub symbol: String,
    pub last_price: f64,
    pub mark_price: f64,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExchangeInfo {
    pub symbol: String,
    pub rules: grid_core::types::ExchangeRules,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum UserDataEvent {
    OrderUpdate(OpenOrder),
    PositionUpdate(Position),
}

#[derive(Debug, Clone, PartialEq)]
pub enum UserDataStreamItem {
    Event(UserDataEvent),
    ReplayBarrier,
}

pub struct UserDataSubscription {
    receiver: mpsc::Receiver<UserDataStreamItem>,
    replay_barrier_requests: mpsc::Sender<()>,
}

impl UserDataSubscription {
    pub fn new(
        receiver: mpsc::Receiver<UserDataStreamItem>,
        replay_barrier_requests: mpsc::Sender<()>,
    ) -> Self {
        Self {
            receiver,
            replay_barrier_requests,
        }
    }

    pub fn from_receiver(mut receiver: mpsc::Receiver<UserDataEvent>, buffer: usize) -> Self {
        let (stream_sender, stream_receiver) = mpsc::channel(buffer);
        let (barrier_sender, mut barrier_receiver) = mpsc::channel(buffer);

        tokio::spawn(async move {
            let mut event_stream_open = true;
            let mut barrier_stream_open = true;

            loop {
                tokio::select! {
                    biased;
                    maybe_event = async {
                        if event_stream_open {
                            receiver.recv().await
                        } else {
                            std::future::pending::<Option<UserDataEvent>>().await
                        }
                    } => {
                        match maybe_event {
                            Some(event) => {
                                if stream_sender.send(UserDataStreamItem::Event(event)).await.is_err() {
                                    return;
                                }
                            }
                            None => {
                                event_stream_open = false;
                            }
                        }
                    }
                    maybe_barrier = async {
                        if barrier_stream_open {
                            barrier_receiver.recv().await
                        } else {
                            std::future::pending::<Option<()>>().await
                        }
                    } => {
                        match maybe_barrier {
                            Some(()) => {
                                if stream_sender.send(UserDataStreamItem::ReplayBarrier).await.is_err() {
                                    return;
                                }
                            }
                            None => {
                                barrier_stream_open = false;
                            }
                        }
                    }
                }

                if !event_stream_open && !barrier_stream_open {
                    return;
                }
            }
        });

        Self::new(stream_receiver, barrier_sender)
    }

    pub async fn request_replay_barrier(&self) -> Result<()> {
        self.replay_barrier_requests
            .send(())
            .await
            .map_err(|_| anyhow!("user data replay barrier request channel closed"))
    }

    pub fn into_receiver(self) -> mpsc::Receiver<UserDataStreamItem> {
        self.receiver
    }
}

// ── Snapshot type (for persistence) ──

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
// Internal persistence snapshot: keeps full engine state, including fields not exposed over HTTP.
pub struct InstanceSnapshot {
    pub id: String,
    pub symbol: String,
    pub config: grid_core::strategy::GridConfig,
    pub status: super::instance::InstanceStatus,
    pub current_exposure: Exposure,
    pub target_exposure: Option<Exposure>,
    pub pending_order: Option<super::instance::PendingOrder>,
    pub risk_state: super::instance::RiskState,
    pub last_price: Option<f64>,
    pub out_of_band_since: Option<DateTime<Utc>>,
}

// ── Port traits ──

#[async_trait]
pub trait ExchangePort: Send + Sync {
    async fn submit_order(&self, req: OrderRequest) -> Result<OrderReceipt>;
    async fn cancel_order(&self, symbol: &str, order_id: &str) -> Result<()>;
    async fn cancel_all(&self, symbol: &str) -> Result<()>;
    async fn get_position(&self, symbol: &str) -> Result<Position>;
    async fn get_open_orders(&self, symbol: &str) -> Result<Vec<OpenOrder>>;
    async fn get_exchange_info(&self, symbol: &str) -> Result<ExchangeInfo>;
}

#[async_trait]
pub trait MarketDataPort: Send + Sync {
    async fn subscribe_prices(&self, symbol: &str) -> Result<mpsc::Receiver<PriceTick>>;
    async fn subscribe_user_data(&self) -> Result<UserDataSubscription>;
}

#[async_trait]
pub trait PersistencePort: Send + Sync {
    async fn save_instance_state(&self, id: &str, state: &InstanceSnapshot) -> Result<()>;
    async fn load_instance_state(&self, id: &str) -> Result<Option<InstanceSnapshot>>;
}

pub trait ClockPort: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}
