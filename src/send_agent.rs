//! Outbound "send selection to agent" channel.
//!
//! Unlike the inbound observation providers elsewhere in [`crate::agent`],
//! this module lets Lens hand the current content selection to a *running*
//! interactive agent session owned by a terminal workspace manager. The only
//! production backend is Herdr (`herdr agent list`, `herdr pane send-text`);
//! the [`AgentTargetProvider`] seam keeps other terminals addable without
//! touching the App state machine.
//!
//! Safety contract:
//! - payload text is delivered as a literal argv argument (never a shell);
//! - the backend never appends Enter — the draft stays unsent in the target
//!   composer and the human submits it manually;
//! - all process output is bounded and every call is fail-closed.

use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::Duration,
};

use anyhow::{Context, Result, anyhow};

/// Maximum payload delivered in one send. The Herdr pane input is a composer
/// buffer, not a file transfer: the whole staged message (anchor, annotation,
/// and selection together) must fit in 4 KiB. Larger selections keep their
/// head and tail and omit the middle at whole-line boundaries; the anchor
/// still records the full original range so the agent can open the file.
pub const MAX_SEND_BYTES: usize = 4096;

/// Maximum annotation length in UTF-8 bytes. Annotations are one-line
/// questions or instructions; a screenful is more than enough.
pub const MAX_ANNOTATION_BYTES: usize = 512;

/// Bytes reserved up front while budgeting a truncated payload (anchor suffix
/// and omitted-marker line). The final assembly is still verified against
/// [`MAX_SEND_BYTES`], so this only needs to be conservative.
const TRUNCATION_RESERVE: usize = 220;

/// Hard ceiling for any single backend CLI response.
const MAX_BACKEND_OUTPUT: usize = 256 * 1024;

/// Discover timeout. Listing local panes is a socket round-trip.
const DISCOVER_TIMEOUT: Duration = Duration::from_secs(2);

/// Delivery timeout.
const SEND_TIMEOUT: Duration = Duration::from_secs(3);

/// Agent lifecycle state as reported by the workspace manager. Blocked agents
/// are waiting at a permission/question dialog and must not receive drafts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentLifecycle {
    Idle,
    Working,
    Blocked,
    Done,
    Unknown,
}

impl AgentLifecycle {
    fn parse(raw: &str) -> Self {
        match raw {
            "idle" => Self::Idle,
            "working" => Self::Working,
            "blocked" => Self::Blocked,
            "done" => Self::Done,
            _ => Self::Unknown,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Blocked => "blocked",
            Self::Done => "done",
            Self::Unknown => "unknown",
        }
    }

    const fn selectable(self) -> bool {
        !matches!(self, Self::Blocked)
    }
}

/// One reachable interactive agent pane.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentTarget {
    pub agent: String,
    pub pane_id: String,
    pub title: String,
    pub cwd: PathBuf,
    pub status: AgentLifecycle,
    /// False for blocked sessions: rendered dimmed and not Enter-selectable.
    pub selectable: bool,
    /// cwd canonicalizes to the Lens workspace root; drives pinning order.
    pub same_workspace: bool,
}

/// Result of opening the picker.
#[derive(Clone, Debug)]
pub struct AgentDiscovery {
    pub targets: Vec<AgentTarget>,
}

/// Backend seam. Implementations must be safe to call from the runtime worker
/// thread and must never block indefinitely.
pub trait AgentTargetProvider: Send + Sync {
    /// Whether the environment exposes a usable backend. When false the
    /// send-to-agent entry points stay completely invisible.
    fn available(&self) -> bool;

    /// Discover all agent panes visible to this workspace manager.
    fn discover(&self, workspace_root: &Path) -> Result<AgentDiscovery>;

    /// Insert literal text into the pane's focused input without submitting.
    fn send_draft(&self, pane_id: &str, text: &str) -> Result<()>;

    /// Move workspace focus to the pane. A focus failure is not a delivery
    /// failure: the draft is already in the composer.
    fn focus_pane(&self, pane_id: &str) -> Result<()>;
}

// ---------------------------------------------------------------------------
// Pure logic (unit-tested with synthetic fixtures, no processes)
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
struct RawAgentList {
    result: RawAgentListResult,
}

#[derive(serde::Deserialize)]
struct RawAgentListResult {
    #[serde(default)]
    agents: Vec<RawAgent>,
}

#[derive(serde::Deserialize)]
struct RawAgent {
    agent: String,
    pane_id: String,
    #[serde(default)]
    agent_status: String,
    #[serde(default)]
    cwd: String,
    #[serde(default)]
    terminal_title_stripped: String,
}

/// Parse the `herdr agent list` JSON envelope into normalized, ordered targets.
///
/// `workspace_root` is the Lens repo root (already canonicalized by `App`).
/// Entries missing required identity fields are skipped; the pane Lens itself
/// runs in (`self_pane`) is removed; sessions whose cwd equals the workspace
/// are pinned first; blocked sessions remain visible but are flagged
/// unselectable.
pub fn parse_agent_list(
    json: &str,
    workspace_root: &Path,
    self_pane: Option<&str>,
) -> Result<AgentDiscovery> {
    let parsed: RawAgentList =
        serde_json::from_str(json).context("agent list returned invalid JSON")?;
    let workspace_root = canonicalized(workspace_root);
    let mut targets: Vec<AgentTarget> = parsed
        .result
        .agents
        .into_iter()
        .filter(|raw| !raw.agent.is_empty() && !raw.pane_id.is_empty())
        .filter(|raw| self_pane.is_none_or(|pane| pane != raw.pane_id))
        .map(|raw| {
            let cwd = PathBuf::from(raw.cwd);
            let status = AgentLifecycle::parse(raw.agent_status.trim());
            let same_workspace = canonicalized(&cwd) == workspace_root;
            AgentTarget {
                agent: raw.agent,
                pane_id: raw.pane_id,
                title: sanitize_title(&raw.terminal_title_stripped),
                cwd,
                status,
                selectable: status.selectable(),
                same_workspace,
            }
        })
        .collect();

    targets.sort_by(|a, b| {
        b.same_workspace
            .cmp(&a.same_workspace)
            .then_with(|| a.agent.cmp(&b.agent))
            .then_with(|| a.title.cmp(&b.title))
            .then_with(|| a.pane_id.cmp(&b.pane_id))
    });

    Ok(AgentDiscovery { targets })
}

/// Truncate at a UTF-8 char boundary so the composer never receives a split
/// grapheme lead byte. Returns the payload and whether truncation happened.
pub fn truncate_payload(text: &str, max_bytes: usize) -> (String, bool) {
    if text.len() <= max_bytes {
        return (text.to_owned(), false);
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_owned(), true)
}

// ---------------------------------------------------------------------------
// Payload wrapping (anchor + annotation + fenced selection)
// ---------------------------------------------------------------------------

/// How the staged message presents the selection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SendTemplate {
    /// The selection text alone, with an optional annotation above it.
    Plain,
    /// `path:start-end` header, an optional `▎` annotation, and a fenced code
    /// block. Only available when the selection has source coordinates.
    Anchor,
}

/// Source coordinates of the snapshot selection, captured when the picker
/// opens. Line numbers are 1-based and inclusive.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SelectionAnchor {
    /// Workspace-relative display path.
    pub path: String,
    /// First line of the range (new-file line for Diff hunks); 0 means the
    /// anchor carries a path only.
    pub start_line: usize,
    /// Last line of the range; equal to `start_line` for a single line.
    pub end_line: usize,
    /// Fence language hint derived from the file extension, or `"diff"` for
    /// a unified-diff hunk.
    pub language: Option<&'static str>,
    /// Whether truncated payloads prefix retained lines with a `line│`
    /// gutter. Source selections do (snippets must map back to real lines);
    /// diff hunks do not (their `+`/`-` prefixes and `@@` headers already
    /// carry position, and a number column would garble the patch).
    pub gutter: bool,
}

impl SelectionAnchor {
    /// `path:line` for a single line, `path:start-end` for a multi-line range,
    /// and bare `path` when no line numbers apply.
    fn header(&self) -> String {
        if self.start_line == 0 {
            self.path.clone()
        } else if self.start_line == self.end_line {
            format!("{}:{}", self.path, self.start_line)
        } else {
            format!("{}:{}-{}", self.path, self.start_line, self.end_line)
        }
    }
}

/// The final text to stage plus bookkeeping for the picker's status line.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuiltPayload {
    pub text: String,
    /// True when middle lines were omitted to fit [`MAX_SEND_BYTES`].
    pub truncated: bool,
    pub total_lines: usize,
    pub kept_lines: usize,
    pub omitted_lines: usize,
    pub omitted_bytes: usize,
}

impl BuiltPayload {
    fn full(text: String, total_lines: usize) -> Self {
        Self {
            text,
            truncated: false,
            total_lines,
            kept_lines: total_lines,
            omitted_lines: 0,
            omitted_bytes: 0,
        }
    }
}

/// Build the exact message staged into the agent composer. Pure and
/// budget-exact: the result never exceeds [`MAX_SEND_BYTES`] and never splits
/// a UTF-8 character. `Anchor` without an anchor falls back to `Plain`.
pub fn build_payload(
    selection: &str,
    anchor: Option<&SelectionAnchor>,
    annotation: &str,
    template: SendTemplate,
) -> BuiltPayload {
    let annotation = annotation.trim();
    let code = selection.strip_suffix('\n').unwrap_or(selection);
    match (template, anchor) {
        (SendTemplate::Anchor, Some(anchor)) => build_anchored(code, annotation, anchor),
        _ => build_plain(code, annotation),
    }
}

/// Annotation block: every line prefixed with `▎` so the human's note stays
/// visually separated from quoted code in the composer and the chat history.
fn annotation_block(annotation: &str) -> String {
    annotation
        .lines()
        .map(|line| format!("▎{line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn build_plain(code: &str, annotation: &str) -> BuiltPayload {
    let lines: Vec<&str> = code.split('\n').collect();
    let total = lines.len();
    let prefix = if annotation.is_empty() {
        String::new()
    } else {
        format!("{}\n\n", annotation_block(annotation))
    };

    let full = format!("{prefix}{code}");
    if full.len() <= MAX_SEND_BYTES {
        return BuiltPayload::full(full, total);
    }

    let budget = MAX_SEND_BYTES - prefix.len() - TRUNCATION_RESERVE;
    let split = split_head_tail(&lines, budget, 0);
    let marker = plain_marker(split.omitted, split.omitted_bytes);
    let text = format!(
        "{prefix}{}\n{}\n{}",
        split.head.join("\n"),
        marker,
        split.tail.join("\n")
    );
    let verified = verify_budget(text, &lines, &split, |head, tail, omitted, bytes| {
        format!(
            "{prefix}{}\n{}\n{}",
            head.join("\n"),
            plain_marker(omitted, bytes),
            tail.join("\n")
        )
    });
    BuiltPayload {
        total_lines: total,
        kept_lines: total - verified.omitted,
        omitted_lines: verified.omitted,
        omitted_bytes: verified.omitted_bytes,
        truncated: true,
        text: verified.text,
    }
}

fn build_anchored(code: &str, annotation: &str, anchor: &SelectionAnchor) -> BuiltPayload {
    let lines: Vec<&str> = code.split('\n').collect();
    let total = lines.len();
    let fence = code_fence(code);
    let fence_open = match anchor.language {
        Some(language) => format!("{fence}{language}"),
        None => fence.clone(),
    };
    let note = if annotation.is_empty() {
        String::new()
    } else {
        format!("{}\n\n", annotation_block(annotation))
    };

    let full = format!("{}\n{note}{fence_open}\n{code}\n{fence}", anchor.header());
    if full.len() <= MAX_SEND_BYTES {
        return BuiltPayload::full(full, total);
    }

    // Truncated: the anchor gains a summary suffix. Source selections also
    // prefix retained lines with a numbered gutter so surviving snippets map
    // to real file lines; diff hunks stay verbatim (their +/- prefixes and
    // @@ headers already carry position, and a number column garbles them).
    let gutter_width = if anchor.gutter {
        anchor.end_line.checked_ilog10().unwrap_or(0) as usize + 1
    } else {
        0
    };
    let gutter_bytes = if anchor.gutter {
        gutter_width + "│".len()
    } else {
        0
    };
    let fixed = anchor.header().len()
        + anchor_suffix(total, total).len()
        + note.len()
        + fence_open.len()
        + 1
        + fence.len()
        + TRUNCATION_RESERVE;
    let budget = MAX_SEND_BYTES.saturating_sub(fixed);
    let split = split_head_tail(&lines, budget, gutter_bytes);

    let render_line = |line_no: usize, line: &str| {
        if anchor.gutter {
            format!("{line_no:>gutter_width$}│{line}\n")
        } else {
            format!("{line}\n")
        }
    };
    let assemble = |head_count: usize, tail_count: usize| -> (String, usize, usize) {
        let omitted = total - head_count - tail_count;
        let kept_bytes: usize = lines[..head_count]
            .iter()
            .chain(lines[total - tail_count..].iter())
            .map(|line| line.len() + 1)
            .sum();
        let total_bytes: usize = lines.iter().map(|line| line.len() + 1).sum();
        let omitted_bytes = total_bytes.saturating_sub(kept_bytes);
        let mut text = format!(
            "{}{}\n{note}{fence_open}\n",
            anchor.header(),
            anchor_suffix(total, omitted)
        );
        for (offset, line) in lines[..head_count].iter().enumerate() {
            text.push_str(&render_line(anchor.start_line + offset, line));
        }
        if omitted > 0 {
            text.push_str(&code_marker(omitted, omitted_bytes, anchor));
            text.push('\n');
        }
        for (index, line) in lines[total - tail_count..].iter().enumerate() {
            let line_no = anchor.start_line + total - tail_count + index;
            text.push_str(&render_line(line_no, line));
        }
        text.push_str(&fence);
        (text, omitted, omitted_bytes)
    };

    // Exact shrink: the gutter width is constant, so per-line accounting is
    // exact; drop tail lines (then head lines) until the budget is met.
    let (mut head_count, mut tail_count) = (split.head.len(), split.tail.len());
    let (mut text, mut omitted, mut omitted_bytes) = assemble(head_count, tail_count);
    while text.len() > MAX_SEND_BYTES && head_count + tail_count > 0 {
        if tail_count > 0 {
            tail_count -= 1;
        } else {
            head_count -= 1;
        }
        (text, omitted, omitted_bytes) = assemble(head_count, tail_count);
    }

    BuiltPayload {
        text,
        truncated: true,
        total_lines: total,
        kept_lines: total - omitted,
        omitted_lines: omitted,
        omitted_bytes,
    }
}

fn anchor_suffix(total: usize, omitted: usize) -> String {
    format!(" ({total} lines selected, {omitted} omitted)")
}

fn plain_marker(omitted: usize, omitted_bytes: usize) -> String {
    format!(
        "⋮ ── omitted {omitted} lines ({}) ──",
        format_bytes(omitted_bytes)
    )
}

fn code_marker(omitted: usize, omitted_bytes: usize, anchor: &SelectionAnchor) -> String {
    format!(
        "⋮ ── omitted {omitted} lines ({}) see {} ──",
        format_bytes(omitted_bytes),
        anchor.header()
    )
}

/// Head/tail line split. Each retained line costs its text bytes, one
/// newline, and (in anchored mode) the gutter width; the head gets about 55%
/// of the budget and the tail the rest, always on whole-line boundaries.
struct LineSplit<'a> {
    head: Vec<&'a str>,
    tail: Vec<&'a str>,
    omitted: usize,
    omitted_bytes: usize,
}

fn split_head_tail<'a>(lines: &[&'a str], budget: usize, gutter_bytes: usize) -> LineSplit<'a> {
    let cost = |line: &&str| line.len() + 1 + gutter_bytes;
    let total_cost: usize = lines.iter().map(cost).sum();
    if total_cost <= budget {
        return LineSplit {
            head: lines.to_vec(),
            tail: Vec::new(),
            omitted: 0,
            omitted_bytes: 0,
        };
    }
    let head_budget = budget * 55 / 100;
    let tail_budget = budget.saturating_sub(head_budget);
    let mut used = 0usize;
    let mut head_count = 0usize;
    for line in lines {
        let line_cost = cost(line);
        if used + line_cost > head_budget || head_count + 1 >= lines.len() {
            break;
        }
        used += line_cost;
        head_count += 1;
    }
    used = 0;
    let mut tail_count = 0usize;
    for line in lines.iter().rev() {
        let line_cost = cost(line);
        if used + line_cost > tail_budget || head_count + tail_count + 1 >= lines.len() {
            break;
        }
        used += line_cost;
        tail_count += 1;
    }
    if head_count == 0 && !lines.is_empty() {
        head_count = 1;
    }
    if tail_count == 0 && lines.len() > head_count {
        tail_count = 1;
    }
    let omitted = lines.len() - head_count - tail_count;
    let kept_bytes: usize = lines[..head_count]
        .iter()
        .chain(lines[lines.len() - tail_count..].iter())
        .map(|line| line.len() + 1)
        .sum();
    let total_bytes: usize = lines.iter().map(|line| line.len() + 1).sum();
    LineSplit {
        head: lines[..head_count].to_vec(),
        tail: lines[lines.len() - tail_count..].to_vec(),
        omitted,
        omitted_bytes: total_bytes.saturating_sub(kept_bytes),
    }
}

/// Final safety net for the plain path: keep dropping retained lines until
/// the assembled message fits. The anchored path performs exact accounting
/// (gutter width is fixed), so this is only reached if an estimate was off.
fn verify_budget<'a>(
    mut text: String,
    lines: &[&'a str],
    split: &LineSplit<'a>,
    reassemble: impl Fn(&[&'a str], &[&'a str], usize, usize) -> String,
) -> LineSplitText {
    let mut head_count = split.head.len();
    let mut tail_count = split.tail.len();
    loop {
        if text.len() <= MAX_SEND_BYTES || head_count + tail_count == 0 {
            break;
        }
        if tail_count > 0 {
            tail_count -= 1;
        } else {
            head_count -= 1;
        }
        let omitted = lines.len() - head_count - tail_count;
        let kept_bytes: usize = lines[..head_count]
            .iter()
            .chain(lines[lines.len() - tail_count..].iter())
            .map(|line| line.len() + 1)
            .sum();
        let total_bytes: usize = lines.iter().map(|line| line.len() + 1).sum();
        text = reassemble(
            &lines[..head_count],
            &lines[lines.len() - tail_count..],
            omitted,
            total_bytes.saturating_sub(kept_bytes),
        );
    }
    let omitted = lines.len() - head_count - tail_count;
    let kept_bytes: usize = lines[..head_count]
        .iter()
        .chain(lines[lines.len() - tail_count..].iter())
        .map(|line| line.len() + 1)
        .sum();
    let total_bytes: usize = lines.iter().map(|line| line.len() + 1).sum();
    LineSplitText {
        text,
        omitted,
        omitted_bytes: total_bytes.saturating_sub(kept_bytes),
    }
}

struct LineSplitText {
    text: String,
    omitted: usize,
    omitted_bytes: usize,
}

fn format_bytes(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else {
        format!("{}.{} KB", bytes / 1024, (bytes % 1024) * 10 / 1024)
    }
}

/// Fence delimiter long enough that the selection's own backtick runs cannot
/// close the block early (minimum three backticks).
fn code_fence(code: &str) -> String {
    let mut longest = 0usize;
    let mut current = 0usize;
    for ch in code.chars() {
        if ch == '`' {
            current += 1;
            longest = longest.max(current);
        } else {
            current = 0;
        }
    }
    "`".repeat(longest + 1).max("```".to_owned())
}

/// Best-effort fenced-code language hint from a file extension.
pub fn fence_language(extension: &str) -> Option<&'static str> {
    Some(match extension {
        "rs" => "rust",
        "py" | "pyi" => "python",
        "js" | "mjs" | "cjs" | "jsx" => "javascript",
        "ts" | "mts" | "cts" | "tsx" => "typescript",
        "go" => "go",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" => "cpp",
        "java" => "java",
        "kt" | "kts" => "kotlin",
        "rb" => "ruby",
        "php" => "php",
        "swift" => "swift",
        "scala" => "scala",
        "sh" | "bash" | "zsh" => "bash",
        "ps1" | "psm1" => "powershell",
        "lua" => "lua",
        "pl" => "perl",
        "md" | "markdown" => "markdown",
        "diff" | "patch" => "diff",
        "json" | "jsonc" => "json",
        "yaml" | "yml" => "yaml",
        "toml" => "toml",
        "html" | "htm" | "xml" => "xml",
        "css" | "scss" | "sass" => "css",
        "sql" => "sql",
        "proto" => "proto",
        _ => return None,
    })
}

fn sanitize_title(text: &str) -> String {
    text.chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect()
}

// ---------------------------------------------------------------------------
// Picker view model (pure reducer; the App owns generations and runtime I/O)
// ---------------------------------------------------------------------------

/// Picker lifecycle. The App keeps one of these even while closed so the
/// runtime completions always have a state to land on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SendPhase {
    Closed,
    /// A discovery request is in flight; the UI shows a waiting row.
    Discovering,
    /// Targets arrived and the menu is interactive.
    Picking,
    /// The chosen draft is being delivered.
    Sending,
}

#[derive(Clone, Debug)]
pub struct SendToAgentState {
    pub phase: SendPhase,
    pub targets: Vec<AgentTarget>,
    pub selected: usize,
    /// Raw selection snapshot taken when the picker opened; wrapping and
    /// truncation happen in [`SendToAgentState::built_payload`].
    pub selection: String,
    /// Source coordinates for the anchor template; None for Diff views,
    /// search previews, and other coordinate-free selections.
    pub anchor: Option<SelectionAnchor>,
    /// Active template; forced to Plain when there is no anchor.
    pub template: SendTemplate,
    /// One-line annotation drafted directly in the picker, `▎`-prefixed at
    /// build time.
    pub annotation: String,
    /// Annotation caret as a UTF-8 byte offset.
    pub annotation_caret: usize,
    /// Generation of the request currently represented by this state.
    pub generation: u64,
}

impl Default for SendToAgentState {
    fn default() -> Self {
        Self {
            phase: SendPhase::Closed,
            targets: Vec::new(),
            selected: 0,
            selection: String::new(),
            anchor: None,
            template: SendTemplate::Plain,
            annotation: String::new(),
            annotation_caret: 0,
            generation: 0,
        }
    }
}

impl SendToAgentState {
    pub const fn is_open(&self) -> bool {
        !matches!(self.phase, SendPhase::Closed)
    }

    /// Snapshot the selection and move into the discovering phase. The picker
    /// is interactive for annotation drafting before discovery returns.
    pub fn begin_discover(
        &mut self,
        generation: u64,
        selection: String,
        anchor: Option<SelectionAnchor>,
    ) {
        self.phase = SendPhase::Discovering;
        self.targets.clear();
        self.selected = 0;
        self.selection = selection;
        self.template = if anchor.is_some() {
            SendTemplate::Anchor
        } else {
            SendTemplate::Plain
        };
        self.anchor = anchor;
        self.annotation.clear();
        self.annotation_caret = 0;
        self.generation = generation;
    }

    /// Apply a discovery result belonging to the in-flight generation.
    /// Stale results (generation mismatch) are ignored. Returns false when
    /// the result was discarded.
    pub fn apply_discovery(&mut self, generation: u64, targets: Vec<AgentTarget>) -> bool {
        if generation != self.generation || !matches!(self.phase, SendPhase::Discovering) {
            return false;
        }
        self.targets = targets;
        self.selected = self
            .targets
            .iter()
            .position(|target| target.selectable)
            .unwrap_or(0);
        self.phase = SendPhase::Picking;
        true
    }

    /// A failed/empty discovery returns the picker to closed so the status
    /// bar can own the message. Stale failures are ignored.
    pub fn fail_discovery(&mut self, generation: u64) -> bool {
        if generation != self.generation || !matches!(self.phase, SendPhase::Discovering) {
            return false;
        }
        self.close();
        true
    }

    pub fn close(&mut self) {
        self.phase = SendPhase::Closed;
        self.targets.clear();
        self.selected = 0;
        self.selection.clear();
        self.anchor = None;
        self.template = SendTemplate::Plain;
        self.annotation.clear();
        self.annotation_caret = 0;
        self.generation = 0;
    }

    pub fn move_selection(&mut self, delta: isize) {
        if self.targets.is_empty() {
            return;
        }
        let len = self.targets.len() as isize;
        let current = self.selected as isize;
        let mut next = (current + delta).rem_euclid(len) as usize;
        // Skip blocked rows when navigating from the keyboard.
        for _ in 0..self.targets.len() {
            if self.targets[next].selectable {
                self.selected = next;
                return;
            }
            next = (next as isize + delta.signum()).rem_euclid(len) as usize;
        }
    }

    pub fn select_index(&mut self, index: usize) -> bool {
        if self
            .targets
            .get(index)
            .is_some_and(|target| target.selectable)
        {
            self.selected = index;
            true
        } else {
            false
        }
    }

    pub fn selected_target(&self) -> Option<&AgentTarget> {
        self.targets
            .get(self.selected)
            .filter(|target| target.selectable)
    }

    /// Whether the anchor template can be selected (the selection has source
    /// coordinates).
    pub const fn anchor_available(&self) -> bool {
        self.anchor.is_some()
    }

    /// Cycle between the anchored and plain templates. No-op without an
    /// anchor. Returns the resulting template.
    pub fn cycle_template(&mut self) -> SendTemplate {
        if self.anchor.is_some() {
            self.template = match self.template {
                SendTemplate::Anchor => SendTemplate::Plain,
                SendTemplate::Plain => SendTemplate::Anchor,
            };
        }
        self.template
    }

    /// Insert a printable character at the annotation caret. Control
    /// characters are rejected and the annotation is hard-capped at
    /// [`MAX_ANNOTATION_BYTES`]; returns false when nothing was inserted.
    pub fn insert_annotation_char(&mut self, ch: char) -> bool {
        if ch.is_control() {
            return false;
        }
        if self.annotation.len() + ch.len_utf8() > MAX_ANNOTATION_BYTES {
            return false;
        }
        self.annotation.insert(self.annotation_caret, ch);
        self.annotation_caret += ch.len_utf8();
        true
    }

    /// Delete the character before the caret.
    pub fn annotation_backspace(&mut self) {
        if self.annotation_caret == 0 {
            return;
        }
        let mut prev = self.annotation_caret - 1;
        while !self.annotation.is_char_boundary(prev) {
            prev -= 1;
        }
        self.annotation
            .replace_range(prev..self.annotation_caret, "");
        self.annotation_caret = prev;
    }

    pub fn annotation_move_caret(&mut self, delta: isize) {
        let len = self.annotation.chars().count() as isize;
        let current = self.annotation[..self.annotation_caret].chars().count() as isize;
        let next = (current + delta).clamp(0, len) as usize;
        self.annotation_caret = self
            .annotation
            .char_indices()
            .nth(next)
            .map_or(self.annotation.len(), |(index, _)| index);
    }

    pub fn annotation_home(&mut self) {
        self.annotation_caret = 0;
    }

    pub fn annotation_end(&mut self) {
        self.annotation_caret = self.annotation.len();
    }

    /// Assemble the exact message that will be staged, applying the template,
    /// annotation, and the 4 KiB head/tail budget.
    pub fn built_payload(&self) -> BuiltPayload {
        build_payload(
            &self.selection,
            self.anchor.as_ref(),
            &self.annotation,
            self.template,
        )
    }

    pub fn begin_sending(&mut self, generation: u64) {
        self.phase = SendPhase::Sending;
        self.generation = generation;
    }

    /// Payload byte length for the picker subtitle / status report.
    pub fn payload_bytes(&self) -> usize {
        self.built_payload().text.len()
    }
}

fn canonicalized(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

// ---------------------------------------------------------------------------
// Herdr production backend
// ---------------------------------------------------------------------------

/// Production backend driving the Herdr CLI over its local socket.
pub struct HerdrProvider {
    /// Resolved argv[0]: `HERDR_BIN_PATH` override or `herdr` from PATH.
    binary: String,
}

impl HerdrProvider {
    pub fn from_environment() -> Self {
        let binary = std::env::var("HERDR_BIN_PATH")
            .ok()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "herdr".to_owned());
        Self { binary }
    }

    fn run(&self, args: &[&str], timeout: Duration) -> Result<String> {
        let mut command = Command::new(&self.binary);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // ETXTBSY is a transient kernel state: the binary file is briefly
        // open for writing (e.g. an in-place upgrade) when we exec it. Retry
        // a few bounded times instead of failing a local socket call for a
        // window measured in milliseconds.
        let mut child = None;
        let mut last_error = None;
        for attempt in 0..3_u32 {
            match command.spawn() {
                Ok(spawned) => {
                    child = Some(spawned);
                    break;
                }
                Err(error) if is_text_busy(&error) => {
                    last_error = Some(error);
                    thread::sleep(Duration::from_millis(10u64.saturating_mul(1 << attempt)));
                }
                Err(error) => {
                    return Err(error).with_context(|| format!("failed to launch {}", self.binary));
                }
            }
        }
        let child = child.ok_or_else(|| {
            anyhow!(
                "failed to launch {} after retries: {}",
                self.binary,
                last_error
                    .as_ref()
                    .map(std::io::Error::to_string)
                    .unwrap_or_else(|| "executable busy".to_owned())
            )
        })?;
        let pid = child.id();

        let (tx, rx) = mpsc::channel();
        // The child moves into a detached waiter; on timeout we kill it by
        // pid. A local socket call should never reach that branch.
        thread::spawn(move || {
            let _ = tx.send(child.wait_with_output());
        });

        let output = match rx.recv_timeout(timeout) {
            Ok(Ok(output)) => output,
            Ok(Err(error)) => return Err(error).context("agent backend wait failed"),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                kill_process(pid);
                return Err(anyhow!("agent backend timed out"));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(anyhow!("agent backend worker disappeared"));
            }
        };

        if !output.status.success() {
            let detail = String::from_utf8_lossy(&output.stderr);
            let detail: String = detail.chars().take(400).collect();
            return Err(anyhow!(
                "{} exited with status {}{}",
                self.binary,
                output.status,
                if detail.trim().is_empty() {
                    String::new()
                } else {
                    format!(": {}", detail.trim())
                }
            ));
        }

        let mut stdout = output.stdout;
        if stdout.len() > MAX_BACKEND_OUTPUT {
            stdout.truncate(MAX_BACKEND_OUTPUT);
        }
        String::from_utf8(stdout).map_err(|_| anyhow!("agent backend returned non-UTF-8 output"))
    }
}

impl AgentTargetProvider for HerdrProvider {
    fn available(&self) -> bool {
        // The Herdr client exports HERDR_ENV=1 to every pane it owns. Outside
        // Herdr the feature stays hidden even if a `herdr` binary is on PATH.
        std::env::var("HERDR_ENV").is_ok_and(|value| value == "1")
    }

    fn discover(&self, workspace_root: &Path) -> Result<AgentDiscovery> {
        let json = self.run(&["agent", "list"], DISCOVER_TIMEOUT)?;
        let self_pane = std::env::var("HERDR_PANE_ID").ok();
        parse_agent_list(&json, workspace_root, self_pane.as_deref())
    }

    fn send_draft(&self, pane_id: &str, text: &str) -> Result<()> {
        // argv, not stdin/shell: `herdr pane send-text <PANE_ID> <TEXT>`.
        // Herdr honors the pane's bracketed-paste mode and never adds Enter.
        self.run(&["pane", "send-text", pane_id, text], SEND_TIMEOUT)?;
        Ok(())
    }

    fn focus_pane(&self, pane_id: &str) -> Result<()> {
        self.run(&["agent", "focus", pane_id], SEND_TIMEOUT)?;
        Ok(())
    }
}

/// Whether a spawn failed with ETXTBSY (the executable is briefly open for
/// writing). POSIX reports raw error 26; Windows has no equivalent state.
#[cfg(unix)]
fn is_text_busy(error: &std::io::Error) -> bool {
    error.raw_os_error() == Some(libc::ETXTBSY)
}

#[cfg(not(unix))]
fn is_text_busy(_error: &std::io::Error) -> bool {
    false
}

#[cfg(unix)]
fn kill_process(pid: u32) {
    // Best-effort SIGKILL so a wedged backend cannot outlive the timeout.
    let _ = unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
}

#[cfg(windows)]
fn kill_process(pid: u32) {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};

    // Best-effort terminate; the detached waiter then reaps the exit.
    unsafe {
        let handle: HANDLE = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if !handle.is_null() {
            let _ = TerminateProcess(handle, 1);
            let _ = CloseHandle(handle);
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"{"id":"cli:agent:list","result":{"agents":[
      {"agent":"claude","agent_session":{"agent":"claude","kind":"id","source":"herdr:claude","value":"a"},"agent_status":"working","cwd":"/workspace/repo","focused":false,"pane_id":"w1:p2","terminal_title":"◑ feat","terminal_title_stripped":"feat work","workspace_id":"w1"},
      {"agent":"codex","agent_status":"idle","cwd":"/elsewhere","pane_id":"w1:p3","terminal_title_stripped":"other"},
      {"agent":"claude","agent_status":"blocked","cwd":"/workspace/repo","pane_id":"w1:p4","terminal_title_stripped":"needs approval"},
      {"agent":"","agent_status":"idle","cwd":"/workspace/repo","pane_id":"w1:p9","terminal_title_stripped":"missing agent"},
      {"agent":"opencode","agent_status":"weird","cwd":"/workspace/repo","pane_id":"w1:p5","terminal_title_stripped":"unknown state"}
    ]},"type":"agent_list"}"#;

    #[test]
    fn parses_filters_orders_and_flags_targets() {
        let discovery =
            parse_agent_list(FIXTURE, Path::new("/workspace/repo"), Some("w1:p1")).unwrap();
        let panes: Vec<&str> = discovery
            .targets
            .iter()
            .map(|target| target.pane_id.as_str())
            .collect();
        // empty-agent entry dropped. Ordering: same-workspace first, then
        // agent name, then title. Within same-workspace claude entries the
        // "feat work" title sorts before "needs approval".
        assert_eq!(panes, ["w1:p2", "w1:p4", "w1:p5", "w1:p3"]);
        let blocked = discovery
            .targets
            .iter()
            .find(|t| t.pane_id == "w1:p4")
            .unwrap();
        assert!(!blocked.selectable);
        assert_eq!(blocked.status, AgentLifecycle::Blocked);
        assert!(blocked.same_workspace);
        let unknown = discovery
            .targets
            .iter()
            .find(|t| t.pane_id == "w1:p5")
            .unwrap();
        assert_eq!(unknown.status, AgentLifecycle::Unknown);
        assert!(unknown.selectable);
        let other = discovery
            .targets
            .iter()
            .find(|t| t.pane_id == "w1:p3")
            .unwrap();
        assert!(!other.same_workspace);
        assert!(discovery.targets.iter().any(|t| t.selectable));
    }

    #[test]
    fn self_pane_is_excluded() {
        let discovery =
            parse_agent_list(FIXTURE, Path::new("/workspace/repo"), Some("w1:p2")).unwrap();
        assert!(
            discovery
                .targets
                .iter()
                .all(|target| target.pane_id != "w1:p2")
        );
    }

    #[test]
    fn invalid_json_is_an_error_not_an_empty_menu() {
        assert!(parse_agent_list("{not json", Path::new("/tmp"), None).is_err());
    }

    #[test]
    fn truncation_keeps_utf8_boundaries() {
        let sample = "é".repeat(100); // 2 bytes each
        let (cut, truncated) = truncate_payload(&sample, 10);
        assert!(truncated);
        assert_eq!(cut.len(), 10);
        assert!(cut.chars().count() == 5);
        let (full, truncated) = truncate_payload("short", 100);
        assert_eq!(full, "short");
        assert!(!truncated);
    }

    #[test]
    fn picker_reducer_tracks_selection_and_generations() {
        let root = Path::new("/workspace/repo");
        let discovery = parse_agent_list(FIXTURE, root, None).unwrap();
        let mut state = SendToAgentState::default();
        state.begin_discover(7, "let x = 1;".to_owned(), None);
        assert_eq!(state.phase, SendPhase::Discovering);

        // Stale generation is discarded.
        assert!(!state.apply_discovery(6, discovery.targets.clone()));
        assert_eq!(state.phase, SendPhase::Discovering);

        assert!(state.apply_discovery(7, discovery.targets.clone()));
        assert_eq!(state.phase, SendPhase::Picking);
        // First same-workspace selectable row is w1:p2 (the blocked w1:p4
        // must not become the initial selection).
        assert_eq!(state.selected_target().unwrap().pane_id, "w1:p2");

        // Keyboard navigation wraps but never lands on the blocked row.
        state.move_selection(1); // p2 -> p5 (p4 blocked is skipped)
        assert_eq!(state.selected_target().unwrap().pane_id, "w1:p5");
        state.move_selection(-1);
        assert_eq!(state.selected_target().unwrap().pane_id, "w1:p2");

        // Direct blocked-row selection is rejected.
        let blocked_index = discovery
            .targets
            .iter()
            .position(|t| t.pane_id == "w1:p4")
            .unwrap();
        assert!(!state.select_index(blocked_index));

        state.begin_sending(8);
        assert_eq!(state.phase, SendPhase::Sending);
        state.close();
        assert!(!state.is_open());
    }

    #[test]
    fn picker_stale_failure_after_close_is_ignored() {
        let mut state = SendToAgentState::default();
        state.begin_discover(1, String::new(), None);
        state.close();
        assert!(!state.fail_discovery(1));
    }

    #[test]
    fn control_characters_are_stripped_from_titles() {
        // JSON escapes decode to ESC / BEL; both must become spaces.
        let json = "{\"result\":{\"agents\":[\
          {\"agent\":\"claude\",\"agent_status\":\"idle\",\"cwd\":\"/w\",\"pane_id\":\"p:1\",\"terminal_title_stripped\":\"a\\u001b]52;x\\u0007b\"}\
        ]}}";
        let discovery = parse_agent_list(json, Path::new("/w"), None).unwrap();
        assert_eq!(discovery.targets[0].title, "a ]52;x b");
    }

    fn anchor(path: &str, start: usize, end: usize) -> SelectionAnchor {
        let language = Path::new(path)
            .extension()
            .and_then(|ext| ext.to_str())
            .and_then(fence_language);
        SelectionAnchor {
            path: path.to_owned(),
            start_line: start,
            end_line: end,
            language,
            gutter: true,
        }
    }

    #[test]
    fn plain_template_without_annotation_is_the_raw_selection() {
        let built = build_payload("lpha ", None, "", SendTemplate::Plain);
        assert_eq!(built.text, "lpha ");
        assert!(!built.truncated);
    }

    #[test]
    fn anchor_template_wraps_single_line_with_path_and_line() {
        let built = build_payload(
            "let x = 1;",
            Some(&anchor("src/app.rs", 10, 10)),
            "",
            SendTemplate::Anchor,
        );
        assert_eq!(built.text, "src/app.rs:10\n```rust\nlet x = 1;\n```");
    }

    #[test]
    fn anchor_header_uses_a_range_for_multiple_lines() {
        let built = build_payload(
            "a\nb",
            Some(&anchor("x.go", 10, 11)),
            "",
            SendTemplate::Anchor,
        );
        assert!(built.text.starts_with("x.go:10-11\n```go\n"));
    }

    #[test]
    fn annotation_sits_between_anchor_and_code_with_bar_prefix() {
        let built = build_payload(
            "let x = 1;",
            Some(&anchor("src/app.rs", 10, 10)),
            "why no buffer?",
            SendTemplate::Anchor,
        );
        assert_eq!(
            built.text,
            "src/app.rs:10\n▎why no buffer?\n\n```rust\nlet x = 1;\n```"
        );
    }

    #[test]
    fn plain_annotation_precedes_the_selection() {
        let built = build_payload(
            "@@ -1 +1 @@\n-x\n+y",
            None,
            "reproduces on my machine?",
            SendTemplate::Plain,
        );
        assert_eq!(
            built.text,
            "▎reproduces on my machine?\n\n@@ -1 +1 @@\n-x\n+y"
        );
    }

    #[test]
    fn diff_anchor_uses_diff_fence_and_keeps_patch_prefixes_without_gutter() {
        let anchor = SelectionAnchor {
            path: "src/app.rs".to_owned(),
            start_line: 42,
            end_line: 42,
            language: Some("diff"),
            gutter: false,
        };
        let built = build_payload(
            "-old\n+new",
            Some(&anchor),
            "did you intend this?",
            SendTemplate::Anchor,
        );
        assert_eq!(
            built.text,
            "src/app.rs:42\n▎did you intend this?\n\n```diff\n-old\n+new\n```"
        );
    }

    #[test]
    fn path_only_anchor_omits_line_numbers() {
        let anchor = SelectionAnchor {
            path: "src/app.rs".to_owned(),
            start_line: 0,
            end_line: 0,
            language: Some("diff"),
            gutter: false,
        };
        let built = build_payload("-gone", Some(&anchor), "", SendTemplate::Anchor);
        assert_eq!(built.text, "src/app.rs\n```diff\n-gone\n```");
    }

    #[test]
    fn truncated_diff_anchor_keeps_verbatim_lines_without_number_gutter() {
        let lines: Vec<String> = (0..200)
            .map(|i| format!("+line {i:03} {}", "x".repeat(34)))
            .collect();
        let code = lines.join("\n");
        let anchor = SelectionAnchor {
            path: "big.rs".to_owned(),
            start_line: 100,
            end_line: 299,
            language: Some("diff"),
            gutter: false,
        };
        let built = build_payload(&code, Some(&anchor), "", SendTemplate::Anchor);
        assert!(built.truncated);
        assert!(built.text.len() <= MAX_SEND_BYTES);
        assert!(
            built.text.contains("\n+line 000"),
            "diff lines must stay verbatim: {}",
            built.text.lines().nth(4).unwrap_or("")
        );
        assert!(
            !built.text.contains('│'),
            "no numbered gutter may be injected into a patch"
        );
        assert!(built.text.contains("```diff"));
    }

    #[test]
    fn unknown_extension_opens_a_bare_fence() {
        let built = build_payload(
            "native.rule()",
            Some(&anchor("scripts/deploy.bzl", 7, 7)),
            "",
            SendTemplate::Anchor,
        );
        assert_eq!(built.text, "scripts/deploy.bzl:7\n```\nnative.rule()\n```");
    }

    #[test]
    fn embedded_backtick_runs_upgrade_the_fence() {
        let code = "let s = \"```\";";
        let built = build_payload(code, Some(&anchor("a.rs", 1, 1)), "", SendTemplate::Anchor);
        assert!(
            built.text.contains("````rust\n"),
            "fence must outrun the embedded run: {}",
            built.text
        );
        assert!(built.text.ends_with("````"));
    }

    #[test]
    fn large_selection_keeps_head_and_tail_with_gutter_and_marker() {
        let lines: Vec<String> = (0..200)
            .map(|i| format!("line {i:03} {}", "x".repeat(34)))
            .collect();
        let code = lines.join("\n");
        let built = build_payload(
            &code,
            Some(&anchor("big.rs", 100, 299)),
            "explain shape",
            SendTemplate::Anchor,
        );
        assert!(built.text.len() <= MAX_SEND_BYTES, "{}", built.text.len());
        assert!(built.truncated);
        assert_eq!(built.total_lines, 200);
        assert!(built.omitted_lines > 0);
        assert_eq!(built.kept_lines, 200 - built.omitted_lines);
        assert!(
            built
                .text
                .starts_with("big.rs:100-299 (200 lines selected, ")
        );
        assert!(built.text.contains("▎explain shape\n\n"));
        // First retained line carries the gutter; last retained line maps to
        // the real final source line.
        assert!(built.text.contains("\n100│line 000"));
        assert!(built.text.contains("\n299│line 199 "));
        assert!(built.text.contains("omitted"));
        assert!(built.text.ends_with("```"));
    }

    #[test]
    fn truncated_plain_payload_still_carries_annotation_and_fits() {
        let lines: Vec<String> = (0..400)
            .map(|i| format!("payload line {i} data data data"))
            .collect();
        let code = lines.join("\n");
        let built = build_payload(&code, None, "see hunk", SendTemplate::Plain);
        assert!(built.text.len() <= MAX_SEND_BYTES);
        assert!(built.truncated);
        assert!(built.text.starts_with("▎see hunk\n\n"));
        assert!(built.text.contains("omitted"));
    }

    #[test]
    fn annotation_input_respects_the_byte_cap_and_controls() {
        let mut state = SendToAgentState::default();
        state.begin_discover(1, "x".to_owned(), None);
        assert!(state.insert_annotation_char('好'));
        assert!(!state.insert_annotation_char('\n'));
        assert!(!state.insert_annotation_char('\u{7f}'));
        let max = "é".repeat(MAX_ANNOTATION_BYTES / 2); // 2 bytes each
        state.annotation = max.clone();
        state.annotation_caret = max.len();
        assert!(!state.insert_annotation_char('x'), "cap is hard");
        state.annotation_backspace();
        assert_eq!(state.annotation.chars().count(), max.chars().count() - 1);
    }

    #[test]
    fn template_cycle_is_gated_on_anchor() {
        let mut state = SendToAgentState::default();
        state.begin_discover(1, "x".to_owned(), Some(anchor("a.rs", 1, 1)));
        assert_eq!(state.template, SendTemplate::Anchor);
        assert_eq!(state.cycle_template(), SendTemplate::Plain);
        assert_eq!(state.cycle_template(), SendTemplate::Anchor);

        state.close();
        state.begin_discover(1, "x".to_owned(), None);
        assert_eq!(state.template, SendTemplate::Plain);
        assert_eq!(state.cycle_template(), SendTemplate::Plain);
    }

    #[test]
    fn annotation_caret_moves_by_char_not_byte() {
        let mut state = SendToAgentState::default();
        state.begin_discover(1, "x".to_owned(), None);
        for ch in "好ab".chars() {
            state.insert_annotation_char(ch);
        }
        assert_eq!(state.annotation, "好ab");
        state.annotation_move_caret(-1);
        assert_eq!(state.annotation_caret, "好a".len());
        state.annotation_home();
        assert_eq!(state.annotation_caret, 0);
        state.annotation_end();
        assert_eq!(state.annotation_caret, state.annotation.len());
    }
}
