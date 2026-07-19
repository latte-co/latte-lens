use std::{
    env,
    ffi::{OsStr, OsString},
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Map, Value, json};

use super::{
    CLAUDE_HOOK_OBSERVER_ID, CODEX_HOOK_OBSERVER_ID, FileLock, MetadataError,
    OPENCODE_PLUGIN_OBSERVER_ID, TRAEX_HOOK_OBSERVER_ID, create_private_directories,
    replace_atomically, resolve_state_root_from_environment, set_no_follow,
    set_private_file_options, set_private_file_permissions, stable_hash,
};

const MAX_CONFIG_BYTES: usize = 1024 * 1024;
const MAX_BACKUPS: usize = 5;
const MANIFEST_VERSION: u64 = 1;
const SETUP_LOCK_TIMEOUT: Duration = Duration::from_secs(2);
const OPENCODE_ASSET: &str = include_str!("../../integrations/opencode/latte-lens.js");

const CODEX_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PermissionRequest",
    "PostToolUse",
    "SubagentStart",
    "SubagentStop",
    "Stop",
];
const CLAUDE_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PermissionRequest",
    "PermissionDenied",
    "PostToolUse",
    "PostToolUseFailure",
    "SubagentStart",
    "SubagentStop",
    "Stop",
    "StopFailure",
    "SessionEnd",
];
const TRAEX_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PermissionRequest",
    "PostToolUse",
    "PostToolUseFailure",
    "SubagentStart",
    "SubagentStop",
    "Stop",
    "SessionEnd",
];

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum HookSetupAgent {
    Codex,
    ClaudeCode,
    OpenCode,
    TraeX,
}

impl HookSetupAgent {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::ClaudeCode => "claude-code",
            Self::OpenCode => "opencode",
            Self::TraeX => "traex",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "codex" => Ok(Self::Codex),
            "claude-code" => Ok(Self::ClaudeCode),
            "opencode" => Ok(Self::OpenCode),
            "traex" => Ok(Self::TraeX),
            _ => bail!("unknown hook setup agent in backup manifest"),
        }
    }
}

impl std::fmt::Display for HookSetupAgent {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Debug)]
pub struct HookSetupOptions {
    pub binary: PathBuf,
    pub home: PathBuf,
    pub state_root: PathBuf,
    pub temporary_root: PathBuf,
    pub codex_dir: PathBuf,
    pub claude_dir: PathBuf,
    pub opencode_dir: PathBuf,
    pub traex_dir: PathBuf,
}

impl HookSetupOptions {
    pub fn from_environment(binary: PathBuf) -> Result<Self> {
        let home = home_directory()?;
        let codex_dir = env::var_os("CODEX_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".codex"));
        let claude_dir = env::var_os("CLAUDE_CONFIG_DIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".claude"));
        let opencode_dir = env::var_os("XDG_CONFIG_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"))
            .join("opencode");
        let traex_dir = home.join(".trae");
        Ok(Self {
            binary,
            home,
            state_root: resolve_state_root_from_environment().map_err(|error| anyhow!(error))?,
            temporary_root: env::temp_dir(),
            codex_dir,
            claude_dir,
            opencode_dir,
            traex_dir,
        })
    }
}

#[derive(Debug)]
pub struct HookSetupReport {
    pub transaction_id: Option<String>,
    pub backup_directory: Option<PathBuf>,
    pub configured: Vec<HookSetupAgent>,
    pub skipped: Vec<HookSetupAgent>,
}

#[derive(Debug)]
pub struct HookRestoreReport {
    pub transaction_id: String,
    pub restored: Vec<HookSetupAgent>,
}

#[derive(Clone, Copy, Debug)]
enum ConfigKind {
    Codex,
    Claude,
    OpenCode,
    TraeX,
}

#[derive(Debug)]
struct TargetSpec {
    agent: HookSetupAgent,
    root: PathBuf,
    path: PathBuf,
    kind: ConfigKind,
}

#[derive(Debug)]
struct OriginalFile {
    bytes: Vec<u8>,
    mode: Option<u32>,
    readonly: bool,
}

#[derive(Debug)]
struct PreparedFile {
    spec: TargetSpec,
    original: Option<OriginalFile>,
    updated: Vec<u8>,
}

#[derive(Clone, Debug)]
struct ManifestEntry {
    agent: HookSetupAgent,
    target: PathBuf,
    backup_name: String,
    existed: bool,
    original_mode: Option<u32>,
    original_readonly: bool,
    installed_digest: String,
    applied: bool,
}

#[derive(Debug)]
struct SetupManifest {
    transaction_id: String,
    status: String,
    entries: Vec<ManifestEntry>,
}

pub fn setup_user_hooks(options: HookSetupOptions) -> Result<HookSetupReport> {
    setup_user_hooks_inner(options, None)
}

fn setup_user_hooks_inner(
    options: HookSetupOptions,
    fail_before_write: Option<usize>,
) -> Result<HookSetupReport> {
    validate_setup_options(&options)?;
    create_private_directories(&options.state_root, &options.state_root)
        .map_err(metadata_error("create hook setup state directory"))?;
    let lock_path = options.state_root.join("hook-setup.lock");
    let Some(_lock) = FileLock::acquire(&lock_path, Instant::now() + SETUP_LOCK_TIMEOUT)
        .map_err(metadata_error("acquire hook setup lock"))?
    else {
        bail!("another Latte Lens hook setup is already running");
    };

    let (targets, skipped) = discover_targets(&options)?;
    let mut prepared = Vec::new();
    for target in targets {
        if let Some(file) = prepare_target(target, &options.binary)? {
            prepared.push(file);
        }
    }

    if prepared.is_empty() {
        return Ok(HookSetupReport {
            transaction_id: None,
            backup_directory: None,
            configured: Vec::new(),
            skipped,
        });
    }

    let transaction_id = generate_transaction_id();
    let temporary_directory = options
        .temporary_root
        .join(format!("latte-lens-hooks-{transaction_id}"));
    let backup_root = options.state_root.join("hook-backups");
    let backup_directory = backup_root.join(&transaction_id);
    create_private_directories(&options.temporary_root, &temporary_directory)
        .map_err(metadata_error("create temporary hook backup"))?;
    create_private_directories(&options.state_root, &backup_directory)
        .map_err(metadata_error("create durable hook backup"))?;

    let mut manifest = SetupManifest {
        transaction_id: transaction_id.clone(),
        status: "prepared".to_owned(),
        entries: Vec::with_capacity(prepared.len()),
    };
    for file in &prepared {
        let backup_name = format!("{}.original", file.spec.agent.as_str());
        if let Some(original) = &file.original {
            write_private_new(&temporary_directory.join(&backup_name), &original.bytes)?;
            write_private_new(&backup_directory.join(&backup_name), &original.bytes)?;
        }
        manifest.entries.push(ManifestEntry {
            agent: file.spec.agent,
            target: file.spec.path.clone(),
            backup_name,
            existed: file.original.is_some(),
            original_mode: file.original.as_ref().and_then(|file| file.mode),
            original_readonly: file.original.as_ref().is_some_and(|file| file.readonly),
            installed_digest: digest(&file.updated),
            applied: false,
        });
    }
    write_manifest(&backup_directory, &manifest)?;

    manifest.status = "applying".to_owned();
    write_manifest(&backup_directory, &manifest)?;
    let mut failure = None;
    for (index, file) in prepared.iter().enumerate() {
        if fail_before_write == Some(index) {
            failure = Some(anyhow!("injected hook setup write failure"));
            break;
        }
        if let Err(error) = write_prepared(file, &transaction_id) {
            failure = Some(error);
            break;
        }
        manifest.entries[index].applied = true;
        if let Err(error) = write_manifest(&backup_directory, &manifest) {
            failure = Some(error);
            break;
        }
    }

    if let Some(error) = failure {
        let rollback = rollback_applied(
            &mut manifest,
            &temporary_directory,
            &backup_directory,
            &transaction_id,
        );
        manifest.status = if rollback.is_ok() {
            "rolled-back".to_owned()
        } else {
            "rollback-incomplete".to_owned()
        };
        let _ = write_manifest(&backup_directory, &manifest);
        if rollback.is_ok() {
            let _ = fs::remove_dir_all(&temporary_directory);
        }
        rollback.with_context(|| {
            format!(
                "hook setup failed and rollback was incomplete; recovery backup: {}",
                backup_directory.display()
            )
        })?;
        return Err(error).with_context(|| {
            format!(
                "hook setup failed; original configuration was restored from {}",
                backup_directory.display()
            )
        });
    }

    manifest.status = "committed".to_owned();
    write_manifest(&backup_directory, &manifest)?;
    let _ = fs::remove_dir_all(&temporary_directory);
    prune_backups(&backup_root, MAX_BACKUPS, Some(&transaction_id));
    let mut configured = prepared
        .iter()
        .map(|file| file.spec.agent)
        .collect::<Vec<_>>();
    configured.sort_unstable();
    configured.dedup();
    Ok(HookSetupReport {
        transaction_id: Some(transaction_id),
        backup_directory: Some(backup_directory),
        configured,
        skipped,
    })
}

pub fn restore_user_hooks(
    options: HookSetupOptions,
    transaction_id: &str,
) -> Result<HookRestoreReport> {
    validate_setup_options(&options)?;
    validate_transaction_id(transaction_id)?;
    create_private_directories(&options.state_root, &options.state_root)
        .map_err(metadata_error("create hook setup state directory"))?;
    let lock_path = options.state_root.join("hook-setup.lock");
    let Some(_lock) = FileLock::acquire(&lock_path, Instant::now() + SETUP_LOCK_TIMEOUT)
        .map_err(metadata_error("acquire hook setup lock"))?
    else {
        bail!("another Latte Lens hook setup is already running");
    };

    let backup_directory = options.state_root.join("hook-backups").join(transaction_id);
    ensure_safe_directory(&backup_directory, "hook backup transaction")?;
    let mut manifest = read_manifest(&backup_directory)?;
    if manifest.transaction_id != transaction_id {
        bail!("hook backup transaction id does not match its directory");
    }

    preflight_restore(&manifest)?;
    let restore_id = generate_transaction_id();
    let restored = rollback_applied(
        &mut manifest,
        &backup_directory,
        &backup_directory,
        &restore_id,
    )?;
    manifest.status = "restored".to_owned();
    write_manifest(&backup_directory, &manifest)?;
    let mut restored = restored;
    restored.sort_unstable();
    restored.dedup();
    Ok(HookRestoreReport {
        transaction_id: transaction_id.to_owned(),
        restored,
    })
}

fn validate_setup_options(options: &HookSetupOptions) -> Result<()> {
    for (label, path) in [
        ("binary", options.binary.as_path()),
        ("home", options.home.as_path()),
        ("state root", options.state_root.as_path()),
        ("temporary root", options.temporary_root.as_path()),
        ("Codex directory", options.codex_dir.as_path()),
        ("Claude directory", options.claude_dir.as_path()),
        ("OpenCode directory", options.opencode_dir.as_path()),
        ("TraeX directory", options.traex_dir.as_path()),
    ] {
        if !path.is_absolute()
            || path
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir))
        {
            bail!("{label} must be an absolute normalized path");
        }
    }
    let binary = fs::symlink_metadata(&options.binary)
        .with_context(|| format!("inspect Latte Lens binary at {}", options.binary.display()))?;
    if !binary.file_type().is_file() || is_reparse_point(&binary) {
        bail!("Latte Lens binary must be a regular non-link file");
    }
    Ok(())
}

fn discover_targets(options: &HookSetupOptions) -> Result<(Vec<TargetSpec>, Vec<HookSetupAgent>)> {
    let candidates = [
        (
            HookSetupAgent::Codex,
            options.codex_dir.clone(),
            options.codex_dir.join("hooks.json"),
            ConfigKind::Codex,
        ),
        (
            HookSetupAgent::ClaudeCode,
            options.claude_dir.clone(),
            options.claude_dir.join("settings.json"),
            ConfigKind::Claude,
        ),
        (
            HookSetupAgent::OpenCode,
            options.opencode_dir.clone(),
            options.opencode_dir.join("plugins").join("latte-lens.js"),
            ConfigKind::OpenCode,
        ),
        (
            HookSetupAgent::TraeX,
            options.traex_dir.clone(),
            options.traex_dir.join("hooks.json"),
            ConfigKind::TraeX,
        ),
    ];
    let mut targets = Vec::new();
    let mut skipped = Vec::new();
    for (agent, root, path, kind) in candidates {
        match fs::symlink_metadata(&root) {
            Ok(metadata)
                if metadata.file_type().is_dir()
                    && !metadata.file_type().is_symlink()
                    && !is_reparse_point(&metadata) =>
            {
                targets.push(TargetSpec {
                    agent,
                    root,
                    path,
                    kind,
                });
            }
            Ok(_) => bail!("{} configuration directory is not a safe directory", agent),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => skipped.push(agent),
            Err(error) => return Err(error).context("inspect Code Agent configuration directory"),
        }
    }
    Ok((targets, skipped))
}

fn ensure_safe_directory(path: &Path, description: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect {description} at {}", path.display()))?;
    if !metadata.file_type().is_dir()
        || metadata.file_type().is_symlink()
        || is_reparse_point(&metadata)
    {
        bail!("{description} is not a safe directory");
    }
    Ok(())
}

fn prepare_target(target: TargetSpec, binary: &Path) -> Result<Option<PreparedFile>> {
    let original = read_optional_regular(&target.path)?;
    let original_bytes = original.as_ref().map(|file| file.bytes.as_slice());
    let updated = match target.kind {
        ConfigKind::Codex => prepare_json_config(
            original_bytes,
            target.agent,
            CODEX_HOOK_OBSERVER_ID,
            |root| install_codex_hooks(root, binary),
        )?,
        ConfigKind::Claude => prepare_json_config(
            original_bytes,
            target.agent,
            CLAUDE_HOOK_OBSERVER_ID,
            |root| install_claude_hooks(root, binary),
        )?,
        ConfigKind::TraeX => prepare_json_config(
            original_bytes,
            target.agent,
            TRAEX_HOOK_OBSERVER_ID,
            |root| install_traex_hooks(root, binary),
        )?,
        ConfigKind::OpenCode => prepare_opencode_plugin(original_bytes, binary)?,
    };
    if original_bytes == Some(updated.as_slice()) {
        return Ok(None);
    }
    Ok(Some(PreparedFile {
        spec: target,
        original,
        updated,
    }))
}

fn prepare_json_config<F>(
    original: Option<&[u8]>,
    agent: HookSetupAgent,
    _observer: &str,
    install: F,
) -> Result<Vec<u8>>
where
    F: FnOnce(&mut Value) -> Result<()>,
{
    let mut root = match original {
        Some(bytes) => serde_json::from_slice(bytes)
            .with_context(|| format!("parse existing {agent} hook configuration"))?,
        None => json!({}),
    };
    let before = root.clone();
    install(&mut root)?;
    if root == before
        && let Some(original) = original
    {
        return Ok(original.to_vec());
    }
    let mut bytes = serde_json::to_vec_pretty(&root)
        .with_context(|| format!("serialize {agent} hook configuration"))?;
    bytes.push(b'\n');
    if bytes.len() > MAX_CONFIG_BYTES {
        bail!("{agent} hook configuration exceeds the 1 MiB limit");
    }
    Ok(bytes)
}

fn install_codex_hooks(root: &mut Value, binary: &Path) -> Result<()> {
    let hooks = hooks_object(root, HookSetupAgent::Codex)?;
    for event in CODEX_EVENTS {
        let command = hook_command(
            binary,
            CODEX_HOOK_OBSERVER_ID,
            event,
            Some(("--workspace", ".")),
        );
        let mut group = Map::new();
        if *event == "SessionStart" {
            group.insert(
                "matcher".to_owned(),
                Value::String("startup|resume|clear|compact".to_owned()),
            );
        }
        group.insert(
            "hooks".to_owned(),
            json!([{ "type": "command", "command": command, "timeout": 1 }]),
        );
        install_nested_hook(hooks, event, CODEX_HOOK_OBSERVER_ID, Value::Object(group))?;
    }
    Ok(())
}

fn install_claude_hooks(root: &mut Value, binary: &Path) -> Result<()> {
    let hooks = hooks_object(root, HookSetupAgent::ClaudeCode)?;
    let binary = binary
        .to_str()
        .ok_or_else(|| anyhow!("Latte Lens binary path must be Unicode for Claude Code hooks"))?;
    for event in CLAUDE_EVENTS {
        let mut group = Map::new();
        if *event == "SessionStart" {
            group.insert(
                "matcher".to_owned(),
                Value::String("startup|resume|clear|compact".to_owned()),
            );
        }
        group.insert(
            "hooks".to_owned(),
            json!([{
                "type": "command",
                "command": binary,
                "args": [
                    "hook", "--observer", CLAUDE_HOOK_OBSERVER_ID,
                    "--event", event, "--workspace", "${CLAUDE_PROJECT_DIR}"
                ],
                "timeout": 1
            }]),
        );
        install_nested_hook(hooks, event, CLAUDE_HOOK_OBSERVER_ID, Value::Object(group))?;
    }
    Ok(())
}

fn install_traex_hooks(root: &mut Value, binary: &Path) -> Result<()> {
    let hooks = hooks_object(root, HookSetupAgent::TraeX)?;
    for event in TRAEX_EVENTS {
        let command = hook_command(
            binary,
            TRAEX_HOOK_OBSERVER_ID,
            event,
            Some(("--workspace", ".")),
        );
        let group = json!({
            "hooks": [{ "type": "command", "command": command, "timeout": "1s" }]
        });
        install_nested_hook(hooks, event, TRAEX_HOOK_OBSERVER_ID, group)?;
    }
    Ok(())
}

fn hooks_object(root: &mut Value, agent: HookSetupAgent) -> Result<&mut Map<String, Value>> {
    let object = root
        .as_object_mut()
        .ok_or_else(|| anyhow!("{agent} configuration root must be a JSON object"))?;
    let hooks = object.entry("hooks").or_insert_with(|| json!({}));
    hooks
        .as_object_mut()
        .ok_or_else(|| anyhow!("{agent} hooks value must be a JSON object"))
}

fn install_nested_hook(
    hooks: &mut Map<String, Value>,
    event: &str,
    observer: &str,
    desired: Value,
) -> Result<()> {
    let entries = hooks.entry(event).or_insert_with(|| json!([]));
    let entries = entries
        .as_array_mut()
        .ok_or_else(|| anyhow!("hook entries for {event} must be a JSON array"))?;
    let owned_count = entries
        .iter()
        .map(|entry| count_owned_hooks(entry, observer))
        .sum::<usize>();
    if owned_count == 1 && entries.iter().any(|entry| entry == &desired) {
        return Ok(());
    }
    entries.retain_mut(|entry| remove_owned_hooks(entry, observer));
    entries.push(desired);
    Ok(())
}

fn count_owned_hooks(group: &Value, observer: &str) -> usize {
    group
        .get("hooks")
        .and_then(Value::as_array)
        .map(|hooks| {
            hooks
                .iter()
                .filter(|hook| is_owned_hook(hook, observer))
                .count()
        })
        .unwrap_or(0)
}

fn remove_owned_hooks(group: &mut Value, observer: &str) -> bool {
    let Some(hooks) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
        return true;
    };
    hooks.retain(|hook| !is_owned_hook(hook, observer));
    !hooks.is_empty()
}

fn is_owned_hook(hook: &Value, observer: &str) -> bool {
    if hook.get("type").and_then(Value::as_str) != Some("command") {
        return false;
    }
    if hook
        .get("args")
        .and_then(Value::as_array)
        .is_some_and(|args| {
            args.windows(2).any(|pair| {
                pair[0].as_str() == Some("--observer") && pair[1].as_str() == Some(observer)
            })
        })
    {
        return true;
    }
    hook.get("command")
        .and_then(Value::as_str)
        .is_some_and(|command| {
            command
                .split_ascii_whitespace()
                .collect::<Vec<_>>()
                .windows(2)
                .any(|pair| pair == ["--observer", observer])
        })
}

fn prepare_opencode_plugin(original: Option<&[u8]>, binary: &Path) -> Result<Vec<u8>> {
    if let Some(bytes) = original {
        let text = std::str::from_utf8(bytes).context("existing OpenCode plugin is not UTF-8")?;
        if !text.contains("export const LatteLensPlugin")
            || !text.contains(OPENCODE_PLUGIN_OBSERVER_ID)
        {
            bail!(
                "existing OpenCode plugins/latte-lens.js is not managed by Latte Lens; move or rename it before setup"
            );
        }
    }
    let binary = binary
        .to_str()
        .ok_or_else(|| anyhow!("Latte Lens binary path must be Unicode for OpenCode"))?;
    let fallback = serde_json::to_string(binary).context("encode OpenCode binary path")?;
    let needle = "const binary = process.env.LATTE_LENS_BIN || \"latte-lens\"";
    let replacement = format!("const binary = process.env.LATTE_LENS_BIN || {fallback}");
    let updated = OPENCODE_ASSET.replacen(needle, &replacement, 1);
    if updated == OPENCODE_ASSET {
        bail!("embedded OpenCode plugin is missing its binary placeholder");
    }
    Ok(updated.into_bytes())
}

fn hook_command(binary: &Path, observer: &str, event: &str, extra: Option<(&str, &str)>) -> String {
    let mut command = format!(
        "{} hook --observer {observer} --event {event}",
        quote_command_path(binary)
    );
    if let Some((name, value)) = extra {
        command.push(' ');
        command.push_str(name);
        command.push(' ');
        command.push_str(value);
    }
    command
}

#[cfg(unix)]
fn quote_command_path(path: &Path) -> String {
    let value = path.to_string_lossy();
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(windows)]
fn quote_command_path(path: &Path) -> String {
    format!("\"{}\"", path.to_string_lossy().replace('"', "\\\""))
}

#[cfg(not(any(unix, windows)))]
fn quote_command_path(path: &Path) -> String {
    format!("\"{}\"", path.to_string_lossy().replace('"', "\\\""))
}

fn write_prepared(file: &PreparedFile, transaction_id: &str) -> Result<()> {
    let parent = file
        .spec
        .path
        .parent()
        .ok_or_else(|| anyhow!("hook configuration has no parent directory"))?;
    create_private_directories(&file.spec.root, parent)
        .map_err(metadata_error("create Code Agent hook directory"))?;
    atomic_write(
        &file.spec.path,
        &file.updated,
        file.original.as_ref().and_then(|file| file.mode),
        file.original.as_ref().is_some_and(|file| file.readonly),
        transaction_id,
    )
}

fn rollback_applied(
    manifest: &mut SetupManifest,
    backup_source: &Path,
    journal_directory: &Path,
    transaction_id: &str,
) -> Result<Vec<HookSetupAgent>> {
    let mut restored = Vec::new();
    for index in (0..manifest.entries.len()).rev() {
        if !manifest.entries[index].applied {
            continue;
        }
        let entry = &manifest.entries[index];
        let current = read_optional_regular(&entry.target)?;
        if current.as_ref().map(|file| digest(&file.bytes)).as_deref()
            != Some(entry.installed_digest.as_str())
        {
            bail!(
                "{} configuration changed after setup; refusing to overwrite it during rollback",
                entry.agent
            );
        }
        if entry.existed {
            let backup =
                read_bounded_regular(&backup_source.join(&entry.backup_name), MAX_CONFIG_BYTES)?;
            atomic_write(
                &entry.target,
                &backup,
                entry.original_mode,
                entry.original_readonly,
                transaction_id,
            )?;
        } else {
            fs::remove_file(&entry.target).with_context(|| {
                format!("remove newly-created {} hook configuration", entry.agent)
            })?;
        }
        restored.push(entry.agent);
        manifest.entries[index].applied = false;
        write_manifest(journal_directory, manifest)?;
    }
    Ok(restored)
}

fn preflight_restore(manifest: &SetupManifest) -> Result<()> {
    for entry in manifest.entries.iter().filter(|entry| entry.applied) {
        let current = read_optional_regular(&entry.target)?;
        if current.as_ref().map(|file| digest(&file.bytes)).as_deref()
            != Some(entry.installed_digest.as_str())
        {
            bail!(
                "{} configuration changed after setup; refusing to overwrite it during restore",
                entry.agent
            );
        }
    }
    Ok(())
}

fn read_optional_regular(path: &Path) -> Result<Option<OriginalFile>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("inspect Code Agent hook configuration"),
    };
    if !metadata.file_type().is_file()
        || metadata.file_type().is_symlink()
        || is_reparse_point(&metadata)
        || metadata.len() > MAX_CONFIG_BYTES as u64
    {
        bail!("Code Agent hook configuration must be a bounded regular non-link file");
    }
    let bytes = read_bounded_regular(path, MAX_CONFIG_BYTES)?;
    Ok(Some(OriginalFile {
        bytes,
        mode: permission_mode(&metadata),
        readonly: metadata.permissions().readonly(),
    }))
}

fn read_bounded_regular(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let mut options = OpenOptions::new();
    options.read(true);
    set_no_follow(&mut options);
    let mut file = options
        .open(path)
        .with_context(|| format!("open bounded hook configuration at {}", path.display()))?;
    let metadata = file
        .metadata()
        .context("inspect opened hook configuration")?;
    if !metadata.file_type().is_file() || metadata.len() > limit as u64 {
        bail!("hook configuration exceeds its size or file-type boundary");
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    Read::by_ref(&mut file)
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .context("read bounded hook configuration")?;
    if bytes.len() > limit {
        bail!("hook configuration exceeds the 1 MiB limit");
    }
    Ok(bytes)
}

fn atomic_write(
    path: &Path,
    bytes: &[u8],
    mode: Option<u32>,
    readonly: bool,
    transaction_id: &str,
) -> Result<()> {
    if bytes.len() > MAX_CONFIG_BYTES {
        bail!("hook configuration exceeds the 1 MiB limit");
    }
    if let Ok(metadata) = fs::symlink_metadata(path)
        && (!metadata.file_type().is_file()
            || metadata.file_type().is_symlink()
            || is_reparse_point(&metadata))
    {
        bail!("refusing to replace a non-regular hook configuration");
    }
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("hook configuration has no parent directory"))?;
    let parent_metadata = fs::symlink_metadata(parent).context("inspect hook directory")?;
    if !parent_metadata.file_type().is_dir()
        || parent_metadata.file_type().is_symlink()
        || is_reparse_point(&parent_metadata)
    {
        bail!("hook configuration parent is not a safe directory");
    }
    let name = path
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| anyhow!("hook configuration filename must be Unicode"))?;
    let temporary = parent.join(format!(".{name}.latte-lens-{transaction_id}.tmp"));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    set_private_file_options(&mut options);
    let mut file = options
        .open(&temporary)
        .context("create atomic hook configuration temporary file")?;
    set_private_file_permissions(&temporary)
        .map_err(metadata_error("secure hook configuration temporary file"))?;
    let result = (|| {
        file.write_all(bytes)
            .context("write hook configuration temporary file")?;
        file.sync_all()
            .context("sync hook configuration temporary file")?;
        drop(file);
        apply_permissions(&temporary, mode, readonly)?;
        replace_atomically(&temporary, path)
            .map_err(metadata_error("replace hook configuration atomically"))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn write_private_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    set_private_file_options(&mut options);
    let mut file = options.open(path).context("create private hook backup")?;
    set_private_file_permissions(path).map_err(metadata_error("secure private hook backup"))?;
    file.write_all(bytes).context("write private hook backup")?;
    file.sync_all().context("sync private hook backup")
}

fn write_manifest(directory: &Path, manifest: &SetupManifest) -> Result<()> {
    let path = directory.join("manifest.json");
    let entries = manifest
        .entries
        .iter()
        .map(|entry| {
            json!({
                "agent": entry.agent.as_str(),
                "target": encode_path(&entry.target),
                "backup": entry.backup_name,
                "existed": entry.existed,
                "original_mode": entry.original_mode,
                "original_readonly": entry.original_readonly,
                "installed_digest": entry.installed_digest,
                "applied": entry.applied,
            })
        })
        .collect::<Vec<_>>();
    let value = json!({
        "version": MANIFEST_VERSION,
        "transaction_id": manifest.transaction_id,
        "status": manifest.status,
        "entries": entries,
    });
    let mut bytes = serde_json::to_vec_pretty(&value).context("serialize hook backup manifest")?;
    bytes.push(b'\n');
    if path.exists() {
        atomic_write(&path, &bytes, Some(0o600), false, &manifest.transaction_id)
    } else {
        write_private_new(&path, &bytes)
    }
}

fn read_manifest(directory: &Path) -> Result<SetupManifest> {
    let bytes = read_bounded_regular(&directory.join("manifest.json"), MAX_CONFIG_BYTES)?;
    let value: Value = serde_json::from_slice(&bytes).context("parse hook backup manifest")?;
    let object = value
        .as_object()
        .ok_or_else(|| anyhow!("hook backup manifest root must be an object"))?;
    if object.get("version").and_then(Value::as_u64) != Some(MANIFEST_VERSION) {
        bail!("unsupported hook backup manifest version");
    }
    let transaction_id = required_string(object, "transaction_id")?.to_owned();
    validate_transaction_id(&transaction_id)?;
    let status = required_string(object, "status")?.to_owned();
    let entries = object
        .get("entries")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("hook backup manifest entries must be an array"))?
        .iter()
        .map(parse_manifest_entry)
        .collect::<Result<Vec<_>>>()?;
    Ok(SetupManifest {
        transaction_id,
        status,
        entries,
    })
}

fn parse_manifest_entry(value: &Value) -> Result<ManifestEntry> {
    let object = value
        .as_object()
        .ok_or_else(|| anyhow!("hook backup manifest entry must be an object"))?;
    let original_mode = match object.get("original_mode") {
        Some(Value::Null) | None => None,
        Some(value) => Some(
            u32::try_from(
                value
                    .as_u64()
                    .ok_or_else(|| anyhow!("invalid original_mode in hook backup"))?,
            )
            .context("original_mode exceeds u32")?,
        ),
    };
    Ok(ManifestEntry {
        agent: HookSetupAgent::parse(required_string(object, "agent")?)?,
        target: decode_path(required_string(object, "target")?)?,
        backup_name: safe_backup_name(required_string(object, "backup")?)?.to_owned(),
        existed: required_bool(object, "existed")?,
        original_mode,
        original_readonly: required_bool(object, "original_readonly")?,
        installed_digest: validate_digest(required_string(object, "installed_digest")?)?.to_owned(),
        applied: required_bool(object, "applied")?,
    })
}

fn required_string<'a>(object: &'a Map<String, Value>, key: &str) -> Result<&'a str> {
    object
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("hook backup manifest field {key} must be a string"))
}

fn required_bool(object: &Map<String, Value>, key: &str) -> Result<bool> {
    object
        .get(key)
        .and_then(Value::as_bool)
        .ok_or_else(|| anyhow!("hook backup manifest field {key} must be a boolean"))
}

fn safe_backup_name(value: &str) -> Result<&str> {
    if value.is_empty()
        || value.len() > 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
    {
        bail!("unsafe hook backup filename");
    }
    Ok(value)
}

fn validate_digest(value: &str) -> Result<&str> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("invalid digest in hook backup manifest");
    }
    Ok(value)
}

fn validate_transaction_id(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 80
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        bail!("invalid hook backup transaction id");
    }
    Ok(())
}

fn digest(bytes: &[u8]) -> String {
    stable_hash(b"hook-setup-file", &[bytes]).to_hex()
}

fn generate_transaction_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{nanos}-{}", std::process::id())
}

fn prune_backups(root: &Path, keep: usize, preserve: Option<&str>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    let mut directories = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_str()?;
            if preserve == Some(name) {
                return None;
            }
            let metadata = entry.metadata().ok()?;
            metadata
                .file_type()
                .is_dir()
                .then_some((name.to_owned(), entry.path()))
        })
        .collect::<Vec<_>>();
    directories.sort_by(|left, right| right.0.cmp(&left.0));
    for (_, path) in directories.into_iter().skip(keep.saturating_sub(1)) {
        let _ = fs::remove_dir_all(path);
    }
}

fn home_directory() -> Result<PathBuf> {
    env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .or_else(|| env::var_os("USERPROFILE").filter(|value| !value.is_empty()))
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("HOME or USERPROFILE must be set for hook setup"))
}

fn metadata_error(operation: &'static str) -> impl FnOnce(MetadataError) -> anyhow::Error {
    move |error| anyhow!("{operation}: {error}")
}

#[cfg(unix)]
fn permission_mode(metadata: &fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    Some(metadata.permissions().mode())
}

#[cfg(not(unix))]
fn permission_mode(_metadata: &fs::Metadata) -> Option<u32> {
    None
}

#[cfg(unix)]
fn apply_permissions(path: &Path, mode: Option<u32>, _readonly: bool) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if let Some(mode) = mode {
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
            .context("preserve hook configuration permissions")?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn apply_permissions(path: &Path, _mode: Option<u32>, readonly: bool) -> Result<()> {
    let mut permissions = fs::metadata(path)
        .context("inspect hook configuration permissions")?
        .permissions();
    permissions.set_readonly(readonly);
    fs::set_permissions(path, permissions).context("preserve hook configuration permissions")
}

#[cfg(windows)]
fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
const fn is_reparse_point(_metadata: &fs::Metadata) -> bool {
    false
}

#[cfg(unix)]
fn encode_path(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    URL_SAFE_NO_PAD.encode(path.as_os_str().as_bytes())
}

#[cfg(unix)]
fn decode_path(value: &str) -> Result<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let bytes = URL_SAFE_NO_PAD
        .decode(value)
        .context("decode hook backup path")?;
    Ok(PathBuf::from(OsString::from_vec(bytes)))
}

#[cfg(windows)]
fn encode_path(path: &Path) -> String {
    use std::os::windows::ffi::OsStrExt;
    let bytes = path
        .as_os_str()
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    URL_SAFE_NO_PAD.encode(bytes)
}

#[cfg(windows)]
fn decode_path(value: &str) -> Result<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    let bytes = URL_SAFE_NO_PAD
        .decode(value)
        .context("decode hook backup path")?;
    if bytes.len() % 2 != 0 {
        bail!("encoded Windows hook backup path has an odd byte length");
    }
    let wide = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect::<Vec<_>>();
    Ok(PathBuf::from(OsString::from_wide(&wide)))
}

#[cfg(not(any(unix, windows)))]
fn encode_path(path: &Path) -> String {
    URL_SAFE_NO_PAD.encode(path.to_string_lossy().as_bytes())
}

#[cfg(not(any(unix, windows)))]
fn decode_path(value: &str) -> Result<PathBuf> {
    let bytes = URL_SAFE_NO_PAD
        .decode(value)
        .context("decode hook backup path")?;
    Ok(PathBuf::from(String::from_utf8(bytes)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(root: &Path) -> HookSetupOptions {
        let binary = root.join("bin/latte-lens");
        fs::create_dir_all(binary.parent().expect("binary parent")).expect("binary parent");
        fs::write(&binary, b"binary").expect("binary");
        let home = root.join("home");
        let codex_dir = home.join(".codex");
        let claude_dir = home.join(".claude");
        let opencode_dir = home.join(".config/opencode");
        let traex_dir = home.join(".trae");
        for directory in [&codex_dir, &claude_dir, &opencode_dir, &traex_dir] {
            fs::create_dir_all(directory).expect("agent directory");
        }
        HookSetupOptions {
            binary,
            home,
            state_root: root.join("state"),
            temporary_root: root.join("tmp"),
            codex_dir,
            claude_dir,
            opencode_dir,
            traex_dir,
        }
    }

    #[test]
    fn setup_merges_all_user_configs_and_is_idempotent() {
        let sandbox = tempfile::tempdir().expect("sandbox");
        let options = options(sandbox.path());
        fs::create_dir_all(&options.temporary_root).expect("tmp");
        fs::write(
            options.codex_dir.join("hooks.json"),
            br#"{"other":true,"hooks":{"Stop":[{"hooks":[{"type":"command","command":"other"}]}]}}"#,
        )
        .expect("codex config");
        fs::write(
            options.claude_dir.join("settings.json"),
            br#"{"theme":"dark"}"#,
        )
        .expect("claude config");
        fs::write(
            options.traex_dir.join("hooks.json"),
            br#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"other"}]}]}}"#,
        )
        .expect("traex config");

        let first = setup_user_hooks(options.clone()).expect("first setup");
        assert_eq!(first.configured.len(), 4);
        assert!(first.backup_directory.expect("backup").is_dir());
        let codex = fs::read_to_string(options.codex_dir.join("hooks.json")).expect("codex");
        assert!(codex.contains("other"));
        assert!(codex.contains(CODEX_HOOK_OBSERVER_ID));
        assert_eq!(
            codex.matches("--workspace .").count(),
            CODEX_EVENTS.len(),
            "every Codex hook must explicitly bind the session working directory"
        );
        let claude = fs::read_to_string(options.claude_dir.join("settings.json")).expect("claude");
        assert!(claude.contains("\"theme\": \"dark\""));
        assert!(claude.contains(CLAUDE_HOOK_OBSERVER_ID));
        let traex = fs::read_to_string(options.traex_dir.join("hooks.json")).expect("traex");
        assert!(traex.contains("other"));
        assert!(traex.contains(TRAEX_HOOK_OBSERVER_ID));
        let opencode = fs::read_to_string(options.opencode_dir.join("plugins/latte-lens.js"))
            .expect("opencode");
        let encoded_binary =
            serde_json::to_string(options.binary.to_str().expect("binary path")).expect("encode");
        assert!(opencode.contains(&encoded_binary));

        let snapshots = [
            fs::read(options.codex_dir.join("hooks.json")).expect("codex"),
            fs::read(options.claude_dir.join("settings.json")).expect("claude"),
            fs::read(options.opencode_dir.join("plugins/latte-lens.js")).expect("opencode"),
            fs::read(options.traex_dir.join("hooks.json")).expect("traex"),
        ];
        let second = setup_user_hooks(options.clone()).expect("second setup");
        assert!(second.transaction_id.is_none());
        assert_eq!(
            snapshots,
            [
                fs::read(options.codex_dir.join("hooks.json")).expect("codex"),
                fs::read(options.claude_dir.join("settings.json")).expect("claude"),
                fs::read(options.opencode_dir.join("plugins/latte-lens.js")).expect("opencode"),
                fs::read(options.traex_dir.join("hooks.json")).expect("traex"),
            ]
        );
    }

    #[test]
    fn malformed_config_aborts_before_any_file_is_changed() {
        let sandbox = tempfile::tempdir().expect("sandbox");
        let options = options(sandbox.path());
        fs::create_dir_all(&options.temporary_root).expect("tmp");
        let codex_path = options.codex_dir.join("hooks.json");
        let claude_path = options.claude_dir.join("settings.json");
        fs::write(&codex_path, b"{}").expect("codex");
        fs::write(&claude_path, b"not-json").expect("claude");

        let error = setup_user_hooks(options).expect_err("malformed setup");

        assert!(error.to_string().contains("parse existing claude-code"));
        assert_eq!(fs::read(codex_path).expect("codex"), b"{}");
    }

    #[test]
    fn write_failure_rolls_back_files_already_committed() {
        let sandbox = tempfile::tempdir().expect("sandbox");
        let options = options(sandbox.path());
        fs::create_dir_all(&options.temporary_root).expect("tmp");
        let codex_path = options.codex_dir.join("hooks.json");
        fs::write(&codex_path, b"{\"original\":true}\n").expect("codex");

        let error = setup_user_hooks_inner(options.clone(), Some(1)).expect_err("failure");

        assert!(
            error
                .to_string()
                .contains("original configuration was restored")
        );
        assert_eq!(
            fs::read(codex_path).expect("codex"),
            b"{\"original\":true}\n"
        );
        assert!(!options.claude_dir.join("settings.json").exists());
    }

    #[test]
    fn durable_backup_restores_only_when_installed_bytes_are_unchanged() {
        let sandbox = tempfile::tempdir().expect("sandbox");
        let options = options(sandbox.path());
        fs::create_dir_all(&options.temporary_root).expect("tmp");
        let codex_path = options.codex_dir.join("hooks.json");
        fs::write(&codex_path, b"{\"original\":true}\n").expect("codex");
        let setup = setup_user_hooks(options.clone()).expect("setup");
        let transaction = setup.transaction_id.expect("transaction");
        let installed = fs::read(&codex_path).expect("installed codex config");

        fs::write(&codex_path, b"{\"user_changed\":true}\n").expect("user change");
        let error = restore_user_hooks(options.clone(), &transaction).expect_err("conflict");
        assert!(error.to_string().contains("changed after setup"));
        assert_eq!(
            fs::read(&codex_path).expect("codex"),
            b"{\"user_changed\":true}\n"
        );

        fs::write(&codex_path, installed).expect("restore installed bytes");
        restore_user_hooks(options, &transaction).expect("restore");
        assert_eq!(
            fs::read(codex_path).expect("codex"),
            b"{\"original\":true}\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn setup_rejects_symlinked_agent_config() {
        use std::os::unix::fs::symlink;

        let sandbox = tempfile::tempdir().expect("sandbox");
        let options = options(sandbox.path());
        fs::create_dir_all(&options.temporary_root).expect("tmp");
        let outside = sandbox.path().join("outside.json");
        fs::write(&outside, b"{}").expect("outside");
        symlink(&outside, options.codex_dir.join("hooks.json")).expect("symlink");

        let error = setup_user_hooks(options).expect_err("unsafe config");
        assert!(error.to_string().contains("regular non-link"));
        assert_eq!(fs::read(outside).expect("outside"), b"{}");
    }
}
