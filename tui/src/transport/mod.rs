use anyhow::{Context, Result};
use futures_util::StreamExt;
use reqwest::Client;
use serde::de::DeserializeOwned;
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_tungstenite::connect_async;

use crate::{
    events::{AppEvent, EffectResultEvent, ProtocolEvent},
    protocol::{
        CommandAccepted, CommandRequest, CommandType, HttpErrorEnvelope, HttpSuccessEnvelope,
        InstancesDirectory, RiskEvent, RuntimeSnapshot, ServerEnvelope,
    },
};

#[derive(Debug, Clone)]
pub struct TransportClient {
    http: Client,
    base_url: String,
    ws_url: String,
}

impl TransportClient {
    pub fn new(base_url: String, ws_url: String) -> Self {
        Self {
            http: Client::new(),
            base_url: base_url.trim_end_matches('/').to_owned(),
            ws_url: normalize_ws_base(&ws_url),
        }
    }

    pub async fn fetch_instances(&self) -> Result<InstancesDirectory> {
        let response = self
            .http
            .get(format!("{}/instances", self.base_url))
            .send()
            .await
            .context("fetch instances request failed")?;
        Self::decode_response(response, "fetch instances").await
    }

    pub async fn fetch_snapshot(&self) -> Result<RuntimeSnapshot> {
        let response = self
            .http
            .get(format!("{}/runtime/snapshot", self.base_url))
            .send()
            .await
            .context("fetch snapshot request failed")?;
        Self::decode_response(response, "fetch snapshot").await
    }

    pub async fn fetch_instance_snapshot(&self, symbol: &str) -> Result<RuntimeSnapshot> {
        let response = self
            .http
            .get(self.instance_http_url(symbol, "runtime/snapshot"))
            .send()
            .await
            .context("fetch instance snapshot request failed")?;
        Self::decode_response(response, "fetch instance snapshot").await
    }

    pub async fn fetch_risk_events(&self) -> Result<Vec<RiskEvent>> {
        let response = self
            .http
            .get(format!("{}/risk/events", self.base_url))
            .send()
            .await
            .context("fetch risk events request failed")?;
        Self::decode_response(response, "fetch risk events").await
    }

    pub async fn fetch_instance_risk_events(&self, symbol: &str) -> Result<Vec<RiskEvent>> {
        let response = self
            .http
            .get(self.instance_http_url(symbol, "risk/events"))
            .send()
            .await
            .context("fetch instance risk events request failed")?;
        Self::decode_response(response, "fetch instance risk events").await
    }

    pub async fn send_command(
        &self,
        command: CommandType,
        command_id: String,
    ) -> Result<CommandAccepted> {
        let path = match command {
            CommandType::Pause => "pause",
            CommandType::Resume => "resume",
            CommandType::CancelAll => "cancel-all",
            CommandType::FlattenNow => "flatten-now",
            CommandType::ShutdownAfterFlatten => "shutdown-after-flatten",
        };

        let response = self
            .http
            .post(format!("{}/commands/{}", self.base_url, path))
            .json(&CommandRequest { command_id })
            .send()
            .await
            .context("send command request failed")?;
        Self::decode_response(response, "send command").await
    }

    pub async fn send_instance_command(
        &self,
        symbol: &str,
        command: CommandType,
        command_id: String,
    ) -> Result<CommandAccepted> {
        let path = match command {
            CommandType::Pause => "pause",
            CommandType::Resume => "resume",
            CommandType::CancelAll => "cancel-all",
            CommandType::FlattenNow => "flatten-now",
            CommandType::ShutdownAfterFlatten => "shutdown-after-flatten",
        };

        let response = self
            .http
            .post(self.instance_http_url(symbol, &format!("commands/{path}")))
            .json(&CommandRequest { command_id })
            .send()
            .await
            .context("send instance command request failed")?;
        Self::decode_response(response, "send instance command").await
    }

    pub fn spawn_ws_listener(&self, app_tx: mpsc::Sender<AppEvent>) -> JoinHandle<()> {
        let this = self.clone();
        tokio::spawn(async move {
            match connect_async(&this.ws_url).await {
                Ok((stream, _)) => {
                    let _ = app_tx
                        .send(AppEvent::EffectResult(EffectResultEvent::WsConnected {
                            symbol: String::new(),
                            generation: 0,
                        }))
                        .await;
                    let (_, mut read) = stream.split();
                    while let Some(message) = read.next().await {
                        match message {
                            Ok(msg) if msg.is_text() => {
                                match serde_json::from_str::<ServerEnvelope>(
                                    msg.to_text().unwrap_or_default(),
                                ) {
                                    Ok(envelope) => {
                                        if app_tx
                                            .send(AppEvent::Protocol(envelope.event.into()))
                                            .await
                                            .is_err()
                                        {
                                            break;
                                        }
                                    }
                                    Err(error) => {
                                        let _ = app_tx
                                            .send(AppEvent::EffectResult(
                                                EffectResultEvent::WsDisconnected {
                                                    symbol: String::new(),
                                                    generation: 0,
                                                    reason: error.to_string(),
                                                },
                                            ))
                                            .await;
                                        break;
                                    }
                                }
                            }
                            Ok(msg) if msg.is_close() => {
                                let _ = app_tx
                                    .send(AppEvent::EffectResult(
                                        EffectResultEvent::WsDisconnected {
                                            symbol: String::new(),
                                            generation: 0,
                                            reason: "server closed socket".into(),
                                        },
                                    ))
                                    .await;
                                break;
                            }
                            Ok(_) => {}
                            Err(error) => {
                                let _ = app_tx
                                    .send(AppEvent::EffectResult(
                                        EffectResultEvent::WsDisconnected {
                                            symbol: String::new(),
                                            generation: 0,
                                            reason: error.to_string(),
                                        },
                                    ))
                                    .await;
                                break;
                            }
                        }
                    }
                }
                Err(error) => {
                    let _ = app_tx
                        .send(AppEvent::EffectResult(EffectResultEvent::WsDisconnected {
                            symbol: String::new(),
                            generation: 0,
                            reason: error.to_string(),
                        }))
                        .await;
                }
            }
        })
    }

    pub fn spawn_instance_ws_listener(
        &self,
        symbol: String,
        generation: u64,
        app_tx: mpsc::Sender<AppEvent>,
    ) -> JoinHandle<()> {
        let this = self.clone();
        tokio::spawn(async move {
            let ws_url = this.instance_ws_url(&symbol);
            match connect_async(&ws_url).await {
                Ok((stream, _)) => {
                    let _ = app_tx
                        .send(AppEvent::EffectResult(EffectResultEvent::WsConnected {
                            symbol: symbol.clone(),
                            generation,
                        }))
                        .await;
                    let (_, mut read) = stream.split();
                    while let Some(message) = read.next().await {
                        match message {
                            Ok(msg) if msg.is_text() => {
                                match serde_json::from_str::<ServerEnvelope>(
                                    msg.to_text().unwrap_or_default(),
                                ) {
                                    Ok(envelope) => {
                                        if app_tx
                                            .send(AppEvent::Protocol(ProtocolEvent {
                                                symbol: Some(symbol.clone()),
                                                generation: Some(generation),
                                                event: envelope.event,
                                            }))
                                            .await
                                            .is_err()
                                        {
                                            break;
                                        }
                                    }
                                    Err(error) => {
                                        let _ = app_tx
                                            .send(AppEvent::EffectResult(
                                                EffectResultEvent::WsDisconnected {
                                                    symbol: symbol.clone(),
                                                    generation,
                                                    reason: error.to_string(),
                                                },
                                            ))
                                            .await;
                                        break;
                                    }
                                }
                            }
                            Ok(msg) if msg.is_close() => {
                                let _ = app_tx
                                    .send(AppEvent::EffectResult(
                                        EffectResultEvent::WsDisconnected {
                                            symbol: symbol.clone(),
                                            generation,
                                            reason: "server closed socket".into(),
                                        },
                                    ))
                                    .await;
                                break;
                            }
                            Ok(_) => {}
                            Err(error) => {
                                let _ = app_tx
                                    .send(AppEvent::EffectResult(
                                        EffectResultEvent::WsDisconnected {
                                            symbol: symbol.clone(),
                                            generation,
                                            reason: error.to_string(),
                                        },
                                    ))
                                    .await;
                                break;
                            }
                        }
                    }
                }
                Err(error) => {
                    let _ = app_tx
                        .send(AppEvent::EffectResult(EffectResultEvent::WsDisconnected {
                            symbol,
                            generation,
                            reason: error.to_string(),
                        }))
                        .await;
                }
            }
        })
    }

    fn instance_http_url(&self, symbol: &str, path: &str) -> String {
        format!("{}/instances/{symbol}/{path}", self.base_url)
    }

    fn instance_ws_url(&self, symbol: &str) -> String {
        format!("{}/instances/{symbol}/ws", self.ws_url)
    }

    async fn decode_response<T>(response: reqwest::Response, action: &str) -> Result<T>
    where
        T: DeserializeOwned,
    {
        let status = response.status();
        let body = response
            .text()
            .await
            .context("failed to read response body")?;
        if status.is_success() {
            let envelope: HttpSuccessEnvelope<T> =
                serde_json::from_str(&body).with_context(|| format!("{action} decode failed"))?;
            return Ok(envelope.data);
        }

        if let Ok(error) = serde_json::from_str::<HttpErrorEnvelope>(&body) {
            anyhow::bail!(
                "{} failed: {} ({})",
                action,
                error.error.message,
                error.error.code
            );
        }

        anyhow::bail!("{action} failed with status {status}: {body}");
    }
}

fn normalize_ws_base(ws_url: &str) -> String {
    let trimmed = ws_url.trim_end_matches('/');
    trimmed.strip_suffix("/ws").unwrap_or(trimmed).to_owned()
}
