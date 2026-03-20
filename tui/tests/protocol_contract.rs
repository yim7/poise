use std::{collections::HashSet, fs, path::PathBuf};

use grid_platform_tui::protocol::{
    CommandAck, CommandStatus, ConnectionState, HttpSuccessEnvelope, RiskEvent, RuntimeSnapshot,
    ServerEnvelope, ServerEvent, StrategyStatus,
};

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../contracts")
}

#[test]
fn runtime_snapshot_fixture_decodes() {
    let raw = fs::read_to_string(fixtures_dir().join("runtime_snapshot.json")).unwrap();
    let snapshot: RuntimeSnapshot = serde_json::from_str(&raw).unwrap();
    let serialized = serde_json::to_value(&snapshot).unwrap();
    assert_eq!(snapshot.runtime.symbol, "XAUUSDT");
    assert_eq!(snapshot.strategy.status, StrategyStatus::Occupied);
    assert_eq!(snapshot.strategy.levels.len(), 6);
    assert_eq!(snapshot.connection.user_stream_connected, None);
    assert_eq!(snapshot.execution.open_orders.len(), 1);
    assert_eq!(
        snapshot.execution.last_command_ack.as_deref(),
        Some("cmd_flatten_01")
    );
    assert_eq!(
        snapshot.execution.recent_commands[0].command_id,
        "cmd_flatten_01"
    );
    let occupied_client_order_ids = snapshot
        .strategy
        .levels
        .iter()
        .filter(|level| level.state == grid_platform_tui::protocol::GridLevelState::Occupied)
        .filter_map(|level| level.client_order_id.as_deref())
        .collect::<HashSet<_>>();
    assert!(
        snapshot
            .execution
            .open_orders
            .iter()
            .all(|order| !occupied_client_order_ids.contains(order.client_order_id.as_str()))
    );
    assert_eq!(
        serialized["execution"]["recent_fills"][0]["client_order_id"],
        "flatten_reduce_only_01"
    );
    assert_eq!(
        serialized["execution"]["recent_commands"][0]["client_order_ids"][0],
        "flatten_reduce_only_01"
    );
    assert_eq!(
        serialized["execution"]["recent_commands"][0]["order_ids"][0],
        "ord_0999"
    );
    assert_eq!(
        serialized["execution"]["recent_commands"][0]["trade_ids"][0],
        "fill_9001"
    );
    assert_eq!(serialized["strategy"]["status"], "occupied");
}

#[test]
fn runtime_snapshot_decodes_when_user_stream_field_is_omitted() {
    let raw = r#"{
        "connection": {
            "http_available": true,
            "ws_connected": false,
            "latency_ms": 42,
            "last_heartbeat_at": "2025-01-01T00:00:00Z",
            "reconnect_backoff_ms": 0,
            "stale_age_ms": 0
        },
        "runtime": {
            "symbol": "XAUUSDT",
            "env": "testnet",
            "session_state": "regular",
            "strategy_state": "running",
            "last_price": 2361.48,
            "mark_price": 2361.55,
            "position_qty": 0.25,
            "position_avg_price": 2354.2,
            "unrealized_pnl": 1.84,
            "realized_pnl": 14.52
        },
        "execution": {
            "open_orders": [],
            "recent_fills": [],
            "pending_commands": [],
            "last_command_ack": null,
            "last_command_ack_event": null,
            "recent_commands": []
        },
        "risk": {
            "current_notional": 590.39,
            "max_notional": 1500.0,
            "daily_loss_limit": -120.0,
            "stop_loss_pct": 4.0,
            "risk_level": "watch",
            "breaker_engaged": false,
            "unacked_alerts": 1
        }
    }"#;
    let snapshot: RuntimeSnapshot = serde_json::from_str(raw).unwrap();
    assert_eq!(snapshot.connection.user_stream_connected, None);
}

#[test]
fn runtime_snapshot_decodes_open_orders_source_and_legacy_payloads() {
    let raw = r#"{
        "connection": {
            "http_available": true,
            "ws_connected": false,
            "user_stream_connected": null,
            "latency_ms": 42,
            "last_heartbeat_at": "2025-01-01T00:00:00Z",
            "reconnect_backoff_ms": 0,
            "stale_age_ms": 0
        },
        "runtime": {
            "symbol": "XAUUSDT",
            "env": "testnet",
            "session_state": "regular",
            "strategy_state": "running",
            "last_price": 2361.48,
            "mark_price": 2361.55,
            "position_qty": 0.25,
            "position_avg_price": 2354.2,
            "unrealized_pnl": 1.84,
            "realized_pnl": 14.52
        },
        "execution": {
            "open_orders": [],
            "recent_fills": [],
            "pending_commands": [],
            "last_command_ack": null,
            "last_command_ack_event": null,
            "recent_commands": [],
            "open_orders_source": "strategy_mirror"
        },
        "risk": {
            "current_notional": 590.39,
            "max_notional": 1500.0,
            "daily_loss_limit": -120.0,
            "stop_loss_pct": 4.0,
            "risk_level": "watch",
            "breaker_engaged": false,
            "unacked_alerts": 1
        }
    }"#;
    let parsed: RuntimeSnapshot = serde_json::from_str(raw).unwrap();
    let serialized = serde_json::to_value(&parsed).unwrap();
    assert_eq!(
        serialized["execution"]["open_orders_source"],
        "strategy_mirror"
    );

    let legacy_raw = r#"{
        "connection": {
            "http_available": true,
            "ws_connected": false,
            "latency_ms": 42,
            "last_heartbeat_at": "2025-01-01T00:00:00Z",
            "reconnect_backoff_ms": 0,
            "stale_age_ms": 0
        },
        "runtime": {
            "symbol": "XAUUSDT",
            "env": "testnet",
            "session_state": "regular",
            "strategy_state": "running",
            "last_price": 2361.48,
            "mark_price": 2361.55,
            "position_qty": 0.25,
            "position_avg_price": 2354.2,
            "unrealized_pnl": 1.84,
            "realized_pnl": 14.52
        },
        "execution": {
            "open_orders": [],
            "recent_fills": [],
            "pending_commands": [],
            "last_command_ack": null,
            "last_command_ack_event": null,
            "recent_commands": []
        },
        "risk": {
            "current_notional": 590.39,
            "max_notional": 1500.0,
            "daily_loss_limit": -120.0,
            "stop_loss_pct": 4.0,
            "risk_level": "watch",
            "breaker_engaged": false,
            "unacked_alerts": 1
        }
    }"#;
    let legacy: RuntimeSnapshot = serde_json::from_str(legacy_raw).unwrap();
    let legacy_serialized = serde_json::to_value(&legacy).unwrap();
    assert_eq!(
        legacy_serialized["execution"]["open_orders_source"],
        "strategy_mirror"
    );
}

#[test]
fn server_envelope_decodes_runtime_snapshot() {
    let raw = fs::read_to_string(fixtures_dir().join("runtime_snapshot_event.json")).unwrap();
    let parsed: ServerEnvelope = serde_json::from_str(&raw).unwrap();
    assert_eq!(parsed.event_id, "evt_runtime_snapshot_0001");
    assert_eq!(parsed.sequence, None);
    match parsed.event {
        ServerEvent::RuntimeSnapshot(snapshot) => assert_eq!(snapshot.runtime.env, "testnet"),
        _ => panic!("unexpected event type"),
    }
}

#[test]
fn server_envelope_decodes_command_ack() {
    let raw = fs::read_to_string(fixtures_dir().join("command_ack_event.json")).unwrap();
    let parsed: ServerEnvelope = serde_json::from_str(&raw).unwrap();
    let serialized = serde_json::to_value(&parsed).unwrap();
    assert_eq!(parsed.sequence, Some(12));
    match parsed.event {
        ServerEvent::CommandAck(CommandAck {
            command_id, status, ..
        }) => {
            assert_eq!(command_id, "cmd_flatten_01");
            assert_eq!(status, CommandStatus::Completed);
        }
        _ => panic!("unexpected event type"),
    }
    assert_eq!(
        serialized["payload"]["client_order_ids"][0],
        "flatten_reduce_only_01"
    );
    assert_eq!(serialized["payload"]["order_ids"][0], "ord_0999");
    assert_eq!(serialized["payload"]["trade_ids"][0], "fill_9001");
}

#[test]
fn server_envelope_decodes_risk_alert() {
    let raw = fs::read_to_string(fixtures_dir().join("risk_alert_event.json")).unwrap();
    let parsed: ServerEnvelope = serde_json::from_str(&raw).unwrap();
    assert_eq!(parsed.sequence, None);
    match parsed.event {
        ServerEvent::RiskAlert(RiskEvent { code, message, .. }) => {
            assert_eq!(code, "STOP_LOSS_TRIGGERED");
            assert!(message.contains("stop-loss threshold"));
        }
        _ => panic!("unexpected event type"),
    }
}

#[test]
fn server_envelope_decodes_connection_changed() {
    let raw = fs::read_to_string(fixtures_dir().join("connection_changed_event.json")).unwrap();
    let parsed: ServerEnvelope = serde_json::from_str(&raw).unwrap();
    assert_eq!(parsed.sequence, Some(14));
    match parsed.event {
        ServerEvent::ConnectionChanged(ConnectionState {
            ws_connected,
            user_stream_connected,
            latency_ms,
            ..
        }) => {
            assert!(ws_connected);
            assert_eq!(user_stream_connected, None);
            assert_eq!(latency_ms, Some(87));
        }
        _ => panic!("unexpected event type"),
    }
}

#[test]
fn connection_changed_decodes_when_user_stream_field_is_omitted() {
    let raw = r#"{
        "version": "v1alpha1",
        "event_id": "evt_connection_changed_legacy",
        "type": "connection_changed",
        "emitted_at": "2025-01-01T00:00:06Z",
        "sequence": 14,
        "payload": {
            "http_available": true,
            "ws_connected": true,
            "latency_ms": 87,
            "last_heartbeat_at": "2025-01-01T00:00:06Z",
            "reconnect_backoff_ms": 0,
            "stale_age_ms": 0
        }
    }"#;
    let parsed: ServerEnvelope = serde_json::from_str(raw).unwrap();
    match parsed.event {
        ServerEvent::ConnectionChanged(ConnectionState {
            user_stream_connected,
            ..
        }) => assert_eq!(user_stream_connected, None),
        _ => panic!("unexpected event type"),
    }
}

#[test]
fn http_success_envelope_decodes_runtime_snapshot() {
    let raw = r#"{
        "version": "v1alpha1",
        "status": "ok",
        "data": {
            "connection": {
                "http_available": true,
                "ws_connected": false,
                "user_stream_connected": null,
                "latency_ms": 42,
                "last_heartbeat_at": "2025-01-01T00:00:00Z",
                "reconnect_backoff_ms": 0,
                "stale_age_ms": 0
            },
            "runtime": {
                "symbol": "XAUUSDT",
                "env": "testnet",
                "session_state": "regular",
                "strategy_state": "running",
                "last_price": 2361.48,
                "mark_price": 2361.55,
                "position_qty": 0.25,
                "position_avg_price": 2354.2,
                "unrealized_pnl": 1.84,
                "realized_pnl": 14.52
            },
            "execution": {
                "open_orders": [],
                "recent_fills": [],
                "pending_commands": [],
                "last_command_ack": "cmd_resume_01",
                "last_command_ack_event": {
                    "command_id": "cmd_resume_01",
                    "command": "resume",
                    "status": "completed",
                    "message": "Strategy resumed.",
                    "emitted_at": "2025-01-01T00:00:05Z"
                },
                "recent_commands": [
                    {
                        "command_id": "cmd_resume_01",
                        "command": "resume",
                        "status": "completed",
                        "summary": "Strategy resumed.",
                        "requested_at": "2025-01-01T00:00:03Z",
                        "accepted_at": "2025-01-01T00:00:04Z",
                        "finished_at": "2025-01-01T00:00:05Z"
                    }
                ]
            },
            "risk": {
                "current_notional": 590.39,
                "max_notional": 1500.0,
                "daily_loss_limit": -120.0,
                "stop_loss_pct": 4.0,
                "risk_level": "watch",
                "breaker_engaged": false,
                "unacked_alerts": 1
            }
        }
    }"#;
    let parsed: HttpSuccessEnvelope<RuntimeSnapshot> = serde_json::from_str(raw).unwrap();
    assert_eq!(parsed.version, "v1alpha1");
    assert_eq!(parsed.status, "ok");
    assert_eq!(parsed.data.runtime.symbol, "XAUUSDT");
    assert_eq!(parsed.data.connection.user_stream_connected, None);
    assert_eq!(
        parsed.data.execution.recent_commands[0].command_id,
        "cmd_resume_01"
    );
}
