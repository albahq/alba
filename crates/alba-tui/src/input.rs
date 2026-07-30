//! Maps terminal input events to user intents.
//!
//! This module is pure: it never touches state or the filesystem. The
//! keymap reads a `crossterm::event::Event` and a `Mode` and returns an
//! `Action` naming what the user asked for — the event loop later
//! resolves that intent into a state mutation and/or a command to the
//! engine.

use crate::state::Mode;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};

/// What a terminal event asks the app to do. The event loop translates
/// Action -> state mutation and/or SessionCommand; this function only
/// names the intent, which is what makes the keymap testable.
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Action {
    Quit,         // q
    CancelOrQuit, // Ctrl-C: cancel if running, else quit
    SelectNext,
    SelectPrevious,
    Rerun { force: bool }, // r / f on the selected beam
    CancelRun,             // c
    ToggleWatch,           // w
    ScrollUp(usize),
    ScrollDown(usize),
    FollowTail,    // G
    EnterSearch,   // /
    EnterCopy,     // v
    EnterGraph,    // g
    EnterHelp,     // ?
    LeaveMode,     // Esc
    Key(KeyEvent), // anything else, for the active mode to interpret
    Mouse(MouseEvent),
    None,
}

/// Maps a terminal event and current mode to a user intent.
pub fn action_for(event: &Event, mode: &Mode) -> Action {
    match mode {
        Mode::Normal => action_in_normal_mode(event),
        _ => action_in_modal_mode(event),
    }
}

/// In Normal mode, all keybindings apply.
fn action_in_normal_mode(event: &Event) -> Action {
    match event {
        Event::Key(key_event) => {
            // Ctrl-C always means cancel or quit
            if key_event.modifiers.contains(KeyModifiers::CONTROL)
                && key_event.code == KeyCode::Char('c')
            {
                return Action::CancelOrQuit;
            }

            // Single character keys
            match key_event.code {
                KeyCode::Char('q') => Action::Quit,
                KeyCode::Char('r') => Action::Rerun { force: false },
                KeyCode::Char('f') => Action::Rerun { force: true },
                KeyCode::Char('c') => Action::CancelRun,
                KeyCode::Char('w') => Action::ToggleWatch,
                KeyCode::Char('/') => Action::EnterSearch,
                KeyCode::Char('v') => Action::EnterCopy,
                KeyCode::Char('g') => Action::EnterGraph,
                KeyCode::Char('?') => Action::EnterHelp,
                KeyCode::Char('G') => Action::FollowTail,
                KeyCode::Char('j') => Action::SelectNext,
                KeyCode::Char('k') => Action::SelectPrevious,
                KeyCode::Down => Action::SelectNext,
                KeyCode::Up => Action::SelectPrevious,
                KeyCode::Esc => Action::LeaveMode,
                _ => Action::Key(*key_event),
            }
        }
        Event::Mouse(mouse_event) => match mouse_event.kind {
            MouseEventKind::ScrollUp => Action::ScrollUp(3),
            MouseEventKind::ScrollDown => Action::ScrollDown(3),
            _ => Action::Mouse(*mouse_event),
        },
        _ => Action::None,
    }
}

/// In modal modes (Search, Copy, Graph, Help), only Esc and Ctrl-C are special;
/// everything else passes through for the mode's own handler.
fn action_in_modal_mode(event: &Event) -> Action {
    match event {
        Event::Key(key_event) => {
            // Ctrl-C always means cancel or quit
            if key_event.modifiers.contains(KeyModifiers::CONTROL)
                && key_event.code == KeyCode::Char('c')
            {
                return Action::CancelOrQuit;
            }

            // Esc always leaves the mode
            if key_event.code == KeyCode::Esc {
                return Action::LeaveMode;
            }

            // Everything else is for the mode to interpret
            Action::Key(*key_event)
        }
        Event::Mouse(mouse_event) => Action::Mouse(*mouse_event),
        _ => Action::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }
    fn ctrl(letter: char) -> Event {
        Event::Key(KeyEvent::new(KeyCode::Char(letter), KeyModifiers::CONTROL))
    }

    #[test]
    fn normal_mode_maps_the_spec_keys() {
        let mode = Mode::Normal;
        assert_eq!(action_for(&key(KeyCode::Char('q')), &mode), Action::Quit);
        assert_eq!(
            action_for(&key(KeyCode::Char('r')), &mode),
            Action::Rerun { force: false }
        );
        assert_eq!(
            action_for(&key(KeyCode::Char('f')), &mode),
            Action::Rerun { force: true }
        );
        assert_eq!(
            action_for(&key(KeyCode::Char('c')), &mode),
            Action::CancelRun
        );
        assert_eq!(
            action_for(&key(KeyCode::Char('w')), &mode),
            Action::ToggleWatch
        );
        assert_eq!(
            action_for(&key(KeyCode::Char('/')), &mode),
            Action::EnterSearch
        );
        assert_eq!(
            action_for(&key(KeyCode::Char('v')), &mode),
            Action::EnterCopy
        );
        assert_eq!(
            action_for(&key(KeyCode::Char('g')), &mode),
            Action::EnterGraph
        );
        assert_eq!(
            action_for(&key(KeyCode::Char('?')), &mode),
            Action::EnterHelp
        );
        assert_eq!(
            action_for(&key(KeyCode::Char('G')), &mode),
            Action::FollowTail
        );
        assert_eq!(action_for(&key(KeyCode::Down), &mode), Action::SelectNext);
        assert_eq!(
            action_for(&key(KeyCode::Char('j')), &mode),
            Action::SelectNext
        );
        assert_eq!(action_for(&key(KeyCode::Up), &mode), Action::SelectPrevious);
        assert_eq!(
            action_for(&key(KeyCode::Char('k')), &mode),
            Action::SelectPrevious
        );
    }

    /// Ctrl-C is one action whose meaning the event loop resolves against
    /// the run state — the keymap itself never asks "is a run in flight".
    #[test]
    fn ctrl_c_maps_to_cancel_or_quit_in_every_mode() {
        for mode in [
            Mode::Normal,
            Mode::Search,
            Mode::Copy,
            Mode::Graph,
            Mode::Help,
        ] {
            assert_eq!(action_for(&ctrl('c'), &mode), Action::CancelOrQuit);
        }
    }

    /// Other modes swallow normal keys: `q` while searching is the letter
    /// q, not quit.
    #[test]
    fn modal_input_passes_ordinary_keys_through() {
        assert_eq!(
            action_for(&key(KeyCode::Char('q')), &Mode::Search),
            Action::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE))
        );
        assert_eq!(
            action_for(&key(KeyCode::Esc), &Mode::Search),
            Action::LeaveMode
        );
    }
}
