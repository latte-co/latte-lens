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
/// buffer, not a file transfer; large selections are truncated at a UTF-8
/// boundary and the UI reports the truncation. The lower Windows ceiling
/// keeps the whole argv command line inside the CreateProcess limit.
pub const MAX_SEND_BYTES: usize = if cfg!(windows) { 8 * 1024 } else { 32 * 1024 };

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
    /// Selection snapshot taken when the picker opened; what gets delivered.
    pub payload: String,
    pub truncated: bool,
    /// Generation of the request currently represented by this state.
    pub generation: u64,
}

impl Default for SendToAgentState {
    fn default() -> Self {
        Self {
            phase: SendPhase::Closed,
            targets: Vec::new(),
            selected: 0,
            payload: String::new(),
            truncated: false,
            generation: 0,
        }
    }
}

impl SendToAgentState {
    pub const fn is_open(&self) -> bool {
        !matches!(self.phase, SendPhase::Closed)
    }

    /// Snapshot the payload and move into the discovering phase.
    pub fn begin_discover(&mut self, generation: u64, payload: String, truncated: bool) {
        self.phase = SendPhase::Discovering;
        self.targets.clear();
        self.selected = 0;
        self.payload = payload;
        self.truncated = truncated;
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
        self.payload.clear();
        self.truncated = false;
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

    pub fn begin_sending(&mut self, generation: u64) {
        self.phase = SendPhase::Sending;
        self.generation = generation;
    }

    /// Payload byte length for the picker subtitle / status report.
    pub fn payload_chars(&self) -> usize {
        self.payload.chars().count()
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
        state.begin_discover(7, "let x = 1;".to_owned(), false);
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
        state.begin_discover(1, String::new(), false);
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
}
