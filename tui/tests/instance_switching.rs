use grid_platform_tui::{
    effects::Effect,
    events::{AppEvent, EffectResultEvent, LocalUiEvent, ProtocolEvent},
    protocol::{InstanceSummary, InstancesDirectory, RuntimeSnapshot, ServerEvent},
    state::{AppState, SnapshotBootstrapState},
    store::reduce,
};

fn sample_directory() -> InstancesDirectory {
    InstancesDirectory {
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
    }
}

#[test]
fn loading_instances_should_select_the_default_symbol_first() {
    let mut state = AppState::waiting_first_snapshot();

    let effects = reduce(
        &mut state,
        AppEvent::EffectResult(EffectResultEvent::InstancesLoaded(sample_directory())),
    );

    assert!(matches!(
        state.snapshot_state,
        SnapshotBootstrapState::WaitingFirstSnapshot
    ));
    assert_eq!(state.instances.environment, "testnet");
    assert_eq!(state.instances.default_symbol.as_deref(), Some("BTCUSDT"));
    assert_eq!(state.instances.current_symbol.as_deref(), Some("BTCUSDT"));
    assert_eq!(state.instances.generation, 1);
    assert_eq!(state.instances.items.len(), 2);
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
fn selecting_a_different_instance_should_request_a_symbol_switch() {
    let mut state = AppState::waiting_first_snapshot();
    let _ = reduce(
        &mut state,
        AppEvent::EffectResult(EffectResultEvent::InstancesLoaded(sample_directory())),
    );

    let effects = reduce(
        &mut state,
        AppEvent::LocalUi(LocalUiEvent::SelectInstance("ETHUSDT".into())),
    );

    assert_eq!(state.instances.current_symbol.as_deref(), Some("ETHUSDT"));
    assert_eq!(state.instances.generation, 2);
    assert_eq!(
        effects,
        vec![
            Effect::UseInstance {
                symbol: "ETHUSDT".into(),
                generation: 2,
            },
            Effect::FetchSnapshot {
                symbol: "ETHUSDT".into(),
                generation: 2,
            },
        ]
    );
}

#[test]
fn stale_protocol_events_from_previous_instance_should_be_ignored() {
    let mut state = AppState::waiting_first_snapshot();
    let _ = reduce(
        &mut state,
        AppEvent::EffectResult(EffectResultEvent::InstancesLoaded(sample_directory())),
    );
    let _ = reduce(
        &mut state,
        AppEvent::LocalUi(LocalUiEvent::SelectInstance("ETHUSDT".into())),
    );

    let mut eth_snapshot = RuntimeSnapshot::sample();
    eth_snapshot.runtime.symbol = "ETHUSDT".into();
    eth_snapshot.runtime.env = "testnet".into();
    eth_snapshot.runtime.last_price = 2500.0;
    eth_snapshot.runtime.mark_price = 2500.0;
    let _ = reduce(
        &mut state,
        AppEvent::EffectResult(EffectResultEvent::SnapshotLoaded {
            symbol: "ETHUSDT".into(),
            generation: 2,
            snapshot: eth_snapshot,
        }),
    );

    let mut stale_snapshot = RuntimeSnapshot::sample();
    stale_snapshot.runtime.symbol = "BTCUSDT".into();
    stale_snapshot.runtime.env = "testnet".into();
    stale_snapshot.runtime.last_price = 95000.0;
    stale_snapshot.runtime.mark_price = 95000.0;

    let _ = reduce(
        &mut state,
        AppEvent::Protocol(ProtocolEvent {
            symbol: Some("BTCUSDT".into()),
            generation: Some(1),
            event: ServerEvent::RuntimeSnapshot(stale_snapshot),
        }),
    );

    assert_eq!(state.instances.current_symbol.as_deref(), Some("ETHUSDT"));
    assert_eq!(state.runtime.symbol, "ETHUSDT");
    assert_eq!(state.runtime.last_price, 2500.0);
    assert_eq!(state.runtime.mark_price, 2500.0);
}

#[test]
fn stale_same_symbol_snapshot_from_previous_generation_should_be_ignored() {
    let mut state = AppState::waiting_first_snapshot();
    let _ = reduce(
        &mut state,
        AppEvent::EffectResult(EffectResultEvent::InstancesLoaded(sample_directory())),
    );
    let _ = reduce(
        &mut state,
        AppEvent::LocalUi(LocalUiEvent::SelectInstance("ETHUSDT".into())),
    );
    let _ = reduce(
        &mut state,
        AppEvent::LocalUi(LocalUiEvent::SelectInstance("BTCUSDT".into())),
    );

    let mut current_snapshot = RuntimeSnapshot::sample();
    current_snapshot.runtime.symbol = "BTCUSDT".into();
    current_snapshot.runtime.env = "testnet".into();
    current_snapshot.runtime.last_price = 101.0;
    current_snapshot.runtime.mark_price = 101.0;
    let _ = reduce(
        &mut state,
        AppEvent::EffectResult(EffectResultEvent::SnapshotLoaded {
            symbol: "BTCUSDT".into(),
            generation: 3,
            snapshot: current_snapshot,
        }),
    );

    let mut stale_snapshot = RuntimeSnapshot::sample();
    stale_snapshot.runtime.symbol = "BTCUSDT".into();
    stale_snapshot.runtime.env = "testnet".into();
    stale_snapshot.runtime.last_price = 95.0;
    stale_snapshot.runtime.mark_price = 95.0;
    let _ = reduce(
        &mut state,
        AppEvent::EffectResult(EffectResultEvent::SnapshotLoaded {
            symbol: "BTCUSDT".into(),
            generation: 1,
            snapshot: stale_snapshot,
        }),
    );

    assert_eq!(state.instances.current_symbol.as_deref(), Some("BTCUSDT"));
    assert_eq!(state.instances.generation, 3);
    assert_eq!(state.runtime.last_price, 101.0);
    assert_eq!(state.runtime.mark_price, 101.0);
}
