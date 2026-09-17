use std::{
    fs, io,
    path::{Path, PathBuf},
};

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
    navigation::{AppOptions, load_user_configuration},
    preview::PreviewRegistry,
};
#[cfg(not(windows))]
use ratatui::crossterm::event::{
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::{
    event::{DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture},
    execute,
    terminal::{LeaveAlternateScreen, disable_raw_mode},
};

/// See what your agents are changing.
#[derive(Debug, Parser)]
#[command(name = "latte-lens", version, about)]
struct Cli {
    #[cfg(feature = "agent-observability")]
    #[command(subcommand)]
    command: Option<Command>,

    /// Repository, directory, or file to inspect.
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

/// Best-effort restoration of terminal state. Called from the panic hook so
/// that a panic in the TUI loop doesn't leave the terminal in raw mode /
/// alternate screen (which manifests as a frozen, "crashed" terminal).
fn restore_terminal_after_panic() {
    let _ = disable_raw_mode();
    let mut stdout = io::stdout();
    let _ = execute!(stdout, LeaveAlternateScreen);
    #[cfg(not(windows))]
    let _ = execute!(stdout, PopKeyboardEnhancementFlags);
    let _ = execute!(stdout, DisableMouseCapture);
}

/// Install a panic hook that restores the terminal before delegating to the
/// default hook (which prints the panic message). Only restores for main-
/// thread panics; worker threads are already guarded by `catch_unwind`.
fn install_terminal_panic_hook() {
    let main_thread = std::thread::current().id();
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if std::thread::current().id() == main_thread {
            restore_terminal_after_panic();
        }
        default_hook(info);
    }));
}

fn run_tui(path: PathBuf) -> Result<()> {
    // `latte-lens path/to/file.md` opens a Files view holding exactly that
    // one file (like an editor opening a document). Every runtime requires a
    // directory root, so the workspace root becomes the containing directory
    // and the file travels through `AppOptions::initial_file` as a display
    // filter on the initial Files tab.
    let requested = match path.canonicalize() {
        Ok(requested) => requested,
        Err(error) => return Err(cannot_open_error(&path, error)),
    };
    let (workspace, initial_file) = if requested.is_dir() {
        (requested, None)
    } else if requested.is_file() {
        let parent = requested
            .parent()
            .map(Path::to_path_buf)
            .context("opened file has no parent directory")?;
        (parent, Some(requested))
    } else {
        bail!(
            "{} is neither a directory nor a regular file",
            requested.display()
        );
    };
    // Ensure any panic in the TUI loop restores the terminal instead of
    // leaving it frozen in raw mode / alternate screen.
    install_terminal_panic_hook();
    // Resolve startup configuration once before rendering; theme and navigation
    // failures remain isolated in the returned warnings.
    let loaded = load_user_configuration(&workspace);
    latte_lens::theme::install(loaded.theme.theme);
    let navigation_config_warning = match (loaded.navigation.warning, loaded.theme.warning) {
        (Some(navigation), Some(theme)) => Some(format!("{navigation} · {theme}")),
        (Some(navigation), None) => Some(navigation),
        (None, Some(theme)) => Some(theme),
        (None, None) => None,
    };
    let mut app = App::with_options(
        workspace.clone(),
        PreviewRegistry::with_builtins(),
        AppOptions {
            navigation: loaded.navigation.settings,
            navigation_config_warning,
            initial_file,
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

/// Build the friendliest possible error for a path that cannot be opened:
/// state what was attempted, and when the miss looks like a typo, offer the
/// closest names from the same directory instead of a bare OS error.
fn cannot_open_error(path: &Path, error: io::Error) -> anyhow::Error {
    let attempted = absolute_candidate(path);
    match error.kind() {
        io::ErrorKind::NotFound => {
            let mut message = format!(
                "cannot open {}\n\n{} does not exist",
                path.display(),
                attempted.display()
            );
            match attempted.parent() {
                Some(parent) if parent.is_dir() => {
                    match similar_entry_names(&attempted, parent).as_slice() {
                        [] => {}
                        [single] => message.push_str(&format!("\n\nDid you mean '{single}'?")),
                        many => {
                            message.push_str("\n\nDid you mean one of the following?");
                            for name in many {
                                message.push_str(&format!("\n  {name}"));
                            }
                        }
                    }
                }
                Some(parent) => message.push_str(&format!(
                    "\n\ndirectory {} does not exist",
                    parent.display()
                )),
                None => {}
            }
            anyhow::Error::msg(message)
        }
        io::ErrorKind::PermissionDenied => anyhow::Error::msg(format!(
            "cannot open {}\n\n{}: permission denied",
            path.display(),
            attempted.display()
        )),
        _ => anyhow::Error::new(error).context(format!("cannot open {}", path.display())),
    }
}

/// Resolve `path` against the current directory so error messages show where
/// the lookup actually happened, without requiring the path to exist.
fn absolute_candidate(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    match std::env::current_dir() {
        Ok(current) => current.join(path),
        Err(_) => path.to_path_buf(),
    }
}

const SUGGESTION_SCAN_LIMIT: usize = 4096;
const MAX_SUGGESTION_DISTANCE: usize = 2;
const MAX_SUGGESTIONS: usize = 3;

/// Collect the closest entry names in `parent` to the failed path's final
/// component. The scan is bounded and the result is deterministically ordered
/// so a typo surfaces a small, stable set of suggestions.
fn similar_entry_names(target: &Path, parent: &Path) -> Vec<String> {
    let Some(name) = target.file_name().and_then(|name| name.to_str()) else {
        return Vec::new();
    };
    let needle = name.to_lowercase();
    let hidden = needle.starts_with('.');
    let entries = match fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };
    let mut candidates: Vec<(usize, String)> = Vec::new();
    for entry in entries.take(SUGGESTION_SCAN_LIMIT).flatten() {
        let Ok(candidate) = entry.file_name().into_string() else {
            continue;
        };
        if candidate == name {
            continue;
        }
        let hay = candidate.to_lowercase();
        if hay.starts_with('.') != hidden {
            continue;
        }
        let distance = bounded_edit_distance(&needle, &hay, MAX_SUGGESTION_DISTANCE + 1);
        let prefix = hay.starts_with(&needle) || needle.starts_with(&hay);
        if distance <= MAX_SUGGESTION_DISTANCE || prefix {
            candidates.push((distance, candidate));
        }
    }
    candidates.sort();
    candidates.truncate(MAX_SUGGESTIONS);
    candidates.into_iter().map(|(_, name)| name).collect()
}

/// Levenshtein distance over characters, abandoning the computation (and
/// returning `cap + 1`) once no completion of the current row can land within
/// `cap` edits.
fn bounded_edit_distance(left: &str, right: &str, cap: usize) -> usize {
    let left: Vec<char> = left.chars().collect();
    let right: Vec<char> = right.chars().collect();
    if left.len().abs_diff(right.len()) > cap {
        return cap + 1;
    }
    let mut previous: Vec<usize> = (0..=right.len()).collect();
    let mut current = vec![0_usize; right.len() + 1];
    for (index, left_char) in left.iter().enumerate() {
        current[0] = index + 1;
        let mut row_minimum = current[0];
        for (position, right_char) in right.iter().enumerate() {
            let substitution = previous[position] + usize::from(left_char != right_char);
            let value = (previous[position + 1] + 1)
                .min(current[position] + 1)
                .min(substitution);
            current[position + 1] = value;
            row_minimum = row_minimum.min(value);
        }
        if row_minimum > cap {
            return cap + 1;
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[right.len()]
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
        execute!(stdout, EnableMouseCapture, EnableBracketedPaste)?;
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
        let _ = execute!(stdout, DisableMouseCapture, DisableBracketedPaste);
    }
}

#[cfg(test)]
mod tests {
    use super::{bounded_edit_distance, similar_entry_names};

    #[test]
    fn edit_distance_counts_edits_and_respects_the_cap() {
        assert_eq!(bounded_edit_distance("agents.md", "agents.md2", 3), 1);
        assert_eq!(bounded_edit_distance("abc", "abc", 3), 0);
        assert_eq!(bounded_edit_distance("a", "zzzzzzz", 2), 3);
    }

    #[test]
    fn suggestions_rank_closest_names_and_skip_unrelated_entries() {
        let sandbox = tempfile::tempdir().expect("sandbox");
        for name in ["AGENTS.md", "AGENTS.md.bak", "notes.txt", ".git"] {
            std::fs::write(sandbox.path().join(name), b"x").expect("write");
        }
        let suggestions = similar_entry_names(&sandbox.path().join("AGENTS.md2"), sandbox.path());
        assert_eq!(suggestions, vec!["AGENTS.md".to_string()]);
    }

    #[test]
    fn suggestions_break_distance_ties_deterministically() {
        let sandbox = tempfile::tempdir().expect("sandbox");
        for name in ["alphax.txt", "alpha.txt"] {
            std::fs::write(sandbox.path().join(name), b"x").expect("write");
        }
        let suggestions = similar_entry_names(&sandbox.path().join("alphay.txt"), sandbox.path());
        assert_eq!(
            suggestions,
            vec!["alpha.txt".to_string(), "alphax.txt".to_string()]
        );
    }
}
