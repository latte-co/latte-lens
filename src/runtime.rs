use std::{
    any::Any,
    collections::HashSet,
    panic::{AssertUnwindSafe, catch_unwind},
    path::PathBuf,
    sync::{Arc, Condvar, Mutex, MutexGuard},
    thread::{self, JoinHandle},
};

use anyhow::{Result, anyhow};

use crate::{
    content_safety::{ensure_beneath, path_exists_without_following},
    git::StatusMap,
    preview::{PreviewRegistry, PreviewRequest, PreviewResolution},
    repo_graph::{RepoChange, RepoGraph},
    tree::{self, ScanResult},
};

const PREVIEW_MAX_BYTES: usize = 512 * 1024;
const PREVIEW_MAX_LINES: usize = 2_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ContentKind {
    Diff,
    Preview,
}

#[derive(Debug)]
pub(crate) struct RefreshRequest {
    pub generation: u64,
    pub scan_entry_limit: usize,
}

#[derive(Debug)]
pub(crate) struct ContentRequest {
    pub generation: u64,
    pub kind: ContentKind,
    pub target: ContentTarget,
}

#[derive(Clone, Debug)]
pub(crate) enum ContentTarget {
    Workspace(PathBuf),
    Repository(RepoChange),
}

#[derive(Debug)]
pub(crate) struct RefreshSnapshot {
    pub branch: Option<String>,
    pub projected_change_count: usize,
    pub scan: ScanResult,
    pub graph: Option<RepoGraph>,
    pub existing_changes: HashSet<crate::repo_graph::RepoPath>,
}

#[derive(Debug)]
pub(crate) struct ContentSnapshot {
    pub provider: Option<String>,
    pub lines: Vec<String>,
    pub show_line_numbers: bool,
}

#[derive(Debug)]
pub(crate) struct RefreshCompletion {
    pub generation: u64,
    pub result: Result<RefreshSnapshot, String>,
}

#[derive(Debug)]
pub(crate) struct ContentCompletion {
    pub generation: u64,
    pub kind: ContentKind,
    pub result: Result<ContentSnapshot, String>,
}

#[derive(Debug)]
struct RequestSlot<T> {
    active: bool,
    pending: Option<T>,
}

impl<T> Default for RequestSlot<T> {
    fn default() -> Self {
        Self {
            active: false,
            pending: None,
        }
    }
}

impl<T> RequestSlot<T> {
    fn submit(&mut self, request: T) {
        self.pending = Some(request);
    }

    fn start_next(&mut self) -> Option<T> {
        if self.active {
            return None;
        }
        let request = self.pending.take()?;
        self.active = true;
        Some(request)
    }

    fn complete(&mut self) {
        debug_assert!(self.active);
        self.active = false;
    }

    fn cancel_pending(&mut self) {
        self.pending = None;
    }

    fn has_work(&self) -> bool {
        self.active || self.pending.is_some()
    }
}

#[derive(Debug, Default)]
pub(crate) struct RequestGeneration {
    next: u64,
    latest_requested: u64,
    last_applied: u64,
    loading: bool,
}

impl RequestGeneration {
    pub fn begin(&mut self) -> u64 {
        self.next = self
            .next
            .checked_add(1)
            .expect("background request generation exhausted");
        self.latest_requested = self.next;
        self.loading = true;
        self.next
    }

    pub fn invalidate(&mut self) {
        let generation = self.begin();
        self.last_applied = generation;
        self.loading = false;
    }

    pub fn accept(&mut self, generation: u64) -> bool {
        if generation != self.latest_requested || generation <= self.last_applied {
            return false;
        }
        self.last_applied = generation;
        self.loading = false;
        true
    }

    pub const fn is_loading(&self) -> bool {
        self.loading
    }
}

#[derive(Default)]
struct SharedState {
    shutdown: bool,
    refresh: RequestSlot<RefreshRequest>,
    content: RequestSlot<ContentRequest>,
    completed_refresh: Option<RefreshCompletion>,
    completed_content: Option<ContentCompletion>,
    preview_registry_update: Option<PreviewRegistry>,
    prefer_refresh: bool,
}

struct Shared {
    state: Mutex<SharedState>,
    changed: Condvar,
}

pub(crate) struct WorkerRuntime {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
}

impl WorkerRuntime {
    pub fn start(root: PathBuf, preview_registry: PreviewRegistry) -> Result<Self> {
        let shared = Arc::new(Shared {
            state: Mutex::new(SharedState {
                prefer_refresh: true,
                ..SharedState::default()
            }),
            changed: Condvar::new(),
        });
        let worker_shared = Arc::clone(&shared);
        let worker = thread::Builder::new()
            .name("lattelens-io".to_owned())
            .spawn(move || worker_loop(worker_shared, root, preview_registry))?;
        Ok(Self {
            shared,
            worker: Some(worker),
        })
    }

    pub fn request_refresh(&self, request: RefreshRequest) {
        let mut state = self.lock_state();
        state.refresh.submit(request);
        self.shared.changed.notify_one();
    }

    pub fn request_content(&self, request: ContentRequest) {
        let mut state = self.lock_state();
        state.content.submit(request);
        self.shared.changed.notify_one();
    }

    pub fn cancel_pending_content(&self) {
        let mut state = self.lock_state();
        state.content.cancel_pending();
    }

    pub fn update_preview_registry(&self, registry: PreviewRegistry) {
        let mut state = self.lock_state();
        state.preview_registry_update = Some(registry);
        self.shared.changed.notify_one();
    }

    pub fn take_completions(&self) -> (Option<RefreshCompletion>, Option<ContentCompletion>) {
        let mut state = self.lock_state();
        (
            state.completed_refresh.take(),
            state.completed_content.take(),
        )
    }

    /// Wait for a result without polling. This is used during startup and by
    /// deterministic tests, never by the interactive event loop.
    pub fn wait_for_completion(&self) -> bool {
        let mut state = self.lock_state();
        while !state.shutdown
            && state.completed_refresh.is_none()
            && state.completed_content.is_none()
            && (state.refresh.has_work() || state.content.has_work())
        {
            state = self
                .shared
                .changed
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        state.completed_refresh.is_some() || state.completed_content.is_some()
    }

    fn lock_state(&self) -> MutexGuard<'_, SharedState> {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Drop for WorkerRuntime {
    fn drop(&mut self) {
        {
            let mut state = self.lock_state();
            state.shutdown = true;
            state.refresh.cancel_pending();
            state.content.cancel_pending();
            self.shared.changed.notify_all();
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

enum Work {
    Refresh(RefreshRequest),
    Content(ContentRequest),
}

fn worker_loop(shared: Arc<Shared>, root: PathBuf, mut preview_registry: PreviewRegistry) {
    let mut graph = None;
    loop {
        let work = {
            let mut state = shared
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            while !state.shutdown
                && state.refresh.pending.is_none()
                && state.content.pending.is_none()
                && state.preview_registry_update.is_none()
            {
                state = shared
                    .changed
                    .wait(state)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
            if state.shutdown {
                return;
            }
            if let Some(registry) = state.preview_registry_update.take() {
                preview_registry = registry;
            }
            take_next_work(&mut state)
        };

        let Some(work) = work else {
            continue;
        };
        match work {
            Work::Refresh(request) => {
                let generation = request.generation;
                let result = catch_worker_error(|| execute_refresh(&root, request));
                if let Ok(snapshot) = &result {
                    graph = snapshot.graph.clone();
                }
                let mut state = shared
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                state.refresh.complete();
                state.completed_refresh = Some(RefreshCompletion { generation, result });
                shared.changed.notify_all();
            }
            Work::Content(request) => {
                let generation = request.generation;
                let kind = request.kind;
                let result = catch_worker_error(|| {
                    execute_content(&root, graph.as_ref(), &preview_registry, request)
                });
                let mut state = shared
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                state.content.complete();
                state.completed_content = Some(ContentCompletion {
                    generation,
                    kind,
                    result,
                });
                shared.changed.notify_all();
            }
        }
    }
}

fn take_next_work(state: &mut SharedState) -> Option<Work> {
    let both_pending = state.refresh.pending.is_some() && state.content.pending.is_some();
    let work = if state.prefer_refresh {
        state
            .refresh
            .start_next()
            .map(Work::Refresh)
            .or_else(|| state.content.start_next().map(Work::Content))
    } else {
        state
            .content
            .start_next()
            .map(Work::Content)
            .or_else(|| state.refresh.start_next().map(Work::Refresh))
    };
    if both_pending {
        state.prefer_refresh = !state.prefer_refresh;
    }
    work
}

fn execute_refresh(root: &std::path::Path, request: RefreshRequest) -> Result<RefreshSnapshot> {
    let graph = RepoGraph::discover(root)?;
    let statuses = workspace_statuses(root, &graph);
    let primary = graph.repositories().iter().find(|snapshot| {
        matches!(
            snapshot.node.kind,
            crate::repo_graph::RepoKind::WorkspaceRoot | crate::repo_graph::RepoKind::Containing
        )
    });
    let branch = primary.and_then(|snapshot| snapshot.branch.clone());
    let projected_change_count = graph.projected_change_count();
    let existing_changes = graph
        .repositories()
        .iter()
        .flat_map(|snapshot| snapshot.changes.iter())
        .filter(|change| {
            graph
                .repository(&change.path.repo_id)
                .is_some_and(|snapshot| {
                    path_exists_without_following(
                        &snapshot.node.worktree.join(&change.path.relative),
                    )
                })
        })
        .map(|change| change.path.clone())
        .collect();
    let scan = tree::scan_with_limit(root, &statuses, request.scan_entry_limit)?;
    Ok(RefreshSnapshot {
        branch,
        projected_change_count,
        scan,
        graph: Some(graph),
        existing_changes,
    })
}

fn execute_content(
    root: &std::path::Path,
    graph: Option<&RepoGraph>,
    registry: &PreviewRegistry,
    request: ContentRequest,
) -> Result<ContentSnapshot> {
    match request.kind {
        ContentKind::Diff => {
            let ContentTarget::Repository(change) = request.target else {
                return Err(anyhow!("selected file is not owned by a Git repository"));
            };
            let graph = graph.ok_or_else(|| anyhow!("repository graph is not available"))?;
            let snapshot = graph.repository(&change.path.repo_id).ok_or_else(|| {
                anyhow!(
                    "repository {} is no longer available",
                    change.path.repo_id.path().display()
                )
            })?;
            let repo = snapshot.node.repo.as_ref().ok_or_else(|| {
                anyhow!(
                    "repository {} is an uninitialized submodule",
                    snapshot.node.worktree.display()
                )
            })?;
            ensure_beneath(root, &snapshot.node.worktree.join(&change.path.relative))?;
            Ok(ContentSnapshot {
                provider: None,
                lines: repo.diff_for_change(
                    &change.path.relative,
                    change.original_path.as_deref(),
                    Some(change.status),
                )?,
                show_line_numbers: false,
            })
        }
        ContentKind::Preview => {
            let (absolute, display_path) = match request.target {
                ContentTarget::Workspace(relative) => (root.join(&relative), relative),
                ContentTarget::Repository(change) => {
                    let graph =
                        graph.ok_or_else(|| anyhow!("repository graph is not available"))?;
                    let snapshot = graph.repository(&change.path.repo_id).ok_or_else(|| {
                        anyhow!(
                            "repository {} is no longer available",
                            change.path.repo_id.path().display()
                        )
                    })?;
                    (
                        snapshot.node.worktree.join(&change.path.relative),
                        change.path.relative,
                    )
                }
            };
            let preview_request = PreviewRequest::new(&absolute, &display_path)
                .within_root(root)
                .with_limits(PREVIEW_MAX_BYTES, PREVIEW_MAX_LINES);
            let preview = match registry.resolve(&preview_request)? {
                PreviewResolution::Preview(preview) => preview,
                PreviewResolution::Unsupported => {
                    return Ok(ContentSnapshot {
                        provider: None,
                        lines: vec![
                            format!("No preview provider accepted {}.", display_path.display()),
                            "Register a PreviewProvider to support this file type.".to_owned(),
                        ],
                        show_line_numbers: false,
                    });
                }
                PreviewResolution::Unsafe {
                    kind,
                    offending_path,
                } => {
                    let offending = offending_path.strip_prefix(root).unwrap_or(&offending_path);
                    let location = if offending_path == absolute {
                        format!("{} is a {}", display_path.display(), kind.label())
                    } else {
                        format!(
                            "{} traverses {} at {}",
                            display_path.display(),
                            kind.label(),
                            offending.display()
                        )
                    };
                    return Ok(ContentSnapshot {
                        provider: None,
                        lines: vec![
                            format!("Preview unavailable: {location}."),
                            "Latte Lens reads only regular files and never follows symbolic links for content."
                                .to_owned(),
                        ],
                        show_line_numbers: false,
                    });
                }
            };
            let mut lines = preview.lines;
            if preview.truncated {
                lines.push(format!(
                    "… preview truncated at {PREVIEW_MAX_BYTES} bytes or {PREVIEW_MAX_LINES} lines"
                ));
            }
            Ok(ContentSnapshot {
                provider: Some(preview.provider_id),
                lines,
                show_line_numbers: preview.show_line_numbers,
            })
        }
    }
}

fn workspace_statuses(root: &std::path::Path, graph: &RepoGraph) -> StatusMap {
    graph
        .repositories()
        .iter()
        .flat_map(|snapshot| snapshot.changes.iter())
        .filter_map(|change| {
            let snapshot = graph.repository(&change.path.repo_id)?;
            let absolute = snapshot.node.worktree.join(&change.path.relative);
            let relative = absolute.strip_prefix(root).ok()?.to_path_buf();
            Some((relative, change.status))
        })
        .collect()
}

fn catch_worker_error<T>(work: impl FnOnce() -> Result<T>) -> Result<T, String> {
    match catch_unwind(AssertUnwindSafe(work)) {
        Ok(result) => result.map_err(|error| format!("{error:#}")),
        Err(payload) => Err(format!(
            "background worker panicked: {}",
            panic_message(payload.as_ref())
        )),
    }
}

fn panic_message(payload: &(dyn Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("unknown panic")
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path, process::Command};

    use super::*;

    #[test]
    fn generations_reject_stale_and_duplicate_completions() {
        let mut generations = RequestGeneration::default();
        let old = generations.begin();
        let current = generations.begin();

        assert!(generations.is_loading());
        assert!(!generations.accept(old));
        assert!(generations.is_loading());
        assert!(generations.accept(current));
        assert!(!generations.is_loading());
        assert!(!generations.accept(current));
    }

    #[test]
    fn refresh_request_coalescing_keeps_only_one_active_and_latest_pending() {
        let mut slot = RequestSlot::default();
        slot.submit(1);
        assert_eq!(slot.start_next(), Some(1));

        slot.submit(2);
        slot.submit(3);
        assert!(slot.start_next().is_none());

        slot.complete();
        assert_eq!(slot.start_next(), Some(3));
        slot.complete();
        assert!(!slot.has_work());
    }

    #[test]
    fn content_request_coalescing_replaces_pending_selection_without_growth() {
        let mut slot = RequestSlot::default();
        slot.submit(ContentRequest {
            generation: 1,
            kind: ContentKind::Preview,
            target: ContentTarget::Workspace(PathBuf::from("old.txt")),
        });
        assert_eq!(slot.start_next().unwrap().generation, 1);

        for generation in 2..=100 {
            slot.submit(ContentRequest {
                generation,
                kind: ContentKind::Preview,
                target: ContentTarget::Workspace(PathBuf::from(format!("{generation}.txt"))),
            });
        }
        slot.complete();

        let latest = slot.start_next().unwrap();
        assert_eq!(latest.generation, 100);
        assert!(matches!(
            latest.target,
            ContentTarget::Workspace(path) if path == std::path::Path::new("100.txt")
        ));
        slot.complete();
        assert!(!slot.has_work());
    }

    #[test]
    fn invalidation_rejects_in_flight_work_without_staying_loading() {
        let mut generations = RequestGeneration::default();
        let obsolete = generations.begin();
        generations.invalidate();

        assert!(!generations.is_loading());
        assert!(!generations.accept(obsolete));
    }

    #[test]
    fn refresh_projects_internal_submodule_dirt_without_parent_double_count() {
        let source = tempfile::tempdir().unwrap();
        init_test_repo(source.path());
        fs::write(source.path().join("tracked.txt"), "initial\n").unwrap();
        test_git(source.path(), &["add", "--all"]);
        test_git(source.path(), &["commit", "--quiet", "-m", "initial"]);

        let parent = tempfile::tempdir().unwrap();
        init_test_repo(parent.path());
        let output = Command::new("git")
            .args([
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "--quiet",
            ])
            .arg(source.path())
            .arg("child")
            .current_dir(parent.path())
            .output()
            .unwrap();
        assert!(output.status.success());
        test_git(parent.path(), &["add", "--all"]);
        test_git(parent.path(), &["commit", "--quiet", "-m", "add child"]);
        fs::write(parent.path().join("child/tracked.txt"), "internal dirt\n").unwrap();

        let snapshot = execute_refresh(
            parent.path(),
            RefreshRequest {
                generation: 1,
                scan_entry_limit: tree::DEFAULT_MAX_ENTRIES,
            },
        )
        .unwrap();
        let graph = snapshot.graph.as_ref().unwrap();
        let raw_status_entries: usize = graph
            .repositories()
            .iter()
            .map(|repository| repository.changes.len())
            .sum();

        assert_eq!(raw_status_entries, 2);
        assert_eq!(graph.projected_change_count(), 1);
        assert_eq!(snapshot.projected_change_count, 1);
    }

    fn init_test_repo(root: &Path) {
        test_git(root, &["-c", "init.defaultBranch=main", "init", "--quiet"]);
        test_git(root, &["config", "user.name", "Latte Lens Tests"]);
        test_git(root, &["config", "user.email", "lattelens@example.invalid"]);
    }

    fn test_git(root: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
