use std::{io, path::PathBuf};

#[cfg(feature = "agent-observability")]
use std::{
    env,
    ffi::OsStr,
    io::Read,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use clap::Parser;
#[cfg(feature = "agent-observability")]
use clap::{Args, Subcommand};
#[cfg(feature = "agent-observability")]
use latte_lens::agent::*;
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
    #[cfg(feature = "agent-observability")]
    #[command(subcommand)]
    command: Option<Command>,

    /// Repository or directory to inspect.
    #[arg(default_value = ".")]
    path: PathBuf,
}

#[cfg(feature = "agent-observability")]
#[derive(Debug, Subcommand)]
enum Command {
    /// Receive one bounded Code Agent hook event without starting the TUI.
    Hook(HookArgs),
    /// Install or restore user-level Code Agent hook configuration.
    Hooks(HooksArgs),
}

#[cfg(feature = "agent-observability")]
#[derive(Debug, Args)]
struct HookArgs {
    #[arg(long)]
    observer: String,
    #[arg(long)]
    event: String,
    #[arg(long)]
    observer_version: Option<String>,
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
}

#[cfg(feature = "agent-observability")]
#[derive(Debug, Args)]
struct HooksArgs {
    #[command(subcommand)]
    command: HooksCommand,
}

#[cfg(feature = "agent-observability")]
#[derive(Debug, Subcommand)]
enum HooksCommand {
    /// Merge Latte Lens hooks into every existing user-level Agent config.
    Setup,
    /// Restore the exact pre-setup files when they have not changed since setup.
    Restore {
        /// Transaction identifier printed by `hooks setup`.
        transaction_id: String,
    },
}

#[cfg(feature = "agent-observability")]
const HOOK_LIVE_DEADLINE: Duration = Duration::from_millis(5);
#[cfg(feature = "agent-observability")]
const HOOK_METADATA_FALLBACK_BUDGET: Duration = Duration::from_millis(2);

fn main() -> Result<()> {
    #[cfg(feature = "agent-observability")]
    let hook_requested = env::args_os()
        .nth(1)
        .is_some_and(|argument| argument == OsStr::new("hook"));
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            #[cfg(feature = "agent-observability")]
            if hook_requested {
                return Ok(());
            }
            error.exit()
        }
    };

    #[cfg(feature = "agent-observability")]
    if let Some(command) = cli.command {
        match command {
            Command::Hook(hook) => {
                let _ = run_hook(hook);
                return Ok(());
            }
            Command::Hooks(hooks) => return run_hooks_command(hooks),
        }
    }

    run_tui(cli.path)
}

#[cfg(feature = "agent-observability")]
fn run_hooks_command(hooks: HooksArgs) -> Result<()> {
    let options = HookSetupOptions::from_environment(env::current_exe()?)?;
    match hooks.command {
        HooksCommand::Setup => {
            let report = setup_user_hooks(options)?;
            for agent in &report.configured {
                println!("configured {agent}");
            }
            for agent in &report.skipped {
                println!("skipped {agent}: configuration directory not found");
            }
            if let (Some(transaction), Some(backup)) =
                (report.transaction_id, report.backup_directory)
            {
                println!("hook setup transaction: {transaction}");
                println!("recovery backup: {}", backup.display());
            } else {
                println!("hooks already up to date");
            }
        }
        HooksCommand::Restore { transaction_id } => {
            let report = restore_user_hooks(options, &transaction_id)?;
            for agent in &report.restored {
                println!("restored {agent}");
            }
            println!("restored hook setup transaction: {}", report.transaction_id);
        }
    }
    Ok(())
}

fn run_tui(path: PathBuf) -> Result<()> {
    let workspace = path
        .canonicalize()
        .with_context(|| format!("cannot open {}", path.display()))?;
    if !workspace.is_dir() {
        bail!("{} is not a directory", workspace.display());
    }
    let loaded = NavigationSettings::load_user_config(&workspace);
    let mut app = App::with_options(
        workspace.clone(),
        PreviewRegistry::with_builtins(),
        AppOptions {
            navigation: loaded.settings,
            navigation_config_warning: loaded.warning,
        },
    )?;
    #[cfg(feature = "agent-observability")]
    if let Ok(agent) = start_production_agent_runtime(&workspace) {
        let _ = app.attach_agent_runtime(agent.runtime, agent.selector);
    }

    ratatui::run(|terminal| -> io::Result<()> {
        let _terminal_input = TerminalInputGuard::enable()?;
        app.run(terminal)
    })?;
    Ok(())
}

#[cfg(feature = "agent-observability")]
fn run_hook(hook: HookArgs) -> Result<(), ()> {
    let observer = ObserverId::parse(hook.observer).map_err(|_| ())?;
    let adapters = production_adapter_registry();
    if adapters.resolve(&observer).is_none() {
        return Ok(());
    }

    let mut payload = Vec::with_capacity(1024);
    Read::by_ref(&mut io::stdin())
        .take(MAX_ADAPTER_INPUT_BYTES as u64 + 1)
        .read_to_end(&mut payload)
        .map_err(|_| ())?;
    if payload.len() > MAX_ADAPTER_INPUT_BYTES {
        return Ok(());
    }

    let state_root = resolve_state_root_from_environment().map_err(|_| ())?;
    let identity = load_or_create_install_identity(state_root.clone()).map_err(|_| ())?;
    let workspace = resolve_workspace(&hook.workspace, &identity).map_err(|_| ())?;
    let install = identity.install_id().clone();
    let metadata = FilesystemMetadataStore::new(state_root, install.clone()).map_err(|_| ())?;
    let registry = FilesystemLiveReceiverRegistry::new(
        resolve_runtime_root_from_environment().map_err(|_| ())?,
        install,
    )
    .map_err(|_| ())?;
    let publisher = RegistryLivePublisher::new(registry, workspace.primary().clone());
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64;
    let live_deadline = Instant::now() + HOOK_LIVE_DEADLINE;
    let _ = emit_hook_invocation(
        HookInvocation {
            observer: &observer,
            event_name: &hook.event,
            observer_version: hook.observer_version.as_deref(),
            observed_at: Timestamp::from_unix_millis(now),
            workspace: workspace.primary().clone(),
            payload: &payload,
        },
        &adapters,
        &identity,
        &publisher,
        &metadata,
        live_deadline,
        HOOK_METADATA_FALLBACK_BUDGET,
    );
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
