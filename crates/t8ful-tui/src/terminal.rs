use std::io;

use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use ratatui::crossterm::execute;

/// Owns the alternate-screen terminal and restores it on every exit path.
///
/// `ratatui::try_init` installs a panic hook that restores the terminal, and
/// `Drop` covers normal returns and errors. When mouse capture is requested,
/// a wrapper hook disables it before the ratatui restore hook runs.
pub struct TerminalGuard {
    terminal: DefaultTerminal,
    mouse: bool,
    restored: bool,
}

impl TerminalGuard {
    pub fn enter(mouse: bool) -> io::Result<Self> {
        match ratatui::try_init() {
            Ok(terminal) => {
                if mouse {
                    // Capture is best-effort: a terminal that rejects it still
                    // gets a usable keyboard-only interface.
                    let _ = execute!(io::stdout(), EnableMouseCapture);

                    let previous = std::panic::take_hook();

                    std::panic::set_hook(Box::new(move |info| {
                        let _ = execute!(io::stdout(), DisableMouseCapture);
                        previous(info);
                    }));
                }

                Ok(Self {
                    terminal,
                    mouse,
                    restored: false,
                })
            }
            Err(error) => {
                // Raw mode may already be enabled when a later step fails.
                ratatui::restore();

                Err(error)
            }
        }
    }

    pub fn terminal(&mut self) -> &mut DefaultTerminal {
        &mut self.terminal
    }

    pub fn restore(&mut self) {
        if self.restored {
            return;
        }

        if self.mouse {
            let _ = execute!(io::stdout(), DisableMouseCapture);
        }

        ratatui::restore();
        self.restored = true;
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.restore();
    }
}
