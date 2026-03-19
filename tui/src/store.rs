use crate::effects::Effect;
use crate::events::{
    AppEvent, EffectResultEvent, InputEvent, KeyAction, LocalUiEvent, SystemEvent,
};
use crate::protocol::{CommandAck, CommandStatus, CommandType, ServerEvent};
use crate::state::{
    AppState, COMMAND_TIMEOUT_TICKS, CommandTimelineEntry, CommandTimelineStage, DirtyFlags, Modal,
    Page, Toast, ToastLevel,
};

pub fn reduce(state: &mut AppState, event: AppEvent) -> Vec<Effect> {
    match event {
        AppEvent::Protocol(protocol) => handle_protocol_event(state, protocol),
        AppEvent::Input(input) => handle_input_event(state, input),
        AppEvent::System(system) => handle_system_event(state, system),
        AppEvent::EffectResult(result) => handle_effect_result(state, result),
        AppEvent::LocalUi(local) => handle_local_ui_event(state, local),
        AppEvent::Command(_) => Vec::new(),
    }
}

fn handle_protocol_event(state: &mut AppState, event: ServerEvent) -> Vec<Effect> {
    match event {
        ServerEvent::RuntimeSnapshot(snapshot) => {
            state.sync_runtime_snapshot(snapshot);
            Vec::new()
        }
        ServerEvent::PriceUpdated(price) => {
            state.runtime.last_price = price.last_price;
            state.runtime.mark_price = price.mark_price;
            state.connection.last_heartbeat_at = price.emitted_at;
            state.connection.stale_age_ms = 0;
            state.mark_dirty(
                DirtyFlags {
                    runtime: true,
                    connection: true,
                    ..DirtyFlags::default()
                },
                false,
            );
            Vec::new()
        }
        ServerEvent::RiskAlert(alert) => {
            state.risk.risk_level = alert.severity;
            state.risk.alerts.push_front(alert);
            while state.risk.alerts.len() > 20 {
                state.risk.alerts.pop_back();
            }
            state.mark_dirty(
                DirtyFlags {
                    risk: true,
                    ..DirtyFlags::default()
                },
                true,
            );
            Vec::new()
        }
        ServerEvent::CommandAck(ack) => {
            apply_command_ack(state, ack);
            Vec::new()
        }
        ServerEvent::ConnectionChanged(connection) => {
            state.connection.http_available = connection.http_available;
            state.connection.market_ws_connected = connection.ws_connected;
            state.connection.user_stream_connected = connection.user_stream_connected;
            state.connection.latency_ms = connection.latency_ms;
            state.connection.stale_age_ms = connection.stale_age_ms;
            state.connection.last_heartbeat_at = connection.last_heartbeat_at;
            state.connection.market_reconnect_backoff_ms = connection.reconnect_backoff_ms;
            state.mark_dirty(
                DirtyFlags {
                    connection: true,
                    ..DirtyFlags::default()
                },
                true,
            );
            Vec::new()
        }
    }
}

fn handle_input_event(state: &mut AppState, event: InputEvent) -> Vec<Effect> {
    match event {
        InputEvent::Resize(width, height) => {
            state.ui.width = width;
            state.ui.height = height;
            state.mark_dirty(
                DirtyFlags {
                    ui: true,
                    ..DirtyFlags::default()
                },
                true,
            );
            Vec::new()
        }
        InputEvent::Key(action) => match action {
            KeyAction::ViewDashboard => switch_page(state, Page::Dashboard),
            KeyAction::ViewGrid => switch_page(state, Page::Grid),
            KeyAction::ViewMarket => switch_page(state, Page::Market),
            KeyAction::ViewEvents => switch_page(state, Page::Events),
            KeyAction::ToggleHelp => switch_page(state, Page::Help),
            KeyAction::NextFocus => advance_focus(state, 1),
            KeyAction::PrevFocus => advance_focus(state, -1),
            KeyAction::Pause => submit_command(state, CommandType::Pause),
            KeyAction::Resume => submit_command(state, CommandType::Resume),
            KeyAction::CancelAll => open_confirm(state, CommandType::CancelAll),
            KeyAction::FlattenNow => open_confirm(state, CommandType::FlattenNow),
            KeyAction::ShutdownAfterFlatten => {
                open_confirm(state, CommandType::ShutdownAfterFlatten)
            }
            KeyAction::Confirm => handle_local_ui_event(state, LocalUiEvent::ConfirmModal),
            KeyAction::Cancel => handle_local_ui_event(state, LocalUiEvent::CancelModal),
            KeyAction::Quit => {
                state.ui.should_quit = true;
                Vec::new()
            }
        },
    }
}

fn handle_system_event(state: &mut AppState, event: SystemEvent) -> Vec<Effect> {
    match event {
        SystemEvent::RenderTick => {
            if let Some(toast) = &mut state.ui.toast
                && toast.ttl_ticks > 0
            {
                toast.ttl_ticks -= 1;
                if toast.ttl_ticks == 0 {
                    state.ui.toast = None;
                }
            }
            Vec::new()
        }
        SystemEvent::HealthTick => {
            state.clock_ticks += 1;
            let mut flags = DirtyFlags::default();

            if !state.connection.ws_connected {
                flags.connection = true;
            }

            if expire_stalled_commands(state) {
                flags.execution = true;
                flags.ui = true;
            }

            if flags.any() {
                state.mark_dirty(flags, false);
            }
            Vec::new()
        }
    }
}

fn handle_effect_result(state: &mut AppState, event: EffectResultEvent) -> Vec<Effect> {
    match event {
        EffectResultEvent::SnapshotLoaded(snapshot) => {
            if state.connection.ws_connected {
                state.sync_runtime_snapshot(snapshot);
                Vec::new()
            } else {
                *state = AppState::from_snapshot(snapshot);
                vec![Effect::ConnectWs]
            }
        }
        EffectResultEvent::SnapshotFailed(error) => {
            state.ui.toast = Some(Toast {
                level: ToastLevel::Danger,
                message: format!("snapshot failed: {error}"),
                ttl_ticks: 24,
            });
            state.mark_dirty(
                DirtyFlags {
                    ui: true,
                    ..DirtyFlags::default()
                },
                true,
            );
            Vec::new()
        }
        EffectResultEvent::WsConnected => {
            let reconnect_attempt = state.connection.reconnect_attempt;
            state.connection.ws_connected = true;
            state.connection.reconnect_attempt = 0;
            state.connection.reconnect_backoff_ms = 0;
            let timestamp = state.local_timestamp();
            push_system_event(
                state,
                "info",
                "transport",
                "WebSocket connected and streaming.",
                timestamp,
            );
            state.ui.toast = Some(Toast {
                level: ToastLevel::Info,
                message: "ws connected".into(),
                ttl_ticks: 16,
            });
            state.mark_dirty(
                DirtyFlags {
                    connection: true,
                    ui: true,
                    ..DirtyFlags::default()
                },
                true,
            );
            if reconnect_attempt > 0 {
                vec![Effect::FetchSnapshot]
            } else {
                Vec::new()
            }
        }
        EffectResultEvent::WsDisconnected(reason) => {
            state.connection.ws_connected = false;
            state.connection.reconnect_attempt += 1;
            let backoff_multiplier = 2u64
                .saturating_pow(state.connection.reconnect_attempt.saturating_sub(1))
                .min(8);
            state.connection.reconnect_backoff_ms = 1_000u64.saturating_mul(backoff_multiplier);
            let timestamp = state.local_timestamp();
            push_system_event(
                state,
                "warn",
                "transport",
                &format!("WebSocket disconnected: {reason}"),
                timestamp,
            );
            state.ui.toast = Some(Toast {
                level: ToastLevel::Warning,
                message: format!("ws disconnected: {reason}"),
                ttl_ticks: 24,
            });
            state.mark_dirty(DirtyFlags::all(), true);
            vec![Effect::ReconnectWs {
                attempt: state.connection.reconnect_attempt,
            }]
        }
        EffectResultEvent::CommandAccepted(accepted) => {
            if let Some(item) = state
                .execution
                .pending_commands
                .iter_mut()
                .find(|item| item.command_id == accepted.command_id)
            {
                item.status = accepted.status;
            }
            record_command_stage(
                state,
                &accepted.command_id,
                accepted.command,
                CommandTimelineStage::Accepted,
                "Service accepted command; waiting for final acknowledgement.".into(),
                accepted.accepted_at,
                None,
            );
            state.mark_dirty(
                DirtyFlags {
                    execution: true,
                    ..DirtyFlags::default()
                },
                true,
            );
            Vec::new()
        }
        EffectResultEvent::CommandFailed { command_id, error } => {
            let command = state
                .execution
                .pending_commands
                .iter()
                .find(|item| item.command_id == command_id)
                .map(|item| item.command)
                .or_else(|| {
                    state
                        .execution
                        .command_timeline
                        .iter()
                        .find(|item| item.command_id == command_id)
                        .map(|item| item.command)
                })
                .unwrap_or(CommandType::Pause);
            state
                .execution
                .pending_commands
                .retain(|item| item.command_id != command_id);
            let timestamp = state.local_timestamp();
            record_command_stage(
                state,
                &command_id,
                command,
                CommandTimelineStage::Failed,
                format!("Command request failed before ack: {error}"),
                timestamp,
                None,
            );
            state.ui.toast = Some(Toast {
                level: ToastLevel::Danger,
                message: format!("command failed: {error}"),
                ttl_ticks: 24,
            });
            state.mark_dirty(DirtyFlags::all(), true);
            Vec::new()
        }
    }
}

fn handle_local_ui_event(state: &mut AppState, event: LocalUiEvent) -> Vec<Effect> {
    match event {
        LocalUiEvent::OpenConfirm(command) => {
            state.ui.modal = Some(Modal::Confirm(command));
            state.mark_dirty(
                DirtyFlags {
                    ui: true,
                    ..DirtyFlags::default()
                },
                true,
            );
            Vec::new()
        }
        LocalUiEvent::ConfirmModal => {
            if let Some(Modal::Confirm(command)) = state.ui.modal.take() {
                submit_command(state, command)
            } else {
                Vec::new()
            }
        }
        LocalUiEvent::CancelModal => {
            state.ui.modal = None;
            state.mark_dirty(
                DirtyFlags {
                    ui: true,
                    ..DirtyFlags::default()
                },
                true,
            );
            Vec::new()
        }
        LocalUiEvent::ClearToast => {
            state.ui.toast = None;
            state.mark_dirty(
                DirtyFlags {
                    ui: true,
                    ..DirtyFlags::default()
                },
                true,
            );
            Vec::new()
        }
    }
}

fn submit_command(state: &mut AppState, command: CommandType) -> Vec<Effect> {
    let command_id = state.queue_command(command);
    state.ui.modal = None;
    state.mark_dirty(
        DirtyFlags {
            execution: true,
            ui: true,
            ..DirtyFlags::default()
        },
        true,
    );
    vec![Effect::SendCommand {
        command,
        command_id,
    }]
}

fn open_confirm(state: &mut AppState, command: CommandType) -> Vec<Effect> {
    handle_local_ui_event(state, LocalUiEvent::OpenConfirm(command))
}

fn switch_page(state: &mut AppState, page: Page) -> Vec<Effect> {
    state.ui.page = page;
    state.ui.focus_index = page.normalize_focus(state.ui.focus_index);
    state.mark_dirty(
        DirtyFlags {
            ui: true,
            ..DirtyFlags::default()
        },
        true,
    );
    Vec::new()
}

fn advance_focus(state: &mut AppState, delta: isize) -> Vec<Effect> {
    let count = state.ui.page.panel_count();
    if count > 0 {
        let current = state.ui.page.normalize_focus(state.ui.focus_index) as isize;
        let wrapped = (current + delta).rem_euclid(count as isize) as usize;
        state.ui.focus_index = wrapped;
        state.mark_dirty(
            DirtyFlags {
                ui: true,
                ..DirtyFlags::default()
            },
            true,
        );
    }
    Vec::new()
}

fn apply_command_ack(state: &mut AppState, ack: CommandAck) {
    state
        .execution
        .pending_commands
        .retain(|item| item.command_id != ack.command_id);
    state.execution.last_command_ack = Some(ack.clone());
    push_system_event(
        state,
        command_status_level(ack.status),
        "service",
        &ack.message,
        ack.emitted_at.clone(),
    );

    let stage = match ack.status {
        CommandStatus::Completed => CommandTimelineStage::Ack,
        CommandStatus::Failed => CommandTimelineStage::Failed,
        CommandStatus::TimedOut => CommandTimelineStage::TimedOut,
        CommandStatus::Pending => CommandTimelineStage::Pending,
        CommandStatus::Accepted => CommandTimelineStage::Accepted,
    };
    record_command_stage(
        state,
        &ack.command_id,
        ack.command,
        stage,
        ack.message.clone(),
        ack.emitted_at.clone(),
        Some(ack.links.clone()),
    );

    if stage == CommandTimelineStage::Ack {
        state.runtime.strategy_state = match ack.command {
            CommandType::Pause => "paused".into(),
            CommandType::Resume => "running".into(),
            CommandType::CancelAll => state.runtime.strategy_state.clone(),
            CommandType::FlattenNow => state.runtime.strategy_state.clone(),
            CommandType::ShutdownAfterFlatten => "paused".into(),
        };
        if matches!(
            ack.command,
            CommandType::FlattenNow | CommandType::ShutdownAfterFlatten
        ) {
            state.runtime.position_qty = 0.0;
            state.runtime.unrealized_pnl = 0.0;
        }
        if matches!(
            ack.command,
            CommandType::CancelAll | CommandType::ShutdownAfterFlatten
        ) {
            state.execution.open_orders.clear();
        }
    }

    state.ui.toast = Some(Toast {
        level: match stage {
            CommandTimelineStage::Ack => ToastLevel::Info,
            CommandTimelineStage::Failed => ToastLevel::Danger,
            CommandTimelineStage::TimedOut => ToastLevel::Warning,
            CommandTimelineStage::Pending | CommandTimelineStage::Accepted => ToastLevel::Info,
        },
        message: ack.message,
        ttl_ticks: 16,
    });
    state.mark_dirty(
        DirtyFlags {
            execution: true,
            runtime: true,
            ui: true,
            ..DirtyFlags::default()
        },
        true,
    );
}

fn record_command_stage(
    state: &mut AppState,
    command_id: &str,
    command: CommandType,
    stage: CommandTimelineStage,
    summary: String,
    timestamp: String,
    links: Option<crate::protocol::CommandLinks>,
) {
    let next_timeout = (!stage.is_terminal()).then(|| state.clock_ticks + COMMAND_TIMEOUT_TICKS);
    let incoming_links = links.unwrap_or_default();

    if let Some(entry) = state
        .execution
        .command_timeline
        .iter_mut()
        .find(|entry| entry.command_id == command_id)
    {
        entry.command = command;
        entry.stage = stage;
        entry.summary = summary;
        if !incoming_links.client_order_ids.is_empty()
            || !incoming_links.order_ids.is_empty()
            || !incoming_links.trade_ids.is_empty()
        {
            entry.links = incoming_links.clone();
        }
        match stage {
            CommandTimelineStage::Pending => {
                entry.requested_at = timestamp;
                entry.accepted_at = None;
                entry.finished_at = None;
            }
            CommandTimelineStage::Accepted => {
                entry.accepted_at = Some(timestamp);
                entry.finished_at = None;
            }
            CommandTimelineStage::Ack
            | CommandTimelineStage::Failed
            | CommandTimelineStage::TimedOut => {
                entry.finished_at = Some(timestamp);
            }
        }
        entry.timeout_at_tick = next_timeout;
    } else {
        state
            .execution
            .command_timeline
            .push_front(CommandTimelineEntry {
                command_id: command_id.into(),
                command,
                stage,
                summary,
                requested_at: timestamp.clone(),
                accepted_at: matches!(stage, CommandTimelineStage::Accepted)
                    .then_some(timestamp.clone()),
                finished_at: stage.is_terminal().then_some(timestamp),
                links: incoming_links,
                timeout_at_tick: next_timeout,
            });
    }

    state.trim_command_timeline();
}

fn expire_stalled_commands(state: &mut AppState) -> bool {
    let expired = state
        .execution
        .command_timeline
        .iter()
        .filter(|entry| {
            matches!(
                entry.stage,
                CommandTimelineStage::Pending | CommandTimelineStage::Accepted
            )
        })
        .filter_map(|entry| {
            entry
                .timeout_at_tick
                .filter(|deadline| *deadline <= state.clock_ticks)
                .map(|_| (entry.command_id.clone(), entry.command))
        })
        .collect::<Vec<_>>();

    if expired.is_empty() {
        return false;
    }

    let timestamp = state.local_timestamp();
    for (command_id, command) in expired {
        state
            .execution
            .pending_commands
            .retain(|item| item.command_id != command_id);
        record_command_stage(
            state,
            &command_id,
            command,
            CommandTimelineStage::TimedOut,
            "No final acknowledgement arrived within the timeout window.".into(),
            timestamp.clone(),
            None,
        );
    }

    state.ui.toast = Some(Toast {
        level: ToastLevel::Warning,
        message: "one or more commands timed out".into(),
        ttl_ticks: 20,
    });
    true
}

fn push_system_event(
    state: &mut AppState,
    level: &str,
    source: &str,
    message: &str,
    created_at: String,
) {
    state
        .system_events
        .push_front(crate::protocol::SystemEvent {
            level: level.into(),
            source: source.into(),
            message: message.into(),
            created_at,
        });
    while state.system_events.len() > 50 {
        state.system_events.pop_back();
    }
}

fn command_status_level(status: CommandStatus) -> &'static str {
    match status {
        CommandStatus::Pending | CommandStatus::Accepted | CommandStatus::Completed => "info",
        CommandStatus::TimedOut => "warn",
        CommandStatus::Failed => "error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{AppEvent, EffectResultEvent, InputEvent, KeyAction, LocalUiEvent};
    use crate::protocol::{CommandAccepted, CommandRecord, RuntimeSnapshot};

    #[test]
    fn pause_shortcut_creates_send_command_effect() {
        let mut state = AppState::sample();
        let pending_before = state.execution.pending_commands.len();
        let effects = reduce(
            &mut state,
            AppEvent::Input(InputEvent::Key(KeyAction::Pause)),
        );
        assert!(matches!(
            effects.first(),
            Some(Effect::SendCommand {
                command: CommandType::Pause,
                ..
            })
        ));
        assert_eq!(state.execution.pending_commands.len(), pending_before + 1);
    }

    #[test]
    fn websocket_disconnect_schedules_reconnect() {
        let mut state = AppState::sample();
        let effects = reduce(
            &mut state,
            AppEvent::EffectResult(EffectResultEvent::WsDisconnected("boom".into())),
        );
        assert!(matches!(
            effects.first(),
            Some(Effect::ReconnectWs { attempt: 1 })
        ));
        assert!(!state.connection.ws_connected);
    }

    #[test]
    fn websocket_reconnect_triggers_snapshot_refetch() {
        let mut state = AppState::sample();
        state.connection.ws_connected = false;
        state.connection.reconnect_attempt = 1;

        let effects = reduce(
            &mut state,
            AppEvent::EffectResult(EffectResultEvent::WsConnected),
        );

        assert_eq!(effects, vec![Effect::FetchSnapshot]);
        assert!(state.connection.ws_connected);
    }

    #[test]
    fn confirm_modal_submits_deferred_command() {
        let mut state = AppState::sample();
        reduce(
            &mut state,
            AppEvent::LocalUi(LocalUiEvent::OpenConfirm(CommandType::FlattenNow)),
        );
        let effects = reduce(
            &mut state,
            AppEvent::Input(InputEvent::Key(KeyAction::Confirm)),
        );
        assert!(matches!(
            effects.first(),
            Some(Effect::SendCommand {
                command: CommandType::FlattenNow,
                ..
            })
        ));
    }

    #[test]
    fn snapshot_loaded_replaces_state() {
        let mut state = AppState::sample();
        state.connection.ws_connected = false;
        let mut snapshot = RuntimeSnapshot::sample();
        snapshot.runtime.symbol = "TSLAUSDT".into();
        let effects = reduce(
            &mut state,
            AppEvent::EffectResult(EffectResultEvent::SnapshotLoaded(snapshot)),
        );
        assert_eq!(state.runtime.symbol, "TSLAUSDT");
        assert_eq!(effects, vec![Effect::ConnectWs]);
    }

    #[test]
    fn runtime_snapshot_preserves_transport_connection_state() {
        let mut state = AppState::sample();
        state.connection.ws_connected = true;
        state.ui.page = Page::Events;

        let mut snapshot = RuntimeSnapshot::sample();
        snapshot.connection.ws_connected = false;
        snapshot.runtime.symbol = "TSLAUSDT".into();

        reduce(
            &mut state,
            AppEvent::Protocol(ServerEvent::RuntimeSnapshot(snapshot)),
        );

        assert!(state.connection.ws_connected);
        assert_eq!(state.runtime.symbol, "TSLAUSDT");
        assert_eq!(state.ui.page, Page::Events);
    }

    #[test]
    fn runtime_snapshot_recovers_command_result_from_snapshot() {
        let mut state = AppState::sample();
        let mut snapshot = RuntimeSnapshot::sample();
        snapshot.execution.last_command_ack_event = Some(CommandAck {
            command_id: "cmd_flatten_recovered".into(),
            command: CommandType::FlattenNow,
            status: CommandStatus::Completed,
            message: "Position flattened.".into(),
            links: crate::protocol::CommandLinks {
                client_order_ids: vec!["flatten_reduce_only_cmd_flatten_recovered".into()],
                order_ids: vec!["order_cmd_flatten_recovered".into()],
                trade_ids: vec!["trade_cmd_flatten_recovered".into()],
            },
            emitted_at: "2025-01-01T00:00:05Z".into(),
        });
        snapshot.execution.recent_commands = vec![CommandRecord {
            command_id: "cmd_flatten_recovered".into(),
            command: CommandType::FlattenNow,
            status: CommandStatus::Completed,
            summary: "Position flattened.".into(),
            requested_at: "2025-01-01T00:00:03Z".into(),
            accepted_at: Some("2025-01-01T00:00:04Z".into()),
            finished_at: Some("2025-01-01T00:00:05Z".into()),
            links: crate::protocol::CommandLinks {
                client_order_ids: vec!["flatten_reduce_only_cmd_flatten_recovered".into()],
                order_ids: vec!["order_cmd_flatten_recovered".into()],
                trade_ids: vec!["trade_cmd_flatten_recovered".into()],
            },
        }];

        reduce(
            &mut state,
            AppEvent::Protocol(ServerEvent::RuntimeSnapshot(snapshot)),
        );

        assert!(
            state
                .execution
                .last_command_ack
                .as_ref()
                .is_some_and(|ack| ack.command_id == "cmd_flatten_recovered")
        );
        assert!(state.execution.command_timeline.iter().any(|entry| {
            entry.command_id == "cmd_flatten_recovered"
                && entry.stage == CommandTimelineStage::Ack
                && entry.summary == "Position flattened."
        }));
    }

    #[test]
    fn command_accepted_updates_pending_status_and_timeline() {
        let mut state = AppState::sample();
        let effects = reduce(
            &mut state,
            AppEvent::Input(InputEvent::Key(KeyAction::Pause)),
        );
        let command_id = match effects.first().unwrap() {
            Effect::SendCommand { command_id, .. } => command_id.clone(),
            _ => unreachable!(),
        };
        reduce(
            &mut state,
            AppEvent::EffectResult(EffectResultEvent::CommandAccepted(CommandAccepted {
                version: "v1alpha1".into(),
                command_id: command_id.clone(),
                command: CommandType::Pause,
                status: CommandStatus::Accepted,
                accepted_at: "2025-01-01T00:00:00Z".into(),
            })),
        );
        assert_eq!(
            state
                .execution
                .pending_commands
                .iter()
                .find(|item| item.command_id == command_id)
                .unwrap()
                .status,
            CommandStatus::Accepted
        );
        assert_eq!(
            state
                .execution
                .command_timeline
                .iter()
                .find(|item| item.command_id == command_id)
                .unwrap()
                .stage,
            CommandTimelineStage::Accepted
        );
    }

    #[test]
    fn focus_navigation_wraps_per_page() {
        let mut state = AppState::sample();
        state.ui.page = Page::Events;
        state.ui.focus_index = 0;
        reduce(
            &mut state,
            AppEvent::Input(InputEvent::Key(KeyAction::PrevFocus)),
        );
        assert_eq!(state.ui.focus_index, Page::Events.panel_count() - 1);
        reduce(
            &mut state,
            AppEvent::Input(InputEvent::Key(KeyAction::NextFocus)),
        );
        assert_eq!(state.ui.focus_index, 0);
    }

    #[test]
    fn command_ack_completes_timeline() {
        let mut state = AppState::sample();
        let effects = reduce(
            &mut state,
            AppEvent::Input(InputEvent::Key(KeyAction::Pause)),
        );
        let command_id = match effects.first().unwrap() {
            Effect::SendCommand { command_id, .. } => command_id.clone(),
            _ => unreachable!(),
        };
        reduce(
            &mut state,
            AppEvent::Protocol(ServerEvent::CommandAck(CommandAck {
                command_id: command_id.clone(),
                command: CommandType::Pause,
                status: CommandStatus::Completed,
                message: "Paused.".into(),
                links: crate::protocol::CommandLinks::default(),
                emitted_at: "2025-01-01T00:00:02Z".into(),
            })),
        );
        assert!(
            state
                .execution
                .pending_commands
                .iter()
                .all(|item| item.command_id != command_id)
        );
        assert_eq!(
            state
                .execution
                .command_timeline
                .iter()
                .find(|item| item.command_id == command_id)
                .unwrap()
                .stage,
            CommandTimelineStage::Ack
        );
    }

    #[test]
    fn accepted_command_times_out_after_health_ticks() {
        let mut state = AppState::sample();
        let effects = reduce(
            &mut state,
            AppEvent::Input(InputEvent::Key(KeyAction::Pause)),
        );
        let command_id = match effects.first().unwrap() {
            Effect::SendCommand { command_id, .. } => command_id.clone(),
            _ => unreachable!(),
        };
        reduce(
            &mut state,
            AppEvent::EffectResult(EffectResultEvent::CommandAccepted(CommandAccepted {
                version: "v1alpha1".into(),
                command_id: command_id.clone(),
                command: CommandType::Pause,
                status: CommandStatus::Accepted,
                accepted_at: "2025-01-01T00:00:01Z".into(),
            })),
        );
        for _ in 0..COMMAND_TIMEOUT_TICKS {
            reduce(&mut state, AppEvent::System(SystemEvent::HealthTick));
        }
        assert!(
            state
                .execution
                .pending_commands
                .iter()
                .all(|item| item.command_id != command_id)
        );
        assert_eq!(
            state
                .execution
                .command_timeline
                .iter()
                .find(|item| item.command_id == command_id)
                .unwrap()
                .stage,
            CommandTimelineStage::TimedOut
        );
    }

    #[test]
    fn failed_command_ack_uses_error_system_event_level() {
        let mut state = AppState::sample();

        reduce(
            &mut state,
            AppEvent::Protocol(ServerEvent::CommandAck(CommandAck {
                command_id: "cmd_failed_level".into(),
                command: CommandType::CancelAll,
                status: CommandStatus::Failed,
                message: "exchange rejected cancel-all".into(),
                links: crate::protocol::CommandLinks::default(),
                emitted_at: "2025-01-01T00:00:02Z".into(),
            })),
        );

        assert_eq!(
            state.system_events.front().expect("system event").level,
            "error"
        );
    }

    #[test]
    fn shutdown_after_flatten_ack_clears_open_orders_locally() {
        let mut state = AppState::sample();
        assert!(!state.execution.open_orders.is_empty());

        reduce(
            &mut state,
            AppEvent::Protocol(ServerEvent::CommandAck(CommandAck {
                command_id: "cmd_shutdown_local".into(),
                command: CommandType::ShutdownAfterFlatten,
                status: CommandStatus::Completed,
                message: "Position flattened and shutdown requested.".into(),
                links: crate::protocol::CommandLinks {
                    client_order_ids: vec!["grid_buy_01".into(), "reduce_only_cmd".into()],
                    order_ids: vec!["ord_1001".into(), "order_cmd".into()],
                    trade_ids: vec!["trade_cmd".into()],
                },
                emitted_at: "2025-01-01T00:00:02Z".into(),
            })),
        );

        assert!(state.execution.open_orders.is_empty());
        assert_eq!(state.runtime.strategy_state, "paused");
    }
}
