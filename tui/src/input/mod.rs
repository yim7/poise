use crossterm::event::{KeyCode, KeyEvent};

use crate::events::KeyAction;

pub fn map_key_event(event: KeyEvent) -> Option<KeyAction> {
    match event.code {
        KeyCode::Char('1') => Some(KeyAction::ViewDashboard),
        KeyCode::Char('2') => Some(KeyAction::ViewGrid),
        KeyCode::Char('3') => Some(KeyAction::ViewMarket),
        KeyCode::Char('4') => Some(KeyAction::ViewEvents),
        KeyCode::Char('?') => Some(KeyAction::ToggleHelp),
        KeyCode::Tab => Some(KeyAction::NextFocus),
        KeyCode::BackTab => Some(KeyAction::PrevFocus),
        KeyCode::Char('p') => Some(KeyAction::Pause),
        KeyCode::Char('r') => Some(KeyAction::Resume),
        KeyCode::Char('c') => Some(KeyAction::CancelAll),
        KeyCode::Char('f') => Some(KeyAction::FlattenNow),
        KeyCode::Char('s') => Some(KeyAction::ShutdownAfterFlatten),
        KeyCode::Enter => Some(KeyAction::Confirm),
        KeyCode::Esc | KeyCode::Char('n') => Some(KeyAction::Cancel),
        KeyCode::Char('q') => Some(KeyAction::Quit),
        _ => None,
    }
}
