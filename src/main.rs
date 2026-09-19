use std::{
    fs, io,
    path::{Path, PathBuf},
};

#[cfg(feature = "agent-observability")]
use std::{env, ffi::OsStr, io::Read, time::Instant};

use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use anyhow::anyhow;
use anyhow::{Context, Result, bail};
#[cfg(feature = "agent-observability")]
use clap::Args;
use clap::{Parser, Subcommand};
#[cfg(feature = "agent-observability")]
use latte_lens::agent::*;
#[cfg(unix)]
use latte_lens::ipc;
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
    #[command(subcommand)]
    command: Option<Command>,

    /// Repository, directory, or file to inspect.
    #[arg(default_value = ".")]
    path: PathBuf,

    /// Deliver the path to a running instance instead of starting a new
    /// viewer. The deepest covering workspace is used; with a single running
    /// instance anything goes to it, and with several you pick from a menu.
    /// Without any instance, a fresh viewer starts for the same path.
    /// (Subcommands run first; this flag only affects the viewer path.)
    #[arg(long)]
    attach: bool,

    /// With --attach: deliver to the instance matching this PID or root path
    /// instead of the automatic choice. Only needed to disambiguate when
    /// several instances are running from a non-interactive shell.
    #[arg(long, requires = "attach")]
    target: Option<String>,

    /// With --attach: open the path in a new Files tab instead of the
    /// active one. Files outside every workspace always open in their own
    /// new tab.
    #[arg(long, requires = "attach")]
    new_tab: bool,
}

#[derive(Debug, Subcommand)]
enum Command {
    #[cfg(feature = "agent-observability")]
    /// Receive one bounded Code Agent hook event without starting the TUI.
    Hook(HookArgs),
    #[cfg(feature = "agent-observability")]
    /// Install or restore user-level Code Agent hook configuration.
    Hooks(HooksArgs),
    /// List the Latte Lens instances running for this user.
    Ps {
        /// Emit a JSON array of instances instead of a table.
        #[arg(long)]
        json: bool,
    },
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

    if let Some(command) = cli.command {
        match command {
            #[cfg(feature = "agent-observability")]
            Command::Hook(hook) => {
                let _ = run_hook(hook);
                return Ok(());
            }
            #[cfg(feature = "agent-observability")]
            Command::Hooks(hooks) => return run_hooks_command(hooks),
            Command::Ps { json } => return run_ps(json),
        }
    }

    if cli.attach {
        return run_attach(cli.path, cli.target.as_deref(), cli.new_tab);
    }

    run_tui(cli.path)
}

/// `latte-lens --attach <path>`: hand the path to a running instance instead
/// of starting a new viewer. The instance whose workspace covers the path
/// receives it (deepest root wins); `--target` overrides that choice by PID
/// or root path. If no instance covers the path, a single running instance
/// is used automatically; with several instances a terminal session offers
/// an interactive picker (piped/non-interactive input lists the instances
/// and suggests `--target`). Without any running instance the command falls
/// back to starting a fresh viewer for the same path.
#[cfg(unix)]
fn run_attach(path: PathBuf, target: Option<&str>, new_tab: bool) -> Result<()> {
    // Resolve and validate the path first, so a typo reports suggestions
    // instead of instance-routing noise.
    let requested = match path.canonicalize() {
        Ok(requested) => requested,
        Err(error) => return Err(cannot_open_error(&path, error)),
    };
    let dir = ipc::runtime_dir().context("instance discovery is unavailable")?;
    let instances = ipc::discover(&dir);
    if instances.is_empty() {
        return run_tui(path);
    }
    let route = select_instance(&instances, target, &requested)?;
    let instance = match route {
        Route::Instance(instance) => instance,
        Route::Ambiguous(candidates) => {
            use std::io::IsTerminal;
            let stdin = std::io::stdin();
            if !stdin.is_terminal() {
                let listed = format_instance_list(candidates.iter().copied());
                bail!(
                    "no running instance covers {}\nrunning instances:\n{listed}\nre-run with \
                     --target <pid>, or run from a terminal to choose interactively",
                    requested.display()
                );
            }
            let mut reader = std::io::BufReader::new(stdin.lock());
            let mut stdout = std::io::stdout().lock();
            prompt_instance_choice(&candidates, &requested, &mut reader, &mut stdout)?
        }
    };
    let focus = if new_tab {
        ipc::Focus::NewTab
    } else {
        ipc::Focus::Active
    };
    let body = ipc::RequestBody::Open {
        paths: vec![requested.display().to_string()],
        focus,
    };
    match ipc::send(&instance.socket_path, &body) {
        Ok(ipc::ResponseBody::Opened { opened }) => {
            for opened_path in &opened {
                println!("opened {opened_path} in instance {}", instance.info.pid);
            }
            Ok(())
        }
        Ok(ipc::ResponseBody::Error { message }) => {
            bail!("instance {}: {message}", instance.info.pid)
        }
        Ok(_) => bail!(
            "instance {} returned an unexpected reply",
            instance.info.pid
        ),
        Err(error) => Err(anyhow::Error::new(error)
            .context(format!("cannot reach instance {}", instance.info.pid))),
    }
}

/// Choose which running instance receives an `--attach` open request.
///
/// A numeric `--target` is a PID and must match exactly. Otherwise the scope
/// is the requested path (or the target as a path): every instance whose
/// workspace root contains it is a candidate, and the deepest covering root
/// wins — the instance whose workspace most specifically covers the scope.
/// Ties keep discovery order, which is oldest-first, so the newest instance
/// at equal depth wins.
///
/// When no instance covers the scope and no target was given, a single
/// running instance is chosen automatically; with several instances the
/// result is [`Route::Ambiguous`] so the caller can offer a choice.
#[cfg(unix)]
fn select_instance<'a>(
    instances: &'a [ipc::Instance],
    target: Option<&str>,
    requested: &Path,
) -> Result<Route<'a>> {
    if let Some(target) = target
        && let Ok(pid) = target.parse::<u32>()
    {
        return instances
            .iter()
            .find(|instance| instance.info.pid == pid)
            .map(Route::Instance)
            .ok_or_else(|| anyhow!("no running instance has pid {pid}"));
    }
    let scope = match target {
        Some(target) => absolute_candidate(Path::new(target)),
        None => requested.to_path_buf(),
    };
    let mut candidates: Vec<&ipc::Instance> = instances
        .iter()
        .filter(|instance| scope.starts_with(Path::new(&instance.info.root)))
        .collect();
    candidates.sort_by_key(|instance| Path::new(&instance.info.root).components().count());
    if let Some(selected) = candidates.last().copied() {
        return Ok(Route::Instance(selected));
    }
    // Nothing covers the scope. With no explicit target and exactly one
    // running instance there is no ambiguity, so an out-of-workspace path
    // (e.g. `~/.bashrc`) routes straight to it instead of demanding a
    // redundant `--target <pid>`. The instance still rejects paths it
    // cannot preview (directories outside its workspace).
    if target.is_none() && instances.len() == 1 {
        return Ok(Route::Instance(&instances[0]));
    }
    // Multiple running instances and none covers the path: let the caller
    // offer an interactive choice (a TTY) or report the list (a script).
    if target.is_none() {
        return Ok(Route::Ambiguous(instances.iter().collect()));
    }
    let listed = format_instance_list(instances);
    Err(anyhow!(
        "no running instance matches {}\nrunning instances:\n{listed}",
        scope.display()
    ))
}

/// The outcome of routing an `--attach` request: either a single chosen
/// instance or a set of candidates the user must pick from interactively.
#[cfg(unix)]
#[derive(Debug)]
enum Route<'a> {
    Instance(&'a ipc::Instance),
    Ambiguous(Vec<&'a ipc::Instance>),
}

#[cfg(unix)]
fn format_instance_list<'a>(instances: impl IntoIterator<Item = &'a ipc::Instance>) -> String {
    instances
        .into_iter()
        .map(|instance| format!("  pid {}  {}", instance.info.pid, instance.info.root))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Prompt for a 1-based choice among running instances. Returns the chosen
/// instance, or an error when the input is not a TTY, is cancelled, or is
/// out of range.
#[cfg(unix)]
fn prompt_instance_choice<'a>(
    instances: &[&'a ipc::Instance],
    scope: &Path,
    stdin: &mut impl std::io::BufRead,
    stdout: &mut impl std::io::Write,
) -> Result<&'a ipc::Instance> {
    writeln!(
        stdout,
        "No running instance covers {}; choose one:",
        scope.display()
    )?;
    for (index, instance) in instances.iter().enumerate() {
        writeln!(
            stdout,
            "  [{}] pid {}  {}",
            index + 1,
            instance.info.pid,
            instance.info.root
        )?;
    }
    write!(
        stdout,
        "Enter a number 1-{} (q to cancel): ",
        instances.len()
    )?;
    stdout.flush()?;
    let mut line = String::new();
    stdin.read_line(&mut line)?;
    let answer = line.trim();
    if answer.eq_ignore_ascii_case("q") || answer.is_empty() {
        bail!("cancelled");
    }
    let index: usize = answer
        .parse()
        .map_err(|_| anyhow!("not a valid choice: {answer}"))?;
    instances
        .get(
            index
                .checked_sub(1)
                .ok_or_else(|| anyhow!("choice out of range"))?,
        )
        .copied()
        .ok_or_else(|| anyhow!("choice out of range: {index}"))
}
/// One row of `latte-lens ps --json`: the protocol's instance identity plus
/// the socket path a future `--attach` client would talk to.
#[cfg(unix)]
#[derive(serde::Serialize)]
struct PsEntry<'a> {
    #[serde(flatten)]
    info: &'a ipc::InstanceInfo,
    socket: &'a Path,
}

/// List live Latte Lens instances from the per-user runtime directory.
/// Discovery proves liveness by handshake, so the output never lists a
/// crashed instance's leftover socket.
#[cfg(unix)]
fn run_ps(json: bool) -> Result<()> {
    let dir = ipc::runtime_dir().context("instance discovery is unavailable")?;
    let instances = ipc::discover(&dir);
    if json {
        let entries: Vec<PsEntry<'_>> = instances
            .iter()
            .map(|instance| PsEntry {
                info: &instance.info,
                socket: instance.socket_path.as_path(),
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&entries)?);
        return Ok(());
    }
    if instances.is_empty() {
        println!("no running instances");
        return Ok(());
    }
    let now_ms = unix_millis_now();
    println!("{:<8} {:<9} {:<12} ROOT", "PID", "UPTIME", "VERSION");
    for instance in &instances {
        let uptime = format_uptime(Duration::from_millis(
            now_ms.saturating_sub(instance.info.started_at_ms),
        ));
        println!(
            "{:<8} {:<9} {:<12} {}",
            instance.info.pid, uptime, instance.info.version, instance.info.root
        );
    }
    Ok(())
}

#[cfg(unix)]
fn unix_millis_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// Render elapsed time compactly: "42s", "12m05s", "3h05m", "2d04h".
#[cfg(unix)]
fn format_uptime(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    match seconds {
        0..=59 => format!("{seconds}s"),
        60..=3599 => format!("{}m{:02}s", seconds / 60, seconds % 60),
        3600..=86_399 => format!("{}h{:02}m", seconds / 3600, (seconds % 3600) / 60),
        _ => format!("{}d{:02}h", seconds / 86_400, (seconds % 86_400) / 3600),
    }
}

// Instance discovery and `--attach` delivery use Unix-domain sockets. Stable
// Rust still only exposes Windows AF_UNIX on nightly, so on Windows the two
// commands fail with a clear message instead of silently doing nothing; the
// TUI itself is unaffected.
#[cfg(windows)]
fn run_ps(_json: bool) -> Result<()> {
    bail!("`latte-lens ps` is only available on Unix (Windows AF_UNIX is not yet stable)")
}

#[cfg(windows)]
fn run_attach(_path: PathBuf, _target: Option<&str>, _new_tab: bool) -> Result<()> {
    bail!("`--attach` is only available on Unix (Windows AF_UNIX is not yet stable)")
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

    // Serve the instance-discovery handshake (and forwarded open requests)
    // while the TUI runs. Best-effort: a broken runtime directory must not
    // block the viewer. The server must stay bound for the whole function:
    // dropping it stops the listener and removes the socket immediately.
    // Unix only: stable Windows does not expose AF_UNIX.
    #[cfg(unix)]
    let _instance_server: Option<ipc::IpcServer> = {
        let instance_inbox = ipc::new_request_inbox();
        let server = match ipc::IpcServer::start_serving(&workspace, &instance_inbox) {
            Ok(server) => Some(server),
            Err(error) => {
                eprintln!("latte-lens: instance discovery unavailable: {error}");
                None
            }
        };
        app.attach_instance_inbox(instance_inbox);
        server
    };

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
    #[cfg(unix)]
    use super::{Route, format_uptime, prompt_instance_choice, select_instance};
    use super::{bounded_edit_distance, similar_entry_names};
    #[cfg(unix)]
    use latte_lens::ipc::{self, Instance, InstanceInfo};
    #[cfg(unix)]
    use std::io::Cursor;
    #[cfg(unix)]
    use std::path::Path;
    #[cfg(unix)]
    use std::path::PathBuf;
    #[cfg(unix)]
    use std::time::Duration;

    #[cfg(unix)]
    fn instance(pid: u32, root: &str) -> Instance {
        Instance {
            socket_path: PathBuf::from(format!("/runtime/{pid}.sock")),
            info: InstanceInfo {
                proto: ipc::PROTOCOL_VERSION,
                pid,
                root: root.to_string(),
                version: "0.0.0".to_string(),
                started_at_ms: u64::from(pid),
            },
        }
    }

    #[cfg(unix)]
    fn selected_pid<'a>(route: Route<'a>) -> u32 {
        match route {
            Route::Instance(instance) => instance.info.pid,
            Route::Ambiguous(_) => panic!("expected a chosen instance, got ambiguity"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn routing_prefers_the_deepest_instance_root_covering_the_path() {
        let instances = vec![
            instance(10, "/home/me"),
            instance(11, "/home/me/projects/alpha"),
        ];
        let route = select_instance(
            &instances,
            None,
            Path::new("/home/me/projects/alpha/src/lib.rs"),
        )
        .expect("selected");
        assert_eq!(selected_pid(route), 11);

        let route =
            select_instance(&instances, None, Path::new("/home/me/notes.txt")).expect("selected");
        assert_eq!(selected_pid(route), 10);
    }

    #[cfg(unix)]
    #[test]
    fn routing_matches_roots_by_components_not_string_prefixes() {
        // `/home/me/project-x` shares a string prefix with root
        // `/home/me/project` but no path components, so it is not covered.
        // With another instance running it stays ambiguous rather than
        // silently attaching to the prefix-lookalike.
        let instances = vec![instance(20, "/home/me/project"), instance(21, "/elsewhere")];
        let route =
            select_instance(&instances, None, Path::new("/home/me/project-x/file.rs")).unwrap();
        let pids = match route {
            Route::Ambiguous(candidates) => candidates
                .iter()
                .map(|candidate| candidate.info.pid)
                .collect::<Vec<_>>(),
            Route::Instance(instance) => {
                panic!("expected ambiguity, chose pid {}", instance.info.pid)
            }
        };
        assert_eq!(pids, vec![20, 21]);
    }

    #[cfg(unix)]
    #[test]
    fn a_numeric_target_selects_by_exact_pid() {
        let instances = vec![instance(30, "/a"), instance(31, "/b")];
        let route =
            select_instance(&instances, Some("30"), Path::new("/b/file.rs")).expect("selected");
        assert_eq!(selected_pid(route), 30);
        assert!(select_instance(&instances, Some("99"), Path::new("/b/file.rs")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_path_target_reroutes_inside_the_target_subtree() {
        let instances = vec![instance(40, "/home/me"), instance(41, "/other")];
        let route =
            select_instance(&instances, Some("/home/me/projects"), Path::new("/other")).unwrap();
        assert_eq!(selected_pid(route), 40);
    }

    #[cfg(unix)]
    #[test]
    fn a_single_running_instance_receives_uncovered_paths_without_a_target() {
        let instances = vec![instance(42, "/data00/projects/latte-co")];
        let route = select_instance(&instances, None, Path::new("/data00/home/me/.bashrc"))
            .expect("selected");
        assert_eq!(selected_pid(route), 42);
    }

    #[cfg(unix)]
    #[test]
    fn several_instances_without_coverage_become_an_ambiguous_choice() {
        let instances = vec![instance(50, "/alpha"), instance(51, "/beta")];
        let route = select_instance(&instances, None, Path::new("/gamma/file.rs")).unwrap();
        let pids = match route {
            Route::Ambiguous(candidates) => candidates
                .iter()
                .map(|candidate| candidate.info.pid)
                .collect::<Vec<_>>(),
            Route::Instance(instance) => {
                panic!("expected ambiguity, chose pid {}", instance.info.pid)
            }
        };
        assert_eq!(pids, vec![50, 51]);
    }

    #[cfg(unix)]
    #[test]
    fn an_explicit_path_target_that_covers_nothing_reports_the_list() {
        let instances = vec![instance(60, "/alpha"), instance(61, "/beta")];
        let error = select_instance(&instances, Some("/gamma"), Path::new("/gamma/file.rs"))
            .expect_err("no match");
        let message = format!("{error:#}");
        assert!(message.contains("no running instance matches"));
        assert!(message.contains("pid 60"));
        assert!(message.contains("pid 61"));
    }

    #[cfg(unix)]
    #[test]
    fn the_instance_choice_prompt_accepts_a_number() {
        let instances = [instance(70, "/alpha"), instance(71, "/beta")];
        let mut input = Cursor::new(b"2\n".to_vec());
        let mut output = Vec::new();
        let chosen = prompt_instance_choice(
            &instances.iter().collect::<Vec<_>>(),
            Path::new("/x/f"),
            &mut input,
            &mut output,
        )
        .unwrap();
        assert_eq!(chosen.info.pid, 71);
        let rendered = String::from_utf8(output).unwrap();
        assert!(rendered.contains("pid 70"));
        assert!(rendered.contains("pid 71"));
    }

    #[cfg(unix)]
    #[test]
    fn the_instance_choice_prompt_rejects_bad_cancel_and_out_of_range_input() {
        let instances = [instance(80, "/alpha"), instance(81, "/beta")];
        let refs: Vec<&Instance> = instances.iter().collect();
        for answer in ["q\n", "\n", "9\n", "abc\n", "0\n"] {
            let mut input = Cursor::new(answer.as_bytes().to_vec());
            let mut output = Vec::new();
            assert!(
                prompt_instance_choice(&refs, Path::new("/x/f"), &mut input, &mut output).is_err(),
                "answer {answer:?} should not select an instance"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn uptime_renders_each_scale_compactly() {
        assert_eq!(format_uptime(Duration::from_secs(0)), "0s");
        assert_eq!(format_uptime(Duration::from_secs(42)), "42s");
        assert_eq!(format_uptime(Duration::from_secs(65)), "1m05s");
        assert_eq!(format_uptime(Duration::from_secs(11_100)), "3h05m");
        assert_eq!(format_uptime(Duration::from_secs(187_200)), "2d04h");
    }

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
