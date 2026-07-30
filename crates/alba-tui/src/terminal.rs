//! Terminal setup and — above all — restoration.
//!
//! A TUI that panics without restoring the terminal leaves the user's
//! shell in raw mode with no echo: the cardinal sin. So the alternate
//! screen and raw mode are held by an RAII guard, doubled by a panic
//! hook that restores *before* the panic message prints — the message
//! must land on a terminal that can display it.
//!
//! **Invariant:** no exit path from [`TerminalGuard::enter`] leaves the
//! terminal altered. Any error before the guard is returned must restore
//! the terminal to its original state.

use std::io::{self, Stdout};

use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

pub struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalGuard {
    pub fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(error) = crossterm::execute!(stdout, EnterAlternateScreen, EnableMouseCapture) {
            restore();
            return Err(error);
        }

        // Restore before the default hook prints, chaining to it so the
        // panic message itself is not lost.
        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore();
            default_hook(info);
        }));

        let terminal = Terminal::new(CrosstermBackend::new(io::stdout())).inspect_err(|_| {
            restore();
        })?;
        Ok(Self { terminal })
    }

    pub fn terminal(&mut self) -> &mut Terminal<CrosstermBackend<Stdout>> {
        &mut self.terminal
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore();
    }
}

/// Idempotent, best-effort: called from the panic hook and from `Drop`,
/// possibly both. Errors are ignored — there is nothing to report them
/// to that this very restoration is not trying to fix.
fn restore() {
    let _ = disable_raw_mode();
    let _ = crossterm::execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
}
