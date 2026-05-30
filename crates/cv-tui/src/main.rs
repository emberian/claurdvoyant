//! cv-tui — a delightful terminal browser for claurdvoyant sessions.
//!
//! Architecture:
//! - [`app::App`] holds all UI state and the bulk of the input/business logic.
//! - [`ui::draw`] is a pure(-ish) render of `App` onto a ratatui [`Frame`].
//! - The event loop here owns the terminal, polls crossterm events, dispatches keys to `App`,
//!   and redraws. Terminal raw-mode/alt-screen is *always* restored — on clean exit, on error,
//!   and on panic (via a panic hook) — so a crash never leaves the user's shell wedged.

mod app;
mod ui;

use std::io::{self, Stdout};
use std::panic;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use app::App;

type Tui = Terminal<CrosstermBackend<Stdout>>;

fn main() -> Result<()> {
    install_panic_hook();

    let mut terminal = setup_terminal()?;
    let res = run(&mut terminal);
    // Restore the terminal regardless of how `run` ended.
    restore_terminal(&mut terminal).ok();

    // Surface any error *after* the terminal is back to a sane state.
    if let Err(e) = res {
        eprintln!("cv-tui error: {e:#}");
        std::process::exit(1);
    }
    Ok(())
}

/// The core event loop: redraw, wait for input, dispatch, repeat until the app says to quit.
fn run(terminal: &mut Tui) -> Result<()> {
    let mut app = App::new();
    app.refresh_sessions();

    while !app.should_quit {
        terminal.draw(|f| ui::draw(f, &mut app))?;

        // Poll so the loop can wake periodically (lets a future spinner animate and keeps the
        // app responsive to resizes) without busy-spinning the CPU.
        if event::poll(Duration::from_millis(200))? {
            match event::read()? {
                Event::Key(key) => app.on_key(key),
                Event::Resize(_, _) => { /* redraw happens next iteration */ }
                _ => {}
            }
        }
    }
    Ok(())
}

fn setup_terminal() -> Result<Tui> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    // If entering the alternate screen fails after raw mode is on, undo raw mode so we don't
    // leave the user's shell in a half-configured state.
    if let Err(e) = execute!(stdout, EnterAlternateScreen) {
        let _ = disable_raw_mode();
        return Err(e.into());
    }
    let terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    Ok(terminal)
}

fn restore_terminal(terminal: &mut Tui) -> Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

/// Best-effort raw teardown used by the panic hook (we don't have the `Terminal` there).
fn restore_terminal_raw() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen);
}

/// Ensure a panic anywhere in the UI still leaves the terminal usable, then prints the panic.
fn install_panic_hook() {
    let original = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        restore_terminal_raw();
        original(info);
    }));
}
