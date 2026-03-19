use anyhow::{Context, Result};
use futures_util::StreamExt;
use reqwest::Client;
use serde::de::DeserializeOwned;
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_tungstenite::connect_async;

use crate::{
    events::{AppEvent, EffectResultEvent},
    protocol::{
        CommandAccepted, CommandRequest, CommandType, HttpErrorEnvelope, HttpSuccessEnvelope,
        RuntimeSnapshot, ServerEnvelope,
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
            base_url,
            ws_url,
        }
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

    pub fn spawn_ws_listener(&self, app_tx: mpsc::Sender<AppEvent>) -> JoinHandle<()> {
        let this = self.clone();
        tokio::spawn(async move {
            match connect_async(&this.ws_url).await {
                Ok((stream, _)) => {
                    let _ = app_tx
                        .send(AppEvent::EffectResult(EffectResultEvent::WsConnected))
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
                                            .send(AppEvent::Protocol(envelope.event))
                                            .await
                                            .is_err()
                                        {
                                            break;
                                        }
                                    }
                                    Err(error) => {
                                        let _ = app_tx
                                            .send(AppEvent::EffectResult(
                                                EffectResultEvent::WsDisconnected(
                                                    error.to_string(),
                                                ),
                                            ))
                                            .await;
                                        break;
                                    }
                                }
                            }
                            Ok(msg) if msg.is_close() => {
                                let _ = app_tx
                                    .send(AppEvent::EffectResult(
                                        EffectResultEvent::WsDisconnected(
                                            "server closed socket".into(),
                                        ),
                                    ))
                                    .await;
                                break;
                            }
                            Ok(_) => {}
                            Err(error) => {
                                let _ = app_tx
                                    .send(AppEvent::EffectResult(
                                        EffectResultEvent::WsDisconnected(error.to_string()),
                                    ))
                                    .await;
                                break;
                            }
                        }
                    }
                }
                Err(error) => {
                    let _ = app_tx
                        .send(AppEvent::EffectResult(EffectResultEvent::WsDisconnected(
                            error.to_string(),
                        )))
                        .await;
                }
            }
        })
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
