use std::{collections::BTreeMap, fs, path::Path};

use grid_platform_tui::{
    locale::{self, Locale},
    protocol::{CommandType, GridLevelState, StrategyStatus},
    selectors::PlacementState,
    state::CommandTimelineStage,
};

#[test]
fn all_supported_locales_define_non_empty_runtime_copy_samples() {
    let english = runtime_copy_samples(Locale::EnUs);
    let chinese = runtime_copy_samples(Locale::ZhCn);

    assert_eq!(
        english.keys().collect::<Vec<_>>(),
        chinese.keys().collect::<Vec<_>>()
    );

    for (key, value) in &english {
        assert!(!value.trim().is_empty(), "empty English copy for key {key}");
    }
    for (key, value) in &chinese {
        assert!(!value.trim().is_empty(), "empty Chinese copy for key {key}");
    }

    for key in [
        "tabs.dashboard",
        "dashboard.exchange_orders_title",
        "footer.ready",
        "toast.snapshot_failed",
        "selector.risk_action_hint",
    ] {
        assert_ne!(
            english[key], chinese[key],
            "expected localized value for key {key}"
        );
    }
}

#[test]
fn localizable_runtime_strings_live_in_locale_module() {
    let crate_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let files = [
        "src/render.rs",
        "src/selectors.rs",
        "src/state.rs",
        "src/store.rs",
    ];
    let forbidden_phrases = [
        "snapshot failed:",
        "risk events failed:",
        "ws connected",
        "ws disconnected:",
        "Initial snapshot pending. Runtime actions are disabled.",
        "Initial snapshot failed. Wait for retry before sending runtime actions.",
        "WebSocket connected and streaming.",
        "WebSocket disconnected:",
        "Service accepted command; waiting for final acknowledgement.",
        "Command request failed before ack:",
        "command failed:",
        "No final acknowledgement arrived within the timeout window.",
        "one or more commands timed out",
        "Waiting for service acceptance.",
        "Recovered pending command from snapshot.",
        "Service accepted command before the client reconnected.",
        "Command already acknowledged before the client reconnected.",
        "Command already failed before the client reconnected.",
        "Command timed out before the client reconnected.",
        "SERVICE RECONNECTING",
        "MARKET RECONNECTING",
        "long inventory",
        "short inventory",
        "flat inventory",
        "Reduce exposure before resuming the grid.",
        "CANCEL ALL",
    ];
    let mut violations = Vec::new();

    for relative_path in files {
        let path = crate_root.join(relative_path);
        let content = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
        let runtime_content = strip_test_module(&content);

        for phrase in forbidden_phrases {
            if runtime_content.contains(phrase) {
                violations.push(format!(
                    "{relative_path}: found localizable phrase {phrase:?}"
                ));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "runtime copy must be centralized in src/locale.rs:\n{}",
        violations.join("\n")
    );
}

fn runtime_copy_samples(locale: Locale) -> BTreeMap<&'static str, String> {
    let copy = locale::copy(locale);
    let (modal_title, modal_detail, modal_risk) = copy.modal().confirm(CommandType::Pause);

    BTreeMap::from([
        ("tabs.dashboard", copy.tabs()[0].to_string()),
        (
            "status.waiting_snapshot_badge",
            copy.status().waiting_snapshot_badge().to_string(),
        ),
        (
            "bootstrap.pending_title",
            copy.bootstrap().pending_title().to_string(),
        ),
        (
            "dashboard.exchange_orders_title",
            copy.dashboard().exchange_orders_title().to_string(),
        ),
        (
            "grid.strategy_orders_title",
            copy.grid().strategy_orders_title().to_string(),
        ),
        (
            "market.connectivity_title",
            copy.market().connectivity_title().to_string(),
        ),
        ("events.commands_title", copy.events().commands_title(2)),
        (
            "help.shortcuts_title",
            copy.help().shortcuts_title().to_string(),
        ),
        (
            "footer.ready",
            copy.footer().ready(false, "Dashboard/Execution"),
        ),
        (
            "toast.snapshot_failed",
            copy.toast().snapshot_failed("boom"),
        ),
        (
            "toast.snapshot_retrying_blocked",
            copy.toast().snapshot_retrying_blocked().to_string(),
        ),
        (
            "store.command_pending_summary",
            copy.store().command_pending_summary().to_string(),
        ),
        (
            "store.recovered_pending_summary",
            copy.store().recovered_pending_summary().to_string(),
        ),
        (
            "store.recovered_accepted_summary",
            copy.store().recovered_accepted_summary().to_string(),
        ),
        (
            "store.recovered_ack_summary",
            copy.store().recovered_ack_summary().to_string(),
        ),
        (
            "store.recovered_failed_summary",
            copy.store().recovered_failed_summary().to_string(),
        ),
        (
            "store.recovered_timed_out_summary",
            copy.store().recovered_timed_out_summary().to_string(),
        ),
        ("modal.confirm.title", modal_title.to_string()),
        ("modal.confirm.detail", modal_detail.to_string()),
        ("modal.confirm.risk", modal_risk.to_string()),
        (
            "common.strategy_state",
            copy.common().strategy_state_label("ACTIVE"),
        ),
        (
            "common.placement_state",
            copy.common()
                .placement_state_label(PlacementState::Live)
                .to_string(),
        ),
        (
            "selector.risk_action_hint",
            copy.selector()
                .risk_action_hint("STOP_LOSS_TRIGGERED")
                .to_string(),
        ),
        (
            "selector.command_label",
            copy.selector()
                .command_label(CommandType::Pause)
                .to_string(),
        ),
        (
            "selector.stage_label",
            copy.selector()
                .stage_label(CommandTimelineStage::TimedOut)
                .to_string(),
        ),
        (
            "selector.command_timing",
            copy.selector()
                .command_timing("req", Some("acc"), Some("end")),
        ),
        (
            "selector.strategy_status",
            copy.selector()
                .strategy_status_label(StrategyStatus::WaitingRangeEntry)
                .to_string(),
        ),
        (
            "selector.grid_level_state",
            copy.selector()
                .grid_level_state_label(GridLevelState::Occupied)
                .to_string(),
        ),
    ])
}

fn strip_test_module(content: &str) -> &str {
    content.split("\n#[cfg(test)]").next().unwrap_or(content)
}
