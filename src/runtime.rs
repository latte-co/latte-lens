use std::{
    any::Any,
    collections::{HashSet, VecDeque},
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Component, Path, PathBuf},
    sync::{Arc, Condvar, Mutex, MutexGuard},
    thread::{self, JoinHandle},
};

use anyhow::{Context, Result, anyhow};

use crate::{
    content_safety::{ensure_beneath, path_exists_without_following, resolves_to_directory},
    folding::{FoldRegion, FoldSource, StructureSnapshot, structure_snapshot},
    git::StatusMap,
    navigation::{LineIndex, NavigationSource, language_for_path},
    preview::{HighlightSpan, PreviewRegistry, PreviewRequest, PreviewResolution},
    repo_graph::{DiscoveryOptions, RepoChange, RepoGraph},
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
    pub scan_depth: usize,
    pub full_repository_discovery: bool,
}

#[derive(Debug)]
pub(crate) struct DirectoryRequest {
    pub tree_epoch: u64,
    pub relative: PathBuf,
    pub scan_entry_limit: usize,
}

#[derive(Debug)]
pub(crate) struct ContentRequest {
    pub generation: u64,
    pub kind: ContentKind,
    pub purpose: ContentPurpose,
    pub target: ContentTarget,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ContentPurpose {
    Display,
    NavigationStage { navigation_generation: u64 },
    NavigationPreview { navigation_generation: u64 },
}

#[derive(Clone, Debug)]
pub(crate) enum ContentTarget {
    Workspace(PathBuf),
    /// A source file owned by an externally resolved dependency package.
    ///
    /// This is deliberately not a workspace target: it is never added to the
    /// tree or repository graph, and `root` is the package boundary that the
    /// preview safety gate must enforce for every read.
    Dependency {
        root: PathBuf,
        relative: PathBuf,
        server_root: PathBuf,
    },
    Repository(RepoChange),
}

#[derive(Debug)]
pub(crate) struct RefreshSnapshot {
    pub branch: Option<String>,
    pub projected_change_count: usize,
    pub scan: ScanResult,
    pub graph: Option<RepoGraph>,
    pub existing_changes: HashSet<crate::repo_graph::RepoPath>,
    pub full_repository_discovery: bool,
}

#[derive(Debug)]
pub(crate) struct ContentSnapshot {
    pub provider: Option<String>,
    pub lines: Vec<String>,
    pub highlights: Vec<Vec<HighlightSpan>>,
    pub show_line_numbers: bool,
    pub identity: Option<ContentIdentity>,
    pub fold_source: FoldSource,
    pub fold_regions: Vec<FoldRegion>,
    pub structure: StructureSnapshot,
    pub navigation_source: Option<NavigationSource>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) enum ContentIdentity {
    Workspace(PathBuf),
    Dependency {
        root: PathBuf,
        relative: PathBuf,
        server_root: PathBuf,
    },
}

impl ContentIdentity {
    pub(crate) fn from_absolute(root: &Path, absolute: &Path) -> Option<Self> {
        Self::normalized_relative(root, absolute).map(Self::Workspace)
    }

    pub(crate) fn dependency(root: PathBuf, absolute: &Path, server_root: PathBuf) -> Option<Self> {
        Self::normalized_relative(&root, absolute).map(|relative| Self::Dependency {
            root,
            relative,
            server_root,
        })
    }

    fn normalized_relative(root: &Path, absolute: &Path) -> Option<PathBuf> {
        let relative = absolute.strip_prefix(root).ok()?;
        let mut normalized = PathBuf::new();
        for component in relative.components() {
            match component {
                Component::Normal(value) => normalized.push(value),
                Component::CurDir
                | Component::ParentDir
                | Component::RootDir
                | Component::Prefix(_) => return None,
            }
        }
        (!normalized.as_os_str().is_empty()).then_some(normalized)
    }

    pub(crate) fn path(&self) -> &Path {
        match self {
            Self::Workspace(path) => path,
            Self::Dependency { relative, .. } => relative,
        }
    }

    pub(crate) fn workspace_path(&self) -> Option<&Path> {
        match self {
            Self::Workspace(path) => Some(path),
            Self::Dependency { .. } => None,
        }
    }

    pub(crate) fn display_path(&self) -> PathBuf {
        match self {
            Self::Workspace(path) => path.clone(),
            Self::Dependency { root, relative, .. } => {
                let mut display = PathBuf::from("dependency");
                display.push(root.file_name().unwrap_or_else(|| root.as_os_str()));
                display.push(relative);
                display
            }
        }
    }

    pub(crate) fn display_label(&self) -> String {
        match self {
            Self::Workspace(path) => path.display().to_string(),
            Self::Dependency { root, relative, .. } => format!(
                "Dependency · {}/{}",
                root.file_name()
                    .unwrap_or_else(|| root.as_os_str())
                    .to_string_lossy(),
                relative.display()
            ),
        }
    }

    pub(crate) fn content_root<'a>(&'a self, workspace_root: &'a Path) -> &'a Path {
        match self {
            Self::Workspace(_) => workspace_root,
            Self::Dependency { root, .. } => root,
        }
    }

    pub(crate) fn absolute_path(&self, workspace_root: &Path) -> PathBuf {
        match self {
            Self::Workspace(path) => workspace_root.join(path),
            Self::Dependency { root, relative, .. } => root.join(relative),
        }
    }
}

#[derive(Debug)]
pub(crate) struct RefreshCompletion {
    pub generation: u64,
    pub result: Result<RefreshSnapshot, String>,
}

#[derive(Debug)]
pub(crate) struct DirectoryCompletion {
    pub tree_epoch: u64,
    pub relative: PathBuf,
    pub result: Result<ScanResult, String>,
}

#[derive(Debug)]
pub(crate) struct ContentCompletion {
    pub generation: u64,
    pub kind: ContentKind,
    pub purpose: ContentPurpose,
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
struct DirectoryQueue {
    active: bool,
    pending: VecDeque<DirectoryRequest>,
}

impl DirectoryQueue {
    fn submit(&mut self, request: DirectoryRequest) {
        if !self.pending.iter().any(|pending| {
            pending.tree_epoch == request.tree_epoch && pending.relative == request.relative
        }) {
            self.pending.push_back(request);
        }
    }

    fn start_next(&mut self) -> Option<DirectoryRequest> {
        if self.active {
            return None;
        }
        let request = self.pending.pop_front()?;
        self.active = true;
        Some(request)
    }

    fn complete(&mut self) {
        debug_assert!(self.active);
        self.active = false;
    }

    fn cancel_pending(&mut self) {
        self.pending.clear();
    }

    fn has_work(&self) -> bool {
        self.active || !self.pending.is_empty()
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
    directory: DirectoryQueue,
    content: RequestSlot<ContentRequest>,
    completed_refresh: Option<RefreshCompletion>,
    completed_directories: VecDeque<DirectoryCompletion>,
    completed_content: Option<ContentCompletion>,
    preview_registry_update: Option<PreviewRegistry>,
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
        // Content paths come from the canonical repository graph, so keep the
        // worker boundary in the same representation on every platform.
        let root = root
            .canonicalize()
            .with_context(|| format!("cannot open workspace {}", root.display()))?;
        let shared = Arc::new(Shared {
            state: Mutex::new(SharedState::default()),
            changed: Condvar::new(),
        });
        let worker_shared = Arc::clone(&shared);
        let worker = thread::Builder::new()
            .name("latte-lens-io".to_owned())
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

    pub fn request_directory(&self, request: DirectoryRequest) {
        let mut state = self.lock_state();
        state.directory.submit(request);
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

    pub fn take_completions(
        &self,
    ) -> (
        Option<RefreshCompletion>,
        Vec<DirectoryCompletion>,
        Option<ContentCompletion>,
    ) {
        let mut state = self.lock_state();
        (
            state.completed_refresh.take(),
            state.completed_directories.drain(..).collect(),
            state.completed_content.take(),
        )
    }

    /// Wait for a result without polling. This is used during startup and by
    /// deterministic tests, never by the interactive event loop.
    pub fn wait_for_completion(&self) -> bool {
        let mut state = self.lock_state();
        while !state.shutdown
            && state.completed_refresh.is_none()
            && state.completed_directories.is_empty()
            && state.completed_content.is_none()
            && (state.refresh.has_work() || state.directory.has_work() || state.content.has_work())
        {
            state = self
                .shared
                .changed
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        state.completed_refresh.is_some()
            || !state.completed_directories.is_empty()
            || state.completed_content.is_some()
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
            state.directory.cancel_pending();
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
    Directory(DirectoryRequest),
    Content(ContentRequest),
}

fn worker_loop(shared: Arc<Shared>, root: PathBuf, mut preview_registry: PreviewRegistry) {
    let mut graph = None;
    let mut statuses = StatusMap::new();
    loop {
        let work = {
            let mut state = shared
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            while !state.shutdown
                && state.refresh.pending.is_none()
                && state.directory.pending.is_empty()
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
                let result = result.map(|executed| {
                    graph = executed.snapshot.graph.clone();
                    statuses = executed.statuses;
                    executed.snapshot
                });
                let mut state = shared
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                state.refresh.complete();
                state.completed_refresh = Some(RefreshCompletion { generation, result });
                shared.changed.notify_all();
            }
            Work::Directory(request) => {
                let tree_epoch = request.tree_epoch;
                let relative = request.relative.clone();
                let result = catch_worker_error(|| execute_directory(&root, &statuses, request));
                let mut state = shared
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                state.directory.complete();
                state.completed_directories.push_back(DirectoryCompletion {
                    tree_epoch,
                    relative,
                    result,
                });
                shared.changed.notify_all();
            }
            Work::Content(request) => {
                let generation = request.generation;
                let kind = request.kind;
                let purpose = request.purpose;
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
                    purpose,
                    result,
                });
                shared.changed.notify_all();
            }
        }
    }
}

fn take_next_work(state: &mut SharedState) -> Option<Work> {
    if let Some(request) = state.refresh.start_next() {
        return Some(Work::Refresh(request));
    }
    if let Some(request) = state.directory.start_next() {
        return Some(Work::Directory(request));
    }
    state.content.start_next().map(Work::Content)
}

struct ExecutedRefresh {
    snapshot: RefreshSnapshot,
    statuses: StatusMap,
}

fn execute_refresh(root: &std::path::Path, request: RefreshRequest) -> Result<ExecutedRefresh> {
    let repo_scan_depth = if request.full_repository_discovery {
        crate::repo_graph::DEFAULT_MAX_DISCOVERY_DEPTH
    } else {
        tree::DEFAULT_INITIAL_SCAN_DEPTH.saturating_sub(1)
    };
    let graph = RepoGraph::discover_with_options(
        root,
        DiscoveryOptions {
            max_entries: crate::repo_graph::DEFAULT_MAX_DISCOVERY_ENTRIES,
            max_repositories: crate::repo_graph::DEFAULT_MAX_REPOSITORIES,
            max_depth: repo_scan_depth,
        },
    )?;
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
    let scan = tree::scan_with_depth(
        root,
        &statuses,
        request.scan_entry_limit,
        request.scan_depth,
    )?;
    Ok(ExecutedRefresh {
        snapshot: RefreshSnapshot {
            branch,
            projected_change_count,
            scan,
            graph: Some(graph),
            existing_changes,
            full_repository_discovery: request.full_repository_discovery,
        },
        statuses,
    })
}

fn execute_directory(
    root: &std::path::Path,
    statuses: &StatusMap,
    request: DirectoryRequest,
) -> Result<ScanResult> {
    let absolute = root.join(&request.relative);
    // All Files follows symlinks, so a directory symlink (or a directory
    // reached through one) is a valid, expandable target. `scan_directory`
    // still rejects any relative path that escapes the scan root textually.
    if !resolves_to_directory(&absolute) {
        anyhow::bail!("{} is not a readable directory", request.relative.display());
    }
    tree::scan_directory(root, &request.relative, statuses, request.scan_entry_limit)
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
                highlights: Vec::new(),
                show_line_numbers: false,
                identity: None,
                fold_source: FoldSource::None,
                fold_regions: Vec::new(),
                structure: StructureSnapshot::unavailable(),
                navigation_source: None,
            })
        }
        ContentKind::Preview => {
            // Only the interactive All Files view (a Workspace target) follows a
            // final symbolic link to preview its target content. Repository and
            // dependency previews keep the strict no-follow policy so a tracked
            // link renders as its target path, never its target's bytes.
            let follow_symlinks = matches!(request.target, ContentTarget::Workspace(_));
            let (absolute, display_path, content_root, identity, navigation_server_root) =
                match request.target {
                    ContentTarget::Workspace(relative) => {
                        let absolute = root.join(&relative);
                        (
                            absolute.clone(),
                            relative,
                            root.to_path_buf(),
                            ContentIdentity::from_absolute(root, &absolute),
                            server_root_for_document(root, graph, &absolute),
                        )
                    }
                    ContentTarget::Dependency {
                        root: dependency_root,
                        relative,
                        server_root,
                    } => {
                        let absolute = dependency_root.join(&relative);
                        let identity = ContentIdentity::dependency(
                            dependency_root.clone(),
                            &absolute,
                            server_root.clone(),
                        );
                        let display_path = identity
                            .as_ref()
                            .map_or_else(|| relative.clone(), ContentIdentity::display_path);
                        (
                            absolute,
                            display_path,
                            dependency_root,
                            identity,
                            server_root,
                        )
                    }
                    ContentTarget::Repository(change) => {
                        let graph =
                            graph.ok_or_else(|| anyhow!("repository graph is not available"))?;
                        let snapshot = graph.repository(&change.path.repo_id).ok_or_else(|| {
                            anyhow!(
                                "repository {} is no longer available",
                                change.path.repo_id.path().display()
                            )
                        })?;
                        let relative = change.path.relative;
                        let absolute = snapshot.node.worktree.join(&relative);
                        (
                            absolute.clone(),
                            relative,
                            root.to_path_buf(),
                            ContentIdentity::from_absolute(root, &absolute),
                            server_root_for_document(root, Some(graph), &absolute),
                        )
                    }
                };
            let preview_request = PreviewRequest::new(&absolute, &display_path)
                .within_root(&content_root)
                .following_symlinks(follow_symlinks)
                .with_limits(PREVIEW_MAX_BYTES, PREVIEW_MAX_LINES);
            let (preview, fold_source) = match registry.resolve(&preview_request)? {
                PreviewResolution::Preview {
                    preview,
                    fold_source,
                } => (preview, fold_source),
                PreviewResolution::Unsupported => {
                    return Ok(ContentSnapshot {
                        provider: None,
                        lines: vec![
                            format!("No preview provider accepted {}.", display_path.display()),
                            "Register a PreviewProvider to support this file type.".to_owned(),
                        ],
                        highlights: Vec::new(),
                        show_line_numbers: false,
                        identity: None,
                        fold_source: FoldSource::None,
                        fold_regions: Vec::new(),
                        structure: StructureSnapshot::unavailable(),
                        navigation_source: None,
                    });
                }
                PreviewResolution::Unsafe {
                    kind,
                    offending_path,
                } => {
                    let offending = offending_path
                        .strip_prefix(&content_root)
                        .unwrap_or(&offending_path);
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
                        highlights: Vec::new(),
                        show_line_numbers: false,
                        identity: None,
                        fold_source: FoldSource::None,
                        fold_regions: Vec::new(),
                        structure: StructureSnapshot::unavailable(),
                        navigation_source: None,
                    });
                }
            };
            let mut lines = preview.lines;
            let mut highlights = preview.highlights;
            let structure = if fold_source.allows_folding() {
                identity
                    .as_ref()
                    .map_or_else(StructureSnapshot::unavailable, |identity| {
                        structure_snapshot(identity.path(), &lines)
                    })
            } else {
                StructureSnapshot::unavailable()
            };
            let fold_regions = structure.folds.clone();
            let navigation_source = if fold_source == FoldSource::BuiltinText
                && !preview.truncated
                && let (Some(identity), Some(language)) =
                    (identity.clone(), language_for_path(&display_path))
            {
                let text: Arc<str> = Arc::from(lines.join("\n"));
                let line_index = Arc::new(LineIndex::new(Arc::clone(&text))?);
                let disk_raw_len = preview_request.open_regular()?.map_or(0, |file| file.len());
                Some(NavigationSource {
                    identity,
                    absolute_path: absolute.clone(),
                    content_root,
                    disk_raw_len,
                    server_root: navigation_server_root,
                    language,
                    text,
                    line_index,
                    structure: Arc::new(structure.clone()),
                })
            } else {
                None
            };
            if preview.truncated {
                lines.push(format!(
                    "… preview truncated at {PREVIEW_MAX_BYTES} bytes or {PREVIEW_MAX_LINES} lines"
                ));
                highlights.push(Vec::new());
            }
            Ok(ContentSnapshot {
                provider: Some(preview.provider_id),
                lines,
                highlights,
                show_line_numbers: preview.show_line_numbers,
                identity,
                fold_source,
                fold_regions,
                structure,
                navigation_source,
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

pub(crate) fn server_root_for_document(
    app_root: &Path,
    graph: Option<&RepoGraph>,
    absolute: &Path,
) -> PathBuf {
    graph
        .into_iter()
        .flat_map(RepoGraph::repositories)
        .filter(|repository| {
            repository.node.repo.is_some()
                && repository.node.worktree.starts_with(app_root)
                && absolute.starts_with(&repository.node.worktree)
        })
        .max_by_key(|repository| repository.node.worktree.components().count())
        .map_or_else(
            || app_root.to_path_buf(),
            |repository| repository.node.worktree.clone(),
        )
}

/// Rebind an immutable preview source to the repository graph installed by a
/// refresh without reopening the file or rebuilding its text indexes.
///
/// A source whose absolute path no longer agrees with its workspace identity
/// is not safe to carry forward. Returning `None` makes navigation degrade
/// closed instead of retaining a server root from the previous graph.
pub(crate) fn rebind_navigation_source(
    app_root: &Path,
    graph: Option<&RepoGraph>,
    source: &NavigationSource,
) -> Option<NavigationSource> {
    match &source.identity {
        ContentIdentity::Workspace(_) => {
            let identity = ContentIdentity::from_absolute(app_root, &source.absolute_path)?;
            if identity != source.identity || source.content_root != app_root {
                return None;
            }
            let mut rebound = source.clone();
            rebound.server_root = server_root_for_document(app_root, graph, &source.absolute_path);
            Some(rebound)
        }
        ContentIdentity::Dependency { .. } => {
            let expected = source.identity.absolute_path(app_root);
            if expected != source.absolute_path
                || source.identity.content_root(app_root) != source.content_root
            {
                return None;
            }
            Some(source.clone())
        }
    }
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
    fn content_identity_normalizes_nested_repository_paths_and_rejects_escape_components() {
        let root = Path::new("/workspace");
        let nested = Path::new("/workspace/vendor/repo/src/lib.rs");
        assert_eq!(
            ContentIdentity::from_absolute(root, nested)
                .expect("nested repository path should remain workspace relative")
                .path(),
            Path::new("vendor/repo/src/lib.rs")
        );
        assert!(ContentIdentity::from_absolute(root, Path::new("/outside/lib.rs")).is_none());
        assert!(
            ContentIdentity::from_absolute(root, Path::new("/workspace/../escape.rs")).is_none()
        );
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
            purpose: ContentPurpose::Display,
            target: ContentTarget::Workspace(PathBuf::from("old.txt")),
        });
        assert_eq!(slot.start_next().unwrap().generation, 1);

        for generation in 2..=100 {
            slot.submit(ContentRequest {
                generation,
                kind: ContentKind::Preview,
                purpose: ContentPurpose::Display,
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
                scan_depth: tree::DEFAULT_INITIAL_SCAN_DEPTH,
                full_repository_discovery: true,
            },
        )
        .unwrap();
        let snapshot = snapshot.snapshot;
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
        test_git(
            root,
            &["config", "user.email", "latte-lens@example.invalid"],
        );
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
