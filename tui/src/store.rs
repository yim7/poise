use std::collections::{HashSet, VecDeque};

use crate::effects::Effect;
use crate::events::{
    AppEvent, EffectResultEvent, InputEvent, KeyAction, LocalUiEvent, ProtocolEvent, SystemEvent,
};
use crate::locale;
use crate::protocol::{CommandAck, CommandStatus, CommandType, ServerEvent};
use crate::state::{
    AppState, COMMAND_TIMEOUT_TICKS, CommandTimelineEntry, CommandTimelineStage, DirtyFlags, Modal,
    Page, SnapshotBootstrapState, Toast, ToastLevel,
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

fn handle_protocol_event(state: &mut AppState, protocol: ProtocolEvent) -> Vec<Effect> {
    if let Some(symbol) = protocol.symbol.as_deref()
        && !current_symbol_matches(state, symbol)
    {
        return Vec::new();
    }
    if let Some(generation) = protocol.generation
        && !current_generation_matches(state, generation)
    {
        return Vec::new();
    }

    match protocol.event {
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
            if alert.acknowledged_at.is_none() {
                state.risk.unacked_alerts = state.risk.unacked_alerts.saturating_add(1);
            }
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
            KeyAction::NextInstance => cycle_instance(state, 1),
            KeyAction::PrevInstance => cycle_instance(state, -1),
            KeyAction::Pause => submit_command(state, CommandType::Pause),
            KeyAction::Resume => submit_command(state, CommandType::Resume),
            KeyAction::CancelAll => open_confirm(state, CommandType::CancelAll),
            KeyAction::FlattenNow => open_confirm(state, CommandType::FlattenNow),
            KeyAction::ShutdownAfterFlatten => {
                open_confirm(state, CommandType::ShutdownAfterFlatten)
            }
            KeyAction::ToggleLocale => {
                state.ui.locale = state.ui.locale.toggle();
                state.mark_dirty(
                    DirtyFlags {
                        ui: true,
                        ..DirtyFlags::default()
                    },
                    true,
                );
                Vec::new()
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
        EffectResultEvent::InstancesLoaded(directory) => {
            let selected_symbol = state
                .instances
                .current_symbol
                .as_ref()
                .filter(|symbol| {
                    directory
                        .instances
                        .iter()
                        .any(|item| item.symbol == **symbol)
                })
                .cloned()
                .unwrap_or_else(|| directory.default_symbol.clone());
            state.instances = crate::state::InstancesViewState::from_directory(directory);
            state.begin_instance_bootstrap(selected_symbol.clone());
            let generation = state.instances.generation;
            vec![
                Effect::UseInstance {
                    symbol: selected_symbol.clone(),
                    generation,
                },
                Effect::FetchSnapshot {
                    symbol: selected_symbol,
                    generation,
                },
            ]
        }
        EffectResultEvent::InstancesFailed(error) => {
            let retry_count = match &state.snapshot_state {
                SnapshotBootstrapState::WaitingFirstSnapshot => 1,
                SnapshotBootstrapState::SnapshotRetrying { retry_count, .. } => {
                    retry_count.saturating_add(1)
                }
                SnapshotBootstrapState::Ready => 1,
            };
            let retry_in_ms = AppState::snapshot_retry_backoff_ms(retry_count);
            state.snapshot_state = SnapshotBootstrapState::SnapshotRetrying {
                last_error: error.clone(),
                retry_count,
                retry_in_ms,
            };
            state.ui.modal = None;
            state.ui.toast = Some(Toast {
                level: ToastLevel::Danger,
                message: locale::copy(state.ui.locale)
                    .toast()
                    .snapshot_failed(&error),
                ttl_ticks: 24,
            });
            state.mark_dirty(DirtyFlags::all(), true);
            vec![Effect::FetchInstancesAfterDelay { retry_in_ms }]
        }
        EffectResultEvent::SnapshotLoaded {
            symbol,
            generation,
            snapshot,
        } => {
            if !current_instance_matches(state, &symbol, generation) {
                return Vec::new();
            }
            let was_connected = state.connection.ws_connected;
            state.sync_runtime_snapshot(snapshot);
            if state.ui.pending_instance_switch.as_deref() == Some(symbol.as_str()) {
                state.ui.toast = Some(Toast {
                    level: ToastLevel::Info,
                    message: locale::copy(state.ui.locale)
                        .toast()
                        .switched_instance(&symbol),
                    ttl_ticks: 24,
                });
                state.ui.pending_instance_switch = None;
            }
            if was_connected {
                vec![Effect::FetchRiskEvents { symbol, generation }]
            } else {
                vec![
                    Effect::FetchRiskEvents {
                        symbol: symbol.clone(),
                        generation,
                    },
                    Effect::ConnectWs { symbol, generation },
                ]
            }
        }
        EffectResultEvent::SnapshotFailed {
            symbol,
            generation,
            error,
        } => {
            if !current_instance_matches(state, &symbol, generation) {
                return Vec::new();
            }
            match &state.snapshot_state {
                SnapshotBootstrapState::Ready => {
                    let toast_copy = locale::copy(state.ui.locale).toast();
                    state.ui.toast = Some(Toast {
                        level: ToastLevel::Danger,
                        message: toast_copy.snapshot_failed(&error),
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
                SnapshotBootstrapState::WaitingFirstSnapshot
                | SnapshotBootstrapState::SnapshotRetrying { .. } => {
                    let retry_count = match &state.snapshot_state {
                        SnapshotBootstrapState::WaitingFirstSnapshot => 1,
                        SnapshotBootstrapState::SnapshotRetrying { retry_count, .. } => {
                            retry_count.saturating_add(1)
                        }
                        SnapshotBootstrapState::Ready => unreachable!(),
                    };
                    let retry_in_ms = AppState::snapshot_retry_backoff_ms(retry_count);
                    state.snapshot_state = SnapshotBootstrapState::SnapshotRetrying {
                        last_error: error,
                        retry_count,
                        retry_in_ms,
                    };
                    state.ui.modal = None;
                    state.mark_dirty(
                        DirtyFlags {
                            ui: true,
                            ..DirtyFlags::default()
                        },
                        true,
                    );
                    vec![Effect::FetchSnapshotAfterDelay {
                        symbol,
                        generation,
                        retry_in_ms,
                    }]
                }
            }
        }
        EffectResultEvent::RiskEventsLoaded {
            symbol,
            generation,
            alerts,
        } => {
            if !current_instance_matches(state, &symbol, generation) {
                return Vec::new();
            }
            state.risk.alerts = merge_risk_alerts(&state.risk.alerts, alerts);
            state.risk.unacked_alerts = state
                .risk
                .alerts
                .iter()
                .filter(|alert| alert.acknowledged_at.is_none())
                .count() as u32;
            state.mark_dirty(
                DirtyFlags {
                    risk: true,
                    ..DirtyFlags::default()
                },
                true,
            );
            Vec::new()
        }
        EffectResultEvent::RiskEventsFailed {
            symbol,
            generation,
            error,
        } => {
            if !current_instance_matches(state, &symbol, generation) {
                return Vec::new();
            }
            let toast_copy = locale::copy(state.ui.locale).toast();
            state.ui.toast = Some(Toast {
                level: ToastLevel::Warning,
                message: toast_copy.risk_events_failed(&error),
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
        EffectResultEvent::WsConnected { symbol, generation } => {
            if !current_instance_matches(state, &symbol, generation) {
                return Vec::new();
            }
            let reconnect_attempt = state.connection.reconnect_attempt;
            state.connection.ws_connected = true;
            state.connection.reconnect_attempt = 0;
            state.connection.reconnect_backoff_ms = 0;
            let timestamp = state.local_timestamp();
            let copy = locale::copy(state.ui.locale);
            push_system_event(
                state,
                "info",
                "transport",
                copy.store().ws_connected_event(),
                timestamp,
            );
            state.ui.toast = Some(Toast {
                level: ToastLevel::Info,
                message: copy.toast().ws_connected().into(),
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
                vec![Effect::FetchSnapshot { symbol, generation }]
            } else {
                Vec::new()
            }
        }
        EffectResultEvent::WsDisconnected {
            symbol,
            generation,
            reason,
        } => {
            if !current_instance_matches(state, &symbol, generation) {
                return Vec::new();
            }
            state.connection.ws_connected = false;
            state.connection.reconnect_attempt += 1;
            let backoff_multiplier = 2u64
                .saturating_pow(state.connection.reconnect_attempt.saturating_sub(1))
                .min(8);
            state.connection.reconnect_backoff_ms = 1_000u64.saturating_mul(backoff_multiplier);
            let timestamp = state.local_timestamp();
            let copy = locale::copy(state.ui.locale);
            push_system_event(
                state,
                "warn",
                "transport",
                &copy.store().ws_disconnected_event(&reason),
                timestamp,
            );
            state.ui.toast = Some(Toast {
                level: ToastLevel::Warning,
                message: copy.toast().ws_disconnected(&reason),
                ttl_ticks: 24,
            });
            state.mark_dirty(DirtyFlags::all(), true);
            vec![Effect::ReconnectWs {
                symbol,
                generation,
                attempt: state.connection.reconnect_attempt,
            }]
        }
        EffectResultEvent::CommandAccepted {
            symbol,
            generation,
            accepted,
        } => {
            if !current_instance_matches(state, &symbol, generation) {
                return Vec::new();
            }
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
                locale::copy(state.ui.locale)
                    .store()
                    .command_accepted_summary()
                    .into(),
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
        EffectResultEvent::CommandFailed {
            symbol,
            generation,
            command_id,
            error,
        } => {
            if !current_instance_matches(state, &symbol, generation) {
                return Vec::new();
            }
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
                locale::copy(state.ui.locale)
                    .store()
                    .command_failed_summary(&error),
                timestamp,
                None,
            );
            state.ui.toast = Some(Toast {
                level: ToastLevel::Danger,
                message: locale::copy(state.ui.locale)
                    .store()
                    .command_failed_toast(&error),
                ttl_ticks: 24,
            });
            state.mark_dirty(DirtyFlags::all(), true);
            Vec::new()
        }
    }
}

fn current_symbol_matches(state: &AppState, symbol: &str) -> bool {
    state.instances.current_symbol.as_deref() == Some(symbol)
}

fn current_generation_matches(state: &AppState, generation: u64) -> bool {
    state.instances.generation == generation
}

fn current_instance_matches(state: &AppState, symbol: &str, generation: u64) -> bool {
    current_symbol_matches(state, symbol) && current_generation_matches(state, generation)
}

fn merge_risk_alerts(
    existing: &VecDeque<crate::protocol::RiskEvent>,
    loaded: Vec<crate::protocol::RiskEvent>,
) -> VecDeque<crate::protocol::RiskEvent> {
    let mut combined = loaded;
    combined.extend(existing.iter().cloned());
    combined.sort_by(|left, right| right.created_at.cmp(&left.created_at));

    let mut seen = HashSet::new();
    let mut merged = VecDeque::new();
    for alert in combined {
        let dedupe_key = (alert.code.clone(), alert.created_at.clone());
        if seen.insert(dedupe_key) {
            merged.push_back(alert);
        }
        if merged.len() == 20 {
            break;
        }
    }

    merged
}

fn bootstrap_blocked_message(state: &AppState) -> Option<&'static str> {
    let toast_copy = locale::copy(state.ui.locale).toast();
    match state.snapshot_state {
        SnapshotBootstrapState::WaitingFirstSnapshot => Some(toast_copy.snapshot_pending_blocked()),
        SnapshotBootstrapState::SnapshotRetrying { .. } => {
            Some(toast_copy.snapshot_retrying_blocked())
        }
        SnapshotBootstrapState::Ready => None,
    }
}

fn set_bootstrap_blocked_toast(state: &mut AppState) {
    if let Some(message) = bootstrap_blocked_message(state) {
        state.ui.modal = None;
        state.ui.toast = Some(Toast {
            level: ToastLevel::Warning,
            message: message.into(),
            ttl_ticks: 24,
        });
        state.mark_dirty(
            DirtyFlags {
                ui: true,
                ..DirtyFlags::default()
            },
            true,
        );
    }
}

fn handle_local_ui_event(state: &mut AppState, event: LocalUiEvent) -> Vec<Effect> {
    match event {
        LocalUiEvent::OpenConfirm(command) => {
            if bootstrap_blocked_message(state).is_some() {
                set_bootstrap_blocked_toast(state);
                return Vec::new();
            }
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
            if bootstrap_blocked_message(state).is_some() {
                state.ui.modal = None;
                set_bootstrap_blocked_toast(state);
                return Vec::new();
            }
            if let Some(Modal::Confirm(command)) = state.ui.modal.take() {
                submit_command(state, command)
            } else {
                Vec::new()
            }
        }
        LocalUiEvent::SelectInstance(symbol) => {
            if state.instances.current_symbol.as_deref() == Some(symbol.as_str()) {
                return Vec::new();
            }
            if !state
                .instances
                .items
                .iter()
                .any(|item| item.symbol == symbol)
            {
                return Vec::new();
            }
            state.begin_instance_bootstrap(symbol.clone());
            state.ui.pending_instance_switch = Some(symbol.clone());
            state.ui.toast = Some(Toast {
                level: ToastLevel::Info,
                message: locale::copy(state.ui.locale)
                    .toast()
                    .switching_instance(&symbol),
                ttl_ticks: 24,
            });
            let generation = state.instances.generation;
            vec![
                Effect::UseInstance {
                    symbol: symbol.clone(),
                    generation,
                },
                Effect::FetchSnapshot { symbol, generation },
            ]
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
    if bootstrap_blocked_message(state).is_some() {
        set_bootstrap_blocked_toast(state);
        return Vec::new();
    }
    let Some(symbol) = state.instances.current_symbol.clone() else {
        set_bootstrap_blocked_toast(state);
        return Vec::new();
    };
    let generation = state.instances.generation;
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
        symbol,
        generation,
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

fn cycle_instance(state: &mut AppState, delta: isize) -> Vec<Effect> {
    let count = state.instances.items.len();
    if count <= 1 {
        return Vec::new();
    }

    let current_index = state
        .instances
        .current_symbol
        .as_ref()
        .and_then(|symbol| {
            state
                .instances
                .items
                .iter()
                .position(|item| item.symbol == *symbol)
        })
        .unwrap_or(0);
    let next_index = (current_index as isize + delta).rem_euclid(count as isize) as usize;
    let next_symbol = state.instances.items[next_index].symbol.clone();
    handle_local_ui_event(state, LocalUiEvent::SelectInstance(next_symbol))
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
            locale::copy(state.ui.locale)
                .store()
                .command_timed_out_summary()
                .into(),
            timestamp.clone(),
            None,
        );
    }

    state.ui.toast = Some(Toast {
        level: ToastLevel::Warning,
        message: locale::copy(state.ui.locale)
            .store()
            .command_timed_out_toast()
            .into(),
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
    use crate::events::{
        AppEvent, EffectResultEvent, InputEvent, KeyAction, LocalUiEvent, ProtocolEvent,
    };
    use crate::locale::Locale;
    use crate::protocol::{
        CommandAccepted, CommandRecord, InstanceSummary, InstancesDirectory, RuntimeSnapshot,
    };
    use crate::state::SnapshotBootstrapState;

    #[test]
    fn startup_defaults_to_waiting_first_snapshot() {
        let state = AppState::waiting_first_snapshot();

        assert!(matches!(
            state.snapshot_state,
            SnapshotBootstrapState::WaitingFirstSnapshot
        ));
        assert!(state.runtime.symbol.is_empty());
        assert!(state.execution.open_orders.is_empty());
        assert!(state.execution.recent_fills.is_empty());
        assert!(state.risk.alerts.is_empty());
    }

    #[test]
    fn first_snapshot_failure_enters_retrying_state() {
        let mut state = AppState::waiting_first_snapshot();
        state.instances.current_symbol = Some("XAUUSDT".into());
        state.instances.generation = 1;

        let effects = reduce(
            &mut state,
            AppEvent::EffectResult(EffectResultEvent::SnapshotFailed {
                symbol: "XAUUSDT".into(),
                generation: 1,
                error: "boom".into(),
            }),
        );

        assert_eq!(
            effects,
            vec![Effect::FetchSnapshotAfterDelay {
                symbol: "XAUUSDT".into(),
                generation: 1,
                retry_in_ms: 1_000,
            }]
        );
        match state.snapshot_state {
            SnapshotBootstrapState::SnapshotRetrying {
                ref last_error,
                retry_count,
                retry_in_ms,
            } => {
                assert_eq!(last_error, "boom");
                assert_eq!(retry_count, 1);
                assert_eq!(retry_in_ms, 1_000);
            }
            other => panic!("unexpected snapshot state: {other:?}"),
        }
    }

    #[test]
    fn retrying_snapshot_success_returns_to_ready() {
        let mut state = AppState::waiting_first_snapshot();
        state.instances.current_symbol = Some("XAUUSDT".into());
        state.instances.generation = 1;
        reduce(
            &mut state,
            AppEvent::EffectResult(EffectResultEvent::SnapshotFailed {
                symbol: "XAUUSDT".into(),
                generation: 1,
                error: "boom".into(),
            }),
        );

        let effects = reduce(
            &mut state,
            AppEvent::EffectResult(EffectResultEvent::SnapshotLoaded {
                symbol: "XAUUSDT".into(),
                generation: 1,
                snapshot: RuntimeSnapshot::sample(),
            }),
        );

        assert!(matches!(
            state.snapshot_state,
            SnapshotBootstrapState::Ready
        ));
        assert_eq!(
            effects,
            vec![
                Effect::FetchRiskEvents {
                    symbol: "XAUUSDT".into(),
                    generation: 1,
                },
                Effect::ConnectWs {
                    symbol: "XAUUSDT".into(),
                    generation: 1,
                },
            ]
        );
    }

    #[test]
    fn instances_loaded_selects_default_symbol_and_bootstraps_snapshot() {
        let mut state = AppState::waiting_first_snapshot();

        let effects = reduce(
            &mut state,
            AppEvent::EffectResult(EffectResultEvent::InstancesLoaded(InstancesDirectory {
                environment: "testnet".into(),
                default_symbol: "BTCUSDT".into(),
                instances: vec![
                    InstanceSummary {
                        symbol: "BTCUSDT".into(),
                        environment: "testnet".into(),
                        is_default: true,
                    },
                    InstanceSummary {
                        symbol: "ETHUSDT".into(),
                        environment: "testnet".into(),
                        is_default: false,
                    },
                ],
            })),
        );

        assert_eq!(state.instances.current_symbol.as_deref(), Some("BTCUSDT"));
        assert_eq!(state.instances.environment, "testnet");
        assert_eq!(state.instances.generation, 1);
        assert_eq!(
            effects,
            vec![
                Effect::UseInstance {
                    symbol: "BTCUSDT".into(),
                    generation: 1,
                },
                Effect::FetchSnapshot {
                    symbol: "BTCUSDT".into(),
                    generation: 1,
                },
            ]
        );
    }

    #[test]
    fn selecting_instance_resets_runtime_and_requests_new_snapshot() {
        let mut state = AppState::sample();
        state.instances = crate::state::InstancesViewState::from_directory(InstancesDirectory {
            environment: "testnet".into(),
            default_symbol: "XAUUSDT".into(),
            instances: vec![
                InstanceSummary {
                    symbol: "XAUUSDT".into(),
                    environment: "testnet".into(),
                    is_default: true,
                },
                InstanceSummary {
                    symbol: "BTCUSDT".into(),
                    environment: "testnet".into(),
                    is_default: false,
                },
            ],
        });
        state.instances.current_symbol = Some("XAUUSDT".into());
        state
            .system_events
            .push_front(crate::protocol::SystemEvent {
                level: "info".into(),
                source: "test".into(),
                message: "old".into(),
                created_at: "T+00s".into(),
            });

        let effects = reduce(
            &mut state,
            AppEvent::LocalUi(LocalUiEvent::SelectInstance("BTCUSDT".into())),
        );

        assert_eq!(state.instances.current_symbol.as_deref(), Some("BTCUSDT"));
        assert_eq!(state.instances.generation, 1);
        assert!(matches!(
            state.snapshot_state,
            SnapshotBootstrapState::WaitingFirstSnapshot
        ));
        assert_eq!(state.runtime.symbol, "");
        assert!(state.execution.open_orders.is_empty());
        assert!(state.risk.alerts.is_empty());
        assert!(state.system_events.is_empty());
        assert!(!state.connection.ws_connected);
        assert_eq!(
            effects,
            vec![
                Effect::UseInstance {
                    symbol: "BTCUSDT".into(),
                    generation: 1,
                },
                Effect::FetchSnapshot {
                    symbol: "BTCUSDT".into(),
                    generation: 1,
                },
            ]
        );
    }

    #[test]
    fn instance_switch_shows_transition_toasts() {
        let mut state = AppState::sample();
        state.ui.locale = Locale::ZhCn;
        state.instances = crate::state::InstancesViewState::from_directory(InstancesDirectory {
            environment: "paper-testnet".into(),
            default_symbol: "XAUUSDT".into(),
            instances: vec![
                InstanceSummary {
                    symbol: "XAUUSDT".into(),
                    environment: "paper-testnet".into(),
                    is_default: true,
                },
                InstanceSummary {
                    symbol: "BTCUSDT".into(),
                    environment: "paper-testnet".into(),
                    is_default: false,
                },
            ],
        });
        state.instances.current_symbol = Some("XAUUSDT".into());

        let effects = reduce(
            &mut state,
            AppEvent::Input(InputEvent::Key(KeyAction::NextInstance)),
        );

        assert_eq!(
            state.ui.toast.as_ref().map(|toast| toast.message.as_str()),
            Some("正在切换到 BTCUSDT")
        );
        assert_eq!(
            effects,
            vec![
                Effect::UseInstance {
                    symbol: "BTCUSDT".into(),
                    generation: 1,
                },
                Effect::FetchSnapshot {
                    symbol: "BTCUSDT".into(),
                    generation: 1,
                },
            ]
        );

        let mut snapshot = RuntimeSnapshot::sample();
        snapshot.runtime.symbol = "BTCUSDT".into();
        snapshot.runtime.env = "paper-testnet".into();
        reduce(
            &mut state,
            AppEvent::EffectResult(EffectResultEvent::SnapshotLoaded {
                symbol: "BTCUSDT".into(),
                generation: 1,
                snapshot,
            }),
        );

        assert_eq!(
            state.ui.toast.as_ref().map(|toast| toast.message.as_str()),
            Some("已切换到 BTCUSDT")
        );
    }

    #[test]
    fn initial_snapshot_load_does_not_emit_switch_complete_toast() {
        let mut state = AppState::waiting_first_snapshot();
        state.instances.environment = "paper-testnet".into();
        state.instances.current_symbol = Some("XAUUSDT".into());

        let mut snapshot = RuntimeSnapshot::sample();
        snapshot.runtime.symbol = "XAUUSDT".into();
        snapshot.runtime.env = "paper-testnet".into();

        reduce(
            &mut state,
            AppEvent::EffectResult(EffectResultEvent::SnapshotLoaded {
                symbol: "XAUUSDT".into(),
                generation: 0,
                snapshot,
            }),
        );

        assert_eq!(state.ui.toast, None);
    }

    #[test]
    fn reconnect_snapshot_reload_keeps_transport_toast() {
        let mut state = AppState::sample();
        state.connection.ws_connected = false;
        state.connection.reconnect_attempt = 2;

        let effects = reduce(
            &mut state,
            AppEvent::EffectResult(EffectResultEvent::WsConnected {
                symbol: "XAUUSDT".into(),
                generation: 0,
            }),
        );

        assert_eq!(
            effects,
            vec![Effect::FetchSnapshot {
                symbol: "XAUUSDT".into(),
                generation: 0,
            }]
        );
        assert_eq!(
            state.ui.toast.as_ref().map(|toast| toast.message.as_str()),
            Some("ws connected")
        );

        let mut snapshot = RuntimeSnapshot::sample();
        snapshot.runtime.symbol = "XAUUSDT".into();
        snapshot.runtime.env = "testnet".into();
        reduce(
            &mut state,
            AppEvent::EffectResult(EffectResultEvent::SnapshotLoaded {
                symbol: "XAUUSDT".into(),
                generation: 0,
                snapshot,
            }),
        );

        assert_eq!(
            state.ui.toast.as_ref().map(|toast| toast.message.as_str()),
            Some("ws connected")
        );
    }

    #[test]
    fn next_instance_shortcut_cycles_symbols() {
        let mut state = AppState::sample();
        state.instances = crate::state::InstancesViewState::from_directory(InstancesDirectory {
            environment: "testnet".into(),
            default_symbol: "XAUUSDT".into(),
            instances: vec![
                InstanceSummary {
                    symbol: "XAUUSDT".into(),
                    environment: "testnet".into(),
                    is_default: true,
                },
                InstanceSummary {
                    symbol: "BTCUSDT".into(),
                    environment: "testnet".into(),
                    is_default: false,
                },
            ],
        });
        state.instances.current_symbol = Some("XAUUSDT".into());

        let effects = reduce(
            &mut state,
            AppEvent::Input(InputEvent::Key(KeyAction::NextInstance)),
        );

        assert_eq!(state.instances.current_symbol.as_deref(), Some("BTCUSDT"));
        assert_eq!(state.instances.generation, 1);
        assert_eq!(
            effects,
            vec![
                Effect::UseInstance {
                    symbol: "BTCUSDT".into(),
                    generation: 1,
                },
                Effect::FetchSnapshot {
                    symbol: "BTCUSDT".into(),
                    generation: 1,
                },
            ]
        );
    }

    #[test]
    fn ignores_stale_snapshot_for_non_current_symbol() {
        let mut state = AppState::waiting_first_snapshot();
        state.instances = crate::state::InstancesViewState::from_directory(InstancesDirectory {
            environment: "testnet".into(),
            default_symbol: "BTCUSDT".into(),
            instances: vec![
                InstanceSummary {
                    symbol: "BTCUSDT".into(),
                    environment: "testnet".into(),
                    is_default: true,
                },
                InstanceSummary {
                    symbol: "ETHUSDT".into(),
                    environment: "testnet".into(),
                    is_default: false,
                },
            ],
        });
        state.instances.current_symbol = Some("BTCUSDT".into());

        let effects = reduce(
            &mut state,
            AppEvent::EffectResult(EffectResultEvent::SnapshotLoaded {
                symbol: "ETHUSDT".into(),
                generation: 0,
                snapshot: RuntimeSnapshot::sample(),
            }),
        );

        assert!(effects.is_empty());
        assert_eq!(state.instances.current_symbol.as_deref(), Some("BTCUSDT"));
        assert_eq!(state.runtime.symbol, "");
        assert!(matches!(
            state.snapshot_state,
            SnapshotBootstrapState::WaitingFirstSnapshot
        ));
    }

    #[test]
    fn waiting_snapshot_blocks_runtime_shortcuts_and_toasts() {
        let mut state = AppState::waiting_first_snapshot();

        for action in [
            KeyAction::Pause,
            KeyAction::Resume,
            KeyAction::CancelAll,
            KeyAction::FlattenNow,
            KeyAction::ShutdownAfterFlatten,
        ] {
            let effects = reduce(&mut state, AppEvent::Input(InputEvent::Key(action)));
            assert!(effects.is_empty());
            assert_eq!(
                state.ui.toast.as_ref().map(|toast| toast.message.as_str()),
                Some("Initial snapshot pending. Runtime actions are disabled.")
            );
            state.ui.toast = None;
        }
    }

    #[test]
    fn retrying_snapshot_blocks_runtime_shortcuts_and_toasts() {
        let mut state = AppState::waiting_first_snapshot();
        state.instances.current_symbol = Some("XAUUSDT".into());
        state.instances.generation = 1;
        reduce(
            &mut state,
            AppEvent::EffectResult(EffectResultEvent::SnapshotFailed {
                symbol: "XAUUSDT".into(),
                generation: 1,
                error: "boom".into(),
            }),
        );

        for action in [
            KeyAction::Pause,
            KeyAction::Resume,
            KeyAction::CancelAll,
            KeyAction::FlattenNow,
            KeyAction::ShutdownAfterFlatten,
        ] {
            let effects = reduce(&mut state, AppEvent::Input(InputEvent::Key(action)));
            assert!(effects.is_empty());
            assert_eq!(
                state.ui.toast.as_ref().map(|toast| toast.message.as_str()),
                Some("Initial snapshot failed. Wait for retry before sending runtime actions.")
            );
            state.ui.toast = None;
        }
    }

    #[test]
    fn waiting_snapshot_blocks_runtime_shortcuts_and_toasts_in_chinese() {
        let mut state = AppState::waiting_first_snapshot();
        state.ui.locale = Locale::ZhCn;

        for action in [
            KeyAction::Pause,
            KeyAction::Resume,
            KeyAction::CancelAll,
            KeyAction::FlattenNow,
            KeyAction::ShutdownAfterFlatten,
        ] {
            let effects = reduce(&mut state, AppEvent::Input(InputEvent::Key(action)));
            assert!(effects.is_empty());
            assert_eq!(
                state.ui.toast.as_ref().map(|toast| toast.message.as_str()),
                Some("首个快照未就绪，运行时操作已禁用。")
            );
            state.ui.toast = None;
        }
    }

    #[test]
    fn retrying_snapshot_blocks_runtime_shortcuts_and_toasts_in_chinese() {
        let mut state = AppState::waiting_first_snapshot();
        state.ui.locale = Locale::ZhCn;
        state.instances.current_symbol = Some("XAUUSDT".into());
        state.instances.generation = 1;
        reduce(
            &mut state,
            AppEvent::EffectResult(EffectResultEvent::SnapshotFailed {
                symbol: "XAUUSDT".into(),
                generation: 1,
                error: "boom".into(),
            }),
        );

        for action in [
            KeyAction::Pause,
            KeyAction::Resume,
            KeyAction::CancelAll,
            KeyAction::FlattenNow,
            KeyAction::ShutdownAfterFlatten,
        ] {
            let effects = reduce(&mut state, AppEvent::Input(InputEvent::Key(action)));
            assert!(effects.is_empty());
            assert_eq!(
                state.ui.toast.as_ref().map(|toast| toast.message.as_str()),
                Some("首个快照获取失败，请等待重试后再发送运行时操作。")
            );
            state.ui.toast = None;
        }
    }

    #[test]
    fn pause_shortcut_creates_send_command_effect() {
        let mut state = AppState::sample();
        state.instances.generation = 7;
        let pending_before = state.execution.pending_commands.len();
        let effects = reduce(
            &mut state,
            AppEvent::Input(InputEvent::Key(KeyAction::Pause)),
        );
        assert!(matches!(
            effects.first(),
            Some(Effect::SendCommand {
                command: CommandType::Pause,
                generation: 7,
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
            AppEvent::EffectResult(EffectResultEvent::WsDisconnected {
                symbol: "XAUUSDT".into(),
                generation: 0,
                reason: "boom".into(),
            }),
        );
        assert!(matches!(
            effects.first(),
            Some(Effect::ReconnectWs {
                symbol,
                generation: 0,
                attempt: 1,
            }) if symbol == "XAUUSDT"
        ));
        assert!(!state.connection.ws_connected);
    }

    #[test]
    fn snapshot_failure_and_transport_toasts_follow_current_locale() {
        let mut state = AppState::sample();
        state.ui.locale = Locale::ZhCn;

        reduce(
            &mut state,
            AppEvent::EffectResult(EffectResultEvent::SnapshotFailed {
                symbol: "XAUUSDT".into(),
                generation: 0,
                error: "boom".into(),
            }),
        );
        assert_eq!(
            state.ui.toast.as_ref().map(|toast| toast.message.as_str()),
            Some("快照获取失败：boom")
        );

        reduce(
            &mut state,
            AppEvent::EffectResult(EffectResultEvent::WsConnected {
                symbol: "XAUUSDT".into(),
                generation: 0,
            }),
        );
        assert_eq!(
            state
                .system_events
                .front()
                .map(|event| event.message.as_str()),
            Some("WebSocket 已连接，开始流式传输。")
        );
        assert_eq!(
            state.ui.toast.as_ref().map(|toast| toast.message.as_str()),
            Some("WebSocket 已连接")
        );

        reduce(
            &mut state,
            AppEvent::EffectResult(EffectResultEvent::WsDisconnected {
                symbol: "XAUUSDT".into(),
                generation: 0,
                reason: "boom".into(),
            }),
        );
        assert_eq!(
            state
                .system_events
                .front()
                .map(|event| event.message.as_str()),
            Some("WebSocket 已断开：boom")
        );
        assert_eq!(
            state.ui.toast.as_ref().map(|toast| toast.message.as_str()),
            Some("WebSocket 已断开：boom")
        );
    }

    #[test]
    fn command_failures_and_timeouts_follow_current_locale() {
        let mut state = AppState::sample();
        state.ui.locale = Locale::ZhCn;

        reduce(
            &mut state,
            AppEvent::EffectResult(EffectResultEvent::CommandFailed {
                symbol: "XAUUSDT".into(),
                generation: 0,
                command_id: "cmd_fail_zh".into(),
                error: "boom".into(),
            }),
        );
        assert_eq!(
            state.ui.toast.as_ref().map(|toast| toast.message.as_str()),
            Some("命令失败：boom")
        );
        assert!(state.execution.command_timeline.iter().any(|entry| {
            entry.command_id == "cmd_fail_zh" && entry.summary == "命令在收到服务端确认前失败：boom"
        }));

        let effects = reduce(
            &mut state,
            AppEvent::Input(InputEvent::Key(KeyAction::Pause)),
        );
        let command_id = match effects.first().expect("send command") {
            Effect::SendCommand { command_id, .. } => command_id.clone(),
            other => panic!("unexpected effect: {other:?}"),
        };
        reduce(
            &mut state,
            AppEvent::EffectResult(EffectResultEvent::CommandAccepted {
                symbol: "XAUUSDT".into(),
                generation: 0,
                accepted: CommandAccepted {
                    version: "v1alpha1".into(),
                    command_id: command_id.clone(),
                    command: CommandType::Pause,
                    status: CommandStatus::Accepted,
                    accepted_at: "2025-01-01T00:00:01Z".into(),
                },
            }),
        );
        for _ in 0..COMMAND_TIMEOUT_TICKS {
            reduce(&mut state, AppEvent::System(SystemEvent::HealthTick));
        }
        assert_eq!(
            state.ui.toast.as_ref().map(|toast| toast.message.as_str()),
            Some("一条或多条命令已超时")
        );
        assert!(state.execution.command_timeline.iter().any(|entry| {
            entry.command_id == command_id && entry.summary == "在超时窗口内没有收到最终确认。"
        }));
    }

    #[test]
    fn toggle_locale_updates_ui_state_without_changing_page() {
        let mut state = AppState::sample();
        state.ui.page = Page::Grid;
        state.ui.focus_index = 2;
        state.ui.modal = Some(Modal::Confirm(CommandType::Pause));
        let pending_command_id = state.queue_command(CommandType::Resume);
        state.dirty.clear();
        state.immediate_render = false;

        let page_before = state.ui.page;
        let focus_before = state.ui.focus_index;
        let modal_before = state.ui.modal.clone();
        let pending_before = state.execution.pending_commands.clone();
        let timeline_before = state.execution.command_timeline.clone();

        let effects = reduce(
            &mut state,
            AppEvent::Input(InputEvent::Key(KeyAction::ToggleLocale)),
        );

        assert!(effects.is_empty());
        assert_eq!(state.ui.page, page_before);
        assert_eq!(state.ui.focus_index, focus_before);
        assert_eq!(state.ui.modal, modal_before);
        assert_eq!(state.ui.locale, Locale::ZhCn);
        assert_eq!(state.execution.pending_commands, pending_before);
        assert_eq!(state.execution.command_timeline, timeline_before);
        assert!(state.dirty.ui);
        assert!(state.take_immediate_render());
        assert!(
            state
                .execution
                .pending_commands
                .iter()
                .any(|command| command.command_id == pending_command_id)
        );
    }

    #[test]
    fn websocket_reconnect_triggers_snapshot_refetch() {
        let mut state = AppState::sample();
        state.connection.ws_connected = false;
        state.connection.reconnect_attempt = 1;

        let effects = reduce(
            &mut state,
            AppEvent::EffectResult(EffectResultEvent::WsConnected {
                symbol: "XAUUSDT".into(),
                generation: 0,
            }),
        );

        assert_eq!(
            effects,
            vec![Effect::FetchSnapshot {
                symbol: "XAUUSDT".into(),
                generation: 0,
            }]
        );
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
        state.instances.current_symbol = Some("TSLAUSDT".into());
        let effects = reduce(
            &mut state,
            AppEvent::EffectResult(EffectResultEvent::SnapshotLoaded {
                symbol: "TSLAUSDT".into(),
                generation: 0,
                snapshot,
            }),
        );
        assert_eq!(state.runtime.symbol, "TSLAUSDT");
        assert_eq!(
            effects,
            vec![
                Effect::FetchRiskEvents {
                    symbol: "TSLAUSDT".into(),
                    generation: 0,
                },
                Effect::ConnectWs {
                    symbol: "TSLAUSDT".into(),
                    generation: 0,
                },
            ]
        );
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
            AppEvent::Protocol(ProtocolEvent::from(ServerEvent::RuntimeSnapshot(snapshot))),
        );

        assert!(state.connection.ws_connected);
        assert_eq!(state.runtime.symbol, "TSLAUSDT");
        assert_eq!(state.ui.page, Page::Events);
    }

    #[test]
    fn snapshot_loaded_while_connected_requests_risk_events_reload() {
        let mut state = AppState::sample();
        state.connection.ws_connected = true;

        let effects = reduce(
            &mut state,
            AppEvent::EffectResult(EffectResultEvent::SnapshotLoaded {
                symbol: "XAUUSDT".into(),
                generation: 0,
                snapshot: RuntimeSnapshot::sample(),
            }),
        );

        assert_eq!(
            effects,
            vec![Effect::FetchRiskEvents {
                symbol: "XAUUSDT".into(),
                generation: 0,
            }]
        );
    }

    #[test]
    fn risk_events_loaded_merge_live_and_recovered_alerts() {
        let mut state = AppState::sample();
        state.risk.alerts.clear();
        state.risk.alerts.push_back(crate::protocol::RiskEvent {
            severity: crate::protocol::RiskLevel::Danger,
            code: "STOP_LOSS_TRIGGERED".into(),
            message: "live".into(),
            created_at: "2025-01-01T00:00:06Z".into(),
            acknowledged_at: None,
        });

        let alerts = vec![crate::protocol::RiskEvent {
            severity: crate::protocol::RiskLevel::Watch,
            code: "DAILY_LOSS_LIMIT_BREACHED".into(),
            message: "recovered".into(),
            created_at: "2025-01-01T00:00:05Z".into(),
            acknowledged_at: None,
        }];

        reduce(
            &mut state,
            AppEvent::EffectResult(EffectResultEvent::RiskEventsLoaded {
                symbol: "XAUUSDT".into(),
                generation: 0,
                alerts,
            }),
        );

        assert_eq!(state.risk.alerts.len(), 2);
        assert_eq!(state.risk.alerts[0].code, "STOP_LOSS_TRIGGERED");
        assert_eq!(state.risk.alerts[1].code, "DAILY_LOSS_LIMIT_BREACHED");
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
            AppEvent::Protocol(ProtocolEvent::from(ServerEvent::RuntimeSnapshot(snapshot))),
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
            AppEvent::EffectResult(EffectResultEvent::CommandAccepted {
                symbol: "XAUUSDT".into(),
                generation: 0,
                accepted: CommandAccepted {
                    version: "v1alpha1".into(),
                    command_id: command_id.clone(),
                    command: CommandType::Pause,
                    status: CommandStatus::Accepted,
                    accepted_at: "2025-01-01T00:00:00Z".into(),
                },
            }),
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
    fn stale_same_symbol_command_result_from_previous_generation_is_ignored() {
        let mut state = AppState::sample();
        state.instances.environment = "testnet".into();
        state.instances.default_symbol = Some("XAUUSDT".into());
        state.instances.current_symbol = Some("XAUUSDT".into());
        state.instances.generation = 1;
        state.instances.items = vec![
            InstanceSummary {
                symbol: "XAUUSDT".into(),
                environment: "testnet".into(),
                is_default: true,
            },
            InstanceSummary {
                symbol: "BTCUSDT".into(),
                environment: "testnet".into(),
                is_default: false,
            },
        ];

        let effects = reduce(
            &mut state,
            AppEvent::Input(InputEvent::Key(KeyAction::Pause)),
        );
        let stale_command_id = match effects.first().unwrap() {
            Effect::SendCommand {
                symbol,
                command,
                generation,
                command_id,
            } => {
                assert_eq!(symbol, "XAUUSDT");
                assert_eq!(*command, CommandType::Pause);
                assert_eq!(*generation, 1);
                command_id.clone()
            }
            other => panic!("unexpected effect: {other:?}"),
        };

        let _ = reduce(
            &mut state,
            AppEvent::LocalUi(LocalUiEvent::SelectInstance("BTCUSDT".into())),
        );
        let _ = reduce(
            &mut state,
            AppEvent::LocalUi(LocalUiEvent::SelectInstance("XAUUSDT".into())),
        );
        reduce(
            &mut state,
            AppEvent::EffectResult(EffectResultEvent::SnapshotLoaded {
                symbol: "XAUUSDT".into(),
                generation: 3,
                snapshot: RuntimeSnapshot::sample(),
            }),
        );
        let current_effects = reduce(
            &mut state,
            AppEvent::Input(InputEvent::Key(KeyAction::Pause)),
        );
        let current_command_id = match current_effects.first().unwrap() {
            Effect::SendCommand {
                symbol,
                command,
                generation,
                command_id,
            } => {
                assert_eq!(symbol, "XAUUSDT");
                assert_eq!(*command, CommandType::Pause);
                assert_eq!(*generation, 3);
                command_id.clone()
            }
            other => panic!("unexpected effect: {other:?}"),
        };

        reduce(
            &mut state,
            AppEvent::EffectResult(EffectResultEvent::CommandAccepted {
                symbol: "XAUUSDT".into(),
                generation: 1,
                accepted: CommandAccepted {
                    version: "v1alpha1".into(),
                    command_id: stale_command_id.clone(),
                    command: CommandType::Pause,
                    status: CommandStatus::Accepted,
                    accepted_at: "2025-01-01T00:00:00Z".into(),
                },
            }),
        );

        assert_eq!(state.instances.current_symbol.as_deref(), Some("XAUUSDT"));
        assert_eq!(state.instances.generation, 3);
        assert_eq!(
            state
                .execution
                .pending_commands
                .iter()
                .find(|item| item.command_id == current_command_id)
                .unwrap()
                .status,
            CommandStatus::Pending
        );
        assert_eq!(
            state
                .execution
                .command_timeline
                .iter()
                .find(|item| item.command_id == current_command_id)
                .unwrap()
                .stage,
            CommandTimelineStage::Pending
        );
        assert!(
            state
                .execution
                .command_timeline
                .iter()
                .all(|item| item.command_id != stale_command_id)
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
            AppEvent::Protocol(ProtocolEvent::from(ServerEvent::CommandAck(CommandAck {
                command_id: command_id.clone(),
                command: CommandType::Pause,
                status: CommandStatus::Completed,
                message: "Paused.".into(),
                links: crate::protocol::CommandLinks::default(),
                emitted_at: "2025-01-01T00:00:02Z".into(),
            }))),
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
            AppEvent::EffectResult(EffectResultEvent::CommandAccepted {
                symbol: "XAUUSDT".into(),
                generation: 0,
                accepted: CommandAccepted {
                    version: "v1alpha1".into(),
                    command_id: command_id.clone(),
                    command: CommandType::Pause,
                    status: CommandStatus::Accepted,
                    accepted_at: "2025-01-01T00:00:01Z".into(),
                },
            }),
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
            AppEvent::Protocol(ProtocolEvent::from(ServerEvent::CommandAck(CommandAck {
                command_id: "cmd_failed_level".into(),
                command: CommandType::CancelAll,
                status: CommandStatus::Failed,
                message: "exchange rejected cancel-all".into(),
                links: crate::protocol::CommandLinks::default(),
                emitted_at: "2025-01-01T00:00:02Z".into(),
            }))),
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
            AppEvent::Protocol(ProtocolEvent::from(ServerEvent::CommandAck(CommandAck {
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
            }))),
        );

        assert!(state.execution.open_orders.is_empty());
        assert_eq!(state.runtime.strategy_state, "paused");
    }
}
