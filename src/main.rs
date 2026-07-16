use std::{io, path::PathBuf};

use anyhow::{Context, Result, bail};
use clap::Parser;
use latte_lens::{
    app::App,
    navigation::{AppOptions, NavigationSettings},
    preview::PreviewRegistry,
};
#[cfg(not(windows))]
use ratatui::crossterm::event::{
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
};

/// See what your agents are changing.
#[derive(Debug, Parser)]
#[command(name = "latte-lens", version, about)]
struct Cli {
    /// Repository or directory to inspect.
    #[arg(default_value = ".")]
    path: PathBuf,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let workspace = cli
        .path
        .canonicalize()
        .with_context(|| format!("cannot open {}", cli.path.display()))?;
    if !workspace.is_dir() {
        bail!("{} is not a directory", workspace.display());
    }
    let loaded = NavigationSettings::load_user_config(&workspace);
    let mut app = App::with_options(
        workspace,
        PreviewRegistry::with_builtins(),
        AppOptions {
            navigation: loaded.settings,
            navigation_config_warning: loaded.warning,
        },
    )?;

    ratatui::run(|terminal| -> io::Result<()> {
        let _terminal_input = TerminalInputGuard::enable()?;
        app.run(terminal)
    })?;
    Ok(())
}

struct TerminalInputGuard {
    #[cfg(not(windows))]
    keyboard_enhanced: bool,
}

impl TerminalInputGuard {
    fn enable() -> io::Result<Self> {
        let mut stdout = io::stdout();
        execute!(stdout, EnableMouseCapture)?;
        #[cfg(not(windows))]
        let keyboard_enhanced = execute!(
            stdout,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )
        .is_ok();
        Ok(Self {
            #[cfg(not(windows))]
            keyboard_enhanced,
        })
    }
}

impl Drop for TerminalInputGuard {
    fn drop(&mut self) {
        let mut stdout = io::stdout();
        #[cfg(not(windows))]
        if self.keyboard_enhanced {
            let _ = execute!(stdout, PopKeyboardEnhancementFlags);
        }
        let _ = execute!(stdout, DisableMouseCapture);
    }
}
