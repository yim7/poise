use std::{env, io, time::Duration};

use anyhow::{Context, Result};
use crossterm::{
    event::{Event, EventStream},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures_util::StreamExt;
use ratatui::{Terminal, backend::CrosstermBackend};
use tokio::{
    select,
    sync::mpsc,
    task::JoinHandle,
    time::{interval, sleep},
};

use crate::{
    effects::Effect,
    events::{AppEvent, EffectResultEvent, InputEvent, SystemEvent},
    input::map_key_event,
    locale::Locale,
    render::draw,
    state::AppState,
    theme::Theme,
    transport::TransportClient,
};

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub base_url: String,
    pub ws_url: String,
    pub ui_locale: Locale,
}

pub fn load_dotenv_if_present() -> Result<()> {
    match dotenvy::dotenv() {
        Ok(_) => Ok(()),
        Err(error) if error.not_found() => Ok(()),
        Err(error) => Err(error.into()),
    }
}

impl AppConfig {
    pub fn from_env() -> Self {
        let ui_locale = env::var("GRID_PLATFORM_UI_LOCALE")
            .ok()
            .and_then(|value| Locale::from_env_value(&value))
            .unwrap_or(Locale::EnUs);

        Self {
            base_url: env::var("GRID_PLATFORM_BASE_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:8000".into()),
            ws_url: env::var("GRID_PLATFORM_WS_URL")
                .unwrap_or_else(|_| "ws://127.0.0.1:8000/ws".into()),
            ui_locale,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{env, fs, path::Path, sync::Mutex};

    use anyhow::Result;
    use tempfile::TempDir;

    use super::{AppConfig, current_instance_matches, load_dotenv_if_present};
    use crate::locale::Locale;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvGuard {
        key: &'static str,
        original: Option<String>,
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // Restore the original process environment after each test.
            unsafe {
                match &self.original {
                    Some(value) => env::set_var(self.key, value),
                    None => env::remove_var(self.key),
                }
            }
        }
    }

    struct CurrentDirGuard {
        original: std::path::PathBuf,
    }

    impl Drop for CurrentDirGuard {
        fn drop(&mut self) {
            env::set_current_dir(&self.original).expect("restore current dir");
        }
    }

    fn set_env(key: &'static str, value: Option<&str>) -> EnvGuard {
        let original = env::var(key).ok();

        unsafe {
            match value {
                Some(value) => env::set_var(key, value),
                None => env::remove_var(key),
            }
        }

        EnvGuard { key, original }
    }

    fn set_ui_locale_env(value: Option<&str>) -> EnvGuard {
        set_env("GRID_PLATFORM_UI_LOCALE", value)
    }

    fn with_temp_cwd(dir: &Path) -> CurrentDirGuard {
        let original = env::current_dir().expect("current dir");
        env::set_current_dir(dir).expect("set current dir");
        CurrentDirGuard { original }
    }

    fn write_dotenv(dir: &Path, content: &str) -> Result<()> {
        fs::write(dir.join(".env"), content.trim_start())?;
        Ok(())
    }

    #[test]
    fn defaults_ui_locale_to_english_when_env_is_absent() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _env_guard = set_ui_locale_env(None);

        assert_eq!(AppConfig::from_env().ui_locale, Locale::EnUs);
    }

    #[test]
    fn parses_ui_locale_from_env() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _env_guard = set_ui_locale_env(Some("zh-CN"));

        assert_eq!(AppConfig::from_env().ui_locale, Locale::ZhCn);
    }

    #[test]
    fn dotenv_file_is_loaded_before_app_config_reads_env() -> Result<()> {
        let _guard = ENV_LOCK.lock().unwrap();
        let temp = TempDir::new()?;
        let _cwd_guard = with_temp_cwd(temp.path());
        let _base_url_guard = set_env("GRID_PLATFORM_BASE_URL", None);
        let _ws_url_guard = set_env("GRID_PLATFORM_WS_URL", None);
        let _locale_guard = set_env("GRID_PLATFORM_UI_LOCALE", None);

        write_dotenv(
            temp.path(),
            r#"
GRID_PLATFORM_BASE_URL=http://127.0.0.1:9001
GRID_PLATFORM_WS_URL=ws://127.0.0.1:9001/ws
GRID_PLATFORM_UI_LOCALE=zh-CN
"#,
        )?;

        load_dotenv_if_present()?;
        let config = AppConfig::from_env();

        assert_eq!(config.base_url, "http://127.0.0.1:9001");
        assert_eq!(config.ws_url, "ws://127.0.0.1:9001/ws");
        assert_eq!(config.ui_locale, Locale::ZhCn);
        Ok(())
    }

    #[test]
    fn process_env_overrides_dotenv_values_for_app_config() -> Result<()> {
        let _guard = ENV_LOCK.lock().unwrap();
        let temp = TempDir::new()?;
        let _cwd_guard = with_temp_cwd(temp.path());
        let _base_url_guard = set_env("GRID_PLATFORM_BASE_URL", Some("http://127.0.0.1:9002"));
        let _ws_url_guard = set_env("GRID_PLATFORM_WS_URL", Some("ws://127.0.0.1:9002/ws"));
        let _locale_guard = set_env("GRID_PLATFORM_UI_LOCALE", Some("en-US"));

        write_dotenv(
            temp.path(),
            r#"
GRID_PLATFORM_BASE_URL=http://127.0.0.1:9001
GRID_PLATFORM_WS_URL=ws://127.0.0.1:9001/ws
GRID_PLATFORM_UI_LOCALE=zh-CN
"#,
        )?;

        load_dotenv_if_present()?;
        let config = AppConfig::from_env();

        assert_eq!(config.base_url, "http://127.0.0.1:9002");
        assert_eq!(config.ws_url, "ws://127.0.0.1:9002/ws");
        assert_eq!(config.ui_locale, Locale::EnUs);
        Ok(())
    }

    #[test]
    fn stale_generation_for_same_symbol_does_not_match_current_instance() {
        assert!(current_instance_matches(Some("BTCUSDT"), 3, "BTCUSDT", 3,));
        assert!(!current_instance_matches(Some("BTCUSDT"), 3, "BTCUSDT", 1,));
    }
}

pub async fn run_app(config: AppConfig) -> Result<()> {
    enable_raw_mode().context("failed to enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).context("failed to enter alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("failed to create terminal")?;
    terminal.clear().context("failed to clear terminal")?;

    let result = run_loop(&mut terminal, config).await;

    disable_raw_mode().ok();
    execute!(terminal.backend_mut(), LeaveAlternateScreen).ok();
    terminal.show_cursor().ok();

    result
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    config: AppConfig,
) -> Result<()> {
    let theme = Theme::default();
    let mut state = AppState::waiting_first_snapshot_with_locale(config.ui_locale);
    let (app_tx, mut app_rx) = mpsc::channel::<AppEvent>(512);
    let (effect_tx, effect_rx) = mpsc::channel::<Effect>(256);
    let transport = TransportClient::new(config.base_url, config.ws_url);

    tokio::spawn(input_task(app_tx.clone()));
    tokio::spawn(clock_task(app_tx.clone()));
    tokio::spawn(effect_task(
        transport,
        app_tx.clone(),
        effect_tx.clone(),
        effect_rx,
    ));
    effect_tx.send(Effect::FetchInstances).await.ok();

    terminal.draw(|frame| draw(frame, &state, &theme))?;
    state.dirty.clear();

    while let Some(event) = app_rx.recv().await {
        let is_render_tick = matches!(&event, AppEvent::System(SystemEvent::RenderTick));
        let effects = crate::store::reduce(&mut state, event);
        for effect in effects {
            effect_tx.send(effect).await.ok();
        }
        if state.ui.should_quit {
            break;
        }
        if state.take_immediate_render() || (is_render_tick && state.dirty.any()) {
            terminal.draw(|frame| draw(frame, &state, &theme))?;
            state.dirty.clear();
        }
    }

    Ok(())
}

async fn input_task(app_tx: mpsc::Sender<AppEvent>) {
    let mut stream = EventStream::new();
    while let Some(Ok(event)) = stream.next().await {
        match event {
            Event::Key(key) => {
                if let Some(action) = map_key_event(key) {
                    if app_tx
                        .send(AppEvent::Input(InputEvent::Key(action)))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
            Event::Resize(width, height) => {
                if app_tx
                    .send(AppEvent::Input(InputEvent::Resize(width, height)))
                    .await
                    .is_err()
                {
                    break;
                }
            }
            _ => {}
        }
    }
}

async fn clock_task(app_tx: mpsc::Sender<AppEvent>) {
    let mut render = interval(Duration::from_millis(83));
    let mut health = interval(Duration::from_secs(1));
    loop {
        select! {
            _ = render.tick() => {
                if app_tx.send(AppEvent::System(SystemEvent::RenderTick)).await.is_err() {
                    break;
                }
            }
            _ = health.tick() => {
                if app_tx.send(AppEvent::System(SystemEvent::HealthTick)).await.is_err() {
                    break;
                }
            }
        }
    }
}

async fn effect_task(
    transport: TransportClient,
    app_tx: mpsc::Sender<AppEvent>,
    effect_tx: mpsc::Sender<Effect>,
    mut effect_rx: mpsc::Receiver<Effect>,
) {
    let mut current_symbol: Option<String> = None;
    let mut current_generation = 0;
    let mut ws_task: Option<JoinHandle<()>> = None;

    while let Some(effect) = effect_rx.recv().await {
        match effect {
            Effect::FetchInstances => match transport.fetch_instances().await {
                Ok(directory) => {
                    let _ = app_tx
                        .send(AppEvent::EffectResult(EffectResultEvent::InstancesLoaded(
                            directory,
                        )))
                        .await;
                }
                Err(error) => {
                    let _ = app_tx
                        .send(AppEvent::EffectResult(EffectResultEvent::InstancesFailed(
                            error.to_string(),
                        )))
                        .await;
                }
            },
            Effect::FetchInstancesAfterDelay { retry_in_ms } => {
                spawn_delayed_effect(
                    effect_tx.clone(),
                    Duration::from_millis(retry_in_ms),
                    Effect::FetchInstances,
                );
            }
            Effect::UseInstance { symbol, generation } => {
                current_symbol = Some(symbol);
                current_generation = generation;
                if let Some(task) = ws_task.take() {
                    task.abort();
                }
            }
            Effect::FetchSnapshot { symbol, generation } => {
                if !current_instance_matches(
                    current_symbol.as_deref(),
                    current_generation,
                    &symbol,
                    generation,
                ) {
                    continue;
                }
                match transport.fetch_instance_snapshot(&symbol).await {
                    Ok(snapshot) => {
                        let _ = app_tx
                            .send(AppEvent::EffectResult(EffectResultEvent::SnapshotLoaded {
                                symbol,
                                generation,
                                snapshot,
                            }))
                            .await;
                    }
                    Err(error) => {
                        let _ = app_tx
                            .send(AppEvent::EffectResult(EffectResultEvent::SnapshotFailed {
                                symbol,
                                generation,
                                error: error.to_string(),
                            }))
                            .await;
                    }
                }
            }
            Effect::FetchSnapshotAfterDelay {
                symbol,
                generation,
                retry_in_ms,
            } => {
                if !current_instance_matches(
                    current_symbol.as_deref(),
                    current_generation,
                    &symbol,
                    generation,
                ) {
                    continue;
                }
                spawn_delayed_effect(
                    effect_tx.clone(),
                    Duration::from_millis(retry_in_ms),
                    Effect::FetchSnapshot { symbol, generation },
                );
            }
            Effect::FetchRiskEvents { symbol, generation } => {
                if !current_instance_matches(
                    current_symbol.as_deref(),
                    current_generation,
                    &symbol,
                    generation,
                ) {
                    continue;
                }
                match transport.fetch_instance_risk_events(&symbol).await {
                    Ok(alerts) => {
                        let _ = app_tx
                            .send(AppEvent::EffectResult(
                                EffectResultEvent::RiskEventsLoaded {
                                    symbol,
                                    generation,
                                    alerts,
                                },
                            ))
                            .await;
                    }
                    Err(error) => {
                        let _ = app_tx
                            .send(AppEvent::EffectResult(
                                EffectResultEvent::RiskEventsFailed {
                                    symbol,
                                    generation,
                                    error: error.to_string(),
                                },
                            ))
                            .await;
                    }
                }
            }
            Effect::ConnectWs { symbol, generation } => {
                if !current_instance_matches(
                    current_symbol.as_deref(),
                    current_generation,
                    &symbol,
                    generation,
                ) {
                    continue;
                }
                if let Some(task) = ws_task.take() {
                    task.abort();
                }
                ws_task =
                    Some(transport.spawn_instance_ws_listener(symbol, generation, app_tx.clone()));
            }
            Effect::ReconnectWs {
                symbol,
                generation,
                attempt,
            } => {
                if !current_instance_matches(
                    current_symbol.as_deref(),
                    current_generation,
                    &symbol,
                    generation,
                ) {
                    continue;
                }
                let backoff_secs = 2u64.saturating_pow(attempt.saturating_sub(1)).min(8);
                spawn_delayed_effect(
                    effect_tx.clone(),
                    Duration::from_secs(backoff_secs),
                    Effect::ConnectWs { symbol, generation },
                );
            }
            Effect::SendCommand {
                symbol,
                generation,
                command,
                command_id,
            } => {
                if !current_instance_matches(
                    current_symbol.as_deref(),
                    current_generation,
                    &symbol,
                    generation,
                ) {
                    continue;
                }
                match transport
                    .send_instance_command(&symbol, command, command_id.clone())
                    .await
                {
                    Ok(accepted) => {
                        let _ = app_tx
                            .send(AppEvent::EffectResult(EffectResultEvent::CommandAccepted {
                                symbol,
                                generation,
                                accepted,
                            }))
                            .await;
                    }
                    Err(error) => {
                        let _ = app_tx
                            .send(AppEvent::EffectResult(EffectResultEvent::CommandFailed {
                                symbol,
                                generation,
                                command_id,
                                error: error.to_string(),
                            }))
                            .await;
                    }
                }
            }
            Effect::LogClientSideEvent(_) => {}
        }
    }

    if let Some(task) = ws_task {
        task.abort();
    }
}

fn spawn_delayed_effect(effect_tx: mpsc::Sender<Effect>, delay: Duration, effect: Effect) {
    tokio::spawn(async move {
        sleep(delay).await;
        let _ = effect_tx.send(effect).await;
    });
}

fn current_instance_matches(
    current_symbol: Option<&str>,
    current_generation: u64,
    symbol: &str,
    generation: u64,
) -> bool {
    current_symbol == Some(symbol) && current_generation == generation
}
