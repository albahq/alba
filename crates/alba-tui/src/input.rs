//! Maps terminal input events to user intents.
//!
//! This module is pure: it never touches state or the filesystem. The
//! keymap reads a `crossterm::event::Event` and a `Mode` and returns an
//! `Action` naming what the user asked for — the event loop later
//! resolves that intent into a state mutation and/or a command to the
//! engine.

use crate::state::Mode;
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
};

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
    FollowTail,     // G
    EnterSearch,    // /
    EnterCopy,      // v
    EnterGraph,     // g
    EnterHelp,      // ?
    SearchNext,     // n: step the committed search forward, wrapping
    SearchPrevious, // N: step the committed search backward, wrapping
    LeaveMode,      // Esc
    Key(KeyEvent),  // anything else, for the active mode to interpret
    Mouse(MouseEvent),
    None,
}

/// Checks if a modifier set contains keys that should gate a binding.
/// SHIFT is allowed (it's just the character's case), but CONTROL, ALT,
/// and SUPER prevent the binding from firing.
fn has_disallowed_modifiers(modifiers: KeyModifiers) -> bool {
    modifiers.contains(KeyModifiers::CONTROL)
        || modifiers.contains(KeyModifiers::ALT)
        || modifiers.contains(KeyModifiers::SUPER)
}

/// Maps a terminal event and current mode to a user intent.
pub fn action_for(event: &Event, mode: &Mode) -> Action {
    // Ctrl-C is handled uniformly across all modes before mode dispatch.
    // Only Press and Repeat trigger actions; Release is ignored to prevent
    // double-firing on Windows where both Press and Release are reported.
    match event {
        Event::Key(key_event)
            if matches!(key_event.kind, KeyEventKind::Press | KeyEventKind::Repeat)
                && key_event.modifiers.contains(KeyModifiers::CONTROL)
                && key_event.code == KeyCode::Char('c') =>
        {
            return Action::CancelOrQuit;
        }
        _ => {}
    }

    match mode {
        Mode::Normal => action_in_normal_mode(event),
        _ => action_in_modal_mode(event),
    }
}

/// In Normal mode, all keybindings apply.
fn action_in_normal_mode(event: &Event) -> Action {
    match event {
        Event::Key(key_event) => {
            // Only Press and Repeat trigger actions; Release is ignored.
            // This prevents double-firing on Windows where both are reported.
            if !matches!(key_event.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                return Action::None;
            }

            // Ctrl-C is already handled in action_for; this point is unreachable
            // for Ctrl-C but we need to gate character bindings against other
            // modifiers (Alt, Super, and for non-char codes, Control).

            // Single character keys: gate out CONTROL, ALT, SUPER.
            // SHIFT is allowed and irrelevant (the character already carries case).
            if !has_disallowed_modifiers(key_event.modifiers) {
                match key_event.code {
                    KeyCode::Char('q') => return Action::Quit,
                    KeyCode::Char('r') => return Action::Rerun { force: false },
                    KeyCode::Char('f') => return Action::Rerun { force: true },
                    KeyCode::Char('c') => return Action::CancelRun,
                    KeyCode::Char('w') => return Action::ToggleWatch,
                    KeyCode::Char('/') => return Action::EnterSearch,
                    KeyCode::Char('v') => return Action::EnterCopy,
                    KeyCode::Char('g') => return Action::EnterGraph,
                    KeyCode::Char('?') => return Action::EnterHelp,
                    KeyCode::Char('G') => return Action::FollowTail,
                    KeyCode::Char('j') => return Action::SelectNext,
                    KeyCode::Char('k') => return Action::SelectPrevious,
                    KeyCode::Char('n') => return Action::SearchNext,
                    KeyCode::Char('N') => return Action::SearchPrevious,
                    _ => {}
                }
            }

            // Arrow keys and Esc don't need modifier gating; they're unambiguous.
            match key_event.code {
                KeyCode::Down => return Action::SelectNext,
                KeyCode::Up => return Action::SelectPrevious,
                KeyCode::Esc => return Action::LeaveMode,
                _ => {}
            }

            // Everything else passes through for potential interpretation.
            Action::Key(*key_event)
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
            // Only Press and Repeat trigger actions; Release is ignored.
            if !matches!(key_event.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                return Action::None;
            }

            // Ctrl-C is already handled in action_for; Esc leaves any mode.
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
    use crate::search::SearchState;
    use crossterm::event::{
        Event, KeyCode, KeyEvent, KeyEventState, KeyModifiers, MouseEvent, MouseEventKind,
    };

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn key_with_kind(code: KeyCode, kind: KeyEventKind) -> Event {
        Event::Key(KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind,
            state: KeyEventState::NONE,
        })
    }

    fn key_with_modifiers(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, modifiers))
    }

    fn ctrl(letter: char) -> Event {
        Event::Key(KeyEvent::new(KeyCode::Char(letter), KeyModifiers::CONTROL))
    }

    fn mouse_event(kind: MouseEventKind) -> Event {
        Event::Mouse(MouseEvent {
            kind,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        })
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
        assert_eq!(
            action_for(&key(KeyCode::Char('n')), &mode),
            Action::SearchNext
        );
        assert_eq!(
            action_for(&key(KeyCode::Char('N')), &mode),
            Action::SearchPrevious
        );
    }

    /// Ctrl-C is one action whose meaning the event loop resolves against
    /// the run state — the keymap itself never asks "is a run in flight".
    #[test]
    fn ctrl_c_maps_to_cancel_or_quit_in_every_mode() {
        for mode in [
            Mode::Normal,
            Mode::Search(SearchState::new()),
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
            action_for(&key(KeyCode::Char('q')), &Mode::Search(SearchState::new())),
            Action::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE))
        );
        assert_eq!(
            action_for(&key(KeyCode::Esc), &Mode::Search(SearchState::new())),
            Action::LeaveMode
        );
    }

    /// Release events must not fire actions (Windows reports both Press
    /// and Release for every key; ignoring Release prevents double-firing).
    #[test]
    fn release_events_do_not_fire_actions() {
        let mode = Mode::Normal;
        let event = key_with_kind(KeyCode::Char('q'), KeyEventKind::Release);
        assert_eq!(action_for(&event, &mode), Action::None);
    }

    /// Repeat events (key held down) must fire actions, not just Press.
    /// Scrolling up/down with j/k requires Repeat to keep working.
    #[test]
    fn repeat_events_fire_actions() {
        let mode = Mode::Normal;
        let event = key_with_kind(KeyCode::Char('j'), KeyEventKind::Repeat);
        assert_eq!(action_for(&event, &mode), Action::SelectNext);
    }

    /// Modified keys (Ctrl, Alt, Super) must not fire unmodified bindings.
    /// Ctrl-q passes through as a key, not as Quit.
    #[test]
    fn ctrl_modifies_keys_prevent_unmodified_bindings() {
        let mode = Mode::Normal;
        let event = key_with_modifiers(KeyCode::Char('q'), KeyModifiers::CONTROL);
        // Ctrl-q should pass through as Key, not fire Quit.
        // (Ctrl-C is special-cased; it fires CancelOrQuit before mode dispatch.)
        match action_for(&event, &mode) {
            Action::Key(ke) => {
                assert_eq!(ke.code, KeyCode::Char('q'));
                assert!(ke.modifiers.contains(KeyModifiers::CONTROL));
            }
            other => panic!("Expected Action::Key, got {:?}", other),
        }
    }

    /// Alt modifies keys prevent unmodified bindings.
    #[test]
    fn alt_modifies_keys_prevent_unmodified_bindings() {
        let mode = Mode::Normal;
        let event = key_with_modifiers(KeyCode::Char('r'), KeyModifiers::ALT);
        match action_for(&event, &mode) {
            Action::Key(ke) => {
                assert_eq!(ke.code, KeyCode::Char('r'));
                assert!(ke.modifiers.contains(KeyModifiers::ALT));
            }
            other => panic!("Expected Action::Key, got {:?}", other),
        }
    }

    /// SHIFT is allowed for character bindings (the character already
    /// carries case). G with SHIFT must still map to FollowTail.
    #[test]
    fn shift_does_not_prevent_character_bindings() {
        let mode = Mode::Normal;
        // G naturally arrives with SHIFT from terminals, since it's uppercase.
        let event = key_with_modifiers(KeyCode::Char('G'), KeyModifiers::SHIFT);
        assert_eq!(action_for(&event, &mode), Action::FollowTail);
    }

    /// `n`/`N` step the committed search, gated exactly like every other
    /// character binding on this file: Release does not fire, Repeat
    /// does, and CONTROL/ALT/SUPER block the binding while SHIFT (the
    /// only way `N` naturally arrives) does not.
    #[test]
    fn search_step_keys_are_gated_like_every_other_binding() {
        let mode = Mode::Normal;
        assert_eq!(
            action_for(&key(KeyCode::Char('n')), &mode),
            Action::SearchNext
        );
        assert_eq!(
            action_for(&key(KeyCode::Char('N')), &mode),
            Action::SearchPrevious
        );
        assert_eq!(
            action_for(
                &key_with_kind(KeyCode::Char('n'), KeyEventKind::Release),
                &mode
            ),
            Action::None
        );
        assert_eq!(
            action_for(
                &key_with_kind(KeyCode::Char('n'), KeyEventKind::Repeat),
                &mode
            ),
            Action::SearchNext
        );
        assert_eq!(
            action_for(
                &key_with_modifiers(KeyCode::Char('N'), KeyModifiers::SHIFT),
                &mode
            ),
            Action::SearchPrevious,
            "SHIFT is just how 'N' arrives, not a gate"
        );
        for modifiers in [
            KeyModifiers::CONTROL,
            KeyModifiers::ALT,
            KeyModifiers::SUPER,
        ] {
            match action_for(&key_with_modifiers(KeyCode::Char('n'), modifiers), &mode) {
                Action::Key(ke) => assert_eq!(ke.code, KeyCode::Char('n')),
                other => panic!(
                    "Expected Action::Key for modifiers {:?}, got {:?}",
                    modifiers, other
                ),
            }
        }
    }

    /// Mouse wheel up in Normal mode maps to ScrollUp(3).
    #[test]
    fn mouse_wheel_up_scrolls_in_normal_mode() {
        let mode = Mode::Normal;
        assert_eq!(
            action_for(&mouse_event(MouseEventKind::ScrollUp), &mode),
            Action::ScrollUp(3)
        );
    }

    /// Mouse wheel down in Normal mode maps to ScrollDown(3).
    #[test]
    fn mouse_wheel_down_scrolls_in_normal_mode() {
        let mode = Mode::Normal;
        assert_eq!(
            action_for(&mouse_event(MouseEventKind::ScrollDown), &mode),
            Action::ScrollDown(3)
        );
    }

    /// Mouse wheel events in modal modes pass through as Mouse actions
    /// (for potential interpretation by the mode's handler).
    #[test]
    fn mouse_wheel_passes_through_in_modal_modes() {
        for mode in [
            Mode::Search(SearchState::new()),
            Mode::Copy,
            Mode::Graph,
            Mode::Help,
        ] {
            let evt = mouse_event(MouseEventKind::ScrollUp);
            match action_for(&evt, &mode) {
                Action::Mouse(me) => {
                    assert_eq!(me.kind, MouseEventKind::ScrollUp);
                }
                other => panic!(
                    "Expected Action::Mouse for mode {:?}, got {:?}",
                    mode, other
                ),
            }
        }
    }

    /// Mouse clicks (e.g., Left) in Normal mode pass through to the UI.
    #[test]
    fn mouse_click_passes_through_in_normal_mode() {
        let mode = Mode::Normal;
        let evt = mouse_event(MouseEventKind::Down(crossterm::event::MouseButton::Left));
        match action_for(&evt, &mode) {
            Action::Mouse(me) => {
                assert_eq!(
                    me.kind,
                    MouseEventKind::Down(crossterm::event::MouseButton::Left)
                );
            }
            other => panic!("Expected Action::Mouse, got {:?}", other),
        }
    }

    /// Mouse clicks in modal modes pass through too.
    #[test]
    fn mouse_click_passes_through_in_modal_modes() {
        for mode in [
            Mode::Search(SearchState::new()),
            Mode::Copy,
            Mode::Graph,
            Mode::Help,
        ] {
            let evt = mouse_event(MouseEventKind::Down(crossterm::event::MouseButton::Right));
            match action_for(&evt, &mode) {
                Action::Mouse(me) => {
                    assert_eq!(
                        me.kind,
                        MouseEventKind::Down(crossterm::event::MouseButton::Right)
                    );
                }
                other => panic!(
                    "Expected Action::Mouse for mode {:?}, got {:?}",
                    mode, other
                ),
            }
        }
    }

    /// Ctrl-C with Release must not fire CancelOrQuit (Windows reports both
    /// Press and Release for every key; the Release must be filtered).
    #[test]
    fn ctrl_c_release_does_not_fire_in_any_mode() {
        let ctrl_c_release = Event::Key(KeyEvent {
            code: KeyCode::Char('c'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Release,
            state: KeyEventState::NONE,
        });
        for mode in [
            Mode::Normal,
            Mode::Search(SearchState::new()),
            Mode::Copy,
            Mode::Graph,
            Mode::Help,
        ] {
            assert_eq!(
                action_for(&ctrl_c_release, &mode),
                Action::None,
                "Ctrl-C Release should not fire in mode {:?}",
                mode
            );
        }
    }

    /// Ctrl-C with Press must fire CancelOrQuit in all modes.
    #[test]
    fn ctrl_c_press_fires_in_all_modes() {
        let ctrl_c_press = Event::Key(KeyEvent {
            code: KeyCode::Char('c'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        });
        for mode in [
            Mode::Normal,
            Mode::Search(SearchState::new()),
            Mode::Copy,
            Mode::Graph,
            Mode::Help,
        ] {
            assert_eq!(
                action_for(&ctrl_c_press, &mode),
                Action::CancelOrQuit,
                "Ctrl-C Press should fire CancelOrQuit in mode {:?}",
                mode
            );
        }
    }
}
