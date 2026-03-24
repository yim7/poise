use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};

use crate::app::{App, View};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandKind {
    Pause,
    Resume,
}

impl CommandKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pause => "pause",
            Self::Resume => "resume",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    None,
    OpenSelectedInstance,
    RefreshSelectedInstance,
    SubmitCommand(CommandKind),
}

pub fn handle_key_event(app: &mut App, key: KeyEvent) -> Action {
    if key.kind != KeyEventKind::Press {
        return Action::None;
    }

    match key.code {
        KeyCode::Char('q') | KeyCode::Char('Q') => {
            app.should_quit = true;
            Action::None
        }
        KeyCode::Up | KeyCode::Char('k') | KeyCode::Char('[') => {
            app.select_previous();
            if app.current_view == View::Instance {
                app.show_instance_for_selected();
                Action::RefreshSelectedInstance
            } else {
                Action::None
            }
        }
        KeyCode::Down | KeyCode::Char('j') | KeyCode::Char(']') => {
            app.select_next();
            if app.current_view == View::Instance {
                app.show_instance_for_selected();
                Action::RefreshSelectedInstance
            } else {
                Action::None
            }
        }
        KeyCode::Enter => {
            app.show_instance_for_selected();
            Action::OpenSelectedInstance
        }
        KeyCode::Esc => {
            match app.current_view {
                View::Help => app.leave_help(),
                View::Instance => app.show_dashboard(),
                View::Dashboard => {}
            }
            Action::None
        }
        KeyCode::Char('?') => {
            app.enter_help();
            Action::None
        }
        KeyCode::Char('p') | KeyCode::Char('P') => {
            if app.current_view == View::Instance && app.has_current_instance() {
                Action::SubmitCommand(CommandKind::Pause)
            } else {
                Action::None
            }
        }
        KeyCode::Char('r') | KeyCode::Char('R') => {
            if app.current_view == View::Instance && app.has_current_instance() {
                Action::SubmitCommand(CommandKind::Resume)
            } else {
                Action::None
            }
        }
        _ => Action::None,
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use crate::app::{App, View};
    use crate::protocol::{
        GridConfig, InstanceSnapshot, InstanceStatus, InstanceSummary, OutOfBandPolicy, ShapeFamily,
    };

    use super::{Action, CommandKind, handle_key_event};

    fn app() -> App {
        App::new(vec![
            InstanceSummary {
                id: "BTCUSDT".into(),
                symbol: "BTCUSDT".into(),
                status: InstanceStatus::Active,
                last_price: Some(100.0),
            },
            InstanceSummary {
                id: "ETHUSDT".into(),
                symbol: "ETHUSDT".into(),
                status: InstanceStatus::Paused,
                last_price: Some(2000.0),
            },
        ])
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn quits_on_q() {
        let mut app = app();

        let action = handle_key_event(&mut app, key(KeyCode::Char('q')));

        assert_eq!(action, Action::None);
        assert!(app.should_quit);
    }

    #[test]
    fn wraps_selection_on_navigation() {
        let mut app = app();

        handle_key_event(&mut app, key(KeyCode::Up));
        assert_eq!(app.selected_index, 1);

        handle_key_event(&mut app, key(KeyCode::Down));
        assert_eq!(app.selected_index, 0);
    }

    #[test]
    fn enter_opens_instance_view() {
        let mut app = app();

        let action = handle_key_event(&mut app, key(KeyCode::Enter));

        assert_eq!(action, Action::OpenSelectedInstance);
        assert_eq!(app.current_view, View::Instance);
    }

    #[test]
    fn esc_returns_to_dashboard() {
        let mut app = app();
        app.current_view = View::Instance;

        let action = handle_key_event(&mut app, key(KeyCode::Esc));

        assert_eq!(action, Action::None);
        assert_eq!(app.current_view, View::Dashboard);
    }

    #[test]
    fn help_view_is_restored_on_escape() {
        let mut app = app();
        app.current_view = View::Instance;

        handle_key_event(&mut app, key(KeyCode::Char('?')));
        assert_eq!(app.current_view, View::Help);

        handle_key_event(&mut app, key(KeyCode::Esc));
        assert_eq!(app.current_view, View::Dashboard);
    }

    #[test]
    fn bracket_navigation_refreshes_current_instance() {
        let mut app = app();
        app.current_view = View::Instance;

        let action = handle_key_event(&mut app, key(KeyCode::Char(']')));

        assert_eq!(action, Action::RefreshSelectedInstance);
        assert_eq!(app.selected_index, 1);
    }

    #[test]
    fn pause_and_resume_are_available_in_instance_view() {
        let mut app = app();
        app.current_view = View::Instance;
        app.current_instance = Some(InstanceSnapshot {
            id: "BTCUSDT".into(),
            symbol: "BTCUSDT".into(),
            status: InstanceStatus::Active,
            current_exposure: 1.0,
            last_price: Some(100.0),
            config: GridConfig {
                lower_price: 90.0,
                upper_price: 110.0,
                long_capacity: 8.0,
                short_capacity: 8.0,
                capacity_notional: 375.0,
                shape_family: ShapeFamily::Linear,
                out_of_band_policy: OutOfBandPolicy::Freeze,
            },
        });

        assert_eq!(
            handle_key_event(&mut app, key(KeyCode::Char('p'))),
            Action::SubmitCommand(CommandKind::Pause)
        );
        assert_eq!(
            handle_key_event(&mut app, key(KeyCode::Char('r'))),
            Action::SubmitCommand(CommandKind::Resume)
        );
    }

    #[test]
    fn commands_are_disabled_without_loaded_instance() {
        let mut app = app();
        app.current_view = View::Instance;
        app.current_instance = None;

        assert_eq!(
            handle_key_event(&mut app, key(KeyCode::Char('p'))),
            Action::None
        );
        assert_eq!(
            handle_key_event(&mut app, key(KeyCode::Char('r'))),
            Action::None
        );
    }
}
