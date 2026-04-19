use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{AppHandle, Manager};
use tauri_plugin_clipboard_manager::ClipboardExt;

use crate::{
    binance_quote::{BinanceQuoteClient, BinanceQuotePayload},
    config_document::{EditableTrackFields, TrackDraft, TrackLoadIssue, load_track_document},
    config_projection,
    error::{CommandError, CommandErrorKind},
    session_store::SessionStore,
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LoadedConfigFilePayload {
    pub config_path: String,
    pub projected_tracks: Vec<TrackDraftPayload>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TrackDraftPayload {
    pub draft_id: String,
    pub fields: EditableTrackFieldsPayload,
    pub load_issues: Vec<TrackLoadIssue>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EditableTrackFieldsPayload {
    pub track_id: String,
    pub symbol: String,
    pub lower_price: f64,
    pub upper_price: f64,
    pub long_exposure_units: f64,
    pub short_exposure_units: f64,
    pub notional_per_unit: f64,
    pub max_notional: f64,
    pub min_rebalance_units: f64,
    pub leverage: u32,
    pub out_of_band_policy: String,
    pub daily_loss_limit: f64,
    pub total_loss_limit: f64,
    pub shape_family: String,
}

pub(crate) fn load_config_file_from_path(
    config_path: impl AsRef<Path>,
) -> Result<LoadedConfigFilePayload, CommandError> {
    let config_path = config_path.as_ref();
    let document = load_track_document(config_path).map_err(|error| {
        CommandError::new(
            CommandErrorKind::Config,
            format!("加载配置文件失败 `{}`: {error}", config_path.display()),
        )
    })?;

    Ok(LoadedConfigFilePayload {
        config_path: config_path.to_string_lossy().into_owned(),
        projected_tracks: document
            .drafts()
            .iter()
            .map(TrackDraftPayload::from)
            .collect(),
    })
}

pub(crate) async fn fetch_binance_quote_with_client(
    client: &BinanceQuoteClient,
    symbol: &str,
) -> BinanceQuotePayload {
    client.fetch_quote(symbol).await
}

pub(crate) fn load_saved_draft_from_store(
    session_root: impl AsRef<Path>,
    config_path: impl AsRef<Path>,
) -> Result<Option<Value>, CommandError> {
    SessionStore::new(session_root).load_json(config_path)
}

pub(crate) fn save_draft_to_store(
    session_root: impl AsRef<Path>,
    config_path: impl AsRef<Path>,
    draft_snapshot: &Value,
) -> Result<(), CommandError> {
    SessionStore::new(session_root).save_json(config_path, draft_snapshot)
}

pub(crate) fn export_current_track_text(draft: TrackDraftPayload) -> Result<String, CommandError> {
    Ok(config_projection::export_current_track(
        &TrackDraft::try_from(draft)?,
    ))
}

pub(crate) fn export_all_tracks_text(
    drafts: Vec<TrackDraftPayload>,
) -> Result<String, CommandError> {
    let drafts = drafts
        .into_iter()
        .map(TrackDraft::try_from)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(config_projection::export_all_tracks(&drafts))
}

#[tauri::command]
pub fn load_config_file(config_path: String) -> Result<LoadedConfigFilePayload, CommandError> {
    load_config_file_from_path(PathBuf::from(config_path))
}

#[tauri::command]
pub fn load_saved_draft(
    app: AppHandle,
    config_path: String,
) -> Result<Option<Value>, CommandError> {
    let session_root = session_root_dir(&app)?;
    load_saved_draft_from_store(session_root, PathBuf::from(config_path))
}

#[tauri::command]
pub fn save_draft(
    app: AppHandle,
    config_path: String,
    draft_snapshot: Value,
) -> Result<(), CommandError> {
    let session_root = session_root_dir(&app)?;
    save_draft_to_store(session_root, PathBuf::from(config_path), &draft_snapshot)
}

#[tauri::command]
pub fn copy_text(app: AppHandle, text: String) -> Result<(), CommandError> {
    app.clipboard().write_text(text).map_err(|error| {
        CommandError::new(
            CommandErrorKind::Clipboard,
            format!("写入剪贴板失败: {error}"),
        )
    })
}

#[tauri::command]
pub async fn fetch_binance_quote(symbol: String) -> BinanceQuotePayload {
    fetch_binance_quote_with_client(&BinanceQuoteClient::default(), &symbol).await
}

#[tauri::command]
pub fn export_current_track(draft: TrackDraftPayload) -> Result<String, CommandError> {
    export_current_track_text(draft)
}

#[tauri::command]
pub fn export_all_tracks(drafts: Vec<TrackDraftPayload>) -> Result<String, CommandError> {
    export_all_tracks_text(drafts)
}

fn session_root_dir(app: &AppHandle) -> Result<PathBuf, CommandError> {
    let config_dir = app.path().app_config_dir().map_err(|error| {
        CommandError::new(
            CommandErrorKind::Internal,
            format!("获取应用配置目录失败: {error}"),
        )
    })?;
    Ok(config_dir.join("sessions"))
}

impl From<&TrackDraft> for TrackDraftPayload {
    fn from(value: &TrackDraft) -> Self {
        Self {
            draft_id: value.draft_id.clone(),
            fields: EditableTrackFieldsPayload::from(&value.fields),
            load_issues: value.load_issues.clone(),
        }
    }
}

impl From<&EditableTrackFields> for EditableTrackFieldsPayload {
    fn from(value: &EditableTrackFields) -> Self {
        Self {
            track_id: value.track_id.clone(),
            symbol: value.symbol.clone(),
            lower_price: value.lower_price,
            upper_price: value.upper_price,
            long_exposure_units: value.long_exposure_units,
            short_exposure_units: value.short_exposure_units,
            notional_per_unit: value.notional_per_unit,
            max_notional: value.max_notional,
            min_rebalance_units: value.min_rebalance_units,
            leverage: value.leverage,
            out_of_band_policy: value.out_of_band_policy.as_str().to_string(),
            daily_loss_limit: value.daily_loss_limit,
            total_loss_limit: value.total_loss_limit,
            shape_family: value.shape_family.as_str().to_string(),
        }
    }
}

impl TryFrom<TrackDraftPayload> for TrackDraft {
    type Error = CommandError;

    fn try_from(value: TrackDraftPayload) -> Result<Self, Self::Error> {
        Ok(Self {
            draft_id: value.draft_id,
            fields: EditableTrackFields::try_from(value.fields)?,
            load_issues: value.load_issues,
        })
    }
}

impl TryFrom<EditableTrackFieldsPayload> for EditableTrackFields {
    type Error = CommandError;

    fn try_from(value: EditableTrackFieldsPayload) -> Result<Self, Self::Error> {
        Ok(Self {
            track_id: validate_non_empty(value.track_id, "track_id")?,
            symbol: validate_non_empty(value.symbol, "symbol")?,
            lower_price: value.lower_price,
            upper_price: value.upper_price,
            long_exposure_units: value.long_exposure_units,
            short_exposure_units: value.short_exposure_units,
            notional_per_unit: value.notional_per_unit,
            max_notional: value.max_notional,
            min_rebalance_units: value.min_rebalance_units,
            leverage: value.leverage,
            out_of_band_policy: match value.out_of_band_policy.as_str() {
                "freeze" => crate::config_document::TrackOutOfBandPolicy::Freeze,
                "hold" => crate::config_document::TrackOutOfBandPolicy::Hold,
                "flatten" => crate::config_document::TrackOutOfBandPolicy::Flatten,
                "terminate" => crate::config_document::TrackOutOfBandPolicy::Terminate,
                other => {
                    return Err(CommandError::new(
                        CommandErrorKind::Config,
                        format!("不支持的 out_of_band_policy: `{other}`"),
                    ));
                }
            },
            daily_loss_limit: value.daily_loss_limit,
            total_loss_limit: value.total_loss_limit,
            shape_family: match value.shape_family.as_str() {
                "linear" => crate::config_document::TrackShapeFamily::Linear,
                "inertial" => crate::config_document::TrackShapeFamily::Inertial,
                "responsive" => crate::config_document::TrackShapeFamily::Responsive,
                other => {
                    return Err(CommandError::new(
                        CommandErrorKind::Config,
                        format!("不支持的 shape_family: `{other}`"),
                    ));
                }
            },
        })
    }
}

fn validate_non_empty(value: String, field: &str) -> Result<String, CommandError> {
    if value.trim().is_empty() {
        return Err(CommandError::new(
            CommandErrorKind::Config,
            format!("{field} 不能为空"),
        ));
    }

    Ok(value)
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        path::Path,
        sync::mpsc,
        thread,
        time::Duration,
    };

    use crate::binance_quote::QuoteErrorKind;
    use serde_json::json;
    use tempfile::tempdir;

    use super::{
        BinanceQuoteClient, EditableTrackFieldsPayload, export_all_tracks_text,
        export_current_track_text, fetch_binance_quote_with_client, load_config_file_from_path,
        load_saved_draft_from_store, save_draft_to_store,
    };

    #[test]
    fn load_config_file_returns_projected_tracks() {
        let temp_dir = tempdir().unwrap();
        let config_path = temp_dir.path().join("grid.toml");
        std::fs::write(
            &config_path,
            r#"
[[tracks]]
track_id = "btc-core"
symbol = "BTCUSDT"
lower_price = 65000.0
upper_price = 68000.0
long_exposure_units = 8.0
short_exposure_units = 8.0
notional_per_unit = 250.0
daily_loss_limit = 100.0
total_loss_limit = 200.0
"#,
        )
        .unwrap();

        let payload = load_config_file_from_path(&config_path).unwrap();

        assert_eq!(payload.config_path, config_path.to_string_lossy());
        assert_eq!(payload.projected_tracks.len(), 1);
        assert_eq!(payload.projected_tracks[0].fields.track_id, "btc-core");
        assert_eq!(payload.projected_tracks[0].fields.symbol, "BTCUSDT");
    }

    #[test]
    fn load_config_file_keeps_invalid_track_in_the_list() {
        let temp_dir = tempdir().unwrap();
        let config_path = temp_dir.path().join("grid.toml");
        std::fs::write(
            &config_path,
            r#"
[[tracks]]
track_id = "good"
symbol = "BTCUSDT"
lower_price = 65000.0
upper_price = 68000.0
long_exposure_units = 8.0
short_exposure_units = 8.0
notional_per_unit = 250.0
daily_loss_limit = 100.0
total_loss_limit = 200.0

[[tracks]]
track_id = "broken"
symbol = "ETHUSDT"
upper_price = 4000.0
long_exposure_units = 5.0
short_exposure_units = 5.0
notional_per_unit = 100.0
daily_loss_limit = 80.0
total_loss_limit = 160.0
"#,
        )
        .unwrap();

        let payload = load_config_file_from_path(&config_path).unwrap();

        assert_eq!(payload.projected_tracks.len(), 2);
        let broken = payload
            .projected_tracks
            .iter()
            .find(|draft| draft.fields.track_id == "broken")
            .unwrap();
        assert!(!broken.load_issues.is_empty());
        assert_eq!(broken.load_issues[0].field_key, "lower_price");
    }

    #[test]
    fn fetch_binance_quote_always_hits_futures_endpoint() {
        let (server_url, requests) = spawn_http_server(
            "HTTP/1.1 200 OK",
            "{\"symbol\":\"BTCUSDT\",\"price\":\"65000.10\"}",
        );
        let client = BinanceQuoteClient::for_base_url(server_url);

        let quote =
            tauri::async_runtime::block_on(fetch_binance_quote_with_client(&client, "btcusdt"));

        assert_eq!(quote.price.as_deref(), Some("65000.10"));
        assert_eq!(quote.error_kind, None);
        assert_eq!(
            requests.recv().unwrap(),
            "GET /fapi/v1/ticker/price?symbol=BTCUSDT HTTP/1.1"
        );
    }

    #[test]
    fn unsupported_symbol_returns_displayable_error_instead_of_panicking() {
        let (server_url, _) = spawn_http_server(
            "HTTP/1.1 400 BAD REQUEST",
            "{\"code\":-1121,\"msg\":\"Invalid symbol.\"}",
        );
        let client = BinanceQuoteClient::for_base_url(server_url);

        let quote =
            tauri::async_runtime::block_on(fetch_binance_quote_with_client(&client, "bad-symbol"));

        assert_eq!(quote.price, None);
        assert_eq!(quote.error_kind, Some(QuoteErrorKind::UnsupportedSymbol));
        assert!(quote.error_message.unwrap().contains("bad-symbol"));
    }

    #[test]
    fn draft_sessions_are_isolated_by_config_path() {
        let temp_dir = tempdir().unwrap();
        let session_root = temp_dir.path().join("sessions");
        let strategy_a = absolute_file(temp_dir.path(), "configs/a.toml");
        let strategy_b = absolute_file(temp_dir.path(), "configs/b.toml");
        std::fs::create_dir_all(strategy_a.parent().unwrap()).unwrap();
        std::fs::write(&strategy_a, "").unwrap();
        std::fs::write(&strategy_b, "").unwrap();

        let snapshot_a = json!({
            "selected_draft_id": "draft-a",
            "projected_tracks_toml": "[[tracks]]\ntrack_id = \"alpha\""
        });
        let snapshot_b = json!({
            "selected_draft_id": "draft-b",
            "projected_tracks_toml": "[[tracks]]\ntrack_id = \"beta\""
        });

        save_draft_to_store(&session_root, &strategy_a, &snapshot_a).unwrap();
        save_draft_to_store(&session_root, &strategy_b, &snapshot_b).unwrap();

        let restored_a = load_saved_draft_from_store(&session_root, &strategy_a).unwrap();
        let restored_b = load_saved_draft_from_store(&session_root, &strategy_b).unwrap();

        assert_eq!(restored_a, Some(snapshot_a));
        assert_eq!(restored_b, Some(snapshot_b));
        assert_ne!(restored_a, restored_b);
    }

    #[test]
    fn saving_draft_twice_to_same_path_overwrites_previous_snapshot() {
        let temp_dir = tempdir().unwrap();
        let session_root = temp_dir.path().join("sessions");
        let config_path = absolute_file(temp_dir.path(), "configs/a.toml");
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(&config_path, "").unwrap();

        let first_snapshot = json!({
            "selected_draft_id": "draft-a",
            "projected_tracks_toml": "[[tracks]]\ntrack_id = \"alpha\""
        });
        let second_snapshot = json!({
            "selected_draft_id": "draft-b",
            "projected_tracks_toml": "[[tracks]]\ntrack_id = \"beta\""
        });

        save_draft_to_store(&session_root, &config_path, &first_snapshot).unwrap();
        save_draft_to_store(&session_root, &config_path, &second_snapshot).unwrap();

        let restored = load_saved_draft_from_store(&session_root, &config_path).unwrap();

        assert_eq!(restored, Some(second_snapshot));
    }

    #[test]
    fn export_current_track_only_returns_tracks_table() {
        let text = export_current_track_text(sample_track_payload("btc-core")).unwrap();

        assert!(text.starts_with("[[tracks]]"));
        assert!(text.contains("track_id = \"btc-core\""));
        assert!(!text.contains("exchange"));
    }

    #[test]
    fn export_all_tracks_keeps_input_order() {
        let text = export_all_tracks_text(vec![
            sample_track_payload("first"),
            sample_track_payload("second"),
        ])
        .unwrap();

        assert!(text.contains("[[tracks]]\ntrack_id = \"first\""));
        assert!(text.contains("[[tracks]]\ntrack_id = \"second\""));
        assert!(
            text.find("track_id = \"first\"").unwrap()
                < text.find("track_id = \"second\"").unwrap()
        );
    }

    #[test]
    fn export_current_track_text_only_contains_the_selected_track() {
        let track = super::TrackDraftPayload {
            draft_id: "alpha-draft".to_string(),
            fields: EditableTrackFieldsPayload {
                track_id: "alpha".to_string(),
                symbol: "BTCUSDT".to_string(),
                lower_price: 100.0,
                upper_price: 120.0,
                long_exposure_units: 8.0,
                short_exposure_units: 8.0,
                notional_per_unit: 200.0,
                max_notional: 1600.0,
                min_rebalance_units: 0.5,
                leverage: 10,
                out_of_band_policy: "freeze".to_string(),
                daily_loss_limit: 100.0,
                total_loss_limit: 200.0,
                shape_family: "linear".to_string(),
            },
            load_issues: Vec::new(),
        };

        let exported = super::export_current_track_text(track).unwrap();

        assert!(exported.contains("[[tracks]]"));
        assert!(exported.contains("track_id = \"alpha\""));
        assert_eq!(exported.matches("[[tracks]]").count(), 1);
    }

    #[test]
    fn export_all_tracks_text_keeps_each_track_block() {
        let alpha = super::TrackDraftPayload {
            draft_id: "alpha-draft".to_string(),
            fields: EditableTrackFieldsPayload {
                track_id: "alpha".to_string(),
                symbol: "BTCUSDT".to_string(),
                lower_price: 100.0,
                upper_price: 120.0,
                long_exposure_units: 8.0,
                short_exposure_units: 8.0,
                notional_per_unit: 200.0,
                max_notional: 1600.0,
                min_rebalance_units: 0.5,
                leverage: 10,
                out_of_band_policy: "freeze".to_string(),
                daily_loss_limit: 100.0,
                total_loss_limit: 200.0,
                shape_family: "linear".to_string(),
            },
            load_issues: Vec::new(),
        };
        let beta = super::TrackDraftPayload {
            draft_id: "beta-draft".to_string(),
            fields: EditableTrackFieldsPayload {
                track_id: "beta".to_string(),
                symbol: "ETHUSDT".to_string(),
                lower_price: 200.0,
                upper_price: 240.0,
                long_exposure_units: 5.0,
                short_exposure_units: 5.0,
                notional_per_unit: 100.0,
                max_notional: 500.0,
                min_rebalance_units: 1.0,
                leverage: 5,
                out_of_band_policy: "hold".to_string(),
                daily_loss_limit: 80.0,
                total_loss_limit: 160.0,
                shape_family: "responsive".to_string(),
            },
            load_issues: Vec::new(),
        };

        let exported = super::export_all_tracks_text(vec![alpha, beta]).unwrap();

        assert_eq!(exported.matches("[[tracks]]").count(), 2);
        assert!(exported.contains("track_id = \"alpha\""));
        assert!(exported.contains("track_id = \"beta\""));
    }

    #[test]
    fn quote_timeout_returns_stable_timeout_error() {
        let (server_url, _) = spawn_http_server_with_delay(
            "HTTP/1.1 200 OK",
            "{\"symbol\":\"BTCUSDT\",\"price\":\"65000.10\"}",
            Duration::from_millis(200),
        );
        let client =
            BinanceQuoteClient::for_base_url_and_timeout(server_url, Duration::from_millis(50));

        let quote =
            tauri::async_runtime::block_on(fetch_binance_quote_with_client(&client, "BTCUSDT"));

        assert_eq!(quote.price, None);
        assert_eq!(quote.error_kind, Some(QuoteErrorKind::TimedOut));
        assert!(quote.error_message.unwrap().contains("超时"));
    }

    #[test]
    fn rate_limited_response_returns_distinct_error_kind() {
        let (server_url, _) = spawn_http_server(
            "HTTP/1.1 429 TOO MANY REQUESTS",
            "{\"code\":-1003,\"msg\":\"Too many requests; please use websocket for live updates.\"}",
        );
        let client = BinanceQuoteClient::for_base_url(server_url);

        let quote =
            tauri::async_runtime::block_on(fetch_binance_quote_with_client(&client, "BTCUSDT"));

        assert_eq!(quote.price, None);
        assert_eq!(quote.error_kind, Some(QuoteErrorKind::RateLimited));
        assert!(quote.error_message.unwrap().contains("Too many requests"));
    }

    #[test]
    fn teapot_response_is_also_classified_as_rate_limited() {
        let (server_url, _) = spawn_http_server(
            "HTTP/1.1 418 I AM A TEAPOT",
            "{\"code\":-1003,\"msg\":\"Way too much request weight used; IP banned until 1234567890.\"}",
        );
        let client = BinanceQuoteClient::for_base_url(server_url);

        let quote =
            tauri::async_runtime::block_on(fetch_binance_quote_with_client(&client, "BTCUSDT"));

        assert_eq!(quote.price, None);
        assert_eq!(quote.error_kind, Some(QuoteErrorKind::RateLimited));
    }

    #[test]
    fn temporarily_unavailable_response_returns_distinct_error_kind() {
        let (server_url, _) = spawn_http_server(
            "HTTP/1.1 503 SERVICE UNAVAILABLE",
            "{\"code\":-1001,\"msg\":\"Service unavailable from a restricted location according to 'b. Eligibility' in https://www.binance.com/en/terms. Please contact customer service if you believe you received this message in error.\"}",
        );
        let client = BinanceQuoteClient::for_base_url(server_url);

        let quote =
            tauri::async_runtime::block_on(fetch_binance_quote_with_client(&client, "BTCUSDT"));

        assert_eq!(quote.price, None);
        assert_eq!(
            quote.error_kind,
            Some(QuoteErrorKind::TemporarilyUnavailable)
        );
    }

    #[test]
    fn generic_upstream_error_keeps_binance_message() {
        let (server_url, _) = spawn_http_server(
            "HTTP/1.1 500 INTERNAL SERVER ERROR",
            "{\"code\":-1000,\"msg\":\"An unknown error occurred while processing the request.\"}",
        );
        let client = BinanceQuoteClient::for_base_url(server_url);

        let quote =
            tauri::async_runtime::block_on(fetch_binance_quote_with_client(&client, "BTCUSDT"));

        assert_eq!(quote.price, None);
        assert_eq!(quote.error_kind, Some(QuoteErrorKind::Upstream));
        assert!(
            quote
                .error_message
                .unwrap()
                .contains("An unknown error occurred")
        );
    }

    fn absolute_file(root: &Path, relative: &str) -> std::path::PathBuf {
        root.join(relative)
    }

    fn spawn_http_server(
        status_line: &'static str,
        body: &'static str,
    ) -> (String, mpsc::Receiver<String>) {
        spawn_http_server_with_delay(status_line, body, Duration::ZERO)
    }

    fn spawn_http_server_with_delay(
        status_line: &'static str,
        body: &'static str,
        response_delay: Duration,
    ) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::channel();

        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0_u8; 4096];
            let read = stream.read(&mut buffer).unwrap();
            let request = String::from_utf8_lossy(&buffer[..read]);
            let first_line = request.lines().next().unwrap().to_string();
            let _ = tx.send(first_line);
            thread::sleep(response_delay);
            let response = format!(
                "{status_line}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.flush().unwrap();
        });

        (format!("http://{address}"), rx)
    }

    fn sample_track_payload(track_id: &str) -> super::TrackDraftPayload {
        super::TrackDraftPayload {
            draft_id: format!("{track_id}-draft"),
            fields: EditableTrackFieldsPayload {
                track_id: track_id.to_string(),
                symbol: "BTCUSDT".to_string(),
                lower_price: 90.0,
                upper_price: 110.0,
                long_exposure_units: 8.0,
                short_exposure_units: 8.0,
                notional_per_unit: 375.0,
                max_notional: 3000.0,
                min_rebalance_units: 0.5,
                leverage: 10,
                out_of_band_policy: "freeze".to_string(),
                daily_loss_limit: 120.0,
                total_loss_limit: 500.0,
                shape_family: "linear".to_string(),
            },
            load_issues: Vec::new(),
        }
    }
}
