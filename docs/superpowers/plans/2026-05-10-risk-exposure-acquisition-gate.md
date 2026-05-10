# Risk Exposure Acquisition Gate Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a configurable risk exposure acquisition gate so automatic risk increases require price advantage while risk reductions stay responsive.

**Architecture:** Keep curve math in `poise_core::strategy`; add a focused engine-owned gate that converts `curve_target` into `allowed_target` and a single next-release maker budget. Reconciler owns target filtering and runtime state, while executor consumes the filtered target plus optional maker budget to restrict `CatchUp` and `CurveMaker`.

**Tech Stack:** Rust workspace crates (`core`, `engine`, `server`, `protocol`, `application`, `tui`), Tauri/Rust config tooling, React/TypeScript workbench, Cargo tests, Vitest.

---

## Execution Rules

- Execute tasks in order.
- For each task: write the failing test first, run the listed focused test and confirm failure, implement only that task, rerun the focused test, then commit.
- After each task commit, update this plan checkbox and write the commit SHA on that task.
- If implementation conflicts with `docs/superpowers/specs/2026-05-10-risk-exposure-acquisition-gate-design.md`, stop and ask for confirmation before editing code or changing this plan.
- If a needed behavior is missing from both spec and plan, stop and ask for confirmation before adding it.

## File Structure

- `core/src/strategy.rs`: Owns `RiskIncreaseDelayConfig`, default values, serde shape, and validation.
- `server/src/config.rs`: Accepts TOML `risk_increase_delay` in `TrackSpec` and passes it into `TrackConfig`.
- `engine/src/risk_exposure_gate.rs`: New focused pure module for startup allocation, backlog release, anchor updates, and next maker budget.
- `engine/src/runtime.rs`: Stores gate state inside automatic runtime state.
- `engine/src/reconciler.rs`: Applies gate after risk cap and before account capacity / executor planning.
- `engine/src/manager.rs`: Carries reconciler maker budget into executor input.
- `engine/src/executor/planning.rs` and `engine/src/executor/policy.rs`: Restrict `CatchUp` to `allowed_target`; plan one next-release `CurveMaker` at the advantage price.
- `protocol/src/lib.rs`, `application/src/read_model.rs`, `server/src/projector.rs`, `tui/src/views/instance.rs`: Expose and display risk increase delay config.
- `tools/track-tuning-workbench/src-tauri/src/config_document.rs`, `tools/track-tuning-workbench/src-tauri/src/commands.rs`, `tools/track-tuning-workbench/src/domain/trackDraft.ts`, `tools/track-tuning-workbench/src/app/workbenchBridge.ts`, `tools/track-tuning-workbench/src/ui/editor/sections/RiskIncreaseDelaySection.tsx`: Preserve and edit the new config in workbench.

## Task 1: Core Config And Server TOML Parsing

**Files:**
- Modify: `core/src/strategy.rs`
- Modify: `server/src/config.rs`
- Test: `core/src/strategy.rs`
- Test: `server/src/config.rs`

- [ ] **Step 1: Write core config tests**

Add these tests to `core/src/strategy.rs` inside `#[cfg(test)] mod tests`:

```rust
#[test]
fn validate_accepts_risk_increase_delay_config() {
    let mut config = neutral_config();
    config.risk_increase_delay = Some(RiskIncreaseDelayConfig {
        startup_initial_ratio: 0.3,
        advantage_min_rebalance_multiples: 2.0,
        base_step_min_rebalance_multiples: 1.0,
        max_step_min_rebalance_multiples: 4.0,
        catchup_ratio: 0.25,
    });

    assert_eq!(validate_config(&config), Ok(()));
}

#[test]
fn validate_rejects_invalid_risk_increase_delay_config() {
    let mut config = neutral_config();
    config.risk_increase_delay = Some(RiskIncreaseDelayConfig {
        startup_initial_ratio: 1.2,
        advantage_min_rebalance_multiples: 2.0,
        base_step_min_rebalance_multiples: 1.0,
        max_step_min_rebalance_multiples: 4.0,
        catchup_ratio: 0.25,
    });

    let error = validate_config(&config).unwrap_err();

    assert!(error.contains("startup_initial_ratio"));
}

#[test]
fn validate_rejects_step_bounds_that_cannot_release() {
    let mut config = neutral_config();
    config.risk_increase_delay = Some(RiskIncreaseDelayConfig {
        startup_initial_ratio: 0.3,
        advantage_min_rebalance_multiples: 2.0,
        base_step_min_rebalance_multiples: 5.0,
        max_step_min_rebalance_multiples: 4.0,
        catchup_ratio: 0.25,
    });

    let error = validate_config(&config).unwrap_err();

    assert!(error.contains("max_step_min_rebalance_multiples"));
}
```

- [ ] **Step 2: Run core test to verify it fails**

Run:

```bash
cargo test -p poise-core strategy::tests::validate_accepts_risk_increase_delay_config
cargo test -p poise-core strategy::tests::validate_rejects_invalid_risk_increase_delay_config
cargo test -p poise-core strategy::tests::validate_rejects_step_bounds_that_cannot_release
```

Expected: FAIL because `RiskIncreaseDelayConfig` and `TrackConfig::risk_increase_delay` do not exist.

- [ ] **Step 3: Implement core config type and validation**

In `core/src/strategy.rs`, add the config struct near `TrackConfig`:

```rust
pub const DEFAULT_RISK_INCREASE_STARTUP_INITIAL_RATIO: f64 = 0.3;
pub const DEFAULT_RISK_INCREASE_ADVANTAGE_MIN_REBALANCE_MULTIPLES: f64 = 2.0;
pub const DEFAULT_RISK_INCREASE_BASE_STEP_MIN_REBALANCE_MULTIPLES: f64 = 1.0;
pub const DEFAULT_RISK_INCREASE_MAX_STEP_MIN_REBALANCE_MULTIPLES: f64 = 4.0;
pub const DEFAULT_RISK_INCREASE_CATCHUP_RATIO: f64 = 0.25;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RiskIncreaseDelayConfig {
    #[serde(default = "default_risk_increase_startup_initial_ratio")]
    pub startup_initial_ratio: f64,
    #[serde(default = "default_risk_increase_advantage_min_rebalance_multiples")]
    pub advantage_min_rebalance_multiples: f64,
    #[serde(default = "default_risk_increase_base_step_min_rebalance_multiples")]
    pub base_step_min_rebalance_multiples: f64,
    #[serde(default = "default_risk_increase_max_step_min_rebalance_multiples")]
    pub max_step_min_rebalance_multiples: f64,
    #[serde(default = "default_risk_increase_catchup_ratio")]
    pub catchup_ratio: f64,
}

impl Default for RiskIncreaseDelayConfig {
    fn default() -> Self {
        Self {
            startup_initial_ratio: DEFAULT_RISK_INCREASE_STARTUP_INITIAL_RATIO,
            advantage_min_rebalance_multiples:
                DEFAULT_RISK_INCREASE_ADVANTAGE_MIN_REBALANCE_MULTIPLES,
            base_step_min_rebalance_multiples:
                DEFAULT_RISK_INCREASE_BASE_STEP_MIN_REBALANCE_MULTIPLES,
            max_step_min_rebalance_multiples:
                DEFAULT_RISK_INCREASE_MAX_STEP_MIN_REBALANCE_MULTIPLES,
            catchup_ratio: DEFAULT_RISK_INCREASE_CATCHUP_RATIO,
        }
    }
}
```

Add `#[serde(default)] pub risk_increase_delay: Option<RiskIncreaseDelayConfig>,` to `TrackConfig`.

Add these default helpers near `default_min_rebalance_units()`:

```rust
fn default_risk_increase_startup_initial_ratio() -> f64 {
    DEFAULT_RISK_INCREASE_STARTUP_INITIAL_RATIO
}

fn default_risk_increase_advantage_min_rebalance_multiples() -> f64 {
    DEFAULT_RISK_INCREASE_ADVANTAGE_MIN_REBALANCE_MULTIPLES
}

fn default_risk_increase_base_step_min_rebalance_multiples() -> f64 {
    DEFAULT_RISK_INCREASE_BASE_STEP_MIN_REBALANCE_MULTIPLES
}

fn default_risk_increase_max_step_min_rebalance_multiples() -> f64 {
    DEFAULT_RISK_INCREASE_MAX_STEP_MIN_REBALANCE_MULTIPLES
}

fn default_risk_increase_catchup_ratio() -> f64 {
    DEFAULT_RISK_INCREASE_CATCHUP_RATIO
}
```

Add validation from `validate_config()`:

```rust
if let Some(delay) = config.risk_increase_delay {
    validate_risk_increase_delay(delay)?;
}
```

Add the validator:

```rust
fn validate_risk_increase_delay(config: RiskIncreaseDelayConfig) -> Result<(), String> {
    if !config.startup_initial_ratio.is_finite()
        || config.startup_initial_ratio <= 0.0
        || config.startup_initial_ratio > 1.0
    {
        return Err("startup_initial_ratio must be finite and in (0, 1]".into());
    }
    if !config.advantage_min_rebalance_multiples.is_finite()
        || config.advantage_min_rebalance_multiples <= 0.0
    {
        return Err("advantage_min_rebalance_multiples must be finite and positive".into());
    }
    if !config.base_step_min_rebalance_multiples.is_finite()
        || config.base_step_min_rebalance_multiples <= 0.0
    {
        return Err("base_step_min_rebalance_multiples must be finite and positive".into());
    }
    if !config.max_step_min_rebalance_multiples.is_finite()
        || config.max_step_min_rebalance_multiples < config.base_step_min_rebalance_multiples
    {
        return Err(
            "max_step_min_rebalance_multiples must be finite and greater than or equal to base_step_min_rebalance_multiples"
                .into(),
        );
    }
    if !config.catchup_ratio.is_finite() || config.catchup_ratio <= 0.0 || config.catchup_ratio > 1.0
    {
        return Err("catchup_ratio must be finite and in (0, 1]".into());
    }
    Ok(())
}
```

Use `rg -n "TrackConfig \\{" core engine application server protocol tui tools/track-tuning-workbench/src-tauri` to locate existing `TrackConfig` literals. Add `risk_increase_delay: None` to each literal that constructs a normal track config, except tests that explicitly set a concrete `Some(RiskIncreaseDelayConfig { startup_initial_ratio: 0.3, advantage_min_rebalance_multiples: 2.0, base_step_min_rebalance_multiples: 1.0, max_step_min_rebalance_multiples: 4.0, catchup_ratio: 0.25 })`.

- [ ] **Step 4: Run core test to verify it passes**

Run:

```bash
cargo test -p poise-core strategy::tests::validate_accepts_risk_increase_delay_config
cargo test -p poise-core strategy::tests::validate_rejects_invalid_risk_increase_delay_config
cargo test -p poise-core strategy::tests::validate_rejects_step_bounds_that_cannot_release
```

Expected: PASS.

- [ ] **Step 5: Write server config parsing test**

Add this test to `server/src/config.rs` inside `mod tests`:

```rust
#[test]
fn parses_risk_increase_delay_config() {
    let config = parse_config(
        r#"
bind_address = "127.0.0.1:8000"

[exchange]
venue = "binance"
deployment = "testnet"
api_key = ""
api_secret = ""

[[tracks]]
track_id = "btc-core"
symbol = "BTCUSDT"
lower_price = 90.0
upper_price = 110.0
long_exposure_units = 8.0
short_exposure_units = 8.0
notional_per_unit = 100.0
min_rebalance_units = 0.5
shape_family = "linear"
out_of_band_policy = "freeze"
max_notional = 800.0
leverage = 10
daily_loss_limit = 100.0
total_loss_limit = 200.0
tick_timeout_secs = 30

[tracks.risk_increase_delay]
startup_initial_ratio = 0.3
advantage_min_rebalance_multiples = 2.0
base_step_min_rebalance_multiples = 1.0
max_step_min_rebalance_multiples = 4.0
catchup_ratio = 0.25
"#,
    )
    .unwrap();

    let delay = config.tracks[0]
        .to_track_definition(config.exchange.venue())
        .unwrap()
        .track_config()
        .risk_increase_delay
        .unwrap();

    assert_eq!(delay.startup_initial_ratio, 0.3);
    assert_eq!(delay.advantage_min_rebalance_multiples, 2.0);
    assert_eq!(delay.base_step_min_rebalance_multiples, 1.0);
    assert_eq!(delay.max_step_min_rebalance_multiples, 4.0);
    assert_eq!(delay.catchup_ratio, 0.25);
}
```

- [ ] **Step 6: Run server config test to verify it fails**

Run:

```bash
cargo test -p poise-server config::tests::parses_risk_increase_delay_config
```

Expected: FAIL because `TrackSpec` does not yet accept `risk_increase_delay`.

- [ ] **Step 7: Implement server config parsing**

In `server/src/config.rs`, import `RiskIncreaseDelayConfig`, add this field to `TrackSpec`, and pass it into `TrackConfig`:

```rust
pub risk_increase_delay: Option<RiskIncreaseDelayConfig>,
```

```rust
risk_increase_delay: self.risk_increase_delay,
```

- [ ] **Step 8: Run server config test to verify it passes**

Run:

```bash
cargo test -p poise-server config::tests::parses_risk_increase_delay_config
```

Expected: PASS.

- [ ] **Step 9: Commit Task 1**

Run:

```bash
git add core/src/strategy.rs server/src/config.rs
git commit -m "feat: add risk increase delay config"
```

Record commit SHA here after committing: ``

## Task 2: Pure Risk Exposure Gate Module

**Files:**
- Create: `engine/src/risk_exposure_gate.rs`
- Modify: `engine/src/lib.rs`
- Test: `engine/src/risk_exposure_gate.rs`

- [ ] **Step 1: Create failing pure gate tests**

Create `engine/src/risk_exposure_gate.rs` with only tests first. Use the public API shown in Step 3 so the tests fail before implementation:

```rust
#[cfg(test)]
mod tests {
    use poise_core::strategy::RiskIncreaseDelayConfig;
    use poise_core::types::Exposure;

    use super::*;

    fn config() -> RiskIncreaseDelayConfig {
        RiskIncreaseDelayConfig {
            startup_initial_ratio: 0.3,
            advantage_min_rebalance_multiples: 2.0,
            base_step_min_rebalance_multiples: 1.0,
            max_step_min_rebalance_multiples: 4.0,
            catchup_ratio: 0.25,
        }
    }

    fn input(
        state: Option<RiskExposureGateState>,
        current_exposure: f64,
        curve_target: f64,
        price: f64,
    ) -> RiskExposureGateInput {
        RiskExposureGateInput {
            config: Some(config()),
            min_rebalance_units: 0.5,
            state,
            current_exposure: Exposure(current_exposure),
            curve_target: Exposure(curve_target),
            strategy_price: price,
        }
    }

    #[test]
    fn startup_allows_initial_ratio_and_keeps_backlog() {
        let decision = apply(input(None, 0.0, 5.0, 100.0));

        assert_eq!(decision.allowed_target, Exposure(1.5));
        assert_eq!(
            decision.state,
            Some(RiskExposureGateState {
                allowed_target: Exposure(1.5),
                anchor_price: 100.0,
                anchor_curve_target: Exposure(5.0),
            })
        );
        assert!(decision.next_release.is_some());
    }

    #[test]
    fn does_not_release_before_advantage_target() {
        let state = RiskExposureGateState {
            allowed_target: Exposure(1.5),
            anchor_price: 100.0,
            anchor_curve_target: Exposure(5.0),
        };

        let decision = apply(input(Some(state.clone()), 1.5, 5.9, 99.6));

        assert_eq!(decision.allowed_target, Exposure(1.5));
        assert_eq!(decision.state, Some(state));
    }

    #[test]
    fn releases_dynamic_step_after_advantage() {
        let state = RiskExposureGateState {
            allowed_target: Exposure(1.5),
            anchor_price: 100.0,
            anchor_curve_target: Exposure(5.0),
        };

        let decision = apply(input(Some(state), 1.5, 6.0, 99.5));

        assert_eq!(decision.allowed_target, Exposure(2.625));
        assert_eq!(
            decision.state,
            Some(RiskExposureGateState {
                allowed_target: Exposure(2.625),
                anchor_price: 99.5,
                anchor_curve_target: Exposure(6.0),
            })
        );
    }

    #[test]
    fn smaller_backlog_does_not_reduce_allowed_target() {
        let state = RiskExposureGateState {
            allowed_target: Exposure(1.5),
            anchor_price: 100.0,
            anchor_curve_target: Exposure(5.0),
        };

        let decision = apply(input(Some(state.clone()), 1.5, 4.0, 101.0));

        assert_eq!(decision.allowed_target, Exposure(1.5));
        assert_eq!(decision.state, Some(state));
    }

    #[test]
    fn curve_target_inside_allowed_target_reduces_immediately() {
        let state = RiskExposureGateState {
            allowed_target: Exposure(1.5),
            anchor_price: 100.0,
            anchor_curve_target: Exposure(5.0),
        };

        let decision = apply(input(Some(state), 1.5, 1.0, 102.0));

        assert_eq!(decision.allowed_target, Exposure(1.0));
        assert_eq!(decision.state, None);
        assert_eq!(decision.next_release, None);
    }

    #[test]
    fn cross_zero_reduces_to_flat_before_new_direction() {
        let state = RiskExposureGateState {
            allowed_target: Exposure(1.5),
            anchor_price: 100.0,
            anchor_curve_target: Exposure(5.0),
        };

        let decision = apply(input(Some(state), 1.5, -1.0, 105.0));

        assert_eq!(decision.allowed_target, Exposure(0.0));
        assert_eq!(decision.state, None);
        assert_eq!(decision.next_release, None);
    }

    #[test]
    fn disabled_config_follows_curve_target() {
        let decision = apply(RiskExposureGateInput {
            config: None,
            min_rebalance_units: 0.5,
            state: None,
            current_exposure: Exposure(0.0),
            curve_target: Exposure(5.0),
            strategy_price: 100.0,
        });

        assert_eq!(decision.allowed_target, Exposure(5.0));
        assert_eq!(decision.state, None);
        assert_eq!(decision.next_release, None);
    }
}
```

- [ ] **Step 2: Run pure gate tests to verify they fail**

Run:

```bash
cargo test -p poise-engine risk_exposure_gate::tests::
```

Expected: FAIL because module types and `apply()` are missing.

- [ ] **Step 3: Implement pure gate module**

Add this module shape above the tests in `engine/src/risk_exposure_gate.rs`:

```rust
use poise_core::strategy::RiskIncreaseDelayConfig;
use poise_core::types::Exposure;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskIncreaseDirection {
    Long,
    Short,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RiskExposureGateState {
    pub allowed_target: Exposure,
    pub anchor_price: f64,
    pub anchor_curve_target: Exposure,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RiskAcquisitionRelease {
    pub direction: RiskIncreaseDirection,
    pub release_target: Exposure,
    pub release_units: f64,
    pub advantage_target: Exposure,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RiskExposureGateInput {
    pub config: Option<RiskIncreaseDelayConfig>,
    pub min_rebalance_units: f64,
    pub state: Option<RiskExposureGateState>,
    pub current_exposure: Exposure,
    pub curve_target: Exposure,
    pub strategy_price: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RiskExposureGateDecision {
    pub allowed_target: Exposure,
    pub state: Option<RiskExposureGateState>,
    pub next_release: Option<RiskAcquisitionRelease>,
}
```

Implement `apply(input)` with this decision order:

```rust
pub fn apply(input: RiskExposureGateInput) -> RiskExposureGateDecision {
    let Some(config) = input.config else {
        return follow_curve(input.curve_target);
    };

    let previous_allowed = input
        .state
        .as_ref()
        .map(|state| state.allowed_target.clone())
        .unwrap_or_else(|| input.current_exposure.clone());

    if crosses_zero(previous_allowed.0, input.curve_target.0) {
        return RiskExposureGateDecision {
            allowed_target: Exposure(0.0),
            state: None,
            next_release: None,
        };
    }

    if inside_or_equal(input.curve_target.0, previous_allowed.0) {
        return RiskExposureGateDecision {
            allowed_target: input.curve_target,
            state: None,
            next_release: None,
        };
    }

    let direction = if input.curve_target.0 > previous_allowed.0 {
        RiskIncreaseDirection::Long
    } else {
        RiskIncreaseDirection::Short
    };

    let mut state = input.state.unwrap_or_else(|| startup_state(
        config,
        input.min_rebalance_units,
        input.current_exposure.clone(),
        input.curve_target.clone(),
        input.strategy_price,
    ));

    if !same_direction_backlog(direction, state.allowed_target.0, input.curve_target.0) {
        state = startup_state(
            config,
            input.min_rebalance_units,
            input.current_exposure.clone(),
            input.curve_target.clone(),
            input.strategy_price,
        );
    }

    let advantage_units = input.min_rebalance_units * config.advantage_min_rebalance_multiples;
    let reached_advantage = match direction {
        RiskIncreaseDirection::Long => {
            input.curve_target.0 >= state.anchor_curve_target.0 + advantage_units
        }
        RiskIncreaseDirection::Short => {
            input.curve_target.0 <= state.anchor_curve_target.0 - advantage_units
        }
    };

    if reached_advantage {
        let release_units = release_units(config, input.min_rebalance_units, state.allowed_target.0, input.curve_target.0);
        state.allowed_target = move_toward(state.allowed_target, input.curve_target, release_units);
        state.anchor_price = input.strategy_price;
        state.anchor_curve_target = input.curve_target;
    }

    let next_release = next_release(config, input.min_rebalance_units, &state, input.curve_target);

    RiskExposureGateDecision {
        allowed_target: state.allowed_target.clone(),
        state: Some(state),
        next_release,
    }
}
```

Add these private helpers:

```rust
fn follow_curve(curve_target: Exposure) -> RiskExposureGateDecision {
    RiskExposureGateDecision {
        allowed_target: curve_target,
        state: None,
        next_release: None,
    }
}

fn startup_state(
    config: RiskIncreaseDelayConfig,
    min_rebalance_units: f64,
    current_exposure: Exposure,
    curve_target: Exposure,
    strategy_price: f64,
) -> RiskExposureGateState {
    let target_units = curve_target.0.abs();
    let ratio_units = target_units * config.startup_initial_ratio;
    let initial_units = if target_units < min_rebalance_units {
        target_units
    } else {
        ratio_units.max(min_rebalance_units).min(target_units)
    };
    let current_units = if current_exposure.0.signum() == curve_target.0.signum() {
        current_exposure.0.abs().min(target_units)
    } else {
        0.0
    };
    let allowed_units = initial_units.max(current_units).min(target_units);
    RiskExposureGateState {
        allowed_target: Exposure(curve_target.0.signum() * allowed_units),
        anchor_price: strategy_price,
        anchor_curve_target: curve_target,
    }
}

fn release_units(
    config: RiskIncreaseDelayConfig,
    min_rebalance_units: f64,
    allowed: f64,
    curve: f64,
) -> f64 {
    let backlog_units = (curve - allowed).abs();
    let base_step_units = min_rebalance_units * config.base_step_min_rebalance_multiples;
    let max_step_units = min_rebalance_units * config.max_step_min_rebalance_multiples;
    let dynamic_units = backlog_units * config.catchup_ratio;
    dynamic_units
        .clamp(base_step_units, max_step_units)
        .min(backlog_units)
}

fn next_release(
    config: RiskIncreaseDelayConfig,
    min_rebalance_units: f64,
    state: &RiskExposureGateState,
    curve_target: Exposure,
) -> Option<RiskAcquisitionRelease> {
    let direction = if curve_target.0 > state.allowed_target.0 {
        RiskIncreaseDirection::Long
    } else if curve_target.0 < state.allowed_target.0 {
        RiskIncreaseDirection::Short
    } else {
        return None;
    };
    let release_units = release_units(
        config,
        min_rebalance_units,
        state.allowed_target.0,
        curve_target.0,
    );
    if release_units <= f64::EPSILON {
        return None;
    }
    let advantage_units = min_rebalance_units * config.advantage_min_rebalance_multiples;
    let advantage_target = match direction {
        RiskIncreaseDirection::Long => Exposure(state.anchor_curve_target.0 + advantage_units),
        RiskIncreaseDirection::Short => Exposure(state.anchor_curve_target.0 - advantage_units),
    };
    Some(RiskAcquisitionRelease {
        direction,
        release_target: move_toward(state.allowed_target.clone(), curve_target, release_units),
        release_units,
        advantage_target,
    })
}

fn move_toward(from: Exposure, to: Exposure, units: f64) -> Exposure {
    if to.0 > from.0 {
        Exposure((from.0 + units).min(to.0))
    } else {
        Exposure((from.0 - units).max(to.0))
    }
}

fn crosses_zero(allowed_target: f64, curve_target: f64) -> bool {
    (allowed_target > f64::EPSILON && curve_target < -f64::EPSILON)
        || (allowed_target < -f64::EPSILON && curve_target > f64::EPSILON)
}

fn inside_or_equal(curve_target: f64, allowed_target: f64) -> bool {
    if allowed_target > f64::EPSILON {
        curve_target <= allowed_target
    } else if allowed_target < -f64::EPSILON {
        curve_target >= allowed_target
    } else {
        curve_target.abs() <= f64::EPSILON
    }
}

fn same_direction_backlog(
    direction: RiskIncreaseDirection,
    allowed_target: f64,
    curve_target: f64,
) -> bool {
    match direction {
        RiskIncreaseDirection::Long => curve_target > allowed_target,
        RiskIncreaseDirection::Short => curve_target < allowed_target,
    }
}
```

Add `pub(crate) mod risk_exposure_gate;` to `engine/src/lib.rs`.

- [ ] **Step 4: Run pure gate tests to verify they pass**

Run:

```bash
cargo test -p poise-engine risk_exposure_gate::tests::
```

Expected: PASS.

- [ ] **Step 5: Commit Task 2**

Run:

```bash
git add engine/src/risk_exposure_gate.rs engine/src/lib.rs
git commit -m "feat: add risk exposure gate"
```

Record commit SHA here after committing: ``

## Task 3: Reconciler Runtime Wiring

**Files:**
- Modify: `engine/src/runtime.rs`
- Modify: `engine/src/reconciler.rs`
- Modify: `engine/src/manager.rs`
- Modify: `engine/src/mutation_frame.rs`
- Test: `engine/src/reconciler.rs`
- Test: `engine/src/manager.rs`

- [ ] **Step 1: Write reconciler tests for gated desired target**

Add these tests to `engine/src/reconciler.rs`:

```rust
#[test]
fn risk_increase_delay_startup_exposes_allowed_target_not_curve_target() {
    let mut track = test_runtime();
    enable_risk_increase_delay(&mut track);

    let result = reconcile_target(&track, 93.75);

    assert_eq!(result.desired_exposure, Exposure(1.5));
    assert!(matches!(
        result.new_runtime_state,
        Some(TrackState::Running(ControlState::Automatic(
            AutoState::AcquiringRiskExposure { .. }
        )))
    ));
    assert!(!result.suppress_execution);
    assert!(result.risk_acquisition.is_some());
}

#[test]
fn risk_increase_delay_reduces_allowed_target_when_curve_reenters_inside() {
    let mut track = test_runtime();
    enable_risk_increase_delay(&mut track);
    track.current_exposure = Exposure(1.5);
    track.desired_exposure = Some(Exposure(1.5));
    track.track_state = TrackState::Running(ControlState::Automatic(
        AutoState::AcquiringRiskExposure {
            gate: RiskExposureGateState {
                allowed_target: Exposure(1.5),
                anchor_price: 93.75,
                anchor_curve_target: Exposure(5.0),
            },
        },
    ));

    let result = reconcile_target(&track, 98.75);

    assert_eq!(result.desired_exposure, Exposure(1.0));
    assert!(matches!(
        result.new_runtime_state,
        Some(TrackState::Running(ControlState::Automatic(AutoState::FollowingBand)))
    ));
    assert!(!result.suppress_execution);
}

#[test]
fn risk_increase_delay_cross_zero_reduces_to_flat_first() {
    let mut track = test_runtime();
    enable_risk_increase_delay(&mut track);
    track.current_exposure = Exposure(1.5);
    track.desired_exposure = Some(Exposure(1.5));
    track.track_state = TrackState::Running(ControlState::Automatic(
        AutoState::AcquiringRiskExposure {
            gate: RiskExposureGateState {
                allowed_target: Exposure(1.5),
                anchor_price: 93.75,
                anchor_curve_target: Exposure(5.0),
            },
        },
    ));

    let result = reconcile_target(&track, 101.25);

    assert_eq!(result.desired_exposure, Exposure(0.0));
    assert!(matches!(
        result.new_runtime_state,
        Some(TrackState::Running(ControlState::Automatic(AutoState::FollowingBand)))
    ));
    assert!(!result.suppress_execution);
}
```

Add test helpers:

```rust
use crate::risk_exposure_gate::RiskExposureGateState;

fn enable_risk_increase_delay(track: &mut TrackRuntime) {
    let mut config = track.config().clone();
    config.risk_increase_delay = Some(RiskIncreaseDelayConfig::default());
    replace_definition(
        track,
        config,
        track.max_notional(),
        track.loss_limits().clone(),
    );
}
```

- [ ] **Step 2: Run reconciler tests to verify they fail**

Run:

```bash
cargo test -p poise-engine reconciler::tests::risk_increase_delay_
```

Expected: FAIL because `RiskExposureGateState`, `AutoState::AcquiringRiskExposure`, reconciler gate wiring, and result `risk_acquisition` do not exist.

- [ ] **Step 3: Add runtime state**

In `engine/src/runtime.rs`, import `RiskExposureGateState` and add this variant:

```rust
AcquiringRiskExposure {
    gate: RiskExposureGateState,
},
```

Map it to `TrackStatus::Active` in `TrackState::status()`:

```rust
Self::Running(ControlState::Automatic(AutoState::FollowingBand))
| Self::Running(ControlState::Automatic(AutoState::AcquiringRiskExposure { .. })) => {
    TrackStatus::Active
}
```

- [ ] **Step 4: Wire reconciler gate after risk cap and before account capacity**

In `engine/src/reconciler.rs`, import the gate types and add this field to `TargetReconcileResult`:

```rust
pub risk_acquisition: Option<RiskAcquisitionRelease>,
```

All early returns must set `risk_acquisition: None`.

After risk outcome returns `approved_target`, apply the gate only for automatic in-band following/acquiring states and only when `track.config().risk_increase_delay.is_some()`:

```rust
let gate_state = match &track.track_state {
    TrackState::Running(ControlState::Automatic(AutoState::AcquiringRiskExposure { gate })) => {
        Some(gate.clone())
    }
    _ => None,
};

let gate_decision = risk_exposure_gate::apply(RiskExposureGateInput {
    config: track.config().risk_increase_delay,
    min_rebalance_units: track.config().min_rebalance_units,
    state: gate_state,
    current_exposure: track.current_exposure.clone(),
    curve_target: approved_target.clone(),
    strategy_price,
});

let approved_target = gate_decision.allowed_target;
let new_runtime_state = merge_gate_state(new_runtime_state, gate_decision.state);
let risk_acquisition = gate_decision.next_release;
```

Add a private helper:

```rust
fn merge_gate_state(
    base_state: Option<TrackState>,
    gate_state: Option<RiskExposureGateState>,
) -> Option<TrackState> {
    match gate_state {
        Some(gate) => Some(TrackState::Running(ControlState::Automatic(
            AutoState::AcquiringRiskExposure { gate },
        ))),
        None => match base_state {
            Some(TrackState::Running(ControlState::Automatic(
                AutoState::AcquiringRiskExposure { .. },
            ))) => Some(TrackState::Running(ControlState::Automatic(
                AutoState::FollowingBand,
            ))),
            other => other,
        },
    }
}
```

- [ ] **Step 5: Carry risk acquisition through manager planning result**

In `engine/src/manager.rs`, add `risk_acquisition: Option<RiskAcquisitionRelease>` to `PlannedInventoryExecution` and copy `target.risk_acquisition` into that field in every `PlannedInventoryExecution` return.

Do not add `risk_acquisition` to `SubmitIntentInput` in this task. That happens in Task 4 together with executor support.

Import `RiskAcquisitionRelease` from `crate::risk_exposure_gate`.

- [ ] **Step 6: Run reconciler tests to verify they pass**

Run:

```bash
cargo test -p poise-engine reconciler::tests::risk_increase_delay_
```

Expected: PASS.

- [ ] **Step 7: Run manager compile-focused test**

Run:

```bash
cargo test -p poise-engine manager::tests::reconcile_track_submits_catch_up_action_from_due_boundary_operation
```

Expected: PASS.

- [ ] **Step 8: Commit Task 3**

Run:

```bash
git add engine/src/runtime.rs engine/src/reconciler.rs engine/src/manager.rs engine/src/mutation_frame.rs
git commit -m "feat: wire risk exposure gate into reconciliation"
```

Record commit SHA here after committing: ``

## Task 4: Executor CatchUp And CurveMaker Budgeting

**Files:**
- Modify: `engine/src/executor/planning.rs`
- Modify: `engine/src/executor/policy.rs`
- Modify: `engine/src/executor/binding.rs`
- Modify: `engine/src/manager.rs`
- Test: `engine/src/executor/mod.rs`
- Test: `engine/src/executor/policy.rs`
- Test: `engine/src/manager.rs`

- [ ] **Step 1: Write executor tests for budgeted maker**

Add this test to `engine/src/executor/mod.rs`:

```rust
#[test]
fn risk_acquisition_maker_uses_advantage_price_and_release_budget() {
    let config = config();
    let rules = rules();
    let instrument = instrument();
    let state = ExecutorState::empty(observed_at()).ensure_revision(&config, Exposure(1.5));
    let plan = plan(ExecutorInput::new(
        SubmitIntentInput {
            instrument: &instrument,
            config: &config,
            exchange_rules: &rules,
            base_qty_per_unit: 1.0,
            min_rebalance_units: config.min_rebalance_units,
            current_exposure: Exposure(1.5),
            desired_exposure: Exposure(1.5),
            execution_quote: Some(ExecutionQuote {
                best_bid: 99.9,
                best_ask: 100.1,
            }),
            policy_context: PolicyContext::Normal,
            price_execution_gate: PriceExecutionGate::Open,
            submit_purpose: SubmitPurpose::AutoReconcile,
            observed_at: observed_at(),
            risk_acquisition: Some(RiskAcquisitionRelease {
                direction: RiskIncreaseDirection::Long,
                release_target: Exposure(2.375),
                release_units: 0.875,
                advantage_target: Exposure(6.0),
            }),
        },
        Some(&state),
    ));

    let maker = plan
        .state
        .bindings
        .iter()
        .find(|binding| binding.proposal_key.policy == PolicyKind::CurveMaker)
        .expect("risk acquisition maker should be planned");

    assert_eq!(maker.request.side, Side::Buy);
    assert!((maker.request.price - 92.5).abs() < 1e-9);
    assert!((maker.quantity_as_exposure_for_test(&config) - 0.875).abs() < 1e-9);
}
```

Add this test-only helper to `LiveOrderBinding` in `engine/src/executor/binding.rs`. Add `#[cfg(test)] use poise_core::strategy::TrackConfig;` near the imports in that file:

```rust
#[cfg(test)]
pub fn quantity_as_exposure_for_test(&self, config: &TrackConfig) -> f64 {
    self.request.quantity / config.base_qty_per_unit()
}
```

- [ ] **Step 2: Run executor maker test to verify it fails**

Run:

```bash
cargo test -p poise-engine executor::tests::risk_acquisition_maker_uses_advantage_price_and_release_budget
```

Expected: FAIL because `SubmitIntentInput::risk_acquisition` and budgeted maker planning do not exist.

- [ ] **Step 3: Add risk acquisition field to executor input**

In `engine/src/executor/planning.rs`, import `RiskAcquisitionRelease` and add this field:

```rust
pub risk_acquisition: Option<RiskAcquisitionRelease>,
```

to `SubmitIntentInput`.

In `engine/src/executor/policy.rs`, add the same field to `PolicyPlanningInput<'a>`:

```rust
pub risk_acquisition: Option<&'a RiskAcquisitionRelease>,
```

Pass `submit_intent.risk_acquisition.as_ref()` when building `PolicyPlanningInput`.

In `engine/src/manager.rs`, change `submit_intent_input()` so it accepts and forwards the release budget:

```rust
fn submit_intent_input<'a>(
    &self,
    track: &'a TrackRuntime,
    desired_exposure: poise_core::types::Exposure,
    risk_acquisition: Option<RiskAcquisitionRelease>,
    observed_at: chrono::DateTime<chrono::Utc>,
) -> executor::SubmitIntentInput<'a> {
    let submit_purpose = self.submit_purpose_for_track(track, &desired_exposure);
    executor::SubmitIntentInput {
        instrument: track.instrument(),
        config: track.config(),
        exchange_rules: &track.exchange_rules,
        base_qty_per_unit: track.config().base_qty_per_unit(),
        min_rebalance_units: track.config().min_rebalance_units,
        current_exposure: track.current_exposure.clone(),
        desired_exposure,
        execution_quote: Self::execution_quote_for_track(track),
        policy_context: Self::policy_context_for_track(track),
        price_execution_gate: track.price_execution_gate,
        submit_purpose,
        observed_at,
        risk_acquisition,
    }
}
```

- [ ] **Step 4: Implement one budgeted risk acquisition CurveMaker**

In `engine/src/executor/policy.rs`, keep `CatchUp` planning unchanged so it only sees `desired_exposure = allowed_target`.

Change normal maker planning:

```rust
if let Some(release) = input.risk_acquisition {
    if let Some(binding) = plan_risk_acquisition_maker_binding(input, release, &covered_operations) {
        desired_bindings.push(binding);
    }
} else {
    desired_bindings.extend(
        select_curve_maker_operations(
            input.view,
            &covered_operations,
            input.exposure_epsilon,
            input.curve_maker_levels_per_side,
        )
        .into_iter()
        .filter_map(|operation| plan_curve_maker_binding(input, operation)),
    );
}
```

Add helper signatures:

```rust
fn plan_risk_acquisition_maker_binding(
    input: &PolicyPlanningInput<'_>,
    release: &RiskAcquisitionRelease,
    covered_operations: &BTreeSet<BoundaryOperation>,
) -> Option<DesiredBinding>

fn risk_acquisition_price(
    input: &PolicyPlanningInput<'_>,
    release: &RiskAcquisitionRelease,
) -> Option<f64>
```

Implement the helpers with this behavior:

```rust
fn plan_risk_acquisition_maker_binding(
    input: &PolicyPlanningInput<'_>,
    release: &RiskAcquisitionRelease,
    covered_operations: &BTreeSet<BoundaryOperation>,
) -> Option<DesiredBinding> {
    let direction = boundary_direction_for_risk_increase_direction(release.direction);
    let selected = select_target_operations(
        input.view,
        covered_operations,
        direction,
        input.exposure_epsilon,
        false,
    );
    let allocations = allocate_operations(input.view, selected, release.release_units);
    if allocations.is_empty() {
        return None;
    }
    let price = risk_acquisition_price(input, release)?;
    let exposure_qty = allocations
        .iter()
        .map(|allocation| allocation.exposure_qty)
        .sum::<f64>();
    let quantity = round_to_step(
        exposure_qty * input.base_qty_per_unit,
        input.exchange_rules.quantity_step,
    );
    if quantity <= f64::EPSILON || !is_meetable_minimum(price, quantity, input.exchange_rules) {
        return None;
    }

    let request = OrderRequest {
        instrument: input.instrument.clone(),
        side: side_for_direction(direction),
        price,
        quantity,
        client_order_id: next_client_order_id(PolicyKind::CurveMaker),
        reduce_only: false,
    };
    let proposal = proposal_for_allocations(PolicyKind::CurveMaker, &allocations);
    Some(DesiredBinding {
        proposal,
        allocations,
        request,
        desired_exposure: release.release_target.clone(),
        submit_purpose: input.submit_purpose,
        policy_state: BindingPolicyState::CurveMaker {
            due_grace_started_at: None,
        },
    })
}

fn risk_acquisition_price(
    input: &PolicyPlanningInput<'_>,
    release: &RiskAcquisitionRelease,
) -> Option<f64> {
    let direction = boundary_direction_for_risk_increase_direction(release.direction);
    let raw_price = trigger_price_for_boundary(release.advantage_target.0, input.config);
    raw_price
        .is_finite()
        .then(|| round_passive_price(raw_price, input.exchange_rules, direction))
}

fn boundary_direction_for_risk_increase_direction(
    direction: RiskIncreaseDirection,
) -> BoundaryDirection {
    match direction {
        RiskIncreaseDirection::Long => BoundaryDirection::Up,
        RiskIncreaseDirection::Short => BoundaryDirection::Down,
    }
}
```

Import `RiskAcquisitionRelease` and `RiskIncreaseDirection` from `crate::risk_exposure_gate`.

- [ ] **Step 5: Keep manager planning when only the risk acquisition maker is due**

In `engine/src/manager.rs`, keep the no-op suppress path only when no risk acquisition maker budget exists:

```rust
if target.suppress_execution && target.risk_acquisition.is_none() {
    let executor_state = executor::refresh_state(
        &track.executor_state,
        track.config(),
        &track.current_exposure,
        &target.desired_exposure,
        track.config().min_rebalance_units,
        observed_at,
    );
    return Ok(PlannedInventoryExecution {
        events: target.events,
        effects: vec![TrackEffect::NoOp],
        desired_exposure: target.desired_exposure.clone(),
        applied_risk_cap: target.applied_risk_cap,
        new_runtime_state: target.new_runtime_state,
        execution_gate_decision: target.execution_gate_decision,
        executor_state,
    });
}
```

Then build executor input with the release:

```rust
let submit_intent = self.submit_intent_input(
    track,
    target.desired_exposure.clone(),
    target.risk_acquisition.clone(),
    observed_at,
);
```

- [ ] **Step 6: Write manager test for maker-only planning**

Add imports to `engine/src/manager.rs` tests:

```rust
use crate::risk_exposure_gate::RiskExposureGateState;
use poise_core::strategy::RiskIncreaseDelayConfig;
```

Add this test:

```rust
#[test]
fn reconcile_track_plans_risk_acquisition_maker_when_allowed_target_is_current_exposure() {
    let (mut manager, id) = manager();
    {
        let track = manager.tracks.get_mut(&id).unwrap();
        let mut config = track.config().clone();
        config.min_rebalance_units = 0.5;
        config.risk_increase_delay = Some(RiskIncreaseDelayConfig::default());
        track.replace_definition_for_test(
            config,
            track.max_notional(),
            track.loss_limits().clone(),
        );
        track.current_exposure = Exposure(1.5);
        track.desired_exposure = Some(Exposure(1.5));
        track.track_state = TrackState::Running(ControlState::Automatic(
            AutoState::AcquiringRiskExposure {
                gate: RiskExposureGateState {
                    allowed_target: Exposure(1.5),
                    anchor_price: 93.75,
                    anchor_curve_target: Exposure(5.0),
                },
            },
        ));
    }

    let transition = manager.observe(&id, market(93.75)).unwrap();

    assert!(transition.effects.iter().any(|effect| matches!(
        effect,
        TrackEffect::SubmitOrder { request, .. }
            if request.side == Side::Buy && (request.price - 92.5).abs() < 1e-9
    )));
}
```

- [ ] **Step 7: Run executor maker test to verify it passes**

Run:

```bash
cargo test -p poise-engine executor::tests::risk_acquisition_maker_uses_advantage_price_and_release_budget
```

Expected: PASS.

- [ ] **Step 8: Write test that normal CurveMaker still works when gate is disabled**

Run existing test:

```bash
cargo test -p poise-engine executor::tests::curve_maker_policy_emits_future_operations_near_spot
```

Expected before implementation changes are complete: PASS. If it fails after Step 4, fix only the disabled-gate path so it keeps the old behavior.

- [ ] **Step 9: Run focused Task 4 tests**

Run:

```bash
cargo test -p poise-engine executor::tests::risk_acquisition_maker_uses_advantage_price_and_release_budget
cargo test -p poise-engine executor::tests::curve_maker_policy_emits_future_operations_near_spot
cargo test -p poise-engine executor::tests::catch_up_policy_cancels_stale_curve_maker_and_takes_over_operation_in_same_round
cargo test -p poise-engine manager::tests::reconcile_track_plans_risk_acquisition_maker_when_allowed_target_is_current_exposure
```

Expected: PASS.

- [ ] **Step 10: Commit Task 4**

Run:

```bash
git add engine/src/executor/planning.rs engine/src/executor/policy.rs engine/src/executor/mod.rs engine/src/executor/binding.rs engine/src/manager.rs
git commit -m "feat: budget risk acquisition curve makers"
```

Record commit SHA here after committing: ``

## Task 5: Protocol, Read Model, TUI Visibility

**Files:**
- Modify: `protocol/src/lib.rs`
- Modify: `application/src/read_model.rs`
- Modify: `server/src/projector.rs`
- Modify: `tui/src/views/instance.rs`
- Modify: `tui/tests/fixtures/track_detail_view.json`
- Test: `protocol/src/lib.rs`
- Test: `server/src/projector.rs`

- [ ] **Step 1: Write protocol serialization test**

Add a protocol test that expects `strategy.risk_increase_delay`:

```rust
#[test]
fn track_detail_serializes_risk_increase_delay() {
    let detail = track_detail_fixture_with_risk_increase_delay();
    let json = serde_json::to_value(&detail).unwrap();

    assert_eq!(
        json["strategy"]["risk_increase_delay"]["startup_initial_ratio"].as_f64(),
        Some(0.3)
    );
    assert_eq!(
        json["strategy"]["risk_increase_delay"]["advantage_min_rebalance_multiples"].as_f64(),
        Some(2.0)
    );
}
```

Use the existing protocol fixture construction style in `protocol/src/lib.rs`; add the risk config only to the fixture for this test.

- [ ] **Step 2: Run protocol test to verify it fails**

Run:

```bash
cargo test -p poise-protocol track_detail_serializes_risk_increase_delay
```

Expected: FAIL because protocol view lacks `risk_increase_delay`.

- [ ] **Step 3: Add protocol view type and projector mapping**

In `protocol/src/lib.rs`, add:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RiskIncreaseDelayView {
    pub startup_initial_ratio: f64,
    pub advantage_min_rebalance_multiples: f64,
    pub base_step_min_rebalance_multiples: f64,
    pub max_step_min_rebalance_multiples: f64,
    pub catchup_ratio: f64,
}
```

Add to `TrackStrategyView`:

```rust
#[serde(default)]
pub risk_increase_delay: Option<RiskIncreaseDelayView>,
```

In `application/src/read_model.rs`, add `risk_increase_delay: Option<RiskIncreaseDelayConfig>` to `TrackReadModel`.

In `server/src/projector.rs`, map `RiskIncreaseDelayConfig` to `RiskIncreaseDelayView`.

- [ ] **Step 4: Run protocol test to verify it passes**

Run:

```bash
cargo test -p poise-protocol track_detail_serializes_risk_increase_delay
```

Expected: PASS.

- [ ] **Step 5: Add TUI display**

In `tui/src/views/instance.rs`, add one compact line in the strategy/config panel:

```text
risk delay: startup 30%, advantage 2.0x, step 1.0x-4.0x, catchup 25%
```

If `risk_increase_delay` is `None`, render:

```text
risk delay: off
```

Update `tui/tests/fixtures/track_detail_view.json` to include the serialized config for one track fixture.

- [ ] **Step 6: Run server projector focused tests**

Run:

```bash
cargo test -p poise-server projector::tests::
```

Expected: PASS.

- [ ] **Step 7: Commit Task 5**

Run:

```bash
git add protocol/src/lib.rs application/src/read_model.rs server/src/projector.rs tui/src/views/instance.rs tui/tests/fixtures/track_detail_view.json
git commit -m "feat: expose risk increase delay views"
```

Record commit SHA here after committing: ``

## Task 6: Workbench Config Editing

**Files:**
- Modify: `tools/track-tuning-workbench/src-tauri/src/config_document.rs`
- Modify: `tools/track-tuning-workbench/src-tauri/src/commands.rs`
- Modify: `tools/track-tuning-workbench/src-tauri/src/config_projection.rs`
- Modify: `tools/track-tuning-workbench/src/domain/trackDraft.ts`
- Modify: `tools/track-tuning-workbench/src/domain/trackValidation.ts`
- Modify: `tools/track-tuning-workbench/src/app/workbenchBridge.ts`
- Create: `tools/track-tuning-workbench/src/ui/editor/sections/RiskIncreaseDelaySection.tsx`
- Modify: `tools/track-tuning-workbench/src/ui/editor/TrackEditor.tsx`
- Test: `tools/track-tuning-workbench/src-tauri/src/config_document.rs`
- Test: `tools/track-tuning-workbench/src/app/workbenchBridge.test.ts`
- Test: `tools/track-tuning-workbench/src/ui/app/AppShell.test.tsx`

- [ ] **Step 1: Write Tauri config document test**

Add a test in `tools/track-tuning-workbench/src-tauri/src/config_document.rs`:

```rust
#[test]
fn loads_and_projects_risk_increase_delay() {
    let document = load_from_str(
        r#"
[exchange]
venue = "binance"
deployment = "testnet"
api_key = "demo"
api_secret = "secret"

[[tracks]]
track_id = "btc-core"
symbol = "BTCUSDT"
lower_price = 90.0
upper_price = 110.0
long_exposure_units = 8.0
short_exposure_units = 8.0
notional_per_unit = 100.0
max_notional = 800.0
min_rebalance_units = 0.5
leverage = 10
out_of_band_policy = "freeze"
daily_loss_limit = 100.0
total_loss_limit = 200.0
shape_family = "linear"

[tracks.risk_increase_delay]
startup_initial_ratio = 0.3
advantage_min_rebalance_multiples = 2.0
base_step_min_rebalance_multiples = 1.0
max_step_min_rebalance_multiples = 4.0
catchup_ratio = 0.25
"#,
    )
    .unwrap();

    let track = &document.drafts()[0].fields;
    let delay = track.risk_increase_delay.unwrap();

    assert_eq!(delay.startup_initial_ratio, 0.3);
    assert_eq!(delay.catchup_ratio, 0.25);
}
```

- [ ] **Step 2: Run Tauri config test to verify it fails**

Run:

```bash
cargo test --manifest-path tools/track-tuning-workbench/src-tauri/Cargo.toml config_document::tests::loads_and_projects_risk_increase_delay
```

Expected: FAIL because workbench config fields do not include `risk_increase_delay`.

- [ ] **Step 3: Implement Tauri config fields**

Add a Rust payload struct mirroring core config:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RiskIncreaseDelayFields {
    pub startup_initial_ratio: f64,
    pub advantage_min_rebalance_multiples: f64,
    pub base_step_min_rebalance_multiples: f64,
    pub max_step_min_rebalance_multiples: f64,
    pub catchup_ratio: f64,
}
```

Add `pub risk_increase_delay: Option<RiskIncreaseDelayFields>` to editable fields, payloads, hash/projection, parse, and save paths.

Projection must emit:

```toml
[tracks.risk_increase_delay]
startup_initial_ratio = 0.3
advantage_min_rebalance_multiples = 2.0
base_step_min_rebalance_multiples = 1.0
max_step_min_rebalance_multiples = 4.0
catchup_ratio = 0.25
```

only when the option is `Some`.

- [ ] **Step 4: Run Tauri config test to verify it passes**

Run:

```bash
cargo test --manifest-path tools/track-tuning-workbench/src-tauri/Cargo.toml config_document::tests::loads_and_projects_risk_increase_delay
```

Expected: PASS.

- [ ] **Step 5: Write frontend bridge test**

Add a test in `tools/track-tuning-workbench/src/app/workbenchBridge.test.ts`:

```typescript
it('maps risk increase delay fields through editable payloads', async () => {
  const bridge = createWorkbenchBridge(fakeTauriInvoker({
    load_config_file: {
      config_path: '/tmp/demo.toml',
      exchange_venue: 'binance',
      projected_tracks: [{
        draft_id: 'btc-core',
        load_issues: [],
        fields: {
          track_id: 'btc-core',
          symbol: 'BTCUSDT',
          lower_price: 90,
          upper_price: 110,
          long_exposure_units: 8,
          short_exposure_units: 8,
          notional_per_unit: 100,
          max_notional: 800,
          min_rebalance_units: 0.5,
          leverage: 10,
          out_of_band_policy: 'freeze',
          daily_loss_limit: 100,
          total_loss_limit: 200,
          shape_family: 'linear',
          risk_increase_delay: {
            startup_initial_ratio: 0.3,
            advantage_min_rebalance_multiples: 2,
            base_step_min_rebalance_multiples: 1,
            max_step_min_rebalance_multiples: 4,
            catchup_ratio: 0.25,
          },
        },
      }],
    },
  }));

  const loaded = await bridge.loadConfigFile('/tmp/demo.toml');

  expect(loaded.tracks[0].riskIncreaseDelay?.startupInitialRatio).toBe('0.3');
  expect(loaded.tracks[0].riskIncreaseDelay?.catchupRatio).toBe('0.25');
});
```

Use the existing fake invoker helper names from the file; if the helper name differs, keep the existing helper and only change this test body.

- [ ] **Step 6: Run frontend bridge test to verify it fails**

Run:

```bash
pnpm --dir tools/track-tuning-workbench test -- workbenchBridge.test.ts
```

Expected: FAIL because TypeScript draft state lacks risk delay fields.

- [ ] **Step 7: Implement frontend draft state and editor section**

Add TypeScript draft shape:

```typescript
export interface RiskIncreaseDelayDraft {
  startupInitialRatio: string;
  advantageMinRebalanceMultiples: string;
  baseStepMinRebalanceMultiples: string;
  maxStepMinRebalanceMultiples: string;
  catchupRatio: string;
}
```

Add optional `riskIncreaseDelay?: RiskIncreaseDelayDraft` to the track draft model, load/save bridge, validation, and payload mapping.

Create `RiskIncreaseDelaySection.tsx` with five numeric inputs and one enable checkbox. Labels:

```text
增加风险延迟
启动初始比例
优势倍数
最小释放倍数
最大释放倍数
追补比例
```

`TrackEditor.tsx` must render this section near the existing risk section.

- [ ] **Step 8: Run frontend tests to verify they pass**

Run:

```bash
pnpm --dir tools/track-tuning-workbench test -- workbenchBridge.test.ts AppShell.test.tsx
```

Expected: PASS.

- [ ] **Step 9: Commit Task 6**

Run:

```bash
git add tools/track-tuning-workbench/src-tauri/src/config_document.rs tools/track-tuning-workbench/src-tauri/src/commands.rs tools/track-tuning-workbench/src-tauri/src/config_projection.rs tools/track-tuning-workbench/src/domain/trackDraft.ts tools/track-tuning-workbench/src/domain/trackValidation.ts tools/track-tuning-workbench/src/app/workbenchBridge.ts tools/track-tuning-workbench/src/app/workbenchBridge.test.ts tools/track-tuning-workbench/src/ui/editor/TrackEditor.tsx tools/track-tuning-workbench/src/ui/editor/sections/RiskIncreaseDelaySection.tsx tools/track-tuning-workbench/src/ui/app/AppShell.test.tsx
git commit -m "feat: edit risk increase delay in workbench"
```

Record commit SHA here after committing: ``

## Task 7: Final Focused Verification

**Files:**
- Modify: `docs/superpowers/plans/2026-05-10-risk-exposure-acquisition-gate.md`

- [ ] **Step 1: Run focused Rust tests**

Run:

```bash
cargo test -p poise-core strategy::tests::validate_accepts_risk_increase_delay_config
cargo test -p poise-core strategy::tests::validate_rejects_invalid_risk_increase_delay_config
cargo test -p poise-core strategy::tests::validate_rejects_step_bounds_that_cannot_release
cargo test -p poise-engine risk_exposure_gate::tests::
cargo test -p poise-engine reconciler::tests::risk_increase_delay_startup_exposes_allowed_target_not_curve_target
cargo test -p poise-engine reconciler::tests::risk_increase_delay_reduces_allowed_target_when_curve_reenters_inside
cargo test -p poise-engine reconciler::tests::risk_increase_delay_cross_zero_reduces_to_flat_first
cargo test -p poise-engine executor::tests::risk_acquisition_maker_uses_advantage_price_and_release_budget
cargo test -p poise-engine executor::tests::curve_maker_policy_emits_future_operations_near_spot
cargo test -p poise-engine executor::tests::catch_up_policy_cancels_stale_curve_maker_and_takes_over_operation_in_same_round
cargo test -p poise-engine manager::tests::reconcile_track_plans_risk_acquisition_maker_when_allowed_target_is_current_exposure
cargo test -p poise-server config::tests::parses_risk_increase_delay_config
cargo test -p poise-server projector::tests::
```

Expected: all commands PASS.

- [ ] **Step 2: Run workbench focused tests**

Run:

```bash
cargo test --manifest-path tools/track-tuning-workbench/src-tauri/Cargo.toml config_document::tests::loads_and_projects_risk_increase_delay
pnpm --dir tools/track-tuning-workbench test -- workbenchBridge.test.ts AppShell.test.tsx
```

Expected: all commands PASS.

- [ ] **Step 3: Check formatting/lint surfaces used by changed files**

Run:

```bash
cargo fmt --check
pnpm --dir tools/track-tuning-workbench build
```

Expected: both commands PASS.

- [ ] **Step 4: Update plan checkboxes and commit verification record**

After all prior tasks have their commit SHAs recorded, update this plan file so all completed task checkboxes are checked.

Run:

```bash
git add docs/superpowers/plans/2026-05-10-risk-exposure-acquisition-gate.md
git commit -m "docs: record risk exposure gate implementation completion"
```

Record commit SHA here after committing: ``
