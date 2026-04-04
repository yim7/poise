use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};

use crate::app::{App, View};
use crate::protocol::TrackCommandType;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandKind {
    Pause,
    Resume,
}

impl CommandKind {
    pub fn as_track_command(self) -> TrackCommandType {
        match self {
            Self::Pause => TrackCommandType::Pause,
            Self::Resume => TrackCommandType::Resume,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    None,
    OpenSelectedInstance,
    RefreshSelectedInstance,
    ToggleDiagnostics,
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
            if app.current_view == View::Instance && app.is_command_enabled(TrackCommandType::Pause)
            {
                Action::SubmitCommand(CommandKind::Pause)
            } else {
                Action::None
            }
        }
        KeyCode::Char('r') | KeyCode::Char('R') => {
            if app.current_view == View::Instance && app.is_command_enabled(TrackCommandType::Resume)
            {
                Action::SubmitCommand(CommandKind::Resume)
            } else {
                Action::None
            }
        }
        KeyCode::Char('d') | KeyCode::Char('D') => {
            if app.current_view == View::Instance {
                Action::ToggleDiagnostics
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
    use crate::protocol::{TrackCommandType, TrackCommandView, TrackDetailView, TrackListResponse};

    use super::{Action, CommandKind, handle_key_event};

    fn app() -> App {
        let mut response: TrackListResponse =
            serde_json::from_str(include_str!("../tests/fixtures/track_list_response.json"))
                .unwrap();
        let mut eth = response.items[0].clone();
        eth.id = "eth-core".into();
        eth.instrument.symbol = "ETHUSDT".into();
        response.items.push(eth);
        App::new(response.items)
    }

    fn detail_view() -> TrackDetailView {
        let mut detail: TrackDetailView =
            serde_json::from_str(include_str!("../tests/fixtures/track_detail_view.json")).unwrap();
        detail.available_commands.push(TrackCommandView {
            command: TrackCommandType::Resume,
            enabled: true,
            disabled_reason: None,
        });
        detail
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
        app.current_track = Some(detail_view());

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
        app.current_track = None;

        assert_eq!(
            handle_key_event(&mut app, key(KeyCode::Char('p'))),
            Action::None
        );
        assert_eq!(
            handle_key_event(&mut app, key(KeyCode::Char('r'))),
            Action::None
        );
    }

    #[test]
    fn d_toggles_diagnostics_in_instance_view() {
        let mut app = app();
        app.current_view = View::Instance;

        assert_eq!(
            handle_key_event(&mut app, key(KeyCode::Char('d'))),
            Action::ToggleDiagnostics
        );
    }
}
