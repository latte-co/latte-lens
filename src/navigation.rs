//! Bounded, read-only code-navigation domain and configuration policy.
//!
//! Semantic navigation uses built-in language-server defaults that user-level
//! product configuration can disable or override. Structural data in
//! [`crate::folding`] is deliberately not a semantic definition/reference
//! fallback.
#![allow(dead_code)]

use std::{
    collections::BTreeMap,
    env,
    fs::Metadata,
    io::Read,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;

use crate::{
    content_safety::{
        ContentPathKind, OpenRegular, inspect_content_path, open_regular,
        path_exists_without_following,
    },
    folding::StructureSnapshot,
    lsp::{PayloadBudget, PayloadPermit},
    runtime::ContentIdentity,
};

pub(crate) const MAX_NAVIGATION_TEXT_BYTES: usize = 512 * 1024;
pub(crate) const MAX_NAVIGATION_LINES: usize = 2_000;
const MAX_CONFIG_BYTES: u64 = 64 * 1024;
const MAX_PROGRAM_BYTES: usize = 4_096;
const MAX_ARGS: usize = 16;
const MAX_ARG_BYTES: usize = 4_096;
const MAX_ARGS_BYTES: usize = 16 * 1024;

#[cfg(test)]
static NAVIGATION_ENVIRONMENT_LOCK: std::sync::OnceLock<std::sync::Mutex<()>> =
    std::sync::OnceLock::new();

#[cfg(test)]
pub(crate) fn lock_navigation_environment() -> std::sync::MutexGuard<'static, ()> {
    NAVIGATION_ENVIRONMENT_LOCK
        .get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct SourcePosition {
    pub line: usize,
    pub byte: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SourceRange {
    pub start: SourcePosition,
    pub end: SourcePosition,
}

impl SourceRange {
    pub(crate) fn is_empty(self) -> bool {
        self.start >= self.end
    }

    pub(crate) fn contains(self, point: SourcePosition) -> bool {
        self.start <= point && point < self.end
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum LanguageFamily {
    Rust,
    TypeScript,
    Python,
    Go,
}

impl LanguageFamily {
    pub const fn config_key(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::TypeScript => "typescript",
            Self::Python => "python",
            Self::Go => "go",
        }
    }

    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Rust => "Rust",
            Self::TypeScript => "TypeScript/JavaScript",
            Self::Python => "Python",
            Self::Go => "Go",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct LanguageDescriptor {
    pub family: Option<LanguageFamily>,
    pub language_id: &'static str,
    pub local_structure: bool,
}

pub(crate) fn language_for_path(path: &Path) -> Option<LanguageDescriptor> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match extension.as_str() {
        "rs" => LanguageDescriptor {
            family: Some(LanguageFamily::Rust),
            language_id: "rust",
            local_structure: true,
        },
        "ts" | "mts" | "cts" => LanguageDescriptor {
            family: Some(LanguageFamily::TypeScript),
            language_id: "typescript",
            local_structure: true,
        },
        "tsx" => LanguageDescriptor {
            family: Some(LanguageFamily::TypeScript),
            language_id: "typescriptreact",
            local_structure: true,
        },
        "js" | "mjs" | "cjs" => LanguageDescriptor {
            family: Some(LanguageFamily::TypeScript),
            language_id: "javascript",
            local_structure: true,
        },
        "jsx" => LanguageDescriptor {
            family: Some(LanguageFamily::TypeScript),
            language_id: "javascriptreact",
            local_structure: true,
        },
        "py" | "pyi" => LanguageDescriptor {
            family: Some(LanguageFamily::Python),
            language_id: "python",
            local_structure: true,
        },
        "go" => LanguageDescriptor {
            family: Some(LanguageFamily::Go),
            language_id: "go",
            local_structure: true,
        },
        "md" | "markdown" => LanguageDescriptor {
            family: None,
            language_id: "markdown",
            local_structure: true,
        },
        _ => return None,
    })
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct DocumentVersion(pub u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NavigationOperation {
    Definition,
    References,
    Implementations,
    DocumentSymbols,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum NavigationTargetRange {
    Source(SourceRange),
    Utf16(lsp_types::Range),
}

/// A package boundary discovered from a language-server location outside the
/// opened workspace. Its root and every component beneath it have passed the
/// shared no-follow content inspection before it reaches the preview runtime.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DependencyTarget {
    pub root: PathBuf,
    pub relative: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum NavigationFileTarget {
    Workspace(PathBuf),
    Dependency(DependencyTarget),
}

/// Complete, immutable, budget-owned source snapshot used by a session.
pub(crate) struct NavigationDocument {
    pub identity: ContentIdentity,
    pub absolute_path: PathBuf,
    /// No-link root used to revalidate the current document from disk.
    pub content_root: PathBuf,
    pub disk_raw_len: u64,
    pub server_root: PathBuf,
    pub language: LanguageDescriptor,
    pub version: DocumentVersion,
    pub text: Arc<str>,
    pub line_index: Arc<LineIndex>,
    pub structure: Arc<StructureSnapshot>,
    _payload_permit: PayloadPermit,
}

#[derive(Clone, Debug)]
pub(crate) struct NavigationSource {
    pub identity: ContentIdentity,
    pub absolute_path: PathBuf,
    pub content_root: PathBuf,
    pub disk_raw_len: u64,
    pub server_root: PathBuf,
    pub language: LanguageDescriptor,
    pub text: Arc<str>,
    pub line_index: Arc<LineIndex>,
    pub structure: Arc<StructureSnapshot>,
}

impl NavigationDocument {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        identity: ContentIdentity,
        absolute_path: PathBuf,
        content_root: PathBuf,
        disk_raw_len: u64,
        server_root: PathBuf,
        language: LanguageDescriptor,
        version: DocumentVersion,
        text: Arc<str>,
        structure: Arc<StructureSnapshot>,
        budget: &PayloadBudget,
    ) -> Result<Self> {
        let line_index = Arc::new(LineIndex::new(Arc::clone(&text))?);
        let offsets = line_index
            .line_count()
            .checked_mul(std::mem::size_of::<usize>())
            .ok_or_else(|| anyhow!("navigation line index charge overflow"))?;
        let tokens = structure
            .recognizable_tokens
            .ranges
            .len()
            .checked_mul(std::mem::size_of::<SourceRange>())
            .ok_or_else(|| anyhow!("navigation token index charge overflow"))?;
        let charge = text
            .len()
            .checked_add(offsets)
            .and_then(|value| value.checked_add(tokens))
            .ok_or_else(|| anyhow!("navigation document charge overflow"))?;
        if charge > 3 * 1024 * 1024 {
            bail!("navigation document exceeds its 3 MiB payload sub-cap");
        }
        let payload_permit = budget.reserve(charge)?;
        Ok(Self {
            identity,
            absolute_path,
            content_root,
            disk_raw_len,
            server_root,
            language,
            version,
            text,
            line_index,
            structure,
            _payload_permit: payload_permit,
        })
    }

    pub(crate) fn from_source(
        source: &NavigationSource,
        version: DocumentVersion,
        budget: &PayloadBudget,
    ) -> Result<Self> {
        let offsets = source
            .line_index
            .line_count()
            .checked_mul(std::mem::size_of::<usize>())
            .ok_or_else(|| anyhow!("navigation line index charge overflow"))?;
        let tokens = source
            .structure
            .recognizable_tokens
            .ranges
            .len()
            .checked_mul(std::mem::size_of::<SourceRange>())
            .ok_or_else(|| anyhow!("navigation token index charge overflow"))?;
        let charge = source
            .text
            .len()
            .checked_add(offsets)
            .and_then(|value| value.checked_add(tokens))
            .ok_or_else(|| anyhow!("navigation document charge overflow"))?;
        if charge > 3 * 1024 * 1024 {
            bail!("navigation document exceeds its 3 MiB payload sub-cap");
        }
        Ok(Self {
            identity: source.identity.clone(),
            absolute_path: source.absolute_path.clone(),
            content_root: source.content_root.clone(),
            disk_raw_len: source.disk_raw_len,
            server_root: source.server_root.clone(),
            language: source.language,
            version,
            text: Arc::clone(&source.text),
            line_index: Arc::clone(&source.line_index),
            structure: Arc::clone(&source.structure),
            _payload_permit: budget.reserve(charge)?,
        })
    }
}

/// Immutable line mapping for the exact normalized text sent to the server.
#[derive(Clone, Debug)]
pub(crate) struct LineIndex {
    text: Arc<str>,
    starts: Arc<[usize]>,
}

impl LineIndex {
    pub(crate) fn new(text: Arc<str>) -> Result<Self> {
        if text.len() > MAX_NAVIGATION_TEXT_BYTES {
            bail!("navigation document exceeds {MAX_NAVIGATION_TEXT_BYTES} bytes");
        }
        let mut starts = Vec::with_capacity(text.lines().count().saturating_add(1));
        starts.push(0);
        starts.extend(
            text.bytes()
                .enumerate()
                .filter_map(|(index, byte)| (byte == b'\n').then_some(index + 1)),
        );
        if starts.len() > MAX_NAVIGATION_LINES.saturating_add(1) {
            bail!("navigation document exceeds {MAX_NAVIGATION_LINES} lines");
        }
        Ok(Self {
            text,
            starts: starts.into(),
        })
    }

    pub(crate) fn line_count(&self) -> usize {
        self.starts.len()
    }

    pub(crate) fn end_position(&self) -> SourcePosition {
        let line = self.starts.len().saturating_sub(1);
        SourcePosition {
            line,
            byte: self.line(line).map_or(0, str::len),
        }
    }

    pub(crate) fn to_utf16(&self, position: SourcePosition) -> Result<lsp_types::Position> {
        let line = self.line(position.line)?;
        if position.byte > line.len() || !line.is_char_boundary(position.byte) {
            bail!("source byte is outside the line or not a UTF-8 boundary");
        }
        let character = line[..position.byte]
            .chars()
            .try_fold(0u32, |sum, scalar| {
                sum.checked_add(u32::try_from(scalar.len_utf16()).ok()?)
            })
            .ok_or_else(|| anyhow!("UTF-16 character offset overflow"))?;
        Ok(lsp_types::Position {
            line: u32::try_from(position.line).context("line index exceeds LSP range")?,
            character,
        })
    }

    pub(crate) fn source_position_for_utf16(
        &self,
        position: lsp_types::Position,
    ) -> Result<SourcePosition> {
        let line_index =
            usize::try_from(position.line).context("line index cannot be represented")?;
        let line = self.line(line_index)?;
        let mut units = 0u32;
        for (byte, scalar) in line.char_indices() {
            if units == position.character {
                return Ok(SourcePosition {
                    line: line_index,
                    byte,
                });
            }
            let next = units
                .checked_add(u32::try_from(scalar.len_utf16()).expect("char UTF-16 length is <= 2"))
                .ok_or_else(|| anyhow!("UTF-16 character offset overflow"))?;
            if position.character < next {
                bail!("UTF-16 character points into a surrogate pair");
            }
            units = next;
        }
        if units == position.character {
            return Ok(SourcePosition {
                line: line_index,
                byte: line.len(),
            });
        }
        bail!("UTF-16 character is beyond the line end")
    }

    pub(crate) fn range_from_utf16(&self, range: lsp_types::Range) -> Result<SourceRange> {
        let converted = SourceRange {
            start: self.source_position_for_utf16(range.start)?,
            end: self.source_position_for_utf16(range.end)?,
        };
        if converted.start > converted.end {
            bail!("LSP range ends before it starts");
        }
        Ok(converted)
    }

    fn line(&self, index: usize) -> Result<&str> {
        let start = *self
            .starts
            .get(index)
            .ok_or_else(|| anyhow!("line index is outside the document"))?;
        let mut end = self
            .starts
            .get(index + 1)
            .copied()
            .unwrap_or(self.text.len());
        if end > start && self.text.as_bytes()[end - 1] == b'\n' {
            end -= 1;
        }
        if end > start && self.text.as_bytes()[end - 1] == b'\r' {
            end -= 1;
        }
        Ok(&self.text[start..end])
    }
}

#[derive(Clone, Debug)]
pub struct NavigationSettings {
    servers: BTreeMap<LanguageFamily, TrustedServer>,
}

impl NavigationSettings {
    pub fn disabled() -> Self {
        Self {
            servers: BTreeMap::new(),
        }
    }

    pub fn is_enabled(&self) -> bool {
        !self.servers.is_empty()
    }

    pub(crate) fn server(&self, family: LanguageFamily) -> Option<&TrustedServer> {
        self.servers.get(&family)
    }

    /// Load code-navigation settings from the user-level Latte Lens configuration.
    ///
    /// Missing default configuration uses built-in language-server commands.
    /// An explicit path that is missing or invalid disables navigation and
    /// returns a warning; it never prevents the viewer from starting.
    pub fn load_user_config(workspace_root: &Path) -> LoadedNavigationSettings {
        match load_user_config(workspace_root) {
            Ok(settings) => LoadedNavigationSettings {
                settings,
                warning: None,
            },
            Err(error) => LoadedNavigationSettings {
                settings: Self::disabled(),
                warning: Some(clean_warning(&format!("{error:#}"))),
            },
        }
    }

    /// Revalidate already-resolved settings after the App root is canonical.
    pub(crate) fn revalidate(&self, workspace_root: &Path) -> Result<()> {
        for server in self.servers.values() {
            server.revalidate_before_spawn(workspace_root)?;
        }
        Ok(())
    }
}

impl Default for NavigationSettings {
    fn default() -> Self {
        Self::disabled()
    }
}

/// Additive App construction options. Legacy constructors remain hermetic and
/// navigation-disabled; production explicitly loads user configuration first.
#[derive(Clone, Debug, Default)]
pub struct AppOptions {
    pub navigation: NavigationSettings,
    pub navigation_config_warning: Option<String>,
}

#[derive(Clone, Debug)]
pub struct LoadedNavigationSettings {
    pub settings: NavigationSettings,
    pub warning: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct TrustedServer {
    pub(crate) program: PathBuf,
    pub(crate) args: Arc<[String]>,
    pub(crate) identity: ExecutableIdentity,
}

impl TrustedServer {
    pub(crate) fn program(&self) -> &Path {
        &self.program
    }

    pub(crate) fn args(&self) -> &[String] {
        &self.args
    }

    /// Final validation must be called in the same platform spawn function,
    /// immediately before the OS process API. It detects changes since the
    /// explicit user authorization, but does not claim atomic path pinning.
    pub(crate) fn revalidate_before_spawn(&self, workspace_root: &Path) -> Result<()> {
        let validated = validate_executable(&self.program, workspace_root)?;
        if validated.path != self.program || validated.identity != self.identity {
            bail!("Configured language server changed since validation.");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ValidatedExecutable {
    pub(crate) path: PathBuf,
    pub(crate) identity: ExecutableIdentity,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ExecutableIdentity {
    #[cfg(unix)]
    Unix { device: u64, inode: u64, mode: u32 },
    #[cfg(windows)]
    Windows {
        volume_serial: u32,
        file_index: u64,
        attributes: u32,
    },
    #[cfg(not(any(unix, windows)))]
    Portable { length: u64, modified: Option<u128> },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    code_navigation: Option<RawCodeNavigation>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCodeNavigation {
    enabled: Option<bool>,
    languages: Option<RawLanguages>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLanguages {
    rust: Option<RawLanguage>,
    typescript: Option<RawLanguage>,
    python: Option<RawLanguage>,
    go: Option<RawLanguage>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLanguage {
    enabled: Option<bool>,
    engine: Option<RawNavigationEngine>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawNavigationEngine {
    #[serde(rename = "type")]
    kind: RawNavigationEngineKind,
    command: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RawNavigationEngineKind {
    LanguageServer,
}

fn load_user_config(workspace_root: &Path) -> Result<NavigationSettings> {
    let workspace_root = workspace_root
        .canonicalize()
        .with_context(|| format!("cannot resolve workspace {}", workspace_root.display()))?;
    let (path, explicit) = user_config_path()?;
    let raw = if !path.exists() {
        if explicit {
            bail!(
                "explicit Latte Lens config does not exist: {}",
                path.display()
            );
        }
        None
    } else {
        ensure_absolute_no_links(&path)?;
        let mut file = match open_regular(None, &path)? {
            OpenRegular::Opened(file) => file,
            OpenRegular::Declined(inspected) => {
                bail!(
                    "Latte Lens config is a {}, not a regular file",
                    inspected.kind.label()
                )
            }
        };
        let mut bytes = Vec::with_capacity(MAX_CONFIG_BYTES as usize + 1);
        file.by_ref()
            .take(MAX_CONFIG_BYTES + 1)
            .read_to_end(&mut bytes)
            .context("cannot read Latte Lens config")?;
        if bytes.len() as u64 > MAX_CONFIG_BYTES {
            bail!("Latte Lens config exceeds 64 KiB");
        }
        if bytes.starts_with(&[0xef, 0xbb, 0xbf]) {
            bail!("Latte Lens config must not contain a UTF-8 BOM");
        }
        Some(parse_raw_config(&bytes)?)
    };

    let raw_navigation = raw
        .as_ref()
        .and_then(|config| config.code_navigation.as_ref());
    if raw_navigation.and_then(|navigation| navigation.enabled) == Some(false) {
        return Ok(NavigationSettings::disabled());
    }
    let raw_languages = raw_navigation.and_then(|navigation| navigation.languages.as_ref());
    let mut servers = BTreeMap::new();
    for (family, raw) in [
        (
            LanguageFamily::Rust,
            raw_languages.and_then(|value| value.rust.as_ref()),
        ),
        (
            LanguageFamily::TypeScript,
            raw_languages.and_then(|value| value.typescript.as_ref()),
        ),
        (
            LanguageFamily::Python,
            raw_languages.and_then(|value| value.python.as_ref()),
        ),
        (
            LanguageFamily::Go,
            raw_languages.and_then(|value| value.go.as_ref()),
        ),
    ] {
        if raw.and_then(|language| language.enabled) == Some(false) {
            continue;
        }
        if let Some(engine) = raw.and_then(|language| language.engine.as_ref()) {
            match engine.kind {
                RawNavigationEngineKind::LanguageServer => {}
            }
            let server = trusted_server_from_command(&engine.command, family, &workspace_root)?;
            servers.insert(family, server);
        } else {
            let command = default_language_server_command(family);
            // Built-in discovery is best effort: an unavailable language server
            // disables only that language, matching editor-style activation.
            if let Ok(server) = trusted_server_from_command(&command, family, &workspace_root) {
                servers.insert(family, server);
            }
        }
    }
    Ok(NavigationSettings { servers })
}

fn default_language_server_command(family: LanguageFamily) -> Vec<String> {
    let command: &[&str] = match family {
        LanguageFamily::Rust => &["rust-analyzer"],
        LanguageFamily::TypeScript => &["typescript-language-server", "--stdio"],
        LanguageFamily::Python => &["pyright-langserver", "--stdio"],
        LanguageFamily::Go => &["gopls", "serve"],
    };
    command.iter().map(|part| (*part).to_owned()).collect()
}

fn trusted_server_from_command(
    command: &[String],
    family: LanguageFamily,
    workspace_root: &Path,
) -> Result<TrustedServer> {
    let (program, args) = command.split_first().ok_or_else(|| {
        anyhow!(
            "enabled {} code navigation has an empty command",
            family.config_key()
        )
    })?;
    validate_string("code navigation command", program, MAX_PROGRAM_BYTES)?;
    validate_args(args)?;
    let resolved = resolve_program(program)?;
    let validated = validate_executable(&resolved, workspace_root)?;
    Ok(TrustedServer {
        program: validated.path,
        args: Arc::from(args),
        identity: validated.identity,
    })
}

fn parse_raw_config(bytes: &[u8]) -> Result<RawConfig> {
    if bytes.len() as u64 > MAX_CONFIG_BYTES {
        bail!("Latte Lens config exceeds 64 KiB");
    }
    if bytes.starts_with(&[0xef, 0xbb, 0xbf]) {
        bail!("Latte Lens config must not contain a UTF-8 BOM");
    }
    std::str::from_utf8(bytes).context("Latte Lens config is not strict UTF-8")?;
    let normalized = normalize_jsonc(bytes)?;
    serde_json::from_slice(&normalized).context("invalid Latte Lens config")
}

fn user_config_path() -> Result<(PathBuf, bool)> {
    if let Some(path) = env::var_os("LATTELENS_CONFIG") {
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            bail!("LATTELENS_CONFIG must be an absolute path");
        }
        return Ok((path, true));
    }
    #[cfg(not(windows))]
    let home = absolute_env_path("HOME")?;
    #[cfg(windows)]
    let home = absolute_env_path("USERPROFILE")?;
    Ok((home.join(".latte/latte-lens.jsonc"), false))
}

fn normalize_jsonc(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut normalized = bytes.to_vec();
    let mut index = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    while index < normalized.len() {
        let byte = normalized[index];
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            index += 1;
            continue;
        }
        if byte == b'"' {
            in_string = true;
            index += 1;
            continue;
        }
        if byte != b'/' || index + 1 >= normalized.len() {
            index += 1;
            continue;
        }
        match normalized[index + 1] {
            b'/' => {
                normalized[index] = b' ';
                normalized[index + 1] = b' ';
                index += 2;
                while index < normalized.len() && !matches!(normalized[index], b'\n' | b'\r') {
                    normalized[index] = b' ';
                    index += 1;
                }
            }
            b'*' => {
                normalized[index] = b' ';
                normalized[index + 1] = b' ';
                index += 2;
                let mut terminated = false;
                while index < normalized.len() {
                    if index + 1 < normalized.len()
                        && normalized[index] == b'*'
                        && normalized[index + 1] == b'/'
                    {
                        normalized[index] = b' ';
                        normalized[index + 1] = b' ';
                        index += 2;
                        terminated = true;
                        break;
                    }
                    if !matches!(normalized[index], b'\n' | b'\r') {
                        normalized[index] = b' ';
                    }
                    index += 1;
                }
                if !terminated {
                    bail!("invalid Latte Lens config: unterminated block comment");
                }
            }
            _ => index += 1,
        }
    }

    index = 0;
    in_string = false;
    escaped = false;
    while index < normalized.len() {
        let byte = normalized[index];
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
        } else if byte == b'"' {
            in_string = true;
        } else if byte == b',' {
            let mut next = index + 1;
            while next < normalized.len() && normalized[next].is_ascii_whitespace() {
                next += 1;
            }
            if next < normalized.len() && matches!(normalized[next], b'}' | b']') {
                normalized[index] = b' ';
            }
        }
        index += 1;
    }
    Ok(normalized)
}

fn absolute_env_path(name: &str) -> Result<PathBuf> {
    let path = env::var_os(name).ok_or_else(|| anyhow!("{name} is not set"))?;
    let path = PathBuf::from(path);
    if !path.is_absolute() {
        bail!("{name} must be absolute");
    }
    Ok(path)
}

fn validate_string(label: &str, value: &str, max: usize) -> Result<()> {
    if value.is_empty() || value.len() > max || value.contains('\0') {
        bail!("{label} must be 1..={max} bytes and contain no NUL");
    }
    Ok(())
}

fn validate_args(args: &[String]) -> Result<()> {
    if args.len() > MAX_ARGS {
        bail!("language server args exceed {MAX_ARGS} items");
    }
    let mut total = 0usize;
    for arg in args {
        validate_string("language server argument", arg, MAX_ARG_BYTES)?;
        total = total
            .checked_add(arg.len())
            .ok_or_else(|| anyhow!("language server args length overflow"))?;
    }
    if total > MAX_ARGS_BYTES {
        bail!("language server args exceed {MAX_ARGS_BYTES} bytes");
    }
    Ok(())
}

fn resolve_program(program: &str) -> Result<PathBuf> {
    let path = Path::new(program);
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    if path.components().count() != 1 || has_path_separator(program) {
        bail!("language server program must be absolute or a basename");
    }
    resolve_basename(program)
}

fn has_path_separator(program: &str) -> bool {
    program.contains('/') || program.contains('\\')
}

fn resolve_basename(program: &str) -> Result<PathBuf> {
    #[cfg(windows)]
    let candidates: Vec<String> = {
        let lower = program.to_ascii_lowercase();
        if lower.ends_with(".cmd")
            || lower.ends_with(".bat")
            || lower.ends_with(".com")
            || lower.ends_with(".ps1")
        {
            bail!("Windows language server must be a native .exe");
        }
        if lower.ends_with(".exe") {
            vec![program.to_owned()]
        } else {
            vec![program.to_owned(), format!("{program}.exe")]
        }
    };
    #[cfg(not(windows))]
    let candidates = vec![program.to_owned()];

    let path = env::var_os("PATH").ok_or_else(|| anyhow!("PATH is not set"))?;
    for directory in env::split_paths(&path) {
        if !directory.is_absolute() {
            continue;
        }
        for candidate in &candidates {
            let joined = directory.join(candidate);
            if joined.is_file() {
                return Ok(joined);
            }
        }
    }
    bail!("configured language server basename was not found in absolute PATH entries")
}

pub(crate) fn validate_executable(
    path: &Path,
    workspace_root: &Path,
) -> Result<ValidatedExecutable> {
    if !path.is_absolute() {
        bail!("language server executable must be absolute");
    }
    if path.starts_with(workspace_root) {
        bail!("language server executable entry must be outside the opened workspace");
    }
    // User-level package managers commonly expose language servers through a
    // bin symlink. Resolve that indirection once and retain only the canonical
    // target: later link replacement cannot redirect the trusted command.
    // The canonical target itself must still traverse no links/reparse points,
    // live outside the workspace, and retain the same file identity at spawn.
    let canonical = path
        .canonicalize()
        .with_context(|| format!("cannot resolve language server {}", path.display()))?;
    ensure_absolute_no_links(&canonical)?;
    let metadata = canonical
        .metadata()
        .with_context(|| format!("cannot inspect language server {}", canonical.display()))?;
    if !metadata.is_file() {
        bail!("language server executable is not a regular file");
    }
    if canonical.starts_with(workspace_root) {
        bail!("language server executable must be outside the opened workspace");
    }
    validate_platform_executable(&canonical, &metadata)?;
    Ok(ValidatedExecutable {
        identity: executable_identity(&canonical, &metadata)?,
        path: canonical,
    })
}

fn ensure_absolute_no_links(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        bail!("path must be absolute: {}", path.display());
    }
    let mut current = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => current.push(prefix.as_os_str()),
            Component::RootDir => current.push(Path::new(std::path::MAIN_SEPARATOR_STR)),
            Component::Normal(value) => {
                current.push(value);
                let metadata = std::fs::symlink_metadata(&current)
                    .with_context(|| format!("cannot inspect {}", current.display()))?;
                if metadata.file_type().is_symlink() {
                    bail!("path traverses symbolic link at {}", current.display());
                }
                #[cfg(windows)]
                if windows_is_reparse(&metadata) {
                    bail!("path traverses reparse point at {}", current.display());
                }
            }
            Component::CurDir | Component::ParentDir => {
                bail!("path contains a relative component: {}", path.display())
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn validate_platform_executable(_path: &Path, metadata: &Metadata) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    if metadata.mode() & 0o111 == 0 {
        bail!("language server executable has no execute bit");
    }
    Ok(())
}

#[cfg(windows)]
fn validate_platform_executable(path: &Path, _metadata: &Metadata) -> Result<()> {
    use std::ffi::OsStr;
    let extension = path.extension().and_then(OsStr::to_str).unwrap_or_default();
    if !extension.eq_ignore_ascii_case("exe") {
        bail!("Windows language server must be a native .exe");
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn validate_platform_executable(_path: &Path, _metadata: &Metadata) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn executable_identity(_path: &Path, metadata: &Metadata) -> Result<ExecutableIdentity> {
    use std::os::unix::fs::MetadataExt;
    Ok(ExecutableIdentity::Unix {
        device: metadata.dev(),
        inode: metadata.ino(),
        mode: metadata.mode(),
    })
}

#[cfg(windows)]
fn executable_identity(path: &Path, _metadata: &Metadata) -> Result<ExecutableIdentity> {
    use std::{os::windows::fs::OpenOptionsExt, os::windows::io::AsRawHandle};
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_FLAG_OPEN_REPARSE_POINT, GetFileInformationByHandle,
    };
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .with_context(|| format!("cannot open language server identity {}", path.display()))?;
    let mut information = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
    // SAFETY: file is live and the API initializes the output on success.
    let succeeded =
        unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, information.as_mut_ptr()) };
    if succeeded == 0 {
        return Err(std::io::Error::last_os_error()).context("cannot read executable identity");
    }
    // SAFETY: the successful call initialized the structure.
    let information = unsafe { information.assume_init() };
    Ok(ExecutableIdentity::Windows {
        volume_serial: information.dwVolumeSerialNumber,
        file_index: (u64::from(information.nFileIndexHigh) << 32)
            | u64::from(information.nFileIndexLow),
        attributes: information.dwFileAttributes,
    })
}

#[cfg(not(any(unix, windows)))]
fn executable_identity(_path: &Path, metadata: &Metadata) -> Result<ExecutableIdentity> {
    use std::time::UNIX_EPOCH;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos());
    Ok(ExecutableIdentity::Portable {
        length: metadata.len(),
        modified,
    })
}

#[cfg(windows)]
fn windows_is_reparse(metadata: &Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

pub(crate) fn path_to_lsp_uri(path: &Path) -> Result<lsp_types::Uri> {
    use std::str::FromStr;
    let url = url::Url::from_file_path(path)
        .map_err(|_| anyhow!("path cannot be represented as a file URI"))?;
    lsp_types::Uri::from_str(url.as_str()).context("file URI is not valid for LSP")
}

pub(crate) fn lsp_uri_to_safe_path(uri: &lsp_types::Uri, workspace_root: &Path) -> Result<PathBuf> {
    let path = lsp_uri_to_file_path(uri)?;
    let path = normalize_lsp_target_path(workspace_root, &path)?;
    let inspected = inspect_content_path(Some(workspace_root), &path)?;
    if inspected.kind != ContentPathKind::Regular || inspected.path != path {
        bail!("navigation target is not a safe regular workspace file");
    }
    Ok(path)
}

/// Classify an LSP location as either a normal workspace target or a
/// transient dependency source. Dependency targets are intentionally limited
/// to package roots with a language manifest and retain the same no-follow
/// regular-file checks as workspace previews.
pub(crate) fn lsp_uri_to_navigation_target(
    uri: &lsp_types::Uri,
    workspace_root: &Path,
) -> Result<NavigationFileTarget> {
    if let Ok(path) = lsp_uri_to_safe_path(uri, workspace_root) {
        return Ok(NavigationFileTarget::Workspace(path));
    }
    let path = lsp_uri_to_file_path(uri)?;
    dependency_target_for_path(&path).map(NavigationFileTarget::Dependency)
}

fn lsp_uri_to_file_path(uri: &lsp_types::Uri) -> Result<PathBuf> {
    let url = url::Url::parse(uri.as_str()).context("invalid LSP URI")?;
    if url.scheme() != "file" || url.query().is_some() || url.fragment().is_some() {
        bail!("navigation target must be an unqualified file URI");
    }
    let path = url
        .to_file_path()
        .map_err(|_| anyhow!("file URI cannot be represented on this platform"))?;
    if !path.is_absolute() {
        bail!("navigation target is not an absolute file path");
    }
    Ok(path)
}

const DEPENDENCY_MANIFESTS: [&str; 5] = [
    "go.mod",
    "Cargo.toml",
    "package.json",
    "pyproject.toml",
    "setup.py",
];
const MAX_DEPENDENCY_ROOT_ANCESTORS: usize = 32;

fn dependency_target_for_path(path: &Path) -> Result<DependencyTarget> {
    let filesystem_root = path
        .ancestors()
        .last()
        .filter(|root| root.is_absolute())
        .ok_or_else(|| anyhow!("dependency target has no filesystem root"))?;
    let inspected = inspect_content_path(Some(filesystem_root), path)?;
    if inspected.kind != ContentPathKind::Regular || inspected.path != path {
        bail!("dependency target is not a safe regular file");
    }

    let mut candidate = path
        .parent()
        .filter(|parent| parent.starts_with(filesystem_root))
        .ok_or_else(|| anyhow!("dependency target has no parent directory"))?;
    for _ in 0..MAX_DEPENDENCY_ROOT_ANCESTORS {
        let directory = inspect_content_path(Some(filesystem_root), candidate)?;
        if directory.kind != ContentPathKind::Directory || directory.path != candidate {
            bail!("dependency target traverses a non-directory path component");
        }
        for manifest_name in DEPENDENCY_MANIFESTS {
            let manifest_path = candidate.join(manifest_name);
            if !path_exists_without_following(&manifest_path) {
                continue;
            }
            let inspected_manifest = inspect_content_path(Some(filesystem_root), &manifest_path)?;
            if inspected_manifest.kind != ContentPathKind::Regular
                || inspected_manifest.path != manifest_path
            {
                bail!("dependency package manifest is not a safe regular file");
            }

            // Windows may expose an LSP file URI through a non-canonical user
            // profile spelling while `File::canonicalize` later resolves it to
            // the physical profile directory. Keep both sides of the preview
            // boundary in that same canonical spelling before handing it to
            // the content-safety gate.
            let canonical_root = candidate.canonicalize().with_context(|| {
                format!("cannot resolve dependency root {}", candidate.display())
            })?;
            let canonical_path = path
                .canonicalize()
                .with_context(|| format!("cannot resolve dependency target {}", path.display()))?;
            let relative = canonical_path
                .strip_prefix(&canonical_root)
                .context("dependency target escaped its package root")?
                .to_path_buf();
            if relative.as_os_str().is_empty() {
                bail!("dependency target cannot be a package directory");
            }
            let canonical_target = inspect_content_path(Some(&canonical_root), &canonical_path)?;
            if canonical_target.kind != ContentPathKind::Regular
                || canonical_target.path != canonical_path
            {
                bail!("canonical dependency target is not a safe regular file");
            }
            let canonical_manifest_path = canonical_root.join(manifest_name);
            let canonical_manifest =
                inspect_content_path(Some(&canonical_root), &canonical_manifest_path)?;
            if canonical_manifest.kind != ContentPathKind::Regular
                || canonical_manifest.path != canonical_manifest_path
            {
                bail!("canonical dependency package manifest is not a safe regular file");
            }
            return Ok(DependencyTarget {
                root: canonical_root,
                relative,
            });
        }
        let Some(parent) = candidate.parent() else {
            break;
        };
        if parent == candidate || !parent.starts_with(filesystem_root) {
            break;
        }
        candidate = parent;
    }
    bail!(
        "navigation target is outside the opened workspace and not a recognized dependency source"
    )
}

#[cfg(not(windows))]
fn normalize_lsp_target_path(_workspace_root: &Path, path: &Path) -> Result<PathBuf> {
    Ok(path.to_path_buf())
}

#[cfg(windows)]
fn normalize_lsp_target_path(workspace_root: &Path, path: &Path) -> Result<PathBuf> {
    use std::{ffi::OsString, path::Prefix};

    #[derive(Eq, PartialEq)]
    enum Volume {
        Disk(u8),
        Unc(OsString, OsString),
    }

    fn absolute_parts(path: &Path) -> Result<(Volume, Vec<OsString>)> {
        let mut components = path.components();
        let volume = match components.next() {
            Some(Component::Prefix(prefix)) => match prefix.kind() {
                Prefix::Disk(disk) | Prefix::VerbatimDisk(disk) => {
                    Volume::Disk(disk.to_ascii_lowercase())
                }
                Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => {
                    Volume::Unc(server.to_ascii_lowercase(), share.to_ascii_lowercase())
                }
                _ => bail!("navigation target uses an unsupported Windows path prefix"),
            },
            _ => bail!("navigation target is not an absolute Windows path"),
        };
        if !matches!(components.next(), Some(Component::RootDir)) {
            bail!("navigation target is not an absolute Windows path");
        }
        let mut names = Vec::new();
        for component in components {
            match component {
                Component::Normal(name) => names.push(name.to_ascii_lowercase()),
                _ => bail!("navigation target contains a non-normal Windows path component"),
            }
        }
        Ok((volume, names))
    }

    if path.strip_prefix(workspace_root).is_ok() {
        return Ok(path.to_path_buf());
    }

    let (root_volume, root_names) = absolute_parts(workspace_root)?;
    let (target_volume, target_names) = absolute_parts(path)?;
    if root_volume != target_volume
        || target_names.len() < root_names.len()
        || target_names[..root_names.len()] != root_names
    {
        bail!("navigation target is outside the opened workspace");
    }

    let mut normalized = workspace_root.to_path_buf();
    for component in path.components().skip(2 + root_names.len()) {
        let Component::Normal(name) = component else {
            bail!("navigation target contains a non-normal Windows path component");
        };
        normalized.push(name);
    }
    Ok(normalized)
}

fn clean_warning(message: &str) -> String {
    let mut cleaned = String::with_capacity(message.len().min(240));
    for character in message.chars() {
        if cleaned.len() >= 240 {
            break;
        }
        if !character.is_control() && character != '\u{1b}' {
            cleaned.push(character);
        }
    }
    cleaned
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, fs, sync::Arc};

    use lsp_types::Position;

    use super::*;

    struct EnvironmentGuard(Vec<(&'static str, Option<OsString>)>);

    impl EnvironmentGuard {
        fn apply(entries: &[(&'static str, Option<OsString>)]) -> Self {
            let previous = entries
                .iter()
                .map(|(name, _)| (*name, std::env::var_os(name)))
                .collect();
            for (name, value) in entries {
                // SAFETY: every unit test that mutates navigation environment
                // variables holds the shared navigation environment lock.
                unsafe {
                    if let Some(value) = value {
                        std::env::set_var(name, value);
                    } else {
                        std::env::remove_var(name);
                    }
                }
            }
            Self(previous)
        }
    }

    impl Drop for EnvironmentGuard {
        fn drop(&mut self) {
            for (name, value) in self.0.iter().rev() {
                // SAFETY: the guard is dropped before releasing the shared
                // navigation environment lock acquired by its caller.
                unsafe {
                    if let Some(value) = value {
                        std::env::set_var(name, value);
                    } else {
                        std::env::remove_var(name);
                    }
                }
            }
        }
    }

    #[test]
    fn line_index_round_trips_utf16_and_rejects_half_surrogate() {
        let index = LineIndex::new(Arc::from("a中😀e\u{301}\r\n\nlast")).unwrap();
        for source in [
            SourcePosition { line: 0, byte: 0 },
            SourcePosition { line: 0, byte: 1 },
            SourcePosition { line: 0, byte: 4 },
            SourcePosition { line: 0, byte: 8 },
            SourcePosition { line: 0, byte: 11 },
            SourcePosition { line: 1, byte: 0 },
            SourcePosition { line: 2, byte: 4 },
        ] {
            let protocol = index.to_utf16(source).unwrap();
            assert_eq!(index.source_position_for_utf16(protocol).unwrap(), source);
        }
        assert!(
            index
                .source_position_for_utf16(Position::new(0, 3))
                .is_err()
        );
        assert!(
            index
                .source_position_for_utf16(Position::new(0, 99))
                .is_err()
        );
        assert!(index.to_utf16(SourcePosition { line: 0, byte: 2 }).is_err());
    }

    #[test]
    fn language_mapping_keeps_semantic_families_explicit() {
        for (extension, family, language_id) in [
            ("rs", Some(LanguageFamily::Rust), "rust"),
            ("ts", Some(LanguageFamily::TypeScript), "typescript"),
            ("mts", Some(LanguageFamily::TypeScript), "typescript"),
            ("cts", Some(LanguageFamily::TypeScript), "typescript"),
            ("tsx", Some(LanguageFamily::TypeScript), "typescriptreact"),
            ("js", Some(LanguageFamily::TypeScript), "javascript"),
            ("mjs", Some(LanguageFamily::TypeScript), "javascript"),
            ("cjs", Some(LanguageFamily::TypeScript), "javascript"),
            ("jsx", Some(LanguageFamily::TypeScript), "javascriptreact"),
            ("py", Some(LanguageFamily::Python), "python"),
            ("pyi", Some(LanguageFamily::Python), "python"),
            ("go", Some(LanguageFamily::Go), "go"),
            ("md", None, "markdown"),
            ("markdown", None, "markdown"),
        ] {
            let descriptor = language_for_path(Path::new(&format!("x.{extension}"))).unwrap();
            assert_eq!(descriptor.family, family);
            assert_eq!(descriptor.language_id, language_id);
            assert!(descriptor.local_structure);
        }
        for (family, key, name) in [
            (LanguageFamily::Rust, "rust", "Rust"),
            (
                LanguageFamily::TypeScript,
                "typescript",
                "TypeScript/JavaScript",
            ),
            (LanguageFamily::Python, "python", "Python"),
            (LanguageFamily::Go, "go", "Go"),
        ] {
            assert_eq!(family.config_key(), key);
            assert_eq!(family.display_name(), name);
        }
        assert!(language_for_path(Path::new("x.txt")).is_none());
        assert!(language_for_path(Path::new("README")).is_none());
    }

    #[test]
    fn navigation_documents_own_budget_and_reject_text_line_and_token_overflow() {
        use crate::{
            folding::StructureSnapshot,
            lsp::{GlobalPayloadBudget, PayloadBudget},
            runtime::ContentIdentity,
        };

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let path = root.join("main.rs");
        fs::write(&path, "fn main() {}\r\n").unwrap();
        let path = path.canonicalize().unwrap();
        let text: Arc<str> = Arc::from("fn main() {}\r\n");
        let identity = ContentIdentity::from_absolute(&root, &path).unwrap();
        let structure = Arc::new(StructureSnapshot::unavailable());
        let budget = PayloadBudget::session(&GlobalPayloadBudget::default());
        let document = NavigationDocument::new(
            identity.clone(),
            path.clone(),
            root.clone(),
            text.len() as u64,
            root.clone(),
            language_for_path(&path).unwrap(),
            DocumentVersion(7),
            Arc::clone(&text),
            Arc::clone(&structure),
            &budget,
        )
        .unwrap();
        assert_eq!(document.version, DocumentVersion(7));
        assert_eq!(
            document.line_index.end_position(),
            SourcePosition { line: 1, byte: 0 }
        );
        let source = NavigationSource {
            identity,
            absolute_path: path,
            content_root: root.clone(),
            disk_raw_len: text.len() as u64,
            server_root: root,
            language: document.language,
            text,
            line_index: Arc::clone(&document.line_index),
            structure,
        };
        let cloned = NavigationDocument::from_source(&source, DocumentVersion(8), &budget).unwrap();
        assert_eq!(cloned.version, DocumentVersion(8));
        assert!(Arc::ptr_eq(&cloned.line_index, &source.line_index));

        assert!(LineIndex::new(Arc::from("x".repeat(MAX_NAVIGATION_TEXT_BYTES + 1))).is_err());
        assert!(LineIndex::new(Arc::from("\n".repeat(MAX_NAVIGATION_LINES + 1))).is_err());
        assert!(
            cloned
                .line_index
                .range_from_utf16(lsp_types::Range::new(
                    Position::new(0, 2),
                    Position::new(0, 1)
                ))
                .is_err()
        );
        assert!(
            cloned
                .line_index
                .to_utf16(SourcePosition { line: 99, byte: 0 })
                .is_err()
        );

        let mut huge = StructureSnapshot::unavailable();
        huge.recognizable_tokens.ranges = vec![
            SourceRange {
                start: SourcePosition { line: 0, byte: 0 },
                end: SourcePosition { line: 0, byte: 1 },
            };
            200_000
        ];
        let huge = Arc::new(huge);
        let mut huge_source = source;
        huge_source.structure = Arc::clone(&huge);
        assert!(
            NavigationDocument::from_source(&huge_source, DocumentVersion(9), &budget).is_err()
        );
        assert!(
            NavigationDocument::new(
                huge_source.identity.clone(),
                huge_source.absolute_path.clone(),
                huge_source.content_root.clone(),
                huge_source.disk_raw_len,
                huge_source.server_root.clone(),
                huge_source.language,
                DocumentVersion(9),
                Arc::clone(&huge_source.text),
                huge,
                &budget,
            )
            .is_err()
        );
    }

    #[test]
    fn jsonc_schema_rejects_duplicate_and_unknown_fields_but_allows_partial_overrides() {
        for input in [
            r#"{"code_navigation":{},"code_navigation":{}}"#,
            r#"{"version":1}"#,
            r#"{"unknown":1}"#,
            r#"{"code_navigation":{"enabled":true,"languages":{"rust":{"enabled":true,"extra":1}}}}"#,
            r#"{"code_navigation":{"enabled":true,"languages":{"java":{"enabled":true}}}}"#,
            r#"{"code_navigation":{"enabled":true,"languages":{"rust":{"enabled":true,"engine":{"type":"other","command":[]}}}}}"#,
        ] {
            assert!(parse_raw_config(input.as_bytes()).is_err(), "{input}");
        }
        let inherited =
            parse_raw_config(br#"{"code_navigation":{"languages":{"rust":{"enabled":true}}}}"#)
                .unwrap();
        let navigation = inherited.code_navigation.unwrap();
        assert_eq!(navigation.enabled, None);
        let rust = navigation.languages.unwrap().rust.unwrap();
        assert_eq!(rust.enabled, Some(true));
        assert!(rust.engine.is_none());
    }

    #[test]
    fn jsonc_accepts_comments_trailing_commas_and_comment_markers_in_strings() {
        let parsed = parse_raw_config(
            br#"{
                // Product configuration, not an LSP-specific file.
                "code_navigation": {
                    "enabled": true,
                    "languages": {
                        "rust": {
                            "enabled": true,
                            "engine": {
                                "type": "language_server",
                                "command": ["https://example.invalid//server",],
                            },
                        },
                    },
                },
                /* Future product domains live beside code_navigation. */
            }"#,
        )
        .unwrap();
        let rust = parsed
            .code_navigation
            .unwrap()
            .languages
            .unwrap()
            .rust
            .unwrap();
        assert_eq!(
            rust.engine.unwrap().command[0],
            "https://example.invalid//server"
        );
        assert!(parse_raw_config(br#"{} /* unterminated"#).is_err());
    }

    #[test]
    fn config_bytes_enforce_exact_size_utf8_and_bom_boundaries() {
        let mut exact = br#"{}"#.to_vec();
        exact.resize(MAX_CONFIG_BYTES as usize, b' ');
        assert!(parse_raw_config(&exact).is_ok());
        exact.push(b' ');
        assert!(parse_raw_config(&exact).is_err());
        assert!(parse_raw_config(b"\xef\xbb\xbf{}").is_err());
        assert!(parse_raw_config(&[0xff]).is_err());
    }

    #[test]
    fn default_config_path_uses_the_cross_platform_latte_home() {
        let _environment = lock_navigation_environment();
        let home = tempfile::tempdir().unwrap();
        #[cfg(not(windows))]
        let home_variable = "HOME";
        #[cfg(windows)]
        let home_variable = "USERPROFILE";
        let _variables = EnvironmentGuard::apply(&[
            ("LATTELENS_CONFIG", None),
            (home_variable, Some(home.path().as_os_str().to_os_string())),
        ]);
        let (path, explicit) = user_config_path().unwrap();
        assert!(!explicit);
        assert_eq!(path, home.path().join(".latte/latte-lens.jsonc"));
    }

    #[test]
    fn built_in_navigation_commands_cover_every_supported_language() {
        assert_eq!(
            default_language_server_command(LanguageFamily::Rust),
            ["rust-analyzer"]
        );
        assert_eq!(
            default_language_server_command(LanguageFamily::TypeScript),
            ["typescript-language-server", "--stdio"]
        );
        assert_eq!(
            default_language_server_command(LanguageFamily::Python),
            ["pyright-langserver", "--stdio"]
        );
        assert_eq!(
            default_language_server_command(LanguageFamily::Go),
            ["gopls", "serve"]
        );
    }

    #[test]
    fn args_and_program_bounds_are_fail_closed() {
        assert!(validate_string("program", "", MAX_PROGRAM_BYTES).is_err());
        assert!(validate_string("program", "bad\0name", MAX_PROGRAM_BYTES).is_err());
        assert!(
            validate_string(
                "program",
                &"x".repeat(MAX_PROGRAM_BYTES + 1),
                MAX_PROGRAM_BYTES
            )
            .is_err()
        );
        assert!(validate_args(&vec!["a".into(); MAX_ARGS + 1]).is_err());
        assert!(validate_args(&["x".repeat(MAX_ARG_BYTES + 1)]).is_err());
        assert!(validate_args(&vec!["x".repeat(MAX_ARG_BYTES); 5]).is_err());
        assert!(resolve_program("./server").is_err());
        assert!(resolve_program("../server").is_err());
        assert!(resolve_program("nested\\server").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn user_config_authorization_and_path_resolution_are_fail_closed() {
        use std::os::unix::fs::PermissionsExt;

        let _environment = lock_navigation_environment();
        let container = tempfile::tempdir().unwrap();
        let container_root = container.path().canonicalize().unwrap();
        let workspace = container_root.join("workspace");
        let home = container_root.join("home");
        let tools = container_root.join("tools");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&tools).unwrap();
        let workspace = workspace.canonicalize().unwrap();
        let config = container_root.join("latte-lens.jsonc");
        let default_server = tools.join("rust-analyzer");
        fs::write(&default_server, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&default_server, fs::Permissions::from_mode(0o755)).unwrap();
        let default_server = default_server.canonicalize().unwrap();
        let controlled_path = std::env::join_paths([tools.clone()]).unwrap();

        {
            let missing = container_root.join("missing.json");
            let _variables =
                EnvironmentGuard::apply(&[("LATTELENS_CONFIG", Some(missing.into_os_string()))]);
            let loaded = NavigationSettings::load_user_config(&workspace);
            assert!(!loaded.settings.is_enabled());
            assert!(
                loaded
                    .warning
                    .is_some_and(|warning| warning.contains("does not exist"))
            );
        }
        {
            let _variables = EnvironmentGuard::apply(&[(
                "LATTELENS_CONFIG",
                Some(OsString::from("relative/latte-lens.jsonc")),
            )]);
            let loaded = NavigationSettings::load_user_config(&workspace);
            assert!(!loaded.settings.is_enabled());
            assert!(
                loaded
                    .warning
                    .is_some_and(|warning| warning.contains("absolute path"))
            );
        }
        {
            let _variables = EnvironmentGuard::apply(&[
                ("LATTELENS_CONFIG", None),
                ("HOME", Some(home.clone().into_os_string())),
                ("PATH", Some(controlled_path.clone())),
            ]);
            let loaded = NavigationSettings::load_user_config(&workspace);
            assert!(loaded.settings.is_enabled());
            assert!(loaded.warning.is_none());
            let rust = loaded.settings.server(LanguageFamily::Rust).unwrap();
            assert_eq!(rust.program(), default_server);
            assert!(rust.args().is_empty());
        }
        for home_value in [None, Some(OsString::from("relative-home"))] {
            let _variables =
                EnvironmentGuard::apply(&[("LATTELENS_CONFIG", None), ("HOME", home_value)]);
            let loaded = NavigationSettings::load_user_config(&workspace);
            assert!(!loaded.settings.is_enabled());
            assert!(loaded.warning.is_some());
        }

        {
            let _variables = EnvironmentGuard::apply(&[(
                "LATTELENS_CONFIG",
                Some(container_root.as_os_str().to_os_string()),
            )]);
            let loaded = NavigationSettings::load_user_config(&workspace);
            assert!(!loaded.settings.is_enabled());
            assert!(
                loaded
                    .warning
                    .is_some_and(|warning| warning.contains("directory"))
            );
        }

        let explicit = [("LATTELENS_CONFIG", Some(config.clone().into_os_string()))];
        {
            fs::write(&config, vec![b' '; MAX_CONFIG_BYTES as usize + 1]).unwrap();
            let _variables = EnvironmentGuard::apply(&explicit);
            let loaded = NavigationSettings::load_user_config(&workspace);
            assert!(
                loaded
                    .warning
                    .is_some_and(|warning| warning.contains("64 KiB"))
            );
        }
        {
            fs::write(&config, b"\xef\xbb\xbf{}").unwrap();
            let _variables = EnvironmentGuard::apply(&explicit);
            let loaded = NavigationSettings::load_user_config(&workspace);
            assert!(
                loaded
                    .warning
                    .is_some_and(|warning| warning.contains("UTF-8 BOM"))
            );
        }
        for (raw, enabled) in [
            (r#"{}"#, true),
            (r#"{"code_navigation":{"enabled":false}}"#, false),
            (r#"{"code_navigation":{"enabled":true}}"#, true),
            (
                r#"{"code_navigation":{"enabled":true,"languages":{"rust":{"enabled":false}}}}"#,
                false,
            ),
        ] {
            fs::write(&config, raw).unwrap();
            let _variables = EnvironmentGuard::apply(&[
                explicit[0].clone(),
                ("PATH", Some(controlled_path.clone())),
            ]);
            let loaded = NavigationSettings::load_user_config(&workspace);
            assert!(loaded.warning.is_none(), "raw={raw:?}");
            assert_eq!(loaded.settings.is_enabled(), enabled, "raw={raw:?}");
        }
        {
            fs::write(
                &config,
                r#"{"code_navigation":{"enabled":true,"languages":{"rust":{"enabled":true}}}}"#,
            )
            .unwrap();
            let _variables = EnvironmentGuard::apply(&[
                explicit[0].clone(),
                ("PATH", Some(controlled_path.clone())),
            ]);
            let loaded = NavigationSettings::load_user_config(&workspace);
            assert!(loaded.warning.is_none());
            assert_eq!(
                loaded
                    .settings
                    .server(LanguageFamily::Rust)
                    .unwrap()
                    .program(),
                default_server
            );
        }

        let server = tools.join("language-server");
        fs::write(&server, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&server, fs::Permissions::from_mode(0o755)).unwrap();
        let server = server.canonicalize().unwrap();
        fs::write(
            &config,
            serde_json::to_vec(&serde_json::json!({
                "code_navigation": {
                    "enabled": true,
                    "languages": {
                        "rust": {
                            "enabled": true,
                            "engine": {
                                "type": "language_server",
                                "command": [server, "--stdio"]
                            }
                        },
                        "go": {"enabled": false}
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();
        {
            let _variables = EnvironmentGuard::apply(&explicit);
            let loaded = NavigationSettings::load_user_config(&workspace);
            assert!(loaded.warning.is_none(), "{:?}", loaded.warning);
            assert!(loaded.settings.is_enabled());
            let rust = loaded.settings.server(LanguageFamily::Rust).unwrap();
            assert_eq!(rust.program(), server);
            assert_eq!(rust.args(), ["--stdio"]);
            loaded.settings.revalidate(&workspace).unwrap();
        }

        fs::write(
            &config,
            r#"{"code_navigation":{"enabled":true,"languages":{"rust":{"enabled":true,"engine":{"type":"language_server","command":["language-server"]}}}}}"#,
        )
        .unwrap();
        let path = std::env::join_paths([PathBuf::from("relative-bin"), tools.clone()]).unwrap();
        {
            let _variables = EnvironmentGuard::apply(&[
                ("LATTELENS_CONFIG", Some(config.clone().into_os_string())),
                ("PATH", Some(path)),
            ]);
            let loaded = NavigationSettings::load_user_config(&workspace);
            assert!(loaded.warning.is_none(), "{:?}", loaded.warning);
            assert_eq!(
                loaded
                    .settings
                    .server(LanguageFamily::Rust)
                    .unwrap()
                    .program(),
                server
            );
        }
        for path in [None, Some(OsString::from("relative-bin"))] {
            let _variables = EnvironmentGuard::apply(&[
                ("LATTELENS_CONFIG", Some(config.clone().into_os_string())),
                ("PATH", path),
            ]);
            let loaded = NavigationSettings::load_user_config(&workspace);
            assert!(!loaded.settings.is_enabled());
            assert!(loaded.warning.is_some());
        }
    }

    #[cfg(unix)]
    #[test]
    fn executable_and_target_path_checks_resolve_links_and_reject_modes_and_escapes() {
        use std::{os::unix::fs::PermissionsExt, str::FromStr};

        let container = tempfile::tempdir().unwrap();
        let workspace = container.path().join("workspace");
        let tools = container.path().join("tools");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&tools).unwrap();
        let workspace = workspace.canonicalize().unwrap();
        let directory = tools.canonicalize().unwrap();
        assert!(validate_executable(Path::new("relative"), &workspace).is_err());
        assert!(validate_executable(&directory, &workspace).is_err());

        let server = directory.join("server");
        fs::write(&server, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&server, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(validate_executable(&server, &workspace).is_err());
        fs::set_permissions(&server, fs::Permissions::from_mode(0o755)).unwrap();
        validate_executable(&server, &workspace).unwrap();
        let link = directory.join("server-link");
        std::os::unix::fs::symlink(&server, &link).unwrap();
        let linked = validate_executable(&link, &workspace).unwrap();
        assert_eq!(linked.path, server.canonicalize().unwrap());
        let workspace_link = workspace.join("server-link");
        std::os::unix::fs::symlink(&server, &workspace_link).unwrap();
        assert!(validate_executable(&workspace_link, &workspace).is_err());

        let broken = directory.join("broken-link");
        std::os::unix::fs::symlink(directory.join("missing-server"), &broken).unwrap();
        assert!(validate_executable(&broken, &workspace).is_err());

        let cycle_a = directory.join("cycle-a");
        let cycle_b = directory.join("cycle-b");
        std::os::unix::fs::symlink(&cycle_b, &cycle_a).unwrap();
        std::os::unix::fs::symlink(&cycle_a, &cycle_b).unwrap();
        assert!(validate_executable(&cycle_a, &workspace).is_err());
        assert!(ensure_absolute_no_links(Path::new("relative/../server")).is_err());

        let inside = workspace.join("inside.rs");
        let outside = directory.join("outside.rs");
        fs::write(&inside, "inside").unwrap();
        fs::write(&outside, "outside").unwrap();
        let outside_uri = path_to_lsp_uri(&outside).unwrap();
        assert!(lsp_uri_to_safe_path(&outside_uri, &workspace).is_err());
        let directory_uri = path_to_lsp_uri(&workspace).unwrap();
        assert!(lsp_uri_to_safe_path(&directory_uri, &workspace).is_err());
        let http = lsp_types::Uri::from_str("https://example.com/main.rs").unwrap();
        assert!(lsp_uri_to_safe_path(&http, &workspace).is_err());
        let fragment = lsp_types::Uri::from_str("file:///tmp/main.rs#symbol").unwrap();
        assert!(lsp_uri_to_safe_path(&fragment, &workspace).is_err());
    }

    #[test]
    fn lsp_uri_to_navigation_target_accepts_a_safe_external_package_source() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().join("workspace");
        let dependency = directory.path().join("cache/example.com/module@v1.2.3");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&dependency).unwrap();
        fs::write(dependency.join("go.mod"), "module example.com/module\n").unwrap();
        fs::write(dependency.join("source.go"), "package module\n").unwrap();
        let workspace = workspace.canonicalize().unwrap();
        let dependency = dependency.canonicalize().unwrap();
        let source = dependency.join("source.go");
        let uri = path_to_lsp_uri(&source).unwrap();
        let uri_path = url::Url::parse(uri.as_str())
            .unwrap()
            .to_file_path()
            .unwrap();
        let canonical_target = uri_path.canonicalize().unwrap();
        let canonical_root = canonical_target.parent().unwrap().to_path_buf();

        assert_eq!(
            lsp_uri_to_navigation_target(&uri, &workspace).unwrap(),
            NavigationFileTarget::Dependency(DependencyTarget {
                root: canonical_root,
                relative: PathBuf::from("source.go"),
            })
        );
    }

    #[test]
    fn lsp_uri_to_navigation_target_rejects_external_files_without_a_package_root() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().join("workspace");
        let external = directory.path().join("external.rs");
        fs::create_dir_all(&workspace).unwrap();
        fs::write(&external, "fn external() {}\n").unwrap();
        let workspace = workspace.canonicalize().unwrap();
        let external = external.canonicalize().unwrap();
        let uri = path_to_lsp_uri(&external).unwrap();

        assert!(lsp_uri_to_navigation_target(&uri, &workspace).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn executable_trust_rejects_workspace_and_detects_replacement() {
        use std::os::unix::fs::PermissionsExt;

        let container = tempfile::tempdir().unwrap();
        let workspace = container.path().join("workspace");
        let tools = container.path().join("tools");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&tools).unwrap();
        let inside = workspace.join("server");
        let outside = tools.join("server");
        for path in [&inside, &outside] {
            fs::write(path, "#!/bin/sh\nexit 0\n").unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let workspace = workspace.canonicalize().unwrap();
        assert!(validate_executable(&inside.canonicalize().unwrap(), &workspace).is_err());
        let outside = outside.canonicalize().unwrap();
        let validated = validate_executable(&outside, &workspace).unwrap();
        let server = TrustedServer {
            program: validated.path,
            args: Arc::from([]),
            identity: validated.identity,
        };
        server.revalidate_before_spawn(&workspace).unwrap();
        let replacement = tools.join("replacement");
        fs::write(&replacement, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&replacement, fs::Permissions::from_mode(0o755)).unwrap();
        fs::rename(&replacement, &outside).unwrap();
        assert!(server.revalidate_before_spawn(&workspace).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn executable_symlink_is_pinned_to_its_authorized_canonical_target() {
        use std::os::unix::fs::PermissionsExt;

        let container = tempfile::tempdir().unwrap();
        let workspace = container.path().join("workspace");
        let tools = container.path().join("tools");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&tools).unwrap();
        let first = tools.join("first-server");
        let second = tools.join("second-server");
        for path in [&first, &second] {
            fs::write(path, "#!/bin/sh\nexit 0\n").unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let link = tools.join("language-server");
        std::os::unix::fs::symlink(&first, &link).unwrap();
        let workspace = workspace.canonicalize().unwrap();
        let validated = validate_executable(&link, &workspace).unwrap();
        let first = first.canonicalize().unwrap();
        assert_eq!(validated.path, first);
        let server = TrustedServer {
            program: validated.path,
            args: Arc::from([]),
            identity: validated.identity,
        };

        fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&second, &link).unwrap();

        assert_eq!(server.program(), first);
        server.revalidate_before_spawn(&workspace).unwrap();
    }

    #[test]
    fn file_uri_round_trip_preserves_spaces_and_unicode() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("空 格.rs");
        fs::write(&path, "fn main() {}\n").unwrap();
        let root = directory.path().canonicalize().unwrap();
        let path = path.canonicalize().unwrap();
        let uri = path_to_lsp_uri(&path).unwrap();
        assert_eq!(lsp_uri_to_safe_path(&uri, &root).unwrap(), path);
        let query: lsp_types::Uri = format!("{}?x=1", uri.as_str()).parse().unwrap();
        assert!(lsp_uri_to_safe_path(&query, &root).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn windows_lsp_targets_match_verbatim_workspace_prefixes() {
        let disk_root = Path::new(r"\\?\C:\Work\Repo");
        let disk_target = Path::new(r"c:\work\repo\src\main.rs");
        assert_eq!(
            normalize_lsp_target_path(disk_root, disk_target).unwrap(),
            disk_root.join(r"src\main.rs")
        );

        let unc_root = Path::new(r"\\?\UNC\Server\Share\Repo");
        let unc_target = Path::new(r"\\server\share\repo\src\main.rs");
        assert_eq!(
            normalize_lsp_target_path(unc_root, unc_target).unwrap(),
            unc_root.join(r"src\main.rs")
        );

        assert!(normalize_lsp_target_path(disk_root, Path::new(r"C:\Work\Other\main.rs")).is_err());
    }

    #[test]
    fn warning_cleanup_is_bounded_and_control_free() {
        let warning = clean_warning(&format!("\u{1b}[31m{}", "x".repeat(300)));
        assert!(warning.len() <= 240);
        assert!(!warning.contains('\u{1b}'));
    }

    #[test]
    fn source_range_is_end_exclusive() {
        let range = SourceRange {
            start: SourcePosition { line: 1, byte: 2 },
            end: SourcePosition { line: 1, byte: 5 },
        };
        assert!(range.contains(SourcePosition { line: 1, byte: 2 }));
        assert!(!range.contains(SourcePosition { line: 1, byte: 5 }));
        assert!(!range.is_empty());
    }
}
