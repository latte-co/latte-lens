use std::{io, path::PathBuf};

use anyhow::Result;
use clap::Parser;
use lattelens::app::App;
use ratatui::crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
};

/// See what your agents are changing.
#[derive(Debug, Parser)]
#[command(name = "lattelens", version, about)]
struct Cli {
    /// Repository or directory to inspect.
    #[arg(default_value = ".")]
    path: PathBuf,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut app = App::new(cli.path)?;

    ratatui::run(|terminal| -> io::Result<()> {
        let _mouse_capture = MouseCaptureGuard::enable()?;
        app.run(terminal)
    })?;
    Ok(())
}

struct MouseCaptureGuard;

impl MouseCaptureGuard {
    fn enable() -> io::Result<Self> {
        execute!(io::stdout(), EnableMouseCapture)?;
        Ok(Self)
    }
}

impl Drop for MouseCaptureGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), DisableMouseCapture);
    }
}
