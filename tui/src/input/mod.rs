use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::events::KeyAction;

pub fn map_key_event(event: KeyEvent) -> Option<KeyAction> {
    let has_control_modifier = event
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER);

    if event.code == KeyCode::Char('c') && event.modifiers.contains(KeyModifiers::CONTROL) {
        return Some(KeyAction::Quit);
    }

    if has_control_modifier {
        return None;
    }

    match event.code {
        KeyCode::Char('1') => Some(KeyAction::ViewDashboard),
        KeyCode::Char('2') => Some(KeyAction::ViewGrid),
        KeyCode::Char('3') => Some(KeyAction::ViewMarket),
        KeyCode::Char('4') => Some(KeyAction::ViewEvents),
        KeyCode::Char('?') => Some(KeyAction::ToggleHelp),
        KeyCode::Char('[') => Some(KeyAction::PrevInstance),
        KeyCode::Char(']') => Some(KeyAction::NextInstance),
        KeyCode::Tab => Some(KeyAction::NextFocus),
        KeyCode::BackTab => Some(KeyAction::PrevFocus),
        KeyCode::Char('p') => Some(KeyAction::Pause),
        KeyCode::Char('r') => Some(KeyAction::Resume),
        KeyCode::Char('c') => Some(KeyAction::CancelAll),
        KeyCode::Char('f') => Some(KeyAction::FlattenNow),
        KeyCode::Char('s') => Some(KeyAction::ShutdownAfterFlatten),
        KeyCode::Char('l') => Some(KeyAction::ToggleLocale),
        KeyCode::Enter => Some(KeyAction::Confirm),
        KeyCode::Esc | KeyCode::Char('n') => Some(KeyAction::Cancel),
        KeyCode::Char('q') => Some(KeyAction::Quit),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::map_key_event;
    use crate::events::KeyAction;

    #[test]
    fn plain_c_opens_cancel_all_confirm() {
        let action = map_key_event(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));

        assert_eq!(action, Some(KeyAction::CancelAll));
    }

    #[test]
    fn ctrl_c_quits_instead_of_opening_cancel_all_confirm() {
        let action = map_key_event(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));

        assert_eq!(action, Some(KeyAction::Quit));
    }

    #[test]
    fn ctrl_modified_danger_shortcuts_are_ignored() {
        let action = map_key_event(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL));

        assert_eq!(action, None);
    }

    #[test]
    fn shifted_question_mark_still_opens_help() {
        let action = map_key_event(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::SHIFT));

        assert_eq!(action, Some(KeyAction::ToggleHelp));
    }

    #[test]
    fn plain_l_toggles_locale() {
        let action = map_key_event(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE));

        assert_eq!(action, Some(KeyAction::ToggleLocale));
    }

    #[test]
    fn plain_right_bracket_cycles_to_next_instance() {
        let action = map_key_event(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE));

        assert_eq!(action, Some(KeyAction::NextInstance));
    }

    #[test]
    fn plain_left_bracket_cycles_to_previous_instance() {
        let action = map_key_event(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE));

        assert_eq!(action, Some(KeyAction::PrevInstance));
    }
}
