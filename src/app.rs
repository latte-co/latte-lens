use std::{
    collections::{HashMap, HashSet, VecDeque},
    io,
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use ratatui::{
    DefaultTerminal,
    crossterm::event::{
        self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
        MouseEventKind,
    },
    layout::Rect,
    widgets::ListState,
};
use regex::RegexBuilder;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[cfg(feature = "agent-observability")]
use crate::agent::{
    ActivityState, AgentRuntime, AgentRuntimeCompletion, AgentRuntimeRequest, AgentState,
    AgentViewSession, AgentViewState, ApplyResult, EvidenceExpiry, ObservationFreshness,
    ObservationMode, ObserverId, RuntimeBackpressure, SessionLifecycle, WorkspaceSelector,
};

use crate::{
    clipboard,
    diff::{DiffLineAnnotation, DiffLineKind, annotate_diff, line_number_width},
    folding::{FoldAnchor, FoldRegion, FoldSource, StructureSnapshot, SymbolId},
    git::{ChangeVersion, DiffStat, FileStatus, GitRepo},
    lsp::{
        NavigationDocumentSymbolCompletion, NavigationProtocolResult, NavigationRuntime,
        NavigationRuntimeCompletion, NavigationRuntimeRequest, ProtocolDocumentSymbol,
        ProtocolLocation,
    },
    navigation::{
        AppOptions, DocumentVersion, NavigationFileTarget, NavigationOperation, NavigationSettings,
        NavigationSource, NavigationTargetRange, SourcePosition, SourceRange,
        lsp_uri_to_navigation_target,
    },
    preview::{HighlightKind, HighlightSpan, PreviewProvider, PreviewRegistry},
    repo_graph::{
        DiscoveryError, DiscoveryTruncation, RepoChange, RepoGraph, RepoId, RepoKind, RepoPath,
        RepoRelationState, RepoSnapshot,
    },
    runtime::{
        ContentCompletion, ContentIdentity, ContentKind, ContentPurpose, ContentRequest,
        ContentSnapshot, ContentTarget, DirectoryCompletion, DirectoryRequest, RefreshCompletion,
        RefreshRequest, RefreshSnapshot, RequestGeneration, WorkerRuntime,
        rebind_navigation_source,
    },
    search::{SearchEvent, SearchMatch, SearchOptions, SearchRequest, SearchRuntime},
    text_layout::{expand_tabs, grapheme_width_at},
    tree::{self, FileEntry},
    ui,
};

#[cfg(feature = "navigation-test-support")]
#[derive(Clone)]
#[doc(hidden)]
pub struct NavigationTestProbe {
    stats: Arc<std::sync::Mutex<crate::lsp::NavigationCleanupStats>>,
}

#[cfg(feature = "navigation-test-support")]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[doc(hidden)]
pub struct NavigationCleanupReport {
    pub sessions_cleaned: usize,
    pub clean_exits: usize,
    pub forced_tree_cleanups: usize,
    pub direct_children_reaped: usize,
    pub io_threads_joined: usize,
    pub process_owners_dropped: usize,
    pub quarantined_process_owners: usize,
}

#[cfg(feature = "navigation-test-support")]
impl NavigationTestProbe {
    pub fn snapshot(&self) -> NavigationCleanupReport {
        let snapshot = self
            .stats
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .snapshot;
        NavigationCleanupReport {
            sessions_cleaned: snapshot.sessions_cleaned,
            clean_exits: snapshot.clean_exits,
            forced_tree_cleanups: snapshot.forced_tree_cleanups,
            direct_children_reaped: snapshot.direct_children_reaped,
            io_threads_joined: snapshot.io_threads_joined,
            process_owners_dropped: snapshot.process_owners_dropped,
            quarantined_process_owners: snapshot.quarantined_process_owners,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TreeScope {
    #[default]
    AllFiles,
    GitChanges,
    #[cfg(feature = "agent-observability")]
    Agents,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum GitRowIdentity {
    Repository(RepoId),
    Directory(RepoPath),
    Change(RepoPath),
    Pointer(RepoPath),
    Issue(PathBuf),
}

#[derive(Clone, Debug)]
pub enum GitRowKind {
    Repository {
        repo_id: RepoId,
        kind: RepoKind,
        change_count: usize,
        status_error: Option<String>,
    },
    Directory,
    Change(RepoChange),
    Pointer(RepoChange),
    Issue(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DiffReviewState {
    Unreviewed,
    Reviewed,
    ChangedAfterReview,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ReviewProgress {
    pub total: usize,
    pub reviewed: usize,
    pub changed_after_review: usize,
}

#[derive(Clone, Debug)]
pub struct GitTreeRow {
    pub identity: GitRowIdentity,
    pub kind: GitRowKind,
    pub depth: usize,
    pub label: String,
    pub detail: String,
    pub status: Option<FileStatus>,
    pub exists: bool,
    ancestors: Vec<GitRowIdentity>,
    file_entry: Option<FileEntry>,
}

impl GitTreeRow {
    pub const fn is_container(&self) -> bool {
        matches!(
            self.kind,
            GitRowKind::Repository { .. } | GitRowKind::Directory
        )
    }

    pub const fn is_change(&self) -> bool {
        matches!(self.kind, GitRowKind::Change(_) | GitRowKind::Pointer(_))
    }

    pub fn file_entry(&self) -> Option<&FileEntry> {
        self.file_entry.as_ref()
    }
}

impl TreeScope {
    pub const fn next(self) -> Self {
        match self {
            Self::AllFiles => Self::GitChanges,
            #[cfg(feature = "agent-observability")]
            Self::GitChanges => Self::Agents,
            #[cfg(not(feature = "agent-observability"))]
            Self::GitChanges => Self::AllFiles,
            #[cfg(feature = "agent-observability")]
            Self::Agents => Self::AllFiles,
        }
    }

    pub const fn previous(self) -> Self {
        match self {
            Self::AllFiles => {
                #[cfg(feature = "agent-observability")]
                {
                    Self::Agents
                }
                #[cfg(not(feature = "agent-observability"))]
                {
                    Self::GitChanges
                }
            }
            Self::GitChanges => Self::AllFiles,
            #[cfg(feature = "agent-observability")]
            Self::Agents => Self::GitChanges,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum FocusPane {
    ScopeTabs,
    #[default]
    Tree,
    Content,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SearchMode {
    Files,
    Text,
}

impl SearchMode {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Files => "Open File",
            Self::Text => "Search Workspace",
        }
    }

    const fn index(self) -> usize {
        match self {
            Self::Files => 0,
            Self::Text => 1,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchResult {
    pub path: PathBuf,
    pub is_dir: bool,
    pub line_number: Option<usize>,
    pub line: Option<String>,
    pub match_range: Option<Range<usize>>,
    source_match_range: Option<Range<usize>>,
}

#[derive(Clone)]
struct SearchRestore {
    focused_pane: FocusPane,
    tree_scope: TreeScope,
    tree_state: ListState,
    all_files_selection: Option<PathBuf>,
    git_changes_selection: Option<GitRowIdentity>,
    pending_all_scope_path: Option<PathBuf>,
    pending_all_scope_navigation: bool,
    pending_git_scope_path: Option<PathBuf>,
    pending_git_scope_fallback: Option<GitRowIdentity>,
    content_lines: Vec<String>,
    content_highlights: Vec<Vec<HighlightSpan>>,
    content_viewport: ContentViewportRestore,
    content_horizontal_scroll: usize,
    content_selection: Option<ContentSelection>,
    clipboard_status: Option<String>,
    content_mode: ContentMode,
    content_provider: Option<String>,
    content_show_line_numbers: bool,
    content_diff_lines: Vec<DiffLineAnnotation>,
    content_identity: Option<ContentIdentity>,
    content_fold_source: FoldSource,
    content_fold_regions: Vec<FoldRegion>,
    content_structure: StructureSnapshot,
    content_collapsed_folds: HashSet<FoldAnchor>,
    content_cursor_line: usize,
    content_successful: bool,
    content_was_loading: bool,
    last_error: Option<String>,
    navigation_source: Option<Arc<NavigationSource>>,
    navigation_document_version: DocumentVersion,
    navigation_caret: NavigationCaret,
    navigation_target_highlight: Option<SourceRange>,
    navigation_status: Option<NavigationStatus>,
    navigation_back: VecDeque<NavigationHistoryEntry>,
    navigation_forward: VecDeque<NavigationHistoryEntry>,
}

#[derive(Clone, Copy, Debug)]
struct ContentViewportRestore {
    line: Option<usize>,
    byte_start: usize,
    synthetic: bool,
    effective_scroll: usize,
}

pub(crate) struct SearchState {
    pub mode: SearchMode,
    pub query: String,
    pub cursor: usize,
    pub results: Vec<SearchResult>,
    pub options: SearchOptions,
    pub searching: bool,
    pub indexing: bool,
    pub truncated: bool,
    pub scanned_files: usize,
    pub error: Option<String>,
    generation: u64,
    due: Option<Instant>,
    selection_hint: Option<SearchSelectionHint>,
    restore: SearchRestore,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SearchResultIdentity {
    path: PathBuf,
    line_number: Option<usize>,
    source_match_range: Option<Range<usize>>,
}

impl From<&SearchResult> for SearchResultIdentity {
    fn from(result: &SearchResult) -> Self {
        Self {
            path: result.path.clone(),
            line_number: result.line_number,
            source_match_range: result.source_match_range.clone(),
        }
    }
}

#[derive(Clone, Debug)]
struct SearchSelectionHint {
    identity: SearchResultIdentity,
    index: usize,
    offset: usize,
}

#[derive(Clone, Debug)]
struct SearchSession {
    query: String,
    cursor: usize,
    results: Vec<SearchResult>,
    options: SearchOptions,
    truncated: bool,
    scanned_files: usize,
    error: Option<String>,
    list_state: ListState,
    selected_identity: Option<SearchResultIdentity>,
    tree_epoch: u64,
    needs_refresh: bool,
}

#[derive(Clone, Debug)]
struct SearchPreviewTarget {
    generation: u64,
    line_number: usize,
    byte_range: Range<usize>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PreviewFindMatch {
    line: usize,
    range: Range<usize>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum QuitKey {
    Q,
    Escape,
}

#[derive(Clone, Copy, Debug)]
struct QuitConfirmation {
    key: QuitKey,
    deadline: Instant,
}

const QUIT_CONFIRM_WINDOW: Duration = Duration::from_millis(1_500);
const NAVIGATION_HISTORY_LIMIT: usize = 128;
const NAVIGATION_STATUS_INFO: Duration = Duration::from_secs(4);
const NAVIGATION_STATUS_ERROR: Duration = Duration::from_secs(8);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NavigationStatusLevel {
    Info,
    Error,
}

#[derive(Clone, Debug)]
pub(crate) struct NavigationStatus {
    pub(crate) level: NavigationStatusLevel,
    pub(crate) message: String,
    expires_at: Instant,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct NavigationTarget {
    document: ContentIdentity,
    range: NavigationTargetRange,
}

#[derive(Clone, Debug)]
struct NavigationHistoryEntry {
    target: NavigationTarget,
    viewport: ContentViewportRestore,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NavigationHistoryIntent {
    Jump,
    Back,
    Forward,
}

#[derive(Clone, Debug)]
struct NavigationInvocation {
    generation: u64,
    operation: NavigationOperation,
    source_identity: ContentIdentity,
    source_version: DocumentVersion,
    origin: NavigationHistoryEntry,
    history_intent: NavigationHistoryIntent,
    destination_viewport: Option<ContentViewportRestore>,
    return_focus: FocusPane,
}

#[derive(Clone, Debug)]
pub(crate) struct NavigationPickerItem {
    target: NavigationTarget,
    pub(crate) label: String,
    pub(crate) detail: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NavigationPickerGroup {
    pub(crate) path: PathBuf,
    pub(crate) results: Range<usize>,
    pub(crate) expanded: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NavigationPickerRow {
    Group(usize),
    Result(usize),
}

#[derive(Clone, Debug)]
pub(crate) struct NavigationPickerPreview {
    pub(crate) path: PathBuf,
    pub(crate) lines: Vec<String>,
    pub(crate) highlights: Vec<Vec<HighlightSpan>>,
    pub(crate) target: SourceRange,
}

#[derive(Clone, Debug)]
pub(crate) struct NavigationPickerState {
    pub(crate) title: String,
    invocation: NavigationInvocation,
    pub(crate) results: Vec<NavigationPickerItem>,
    pub(crate) groups: Vec<NavigationPickerGroup>,
    pub(crate) visible_rows: Vec<NavigationPickerRow>,
    pub(crate) list_state: ListState,
    pub(crate) preview: Option<NavigationPickerPreview>,
    pub(crate) preview_loading: bool,
    pub(crate) preview_error: Option<String>,
    return_focus: FocusPane,
}

#[derive(Clone, Debug)]
struct PendingNavigationStage {
    invocation: NavigationInvocation,
    content_generation: u64,
    target: NavigationTarget,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct NavigationCaret {
    point: SourcePosition,
    preferred_display_column: usize,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct PreviewFindState {
    pub query: String,
    pub cursor: usize,
    matches: Vec<PreviewFindMatch>,
    selected: usize,
    pub case_sensitive: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContentMode {
    Info,
    Diff,
    Preview,
}

impl ContentMode {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Info => "INFO",
            Self::Diff => "DIFF",
            Self::Preview => "PREVIEW",
        }
    }

    pub const fn title(self) -> &'static str {
        match self {
            Self::Info => "Info",
            Self::Diff => "Diff",
            Self::Preview => "Preview",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ContentPoint {
    line: usize,
    byte: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ContentSelection {
    anchor_before: ContentPoint,
    anchor_after: ContentPoint,
    head: ContentPoint,
    dragging: bool,
    dragged: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ContentVisualRow {
    pub line_index: usize,
    pub byte_range: Range<usize>,
    pub continuation: bool,
    pub tab_origin: usize,
    pub summary: Option<String>,
    pub synthetic: bool,
    pub fold_marker: FoldVisualMarker,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum FoldVisualMarker {
    #[default]
    None,
    Expanded,
    Collapsed,
}

impl ContentSelection {
    fn normalized(self) -> (ContentPoint, ContentPoint) {
        if self.head >= self.anchor_before {
            (self.anchor_before, self.head)
        } else {
            (self.head, self.anchor_after)
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct UiRegions {
    pub all_files_tab: Rect,
    pub git_changes_tab: Rect,
    #[cfg(feature = "agent-observability")]
    pub agents_tab: Rect,
    pub refresh_button: Rect,
    pub file_search_button: Rect,
    pub text_search_button: Rect,
    pub search_popup: Rect,
    pub search_input: Rect,
    pub search_clear: Rect,
    pub search_close: Rect,
    pub search_files_mode: Rect,
    pub search_text_mode: Rect,
    pub search_options: [Rect; 4],
    pub search_results: Rect,
    pub navigation_popup: Rect,
    pub navigation_preview: Rect,
    pub navigation_results: Rect,
    pub preview_find_input: Rect,
    pub preview_find_case: Rect,
    pub preview_find_position: Rect,
    pub preview_find_previous: Rect,
    pub preview_find_next: Rect,
    pub preview_find_close: Rect,
    pub tree_body: Rect,
    pub tree_inner: Rect,
    pub divider: Rect,
    pub content_body: Rect,
    pub content_inner: Rect,
}

impl UiRegions {
    fn scope_at(self, column: u16, row: u16) -> Option<TreeScope> {
        if contains(self.all_files_tab, column, row) {
            Some(TreeScope::AllFiles)
        } else if contains(self.git_changes_tab, column, row) {
            Some(TreeScope::GitChanges)
        } else if cfg!(feature = "agent-observability")
            && contains(
                {
                    #[cfg(feature = "agent-observability")]
                    {
                        self.agents_tab
                    }
                    #[cfg(not(feature = "agent-observability"))]
                    {
                        Rect::default()
                    }
                },
                column,
                row,
            )
        {
            #[cfg(feature = "agent-observability")]
            {
                Some(TreeScope::Agents)
            }
            #[cfg(not(feature = "agent-observability"))]
            {
                None
            }
        } else {
            None
        }
    }

    fn refresh_at(self, column: u16, row: u16) -> bool {
        contains(self.refresh_button, column, row)
    }

    fn file_search_at(self, column: u16, row: u16) -> bool {
        contains(self.file_search_button, column, row)
    }

    fn text_search_at(self, column: u16, row: u16) -> bool {
        contains(self.text_search_button, column, row)
    }
}

pub struct App {
    pub root: PathBuf,
    pub repo: Option<GitRepo>,
    pub all_entries: Vec<FileEntry>,
    pub changed_entries: Vec<FileEntry>,
    pub all_files_truncated: bool,
    pub git_changes_truncated: bool,
    pub tree_state: ListState,
    pub tree_scope: TreeScope,
    pub focused_pane: FocusPane,
    pub content_lines: Vec<String>,
    pub content_highlights: Vec<Vec<HighlightSpan>>,
    pub content_scroll: usize,
    pub content_horizontal_scroll: usize,
    content_selection: Option<ContentSelection>,
    pending_clipboard_text: Option<String>,
    pub clipboard_status: Option<String>,
    pub content_mode: ContentMode,
    pub content_provider: Option<String>,
    pub content_show_line_numbers: bool,
    pub(crate) content_diff_lines: Vec<DiffLineAnnotation>,
    content_identity: Option<ContentIdentity>,
    content_fold_source: FoldSource,
    content_fold_regions: Vec<FoldRegion>,
    pub(crate) content_structure: StructureSnapshot,
    content_collapsed_folds: HashSet<FoldAnchor>,
    content_cursor_line: usize,
    content_successful: bool,
    content_projection_width: u16,
    fold_cache: VecDeque<(ContentIdentity, HashSet<FoldAnchor>)>,
    pub branch: Option<String>,
    pub changed_count: usize,
    pub total_repository_count: usize,
    pub dirty_repository_count: usize,
    pub repository_error_count: usize,
    pub repository_graph_truncated: bool,
    pub last_error: Option<String>,
    pub ui_regions: UiRegions,
    tree_panel_width: Option<u16>,
    tree_resize_dragging: bool,
    preview_registry: PreviewRegistry,
    repo_graph: Option<RepoGraph>,
    all_files_selection: Option<PathBuf>,
    git_changes_selection: Option<GitRowIdentity>,
    pending_all_scope_path: Option<PathBuf>,
    pending_all_scope_navigation: bool,
    pending_git_scope_path: Option<PathBuf>,
    pending_git_scope_fallback: Option<GitRowIdentity>,
    all_files_expansion: HashMap<PathBuf, bool>,
    unloaded_directories: HashSet<PathBuf>,
    loading_directories: HashSet<PathBuf>,
    tree_epoch: u64,
    git_changes_expansion: HashMap<GitRowIdentity, bool>,
    reviewed_change_versions: HashMap<RepoPath, ChangeVersion>,
    current_diff_path: Option<RepoPath>,
    pending_diff_path: Option<(u64, RepoPath)>,
    // This is the single selectable/renderable tree dataset. The raw vectors
    // above remain canonical scan results for their respective scopes.
    visible_rows: Vec<FileEntry>,
    visible_changed_entries: Vec<FileEntry>,
    git_rows: Vec<GitTreeRow>,
    visible_git_rows: Vec<GitTreeRow>,
    scan_entry_limit: usize,
    runtime: WorkerRuntime,
    refresh_requests: RequestGeneration,
    content_requests: RequestGeneration,
    navigation_preview_requests: RequestGeneration,
    search_runtime: SearchRuntime,
    pub(crate) search: Option<SearchState>,
    pub(crate) search_list_state: ListState,
    search_sessions: [Option<SearchSession>; 2],
    pub(crate) preview_find: Option<PreviewFindState>,
    search_generation: u64,
    search_preview_target: Option<SearchPreviewTarget>,
    recent_files: Vec<PathBuf>,
    last_search_click: Option<(usize, Instant)>,
    last_refresh_error: Option<String>,
    has_refresh_snapshot: bool,
    quit_confirmation: Option<QuitConfirmation>,
    should_quit: bool,
    #[cfg(feature = "agent-observability")]
    agent_runtime: Option<AgentRuntime>,
    #[cfg(feature = "agent-observability")]
    agent_state: AgentState,
    #[cfg(feature = "agent-observability")]
    agent_view: AgentViewState,
    #[cfg(feature = "agent-observability")]
    agent_generation: u64,
    #[cfg(feature = "agent-observability")]
    agent_selection: Option<crate::agent::SessionKey>,
    #[allow(dead_code)] // Consumed by the following App/navigation integration node.
    pub(crate) navigation_settings: NavigationSettings,
    pub(crate) navigation_config_warning: Option<String>,
    navigation_runtime: NavigationRuntime,
    navigation_source: Option<Arc<NavigationSource>>,
    navigation_document_version: DocumentVersion,
    navigation_generation: u64,
    navigation_invocation: Option<NavigationInvocation>,
    pending_navigation_stage: Option<PendingNavigationStage>,
    pub(crate) navigation_picker: Option<NavigationPickerState>,
    pub(crate) navigation_status: Option<NavigationStatus>,
    navigation_caret: NavigationCaret,
    navigation_hover_highlight: Option<SourceRange>,
    navigation_target_highlight: Option<SourceRange>,
    navigation_back: VecDeque<NavigationHistoryEntry>,
    navigation_forward: VecDeque<NavigationHistoryEntry>,
}

impl App {
    pub fn new(path: PathBuf) -> Result<Self> {
        Self::with_preview_registry(path, PreviewRegistry::with_builtins())
    }

    pub fn with_preview_registry(path: PathBuf, preview_registry: PreviewRegistry) -> Result<Self> {
        Self::with_options(path, preview_registry, AppOptions::default())
    }

    pub fn with_options(
        path: PathBuf,
        preview_registry: PreviewRegistry,
        options: AppOptions,
    ) -> Result<Self> {
        Self::with_preview_registry_scan_limit_and_options(
            path,
            preview_registry,
            tree::DEFAULT_MAX_ENTRIES,
            options,
        )
    }

    #[cfg(test)]
    fn with_preview_registry_and_scan_limit(
        path: PathBuf,
        preview_registry: PreviewRegistry,
        scan_entry_limit: usize,
    ) -> Result<Self> {
        Self::with_preview_registry_scan_limit_and_options(
            path,
            preview_registry,
            scan_entry_limit,
            AppOptions::default(),
        )
    }

    fn with_preview_registry_scan_limit_and_options(
        path: PathBuf,
        preview_registry: PreviewRegistry,
        scan_entry_limit: usize,
        mut options: AppOptions,
    ) -> Result<Self> {
        let requested_root = path
            .canonicalize()
            .with_context(|| format!("cannot open {}", path.display()))?;
        if !requested_root.is_dir() {
            anyhow::bail!("{} is not a directory", requested_root.display());
        }

        // Keep the user-selected directory as the All Files boundary. Git may
        // still belong to a containing repository; the worker rebases that
        // repository's status and diff paths into this workspace.
        let root = requested_root;

        if let Err(error) = options.navigation.revalidate(&root) {
            options.navigation = NavigationSettings::disabled();
            options.navigation_config_warning = Some(format!("{error:#}"));
        }

        let runtime = WorkerRuntime::start(root.clone(), preview_registry.clone())?;
        let search_runtime = SearchRuntime::start(root.clone())?;
        let navigation_runtime =
            NavigationRuntime::start(root.clone(), options.navigation.clone())?;
        let mut app = Self {
            root,
            repo: None,
            all_entries: Vec::new(),
            changed_entries: Vec::new(),
            all_files_truncated: false,
            git_changes_truncated: false,
            tree_state: ListState::default(),
            tree_scope: TreeScope::AllFiles,
            focused_pane: FocusPane::Tree,
            content_lines: vec![
                "Loading workspace…".to_owned(),
                String::new(),
                "The file tree and repository state are being scanned in the background."
                    .to_owned(),
            ],
            content_highlights: Vec::new(),
            content_scroll: 0,
            content_horizontal_scroll: 0,
            content_selection: None,
            pending_clipboard_text: None,
            clipboard_status: None,
            content_mode: ContentMode::Info,
            content_provider: None,
            content_show_line_numbers: false,
            content_diff_lines: Vec::new(),
            content_identity: None,
            content_fold_source: FoldSource::None,
            content_fold_regions: Vec::new(),
            content_structure: StructureSnapshot::unavailable(),
            content_collapsed_folds: HashSet::new(),
            content_cursor_line: 0,
            content_successful: false,
            content_projection_width: 0,
            fold_cache: VecDeque::new(),
            branch: None,
            changed_count: 0,
            total_repository_count: 0,
            dirty_repository_count: 0,
            repository_error_count: 0,
            repository_graph_truncated: false,
            last_error: None,
            ui_regions: UiRegions::default(),
            tree_panel_width: None,
            tree_resize_dragging: false,
            preview_registry,
            repo_graph: None,
            all_files_selection: None,
            git_changes_selection: None,
            pending_all_scope_path: None,
            pending_all_scope_navigation: false,
            pending_git_scope_path: None,
            pending_git_scope_fallback: None,
            all_files_expansion: HashMap::new(),
            unloaded_directories: HashSet::new(),
            loading_directories: HashSet::new(),
            tree_epoch: 0,
            git_changes_expansion: HashMap::new(),
            reviewed_change_versions: HashMap::new(),
            current_diff_path: None,
            pending_diff_path: None,
            visible_rows: Vec::new(),
            visible_changed_entries: Vec::new(),
            git_rows: Vec::new(),
            visible_git_rows: Vec::new(),
            scan_entry_limit,
            runtime,
            refresh_requests: RequestGeneration::default(),
            content_requests: RequestGeneration::default(),
            navigation_preview_requests: RequestGeneration::default(),
            search_runtime,
            search: None,
            search_list_state: ListState::default(),
            search_sessions: [None, None],
            preview_find: None,
            search_generation: 0,
            search_preview_target: None,
            recent_files: Vec::new(),
            last_search_click: None,
            last_refresh_error: None,
            has_refresh_snapshot: false,
            quit_confirmation: None,
            should_quit: false,
            #[cfg(feature = "agent-observability")]
            agent_runtime: None,
            #[cfg(feature = "agent-observability")]
            agent_state: AgentState::new(0),
            #[cfg(feature = "agent-observability")]
            agent_view: AgentViewState::default(),
            #[cfg(feature = "agent-observability")]
            agent_generation: 0,
            #[cfg(feature = "agent-observability")]
            agent_selection: None,
            navigation_settings: options.navigation,
            navigation_config_warning: options.navigation_config_warning,
            navigation_runtime,
            navigation_source: None,
            navigation_document_version: DocumentVersion(0),
            navigation_generation: 0,
            navigation_invocation: None,
            pending_navigation_stage: None,
            navigation_picker: None,
            navigation_status: None,
            navigation_caret: NavigationCaret {
                point: SourcePosition { line: 0, byte: 0 },
                preferred_display_column: 0,
            },
            navigation_hover_highlight: None,
            navigation_target_highlight: None,
            navigation_back: VecDeque::new(),
            navigation_forward: VecDeque::new(),
        };
        app.request_refresh(false);
        Ok(app)
    }

    /// Attach an explicitly constructed Agent runtime. Production construction
    /// keeps the registry empty; synthetic harnesses inject services here
    /// without adding hidden CLI flags or test-only environment behavior.
    #[cfg(feature = "agent-observability")]
    pub fn attach_agent_runtime(
        &mut self,
        runtime: AgentRuntime,
        selector: WorkspaceSelector,
    ) -> Result<(), RuntimeBackpressure> {
        self.agent_generation = self.agent_generation.saturating_add(1);
        self.agent_state.select_generation(self.agent_generation);
        self.agent_view = self.agent_state.view();
        runtime.submit(AgentRuntimeRequest::SelectWorkspace {
            generation: self.agent_generation,
            selector,
        })?;
        runtime.submit(AgentRuntimeRequest::RefreshProviders {
            generation: self.agent_generation,
        })?;
        self.agent_runtime = Some(runtime);
        Ok(())
    }

    #[cfg(feature = "agent-observability")]
    pub fn agent_view(&self) -> &AgentViewState {
        &self.agent_view
    }

    #[cfg(feature = "agent-observability")]
    pub fn selected_agent_session(&self) -> Option<&AgentViewSession> {
        self.tree_state
            .selected()
            .and_then(|index| self.agent_view.sessions.get(index))
    }

    pub fn register_preview_provider<P>(&mut self, provider: P)
    where
        P: PreviewProvider + 'static,
    {
        self.preview_registry.register(provider);
        self.runtime
            .update_preview_registry(self.preview_registry.clone());
        if self.content_mode == ContentMode::Preview {
            self.load_selected_preview();
        }
    }

    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> io::Result<()> {
        while !self.should_quit {
            self.poll_background();
            terminal.draw(|frame| ui::draw(frame, self))?;

            let poll_interval = if self.search.is_some() {
                Duration::from_millis(50)
            } else {
                Duration::from_millis(250)
            };
            if event::poll(poll_interval)? {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        self.handle_key(key);
                        self.flush_clipboard_request();
                    }
                    Event::Mouse(mouse) => {
                        self.handle_mouse(mouse);
                        self.flush_clipboard_request();
                    }
                    _ => {}
                }
            }
        }
        #[cfg(feature = "agent-observability")]
        if let Some(runtime) = &self.agent_runtime {
            runtime.begin_shutdown();
        }
        Ok(())
    }

    pub fn visible_entries(&self) -> &[FileEntry] {
        match self.tree_scope {
            TreeScope::AllFiles => &self.visible_rows,
            TreeScope::GitChanges => &self.visible_changed_entries,
            #[cfg(feature = "agent-observability")]
            TreeScope::Agents => &[],
        }
    }

    pub fn visible_git_rows(&self) -> &[GitTreeRow] {
        &self.visible_git_rows
    }

    pub fn tree_row_count(&self) -> usize {
        match self.tree_scope {
            TreeScope::AllFiles => self.visible_rows.len(),
            TreeScope::GitChanges => self.visible_git_rows.len(),
            #[cfg(feature = "agent-observability")]
            TreeScope::Agents => self.agent_view.sessions.len(),
        }
    }

    pub(crate) fn tree_panel_width(&self, total_width: u16) -> u16 {
        ui::tree_panel_width(total_width, self.tree_panel_width)
    }

    pub(crate) const fn tree_resize_dragging(&self) -> bool {
        self.tree_resize_dragging
    }

    pub fn scope_entry_count(&self) -> usize {
        match self.tree_scope {
            TreeScope::AllFiles => self.all_entries.len(),
            TreeScope::GitChanges => self.changed_count,
            #[cfg(feature = "agent-observability")]
            TreeScope::Agents => self.agent_view.known_count,
        }
    }

    pub const fn scope_is_truncated(&self) -> bool {
        match self.tree_scope {
            TreeScope::AllFiles => self.all_files_truncated,
            TreeScope::GitChanges => self.git_changes_truncated,
            #[cfg(feature = "agent-observability")]
            TreeScope::Agents => self.agent_view.truncated,
        }
    }

    pub fn scope_has_unloaded_directories(&self) -> bool {
        self.tree_scope == TreeScope::AllFiles && !self.unloaded_directories.is_empty()
    }

    pub fn selected_entry(&self) -> Option<&FileEntry> {
        let index = self.tree_state.selected()?;
        match self.tree_scope {
            TreeScope::AllFiles => self.visible_rows.get(index),
            TreeScope::GitChanges => self.visible_git_rows.get(index)?.file_entry(),
            #[cfg(feature = "agent-observability")]
            TreeScope::Agents => None,
        }
    }

    pub fn selected_git_row(&self) -> Option<&GitTreeRow> {
        (self.tree_scope == TreeScope::GitChanges)
            .then(|| self.tree_state.selected())
            .flatten()
            .and_then(|index| self.visible_git_rows.get(index))
    }

    pub(crate) fn directory_is_expanded(&self, entry: &FileEntry) -> bool {
        if !entry.is_dir {
            return false;
        }
        match self.tree_scope {
            TreeScope::AllFiles => self
                .all_files_expansion
                .get(&entry.relative)
                .copied()
                .unwrap_or(false),
            TreeScope::GitChanges => self
                .selected_git_row()
                .map(|row| self.git_row_is_expanded(row))
                .unwrap_or(true),
            #[cfg(feature = "agent-observability")]
            TreeScope::Agents => false,
        }
    }

    pub(crate) fn directory_is_loading(&self, entry: &FileEntry) -> bool {
        entry.is_dir && self.loading_directories.contains(&entry.relative)
    }

    pub fn selected_relative_path(&self) -> Option<PathBuf> {
        match self.tree_scope {
            TreeScope::AllFiles => self.selected_entry().map(|entry| entry.relative.clone()),
            TreeScope::GitChanges => self.selected_git_row().and_then(|row| match &row.kind {
                GitRowKind::Change(_) => row.file_entry().map(|entry| entry.relative.clone()),
                GitRowKind::Pointer(change) => Some(change.path.relative.clone()),
                GitRowKind::Directory => row.file_entry().map(|entry| entry.relative.clone()),
                GitRowKind::Repository { .. } | GitRowKind::Issue(_) => None,
            }),
            #[cfg(feature = "agent-observability")]
            TreeScope::Agents => None,
        }
    }

    pub(crate) fn git_row_is_expanded(&self, row: &GitTreeRow) -> bool {
        row.is_container()
            && self
                .git_changes_expansion
                .get(&row.identity)
                .copied()
                .unwrap_or(true)
    }

    pub(crate) fn git_row_review_state(&self, row: &GitTreeRow) -> Option<DiffReviewState> {
        let GitRowKind::Change(change) = &row.kind else {
            return None;
        };
        let version = &self
            .repo_graph
            .as_ref()?
            .change_details(&change.path)?
            .version;
        Some(match self.reviewed_change_versions.get(&change.path) {
            None => DiffReviewState::Unreviewed,
            Some(reviewed) if reviewed == version => DiffReviewState::Reviewed,
            Some(_) => DiffReviewState::ChangedAfterReview,
        })
    }

    pub(crate) fn git_row_diff_stat(&self, row: &GitTreeRow) -> Option<DiffStat> {
        let change = match &row.kind {
            GitRowKind::Change(change) | GitRowKind::Pointer(change) => change,
            GitRowKind::Repository { .. } | GitRowKind::Directory | GitRowKind::Issue(_) => {
                return None;
            }
        };
        self.repo_graph
            .as_ref()?
            .change_details(&change.path)?
            .diff_stat
    }

    pub(crate) fn review_progress(&self) -> ReviewProgress {
        self.git_rows
            .iter()
            .fold(ReviewProgress::default(), |mut progress, row| {
                let Some(state) = self.git_row_review_state(row) else {
                    return progress;
                };
                progress.total = progress.total.saturating_add(1);
                match state {
                    DiffReviewState::Reviewed => {
                        progress.reviewed = progress.reviewed.saturating_add(1);
                    }
                    DiffReviewState::ChangedAfterReview => {
                        progress.changed_after_review =
                            progress.changed_after_review.saturating_add(1);
                    }
                    DiffReviewState::Unreviewed => {}
                }
                progress
            })
    }

    pub fn selected_content_label(&self) -> String {
        if let Some(identity) = self
            .content_identity
            .as_ref()
            .filter(|identity| identity.workspace_path().is_none())
        {
            return identity.display_label();
        }
        if let Some(result) = self.selected_search_result() {
            return result.line_number.map_or_else(
                || display_workspace_path(&result.path),
                |line| format!("{}:{line}", display_workspace_path(&result.path)),
            );
        }
        match self.tree_scope {
            TreeScope::AllFiles => self
                .selected_entry()
                .map(|entry| entry.relative.display().to_string())
                .unwrap_or_default(),
            TreeScope::GitChanges => self
                .selected_git_row()
                .map(|row| row.label.clone())
                .unwrap_or_default(),
            #[cfg(feature = "agent-observability")]
            TreeScope::Agents => self
                .selected_agent_session()
                .map(AgentViewSession::short_key)
                .unwrap_or_default(),
        }
    }

    /// Canonical real path of the selected All Files symbolic link, if any.
    ///
    /// Returned for the content header so a followed link shows exactly where
    /// it points on disk. Only the interactive All Files view resolves links,
    /// so Git Changes and dependency previews never surface a resolved path.
    pub fn selected_symlink_real_path(&self) -> Option<PathBuf> {
        if self.tree_scope != TreeScope::AllFiles {
            return None;
        }
        let entry = self.selected_entry()?;
        entry.symlink_target.as_ref()?;
        let absolute = self.root.join(&entry.relative);
        Some(absolute.canonicalize().unwrap_or(absolute))
    }

    /// Compute the path to copy for the selected entry.
    ///
    /// - `resolve=false`: relative path (the link path for symlinks), suitable
    ///   for pasting relative to the workspace root.
    /// - `resolve=true`: the real/absolute path. All Files symlinks resolve to
    ///   their target on disk (canonicalize follows the link); everything else
    ///   — including Git Changes entries, which never carry a symlink target —
    ///   resolves to the absolute path under the workspace root.
    ///
    /// The returned `PathBuf` does **not** include a trailing slash for
    /// directories; callers add one when formatting the copy string.
    fn selected_copy_path(&self, resolve: bool) -> Option<PathBuf> {
        let entry = self.selected_entry()?;
        let relative = &entry.relative;
        if !resolve {
            return Some(relative.clone());
        }
        let absolute = self.root.join(relative);
        if entry.symlink_target.is_some() && self.tree_scope == TreeScope::AllFiles {
            // Real path for All Files symlinks (canonicalize follows the link).
            Some(absolute.canonicalize().unwrap_or(absolute))
        } else {
            // Absolute path; Git Changes entries never resolve a symlink target,
            // and ordinary files/directories already have a truthful absolute
            // path, so there is nothing to canonicalize.
            Some(absolute)
        }
    }

    pub fn selected_content_title(&self) -> &'static str {
        if self
            .content_identity
            .as_ref()
            .is_some_and(|identity| identity.workspace_path().is_none())
        {
            return "Dependency Source";
        }
        if let Some(result) = self.selected_search_result() {
            return if result.is_dir {
                "Directory"
            } else {
                "Preview"
            };
        }
        #[cfg(feature = "agent-observability")]
        if self.tree_scope == TreeScope::Agents {
            return "Agent session";
        }
        let Some(row) = self.selected_git_row() else {
            return if self.selected_entry().is_some_and(|entry| entry.is_dir) {
                "Directory"
            } else {
                self.content_mode.title()
            };
        };
        match row.kind {
            GitRowKind::Repository { .. } => "Repository",
            GitRowKind::Directory => "Directory",
            GitRowKind::Pointer(_) => "Submodule pointer",
            GitRowKind::Issue(_) => "Repository error",
            GitRowKind::Change(_) => self.content_mode.title(),
        }
    }

    pub(crate) fn content_is_focused(&self) -> bool {
        self.search.is_none() && self.focused_pane == FocusPane::Content
    }

    pub fn content_selection_range(&self, line: usize) -> Option<Range<usize>> {
        let selection = self.content_selection?;
        let (start, end) = selection.normalized();
        if start == end || line < start.line || line > end.line {
            return None;
        }
        let content = self.content_lines.get(line)?;
        let start_byte = if line == start.line { start.byte } else { 0 };
        let end_byte = if line == end.line {
            end.byte
        } else {
            content.len()
        };
        Some(start_byte.min(content.len())..end_byte.min(content.len()))
    }

    pub(crate) fn content_visual_rows(&self, width: u16) -> Vec<ContentVisualRow> {
        let wrap_content = self.content_wraps_lines();
        let gutter_width = self.content_gutter_width();
        let text_width = usize::from(width).saturating_sub(gutter_width).max(1);
        let mut rows = Vec::new();
        let mut line_index = 0usize;
        let mut region_index = 0usize;
        while line_index < self.content_lines.len() {
            while self
                .content_fold_regions
                .get(region_index)
                .is_some_and(|region| region.start_line < line_index)
            {
                region_index += 1;
            }
            let region = self
                .content_fold_regions
                .get(region_index)
                .filter(|region| region.start_line == line_index);
            let collapsed =
                region.is_some_and(|region| self.content_collapsed_folds.contains(&region.anchor));
            let marker = match (region.is_some(), collapsed) {
                (true, true) => FoldVisualMarker::Collapsed,
                (true, false) => FoldVisualMarker::Expanded,
                (false, _) => FoldVisualMarker::None,
            };
            let line = &self.content_lines[line_index];
            let tab_origin = self.content_tab_origin(line_index);
            let ranges = if wrap_content {
                wrap_line_ranges(line, text_width, tab_origin)
            } else {
                std::iter::once(0..line.len()).collect()
            };
            for (index, byte_range) in ranges.into_iter().enumerate() {
                rows.push(ContentVisualRow {
                    line_index,
                    byte_range,
                    continuation: index > 0,
                    tab_origin: if index == 0 { tab_origin } else { 0 },
                    summary: None,
                    synthetic: false,
                    fold_marker: if index == 0 {
                        marker
                    } else {
                        FoldVisualMarker::None
                    },
                });
            }
            if collapsed {
                let hidden_lines = region
                    .map(|region| region.end_line.saturating_sub(region.start_line))
                    .unwrap_or_default();
                let summary = fold_summary(hidden_lines, text_width);
                let can_append = rows.last().is_some_and(|row| {
                    let segment_width = line
                        .get(row.byte_range.clone())
                        .map_or(0, |segment| expand_tabs(segment, 0, row.tab_origin).1);
                    segment_width.saturating_add(UnicodeWidthStr::width(summary.as_str()))
                        <= text_width
                });
                if can_append {
                    if let Some(row) = rows.last_mut() {
                        row.summary = Some(summary);
                    }
                } else {
                    rows.push(ContentVisualRow {
                        line_index,
                        byte_range: line.len()..line.len(),
                        continuation: true,
                        tab_origin: 0,
                        summary: Some(summary),
                        synthetic: true,
                        fold_marker: FoldVisualMarker::None,
                    });
                }
                line_index =
                    region.map_or(line_index + 1, |region| region.end_line.saturating_add(1));
                continue;
            }
            line_index += 1;
        }
        rows
    }

    pub(crate) fn effective_content_scroll(&self, row_count: usize) -> usize {
        self.content_scroll.min(row_count.saturating_sub(1))
    }

    pub(crate) fn prepare_content_width(&mut self, width: u16) {
        let width = width.max(1);
        if self.content_projection_width == 0 {
            self.content_projection_width = width;
            return;
        }
        if self.content_projection_width == width {
            return;
        }
        let old_rows = self.content_visual_rows(self.content_projection_width);
        let effective_scroll = self.effective_content_scroll(old_rows.len());
        let top = old_rows
            .get(effective_scroll)
            .map(|row| (row.line_index, row.byte_range.start, row.synthetic));
        self.content_projection_width = width;
        if let Some((line, byte, synthetic)) = top {
            let rows = self.content_visual_rows(width);
            let line_len = self.content_lines.get(line).map_or(0, String::len);
            self.content_scroll = rows
                .iter()
                .position(|row| viewport_row_matches(row, line, byte, synthetic, line_len))
                .or_else(|| {
                    if synthetic {
                        rows.iter()
                            .rposition(|row| row.line_index == line && !row.synthetic)
                    } else {
                        None
                    }
                })
                .or_else(|| rows.iter().position(|row| row.line_index == line))
                .unwrap_or(effective_scroll.min(rows.len().saturating_sub(1)));
        }
        let row_count = self.content_visual_rows(width).len();
        self.content_scroll = self.effective_content_scroll(row_count);
    }

    pub(crate) const fn content_wraps_lines(&self) -> bool {
        matches!(self.content_mode, ContentMode::Diff | ContentMode::Preview)
    }

    pub(crate) fn effective_content_horizontal_scroll(&self) -> usize {
        if self.content_wraps_lines() {
            0
        } else {
            self.content_horizontal_scroll.min(u16::MAX as usize)
        }
    }

    pub(crate) fn content_line_number_width(&self) -> usize {
        if self.content_mode == ContentMode::Diff {
            line_number_width(&self.content_diff_lines)
        } else {
            self.content_lines.len().max(1).to_string().len()
        }
    }

    pub(crate) fn content_gutter_width(&self) -> usize {
        if !self.content_show_line_numbers {
            return 0;
        }
        let number_width = self.content_line_number_width();
        if self.content_mode == ContentMode::Diff {
            number_width.saturating_mul(2).saturating_add(4)
        } else {
            number_width.saturating_add(3)
        }
    }

    fn content_tab_origin(&self, line_index: usize) -> usize {
        usize::from(
            self.content_mode == ContentMode::Diff
                && self.content_diff_lines.get(line_index).is_some_and(|line| {
                    matches!(
                        line.kind,
                        DiffLineKind::Addition | DiffLineKind::Deletion | DiffLineKind::Context
                    )
                }),
        )
    }

    pub fn selected_preview_text(&self) -> Option<String> {
        (self.content_mode == ContentMode::Preview)
            .then(|| self.selected_content_text())
            .flatten()
    }

    pub fn selected_content_text(&self) -> Option<String> {
        let selection = self.content_selection?;
        let (start, end) = selection.normalized();
        if start == end {
            return None;
        }

        let mut selected = String::new();
        for line_index in start.line..=end.line {
            let line = self.content_lines.get(line_index)?;
            let start_byte = if line_index == start.line {
                start.byte.min(line.len())
            } else {
                0
            };
            let end_byte = if line_index == end.line {
                end.byte.min(line.len())
            } else {
                line.len()
            };
            selected.push_str(line.get(start_byte..end_byte)?);
            if line_index < end.line {
                selected.push('\n');
            }
        }
        (!selected.is_empty()).then_some(selected)
    }

    /// Apply one terminal key event to the same path used by the interactive loop.
    pub fn handle_key(&mut self, key: KeyEvent) {
        self.navigation_hover_highlight = None;
        if self.navigation_picker.is_some() {
            self.quit_confirmation = None;
            self.handle_navigation_picker_key(key);
            return;
        }
        let copy_key = matches!(key.code, KeyCode::Char('c' | 'C'));
        if copy_key && key.modifiers.contains(KeyModifiers::SUPER) {
            self.quit_confirmation = None;
            self.queue_selected_preview_copy();
            return;
        }
        if copy_key && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.quit_confirmation = None;
            if key.modifiers.contains(KeyModifiers::SHIFT) || self.selected_content_text().is_some()
            {
                self.queue_selected_preview_copy();
            } else {
                self.should_quit = true;
            }
            return;
        }
        if self.preview_find.is_some() {
            self.quit_confirmation = None;
            self.handle_preview_find_key(key);
            return;
        }
        if self.search.is_some() {
            self.quit_confirmation = None;
            self.handle_search_key(key);
            return;
        }
        if key.code == KeyCode::Char('q') && key.modifiers == KeyModifiers::NONE {
            self.request_quit(QuitKey::Q);
            return;
        }
        if key.code == KeyCode::Esc {
            if self.navigation_invocation.is_some() || self.pending_navigation_stage.is_some() {
                self.cancel_pending_navigation();
                return;
            }
            self.request_quit(QuitKey::Escape);
            return;
        }
        self.quit_confirmation = None;

        match (key.code, key.modifiers) {
            (KeyCode::Char('d' | 'D'), KeyModifiers::CONTROL) => {
                self.request_semantic_navigation(NavigationOperation::Definition);
            }
            (KeyCode::Char('r' | 'R'), KeyModifiers::CONTROL) => {
                self.request_semantic_navigation(NavigationOperation::References);
            }
            (KeyCode::Char('o' | 'O'), KeyModifiers::CONTROL) => {
                self.request_semantic_navigation(NavigationOperation::Implementations);
            }
            (KeyCode::Char('s' | 'S'), KeyModifiers::CONTROL) => self.open_document_symbols(),
            (KeyCode::Left, KeyModifiers::ALT) => {
                self.navigate_history(NavigationHistoryIntent::Back);
            }
            (KeyCode::Right, KeyModifiers::ALT) => {
                self.navigate_history(NavigationHistoryIntent::Forward);
            }
            (KeyCode::Char('/'), KeyModifiers::NONE) => self.open_search(SearchMode::Files),
            (KeyCode::Char('p' | 'P'), KeyModifiers::CONTROL) => {
                self.open_search(SearchMode::Files);
            }
            (KeyCode::Char('f' | 'F'), modifiers)
                if modifiers == KeyModifiers::CONTROL | KeyModifiers::SHIFT =>
            {
                self.open_search(SearchMode::Text);
            }
            (KeyCode::Char('t' | 'T'), KeyModifiers::CONTROL) => {
                self.open_search(SearchMode::Text);
            }
            (KeyCode::Char('f' | 'F'), KeyModifiers::CONTROL) => {
                self.open_preview_find();
            }
            (KeyCode::Tab, _) => self.set_tree_scope(self.tree_scope.next()),
            (KeyCode::BackTab, _) => self.set_tree_scope(self.tree_scope.previous()),
            (KeyCode::Char('1'), KeyModifiers::NONE) => {
                self.set_tree_scope(TreeScope::AllFiles);
            }
            (KeyCode::Char('2'), KeyModifiers::NONE) => {
                self.set_tree_scope(TreeScope::GitChanges);
            }
            #[cfg(feature = "agent-observability")]
            (KeyCode::Char('3'), KeyModifiers::NONE) => {
                self.set_tree_scope(TreeScope::Agents);
            }
            (KeyCode::Char('h'), KeyModifiers::NONE) => self.focused_pane = FocusPane::Tree,
            (KeyCode::Char('l'), KeyModifiers::NONE) => self.focused_pane = FocusPane::Content,
            (KeyCode::Char('r'), _) => {
                #[cfg(feature = "agent-observability")]
                if self.tree_scope == TreeScope::Agents {
                    self.request_agent_refresh();
                } else {
                    self.request_refresh(self.tree_scope == TreeScope::GitChanges);
                }
                #[cfg(not(feature = "agent-observability"))]
                self.request_refresh(self.tree_scope == TreeScope::GitChanges);
            }
            (KeyCode::Char('p'), KeyModifiers::NONE) => self.load_selected_preview(),
            (KeyCode::Char('d'), KeyModifiers::NONE) => self.load_selected_diff(),
            (KeyCode::Char(' '), KeyModifiers::NONE) if self.content_mode == ContentMode::Diff => {
                self.toggle_current_diff_review();
            }
            (KeyCode::Char('n'), KeyModifiers::NONE) if self.content_mode == ContentMode::Diff => {
                self.select_changed(1);
            }
            (KeyCode::Char('N'), _) if self.content_mode == ContentMode::Diff => {
                self.select_changed(-1);
            }
            (KeyCode::Char('y'), KeyModifiers::NONE) => {
                self.queue_selected_path_copy(false);
            }
            (KeyCode::Char('Y'), _) => {
                self.queue_selected_path_copy(true);
            }
            _ => match self.focused_pane {
                FocusPane::ScopeTabs => self.handle_scope_tabs_key(key),
                FocusPane::Tree => self.handle_tree_key(key),
                FocusPane::Content => self.handle_content_key(key),
            },
        }
    }

    fn request_quit(&mut self, key: QuitKey) {
        let now = Instant::now();
        if self
            .quit_confirmation
            .is_some_and(|confirmation| confirmation.key == key && now <= confirmation.deadline)
        {
            self.should_quit = true;
            self.quit_confirmation = None;
            return;
        }
        self.quit_confirmation = Some(QuitConfirmation {
            key,
            deadline: now + QUIT_CONFIRM_WINDOW,
        });
    }

    pub(crate) fn quit_confirmation_message(&self) -> Option<&'static str> {
        let confirmation = self
            .quit_confirmation
            .filter(|confirmation| Instant::now() <= confirmation.deadline)?;
        Some(match confirmation.key {
            QuitKey::Q => "Press q again to quit · Ctrl+C quits immediately",
            QuitKey::Escape => "Press Esc again to quit · Ctrl+C quits immediately",
        })
    }

    pub fn open_search(&mut self, mode: SearchMode) {
        if self.search.is_some() {
            self.set_search_mode(mode);
            return;
        }
        let restore = self.capture_search_restore();
        // Search previews temporarily replace ContentSnapshot state. Retire
        // any semantic request before that identity can change, while keeping
        // its visible pre-search state in the restore value above.
        self.cancel_pending_navigation();
        self.activate_search(mode, restore);
    }

    fn capture_search_restore(&self) -> SearchRestore {
        let width = self.ui_regions.content_inner.width.max(1);
        let rows = self.content_visual_rows(width);
        let effective_scroll = self.effective_content_scroll(rows.len());
        let top = rows.get(effective_scroll);
        SearchRestore {
            focused_pane: self.focused_pane,
            tree_scope: self.tree_scope,
            tree_state: self.tree_state,
            all_files_selection: self.all_files_selection.clone(),
            git_changes_selection: self.git_changes_selection.clone(),
            pending_all_scope_path: self.pending_all_scope_path.clone(),
            pending_all_scope_navigation: self.pending_all_scope_navigation,
            pending_git_scope_path: self.pending_git_scope_path.clone(),
            pending_git_scope_fallback: self.pending_git_scope_fallback.clone(),
            content_lines: self.content_lines.clone(),
            content_highlights: self.content_highlights.clone(),
            content_viewport: ContentViewportRestore {
                line: top.map(|row| row.line_index),
                byte_start: top.map_or(0, |row| row.byte_range.start),
                synthetic: top.is_some_and(|row| row.synthetic),
                effective_scroll,
            },
            content_horizontal_scroll: self.content_horizontal_scroll,
            content_selection: self.content_selection,
            clipboard_status: self.clipboard_status.clone(),
            content_mode: self.content_mode,
            content_provider: self.content_provider.clone(),
            content_show_line_numbers: self.content_show_line_numbers,
            content_diff_lines: self.content_diff_lines.clone(),
            content_identity: self.content_identity.clone(),
            content_fold_source: self.content_fold_source,
            content_fold_regions: self.content_fold_regions.clone(),
            content_structure: self.content_structure.clone(),
            content_collapsed_folds: self.content_collapsed_folds.clone(),
            content_cursor_line: self.content_cursor_line,
            content_successful: self.content_successful,
            content_was_loading: self.is_content_loading(),
            last_error: self.last_error.clone(),
            navigation_source: self.navigation_source.clone(),
            navigation_document_version: self.navigation_document_version,
            navigation_caret: self.navigation_caret,
            navigation_target_highlight: self.navigation_target_highlight,
            navigation_status: self.navigation_status.clone(),
            navigation_back: self.navigation_back.clone(),
            navigation_forward: self.navigation_forward.clone(),
        }
    }

    fn activate_search(&mut self, mode: SearchMode, restore: SearchRestore) {
        self.preview_find = None;
        self.clear_content_selection();
        let saved = self.search_sessions[mode.index()].take();
        let is_new = saved.is_none();
        let saved = saved.unwrap_or(SearchSession {
            query: String::new(),
            cursor: 0,
            results: Vec::new(),
            options: SearchOptions::default(),
            truncated: false,
            scanned_files: 0,
            error: None,
            list_state: ListState::default(),
            selected_identity: None,
            tree_epoch: self.tree_epoch,
            needs_refresh: false,
        });
        let should_refresh = saved.needs_refresh || saved.tree_epoch != self.tree_epoch;
        let selection_hint = saved.selected_identity.map(|identity| SearchSelectionHint {
            identity,
            index: saved.list_state.selected().unwrap_or(0),
            offset: saved.list_state.offset(),
        });
        self.search_list_state = saved.list_state;
        self.search = Some(SearchState {
            mode,
            query: saved.query,
            cursor: saved.cursor,
            results: saved.results,
            options: saved.options,
            searching: false,
            indexing: false,
            truncated: saved.truncated,
            scanned_files: saved.scanned_files,
            error: saved.error,
            generation: 0,
            due: None,
            selection_hint: should_refresh.then_some(selection_hint).flatten(),
            restore,
        });
        self.last_search_click = None;
        if is_new || should_refresh {
            self.rebuild_search_results();
        } else {
            self.preview_search_selection();
        }
    }

    fn store_search_session(&mut self, search: &SearchState) {
        let selected_identity = self
            .search_list_state
            .selected()
            .and_then(|index| search.results.get(index))
            .map(SearchResultIdentity::from)
            .or_else(|| {
                search
                    .selection_hint
                    .as_ref()
                    .map(|hint| hint.identity.clone())
            });
        self.search_sessions[search.mode.index()] = Some(SearchSession {
            query: search.query.clone(),
            cursor: search.cursor,
            results: search.results.clone(),
            options: search.options,
            truncated: search.truncated,
            scanned_files: search.scanned_files,
            error: search.error.clone(),
            list_state: self.search_list_state,
            selected_identity,
            tree_epoch: self.tree_epoch,
            needs_refresh: search.searching || search.indexing || search.due.is_some(),
        });
    }

    pub const fn search_is_active(&self) -> bool {
        self.search.is_some()
    }

    pub fn selected_search_result(&self) -> Option<&SearchResult> {
        let index = self.search_list_state.selected()?;
        self.search.as_ref()?.results.get(index)
    }

    pub fn search_mode(&self) -> Option<SearchMode> {
        self.search.as_ref().map(|search| search.mode)
    }

    pub fn search_query(&self) -> Option<&str> {
        self.search.as_ref().map(|search| search.query.as_str())
    }

    pub fn search_results(&self) -> &[SearchResult] {
        self.search
            .as_ref()
            .map_or(&[], |search| search.results.as_slice())
    }

    pub fn search_error(&self) -> Option<&str> {
        self.search
            .as_ref()
            .and_then(|search| search.error.as_deref())
    }

    pub fn open_preview_find(&mut self) {
        if !matches!(self.content_mode, ContentMode::Preview | ContentMode::Diff) {
            self.clipboard_status =
                Some("Open a file preview or diff before using Ctrl+F".to_owned());
            return;
        }
        self.search = None;
        self.search_runtime
            .cancel(self.search_generation.saturating_add(1));
        self.preview_find = Some(PreviewFindState::default());
        self.focused_pane = FocusPane::Content;
        self.clear_content_selection();
    }

    pub const fn preview_find_is_active(&self) -> bool {
        self.preview_find.is_some()
    }

    pub fn preview_find_query(&self) -> Option<&str> {
        self.preview_find.as_ref().map(|find| find.query.as_str())
    }

    pub fn preview_find_position(&self) -> Option<(usize, usize)> {
        let find = self.preview_find.as_ref()?;
        let count = find.matches.len();
        Some((
            usize::from(count > 0) + find.selected.min(count.saturating_sub(1)),
            count,
        ))
    }

    pub fn preview_find_highlights(&self, line: usize) -> Vec<HighlightSpan> {
        let Some(find) = self.preview_find.as_ref() else {
            return Vec::new();
        };
        find.matches
            .iter()
            .enumerate()
            .filter(|(_, found)| found.line == line)
            .map(|(index, found)| HighlightSpan {
                range: found.range.clone(),
                kind: if index == find.selected {
                    HighlightKind::Search
                } else {
                    HighlightKind::SearchMatch
                },
            })
            .collect()
    }

    pub(crate) fn navigation_highlights(&self, line: usize) -> Vec<HighlightSpan> {
        let mut highlights = Vec::with_capacity(2);
        if let Some(range) = self.navigation_target_highlight
            && range.start.line <= line
            && line <= range.end.line
            && let Some(text) = self.content_lines.get(line)
        {
            let start = if line == range.start.line {
                range.start.byte
            } else {
                0
            };
            let end = if line == range.end.line {
                range.end.byte
            } else {
                text.len()
            };
            if start < end && end <= text.len() {
                highlights.push(HighlightSpan {
                    range: start..end,
                    kind: HighlightKind::NavigationTarget,
                });
            }
        }
        if let Some(range) = self.navigation_hover_highlight
            && range.start.line <= line
            && line <= range.end.line
            && let Some(text) = self.content_lines.get(line)
        {
            let start = if line == range.start.line {
                range.start.byte
            } else {
                0
            };
            let end = if line == range.end.line {
                range.end.byte
            } else {
                text.len()
            };
            if start < end && end <= text.len() {
                highlights.push(HighlightSpan {
                    range: start..end,
                    kind: HighlightKind::NavigationHover,
                });
            }
        }
        highlights
    }

    fn handle_preview_find_key(&mut self, key: KeyEvent) {
        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) => self.preview_find = None,
            (KeyCode::Char('f' | 'F'), modifiers)
                if modifiers == KeyModifiers::CONTROL | KeyModifiers::SHIFT =>
            {
                self.preview_find = None;
                self.open_search(SearchMode::Text);
            }
            (KeyCode::Char('t' | 'T'), KeyModifiers::CONTROL) => {
                self.preview_find = None;
                self.open_search(SearchMode::Text);
            }
            (KeyCode::Char('p' | 'P'), KeyModifiers::CONTROL) => {
                self.preview_find = None;
                self.open_search(SearchMode::Files);
            }
            (KeyCode::Enter | KeyCode::F(3), modifiers)
                if modifiers.contains(KeyModifiers::SHIFT) =>
            {
                self.move_preview_find(-1);
            }
            (KeyCode::Enter | KeyCode::F(3) | KeyCode::Down, _) => {
                self.move_preview_find(1);
            }
            (KeyCode::Up, _) => self.move_preview_find(-1),
            (KeyCode::F(2), _) => {
                if let Some(find) = &mut self.preview_find {
                    find.case_sensitive = !find.case_sensitive;
                }
                self.rebuild_preview_find();
            }
            (KeyCode::Left, KeyModifiers::NONE) => self.move_preview_find_cursor(false),
            (KeyCode::Right, KeyModifiers::NONE) => self.move_preview_find_cursor(true),
            (KeyCode::Home, _) => {
                if let Some(find) = &mut self.preview_find {
                    find.cursor = 0;
                }
            }
            (KeyCode::End, _) => {
                if let Some(find) = &mut self.preview_find {
                    find.cursor = find.query.len();
                }
            }
            (KeyCode::Backspace, _) => self.delete_preview_find_character(false),
            (KeyCode::Delete, _) => self.delete_preview_find_character(true),
            (KeyCode::Char('u' | 'U'), KeyModifiers::CONTROL) => {
                if let Some(find) = &mut self.preview_find {
                    find.query.clear();
                    find.cursor = 0;
                }
                self.rebuild_preview_find();
            }
            (KeyCode::Char('w' | 'W'), KeyModifiers::CONTROL) => {
                self.delete_preview_find_word();
            }
            (KeyCode::Char(character), modifiers)
                if !modifiers.intersects(
                    KeyModifiers::CONTROL | KeyModifiers::SUPER | KeyModifiers::ALT,
                ) =>
            {
                if let Some(find) = &mut self.preview_find {
                    find.query.insert(find.cursor, character);
                    find.cursor += character.len_utf8();
                }
                self.rebuild_preview_find();
            }
            _ => {}
        }
    }

    fn rebuild_preview_find(&mut self) {
        let Some(find) = self.preview_find.as_ref() else {
            return;
        };
        let query = find.query.clone();
        let case_sensitive = find.case_sensitive;
        let matches = if query.is_empty() {
            Vec::new()
        } else {
            let pattern = RegexBuilder::new(&regex::escape(&query))
                .case_insensitive(!case_sensitive)
                .build()
                .expect("an escaped literal is always a valid regex");
            self.content_lines
                .iter()
                .enumerate()
                .flat_map(|(line, content)| {
                    pattern
                        .find_iter(content)
                        .map(move |found| PreviewFindMatch {
                            line,
                            range: found.range(),
                        })
                })
                .take(10_000)
                .collect()
        };
        if let Some(find) = &mut self.preview_find {
            find.matches = matches;
            find.selected = 0;
        }
        self.scroll_to_preview_find_match();
    }

    fn move_preview_find(&mut self, delta: isize) {
        let Some(find) = &mut self.preview_find else {
            return;
        };
        let count = find.matches.len();
        if count == 0 {
            return;
        }
        find.selected = if delta >= 0 {
            (find.selected + delta as usize) % count
        } else {
            (find.selected + count - delta.unsigned_abs() % count) % count
        };
        self.scroll_to_preview_find_match();
    }

    fn scroll_to_preview_find_match(&mut self) {
        let Some((line, byte)) = self
            .preview_find
            .as_ref()
            .and_then(|find| find.matches.get(find.selected))
            .map(|found| (found.line, found.range.start))
        else {
            return;
        };
        self.reveal_folded_line(line);
        self.scroll_to_logical_line(line, byte);
    }

    fn move_preview_find_cursor(&mut self, forward: bool) {
        let Some(find) = &mut self.preview_find else {
            return;
        };
        find.cursor = if forward {
            find.query[find.cursor..]
                .grapheme_indices(true)
                .nth(1)
                .map_or(find.query.len(), |(offset, _)| find.cursor + offset)
        } else {
            find.query[..find.cursor]
                .grapheme_indices(true)
                .next_back()
                .map_or(0, |(offset, _)| offset)
        };
    }

    fn delete_preview_find_character(&mut self, forward: bool) {
        let Some(find) = &mut self.preview_find else {
            return;
        };
        let boundary = if forward {
            find.query[find.cursor..]
                .grapheme_indices(true)
                .nth(1)
                .map_or(find.query.len(), |(offset, _)| find.cursor + offset)
        } else {
            find.query[..find.cursor]
                .grapheme_indices(true)
                .next_back()
                .map_or(find.cursor, |(offset, _)| offset)
        };
        if forward {
            find.query.drain(find.cursor..boundary);
        } else {
            find.query.drain(boundary..find.cursor);
            find.cursor = boundary;
        }
        self.rebuild_preview_find();
    }

    fn delete_preview_find_word(&mut self) {
        let Some(find) = &mut self.preview_find else {
            return;
        };
        let before = &find.query[..find.cursor];
        let trimmed = before.trim_end_matches(char::is_whitespace);
        let start = trimmed
            .char_indices()
            .rev()
            .find(|(_, character)| character.is_whitespace())
            .map_or(0, |(index, character)| index + character.len_utf8());
        find.query.drain(start..find.cursor);
        find.cursor = start;
        self.rebuild_preview_find();
    }

    fn handle_search_key(&mut self, key: KeyEvent) {
        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) => self.close_search(true),
            (KeyCode::Char('f' | 'F'), modifiers)
                if modifiers == KeyModifiers::CONTROL | KeyModifiers::SHIFT =>
            {
                self.set_search_mode(SearchMode::Text);
            }
            (KeyCode::Char('t' | 'T'), KeyModifiers::CONTROL) => {
                self.set_search_mode(SearchMode::Text);
            }
            (KeyCode::Char('p' | 'P'), KeyModifiers::CONTROL) => {
                self.set_search_mode(SearchMode::Files);
            }
            (KeyCode::Char('f' | 'F'), KeyModifiers::CONTROL) => {
                self.close_search(true);
                self.open_preview_find();
            }
            (KeyCode::F(2), _) => self.toggle_search_option(|options| {
                options.case_sensitive = !options.case_sensitive;
            }),
            (KeyCode::F(3), _) => self.toggle_search_option(|options| {
                options.whole_word = !options.whole_word;
            }),
            (KeyCode::F(4), _) => self.toggle_search_option(|options| {
                options.regex = !options.regex;
            }),
            (KeyCode::F(5), _) => self.toggle_search_option(|options| {
                options.include_ignored = !options.include_ignored;
            }),
            (KeyCode::Enter, _) => self.accept_search_selection(),
            (KeyCode::Down, _) => self.move_search_selection(1),
            (KeyCode::Up, _) => self.move_search_selection(-1),
            (KeyCode::PageDown, _) => self.move_search_selection(10),
            (KeyCode::PageUp, _) => self.move_search_selection(-10),
            (KeyCode::Left, KeyModifiers::NONE) => self.move_search_cursor(false),
            (KeyCode::Right, KeyModifiers::NONE) => self.move_search_cursor(true),
            (KeyCode::Home, _) => {
                if let Some(search) = &mut self.search {
                    search.cursor = 0;
                }
            }
            (KeyCode::End, _) => {
                if let Some(search) = &mut self.search {
                    search.cursor = search.query.len();
                }
            }
            (KeyCode::Backspace, _) => self.delete_search_character(false),
            (KeyCode::Delete, _) => self.delete_search_character(true),
            (KeyCode::Char('u' | 'U'), KeyModifiers::CONTROL) => {
                self.clear_search();
            }
            (KeyCode::Char('w' | 'W'), KeyModifiers::CONTROL) => {
                self.delete_search_word();
            }
            (KeyCode::Char(character), modifiers)
                if !modifiers.intersects(
                    KeyModifiers::CONTROL | KeyModifiers::SUPER | KeyModifiers::ALT,
                ) =>
            {
                if let Some(search) = &mut self.search {
                    search.query.insert(search.cursor, character);
                    search.cursor += character.len_utf8();
                    search.selection_hint = None;
                }
                self.rebuild_search_results();
            }
            _ => {}
        }
    }

    fn set_search_mode(&mut self, mode: SearchMode) {
        let Some(search) = self.search.take() else {
            self.open_search(mode);
            return;
        };
        if search.mode == mode {
            self.search = Some(search);
            return;
        }
        let restore = search.restore.clone();
        self.store_search_session(&search);
        self.search_generation = self.search_generation.saturating_add(1);
        self.search_runtime.cancel(self.search_generation);
        self.search_preview_target = None;
        self.last_search_click = None;
        self.activate_search(mode, restore);
    }

    fn clear_search(&mut self) {
        if let Some(search) = &mut self.search {
            search.query.clear();
            search.cursor = 0;
            search.results.clear();
            search.searching = false;
            search.indexing = false;
            search.truncated = false;
            search.scanned_files = 0;
            search.error = None;
            search.due = None;
            search.selection_hint = None;
        }
        self.search_generation = self.search_generation.saturating_add(1);
        self.search_runtime.cancel(self.search_generation);
        self.search_list_state = ListState::default();
        self.search_preview_target = None;
        self.rebuild_search_results();
    }

    fn toggle_search_option(&mut self, update: impl FnOnce(&mut SearchOptions)) {
        let Some(search) = &mut self.search else {
            return;
        };
        if search.mode != SearchMode::Text {
            return;
        }
        update(&mut search.options);
        search.selection_hint = None;
        self.rebuild_search_results();
    }

    fn rebuild_search_results(&mut self) {
        let Some(mode) = self.search.as_ref().map(|search| search.mode) else {
            return;
        };
        match mode {
            SearchMode::Files => self.rebuild_file_search_results(),
            SearchMode::Text => self.schedule_text_search(),
        }
    }

    fn rebuild_file_search_results(&mut self) {
        let Some(search) = &self.search else {
            return;
        };
        let query = search.query.clone();
        let case_sensitive = search.options.case_sensitive;
        let mut scored: Vec<(usize, SearchResult)> = if query.is_empty() {
            self.recent_files
                .iter()
                .filter_map(|path| {
                    self.all_entries
                        .iter()
                        .find(|entry| &entry.relative == path && !entry.is_dir)
                        .map(|entry| {
                            (
                                0,
                                SearchResult {
                                    path: entry.relative.clone(),
                                    is_dir: false,
                                    line_number: None,
                                    line: None,
                                    match_range: None,
                                    source_match_range: None,
                                },
                            )
                        })
                })
                .collect()
        } else {
            self.all_entries
                .iter()
                .filter_map(|entry| {
                    file_search_score(&entry.relative, &query, case_sensitive).map(|score| {
                        (
                            score,
                            SearchResult {
                                path: entry.relative.clone(),
                                is_dir: entry.is_dir,
                                line_number: None,
                                line: None,
                                match_range: None,
                                source_match_range: None,
                            },
                        )
                    })
                })
                .collect()
        };
        scored.sort_by(|(left_score, left), (right_score, right)| {
            left_score
                .cmp(right_score)
                .then_with(|| left.is_dir.cmp(&right.is_dir))
                .then_with(|| left.path.cmp(&right.path))
        });
        let truncated = scored.len() > crate::search::MAX_SEARCH_RESULTS;
        let results: Vec<_> = scored
            .into_iter()
            .take(crate::search::MAX_SEARCH_RESULTS)
            .map(|(_, result)| result)
            .collect();
        if let Some(search) = &mut self.search {
            search.results = results;
            search.searching = false;
            search.indexing = false;
            search.truncated = truncated;
            search.scanned_files = 0;
            search.error = None;
            search.due = None;
        }
        let hint = self
            .search
            .as_mut()
            .and_then(|search| search.selection_hint.take());
        self.restore_search_selection(hint);
    }

    fn schedule_text_search(&mut self) {
        self.search_generation = self.search_generation.saturating_add(1);
        let generation = self.search_generation;
        self.search_runtime.cancel(generation);
        let has_query = {
            let Some(search) = &mut self.search else {
                return;
            };
            search.results.clear();
            search.generation = generation;
            search.scanned_files = 0;
            search.truncated = false;
            search.error = None;
            let has_query = !search.query.is_empty();
            if !has_query {
                search.selection_hint = None;
            }
            search.searching = has_query;
            search.indexing = false;
            search.due = has_query.then(|| {
                Instant::now()
                    .checked_add(Duration::from_millis(150))
                    .unwrap()
            });
            has_query
        };
        self.search_list_state = ListState::default();
        self.search_preview_target = None;
        if has_query {
            self.set_info(vec!["Searching workspace content…".to_owned()]);
        }
    }

    fn dispatch_search_if_due(&mut self) {
        let request = self.search.as_mut().and_then(|search| {
            let due = search.due?;
            if Instant::now() < due {
                return None;
            }
            search.due = None;
            Some(SearchRequest {
                generation: search.generation,
                query: search.query.clone(),
                options: search.options,
            })
        });
        if let Some(request) = request {
            self.search_runtime.search(request);
        }
    }

    fn apply_search_events(&mut self) {
        for event in self.search_runtime.take_events() {
            match event {
                SearchEvent::Indexing { generation } => {
                    let Some(search) = &mut self.search else {
                        continue;
                    };
                    if search.mode != SearchMode::Text || search.generation != generation {
                        continue;
                    }
                    search.indexing = true;
                }
                SearchEvent::Batch {
                    generation,
                    matches,
                    scanned_files,
                } => {
                    let Some(search) = &mut self.search else {
                        continue;
                    };
                    if search.mode != SearchMode::Text || search.generation != generation {
                        continue;
                    }
                    search.scanned_files = scanned_files;
                    search.indexing = false;
                    search
                        .results
                        .extend(matches.into_iter().map(search_result));
                    self.apply_streaming_search_selection();
                }
                SearchEvent::Finished {
                    generation,
                    scanned_files,
                    truncated,
                    error,
                } => {
                    let Some(search) = &mut self.search else {
                        continue;
                    };
                    if search.mode != SearchMode::Text || search.generation != generation {
                        continue;
                    }
                    search.searching = false;
                    search.indexing = false;
                    search.scanned_files = scanned_files;
                    search.truncated = truncated;
                    search.error = error;
                    let hint = search.selection_hint.take();
                    if hint.is_some() {
                        self.restore_search_selection(hint);
                    }
                }
            }
        }
    }

    fn current_search_selection_hint(&self) -> Option<SearchSelectionHint> {
        let index = self.search_list_state.selected()?;
        let result = self.search.as_ref()?.results.get(index)?;
        Some(SearchSelectionHint {
            identity: SearchResultIdentity::from(result),
            index,
            offset: self.search_list_state.offset(),
        })
    }

    fn restore_search_selection(&mut self, hint: Option<SearchSelectionHint>) {
        let count = self
            .search
            .as_ref()
            .map_or(0, |search| search.results.len());
        self.search_list_state = ListState::default();
        if count == 0 {
            return;
        }
        let (selected, offset) = hint.map_or((0, 0), |hint| {
            let selected = self
                .search
                .as_ref()
                .and_then(|search| {
                    search
                        .results
                        .iter()
                        .position(|result| SearchResultIdentity::from(result) == hint.identity)
                })
                .unwrap_or_else(|| hint.index.min(count - 1));
            (selected, hint.offset.min(selected))
        });
        self.search_list_state = ListState::default()
            .with_selected(Some(selected))
            .with_offset(offset);
        self.preview_search_selection();
    }

    fn apply_streaming_search_selection(&mut self) {
        let Some(search) = self.search.as_ref() else {
            return;
        };
        if search.results.is_empty() || self.search_list_state.selected().is_some() {
            return;
        }
        if let Some(hint) = search.selection_hint.as_ref() {
            let selected = search
                .results
                .iter()
                .position(|result| SearchResultIdentity::from(result) == hint.identity);
            if let Some(selected) = selected {
                let offset = hint.offset.min(selected);
                self.search_list_state = ListState::default()
                    .with_selected(Some(selected))
                    .with_offset(offset);
                if let Some(search) = &mut self.search {
                    search.selection_hint = None;
                }
                self.preview_search_selection();
            }
        } else {
            self.search_list_state.select(Some(0));
            self.preview_search_selection();
        }
    }

    fn move_search_selection(&mut self, delta: isize) {
        let count = self
            .search
            .as_ref()
            .map_or(0, |search| search.results.len());
        if count == 0 {
            return;
        }
        let current = self.search_list_state.selected().unwrap_or(0);
        let next = current.saturating_add_signed(delta).min(count - 1);
        self.search_list_state.select(Some(next));
        if let Some(search) = &mut self.search {
            search.selection_hint = None;
        }
        self.preview_search_selection();
    }

    fn preview_search_selection(&mut self) {
        let Some(result) = self.selected_search_result().cloned() else {
            return;
        };
        if result.is_dir {
            self.set_info(vec![format!("{} is a directory.", result.path.display())]);
            return;
        }
        self.remember_recent_file(&result.path);
        let generation = self.request_content(
            ContentKind::Preview,
            display_workspace_path(&result.path),
            ContentTarget::Workspace(result.path.clone()),
        );
        self.search_preview_target =
            result
                .line_number
                .zip(result.source_match_range)
                .map(|(line_number, byte_range)| SearchPreviewTarget {
                    generation,
                    line_number,
                    byte_range,
                });
    }

    fn accept_search_selection(&mut self) {
        let Some(result) = self.selected_search_result().cloned() else {
            return;
        };
        let Some(search) = self.search.take() else {
            return;
        };
        self.store_search_session(&search);
        self.search_generation = self.search_generation.saturating_add(1);
        self.search_runtime.cancel(self.search_generation);
        self.last_search_click = None;
        self.search_preview_target = None;
        let changed_identity = (!result.is_dir && self.tree_scope == TreeScope::GitChanges)
            .then(|| self.git_change_identity_for_workspace_path(&result.path))
            .flatten();
        if let Some(identity) = changed_identity {
            self.apply_tree_scope(TreeScope::GitChanges);
            self.expand_git_ancestors(&identity);
            self.rebuild_visible_rows();
            self.restore_git_selection(Some(identity));
        } else {
            self.apply_tree_scope(TreeScope::AllFiles);
            self.reveal_all_files_selection(result.path.clone());
        }
        if result.is_dir {
            self.focused_pane = FocusPane::Tree;
            self.load_selected_info();
        } else {
            self.focused_pane = FocusPane::Content;
            self.remember_recent_file(&result.path);
            let generation = self.request_content(
                ContentKind::Preview,
                display_workspace_path(&result.path),
                ContentTarget::Workspace(result.path.clone()),
            );
            self.search_preview_target = result.line_number.zip(result.source_match_range).map(
                |(line_number, byte_range)| SearchPreviewTarget {
                    generation,
                    line_number,
                    byte_range,
                },
            );
        }
    }

    fn close_search(&mut self, restore_content: bool) {
        let Some(search) = self.search.take() else {
            return;
        };
        let restore = search.restore.clone();
        self.store_search_session(&search);
        self.search_generation = self.search_generation.saturating_add(1);
        self.search_runtime.cancel(self.search_generation);
        self.search_preview_target = None;
        self.last_search_click = None;
        if restore_content {
            self.content_requests.invalidate();
            self.runtime.cancel_pending_content();
            let restored_tree_state = restore.tree_state;
            self.focused_pane = restore.focused_pane;
            self.tree_scope = restore.tree_scope;
            self.all_files_selection = restore.all_files_selection;
            self.git_changes_selection = restore.git_changes_selection;
            self.pending_all_scope_path = restore.pending_all_scope_path;
            self.pending_all_scope_navigation = restore.pending_all_scope_navigation;
            self.pending_git_scope_path = restore.pending_git_scope_path;
            self.pending_git_scope_fallback = restore.pending_git_scope_fallback;
            self.rebuild_visible_rows();
            self.tree_state = restored_tree_state;
            self.normalize_tree_state();
            self.content_lines = restore.content_lines;
            self.content_highlights = restore.content_highlights;
            self.content_horizontal_scroll = restore.content_horizontal_scroll;
            self.content_selection = restore.content_selection;
            self.clipboard_status = restore.clipboard_status;
            self.content_mode = restore.content_mode;
            self.content_provider = restore.content_provider;
            self.content_show_line_numbers = restore.content_show_line_numbers;
            self.content_diff_lines = restore.content_diff_lines;
            self.content_identity = restore.content_identity;
            self.content_fold_source = restore.content_fold_source;
            self.content_fold_regions = restore.content_fold_regions;
            self.content_structure = restore.content_structure;
            self.content_collapsed_folds = restore.content_collapsed_folds;
            self.content_cursor_line = restore.content_cursor_line;
            self.content_successful = restore.content_successful;
            self.last_error = restore.last_error;
            self.navigation_source = restore.navigation_source;
            self.navigation_document_version = restore.navigation_document_version;
            self.navigation_caret = restore.navigation_caret;
            self.navigation_target_highlight = restore.navigation_target_highlight;
            self.navigation_status = restore.navigation_status;
            self.navigation_back = restore.navigation_back;
            self.navigation_forward = restore.navigation_forward;
            self.restore_content_viewport(restore.content_viewport);
            if restore.content_was_loading {
                self.load_scope_default_content();
            }
        }
    }

    fn move_search_cursor(&mut self, forward: bool) {
        let Some(search) = &mut self.search else {
            return;
        };
        search.cursor = if forward {
            search.query[search.cursor..]
                .grapheme_indices(true)
                .nth(1)
                .map_or(search.query.len(), |(offset, _)| search.cursor + offset)
        } else {
            search.query[..search.cursor]
                .grapheme_indices(true)
                .next_back()
                .map_or(0, |(offset, _)| offset)
        };
    }

    fn delete_search_character(&mut self, forward: bool) {
        let Some(search) = &mut self.search else {
            return;
        };
        let boundary = if forward {
            search.query[search.cursor..]
                .grapheme_indices(true)
                .nth(1)
                .map_or(search.query.len(), |(offset, _)| search.cursor + offset)
        } else {
            search.query[..search.cursor]
                .grapheme_indices(true)
                .next_back()
                .map_or(search.cursor, |(offset, _)| offset)
        };
        if forward {
            search.query.drain(search.cursor..boundary);
        } else {
            search.query.drain(boundary..search.cursor);
            search.cursor = boundary;
        }
        search.selection_hint = None;
        self.rebuild_search_results();
    }

    fn delete_search_word(&mut self) {
        let Some(search) = &mut self.search else {
            return;
        };
        let before = &search.query[..search.cursor];
        let trimmed = before.trim_end_matches(char::is_whitespace);
        let start = trimmed
            .char_indices()
            .rev()
            .find(|(_, character)| character.is_whitespace())
            .map_or(0, |(index, character)| index + character.len_utf8());
        search.query.drain(start..search.cursor);
        search.cursor = start;
        search.selection_hint = None;
        self.rebuild_search_results();
    }

    fn remember_recent_file(&mut self, path: &Path) {
        self.recent_files.retain(|candidate| candidate != path);
        self.recent_files.insert(0, path.to_path_buf());
        self.recent_files.truncate(20);
    }

    /// Apply a mouse event using hit boxes captured during the latest draw.
    pub fn handle_mouse(&mut self, mouse: MouseEvent) {
        self.quit_confirmation = None;
        self.navigation_hover_highlight = None;
        if mouse.kind == MouseEventKind::Moved {
            if mouse.modifiers == KeyModifiers::ALT
                && self.navigation_picker.is_none()
                && self.search.is_none()
                && self.preview_find.is_none()
                && self.content_mode == ContentMode::Preview
                && let Some((_, token)) = self.navigation_token_at_mouse(mouse)
            {
                self.navigation_hover_highlight = Some(token);
            }
            return;
        }
        if self.navigation_picker.is_some() {
            match mouse.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    self.handle_navigation_picker_mouse_down(mouse);
                }
                MouseEventKind::ScrollUp => self.move_navigation_picker(-3),
                MouseEventKind::ScrollDown => self.move_navigation_picker(3),
                _ => {}
            }
            return;
        }
        if self.search.is_some() {
            match mouse.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    self.clipboard_status = None;
                    self.handle_search_mouse_down(mouse);
                }
                MouseEventKind::ScrollUp => self.handle_mouse_scroll(mouse, -3),
                MouseEventKind::ScrollDown => self.handle_mouse_scroll(mouse, 3),
                _ => {}
            }
            return;
        }
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.clipboard_status = None;
                self.tree_resize_dragging = false;
                if self.handle_preview_find_mouse_down(mouse) {
                    return;
                }
                if self.handle_search_mouse_down(mouse) {
                    return;
                }
                if self.ui_regions.refresh_at(mouse.column, mouse.row) {
                    self.clear_content_selection();
                    self.focused_pane = FocusPane::Tree;
                    self.request_refresh(self.tree_scope == TreeScope::GitChanges);
                    return;
                }
                if contains(self.ui_regions.divider, mouse.column, mouse.row) {
                    self.clear_content_selection();
                    self.tree_resize_dragging = true;
                    return;
                }
                if let Some(scope) = self.ui_regions.scope_at(mouse.column, mouse.row) {
                    self.clear_content_selection();
                    self.focused_pane = FocusPane::Tree;
                    self.set_tree_scope(scope);
                    return;
                }
                if contains(self.ui_regions.tree_inner, mouse.column, mouse.row) {
                    self.clear_content_selection();
                    self.focused_pane = FocusPane::Tree;
                    let visible_row = usize::from(mouse.row - self.ui_regions.tree_inner.y);
                    let index = self.tree_state.offset().saturating_add(visible_row);
                    if index < self.tree_row_count() {
                        self.select(index);
                        let is_container = match self.tree_scope {
                            TreeScope::AllFiles => {
                                self.selected_entry().is_some_and(|entry| entry.is_dir)
                            }
                            TreeScope::GitChanges => self
                                .selected_git_row()
                                .is_some_and(GitTreeRow::is_container),
                            #[cfg(feature = "agent-observability")]
                            TreeScope::Agents => false,
                        };
                        if is_container {
                            self.toggle_selected_directory();
                        } else if self.tree_scope == TreeScope::GitChanges
                            && self.selected_git_row().is_some_and(GitTreeRow::is_change)
                        {
                            self.focused_pane = FocusPane::Content;
                        }
                    }
                } else if contains(self.ui_regions.content_inner, mouse.column, mouse.row) {
                    self.focused_pane = FocusPane::Content;
                    if self.handle_fold_mouse_down(mouse) {
                        return;
                    }
                    if self.content_mode == ContentMode::Preview
                        && mouse.modifiers == KeyModifiers::ALT
                        && let Some((point, token)) = self.navigation_token_at_mouse(mouse)
                    {
                        self.clear_content_selection();
                        self.navigation_caret = NavigationCaret {
                            point,
                            preferred_display_column: 0,
                        };
                        self.navigation_hover_highlight = Some(token);
                        self.navigation_target_highlight = None;
                        self.request_semantic_navigation(NavigationOperation::Definition);
                        return;
                    }
                    self.begin_content_selection(mouse);
                } else if contains(self.ui_regions.content_body, mouse.column, mouse.row) {
                    self.clear_content_selection();
                    self.focused_pane = FocusPane::Content;
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if self.tree_resize_dragging {
                    self.resize_tree_panel(mouse.column);
                } else {
                    self.drag_content_selection(mouse);
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if self.tree_resize_dragging {
                    self.resize_tree_panel(mouse.column);
                    self.tree_resize_dragging = false;
                } else {
                    self.finish_content_selection(mouse);
                }
            }
            MouseEventKind::ScrollUp => self.handle_mouse_scroll(mouse, -3),
            MouseEventKind::ScrollDown => self.handle_mouse_scroll(mouse, 3),
            _ => {}
        }
    }

    fn handle_preview_find_mouse_down(&mut self, mouse: MouseEvent) -> bool {
        if self.preview_find.is_none() {
            return false;
        }
        if contains(self.ui_regions.preview_find_close, mouse.column, mouse.row) {
            self.preview_find = None;
            return true;
        }
        if contains(
            self.ui_regions.preview_find_previous,
            mouse.column,
            mouse.row,
        ) {
            self.move_preview_find(-1);
            return true;
        }
        if contains(self.ui_regions.preview_find_next, mouse.column, mouse.row) {
            self.move_preview_find(1);
            return true;
        }
        if contains(self.ui_regions.preview_find_case, mouse.column, mouse.row) {
            if let Some(find) = &mut self.preview_find {
                find.case_sensitive = !find.case_sensitive;
            }
            self.rebuild_preview_find();
            return true;
        }
        if contains(self.ui_regions.preview_find_input, mouse.column, mouse.row) {
            if let Some(find) = &mut self.preview_find {
                let query_column = usize::from(
                    mouse
                        .column
                        .saturating_sub(self.ui_regions.preview_find_input.x),
                )
                .saturating_sub(6);
                find.cursor = byte_index_at_display_column(&find.query, query_column);
            }
            return true;
        }
        false
    }

    fn handle_search_mouse_down(&mut self, mouse: MouseEvent) -> bool {
        if self.search.is_none() {
            if self.ui_regions.file_search_at(mouse.column, mouse.row) {
                self.open_search(SearchMode::Files);
                return true;
            }
            if self.ui_regions.text_search_at(mouse.column, mouse.row) {
                self.open_search(SearchMode::Text);
                return true;
            }
            return false;
        }
        if contains(self.ui_regions.search_close, mouse.column, mouse.row) {
            self.close_search(true);
            return true;
        }
        if contains(self.ui_regions.search_clear, mouse.column, mouse.row) {
            self.clear_search();
            return true;
        }
        if contains(self.ui_regions.search_files_mode, mouse.column, mouse.row) {
            self.set_search_mode(SearchMode::Files);
            return true;
        }
        if contains(self.ui_regions.search_text_mode, mouse.column, mouse.row) {
            self.set_search_mode(SearchMode::Text);
            return true;
        }
        for (index, region) in self.ui_regions.search_options.into_iter().enumerate() {
            if !contains(region, mouse.column, mouse.row) {
                continue;
            }
            match index {
                0 => self.toggle_search_option(|options| {
                    options.case_sensitive = !options.case_sensitive;
                }),
                1 => self.toggle_search_option(|options| {
                    options.whole_word = !options.whole_word;
                }),
                2 => self.toggle_search_option(|options| {
                    options.regex = !options.regex;
                }),
                3 => self.toggle_search_option(|options| {
                    options.include_ignored = !options.include_ignored;
                }),
                _ => unreachable!(),
            }
            return true;
        }
        if contains(self.ui_regions.search_input, mouse.column, mouse.row) {
            if let Some(search) = &mut self.search {
                let query_column =
                    usize::from(mouse.column.saturating_sub(self.ui_regions.search_input.x))
                        .saturating_sub(2);
                search.cursor = byte_index_at_display_column(&search.query, query_column);
            }
            return true;
        }
        if contains(self.ui_regions.search_results, mouse.column, mouse.row) {
            let visible_row = usize::from(mouse.row - self.ui_regions.search_results.y);
            let result_height = if self.search_mode() == Some(SearchMode::Text) {
                2
            } else {
                1
            };
            let index = self
                .search_list_state
                .offset()
                .saturating_add(visible_row / result_height);
            let count = self
                .search
                .as_ref()
                .map_or(0, |search| search.results.len());
            if index < count {
                let double_click = self.last_search_click.is_some_and(|(previous, instant)| {
                    previous == index && instant.elapsed() <= Duration::from_millis(400)
                });
                self.search_list_state.select(Some(index));
                if let Some(search) = &mut self.search {
                    search.selection_hint = None;
                }
                self.preview_search_selection();
                self.last_search_click = Some((index, Instant::now()));
                if double_click {
                    self.accept_search_selection();
                }
            }
            return true;
        }
        true
    }

    fn resize_tree_panel(&mut self, column: u16) {
        let total_width = self
            .ui_regions
            .tree_body
            .width
            .saturating_add(self.ui_regions.divider.width)
            .saturating_add(self.ui_regions.content_body.width);
        let requested = column.saturating_sub(self.ui_regions.tree_body.x);
        self.tree_panel_width = Some(ui::tree_panel_width(total_width, Some(requested)));
    }

    fn begin_content_selection(&mut self, mouse: MouseEvent) {
        if !contains(self.content_text_rows(), mouse.column, mouse.row) {
            self.clear_content_selection();
            return;
        }
        let Some((before, after)) = self.content_point_bounds(mouse) else {
            self.clear_content_selection();
            return;
        };
        self.pending_clipboard_text = None;
        self.navigation_caret = NavigationCaret {
            point: SourcePosition {
                line: before.line,
                byte: before.byte,
            },
            preferred_display_column: 0,
        };
        self.navigation_target_highlight = None;
        self.content_selection = Some(ContentSelection {
            anchor_before: before,
            anchor_after: after,
            head: before,
            dragging: true,
            dragged: false,
        });
    }

    fn handle_fold_mouse_down(&mut self, mouse: MouseEvent) -> bool {
        if self.content_mode != ContentMode::Preview || !self.content_show_line_numbers {
            return false;
        }
        let rows_area = self.content_text_rows();
        if !contains(rows_area, mouse.column, mouse.row) {
            return false;
        }
        let marker_column = rows_area
            .x
            .saturating_add(self.content_line_number_width() as u16)
            .saturating_add(1);
        if mouse.column != marker_column {
            return false;
        }
        let visible = usize::from(mouse.row.saturating_sub(rows_area.y));
        let rows = self.content_visual_rows(rows_area.width);
        let effective_scroll = self.effective_content_scroll(rows.len());
        let Some(row) = rows.get(effective_scroll.saturating_add(visible)) else {
            return false;
        };
        if row.fold_marker == FoldVisualMarker::None {
            return false;
        }
        self.content_cursor_line = row.line_index;
        self.toggle_cursor_fold();
        let new_rows = self.content_visual_rows(rows_area.width);
        if let Some(index) = new_rows.iter().position(|row| {
            row.line_index == self.content_cursor_line && row.fold_marker != FoldVisualMarker::None
        }) {
            self.content_scroll = index.saturating_sub(visible);
            self.content_scroll = self.effective_content_scroll(new_rows.len());
        }
        true
    }

    fn drag_content_selection(&mut self, mouse: MouseEvent) {
        let Some(selection) = self
            .content_selection
            .filter(|selection| selection.dragging)
        else {
            return;
        };
        let Some((before, after)) = self.content_point_bounds(mouse) else {
            return;
        };
        let head = if before >= selection.anchor_before {
            after
        } else {
            before
        };
        self.content_selection = Some(ContentSelection {
            head,
            dragged: true,
            ..selection
        });
    }

    fn finish_content_selection(&mut self, mouse: MouseEvent) {
        let Some(selection) = self
            .content_selection
            .filter(|selection| selection.dragging)
        else {
            return;
        };
        if selection.dragged {
            self.drag_content_selection(mouse);
        }
        if let Some(selection) = &mut self.content_selection {
            selection.dragging = false;
        }
        if selection.dragged {
            // Terminal workspace managers may reserve Ctrl+C while still
            // forwarding mouse input to the pane. Copying on release matches
            // native terminal selection and keeps Ctrl+C as an explicit
            // repeat-copy shortcut.
            self.queue_selected_preview_copy();
        }
    }

    fn content_point_bounds(&self, mouse: MouseEvent) -> Option<(ContentPoint, ContentPoint)> {
        if self.content_lines.is_empty() {
            return None;
        }

        let rows = self.content_text_rows();
        let visible_row = if mouse.row < rows.y {
            0
        } else {
            usize::from(mouse.row - rows.y).min(usize::from(rows.height.saturating_sub(1)))
        };
        let visual_rows = self.content_visual_rows(rows.width);
        let effective_scroll = self.effective_content_scroll(visual_rows.len());
        let visual_row = visual_rows.get(effective_scroll.saturating_add(visible_row))?;
        let visible_column = usize::from(mouse.column.saturating_sub(rows.x));
        let rendered_column = self
            .effective_content_horizontal_scroll()
            .saturating_add(visible_column);
        let gutter_width = self.content_gutter_width();
        let text_column = rendered_column.saturating_sub(gutter_width);
        let line = self.content_lines.get(visual_row.line_index)?;
        let segment = line.get(visual_row.byte_range.clone())?;
        let (before, after) =
            grapheme_bounds_at_column(segment, text_column, visual_row.tab_origin);
        Some((
            ContentPoint {
                line: visual_row.line_index,
                byte: visual_row.byte_range.start + before,
            },
            ContentPoint {
                line: visual_row.line_index,
                byte: visual_row.byte_range.start + after,
            },
        ))
    }

    fn navigation_point_at_mouse(&self, mouse: MouseEvent) -> Option<SourcePosition> {
        let rows = self.content_text_rows();
        if !contains(rows, mouse.column, mouse.row) {
            return None;
        }
        let visible_row = usize::from(mouse.row.saturating_sub(rows.y));
        let visual_rows = self.content_visual_rows(rows.width);
        let effective_scroll = self.effective_content_scroll(visual_rows.len());
        if visual_rows
            .get(effective_scroll.saturating_add(visible_row))?
            .synthetic
        {
            return None;
        }
        let (point, _) = self.content_point_bounds(mouse)?;
        Some(SourcePosition {
            line: point.line,
            byte: point.byte,
        })
    }

    fn navigation_token_at_mouse(
        &self,
        mouse: MouseEvent,
    ) -> Option<(SourcePosition, SourceRange)> {
        let point = self.navigation_point_at_mouse(mouse)?;
        let source = self.navigation_source.as_ref()?;
        let token = source.structure.recognizable_tokens.containing(point)?;
        Some((point, token))
    }

    fn content_text_rows(&self) -> Rect {
        let rows = self.ui_regions.content_inner;
        if self.content_mode == ContentMode::Info {
            Rect::new(
                rows.x,
                rows.y.saturating_add(1),
                rows.width,
                rows.height.saturating_sub(1),
            )
        } else {
            rows
        }
    }

    fn clear_content_selection(&mut self) {
        self.content_selection = None;
        self.pending_clipboard_text = None;
    }

    fn queue_selected_preview_copy(&mut self) {
        let Some(text) = self.selected_content_text() else {
            self.pending_clipboard_text = None;
            self.clipboard_status =
                Some("Drag to select content, then press Ctrl+C or Cmd+C".to_owned());
            return;
        };
        let character_count = text.chars().count();
        self.pending_clipboard_text = Some(text);
        self.clipboard_status = Some(format!(
            "Copying {character_count} character{}…",
            if character_count == 1 { "" } else { "s" }
        ));
    }

    /// Copy the selected entry's path to the clipboard.
    ///
    /// - `resolve=false`: relative path (link path for symlinks), with a
    ///   trailing slash for directories.
    /// - `resolve=true`: real/absolute path, with a trailing slash for
    ///   directories.
    ///
    /// Sets `clipboard_status` to a descriptive message showing exactly what
    /// was copied. Clears any pending content-selection copy so the deferred
    /// `flush_clipboard_request` does not overwrite this status.
    fn queue_selected_path_copy(&mut self, resolve: bool) {
        // Clear any pending content-selection copy so the deferred flush does
        // not overwrite the path status set below.
        self.pending_clipboard_text = None;
        let Some(path) = self.selected_copy_path(resolve) else {
            self.clipboard_status = Some("Select a file or directory to copy its path".to_owned());
            return;
        };
        let entry = self.selected_entry();
        let is_dir = entry.is_some_and(|entry| entry.is_dir);
        let is_symlink = entry.is_some_and(|entry| entry.symlink_target.is_some());
        let mut text = path.display().to_string();
        if is_dir && !text.ends_with('/') {
            text.push('/');
        }
        let label = match (resolve, is_symlink, self.tree_scope == TreeScope::AllFiles) {
            (false, true, _) => "Copied link path",
            (false, false, _) => "Copied path",
            (true, true, true) => "Copied real path",
            (true, true, false) => "Copied link path",
            (true, false, _) => "Copied absolute path",
        };
        match clipboard::copy_text(&text) {
            Ok(_delivery) => {
                if self
                    .last_error
                    .as_deref()
                    .is_some_and(|error| error.starts_with("copy failed:"))
                {
                    self.last_error = None;
                }
                self.clipboard_status = Some(format!("{label}: {text}"));
            }
            Err(error) => {
                self.clipboard_status = None;
                self.last_error = Some(format!("copy failed: {error}"));
            }
        }
    }

    fn flush_clipboard_request(&mut self) {
        let Some(text) = self.pending_clipboard_text.take() else {
            return;
        };
        let character_count = text.chars().count();
        match clipboard::copy_text(&text) {
            Ok(delivery) => {
                if self
                    .last_error
                    .as_deref()
                    .is_some_and(|error| error.starts_with("copy failed:"))
                {
                    self.last_error = None;
                }
                self.clipboard_status = Some(clipboard_success_status(character_count, delivery));
            }
            Err(error) => {
                self.clipboard_status = None;
                self.last_error = Some(format!("copy failed: {error}"));
            }
        }
    }

    pub fn set_tree_scope(&mut self, scope: TreeScope) {
        // Git state is intentionally refreshed on every entry to this scope,
        // including a second click on its tab. A long-running TUI should not
        // show a stale changed-files tree after an agent updates the worktree.
        let entering_git_changes =
            scope == TreeScope::GitChanges && self.tree_scope != TreeScope::GitChanges;
        if entering_git_changes {
            self.pending_all_scope_path = None;
            self.pending_all_scope_navigation = false;
            self.pending_git_scope_path = self.selected_file_path_for_scope_sync();
            self.pending_git_scope_fallback = self
                .pending_git_scope_path
                .is_some()
                .then(|| self.git_changes_selection.clone())
                .flatten();
        } else if scope != TreeScope::GitChanges {
            self.pending_git_scope_path = None;
            self.pending_git_scope_fallback = None;
        }
        let pending_path_is_changed = self
            .pending_git_scope_path
            .as_deref()
            .and_then(|path| self.git_change_identity_for_workspace_path(path))
            .is_some();
        let first_git_entry_without_sync = entering_git_changes
            && self.git_changes_selection.is_none()
            && !pending_path_is_changed;
        if scope == TreeScope::GitChanges {
            self.request_refresh(true);
        }
        #[cfg(feature = "agent-observability")]
        if scope == TreeScope::Agents {
            self.request_agent_refresh();
        }

        self.apply_tree_scope(scope);
        // A clean repository row is a valid fallback, but on the first entry
        // it must not become a saved preference while the requested refresh
        // is in flight. Leaving the selection open lets the refreshed view
        // keep the established changed-file-first default. Explicit and
        // previously saved Git selections remain stable across refreshes.
        if first_git_entry_without_sync {
            self.tree_state.select(None);
        }
    }

    fn apply_tree_scope(&mut self, scope: TreeScope) {
        if self.tree_scope == scope {
            return;
        }

        let synchronized_file = self.selected_file_path_for_scope_sync();
        self.remember_current_selection();
        self.tree_scope = scope;
        self.tree_state = ListState::default();
        self.rebuild_visible_rows();
        match scope {
            TreeScope::AllFiles => {
                if let Some(path) = synchronized_file {
                    self.reveal_all_files_selection(path);
                } else {
                    self.restore_visible_selection(self.all_files_selection.clone());
                }
            }
            TreeScope::GitChanges => {
                let synchronized = synchronized_file
                    .as_deref()
                    .and_then(|path| self.git_change_identity_for_workspace_path(path));
                if let Some(identity) = &synchronized {
                    self.expand_git_ancestors(identity);
                }
                self.restore_git_selection(synchronized.or(self.git_changes_selection.clone()));
            }
            #[cfg(feature = "agent-observability")]
            TreeScope::Agents => self.restore_agent_selection(),
        }
    }

    pub fn should_quit(&self) -> bool {
        self.should_quit
    }

    pub const fn is_refreshing(&self) -> bool {
        self.refresh_requests.is_loading()
    }

    pub const fn is_initial_loading(&self) -> bool {
        self.is_refreshing() && !self.has_refresh_snapshot
    }

    pub const fn is_content_loading(&self) -> bool {
        self.content_requests.is_loading()
    }

    const fn is_navigation_preview_loading(&self) -> bool {
        self.navigation_preview_requests.is_loading()
    }

    pub fn is_directory_loading(&self) -> bool {
        !self.loading_directories.is_empty()
    }

    pub fn is_searching(&self) -> bool {
        self.search.as_ref().is_some_and(|search| search.searching)
    }

    fn handle_scope_tabs_key(&mut self, key: KeyEvent) {
        match (key.code, key.modifiers) {
            (KeyCode::Left, KeyModifiers::NONE) => {
                self.set_tree_scope(self.tree_scope.previous());
            }
            (KeyCode::Right, KeyModifiers::NONE) => {
                self.set_tree_scope(self.tree_scope.next());
            }
            (KeyCode::Down, KeyModifiers::NONE) => self.focused_pane = FocusPane::Tree,
            _ => {}
        }
    }

    fn handle_tree_key(&mut self, key: KeyEvent) {
        match (key.code, key.modifiers) {
            (KeyCode::Down | KeyCode::Char('j'), _) => self.move_selection(1),
            (KeyCode::Up, KeyModifiers::NONE) if self.tree_is_at_first_row_or_empty() => {
                self.focused_pane = FocusPane::ScopeTabs;
            }
            (KeyCode::Up | KeyCode::Char('k'), _) => self.move_selection(-1),
            (KeyCode::Home | KeyCode::Char('g'), _) => self.select(0),
            (KeyCode::End | KeyCode::Char('G'), _) => {
                self.select(self.tree_row_count().saturating_sub(1));
            }
            (KeyCode::Left, KeyModifiers::NONE) => self.focused_pane = FocusPane::Tree,
            (KeyCode::Enter, _) => self.activate_selected_tree_entry(),
            (KeyCode::Right, KeyModifiers::NONE) => {
                self.focused_pane = FocusPane::Content;
            }
            _ => {}
        }
    }

    fn handle_content_key(&mut self, key: KeyEvent) {
        match (key.code, key.modifiers) {
            (KeyCode::Char('['), KeyModifiers::NONE)
                if self.content_mode == ContentMode::Preview =>
            {
                self.jump_visible_fold(-1);
            }
            (KeyCode::Char(']'), KeyModifiers::NONE)
                if self.content_mode == ContentMode::Preview =>
            {
                self.jump_visible_fold(1);
            }
            (KeyCode::Enter | KeyCode::Char(' '), KeyModifiers::NONE)
                if self.content_mode == ContentMode::Preview =>
            {
                self.toggle_cursor_fold();
            }
            (KeyCode::Char('{'), KeyModifiers::NONE | KeyModifiers::SHIFT)
                if self.content_mode == ContentMode::Preview =>
            {
                self.collapse_all_folds();
            }
            (KeyCode::Char('}'), KeyModifiers::NONE | KeyModifiers::SHIFT)
                if self.content_mode == ContentMode::Preview =>
            {
                self.expand_all_folds();
            }
            (KeyCode::Down | KeyCode::Char('j'), _) => self.scroll_content(1, 0),
            (KeyCode::Up | KeyCode::Char('k'), _) => self.scroll_content(-1, 0),
            (KeyCode::PageDown, _) => self.scroll_content(12, 0),
            (KeyCode::PageUp, _) => self.scroll_content(-12, 0),
            (KeyCode::Left, KeyModifiers::SHIFT) => self.scroll_content(0, -4),
            (KeyCode::Right, KeyModifiers::SHIFT) => self.scroll_content(0, 4),
            (KeyCode::Left, KeyModifiers::NONE) => self.focused_pane = FocusPane::Tree,
            (KeyCode::Right, KeyModifiers::NONE) => self.focused_pane = FocusPane::Content,
            (KeyCode::Home | KeyCode::Char('g'), _) => {
                self.content_scroll = 0;
                self.sync_content_cursor_to_scroll();
            }
            (KeyCode::End | KeyCode::Char('G'), _) => {
                self.content_scroll = self
                    .content_visual_rows(self.ui_regions.content_inner.width)
                    .len()
                    .saturating_sub(1);
                self.sync_content_cursor_to_scroll();
            }
            _ => {}
        }
    }

    fn handle_mouse_scroll(&mut self, mouse: MouseEvent, delta: isize) {
        if self.search.is_some() {
            if contains(self.ui_regions.search_popup, mouse.column, mouse.row) {
                self.move_search_selection(delta);
            }
            return;
        }
        if contains(self.ui_regions.tree_body, mouse.column, mouse.row) {
            self.focused_pane = FocusPane::Tree;
            self.move_selection(delta);
        } else if contains(self.ui_regions.content_body, mouse.column, mouse.row) {
            self.focused_pane = FocusPane::Content;
            self.scroll_content(delta, 0);
        } else {
            match self.focused_pane {
                FocusPane::ScopeTabs => {}
                FocusPane::Tree => self.move_selection(delta),
                FocusPane::Content => self.scroll_content(delta, 0),
            }
        }
    }

    fn tree_is_at_first_row_or_empty(&self) -> bool {
        self.tree_row_count() == 0 || self.tree_state.selected().unwrap_or(0) == 0
    }

    fn move_selection(&mut self, delta: isize) {
        if self.tree_row_count() == 0 {
            return;
        }

        let current = self.tree_state.selected().unwrap_or(0);
        let next = current
            .saturating_add_signed(delta)
            .min(self.tree_row_count().saturating_sub(1));
        self.select(next);
    }

    fn select(&mut self, index: usize) {
        self.pending_all_scope_path = None;
        self.pending_all_scope_navigation = false;
        self.pending_git_scope_path = None;
        self.pending_git_scope_fallback = None;
        if self.tree_row_count() == 0 {
            self.select_optional(None);
            return;
        }
        self.select_optional(Some(index.min(self.tree_row_count() - 1)));
    }

    fn select_optional(&mut self, index: Option<usize>) {
        self.tree_state.select(index);
        self.normalize_tree_state();
        self.remember_current_selection();
        self.load_scope_default_content();
    }

    fn activate_selected_tree_entry(&mut self) {
        #[cfg(feature = "agent-observability")]
        if self.tree_scope == TreeScope::Agents {
            if self.selected_agent_session().is_some() {
                self.focused_pane = FocusPane::Content;
                self.load_selected_agent_detail();
            }
            return;
        }
        if self.tree_scope == TreeScope::GitChanges {
            match self.selected_git_row() {
                Some(row) if row.is_container() => self.toggle_selected_directory(),
                Some(row) if row.is_change() => {
                    self.focused_pane = FocusPane::Content;
                    self.load_selected_diff();
                }
                _ => {}
            }
        } else {
            match self.selected_entry().map(|entry| entry.is_dir) {
                Some(true) => self.toggle_selected_directory(),
                Some(false) => self.focused_pane = FocusPane::Content,
                None => {}
            }
        }
    }

    fn toggle_selected_directory(&mut self) {
        self.pending_all_scope_path = None;
        self.pending_all_scope_navigation = false;
        self.pending_git_scope_path = None;
        self.pending_git_scope_fallback = None;
        if self.tree_scope == TreeScope::GitChanges {
            let Some(identity) = self
                .selected_git_row()
                .filter(|row| row.is_container())
                .map(|row| row.identity.clone())
            else {
                return;
            };
            let expanded = self
                .git_changes_expansion
                .get(&identity)
                .copied()
                .unwrap_or(true);
            self.git_changes_expansion
                .insert(identity.clone(), !expanded);
            self.rebuild_visible_rows();
            self.restore_git_selection(Some(identity));
            return;
        }
        let Some(relative) = self
            .selected_entry()
            .filter(|entry| entry.is_dir)
            .map(|entry| entry.relative.clone())
        else {
            return;
        };

        let expanded = self
            .all_files_expansion
            .get(&relative)
            .copied()
            .unwrap_or(false);
        self.all_files_expansion.insert(relative.clone(), !expanded);
        if !expanded {
            self.request_directory_load(relative.clone());
        }
        self.rebuild_visible_rows();
        self.restore_visible_selection(Some(relative));
    }

    fn request_directory_load(&mut self, relative: PathBuf) {
        if !self.unloaded_directories.contains(&relative)
            || !self.loading_directories.insert(relative.clone())
        {
            return;
        }
        self.runtime.request_directory(DirectoryRequest {
            tree_epoch: self.tree_epoch,
            relative,
            scan_entry_limit: self.scan_entry_limit,
        });
    }

    fn select_changed(&mut self, delta: isize) {
        let len = self.tree_row_count();
        if len == 0 {
            return;
        }
        let start = self.tree_state.selected().unwrap_or(0);
        let target = (1..=len).find_map(|distance| {
            let distance = distance % len;
            let index = if delta.is_negative() {
                (start + len - distance) % len
            } else {
                (start + distance) % len
            };
            match self.tree_scope {
                TreeScope::AllFiles => self
                    .visible_rows
                    .get(index)
                    .is_some_and(|entry| !entry.is_dir && entry.status.is_some())
                    .then_some(index),
                TreeScope::GitChanges => self
                    .visible_git_rows
                    .get(index)
                    .is_some_and(GitTreeRow::is_change)
                    .then_some(index),
                #[cfg(feature = "agent-observability")]
                TreeScope::Agents => None,
            }
        });
        if let Some(index) = target {
            self.select(index);
            self.load_selected_diff();
        }
    }

    fn sync_content_cursor_to_scroll(&mut self) {
        if self.content_mode != ContentMode::Preview {
            return;
        }
        let rows = self.content_visual_rows(self.ui_regions.content_inner.width.max(1));
        self.content_scroll = self.effective_content_scroll(rows.len());
        if let Some(row) = rows.get(self.content_scroll) {
            self.content_cursor_line = row.line_index;
            self.navigation_caret = NavigationCaret {
                point: first_navigation_point_on_line(&self.content_lines, row.line_index),
                preferred_display_column: 0,
            };
        }
    }

    fn jump_visible_fold(&mut self, delta: isize) {
        let width = self.ui_regions.content_inner.width.max(1);
        let rows = self.content_visual_rows(width);
        let markers: Vec<(usize, usize)> = rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.fold_marker != FoldVisualMarker::None)
            .map(|(index, row)| (index, row.line_index))
            .collect();
        if markers.is_empty() {
            return;
        }
        let position = if delta >= 0 {
            markers
                .iter()
                .position(|(_, line)| *line > self.content_cursor_line)
                .unwrap_or(0)
        } else {
            markers
                .iter()
                .rposition(|(_, line)| *line < self.content_cursor_line)
                .unwrap_or(markers.len() - 1)
        };
        self.content_scroll = markers[position].0;
        self.content_cursor_line = markers[position].1;
        self.clear_content_selection();
    }

    fn toggle_cursor_fold(&mut self) {
        let Some(region) = self
            .content_fold_regions
            .iter()
            .find(|region| region.start_line == self.content_cursor_line)
        else {
            return;
        };
        let anchor = region.anchor;
        if !self.content_collapsed_folds.remove(&anchor) {
            self.content_collapsed_folds.insert(anchor);
        }
        self.clear_content_selection();
        self.scroll_to_logical_line(self.content_cursor_line, 0);
    }

    fn collapse_all_folds(&mut self) {
        self.content_collapsed_folds
            .extend(self.content_fold_regions.iter().map(|region| region.anchor));
        self.clear_content_selection();
        self.ensure_cursor_visible();
    }

    fn expand_all_folds(&mut self) {
        self.content_collapsed_folds.clear();
        self.clear_content_selection();
        self.scroll_to_logical_line(self.content_cursor_line, 0);
    }

    fn ensure_cursor_visible(&mut self) {
        if let Some(outer) = self
            .content_fold_regions
            .iter()
            .filter(|region| {
                self.content_collapsed_folds.contains(&region.anchor)
                    && region.start_line < self.content_cursor_line
                    && self.content_cursor_line <= region.end_line
            })
            .min_by_key(|region| region.start_line)
        {
            self.content_cursor_line = outer.start_line;
        }
        self.scroll_to_logical_line(self.content_cursor_line, 0);
    }

    fn reveal_folded_line(&mut self, line: usize) {
        let hidden_by: Vec<FoldAnchor> = self
            .content_fold_regions
            .iter()
            .filter(|region| {
                region.start_line < line
                    && line <= region.end_line
                    && self.content_collapsed_folds.contains(&region.anchor)
            })
            .map(|region| region.anchor)
            .collect();
        for anchor in hidden_by {
            self.content_collapsed_folds.remove(&anchor);
        }
    }

    fn scroll_to_logical_line(&mut self, line: usize, byte: usize) {
        let width = self.ui_regions.content_inner.width.max(1);
        let rows = self.content_visual_rows(width);
        let line_len = self.content_lines.get(line).map_or(0, String::len);
        self.content_scroll = rows
            .iter()
            .position(|row| {
                row.line_index == line
                    && !row.synthetic
                    && visual_row_contains_byte(row, byte, line_len)
            })
            .or_else(|| rows.iter().position(|row| row.line_index == line))
            .unwrap_or(0);
        self.content_cursor_line = rows
            .get(self.content_scroll)
            .map_or(line, |row| row.line_index);
    }

    fn cache_current_folds(&mut self) {
        if !self.content_successful || !self.content_fold_source.allows_folding() {
            return;
        }
        let Some(identity) = self.content_identity.clone() else {
            return;
        };
        self.fold_cache
            .retain(|(candidate, _)| candidate != &identity);
        self.fold_cache
            .push_front((identity, self.content_collapsed_folds.clone()));
        self.fold_cache.truncate(64);
    }

    fn cached_folds(&self, identity: &ContentIdentity) -> HashSet<FoldAnchor> {
        self.fold_cache
            .iter()
            .find(|(candidate, _)| candidate == identity)
            .map_or_else(HashSet::new, |(_, folds)| folds.clone())
    }

    fn restore_content_viewport(&mut self, viewport: ContentViewportRestore) {
        let width = self.ui_regions.content_inner.width.max(1);
        self.content_projection_width = width;
        let rows = self.content_visual_rows(width);
        let restored = viewport.line.and_then(|line| {
            let line_len = self.content_lines.get(line).map_or(0, String::len);
            rows.iter()
                .position(|row| {
                    viewport_row_matches(
                        row,
                        line,
                        viewport.byte_start,
                        viewport.synthetic,
                        line_len,
                    )
                })
                .or_else(|| {
                    if viewport.synthetic {
                        rows.iter()
                            .rposition(|row| row.line_index == line && !row.synthetic)
                    } else {
                        None
                    }
                })
        });
        self.content_scroll = restored
            .unwrap_or(viewport.effective_scroll)
            .min(rows.len().saturating_sub(1));
    }

    fn scroll_content(&mut self, vertical: isize, horizontal: isize) {
        let row_count = self
            .content_visual_rows(self.ui_regions.content_inner.width.max(1))
            .len();
        self.content_scroll = self.effective_content_scroll(row_count);
        self.content_scroll = self
            .content_scroll
            .saturating_add_signed(vertical)
            .min(row_count.saturating_sub(1));
        if !self.content_wraps_lines() {
            self.content_horizontal_scroll = self
                .content_horizontal_scroll
                .saturating_add_signed(horizontal);
        }
        self.sync_content_cursor_to_scroll();
    }

    fn request_refresh(&mut self, full_repository_discovery: bool) -> u64 {
        // A refresh changes repository ownership and therefore LSP session
        // identity. Retire every request/picker tied to the old graph before
        // the worker can publish its replacement.
        self.cancel_pending_navigation();
        self.remember_current_selection();
        let generation = self.refresh_requests.begin();
        self.last_refresh_error = None;
        self.runtime.request_refresh(RefreshRequest {
            generation,
            scan_entry_limit: self.scan_entry_limit,
            scan_depth: tree::DEFAULT_INITIAL_SCAN_DEPTH,
            full_repository_discovery,
        });
        generation
    }

    #[cfg(feature = "agent-observability")]
    fn request_agent_refresh(&mut self) {
        if let Some(runtime) = &self.agent_runtime {
            let metadata = runtime.submit(AgentRuntimeRequest::RefreshMetadata {
                generation: self.agent_generation,
            });
            let providers = runtime.submit(AgentRuntimeRequest::RefreshProviders {
                generation: self.agent_generation,
            });
            if metadata.is_err() || providers.is_err() {
                self.last_error = Some("Agent refresh queue is full".to_owned());
            }
        }
    }

    pub fn poll_background(&mut self) {
        if self
            .quit_confirmation
            .is_some_and(|confirmation| Instant::now() > confirmation.deadline)
        {
            self.quit_confirmation = None;
        }
        if self
            .navigation_status
            .as_ref()
            .is_some_and(|status| Instant::now() > status.expires_at)
        {
            self.navigation_status = None;
        }
        self.dispatch_search_if_due();
        let (refresh, directories, content) = self.runtime.take_completions();
        if let Some(completion) = refresh {
            self.apply_refresh_completion(completion);
        }
        for completion in directories {
            self.apply_directory_completion(completion);
        }
        if let Some(completion) = content {
            self.apply_content_completion(completion);
        }
        for completion in self.navigation_runtime.take_completions() {
            self.apply_navigation_completion(completion);
        }
        for completion in self.navigation_runtime.take_symbol_completions() {
            self.apply_document_symbol_completion(completion);
        }
        self.apply_search_events();
        #[cfg(feature = "agent-observability")]
        self.poll_agent_background();
    }

    #[cfg(feature = "agent-observability")]
    fn poll_agent_background(&mut self) {
        let completions = self
            .agent_runtime
            .as_ref()
            .map(|runtime| {
                std::iter::from_fn(|| runtime.try_next())
                    .take(64)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for completion in completions {
            let result = match completion {
                AgentRuntimeCompletion::MetadataLoaded {
                    generation,
                    snapshot,
                } => Some(self.agent_state.bootstrap_metadata(generation, snapshot)),
                AgentRuntimeCompletion::EnvelopeReceived {
                    generation,
                    workspace_hints: _,
                    envelope,
                } => Some(self.agent_state.apply_envelope(generation, *envelope)),
                AgentRuntimeCompletion::EvidenceExpired { generation, keys } => {
                    Some(self.agent_state.expire_evidence(generation, &keys))
                }
                AgentRuntimeCompletion::LiveDropped {
                    generation,
                    sessions,
                    unattributed,
                } if generation == self.agent_generation => {
                    let mut changed = false;
                    for dropped in sessions {
                        changed |= self
                            .agent_state
                            .record_live_drop(Some(&dropped.session), dropped.count);
                    }
                    changed |= self.agent_state.record_live_drop(None, unattributed);
                    if changed {
                        self.agent_view = self.agent_state.view();
                        self.restore_agent_selection();
                        if self.tree_scope == TreeScope::Agents {
                            self.load_selected_agent_detail();
                        }
                    }
                    None
                }
                AgentRuntimeCompletion::LiveDropped { .. } => None,
                AgentRuntimeCompletion::ProviderStatus { .. }
                | AgentRuntimeCompletion::RuntimeStatus { .. }
                | AgentRuntimeCompletion::MetadataWriteStatus { .. }
                | AgentRuntimeCompletion::IngressRejected { .. } => None,
                AgentRuntimeCompletion::ContractUpdated {
                    generation,
                    contract,
                } => Some(
                    self.agent_state
                        .apply_contract_update(generation, &contract),
                ),
            };
            if let Some(result) = result {
                self.reduce_agent_apply_result(result);
            }
        }
    }

    #[cfg(feature = "agent-observability")]
    fn reduce_agent_apply_result(&mut self, result: ApplyResult) {
        if let Some(runtime) = &self.agent_runtime {
            for delta in result.metadata_deltas {
                if runtime
                    .submit(AgentRuntimeRequest::PersistMetadata {
                        generation: self.agent_generation,
                        delta,
                    })
                    .is_err()
                {
                    self.last_error = Some(
                        "Agent metadata queue is full; live state remains available".to_owned(),
                    );
                }
            }
            for update in result.expiry_updates {
                if runtime
                    .submit(AgentRuntimeRequest::ScheduleExpiry {
                        generation: self.agent_generation,
                        expiry: EvidenceExpiry {
                            key: update.key,
                            valid_until: update.valid_until,
                        },
                    })
                    .is_err()
                {
                    self.last_error =
                        Some("Agent expiry queue is full; coverage is partial".to_owned());
                }
            }
        }
        if result.changed {
            self.agent_view = self.agent_state.view();
            self.restore_agent_selection();
            if self.tree_scope == TreeScope::Agents {
                self.load_selected_agent_detail();
            }
        }
    }

    /// Return a handle to bounded process/thread cleanup evidence used by the
    /// cross-platform production-spawner integration tests.
    #[cfg(feature = "navigation-test-support")]
    #[doc(hidden)]
    pub fn navigation_test_probe(&self) -> NavigationTestProbe {
        NavigationTestProbe {
            stats: self.navigation_runtime.cleanup_probe(),
        }
    }

    /// Wait until all currently requested work is reduced into application
    /// state. The interactive loop never calls this; it is useful to embedders
    /// and deterministic tests that do not own an event loop.
    #[doc(hidden)]
    pub fn wait_for_background(&mut self) {
        while self.is_refreshing()
            || self.is_directory_loading()
            || self.is_content_loading()
            || self.is_navigation_preview_loading()
            || self.is_searching()
        {
            if self.is_refreshing()
                || self.is_directory_loading()
                || self.is_content_loading()
                || self.is_navigation_preview_loading()
            {
                if !self.runtime.wait_for_completion() {
                    self.last_error =
                        Some("background worker stopped before completing work".to_owned());
                    break;
                }
            } else {
                std::thread::sleep(Duration::from_millis(10));
            }
            self.poll_background();
        }
    }

    fn apply_refresh_completion(&mut self, completion: RefreshCompletion) {
        if !self.refresh_requests.accept(completion.generation) {
            return;
        }
        let snapshot = match completion.result {
            Ok(snapshot) => snapshot,
            Err(error) => {
                let error = format!("refresh failed: {error}");
                self.last_refresh_error = Some(error.clone());
                self.last_error = Some(error);
                return;
            }
        };
        self.apply_refresh_snapshot(snapshot);
    }

    fn apply_refresh_snapshot(&mut self, snapshot: RefreshSnapshot) {
        // Search indexing is lazy. A later refresh only invalidates an
        // existing inventory; rebuilding waits until the next text query.
        if self.has_refresh_snapshot {
            self.search_runtime.refresh_inventory();
        }
        let full_repository_discovery = snapshot.full_repository_discovery;
        let pending_git_scope_path = self.pending_git_scope_path.take();
        let pending_git_scope_fallback = self.pending_git_scope_fallback.take();
        self.remember_current_selection();
        let tree::ScanResult {
            entries,
            truncated,
            unloaded_directories,
        } = snapshot.scan;
        let changed_entries = tree::changed_only(&entries);
        self.has_refresh_snapshot = true;
        self.tree_epoch = self
            .tree_epoch
            .checked_add(1)
            .expect("tree snapshot epoch exhausted");
        self.loading_directories.clear();

        self.branch = snapshot.branch;
        self.all_files_truncated = truncated;
        // Git status paths are synthesized into the filtered tree, but that
        // tree still comes from the bounded filesystem traversal. Keep the
        // conservative partial marker in both views instead of claiming the
        // filesystem-derived Git projection is complete.
        self.git_changes_truncated = truncated;
        self.all_entries = entries;
        self.unloaded_directories = unloaded_directories;
        self.changed_entries = changed_entries;
        if let Some(graph) = snapshot.graph {
            self.repo = graph
                .repositories()
                .iter()
                .find(|snapshot| {
                    matches!(
                        snapshot.node.kind,
                        RepoKind::WorkspaceRoot | RepoKind::Containing
                    )
                })
                .and_then(|snapshot| snapshot.node.repo.clone());
            self.total_repository_count = graph.repositories().len();
            self.dirty_repository_count = graph
                .repositories()
                .iter()
                .filter(|snapshot| repo_has_activity(snapshot))
                .count();
            self.repository_error_count = graph
                .repositories()
                .iter()
                .filter(|snapshot| snapshot.status_error.is_some())
                .count()
                .saturating_add(graph.report().errors.len());
            self.repository_graph_truncated =
                repository_graph_is_truncated(&graph, full_repository_discovery);
            self.git_changes_truncated |= self.repository_graph_truncated;
            if full_repository_discovery
                && !self.repository_graph_truncated
                && self.repository_error_count == 0
            {
                self.reviewed_change_versions
                    .retain(|path, _| graph.change_details(path).is_some());
            }
            self.git_rows = build_git_rows(&self.root, &graph, &snapshot.existing_changes);
            self.changed_count = self
                .git_rows
                .iter()
                .filter(|row| matches!(row.kind, GitRowKind::Change(_) | GitRowKind::Pointer(_)))
                .count();
            debug_assert_eq!(self.changed_count, snapshot.projected_change_count);
            self.repo_graph = Some(graph);
        } else {
            self.repo = None;
            self.changed_count = 0;
            self.total_repository_count = 0;
            self.dirty_repository_count = 0;
            self.repository_error_count = 0;
            self.repository_graph_truncated = false;
            self.git_rows.clear();
            self.repo_graph = None;
        }
        self.rebind_navigation_sources_after_refresh();
        self.reconcile_expansion_state();
        let expanded_boundaries: Vec<PathBuf> = self
            .unloaded_directories
            .iter()
            .filter(|path| {
                self.all_files_expansion
                    .get(*path)
                    .copied()
                    .unwrap_or(false)
            })
            .cloned()
            .collect();
        for directory in expanded_boundaries {
            self.request_directory_load(directory);
        }
        self.tree_state = ListState::default();
        self.rebuild_visible_rows();
        let visible_navigation_path = self
            .navigation_source
            .as_ref()
            .and_then(|source| source.identity.workspace_path().map(Path::to_path_buf));

        match self.tree_scope {
            TreeScope::AllFiles => {
                if let Some(path) = visible_navigation_path {
                    self.reveal_navigation_tree_selection(path);
                } else if let Some(path) = self.pending_all_scope_path.clone() {
                    if self.pending_all_scope_navigation {
                        self.reveal_navigation_tree_selection(path);
                    } else {
                        self.reveal_all_files_selection(path);
                    }
                } else {
                    self.restore_visible_selection(self.all_files_selection.clone());
                }
            }
            TreeScope::GitChanges => {
                let has_synchronized_candidate = pending_git_scope_path.is_some();
                let synchronized = pending_git_scope_path
                    .as_deref()
                    .and_then(|path| self.git_change_identity_for_workspace_path(path));
                if let Some(identity) = &synchronized {
                    self.expand_git_ancestors(identity);
                }
                let fallback = if has_synchronized_candidate {
                    pending_git_scope_fallback
                } else {
                    self.git_changes_selection.clone()
                };
                self.restore_git_selection_inner(
                    synchronized.or(fallback),
                    self.navigation_source.is_none(),
                );
            }
            #[cfg(feature = "agent-observability")]
            TreeScope::Agents => self.restore_agent_selection(),
        }

        self.last_refresh_error = None;
        self.last_error = None;
        if self.search.is_some() {
            let hint = self.current_search_selection_hint().or_else(|| {
                self.search
                    .as_ref()
                    .and_then(|search| search.selection_hint.clone())
            });
            if let Some(search) = &mut self.search {
                search.selection_hint = hint;
            }
            self.rebuild_search_results();
        }
    }

    fn apply_directory_completion(&mut self, completion: DirectoryCompletion) {
        if completion.tree_epoch != self.tree_epoch {
            return;
        }
        self.loading_directories.remove(&completion.relative);
        let scan = match completion.result {
            Ok(scan) => scan,
            Err(error) => {
                self.last_error = Some(format!(
                    "directory scan failed at {}: {error}",
                    completion.relative.display()
                ));
                return;
            }
        };

        self.unloaded_directories.remove(&completion.relative);
        self.unloaded_directories.extend(scan.unloaded_directories);
        self.all_files_truncated |= scan.truncated;
        for entry in scan.entries {
            if let Some(existing) = self
                .all_entries
                .iter_mut()
                .find(|existing| existing.relative == entry.relative)
            {
                *existing = entry;
            } else {
                self.all_entries.push(entry);
            }
        }
        self.all_entries.sort_by(|left, right| {
            tree::compare_tree_paths(&left.relative, left.is_dir, &right.relative, right.is_dir)
        });
        self.changed_entries = tree::changed_only(&self.all_entries);
        self.reconcile_expansion_state();
        self.rebuild_visible_rows();
        match self.tree_scope {
            TreeScope::AllFiles => {
                if let Some(path) = self
                    .navigation_source
                    .as_ref()
                    .and_then(|source| source.identity.workspace_path().map(Path::to_path_buf))
                {
                    self.reveal_navigation_tree_selection(path);
                } else if let Some(path) = self.pending_all_scope_path.clone() {
                    if self.pending_all_scope_navigation {
                        self.reveal_navigation_tree_selection(path);
                    } else {
                        self.reveal_all_files_selection(path);
                    }
                } else {
                    self.restore_visible_selection(Some(completion.relative));
                }
            }
            TreeScope::GitChanges => {
                self.restore_git_selection_inner(
                    self.git_changes_selection.clone(),
                    self.navigation_source.is_none(),
                );
            }
            #[cfg(feature = "agent-observability")]
            TreeScope::Agents => self.restore_agent_selection(),
        }
        self.last_error = None;
        if let Some(session) = &mut self.search_sessions[SearchMode::Files.index()] {
            session.needs_refresh = true;
        }
        if self.search_mode() == Some(SearchMode::Files) {
            let hint = self.current_search_selection_hint();
            if let Some(search) = &mut self.search {
                search.selection_hint = hint;
            }
            self.rebuild_file_search_results();
        }
    }

    fn default_selection_index(&self) -> Option<usize> {
        match self.tree_scope {
            TreeScope::AllFiles => (!self.visible_rows.is_empty()).then_some(0),
            TreeScope::GitChanges => self
                .visible_git_rows
                .iter()
                .position(GitTreeRow::is_change)
                .or_else(|| (!self.visible_git_rows.is_empty()).then_some(0)),
            #[cfg(feature = "agent-observability")]
            TreeScope::Agents => self
                .agent_selection
                .as_ref()
                .and_then(|session| {
                    self.agent_view
                        .sessions
                        .iter()
                        .position(|candidate| &candidate.session == session)
                })
                .or_else(|| (!self.agent_view.sessions.is_empty()).then_some(0)),
        }
    }

    fn remember_current_selection(&mut self) {
        match self.tree_scope {
            TreeScope::AllFiles => self.all_files_selection = self.selected_relative_path(),
            TreeScope::GitChanges => {
                self.git_changes_selection =
                    self.selected_git_row().map(|row| row.identity.clone());
            }
            #[cfg(feature = "agent-observability")]
            TreeScope::Agents => {
                self.agent_selection = self
                    .selected_agent_session()
                    .map(|session| session.session.clone());
            }
        }
    }

    fn selected_file_path_for_scope_sync(&self) -> Option<PathBuf> {
        match self.tree_scope {
            TreeScope::AllFiles => self
                .selected_entry()
                .filter(|entry| !entry.is_dir)
                .map(|entry| entry.relative.clone()),
            TreeScope::GitChanges => {
                let row = self.selected_git_row()?;
                let GitRowKind::Change(change) = &row.kind else {
                    return None;
                };
                self.workspace_path_for_change(change)
            }
            #[cfg(feature = "agent-observability")]
            TreeScope::Agents => None,
        }
    }

    fn git_change_identity_for_workspace_path(&self, path: &Path) -> Option<GitRowIdentity> {
        self.git_rows
            .iter()
            .find(|row| {
                let GitRowKind::Change(change) = &row.kind else {
                    return false;
                };
                self.workspace_path_for_change(change).as_deref() == Some(path)
            })
            .map(|row| row.identity.clone())
    }

    fn workspace_path_for_change(&self, change: &RepoChange) -> Option<PathBuf> {
        let worktree = &self
            .repo_graph
            .as_ref()?
            .repository(&change.path.repo_id)?
            .node
            .worktree;
        worktree
            .join(&change.path.relative)
            .strip_prefix(&self.root)
            .ok()
            .map(Path::to_path_buf)
    }

    fn reveal_all_files_selection(&mut self, path: PathBuf) {
        self.pending_all_scope_navigation = false;
        self.reveal_all_files_selection_inner(path, true);
    }

    fn reveal_navigation_tree_selection(&mut self, path: PathBuf) {
        self.pending_all_scope_navigation = true;
        self.reveal_all_files_selection_inner(path, false);
    }

    fn reveal_all_files_selection_inner(&mut self, path: PathBuf, load_content: bool) {
        self.pending_all_scope_path = Some(path.clone());
        let mut parent = path.parent();
        while let Some(directory) = parent.filter(|path| !path.as_os_str().is_empty()) {
            self.all_files_expansion
                .insert(directory.to_path_buf(), true);
            parent = directory.parent();
        }
        let unloaded_boundaries: Vec<PathBuf> = self
            .unloaded_directories
            .iter()
            .filter(|directory| path.starts_with(directory))
            .cloned()
            .collect();
        for directory in unloaded_boundaries {
            self.request_directory_load(directory);
        }
        self.rebuild_visible_rows();
        let resolved = self.visible_index_for_path(&path).is_some();
        let index = self
            .visible_index_for_path(&path)
            .or_else(|| self.nearest_visible_ancestor_index(&path))
            .or_else(|| self.default_selection_index());
        if load_content {
            self.select_optional(index);
        } else {
            self.tree_state.select(index);
            self.normalize_tree_state();
            self.remember_current_selection();
        }
        let waiting = self
            .loading_directories
            .iter()
            .any(|directory| path.starts_with(directory));
        if resolved || !waiting {
            self.pending_all_scope_path = None;
            self.pending_all_scope_navigation = false;
        }
    }

    fn expand_git_ancestors(&mut self, identity: &GitRowIdentity) {
        let Some(ancestors) = self
            .git_rows
            .iter()
            .find(|row| &row.identity == identity)
            .map(|row| row.ancestors.clone())
        else {
            return;
        };
        for ancestor in ancestors {
            self.git_changes_expansion.insert(ancestor, true);
        }
        self.rebuild_visible_rows();
    }

    const fn default_directory_expansion(scope: TreeScope) -> bool {
        matches!(scope, TreeScope::GitChanges)
    }

    fn reconcile_expansion_state(&mut self) {
        // Directory identities live in the complete scan rather than the
        // filtered Git view. That preserves a Git-scope choice while its
        // directory is temporarily clean, but drops it once the directory is
        // genuinely gone or no longer a directory.
        let directories: HashSet<PathBuf> = self
            .all_entries
            .iter()
            .filter(|entry| entry.is_dir)
            .map(|entry| entry.relative.clone())
            .collect();
        Self::reconcile_expansion_map(
            &mut self.all_files_expansion,
            &directories,
            Self::default_directory_expansion(TreeScope::AllFiles),
        );
        let git_containers: HashSet<GitRowIdentity> = self
            .git_rows
            .iter()
            .filter(|row| row.is_container())
            .map(|row| row.identity.clone())
            .collect();
        self.git_changes_expansion
            .retain(|identity, _| git_containers.contains(identity));
        for identity in git_containers {
            self.git_changes_expansion.entry(identity).or_insert(true);
        }
    }

    fn reconcile_expansion_map(
        expansion: &mut HashMap<PathBuf, bool>,
        directories: &HashSet<PathBuf>,
        default_expanded: bool,
    ) {
        expansion.retain(|path, _| directories.contains(path));
        for directory in directories {
            expansion
                .entry(directory.clone())
                .or_insert(default_expanded);
        }
    }

    fn rebuild_visible_rows(&mut self) {
        self.visible_rows = self
            .entries_for_scope(TreeScope::AllFiles)
            .iter()
            .filter(|entry| {
                entry
                    .relative
                    .ancestors()
                    .skip(1)
                    .filter(|ancestor| !ancestor.as_os_str().is_empty())
                    .all(|ancestor| {
                        self.all_files_expansion
                            .get(ancestor)
                            .copied()
                            .unwrap_or(false)
                    })
            })
            .cloned()
            .collect();
        self.visible_git_rows = self
            .git_rows
            .iter()
            .filter(|row| {
                row.ancestors.iter().all(|ancestor| {
                    self.git_changes_expansion
                        .get(ancestor)
                        .copied()
                        .unwrap_or(true)
                })
            })
            .cloned()
            .collect();
        self.visible_changed_entries = self
            .visible_git_rows
            .iter()
            .filter_map(|row| row.file_entry.clone())
            .collect();
        self.normalize_tree_state();
    }

    fn entries_for_scope(&self, scope: TreeScope) -> &[FileEntry] {
        match scope {
            TreeScope::AllFiles => &self.all_entries,
            TreeScope::GitChanges => &self.changed_entries,
            #[cfg(feature = "agent-observability")]
            TreeScope::Agents => &[],
        }
    }

    fn restore_visible_selection(&mut self, preferred: Option<PathBuf>) {
        let index = preferred
            .as_deref()
            .and_then(|path| self.visible_index_for_path(path))
            .or_else(|| {
                preferred
                    .as_deref()
                    .and_then(|path| self.nearest_visible_ancestor_index(path))
            })
            .or_else(|| self.default_selection_index());
        self.select_optional(index);
    }

    fn restore_git_selection(&mut self, preferred: Option<GitRowIdentity>) {
        self.restore_git_selection_inner(preferred, true);
    }

    fn restore_git_selection_inner(
        &mut self,
        preferred: Option<GitRowIdentity>,
        load_content: bool,
    ) {
        let index = preferred
            .as_ref()
            .and_then(|identity| {
                self.visible_git_rows
                    .iter()
                    .position(|row| &row.identity == identity)
            })
            .or_else(|| {
                preferred.as_ref().and_then(|identity| {
                    self.git_rows
                        .iter()
                        .find(|row| &row.identity == identity)
                        .and_then(|row| {
                            row.ancestors.iter().rev().find_map(|ancestor| {
                                self.visible_git_rows
                                    .iter()
                                    .position(|candidate| &candidate.identity == ancestor)
                            })
                        })
                })
            })
            .or_else(|| self.default_selection_index());
        if load_content {
            self.select_optional(index);
        } else {
            self.tree_state.select(index);
            self.normalize_tree_state();
            self.remember_current_selection();
        }
    }

    #[cfg(feature = "agent-observability")]
    fn restore_agent_selection(&mut self) {
        let index = self
            .agent_selection
            .as_ref()
            .and_then(|selected| {
                self.agent_view
                    .sessions
                    .iter()
                    .position(|candidate| &candidate.session == selected)
            })
            .or_else(|| (!self.agent_view.sessions.is_empty()).then_some(0));
        self.select_optional(index);
    }

    fn visible_index_for_path(&self, path: &Path) -> Option<usize> {
        self.visible_entries()
            .iter()
            .position(|entry| entry.relative == path)
    }

    fn nearest_visible_ancestor_index(&self, path: &Path) -> Option<usize> {
        path.ancestors()
            .filter(|ancestor| !ancestor.as_os_str().is_empty())
            .find_map(|ancestor| self.visible_index_for_path(ancestor))
    }

    fn normalize_tree_state(&mut self) {
        let row_count = self.tree_row_count();
        if row_count == 0 {
            self.tree_state.select(None);
            *self.tree_state.offset_mut() = 0;
            return;
        }

        if self
            .tree_state
            .selected()
            .is_some_and(|selected| selected >= row_count)
        {
            self.tree_state.select(Some(row_count - 1));
        }
        let offset = self.tree_state.offset().min(row_count - 1);
        *self.tree_state.offset_mut() = offset;
    }

    fn load_scope_default_content(&mut self) {
        if self.tree_row_count() == 0 {
            let message = match (self.tree_scope, self.scope_is_truncated()) {
                (TreeScope::AllFiles, false) => "This directory is empty.",
                (TreeScope::AllFiles, true) => {
                    "No filesystem entries found before the scan limit; results are partial."
                }
                (TreeScope::GitChanges, false) if self.total_repository_count == 0 => {
                    "Workspace is not a Git repository and has no changed descendant repositories."
                }
                (TreeScope::GitChanges, false) => {
                    "No uncommitted Git changes in visible repositories."
                }
                (TreeScope::GitChanges, true) => {
                    "No Git changes found in the partial filesystem results."
                }
                #[cfg(feature = "agent-observability")]
                (TreeScope::Agents, _) => "No observed Agent sessions in this workspace.",
            };
            self.set_info(vec![message.to_owned()]);
            return;
        }

        match self.tree_scope {
            TreeScope::AllFiles => self.load_selected_preview(),
            TreeScope::GitChanges => self.load_selected_diff(),
            #[cfg(feature = "agent-observability")]
            TreeScope::Agents => self.load_selected_agent_detail(),
        }
    }

    #[cfg(feature = "agent-observability")]
    fn load_selected_agent_detail(&mut self) {
        let Some(session) = self.selected_agent_session().cloned() else {
            self.set_info(vec!["No observed Agent session selected.".to_owned()]);
            return;
        };
        let observers = if session.observers.is_empty() {
            "none".to_owned()
        } else {
            session
                .observers
                .iter()
                .map(|observer| observer.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        let mut lines = vec![
            format!("Subject       {}", session.subject.as_str()),
            format!("Session       {}", session.short_key()),
            format!(
                "Discovery     {}",
                match session.discovery {
                    crate::agent::SessionDiscovery::StartConfirmed => "Start confirmed",
                    crate::agent::SessionDiscovery::DiscoveredMidSession => "Mid-session",
                }
            ),
            format!(
                "Mode          {}",
                match session.mode {
                    ObservationMode::MetadataOnly => "Metadata only",
                    ObservationMode::LiveObserved => "Live observed",
                }
            ),
            format!("Lifecycle     {}", lifecycle_label(session.lifecycle)),
            format!("Activity      {}", activity_label(session.activity)),
            format!("Freshness     {}", freshness_label(session.freshness)),
            format!(
                "Coverage      {:?}{} · gaps {} · dropped {}",
                session.completeness,
                if session.reconciling {
                    " · Reconciling"
                } else {
                    ""
                },
                session.gap_count,
                session.dropped_live_events
            ),
            format!("Observers     {observers}"),
            format!(
                "Agents        {}/{} live{}",
                session.live_agents,
                session.known_agents,
                if session.agents_truncated {
                    "+ · Partial"
                } else {
                    ""
                }
            ),
            format!("Turns         {} live", session.turns),
            format!("Changes       {} live-only", session.changes),
            format!("Artifacts     {} live-only", session.artifacts),
            String::new(),
            "Observer coverage".to_owned(),
        ];
        for coverage in &session.coverage.observers {
            let instance = coverage
                .instance
                .as_ref()
                .map(|instance| {
                    instance
                        .digest()
                        .to_hex()
                        .chars()
                        .take(8)
                        .collect::<String>()
                })
                .unwrap_or_else(|| "metadata".to_owned());
            lines.push(format!(
                "{}@{} · since {} · snapshot {:?} · gaps {}{}",
                coverage.observer.as_str(),
                instance,
                coverage.observing_since.as_unix_millis(),
                coverage.snapshot_completeness,
                coverage.stream_gap_count,
                if coverage.reconciling {
                    " · Reconciling"
                } else {
                    ""
                }
            ));
        }
        lines.extend([String::new(), "Explain".to_owned()]);
        for decision in &session.decisions {
            let observer = decision
                .winning_observer
                .as_ref()
                .map(ObserverId::as_str)
                .unwrap_or("none");
            lines.push(format!(
                "{:?}: {:?} · {:?} · {:?} · observer {}",
                decision.domain,
                decision.effective_value,
                decision.disposition,
                decision.authority,
                observer
            ));
        }
        self.set_info(lines);
    }

    fn load_selected_info(&mut self) {
        #[cfg(feature = "agent-observability")]
        if self.tree_scope == TreeScope::Agents {
            self.load_selected_agent_detail();
            return;
        }
        if let Some(row) = self.selected_git_row().cloned() {
            let action = if row.is_container() {
                if self.git_row_is_expanded(&row) {
                    "Expanded · Enter or click to collapse."
                } else {
                    "Collapsed · Enter or click to expand."
                }
            } else {
                ""
            };
            let mut lines = match row.kind {
                GitRowKind::Repository {
                    kind,
                    change_count,
                    status_error,
                    ..
                } => {
                    let mut lines = vec![format!(
                        "{} repository · {} changed file{}.",
                        repo_kind_label(kind),
                        change_count,
                        if change_count == 1 { "" } else { "s" }
                    )];
                    if !row.detail.is_empty() {
                        lines.push(row.detail);
                    }
                    if let Some(error) = status_error {
                        lines.push(format!("Status error: {error}"));
                    }
                    lines
                }
                GitRowKind::Directory => {
                    let selected = match &row.identity {
                        GitRowIdentity::Directory(path) => path,
                        _ => unreachable!("directory rows use directory identities"),
                    };
                    let count = self
                        .git_rows
                        .iter()
                        .filter(|candidate| {
                            matches!(
                                &candidate.identity,
                                GitRowIdentity::Change(path)
                                    if path.repo_id == selected.repo_id
                            ) && candidate
                                .file_entry()
                                .is_some_and(|entry| entry.relative.starts_with(&selected.relative))
                        })
                        .count();
                    vec![format!(
                        "{count} changed file{} in this directory.",
                        if count == 1 { "" } else { "s" }
                    )]
                }
                GitRowKind::Issue(message) => vec![message],
                GitRowKind::Pointer(_) => {
                    vec!["Submodule pointer change in the parent repository.".to_owned()]
                }
                GitRowKind::Change(_) => Vec::new(),
            };
            if !action.is_empty() {
                lines.push(String::new());
                lines.push(action.to_owned());
            }
            self.set_info(lines);
            return;
        }
        let Some(entry) = self.selected_entry() else {
            self.set_info(vec!["No file selected.".to_owned()]);
            return;
        };
        let relative = entry.relative.clone();
        // When the selected entry is a symbolic link, surface where it points.
        // Directory links reach this info pane (they never enter Preview mode),
        // so this is the only place their real target is shown.
        let symlink_lines = entry.symlink_target.as_ref().map(|target| {
            let absolute = self.root.join(&entry.relative);
            let real = absolute.canonicalize().unwrap_or(absolute);
            vec![
                format!("⇢ symlink → {}", target.display()),
                format!("↗ resolves to {}", real.display()),
                String::new(),
            ]
        });
        let expanded = self.directory_is_expanded(entry);
        let count = self
            .entries_for_scope(self.tree_scope)
            .iter()
            .filter(|candidate| {
                candidate.status.is_some() && candidate.relative.starts_with(&relative)
            })
            .count();
        let summary = match count {
            0 => "No changed files in this directory.".to_owned(),
            1 => "1 changed file in this directory.".to_owned(),
            count => format!("{count} changed files in this directory."),
        };
        let action = if expanded {
            if self.loading_directories.contains(&relative) {
                "Loading this directory…"
            } else {
                "Expanded · Enter or click to collapse."
            }
        } else {
            "Collapsed · Enter or click to expand."
        };
        let mut info = symlink_lines.unwrap_or_default();
        info.push(summary);
        info.push(String::new());
        info.push(action.to_owned());
        self.set_info(info);
    }

    fn load_selected_diff(&mut self) {
        #[cfg(feature = "agent-observability")]
        if self.tree_scope == TreeScope::Agents {
            self.load_selected_agent_detail();
            return;
        }
        if self.tree_scope == TreeScope::GitChanges {
            let Some(row) = self.selected_git_row().cloned() else {
                self.set_info(vec!["No repository row selected.".to_owned()]);
                return;
            };
            match row.kind {
                GitRowKind::Change(change) => {
                    let label = row.label;
                    let review_path = change.path.clone();
                    self.request_reviewable_diff(
                        label,
                        ContentTarget::Repository(change),
                        review_path,
                    );
                }
                GitRowKind::Pointer(change) => {
                    let label = row.label;
                    self.request_content(
                        ContentKind::Diff,
                        label,
                        ContentTarget::Repository(change),
                    );
                }
                GitRowKind::Repository { .. } | GitRowKind::Directory | GitRowKind::Issue(_) => {
                    self.load_selected_info()
                }
            }
            return;
        }
        let Some(entry) = self.selected_entry() else {
            self.set_info(vec!["No file selected.".to_owned()]);
            return;
        };
        if entry.is_dir {
            self.load_selected_info();
            return;
        }
        let relative = entry.relative.clone();
        let Some(change) = self.change_for_workspace_path(&relative) else {
            self.set_info(vec![
                format!("{} has no uncommitted changes.", relative.display()),
                "Select a changed file to inspect its diff.".to_owned(),
            ]);
            return;
        };

        let review_path = change.path.clone();
        self.request_reviewable_diff(
            relative.display().to_string(),
            ContentTarget::Repository(change),
            review_path,
        );
    }

    fn load_selected_preview(&mut self) {
        #[cfg(feature = "agent-observability")]
        if self.tree_scope == TreeScope::Agents {
            self.load_selected_agent_detail();
            return;
        }
        if self.tree_scope == TreeScope::GitChanges {
            let Some(row) = self.selected_git_row().cloned() else {
                self.set_info(vec!["No repository row selected.".to_owned()]);
                return;
            };
            match row.kind {
                GitRowKind::Change(change) => {
                    if !row.exists {
                        self.set_info(vec![format!(
                            "{} no longer exists in the working tree.",
                            row.label
                        )]);
                        return;
                    }
                    self.request_content(
                        ContentKind::Preview,
                        row.label,
                        ContentTarget::Repository(change),
                    );
                }
                GitRowKind::Pointer(_) => self.set_info(vec![
                    "A submodule pointer has no file preview.".to_owned(),
                    "Press d to inspect the parent repository Gitlink diff.".to_owned(),
                ]),
                GitRowKind::Repository { .. } | GitRowKind::Directory | GitRowKind::Issue(_) => {
                    self.load_selected_info()
                }
            }
            return;
        }
        let Some(entry) = self.selected_entry() else {
            self.set_info(vec!["No file selected.".to_owned()]);
            return;
        };
        if entry.is_dir {
            self.load_selected_info();
            return;
        }
        let relative = entry.relative.clone();
        if !entry.exists {
            self.set_info(vec![format!(
                "{} no longer exists in the working tree.",
                relative.display()
            )]);
            return;
        }

        self.remember_recent_file(&relative);

        self.request_content(
            ContentKind::Preview,
            relative.display().to_string(),
            ContentTarget::Workspace(relative),
        );
    }

    fn request_content(&mut self, kind: ContentKind, label: String, target: ContentTarget) -> u64 {
        self.request_content_with_review_path(kind, label, target, None)
    }

    fn request_reviewable_diff(
        &mut self,
        label: String,
        target: ContentTarget,
        review_path: RepoPath,
    ) -> u64 {
        self.request_content_with_review_path(ContentKind::Diff, label, target, Some(review_path))
    }

    fn request_content_with_review_path(
        &mut self,
        kind: ContentKind,
        label: String,
        target: ContentTarget,
        review_path: Option<RepoPath>,
    ) -> u64 {
        self.cancel_pending_navigation();
        self.navigation_picker = None;
        self.navigation_target_highlight = None;
        let generation = self.content_requests.begin();
        self.cache_current_folds();
        self.reset_content(match kind {
            ContentKind::Diff => ContentMode::Diff,
            ContentKind::Preview => ContentMode::Preview,
        });
        self.pending_diff_path = review_path.map(|path| (generation, path));
        self.content_lines = vec![format!("Loading {label}…")];
        self.runtime.request_content(ContentRequest {
            generation,
            kind,
            purpose: ContentPurpose::Display,
            target,
        });
        generation
    }

    fn change_for_workspace_path(&self, relative: &Path) -> Option<RepoChange> {
        let absolute = self.root.join(relative);
        self.repo_graph
            .as_ref()?
            .repositories()
            .iter()
            .flat_map(|snapshot| snapshot.changes.iter())
            .find(|change| {
                self.repo_graph
                    .as_ref()
                    .and_then(|graph| graph.repository(&change.path.repo_id))
                    .is_some_and(|snapshot| {
                        snapshot.node.worktree.join(&change.path.relative) == absolute
                    })
            })
            .cloned()
    }

    fn set_navigation_status(&mut self, level: NavigationStatusLevel, message: impl AsRef<str>) {
        let duration = match level {
            NavigationStatusLevel::Info => NAVIGATION_STATUS_INFO,
            NavigationStatusLevel::Error => NAVIGATION_STATUS_ERROR,
        };
        self.navigation_status = Some(NavigationStatus {
            level,
            message: clean_navigation_message(message.as_ref()),
            expires_at: Instant::now() + duration,
        });
    }

    fn next_navigation_generation(&mut self) -> u64 {
        self.navigation_preview_requests.invalidate();
        self.runtime.cancel_pending_content();
        if let Some(invocation) = self.navigation_invocation.take() {
            self.navigation_runtime.cancel(invocation.generation);
        }
        if let Some(stage) = self.pending_navigation_stage.take() {
            self.navigation_runtime.cancel(stage.invocation.generation);
        }
        self.navigation_picker = None;
        self.navigation_hover_highlight = None;
        self.navigation_generation = self
            .navigation_generation
            .checked_add(1)
            .expect("navigation generation exhausted");
        self.navigation_status = None;
        self.navigation_generation
    }

    fn cancel_pending_navigation(&mut self) {
        self.navigation_preview_requests.invalidate();
        self.runtime.cancel_pending_content();
        if let Some(invocation) = self.navigation_invocation.take() {
            self.navigation_runtime.cancel(invocation.generation);
        }
        if let Some(stage) = self.pending_navigation_stage.take() {
            self.navigation_runtime.cancel(stage.invocation.generation);
            self.content_requests.invalidate();
            self.runtime.cancel_pending_content();
        }
        self.navigation_picker = None;
        self.navigation_hover_highlight = None;
        self.navigation_generation = self
            .navigation_generation
            .checked_add(1)
            .expect("navigation generation exhausted");
        self.navigation_status = None;
    }

    fn rebind_navigation_sources_after_refresh(&mut self) {
        let root = &self.root;
        let graph = self.repo_graph.as_ref();
        self.navigation_source = self
            .navigation_source
            .as_deref()
            .and_then(|source| rebind_navigation_source(root, graph, source).map(Arc::new));
        if let Some(search) = &mut self.search {
            search.restore.navigation_source = search
                .restore
                .navigation_source
                .as_deref()
                .and_then(|source| rebind_navigation_source(root, graph, source).map(Arc::new));
        }
    }

    fn rebind_content_snapshot_navigation(&self, snapshot: &mut ContentSnapshot) {
        snapshot.navigation_source = snapshot.navigation_source.as_ref().and_then(|source| {
            rebind_navigation_source(&self.root, self.repo_graph.as_ref(), source)
        });
    }

    fn current_navigation_entry(&self) -> Option<NavigationHistoryEntry> {
        let source = self.navigation_source.as_ref()?;
        let rows = self.content_visual_rows(self.ui_regions.content_inner.width.max(1));
        let effective_scroll = self.effective_content_scroll(rows.len());
        let row = rows.get(effective_scroll);
        let viewport = ContentViewportRestore {
            line: row.map(|row| row.line_index),
            byte_start: row.map_or(0, |row| row.byte_range.start),
            synthetic: row.is_some_and(|row| row.synthetic),
            effective_scroll,
        };
        let point = self.navigation_caret.point;
        Some(NavigationHistoryEntry {
            target: NavigationTarget {
                document: source.identity.clone(),
                range: NavigationTargetRange::Source(SourceRange {
                    start: point,
                    end: point,
                }),
            },
            viewport,
        })
    }

    fn request_semantic_navigation(&mut self, operation: NavigationOperation) {
        if self.focused_pane != FocusPane::Content || self.content_mode != ContentMode::Preview {
            self.set_navigation_status(NavigationStatusLevel::Info, "Focus Preview to navigate.");
            return;
        }
        let Some(source) = self.navigation_source.clone() else {
            self.set_navigation_status(
                NavigationStatusLevel::Error,
                "Navigation unavailable: preview is truncated or unsupported.",
            );
            return;
        };
        let generation = self.next_navigation_generation();
        if !source.structure.recognizable_tokens.complete {
            self.set_navigation_status(
                NavigationStatusLevel::Error,
                "Navigation token index is incomplete; refresh Preview.",
            );
            return;
        }
        let Some(token) = source
            .structure
            .recognizable_tokens
            .containing(self.navigation_caret.point)
        else {
            self.set_navigation_status(NavigationStatusLevel::Info, "No navigable token at caret.");
            return;
        };
        let Some(mut origin) = self.current_navigation_entry() else {
            return;
        };
        origin.target.range = NavigationTargetRange::Source(token);
        let invocation = NavigationInvocation {
            generation,
            operation,
            source_identity: source.identity.clone(),
            source_version: self.navigation_document_version,
            origin,
            history_intent: NavigationHistoryIntent::Jump,
            destination_viewport: None,
            return_focus: self.focused_pane,
        };
        let request = NavigationRuntimeRequest {
            generation,
            operation,
            origin: self.navigation_caret.point,
            source,
            version: self.navigation_document_version,
        };
        self.navigation_invocation = Some(invocation);
        if let Err(error) = self.navigation_runtime.request(request) {
            self.navigation_invocation = None;
            self.set_navigation_status(NavigationStatusLevel::Error, format!("{error:#}"));
            return;
        }
        self.set_navigation_status(
            NavigationStatusLevel::Info,
            match operation {
                NavigationOperation::Definition => "Finding definition…",
                NavigationOperation::References => "Finding references…",
                NavigationOperation::Implementations => "Finding implementations…",
                NavigationOperation::DocumentSymbols => "Finding document symbols…",
            },
        );
    }

    fn open_document_symbols(&mut self) {
        if self.focused_pane != FocusPane::Content || self.content_mode != ContentMode::Preview {
            self.set_navigation_status(NavigationStatusLevel::Info, "Focus Preview to navigate.");
            return;
        }
        let Some(source) = self.navigation_source.clone() else {
            self.set_navigation_status(
                NavigationStatusLevel::Error,
                "Document symbols unavailable for this Preview.",
            );
            return;
        };
        let generation = self.next_navigation_generation();
        if source.structure.symbols_complete && source.structure.symbols.is_empty() {
            self.set_navigation_status(NavigationStatusLevel::Info, "No document symbols found.");
            return;
        }
        let Some(origin) = self.current_navigation_entry() else {
            return;
        };
        let invocation = NavigationInvocation {
            generation,
            operation: NavigationOperation::DocumentSymbols,
            source_identity: source.identity.clone(),
            source_version: self.navigation_document_version,
            origin,
            history_intent: NavigationHistoryIntent::Jump,
            destination_viewport: None,
            return_focus: self.focused_pane,
        };
        if !source.structure.symbols_complete {
            let request = NavigationRuntimeRequest {
                generation,
                operation: NavigationOperation::DocumentSymbols,
                origin: self.navigation_caret.point,
                source,
                version: self.navigation_document_version,
            };
            self.navigation_invocation = Some(invocation);
            if let Err(error) = self.navigation_runtime.request(request) {
                self.navigation_invocation = None;
                self.set_navigation_status(NavigationStatusLevel::Error, format!("{error:#}"));
                return;
            }
            self.set_navigation_status(NavigationStatusLevel::Info, "Finding document symbols…");
            return;
        }
        let mut depths = HashMap::<SymbolId, usize>::new();
        let results = source
            .structure
            .symbols
            .iter()
            .map(|symbol| {
                let depth = symbol
                    .parent
                    .and_then(|parent| depths.get(&parent).copied())
                    .map_or(0, |depth| depth.saturating_add(1));
                depths.insert(symbol.id, depth);
                let position = source
                    .line_index
                    .to_utf16(symbol.selection_range.start)
                    .unwrap_or_default();
                NavigationPickerItem {
                    target: NavigationTarget {
                        document: source.identity.clone(),
                        range: NavigationTargetRange::Source(symbol.selection_range),
                    },
                    label: format!(
                        "{}{}:{}:{} · {}",
                        "  ".repeat(depth),
                        source.identity.display_label(),
                        position.line.saturating_add(1),
                        position.character.saturating_add(1),
                        symbol.name
                    ),
                    detail: symbol.detail.clone().or_else(|| symbol.container.clone()),
                }
            })
            .collect();
        self.open_navigation_picker("Document Symbols", invocation, results);
    }

    fn apply_document_symbol_completion(&mut self, completion: NavigationDocumentSymbolCompletion) {
        let Some(invocation) = self.navigation_invocation.take() else {
            return;
        };
        if invocation.operation != NavigationOperation::DocumentSymbols
            || completion.generation != invocation.generation
            || completion.source_identity != invocation.source_identity
            || completion.source_version != invocation.source_version
        {
            self.navigation_invocation = Some(invocation);
            return;
        }
        let Some(source) = self.navigation_source.as_ref() else {
            self.set_navigation_status(
                NavigationStatusLevel::Error,
                "Document symbol source is no longer available.",
            );
            return;
        };
        if source.identity != completion.source_identity
            || self.navigation_document_version != completion.source_version
        {
            // The runtime triple matched the invocation, but the visible
            // document moved on before this reducer turn.
            self.navigation_status = None;
            return;
        }
        if completion.symbols.is_empty() {
            self.set_navigation_status(NavigationStatusLevel::Info, "No document symbols found.");
            return;
        }
        let results = match protocol_symbol_picker_items(source, completion.symbols) {
            Ok(results) => results,
            Err(error) => {
                self.set_navigation_status(
                    NavigationStatusLevel::Error,
                    format!("Invalid document symbol range: {error:#}"),
                );
                return;
            }
        };
        self.open_navigation_picker("Document Symbols", invocation, results);
    }

    fn open_navigation_picker(
        &mut self,
        title: impl Into<String>,
        invocation: NavigationInvocation,
        results: Vec<NavigationPickerItem>,
    ) {
        let groups = navigation_picker_groups(&results);
        let visible_rows = navigation_picker_rows(&groups);
        let mut list_state = ListState::default();
        list_state.select(
            visible_rows
                .iter()
                .position(|row| matches!(row, NavigationPickerRow::Result(_))),
        );
        self.navigation_picker = Some(NavigationPickerState {
            title: title.into(),
            return_focus: invocation.return_focus,
            invocation,
            results,
            groups,
            visible_rows,
            list_state,
            preview: None,
            preview_loading: false,
            preview_error: None,
        });
        self.navigation_invocation = None;
        self.navigation_status = None;
        self.request_navigation_picker_preview();
    }

    fn apply_navigation_completion(&mut self, completion: NavigationRuntimeCompletion) {
        let Some(invocation) = self.navigation_invocation.take() else {
            return;
        };
        if completion.generation != invocation.generation
            || completion.source_identity != invocation.source_identity
            || completion.source_version != invocation.source_version
            || completion.operation != invocation.operation
        {
            self.navigation_invocation = Some(invocation);
            return;
        }
        match completion.result {
            NavigationProtocolResult::Locations(locations) => {
                self.reduce_protocol_locations(invocation, locations);
            }
            NavigationProtocolResult::Unavailable(message) => {
                self.set_navigation_status(NavigationStatusLevel::Error, message);
            }
            NavigationProtocolResult::Failed(message) => {
                self.set_navigation_status(NavigationStatusLevel::Error, message);
            }
            NavigationProtocolResult::Cancelled => {
                self.navigation_status = None;
            }
        }
    }

    fn reduce_protocol_locations(
        &mut self,
        invocation: NavigationInvocation,
        locations: Vec<ProtocolLocation>,
    ) {
        let had_locations = !locations.is_empty();
        let source_server_root = self
            .navigation_source
            .as_ref()
            .filter(|source| source.identity == invocation.source_identity)
            .map(|source| source.server_root.clone());
        let mut targets: Vec<_> = locations
            .into_iter()
            .filter_map(|location| {
                let document = match lsp_uri_to_navigation_target(&location.uri, &self.root).ok()? {
                    NavigationFileTarget::Workspace(absolute) => {
                        ContentIdentity::from_absolute(&self.root, &absolute)?
                    }
                    NavigationFileTarget::Dependency(dependency) => {
                        let absolute = dependency.root.join(&dependency.relative);
                        ContentIdentity::dependency(
                            dependency.root,
                            &absolute,
                            source_server_root.clone()?,
                        )?
                    }
                };
                Some(NavigationTarget {
                    document,
                    range: NavigationTargetRange::Utf16(location.range),
                })
            })
            .collect();
        targets.sort_by(|left, right| {
            navigation_target_sort_key(left).cmp(&navigation_target_sort_key(right))
        });
        targets.dedup();
        if targets.is_empty() {
            self.set_navigation_status(
                if had_locations {
                    NavigationStatusLevel::Error
                } else {
                    NavigationStatusLevel::Info
                },
                if had_locations {
                    "Navigation target is outside the opened workspace or unsafe."
                } else {
                    match invocation.operation {
                        NavigationOperation::Definition => "No definition found.",
                        NavigationOperation::References => "No references found.",
                        NavigationOperation::Implementations => "No implementations found.",
                        NavigationOperation::DocumentSymbols => "No document symbols found.",
                    }
                },
            );
            return;
        }
        let items = targets
            .into_iter()
            .map(|target| NavigationPickerItem {
                label: navigation_target_label(&target),
                target,
                detail: None,
            })
            .collect::<Vec<_>>();
        let direct = invocation.operation == NavigationOperation::Definition && items.len() == 1;
        if direct {
            self.accept_navigation_target(invocation, items[0].target.clone());
        } else {
            let title = match invocation.operation {
                NavigationOperation::Definition => "Definitions",
                NavigationOperation::References => "References",
                NavigationOperation::Implementations => "Implementations",
                NavigationOperation::DocumentSymbols => "Document Symbols",
            };
            self.open_navigation_picker(title, invocation, items);
        }
    }

    fn accept_navigation_target(
        &mut self,
        invocation: NavigationInvocation,
        target: NavigationTarget,
    ) {
        if self
            .navigation_source
            .as_ref()
            .is_some_and(|source| source.identity == target.document)
        {
            let Some(range) = self.resolve_target_in_current_document(&target) else {
                self.set_navigation_status(
                    NavigationStatusLevel::Error,
                    "Navigation target range is invalid.",
                );
                return;
            };
            self.commit_navigation_reveal(&invocation, target.document.clone(), range);
            return;
        }
        let content_generation = self.content_requests.begin();
        self.runtime.request_content(ContentRequest {
            generation: content_generation,
            kind: ContentKind::Preview,
            purpose: ContentPurpose::NavigationStage {
                navigation_generation: invocation.generation,
            },
            target: content_target_for_navigation(&target.document),
        });
        self.pending_navigation_stage = Some(PendingNavigationStage {
            invocation,
            content_generation,
            target,
        });
        self.navigation_picker = None;
        self.set_navigation_status(NavigationStatusLevel::Info, "Loading navigation target…");
    }

    fn resolve_target_in_current_document(&self, target: &NavigationTarget) -> Option<SourceRange> {
        let source = self.navigation_source.as_ref()?;
        if source.identity != target.document {
            return None;
        }
        match target.range.clone() {
            NavigationTargetRange::Source(range) => source
                .line_index
                .to_utf16(range.start)
                .and_then(|_| source.line_index.to_utf16(range.end))
                .ok()
                .map(|_| range),
            NavigationTargetRange::Utf16(range) => source.line_index.range_from_utf16(range).ok(),
        }
    }

    fn apply_navigation_stage_completion(
        &mut self,
        navigation_generation: u64,
        content_generation: u64,
        result: Result<ContentSnapshot, String>,
    ) {
        let Some(stage) = self.pending_navigation_stage.take() else {
            return;
        };
        if stage.invocation.generation != navigation_generation
            || stage.content_generation != content_generation
            || self.navigation_generation != navigation_generation
        {
            return;
        }
        let mut snapshot = match result {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.set_navigation_status(
                    NavigationStatusLevel::Error,
                    format!("Unable to load navigation target: {error}"),
                );
                return;
            }
        };
        self.rebind_content_snapshot_navigation(&mut snapshot);
        let Some(source) = snapshot.navigation_source.as_ref() else {
            self.set_navigation_status(
                NavigationStatusLevel::Error,
                "Navigation target preview is truncated or unsupported.",
            );
            return;
        };
        if source.identity != stage.target.document {
            self.set_navigation_status(
                NavigationStatusLevel::Error,
                "Navigation target identity changed while loading.",
            );
            return;
        }
        let range = match stage.target.range.clone() {
            NavigationTargetRange::Source(range) => source
                .line_index
                .to_utf16(range.start)
                .and_then(|_| source.line_index.to_utf16(range.end))
                .map(|_| range),
            NavigationTargetRange::Utf16(range) => source.line_index.range_from_utf16(range),
        };
        let Ok(range) = range else {
            self.set_navigation_status(
                NavigationStatusLevel::Error,
                "Navigation target range is invalid.",
            );
            return;
        };
        self.install_navigation_snapshot(snapshot);
        self.commit_navigation_reveal(&stage.invocation, stage.target.document, range);
    }

    fn install_navigation_snapshot(&mut self, snapshot: ContentSnapshot) {
        self.cache_current_folds();
        self.reset_content(ContentMode::Preview);
        let cached = snapshot
            .identity
            .as_ref()
            .map_or_else(HashSet::new, |identity| self.cached_folds(identity));
        self.content_provider = snapshot.provider;
        self.content_lines = snapshot.lines;
        self.content_highlights = snapshot.highlights;
        self.content_show_line_numbers = snapshot.show_line_numbers;
        self.content_identity = snapshot.identity;
        self.content_fold_source = snapshot.fold_source;
        self.content_fold_regions = snapshot.fold_regions;
        self.content_structure = snapshot.structure;
        self.navigation_source = snapshot.navigation_source.map(Arc::new);
        let valid_anchors: HashSet<_> = self
            .content_fold_regions
            .iter()
            .map(|region| region.anchor)
            .collect();
        self.content_collapsed_folds = cached.intersection(&valid_anchors).copied().collect();
        self.content_successful = true;
        self.navigation_document_version = DocumentVersion(
            self.navigation_document_version
                .0
                .checked_add(1)
                .expect("navigation document version exhausted"),
        );
    }

    fn commit_navigation_reveal(
        &mut self,
        invocation: &NavigationInvocation,
        document: ContentIdentity,
        range: SourceRange,
    ) {
        self.reveal_folded_line(range.start.line);
        self.scroll_to_logical_line(range.start.line, range.start.byte);
        if let Some(viewport) = invocation.destination_viewport {
            self.restore_content_viewport(viewport);
        }
        self.navigation_caret = NavigationCaret {
            point: range.start,
            preferred_display_column: 0,
        };
        self.navigation_target_highlight = Some(range);
        self.focused_pane = FocusPane::Content;
        if let Some(workspace_path) = document.workspace_path() {
            if self.tree_scope != TreeScope::AllFiles {
                self.apply_tree_scope(TreeScope::AllFiles);
            }
            self.reveal_navigation_tree_selection(workspace_path.to_path_buf());
        }
        match invocation.history_intent {
            NavigationHistoryIntent::Jump => {
                push_bounded_history(&mut self.navigation_back, invocation.origin.clone());
                self.navigation_forward.clear();
            }
            NavigationHistoryIntent::Back => {
                self.navigation_back.pop_back();
                push_bounded_history(&mut self.navigation_forward, invocation.origin.clone());
            }
            NavigationHistoryIntent::Forward => {
                self.navigation_forward.pop_back();
                push_bounded_history(&mut self.navigation_back, invocation.origin.clone());
            }
        }
        self.navigation_invocation = None;
        self.pending_navigation_stage = None;
        self.navigation_picker = None;
        self.set_navigation_status(NavigationStatusLevel::Info, "Navigation target opened.");
    }

    fn navigate_history(&mut self, intent: NavigationHistoryIntent) {
        let target = match intent {
            NavigationHistoryIntent::Back => self.navigation_back.back(),
            NavigationHistoryIntent::Forward => self.navigation_forward.back(),
            NavigationHistoryIntent::Jump => None,
        }
        .cloned();
        let Some(target) = target else {
            self.set_navigation_status(
                NavigationStatusLevel::Info,
                if intent == NavigationHistoryIntent::Back {
                    "No previous navigation location."
                } else {
                    "No forward navigation location."
                },
            );
            return;
        };
        let Some(source_identity) = self
            .navigation_source
            .as_ref()
            .map(|source| source.identity.clone())
        else {
            return;
        };
        let Some(origin) = self.current_navigation_entry() else {
            return;
        };
        let generation = self.next_navigation_generation();
        let invocation = NavigationInvocation {
            generation,
            operation: NavigationOperation::Definition,
            source_identity,
            source_version: self.navigation_document_version,
            origin,
            history_intent: intent,
            destination_viewport: Some(target.viewport),
            return_focus: self.focused_pane,
        };
        self.accept_navigation_target(invocation, target.target);
    }

    fn handle_navigation_picker_key(&mut self, key: KeyEvent) {
        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) => self.close_navigation_picker(),
            (KeyCode::Down, _) => self.move_navigation_picker(1),
            (KeyCode::Up, _) => self.move_navigation_picker(-1),
            (KeyCode::PageDown, _) => self.move_navigation_picker(10),
            (KeyCode::PageUp, _) => self.move_navigation_picker(-10),
            (KeyCode::Enter, _) => self.accept_navigation_picker_selection(),
            _ => {}
        }
    }

    fn handle_navigation_picker_mouse_down(&mut self, mouse: MouseEvent) {
        if !contains(self.ui_regions.navigation_results, mouse.column, mouse.row) {
            return;
        }
        let visible_row = usize::from(mouse.row - self.ui_regions.navigation_results.y);
        let (offset, count) = self.navigation_picker.as_ref().map_or((0, 0), |picker| {
            (picker.list_state.offset(), picker.visible_rows.len())
        });
        let index = offset.saturating_add(visible_row);
        if index >= count {
            return;
        }
        if let Some(picker) = self.navigation_picker.as_mut() {
            picker.list_state.select(Some(index));
        }
        // A concrete location has one unambiguous destination, so one click
        // commits it just like Enter. File group headers remain structural
        // controls: selecting one only toggles its expansion.
        self.accept_navigation_picker_selection();
    }

    fn move_navigation_picker(&mut self, delta: isize) {
        let Some(picker) = self.navigation_picker.as_mut() else {
            return;
        };
        if picker.visible_rows.is_empty() {
            return;
        }
        let current = picker.list_state.selected().unwrap_or(0);
        picker.list_state.select(Some(
            current
                .saturating_add_signed(delta)
                .min(picker.visible_rows.len().saturating_sub(1)),
        ));
        self.request_navigation_picker_preview();
    }

    fn accept_navigation_picker_selection(&mut self) {
        let selected = self.navigation_picker.as_ref().and_then(|picker| {
            picker
                .list_state
                .selected()
                .and_then(|index| picker.visible_rows.get(index).copied())
        });
        let Some(selected) = selected else {
            return;
        };
        if let NavigationPickerRow::Group(group_index) = selected {
            if let Some(picker) = self.navigation_picker.as_mut()
                && let Some(group) = picker.groups.get_mut(group_index)
            {
                group.expanded = !group.expanded;
                picker.visible_rows = navigation_picker_rows(&picker.groups);
                picker.list_state.select(
                    picker
                        .visible_rows
                        .iter()
                        .position(|row| *row == NavigationPickerRow::Group(group_index)),
                );
            }
            self.request_navigation_picker_preview();
            return;
        }
        let NavigationPickerRow::Result(result_index) = selected else {
            return;
        };
        self.navigation_preview_requests.invalidate();
        self.runtime.cancel_pending_content();
        let Some(picker) = self.navigation_picker.take() else {
            return;
        };
        let Some(item) = picker.results.get(result_index) else {
            return;
        };
        self.accept_navigation_target(picker.invocation, item.target.clone());
    }

    fn close_navigation_picker(&mut self) {
        self.navigation_preview_requests.invalidate();
        self.runtime.cancel_pending_content();
        if let Some(picker) = self.navigation_picker.take() {
            self.focused_pane = picker.return_focus;
        }
    }

    fn request_navigation_picker_preview(&mut self) {
        let selected_target = self.navigation_picker.as_ref().and_then(|picker| {
            let row = picker
                .list_state
                .selected()
                .and_then(|index| picker.visible_rows.get(index))?;
            let NavigationPickerRow::Result(result_index) = *row else {
                return None;
            };
            picker
                .results
                .get(result_index)
                .map(|item| item.target.clone())
        });
        let Some(target) = selected_target else {
            self.navigation_preview_requests.invalidate();
            self.runtime.cancel_pending_content();
            if let Some(picker) = self.navigation_picker.as_mut() {
                picker.preview = None;
                picker.preview_loading = false;
                picker.preview_error = None;
            }
            return;
        };

        if self
            .navigation_source
            .as_ref()
            .is_some_and(|source| source.identity == target.document)
        {
            self.navigation_preview_requests.invalidate();
            self.runtime.cancel_pending_content();
            let preview = self
                .resolve_target_in_current_document(&target)
                .map(|range| NavigationPickerPreview {
                    path: target.document.display_path(),
                    lines: self.content_lines.clone(),
                    highlights: self.content_highlights.clone(),
                    target: range,
                });
            if let Some(preview) = preview {
                self.install_navigation_picker_preview(preview);
            } else if let Some(picker) = self.navigation_picker.as_mut() {
                picker.preview_loading = false;
                picker.preview_error = Some("Navigation target range is invalid.".to_owned());
                picker.preview = None;
            }
            return;
        }

        let navigation_generation = self
            .navigation_picker
            .as_ref()
            .map(|picker| picker.invocation.generation)
            .unwrap_or_default();
        let generation = self.navigation_preview_requests.begin();
        if let Some(picker) = self.navigation_picker.as_mut() {
            picker.preview = None;
            picker.preview_loading = true;
            picker.preview_error = None;
        }
        self.runtime.request_content(ContentRequest {
            generation,
            kind: ContentKind::Preview,
            purpose: ContentPurpose::NavigationPreview {
                navigation_generation,
            },
            target: content_target_for_navigation(&target.document),
        });
    }

    fn apply_navigation_preview_completion(
        &mut self,
        navigation_generation: u64,
        result: Result<ContentSnapshot, String>,
    ) {
        let selected_target = self.navigation_picker.as_ref().and_then(|picker| {
            (picker.invocation.generation == navigation_generation)
                .then_some(picker)
                .and_then(|picker| {
                    let row = picker
                        .list_state
                        .selected()
                        .and_then(|index| picker.visible_rows.get(index))?;
                    let NavigationPickerRow::Result(result_index) = *row else {
                        return None;
                    };
                    picker
                        .results
                        .get(result_index)
                        .map(|item| item.target.clone())
                })
        });
        let Some(target) = selected_target else {
            return;
        };
        let preview = match result {
            Ok(mut snapshot) => {
                self.rebind_content_snapshot_navigation(&mut snapshot);
                let Some(source) = snapshot
                    .navigation_source
                    .as_ref()
                    .filter(|source| source.identity == target.document)
                else {
                    if let Some(picker) = self.navigation_picker.as_mut() {
                        picker.preview_loading = false;
                        picker.preview_error =
                            Some("Preview has no safe navigation source.".to_owned());
                    }
                    return;
                };
                let range = match target.range.clone() {
                    NavigationTargetRange::Source(range) => source
                        .line_index
                        .to_utf16(range.start)
                        .and_then(|_| source.line_index.to_utf16(range.end))
                        .map(|_| range),
                    NavigationTargetRange::Utf16(range) => {
                        source.line_index.range_from_utf16(range)
                    }
                };
                let Ok(range) = range else {
                    if let Some(picker) = self.navigation_picker.as_mut() {
                        picker.preview_loading = false;
                        picker.preview_error =
                            Some("Navigation target range is invalid.".to_owned());
                    }
                    return;
                };
                NavigationPickerPreview {
                    path: target.document.display_path(),
                    lines: snapshot.lines,
                    highlights: snapshot.highlights,
                    target: range,
                }
            }
            Err(error) => {
                if let Some(picker) = self.navigation_picker.as_mut() {
                    picker.preview_loading = false;
                    picker.preview_error = Some(clean_navigation_message(&error));
                }
                return;
            }
        };
        self.install_navigation_picker_preview(preview);
    }

    fn install_navigation_picker_preview(&mut self, preview: NavigationPickerPreview) {
        if let Some(picker) = self.navigation_picker.as_mut() {
            let selected_result = picker
                .list_state
                .selected()
                .and_then(|index| picker.visible_rows.get(index))
                .and_then(|row| match row {
                    NavigationPickerRow::Result(index) => Some(*index),
                    NavigationPickerRow::Group(_) => None,
                });
            if let Some(item) = selected_result.and_then(|index| picker.results.get_mut(index))
                && item.detail.is_none()
            {
                item.detail = navigation_preview_summary(&preview);
            }
            picker.preview = Some(preview);
            picker.preview_loading = false;
            picker.preview_error = None;
        }
    }

    fn apply_content_completion(&mut self, completion: ContentCompletion) {
        let ContentCompletion {
            generation,
            kind,
            purpose,
            result,
        } = completion;
        if let ContentPurpose::NavigationPreview {
            navigation_generation,
        } = purpose
        {
            if self.navigation_preview_requests.accept(generation) {
                self.apply_navigation_preview_completion(navigation_generation, result);
            }
            return;
        }
        if !self.content_requests.accept(generation) {
            return;
        }
        if let ContentPurpose::NavigationStage {
            navigation_generation,
        } = purpose
        {
            self.apply_navigation_stage_completion(navigation_generation, generation, result);
            return;
        }
        let mode = match kind {
            ContentKind::Diff => ContentMode::Diff,
            ContentKind::Preview => ContentMode::Preview,
        };
        let completed_diff_path = self
            .pending_diff_path
            .take()
            .filter(|(pending_generation, _)| *pending_generation == generation)
            .map(|(_, path)| path);
        let pending_preview_find = self.preview_find.take();
        self.reset_content(mode);
        match result {
            Ok(mut snapshot) => {
                self.rebind_content_snapshot_navigation(&mut snapshot);
                let cached = snapshot
                    .identity
                    .as_ref()
                    .map_or_else(HashSet::new, |identity| self.cached_folds(identity));
                self.content_provider = snapshot.provider;
                self.content_lines = snapshot.lines;
                self.content_highlights = snapshot.highlights;
                self.content_identity = snapshot.identity;
                self.content_fold_source = snapshot.fold_source;
                self.content_fold_regions = snapshot.fold_regions;
                self.content_structure = snapshot.structure;
                self.navigation_source = snapshot.navigation_source.map(Arc::new);
                self.navigation_document_version = DocumentVersion(
                    self.navigation_document_version
                        .0
                        .checked_add(1)
                        .expect("navigation document version exhausted"),
                );
                let valid_anchors: HashSet<_> = self
                    .content_fold_regions
                    .iter()
                    .map(|region| region.anchor)
                    .collect();
                self.content_collapsed_folds =
                    cached.intersection(&valid_anchors).copied().collect();
                self.content_successful = true;
                self.content_show_line_numbers =
                    mode == ContentMode::Diff || snapshot.show_line_numbers;
                self.content_diff_lines = if mode == ContentMode::Diff {
                    annotate_diff(&self.content_lines)
                } else {
                    Vec::new()
                };
                if mode == ContentMode::Diff {
                    self.current_diff_path = completed_diff_path;
                }
                let search_target = self
                    .search_preview_target
                    .take()
                    .filter(|target| target.generation == generation);
                if let Some(target) = search_target.as_ref() {
                    let line_index = target.line_number.saturating_sub(1);
                    if let Some(line) = self.content_lines.get(line_index)
                        && target.byte_range.end <= line.len()
                    {
                        if self.content_highlights.len() < self.content_lines.len() {
                            self.content_highlights
                                .resize_with(self.content_lines.len(), Vec::new);
                        }
                        self.content_highlights[line_index].push(HighlightSpan {
                            range: target.byte_range.clone(),
                            kind: HighlightKind::Search,
                        });
                        self.reveal_folded_line(line_index);
                        self.scroll_to_logical_line(line_index, target.byte_range.start);
                    }
                }
                if self
                    .last_error
                    .as_deref()
                    .is_some_and(|error| error.starts_with("content failed:"))
                {
                    self.last_error = None;
                }
                if mode == ContentMode::Preview {
                    self.navigation_caret = NavigationCaret {
                        point: first_navigation_point(&self.content_lines),
                        preferred_display_column: 0,
                    };
                    self.preview_find = pending_preview_find;
                    if search_target.is_none() {
                        if self.preview_find.is_some() {
                            self.rebuild_preview_find();
                        } else {
                            self.scroll_to_logical_line(0, 0);
                        }
                    }
                }
            }
            Err(error) => {
                self.content_lines = vec![match kind {
                    ContentKind::Diff => format!("Unable to load diff: {error}"),
                    ContentKind::Preview => format!("Unable to preview file: {error}"),
                }];
                self.last_error = Some(format!("content failed: {error}"));
            }
        }
    }

    fn reset_content(&mut self, mode: ContentMode) {
        self.preview_find = None;
        self.content_scroll = 0;
        self.content_horizontal_scroll = 0;
        self.clear_content_selection();
        self.clipboard_status = None;
        self.content_mode = mode;
        self.content_provider = None;
        self.content_highlights.clear();
        self.content_show_line_numbers = false;
        self.content_diff_lines.clear();
        self.current_diff_path = None;
        self.content_identity = None;
        self.content_fold_source = FoldSource::None;
        self.content_fold_regions.clear();
        self.content_structure = StructureSnapshot::unavailable();
        self.navigation_source = None;
        self.navigation_hover_highlight = None;
        self.navigation_target_highlight = None;
        self.content_collapsed_folds.clear();
        self.content_cursor_line = 0;
        self.content_successful = false;
    }

    fn set_info(&mut self, lines: Vec<String>) {
        self.cancel_pending_navigation();
        self.content_requests.invalidate();
        self.runtime.cancel_pending_content();
        self.pending_diff_path = None;
        self.cache_current_folds();
        self.reset_content(ContentMode::Info);
        self.content_lines = lines;
    }

    fn toggle_current_diff_review(&mut self) {
        let Some(path) = self.current_diff_path.clone() else {
            return;
        };
        let Some(version) = self
            .repo_graph
            .as_ref()
            .and_then(|graph| graph.change_details(&path))
            .map(|details| details.version.clone())
        else {
            return;
        };
        if self.reviewed_change_versions.get(&path) == Some(&version) {
            self.reviewed_change_versions.remove(&path);
        } else {
            self.reviewed_change_versions.insert(path, version);
        }
    }
}

fn first_navigation_point(lines: &[String]) -> SourcePosition {
    first_navigation_point_on_line(lines, 0)
}

fn protocol_symbol_picker_items(
    source: &NavigationSource,
    symbols: Vec<ProtocolDocumentSymbol>,
) -> Result<Vec<NavigationPickerItem>> {
    let mut depths = Vec::<usize>::with_capacity(symbols.len());
    symbols
        .into_iter()
        .enumerate()
        .map(|(index, symbol)| {
            let depth = symbol
                .parent
                .and_then(|parent| (parent < index).then_some(parent))
                .and_then(|parent| depths.get(parent).copied())
                .map_or(0, |depth| depth.saturating_add(1));
            depths.push(depth);
            let position = source.line_index.to_utf16(symbol.selection_range.start)?;
            Ok(NavigationPickerItem {
                target: NavigationTarget {
                    document: source.identity.clone(),
                    range: NavigationTargetRange::Source(symbol.selection_range),
                },
                label: format!(
                    "{}{}:{}:{} · {}",
                    "  ".repeat(depth),
                    source.identity.display_label(),
                    position.line.saturating_add(1),
                    position.character.saturating_add(1),
                    symbol.name
                ),
                detail: symbol.detail.or(symbol.container),
            })
        })
        .collect()
}

fn first_navigation_point_on_line(lines: &[String], line: usize) -> SourcePosition {
    let byte = lines
        .get(line)
        .and_then(|text| {
            text.char_indices()
                .find(|(_, character)| !character.is_whitespace())
                .map(|(byte, _)| byte)
        })
        .unwrap_or(0);
    SourcePosition { line, byte }
}

fn navigation_target_sort_key(target: &NavigationTarget) -> (PathBuf, u32, u32, u32, u32) {
    let (start_line, start_character, end_line, end_character) = match target.range {
        NavigationTargetRange::Utf16(range) => (
            range.start.line,
            range.start.character,
            range.end.line,
            range.end.character,
        ),
        NavigationTargetRange::Source(range) => (
            u32::try_from(range.start.line).unwrap_or(u32::MAX),
            u32::try_from(range.start.byte).unwrap_or(u32::MAX),
            u32::try_from(range.end.line).unwrap_or(u32::MAX),
            u32::try_from(range.end.byte).unwrap_or(u32::MAX),
        ),
    };
    (
        target.document.display_path(),
        start_line,
        start_character,
        end_line,
        end_character,
    )
}

fn content_target_for_navigation(document: &ContentIdentity) -> ContentTarget {
    match document {
        ContentIdentity::Workspace(path) => ContentTarget::Workspace(path.clone()),
        ContentIdentity::Dependency {
            root,
            relative,
            server_root,
        } => ContentTarget::Dependency {
            root: root.clone(),
            relative: relative.clone(),
            server_root: server_root.clone(),
        },
    }
}

fn navigation_picker_groups(results: &[NavigationPickerItem]) -> Vec<NavigationPickerGroup> {
    let mut groups = Vec::new();
    let mut start = 0usize;
    while start < results.len() {
        let document = results[start].target.document.clone();
        let path = document.display_path();
        let mut end = start.saturating_add(1);
        while end < results.len() && results[end].target.document == document {
            end = end.saturating_add(1);
        }
        groups.push(NavigationPickerGroup {
            path,
            results: start..end,
            expanded: true,
        });
        start = end;
    }
    groups
}

fn navigation_picker_rows(groups: &[NavigationPickerGroup]) -> Vec<NavigationPickerRow> {
    let mut rows = Vec::new();
    for (group_index, group) in groups.iter().enumerate() {
        rows.push(NavigationPickerRow::Group(group_index));
        if group.expanded {
            rows.extend(group.results.clone().map(NavigationPickerRow::Result));
        }
    }
    rows
}

fn navigation_preview_summary(preview: &NavigationPickerPreview) -> Option<String> {
    let summary = preview.lines.get(preview.target.start.line)?.trim();
    if summary.is_empty() {
        return None;
    }
    let mut bounded = summary.chars().take(120).collect::<String>();
    if summary.chars().count() > 120 {
        bounded.push('…');
    }
    Some(bounded)
}

fn navigation_target_label(target: &NavigationTarget) -> String {
    let (line, column) = match target.range {
        NavigationTargetRange::Utf16(range) => (range.start.line, range.start.character),
        NavigationTargetRange::Source(range) => (
            u32::try_from(range.start.line).unwrap_or(u32::MAX),
            u32::try_from(range.start.byte).unwrap_or(u32::MAX),
        ),
    };
    format!(
        "{}:{}:{}",
        target.document.display_label(),
        line.saturating_add(1),
        column.saturating_add(1)
    )
}

fn push_bounded_history(
    history: &mut VecDeque<NavigationHistoryEntry>,
    entry: NavigationHistoryEntry,
) {
    if history.len() == NAVIGATION_HISTORY_LIMIT {
        history.pop_front();
    }
    history.push_back(entry);
}

fn clean_navigation_message(message: &str) -> String {
    message
        .chars()
        .filter(|character| !character.is_control() && *character != '\u{1b}')
        .take(240)
        .collect()
}

fn search_result(found: SearchMatch) -> SearchResult {
    SearchResult {
        path: found.path,
        is_dir: false,
        line_number: Some(found.line_number),
        line: Some(found.line),
        match_range: Some(found.summary_range),
        source_match_range: Some(found.byte_range),
    }
}

fn file_search_score(path: &Path, query: &str, case_sensitive: bool) -> Option<usize> {
    let raw_path = path.to_string_lossy().replace('\\', "/");
    let raw_name = raw_path.rsplit('/').next().unwrap_or(&raw_path).to_owned();
    let (path, name, query) = if case_sensitive {
        (raw_path, raw_name, query.to_owned())
    } else {
        (
            raw_path.to_lowercase(),
            raw_name.to_lowercase(),
            query.to_lowercase(),
        )
    };
    if name == query {
        return Some(0);
    }
    if name.starts_with(&query) {
        return Some(10 + name.len().saturating_sub(query.len()));
    }
    if let Some(index) = path
        .split('/')
        .position(|component| component.starts_with(&query))
    {
        return Some(30 + index);
    }
    if let Some(index) = path.find(&query) {
        return Some(60 + index);
    }

    let mut query_characters = query.chars();
    let mut wanted = query_characters.next()?;
    let mut first = None;
    let mut matched = 0;
    for (index, character) in path.chars().enumerate() {
        if character != wanted {
            continue;
        }
        first.get_or_insert(index);
        matched += 1;
        let Some(next) = query_characters.next() else {
            let spread = index.saturating_sub(first.unwrap_or(0));
            return Some(100 + spread + path.chars().count().saturating_sub(matched));
        };
        wanted = next;
    }
    None
}

fn byte_index_at_display_column(value: &str, target: usize) -> usize {
    let mut width: usize = 0;
    for (byte, grapheme) in value.grapheme_indices(true) {
        let grapheme_width = UnicodeWidthStr::width(grapheme).max(1);
        if target < width.saturating_add(grapheme_width) {
            return byte;
        }
        width = width.saturating_add(grapheme_width);
    }
    value.len()
}

fn grapheme_bounds_at_column(line: &str, column: usize, tab_origin: usize) -> (usize, usize) {
    let mut display_column: usize = 0;
    for (byte, grapheme) in line.grapheme_indices(true) {
        let width = grapheme_width_at(grapheme, display_column, tab_origin);
        let end = byte + grapheme.len();
        if column < display_column.saturating_add(width) {
            return (byte, end);
        }
        display_column = display_column.saturating_add(width);
    }
    (line.len(), line.len())
}

fn wrap_line_ranges(line: &str, width: usize, tab_origin: usize) -> Vec<Range<usize>> {
    if line.is_empty() {
        return std::iter::once(0..0).collect();
    }

    let width = width.max(1);
    let mut ranges = Vec::new();
    let mut start = 0;
    let mut used_width: usize = 0;
    let mut current_tab_origin = tab_origin;
    for (byte, grapheme) in line.grapheme_indices(true) {
        let mut grapheme_width = grapheme_width_at(grapheme, used_width, current_tab_origin);
        if used_width > 0 && used_width.saturating_add(grapheme_width) > width {
            ranges.push(start..byte);
            start = byte;
            used_width = 0;
            current_tab_origin = 0;
            grapheme_width = grapheme_width_at(grapheme, used_width, current_tab_origin);
        }
        used_width = used_width.saturating_add(grapheme_width);
    }
    ranges.push(start..line.len());
    ranges
}

fn fold_summary(hidden_lines: usize, text_width: usize) -> String {
    let full = format!(" … {hidden_lines} lines");
    if UnicodeWidthStr::width(full.as_str()) <= text_width {
        full
    } else if text_width >= 2 {
        " …".to_owned()
    } else {
        "…".to_owned()
    }
}

fn visual_row_contains_byte(row: &ContentVisualRow, byte: usize, line_len: usize) -> bool {
    row.byte_range.start <= byte
        && (byte < row.byte_range.end || (byte == line_len && row.byte_range.end == line_len))
}

fn viewport_row_matches(
    row: &ContentVisualRow,
    line: usize,
    byte: usize,
    synthetic: bool,
    line_len: usize,
) -> bool {
    if row.line_index != line || row.synthetic != synthetic {
        return false;
    }
    if synthetic {
        row.byte_range.start == byte
    } else {
        visual_row_contains_byte(row, byte, line_len)
    }
}

fn repo_has_activity(snapshot: &RepoSnapshot) -> bool {
    if !snapshot.changes.is_empty() || snapshot.status_error.is_some() {
        return true;
    }
    snapshot
        .node
        .relation
        .as_ref()
        .is_some_and(|relation| match &relation.state {
            RepoRelationState::OrdinaryNested {
                untracked_in_parent,
                tracked_changes_in_parent,
            } => *untracked_in_parent || *tracked_changes_in_parent,
            RepoRelationState::Submodule { parent_change, .. } => {
                parent_change.as_ref().is_some_and(|change| {
                    change.submodule_pointer_changed() || change.submodule_worktree_dirty()
                })
            }
        })
}

fn clipboard_success_status(
    character_count: usize,
    delivery: clipboard::ClipboardDelivery,
) -> String {
    let noun = if character_count == 1 {
        "character"
    } else {
        "characters"
    };
    match delivery {
        clipboard::ClipboardDelivery::NativeConfirmed => {
            format!("Copied {character_count} {noun}")
        }
        clipboard::ClipboardDelivery::TerminalSequenceSent => {
            format!("Sent {character_count} {noun} to terminal clipboard")
        }
    }
}

fn build_git_rows(
    root: &Path,
    graph: &RepoGraph,
    existing_changes: &HashSet<RepoPath>,
) -> Vec<GitTreeRow> {
    let snapshots: HashMap<RepoId, &RepoSnapshot> = graph
        .repositories()
        .iter()
        .map(|snapshot| (snapshot.node.id.clone(), snapshot))
        .collect();
    let visible: HashSet<RepoId> = graph
        .repositories()
        .iter()
        .map(|snapshot| snapshot.node.id.clone())
        .collect();

    let mut children: HashMap<RepoId, Vec<RepoId>> = HashMap::new();
    let mut roots = Vec::new();
    for id in &visible {
        let parent = snapshots
            .get(id)
            .and_then(|snapshot| snapshot.node.relation.as_ref())
            .map(|relation| relation.parent.clone())
            .filter(|parent| visible.contains(parent));
        if let Some(parent) = parent {
            children.entry(parent).or_default().push(id.clone());
        } else {
            roots.push(id.clone());
        }
    }
    roots.sort();
    for repositories in children.values_mut() {
        repositories.sort();
    }

    let pointer_paths: HashSet<RepoPath> = graph
        .repositories()
        .iter()
        .filter_map(
            |snapshot| match snapshot.node.relation.as_ref()?.state.clone() {
                RepoRelationState::Submodule {
                    parent_change: Some(change),
                    ..
                } => Some(change.path),
                _ => None,
            },
        )
        .collect();
    let change_projection = GitChangeProjection {
        pointer_paths: &pointer_paths,
        existing_changes,
        single_repository: snapshots.len() == 1,
    };
    let mut rows = Vec::new();
    for id in roots {
        append_repository_rows(
            root,
            &id,
            &snapshots,
            &children,
            &change_projection,
            &[],
            &mut rows,
        );
    }
    for error in &graph.report().errors {
        rows.push(issue_row(root, error));
    }
    rows
}

fn repository_graph_is_truncated(graph: &RepoGraph, full_repository_discovery: bool) -> bool {
    graph.report().truncations.iter().any(|truncation| {
        full_repository_discovery || !matches!(truncation, DiscoveryTruncation::DepthLimit { .. })
    })
}

struct GitChangeProjection<'a> {
    pointer_paths: &'a HashSet<RepoPath>,
    existing_changes: &'a HashSet<RepoPath>,
    single_repository: bool,
}

fn append_repository_rows(
    root: &Path,
    id: &RepoId,
    snapshots: &HashMap<RepoId, &RepoSnapshot>,
    children: &HashMap<RepoId, Vec<RepoId>>,
    changes: &GitChangeProjection<'_>,
    repo_ancestors: &[GitRowIdentity],
    rows: &mut Vec<GitTreeRow>,
) {
    let snapshot = snapshots[id];
    let repo_identity = GitRowIdentity::Repository(id.clone());
    let pointer = snapshot
        .node
        .relation
        .as_ref()
        .and_then(|relation| match &relation.state {
            RepoRelationState::Submodule { parent_change, .. } => parent_change.clone(),
            RepoRelationState::OrdinaryNested { .. } => None,
        });
    let direct_count = snapshot
        .changes
        .iter()
        .filter(|change| !changes.pointer_paths.contains(&change.path))
        .count();
    let pointer_count = usize::from(
        pointer
            .as_ref()
            .is_some_and(RepoChange::submodule_pointer_changed),
    );
    let detail = repository_detail(snapshot);
    rows.push(GitTreeRow {
        identity: repo_identity.clone(),
        kind: GitRowKind::Repository {
            repo_id: id.clone(),
            kind: snapshot.node.kind,
            change_count: direct_count + pointer_count,
            status_error: snapshot.status_error.clone(),
        },
        depth: repo_ancestors.len(),
        label: repository_label(root, snapshot, changes.single_repository),
        detail,
        status: None,
        exists: true,
        ancestors: repo_ancestors.to_vec(),
        file_entry: None,
    });

    let mut owned_ancestors = repo_ancestors.to_vec();
    owned_ancestors.push(repo_identity.clone());
    if let Some(pointer) = pointer.filter(RepoChange::submodule_pointer_changed) {
        rows.push(GitTreeRow {
            identity: GitRowIdentity::Pointer(pointer.path.clone()),
            kind: GitRowKind::Pointer(pointer.clone()),
            depth: owned_ancestors.len(),
            label: "(submodule pointer)".to_owned(),
            detail: "parent Gitlink".to_owned(),
            status: Some(pointer.status),
            exists: snapshot.node.kind != RepoKind::SubmodulePlaceholder,
            ancestors: owned_ancestors.clone(),
            file_entry: None,
        });
    }

    append_change_rows(root, snapshot, changes, &owned_ancestors, rows);
    if let Some(child_ids) = children.get(id) {
        for child in child_ids {
            append_repository_rows(
                root,
                child,
                snapshots,
                children,
                changes,
                &owned_ancestors,
                rows,
            );
        }
    }
}

fn append_change_rows(
    root: &Path,
    snapshot: &RepoSnapshot,
    projection: &GitChangeProjection<'_>,
    repo_ancestors: &[GitRowIdentity],
    rows: &mut Vec<GitTreeRow>,
) {
    let mut directories = HashSet::new();
    let changes: Vec<(PathBuf, RepoChange)> = snapshot
        .changes
        .iter()
        .filter(|change| !projection.pointer_paths.contains(&change.path))
        .filter_map(|change| {
            display_relative_for_change(root, snapshot, change)
                .map(|display| (display, change.clone()))
        })
        .collect();
    for (display, _) in &changes {
        let mut parent = display.parent();
        while let Some(path) = parent.filter(|path| !path.as_os_str().is_empty()) {
            directories.insert(path.to_path_buf());
            parent = path.parent();
        }
    }
    let mut items: Vec<(PathBuf, Option<RepoChange>)> = directories
        .into_iter()
        .map(|path| (path, None))
        .chain(
            changes
                .into_iter()
                .map(|(path, change)| (path, Some(change))),
        )
        .collect();
    items.sort_by(|left, right| {
        tree::compare_tree_paths(&left.0, left.1.is_none(), &right.0, right.1.is_none())
    });

    for (display, change) in items {
        let mut ancestors = repo_ancestors.to_vec();
        let directory_limit = display.parent();
        let mut prefixes = Vec::new();
        if let Some(limit) = directory_limit {
            let mut current = PathBuf::new();
            for component in limit.components() {
                current.push(component.as_os_str());
                prefixes.push(current.clone());
            }
        }
        ancestors.extend(prefixes.into_iter().map(|relative| {
            GitRowIdentity::Directory(RepoPath {
                repo_id: snapshot.node.id.clone(),
                relative,
            })
        }));

        let depth = repo_ancestors.len() + display.components().count();
        if let Some(change) = change {
            let entry = FileEntry {
                relative: display.clone(),
                is_dir: false,
                depth: depth.saturating_sub(1),
                status: Some(change.status),
                contains_changes: true,
                exists: projection.existing_changes.contains(&change.path),
                symlink_target: None,
            };
            rows.push(GitTreeRow {
                identity: GitRowIdentity::Change(change.path.clone()),
                kind: GitRowKind::Change(change.clone()),
                depth,
                label: display
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| display.display().to_string()),
                detail: String::new(),
                status: Some(change.status),
                exists: entry.exists,
                ancestors,
                file_entry: Some(entry),
            });
        } else {
            let identity = GitRowIdentity::Directory(RepoPath {
                repo_id: snapshot.node.id.clone(),
                relative: display.clone(),
            });
            let entry = FileEntry {
                relative: display.clone(),
                is_dir: true,
                depth: depth.saturating_sub(1),
                status: None,
                contains_changes: true,
                exists: true,
                symlink_target: None,
            };
            rows.push(GitTreeRow {
                identity,
                kind: GitRowKind::Directory,
                depth,
                label: display
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| display.display().to_string()),
                detail: String::new(),
                status: None,
                exists: entry.exists,
                ancestors,
                file_entry: Some(entry),
            });
        }
    }
}

fn display_relative_for_change(
    root: &Path,
    snapshot: &RepoSnapshot,
    change: &RepoChange,
) -> Option<PathBuf> {
    if snapshot.node.kind != RepoKind::Containing {
        return Some(change.path.relative.clone());
    }
    root.strip_prefix(&snapshot.node.worktree)
        .ok()
        .and_then(|prefix| change.path.relative.strip_prefix(prefix).ok())
        .map(Path::to_path_buf)
}

fn repository_label(root: &Path, snapshot: &RepoSnapshot, single_repository: bool) -> String {
    if let Some(relative) = snapshot
        .node
        .workspace_relative
        .as_deref()
        .filter(|path| !path.as_os_str().is_empty())
    {
        return display_workspace_path(relative);
    }

    if !single_repository {
        return ".".to_owned();
    }

    snapshot
        .node
        .worktree
        .file_name()
        .or_else(|| root.file_name())
        .filter(|name| !name.is_empty())
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Repository".to_owned())
}

pub(crate) fn display_workspace_path(path: &Path) -> String {
    path.iter()
        .map(|component| component.to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn repository_detail(snapshot: &RepoSnapshot) -> String {
    let mut parts = vec![repo_kind_label(snapshot.node.kind).to_owned()];
    if let Some(branch) = &snapshot.branch {
        parts.push(branch.clone());
    }
    if snapshot.status_error.is_some() {
        parts.push("ERROR".to_owned());
    }
    if let Some(relation) = &snapshot.node.relation {
        match &relation.state {
            RepoRelationState::OrdinaryNested {
                untracked_in_parent,
                tracked_changes_in_parent,
            } => {
                if *untracked_in_parent {
                    parts.push("untracked in parent".to_owned());
                }
                if *tracked_changes_in_parent {
                    parts.push("changed in parent".to_owned());
                }
            }
            RepoRelationState::Submodule {
                initialized,
                parent_change,
            } => {
                if !initialized {
                    parts.push("uninitialized".to_owned());
                }
                if let Some(change) = parent_change {
                    if change.submodule_pointer_changed() {
                        parts.push("pointer changed".to_owned());
                    }
                    if change.submodule.modified_content {
                        parts.push("internal modified".to_owned());
                    }
                    if change.submodule.untracked_content {
                        parts.push("internal untracked".to_owned());
                    }
                }
            }
        }
    }
    if !repo_has_activity(snapshot) {
        parts.push("clean".to_owned());
    }
    parts.join(" · ")
}

const fn repo_kind_label(kind: RepoKind) -> &'static str {
    match kind {
        RepoKind::WorkspaceRoot => "root",
        RepoKind::Containing => "containing",
        RepoKind::Nested => "nested",
        RepoKind::LinkedWorktree => "worktree",
        RepoKind::Submodule => "submodule",
        RepoKind::SubmodulePlaceholder => "submodule placeholder",
    }
}

fn issue_row(root: &Path, error: &DiscoveryError) -> GitTreeRow {
    GitTreeRow {
        identity: GitRowIdentity::Issue(error.path.clone()),
        kind: GitRowKind::Issue(error.message.clone()),
        depth: 0,
        label: format!("[error] {}", compact_workspace_path(root, &error.path)),
        detail: error.message.clone(),
        status: None,
        exists: false,
        ancestors: Vec::new(),
        file_entry: None,
    }
}

fn compact_workspace_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .ok()
        .filter(|relative| !relative.as_os_str().is_empty())
        .map(|relative| relative.display().to_string())
        .unwrap_or_else(|| ".".to_owned())
}

fn contains(rect: Rect, column: u16, row: u16) -> bool {
    column >= rect.x
        && column < rect.x.saturating_add(rect.width)
        && row >= rect.y
        && row < rect.y.saturating_add(rect.height)
}

#[cfg(feature = "agent-observability")]
const fn lifecycle_label(value: SessionLifecycle) -> &'static str {
    match value {
        SessionLifecycle::Unknown => "Unknown",
        SessionLifecycle::Open => "Open",
        SessionLifecycle::Ended => "Ended",
        SessionLifecycle::Failed => "Failed",
    }
}

#[cfg(feature = "agent-observability")]
const fn activity_label(value: ActivityState) -> &'static str {
    match value {
        ActivityState::Unknown => "Unknown",
        ActivityState::Working => "Working",
        ActivityState::WaitingPermission => "Waiting permission",
        ActivityState::Idle => "Idle",
    }
}

#[cfg(feature = "agent-observability")]
const fn freshness_label(value: ObservationFreshness) -> &'static str {
    match value {
        ObservationFreshness::Unknown => "Unknown",
        ObservationFreshness::Current => "Current",
        ObservationFreshness::Stale => "Stale",
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, process::Command};

    use ratatui::{Terminal, backend::TestBackend};

    use super::*;
    use crate::folding::fold_regions;
    use crate::repo_graph::DiscoveryOptions;
    use crate::runtime::{ContentSnapshot, RefreshSnapshot};
    use crate::tree::ScanResult;

    #[test]
    fn grapheme_columns_keep_wide_and_combining_characters_atomic() {
        let line = "a拿铁e\u{301}";

        assert_eq!(grapheme_bounds_at_column(line, 0, 0), (0, 1));
        assert_eq!(grapheme_bounds_at_column(line, 1, 0), (1, 4));
        assert_eq!(grapheme_bounds_at_column(line, 2, 0), (1, 4));
        assert_eq!(grapheme_bounds_at_column(line, 3, 0), (4, 7));
        assert_eq!(grapheme_bounds_at_column(line, 5, 0), (7, 10));
        assert_eq!(grapheme_bounds_at_column(line, 6, 0), (10, 10));
    }

    #[test]
    fn preview_wrap_ranges_preserve_grapheme_boundaries_and_empty_lines() {
        assert_eq!(wrap_line_ranges("ab拿c", 3, 0), [0..2, 2..6]);
        assert_eq!(wrap_line_ranges("e\u{301}xy", 2, 0), [0..4, 4..5]);
        assert_eq!(
            wrap_line_ranges("", 8, 0),
            std::iter::once(0..0).collect::<Vec<_>>()
        );
    }

    fn install_fold_fixture(app: &mut App, source: &str, path: &str) {
        app.content_mode = ContentMode::Preview;
        app.content_show_line_numbers = true;
        app.content_lines = source.lines().map(ToOwned::to_owned).collect();
        app.content_fold_regions = fold_regions(Path::new(path), &app.content_lines);
        app.content_fold_source = FoldSource::BuiltinText;
        app.content_successful = true;
        app.ui_regions.content_inner = Rect::new(0, 0, 40, 12);
        app.ui_regions.content_body = app.ui_regions.content_inner;
        app.focused_pane = FocusPane::Content;
    }

    #[test]
    fn collapsed_projection_keeps_original_bytes_and_uses_bounded_summaries() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("fixture.rs"), "fixture").unwrap();
        let mut app = App::new(directory.path().to_path_buf()).unwrap();
        install_fold_fixture(&mut app, "fn 拿铁() {\n\tlet value = 1;\n}", "fixture.rs");
        let anchor = app.content_fold_regions[0].anchor;
        app.content_collapsed_folds.insert(anchor);

        let one = app.content_visual_rows(5);
        assert_eq!(one.last().and_then(|row| row.summary.as_deref()), Some("…"));
        let two = app.content_visual_rows(6);
        assert_eq!(
            two.last().and_then(|row| row.summary.as_deref()),
            Some(" …")
        );
        let full = app.content_visual_rows(20);
        assert_eq!(
            full.last().and_then(|row| row.summary.as_deref()),
            Some(" … 2 lines")
        );
        let rebuilt: String = full
            .iter()
            .filter(|row| row.line_index == 0 && !row.synthetic)
            .filter_map(|row| app.content_lines[0].get(row.byte_range.clone()))
            .collect();
        assert_eq!(rebuilt, app.content_lines[0]);
        assert!(full.iter().all(|row| row.line_index != 1));

        app.content_lines.push("tail".to_owned());
        app.content_selection = Some(ContentSelection {
            anchor_before: ContentPoint { line: 0, byte: 0 },
            anchor_after: ContentPoint { line: 0, byte: 1 },
            head: ContentPoint { line: 3, byte: 4 },
            dragging: false,
            dragged: true,
        });
        assert_eq!(
            app.selected_content_text().as_deref(),
            Some("fn 拿铁() {\n\tlet value = 1;\n}\ntail")
        );
        assert!(!app.selected_content_text().unwrap().contains("lines"));
    }

    #[test]
    fn tab_aware_fold_summary_uses_a_synthetic_row_when_expansion_would_clip() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("fixture.rs"), "fixture").unwrap();
        let mut app = App::new(directory.path().to_path_buf()).unwrap();
        install_fold_fixture(&mut app, "\tfn f() {\n\tlet value = 1;\n}", "fixture.rs");
        app.content_collapsed_folds
            .insert(app.content_fold_regions[0].anchor);

        // 4 gutter columns leave 18 text columns. Treating the leading tab as
        // zero-width would append the full summary and clip; tab expansion does not.
        let rows = app.content_visual_rows(22);
        assert!(rows.last().is_some_and(|row| row.synthetic));
        assert_eq!(
            rows.last().and_then(|row| row.summary.as_deref()),
            Some(" … 2 lines")
        );
    }

    #[test]
    fn fold_keys_preserve_nested_state_and_find_reveals_strict_ancestors() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("fixture.rs"), "fixture").unwrap();
        let mut app = App::new(directory.path().to_path_buf()).unwrap();
        install_fold_fixture(
            &mut app,
            "impl Thing {\n fn outer() {\n  fn nested() {\n   let needle = 1;\n  }\n }\n}",
            "fixture.rs",
        );
        app.collapse_all_folds();
        let collapsed = app.content_collapsed_folds.clone();
        assert!(collapsed.len() >= 3);

        app.content_cursor_line = 0;
        app.toggle_cursor_fold();
        assert!(app.content_collapsed_folds.len() < collapsed.len());
        assert!(
            app.content_collapsed_folds
                .iter()
                .any(|anchor| collapsed.contains(anchor))
        );

        app.collapse_all_folds();
        app.open_preview_find();
        for character in "needle".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
        assert_eq!(app.preview_find_position(), Some((1, 1)));
        assert!(app.content_collapsed_folds.iter().all(|anchor| {
            app.content_fold_regions
                .iter()
                .find(|region| region.anchor == *anchor)
                .is_none_or(|region| !(region.start_line < 3 && 3 <= region.end_line))
        }));
        assert_eq!(app.content_cursor_line, 3);
    }

    #[test]
    fn fold_state_survives_search_restore_and_successful_preview_reload() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("fold.rs"),
            "fn folded() {\n let cached = true;\n}\n",
        )
        .unwrap();
        let mut app = App::new(directory.path().to_path_buf()).unwrap();
        app.wait_for_background();
        app.focused_pane = FocusPane::Content;
        app.collapse_all_folds();
        let collapsed = app.content_collapsed_folds.clone();
        assert!(!collapsed.is_empty());

        app.open_search(SearchMode::Files);
        app.close_search(true);
        assert_eq!(app.content_collapsed_folds, collapsed);

        app.load_selected_preview();
        app.wait_for_background();
        assert_eq!(app.content_collapsed_folds, collapsed);
        assert!(
            app.content_visual_rows(40)
                .iter()
                .any(|row| row.fold_marker == FoldVisualMarker::Collapsed)
        );
    }

    #[test]
    fn slash_search_restores_preview_folds_and_effective_viewport() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("fixture.rs"), "fixture").unwrap();
        let mut app = App::new(directory.path().to_path_buf()).unwrap();
        install_fold_fixture(
            &mut app,
            "fn folded() {\n let hidden = true;\n}\nfn tail() {\n let shown = true;\n}",
            "fixture.rs",
        );
        app.collapse_all_folds();
        let collapsed = app.content_collapsed_folds.clone();
        let rows = app.content_visual_rows(40);
        app.content_scroll = rows.len().saturating_sub(1);
        let expected_scroll = app.effective_content_scroll(rows.len());

        app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        assert_eq!(
            app.search.as_ref().map(|search| search.mode),
            Some(SearchMode::Files)
        );
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        assert!(app.search.is_none());
        assert_eq!(app.content_mode, ContentMode::Preview);
        assert_eq!(app.content_collapsed_folds, collapsed);
        assert_eq!(app.content_scroll, expected_scroll);
    }

    #[test]
    fn effective_scroll_normalizes_huge_raw_offsets_before_navigation() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("fixture.txt"), "fixture").unwrap();
        let mut app = App::new(directory.path().to_path_buf()).unwrap();
        install_fold_fixture(&mut app, "one\ntwo\nthree", "fixture.txt");
        app.ui_regions.content_inner.width = 40;
        let rows = app.content_visual_rows(40);
        app.content_scroll = usize::MAX;

        app.scroll_content(-1, 0);

        assert_eq!(app.content_scroll, rows.len() - 2);
        assert_eq!(app.content_cursor_line, rows[rows.len() - 2].line_index);
    }

    #[test]
    fn resizing_preserves_the_top_logical_line() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("fixture.rs"), "fixture").unwrap();
        let mut app = App::new(directory.path().to_path_buf()).unwrap();
        install_fold_fixture(
            &mut app,
            "fn first_with_a_long_header_name() {\n let one = 1;\n}\nfn second_with_a_long_header_name() {\n let two = 2;\n}",
            "fixture.rs",
        );
        app.content_projection_width = 18;
        let old = app.content_visual_rows(18);
        app.content_scroll = old.iter().position(|row| row.line_index == 3).unwrap();
        app.prepare_content_width(50);
        let new = app.content_visual_rows(50);
        assert_eq!(new[app.content_scroll].line_index, 3);
    }

    #[test]
    fn search_restore_reanchors_wrapped_viewport_at_the_new_width() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("fixture.rs"), "fixture").unwrap();
        let mut app = App::new(directory.path().to_path_buf()).unwrap();
        install_fold_fixture(
            &mut app,
            "fn a_very_long_function_header_for_wrapping() {\n let hidden = true;\n}",
            "fixture.rs",
        );
        app.collapse_all_folds();
        let collapsed = app.content_collapsed_folds.clone();
        app.ui_regions.content_inner.width = 14;
        app.content_projection_width = 14;
        let old_rows = app.content_visual_rows(14);
        let old_index = old_rows
            .iter()
            .position(|row| row.line_index == 0 && !row.synthetic && row.byte_range.start > 0)
            .unwrap();
        let anchor_byte = old_rows[old_index].byte_range.start;
        app.content_scroll = old_index;

        app.open_search(SearchMode::Files);
        app.ui_regions.content_inner.width = 28;
        app.prepare_content_width(28);
        app.close_search(true);

        let new_rows = app.content_visual_rows(28);
        let restored = &new_rows[app.content_scroll];
        assert_eq!(restored.line_index, 0);
        assert!(visual_row_contains_byte(
            restored,
            anchor_byte,
            app.content_lines[0].len()
        ));
        assert_eq!(app.content_collapsed_folds, collapsed);
    }

    #[test]
    fn search_restore_maps_a_removed_synthetic_summary_to_the_last_source_row() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("fixture.rs"), "fixture").unwrap();
        let mut app = App::new(directory.path().to_path_buf()).unwrap();
        install_fold_fixture(
            &mut app,
            "fn long_fold_header() {\n let hidden = true;\n}",
            "fixture.rs",
        );
        app.collapse_all_folds();
        app.ui_regions.content_inner.width = 8;
        app.content_projection_width = 8;
        let old_rows = app.content_visual_rows(8);
        app.content_scroll = old_rows.iter().position(|row| row.synthetic).unwrap();

        app.open_search(SearchMode::Files);
        app.ui_regions.content_inner.width = 80;
        app.prepare_content_width(80);
        app.close_search(true);

        let new_rows = app.content_visual_rows(80);
        assert!(!new_rows[app.content_scroll].synthetic);
        assert_eq!(new_rows[app.content_scroll].line_index, 0);
        assert_eq!(
            app.content_scroll,
            new_rows
                .iter()
                .rposition(|row| row.line_index == 0 && !row.synthetic)
                .unwrap()
        );
    }

    #[test]
    fn find_byte_at_wrap_boundary_selects_the_following_visual_row() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("fixture.txt"), "fixture").unwrap();
        let mut app = App::new(directory.path().to_path_buf()).unwrap();
        install_fold_fixture(&mut app, "abcdef", "fixture.txt");
        app.ui_regions.content_inner.width = 7; // 4-column gutter + 3 text columns.
        app.scroll_to_logical_line(0, 3);
        let rows = app.content_visual_rows(7);
        assert_eq!(rows[app.content_scroll].byte_range, 3..6);
    }

    #[test]
    fn test_backend_renders_and_hits_rows_beyond_u16_scroll_without_aliasing() {
        const AFTER_LIMIT_LINE: usize = 66_000;
        const FINAL_LINE: usize = 70_000;

        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("fixture.txt"), "fixture").unwrap();
        let mut app = App::new(directory.path().to_path_buf()).unwrap();
        app.content_requests.invalidate();
        app.runtime.cancel_pending_content();
        app.content_mode = ContentMode::Preview;
        app.content_show_line_numbers = true;
        app.focused_pane = FocusPane::Content;
        app.content_lines = (0..=FINAL_LINE)
            .map(|index| format!("row-{index}"))
            .collect();
        app.content_lines[AFTER_LIMIT_LINE] = "UNIQUE_AFTER_U16_LIMIT".to_owned();
        app.content_lines[AFTER_LIMIT_LINE + 1] =
            "wrapped metadata stays attached to its original logical source line across continuations"
                .to_owned();
        app.content_lines[FINAL_LINE] = "UNIQUE_FINAL_SENTINEL".to_owned();
        app.content_highlights = vec![Vec::new(); app.content_lines.len()];
        // A 100-column terminal produces a 54-column content inner area with
        // the default tree width.
        app.ui_regions.content_inner.width = 54;
        app.content_projection_width = 54;
        let visual_rows = app.content_visual_rows(54);
        let after_limit_row = visual_rows
            .iter()
            .position(|row| row.line_index == AFTER_LIMIT_LINE)
            .unwrap();
        assert!(after_limit_row > usize::from(u16::MAX));
        let wrapped: Vec<_> = visual_rows
            .iter()
            .filter(|row| row.line_index == AFTER_LIMIT_LINE + 1)
            .collect();
        assert!(wrapped.len() > 1);
        assert!(wrapped[1].continuation);
        assert_eq!(wrapped[1].line_index, AFTER_LIMIT_LINE + 1);

        app.content_scroll = after_limit_row;
        let backend = TestBackend::new(100, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| crate::ui::draw(frame, &mut app))
            .unwrap();
        let area = app.ui_regions.content_inner;
        let top_row: String = (area.x..area.right())
            .map(|column| terminal.backend().buffer()[(column, area.y)].symbol())
            .collect();
        assert!(top_row.contains("66001"), "{top_row:?}");
        assert!(top_row.contains("UNIQUE_AFTER_U16_LIMIT"), "{top_row:?}");

        let text_column = area.x.saturating_add(app.content_gutter_width() as u16);
        let point = app
            .content_point_bounds(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: text_column,
                row: area.y,
                modifiers: KeyModifiers::NONE,
            })
            .unwrap();
        assert_eq!(point.0.line, AFTER_LIMIT_LINE);
        app.sync_content_cursor_to_scroll();
        assert_eq!(app.content_cursor_line, AFTER_LIMIT_LINE);
        drag_content(
            &mut app,
            text_column,
            area.y,
            text_column.saturating_add(6),
            area.y,
        );
        assert!(
            app.selected_content_text()
                .is_some_and(|selected| selected.starts_with("UNIQUE"))
        );

        app.content_scroll = usize::MAX;
        terminal
            .draw(|frame| crate::ui::draw(frame, &mut app))
            .unwrap();
        let area = app.ui_regions.content_inner;
        let final_top: String = (area.x..area.right())
            .map(|column| terminal.backend().buffer()[(column, area.y)].symbol())
            .collect();
        assert!(final_top.contains("70001"), "{final_top:?}");
        assert!(final_top.contains("UNIQUE_FINAL_SENTINEL"), "{final_top:?}");
        assert!(!final_top.contains('▸'));
        assert!(
            app.content_point_bounds(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: text_column,
                row: area.y.saturating_add(3),
                modifiers: KeyModifiers::NONE,
            })
            .is_none(),
            "blank rows below EOF must not alias the final source row"
        );

        app.content_horizontal_scroll = usize::MAX;
        assert_eq!(app.effective_content_horizontal_scroll(), 0);
        app.content_mode = ContentMode::Info;
        assert_eq!(
            app.effective_content_horizontal_scroll(),
            usize::from(u16::MAX)
        );
        app.content_lines.clear();
        app.content_highlights.clear();
        app.content_scroll = usize::MAX;
        terminal
            .draw(|frame| crate::ui::draw(frame, &mut app))
            .unwrap();
    }

    #[test]
    fn tabs_use_visible_columns_for_selection_and_wrapping() {
        assert_eq!(grapheme_bounds_at_column("\tfield", 0, 0), (0, 1));
        assert_eq!(grapheme_bounds_at_column("\tfield", 3, 0), (0, 1));
        assert_eq!(grapheme_bounds_at_column("\tfield", 4, 0), (1, 2));
        assert_eq!(wrap_line_ranges("\t1234", 6, 0), [0..3, 3..5]);
        assert_eq!(wrap_line_ranges("+\tab", 6, 1), [0..3, 3..4]);
    }

    #[test]
    fn workspace_paths_use_portable_ui_separators() {
        let path = Path::new("modules").join("child").join("nested");

        assert_eq!(display_workspace_path(&path), "modules/child/nested");
    }

    #[test]
    fn file_search_prefers_names_prefixes_and_compact_subsequences() {
        assert!(
            file_search_score(Path::new("src/app.rs"), "app", false)
                < file_search_score(Path::new("docs/app-notes.md"), "app", false)
        );
        assert!(file_search_score(Path::new("src/app_controller.rs"), "appc", false).is_some());
        assert!(file_search_score(Path::new("src/lib.rs"), "app", false).is_none());
        assert!(file_search_score(Path::new("src/App.rs"), "app", true).is_none());
    }

    #[test]
    fn search_cursor_columns_preserve_wide_and_combining_graphemes() {
        let query = "a拿e\u{301}";

        assert_eq!(byte_index_at_display_column(query, 0), 0);
        assert_eq!(byte_index_at_display_column(query, 1), 1);
        assert_eq!(byte_index_at_display_column(query, 2), 1);
        assert_eq!(byte_index_at_display_column(query, 3), 4);
        assert_eq!(byte_index_at_display_column(query, 4), query.len());
    }

    #[test]
    fn forward_and_reverse_unicode_drags_are_symmetric_in_every_content_mode() {
        for mode in [ContentMode::Preview, ContentMode::Diff, ContentMode::Info] {
            let directory = tempfile::tempdir().unwrap();
            fs::write(directory.path().join("fixture.txt"), "fixture").unwrap();
            let mut app = App::new(directory.path().to_path_buf()).unwrap();
            app.content_mode = mode;
            app.content_show_line_numbers = false;
            app.content_lines = vec!["a拿".to_owned(), "铁e\u{301}z".to_owned()];
            app.ui_regions.content_inner = Rect::new(10, 20, 40, 8);
            app.ui_regions.content_body = app.ui_regions.content_inner;
            let first_row = app.content_text_rows().y;

            drag_content(&mut app, 11, first_row, 11, first_row);
            assert_eq!(app.selected_content_text().as_deref(), Some("拿"));

            drag_content(&mut app, 11, first_row, 10, first_row);
            assert_eq!(app.selected_content_text().as_deref(), Some("a拿"));

            drag_content(&mut app, 10, first_row, 11, first_row);
            assert_eq!(app.selected_content_text().as_deref(), Some("a拿"));

            drag_content(&mut app, 11, first_row, 12, first_row + 1);
            assert_eq!(
                app.selected_content_text().as_deref(),
                Some("拿\n铁e\u{301}")
            );

            drag_content(&mut app, 12, first_row + 1, 11, first_row);
            assert_eq!(
                app.selected_content_text().as_deref(),
                Some("拿\n铁e\u{301}")
            );
        }
    }

    #[test]
    fn clipboard_status_only_claims_copy_for_confirmed_native_delivery() {
        assert_eq!(
            clipboard_success_status(1, clipboard::ClipboardDelivery::NativeConfirmed),
            "Copied 1 character"
        );
        assert_eq!(
            clipboard_success_status(2, clipboard::ClipboardDelivery::TerminalSequenceSent),
            "Sent 2 characters to terminal clipboard"
        );
    }

    #[test]
    fn limited_scan_propagates_partial_state_to_both_scopes() {
        let directory = tempfile::tempdir().unwrap();
        for name in ["a.txt", "b.txt", "c.txt"] {
            fs::write(directory.path().join(name), name).unwrap();
        }

        let mut app = App::with_preview_registry_and_scan_limit(
            directory.path().to_path_buf(),
            PreviewRegistry::with_builtins(),
            2,
        )
        .unwrap();
        app.wait_for_background();

        assert_eq!(app.all_entries.len(), 2);
        assert!(app.all_files_truncated);
        assert!(app.git_changes_truncated);
        assert!(app.scope_is_truncated());
        assert_eq!(app.scope_entry_count(), 2);

        app.set_tree_scope(TreeScope::GitChanges);
        assert!(app.scope_is_truncated());
        assert_eq!(app.scope_entry_count(), 0);
        assert_eq!(
            app.content_lines,
            ["No Git changes found in the partial filesystem results."]
        );
        assert!(!app.content_lines[0].contains("No uncommitted Git changes"));
    }

    #[test]
    fn stale_refresh_completion_cannot_replace_the_latest_snapshot() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("existing.txt"), "existing").unwrap();
        let mut app = App::new(directory.path().to_path_buf()).unwrap();
        let original_count = app.all_entries.len();

        let stale = app.refresh_requests.begin();
        let current = app.refresh_requests.begin();
        app.apply_refresh_completion(RefreshCompletion {
            generation: stale,
            result: Ok(empty_snapshot()),
        });

        assert!(app.is_refreshing());
        assert_eq!(app.all_entries.len(), original_count);
        assert_eq!(app.changed_count, 0);

        app.apply_refresh_completion(RefreshCompletion {
            generation: current,
            result: Ok(empty_snapshot()),
        });
        assert!(!app.is_refreshing());
        assert!(app.all_entries.is_empty());
        assert_eq!(app.changed_count, 0);
    }

    #[test]
    fn stale_preview_cannot_overwrite_a_newer_diff_selection() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("file.txt"), "fixture").unwrap();
        let mut app = App::new(directory.path().to_path_buf()).unwrap();

        let stale_preview = app.content_requests.begin();
        let current_diff = app.content_requests.begin();
        app.reset_content(ContentMode::Diff);
        app.content_lines = vec!["Loading current diff…".to_owned()];
        app.apply_content_completion(ContentCompletion {
            generation: stale_preview,
            kind: ContentKind::Preview,
            purpose: ContentPurpose::Display,
            result: Ok(content_snapshot("obsolete preview")),
        });

        assert!(app.is_content_loading());
        assert_eq!(app.content_mode, ContentMode::Diff);
        assert_eq!(app.content_lines, ["Loading current diff…"]);

        app.apply_content_completion(ContentCompletion {
            generation: current_diff,
            kind: ContentKind::Diff,
            purpose: ContentPurpose::Display,
            result: Ok(content_snapshot("current diff")),
        });
        assert!(!app.is_content_loading());
        assert_eq!(app.content_mode, ContentMode::Diff);
        assert_eq!(app.content_lines, ["current diff"]);
    }

    #[test]
    fn current_worker_errors_leave_snapshots_intact_and_end_loading() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("file.txt"), "fixture").unwrap();
        let mut app = App::new(directory.path().to_path_buf()).unwrap();
        let original_entries = app.all_entries.clone();

        let refresh = app.refresh_requests.begin();
        app.apply_refresh_completion(RefreshCompletion {
            generation: refresh,
            result: Err("fixture refresh error".to_owned()),
        });
        assert!(!app.is_refreshing());
        assert_eq!(app.all_entries, original_entries);
        assert_eq!(
            app.last_error.as_deref(),
            Some("refresh failed: fixture refresh error")
        );

        let content = app.content_requests.begin();
        app.apply_content_completion(ContentCompletion {
            generation: content,
            kind: ContentKind::Preview,
            purpose: ContentPurpose::Display,
            result: Ok(content_snapshot("recovered content")),
        });
        assert_eq!(
            app.last_error.as_deref(),
            Some("refresh failed: fixture refresh error")
        );

        let content = app.content_requests.begin();
        app.apply_content_completion(ContentCompletion {
            generation: content,
            kind: ContentKind::Preview,
            purpose: ContentPurpose::Display,
            result: Err("fixture content error".to_owned()),
        });
        assert!(!app.is_content_loading());
        assert!(app.content_lines[0].contains("Unable to preview file"));
        assert_eq!(
            app.last_error.as_deref(),
            Some("content failed: fixture content error")
        );
    }

    #[test]
    fn startup_depth_boundary_is_not_reported_as_partial() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir_all(directory.path().join("nested/deeper")).unwrap();
        let graph = RepoGraph::discover_with_options(
            directory.path(),
            DiscoveryOptions {
                max_entries: 8,
                max_repositories: 8,
                max_depth: 1,
            },
        )
        .unwrap();
        assert!(graph.report().is_truncated());

        let mut app = App::new(directory.path().to_path_buf()).unwrap();
        app.apply_refresh_snapshot(RefreshSnapshot {
            branch: None,
            projected_change_count: 0,
            scan: ScanResult {
                entries: Vec::new(),
                truncated: false,
                unloaded_directories: HashSet::new(),
            },
            graph: Some(graph),
            existing_changes: HashSet::new(),
            full_repository_discovery: false,
        });
        app.apply_tree_scope(TreeScope::GitChanges);

        assert!(!app.repository_graph_truncated);
        assert!(!app.git_changes_truncated);
        assert!(
            app.visible_git_rows()
                .iter()
                .all(|row| !row.label.starts_with("[partial]"))
        );
    }

    #[test]
    fn full_repository_truncation_sets_status_without_a_selectable_row() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir_all(directory.path().join("nested/deeper")).unwrap();
        let graph = RepoGraph::discover_with_options(
            directory.path(),
            DiscoveryOptions {
                max_entries: 8,
                max_repositories: 8,
                max_depth: 1,
            },
        )
        .unwrap();
        assert!(graph.report().is_truncated());

        let mut app = App::new(directory.path().to_path_buf()).unwrap();
        app.apply_refresh_snapshot(RefreshSnapshot {
            branch: None,
            projected_change_count: 0,
            scan: ScanResult {
                entries: Vec::new(),
                truncated: false,
                unloaded_directories: HashSet::new(),
            },
            graph: Some(graph),
            existing_changes: HashSet::new(),
            full_repository_discovery: true,
        });
        app.apply_tree_scope(TreeScope::GitChanges);

        assert!(app.repository_graph_truncated);
        assert!(app.git_changes_truncated);
        assert!(
            app.visible_git_rows()
                .iter()
                .all(|row| !row.label.starts_with("[partial]"))
        );
    }

    #[test]
    fn startup_hard_limit_still_sets_partial_status() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("nested")).unwrap();
        let graph = RepoGraph::discover_with_options(
            directory.path(),
            DiscoveryOptions {
                max_entries: 0,
                max_repositories: 8,
                max_depth: 1,
            },
        )
        .unwrap();
        assert!(graph.report().is_truncated());

        let mut app = App::new(directory.path().to_path_buf()).unwrap();
        app.apply_refresh_snapshot(RefreshSnapshot {
            branch: None,
            projected_change_count: 0,
            scan: ScanResult {
                entries: Vec::new(),
                truncated: false,
                unloaded_directories: HashSet::new(),
            },
            graph: Some(graph),
            existing_changes: HashSet::new(),
            full_repository_discovery: false,
        });
        app.apply_tree_scope(TreeScope::GitChanges);

        assert!(app.repository_graph_truncated);
        assert!(app.git_changes_truncated);
        assert!(
            app.visible_git_rows()
                .iter()
                .all(|row| !row.label.starts_with("[partial]"))
        );
    }

    fn empty_snapshot() -> RefreshSnapshot {
        RefreshSnapshot {
            branch: None,
            projected_change_count: 0,
            graph: None,
            existing_changes: HashSet::new(),
            scan: ScanResult {
                entries: Vec::new(),
                truncated: false,
                unloaded_directories: HashSet::new(),
            },
            full_repository_discovery: true,
        }
    }

    fn content_snapshot(line: &str) -> ContentSnapshot {
        ContentSnapshot {
            provider: None,
            lines: vec![line.to_owned()],
            highlights: Vec::new(),
            show_line_numbers: false,
            identity: None,
            fold_source: FoldSource::None,
            fold_regions: Vec::new(),
            structure: crate::folding::StructureSnapshot::unavailable(),
            navigation_source: None,
        }
    }

    fn navigation_app(source: &str) -> App {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.keep();
        fs::write(root.join("caller.rs"), source).unwrap();
        fs::write(root.join("target.rs"), "fn target() {}\n").unwrap();
        let mut app = App::new(root).unwrap();
        app.wait_for_background();
        app.request_content(
            ContentKind::Preview,
            "caller.rs".to_owned(),
            ContentTarget::Workspace(PathBuf::from("caller.rs")),
        );
        app.wait_for_background();
        app.focused_pane = FocusPane::Content;
        app.ui_regions.content_inner = Rect::new(0, 0, 80, 20);
        app.ui_regions.content_body = app.ui_regions.content_inner;
        app.navigation_caret = NavigationCaret {
            point: SourcePosition { line: 0, byte: 3 },
            preferred_display_column: 3,
        };
        app
    }

    fn init_git_repository(path: &Path) {
        fs::create_dir_all(path).unwrap();
        let output = Command::new("git")
            .args(["-c", "init.defaultBranch=main", "init", "--quiet"])
            .current_dir(path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git init failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn mark_local_symbols_incomplete(app: &mut App) {
        let mut structure = app.content_structure.clone();
        structure.symbols.clear();
        structure.symbols_complete = false;
        app.content_structure = structure.clone();
        let mut source = app
            .navigation_source
            .as_ref()
            .map(|source| source.as_ref().clone())
            .unwrap();
        source.structure = Arc::new(structure);
        app.navigation_source = Some(Arc::new(source));
    }

    fn protocol_symbol(
        name: &str,
        selection_range: SourceRange,
        parent: Option<usize>,
    ) -> ProtocolDocumentSymbol {
        ProtocolDocumentSymbol::for_app_test(
            name,
            selection_range,
            selection_range,
            parent,
            None,
            None,
        )
    }

    fn set_search_result(app: &mut App, path: &str) {
        let search = app.search.as_mut().unwrap();
        search.results = vec![SearchResult {
            path: PathBuf::from(path),
            is_dir: false,
            line_number: None,
            line: None,
            match_range: None,
            source_match_range: None,
        }];
        app.search_list_state.select(Some(0));
    }

    #[test]
    fn search_cancel_restores_navigation_identity_and_rejects_late_preview_state() {
        let mut app = navigation_app("fn caller() {}\n");
        let caller_source = app.navigation_source.clone().unwrap();
        let caller_version = app.navigation_document_version;
        let caller_caret = NavigationCaret {
            point: SourcePosition { line: 0, byte: 4 },
            preferred_display_column: 4,
        };
        let caller_highlight = SourceRange {
            start: SourcePosition { line: 0, byte: 3 },
            end: SourcePosition { line: 0, byte: 9 },
        };
        app.navigation_caret = caller_caret;
        app.navigation_target_highlight = Some(caller_highlight);
        app.set_navigation_status(NavigationStatusLevel::Info, "caller navigation state");
        let history_entry = app.current_navigation_entry().unwrap();
        app.navigation_back.push_back(history_entry);
        let original_tree_scope = app.tree_scope;
        let original_tree_selection = app.tree_state.selected();

        app.open_search(SearchMode::Files);
        set_search_result(&mut app, "target.rs");
        app.preview_search_selection();
        app.wait_for_background();
        let target_source = app.navigation_source.clone().unwrap();
        let target_version = app.navigation_document_version;
        assert_ne!(target_source.identity, caller_source.identity);

        app.close_search(true);
        assert_eq!(
            app.navigation_source
                .as_ref()
                .map(|source| &source.identity),
            Some(&caller_source.identity)
        );
        assert_eq!(app.navigation_document_version, caller_version);
        assert_eq!(app.navigation_caret, caller_caret);
        assert_eq!(app.navigation_target_highlight, Some(caller_highlight));
        let restored_source = app.navigation_source.as_ref().unwrap();
        assert_eq!(restored_source.absolute_path, caller_source.absolute_path);
        assert_eq!(restored_source.server_root, caller_source.server_root);
        assert_eq!(restored_source.text, caller_source.text);
        assert_eq!(app.navigation_back.len(), 1);
        assert_eq!(app.tree_scope, original_tree_scope);
        assert_eq!(app.tree_state.selected(), original_tree_selection);

        app.request_semantic_navigation(NavigationOperation::Definition);
        let current = app.navigation_invocation.clone().unwrap();
        assert_eq!(current.source_identity, caller_source.identity);
        assert_eq!(current.source_version, caller_version);
        assert!(matches!(
            current.origin.target.range,
            NavigationTargetRange::Source(range) if range.contains(caller_caret.point)
        ));
        app.apply_navigation_completion(NavigationRuntimeCompletion {
            generation: current.generation,
            operation: NavigationOperation::Definition,
            source_identity: target_source.identity.clone(),
            source_version: target_version,
            result: NavigationProtocolResult::Locations(Vec::new()),
        });
        assert_eq!(
            app.navigation_invocation
                .as_ref()
                .map(|invocation| (&invocation.source_identity, invocation.source_version)),
            Some((&caller_source.identity, caller_version))
        );
        app.cancel_pending_navigation();
    }

    #[test]
    fn search_accept_adopts_target_navigation_identity_while_empty_and_error_cancel_restore() {
        let mut app = navigation_app("fn caller() {}\n");
        let caller = app.navigation_source.clone().unwrap();

        app.open_search(SearchMode::Files);
        set_search_result(&mut app, "target.rs");
        app.preview_search_selection();
        app.wait_for_background();
        app.accept_search_selection();
        app.wait_for_background();
        assert!(app.search.is_none());
        assert_eq!(
            app.navigation_source
                .as_ref()
                .map(|source| source.identity.path()),
            Some(Path::new("target.rs"))
        );

        app.request_content(
            ContentKind::Preview,
            "caller.rs".to_owned(),
            ContentTarget::Workspace(PathBuf::from("caller.rs")),
        );
        app.wait_for_background();
        let restored_caller = app.navigation_source.clone().unwrap();
        assert_eq!(restored_caller.identity, caller.identity);

        for (lines, error) in [
            (vec!["No matches".to_owned()], None),
            (
                vec!["Search failed".to_owned()],
                Some("search fixture error".to_owned()),
            ),
        ] {
            app.open_search(SearchMode::Text);
            app.set_info(lines);
            app.last_error = error;
            app.close_search(true);
            assert_eq!(
                app.navigation_source
                    .as_ref()
                    .map(|source| &source.identity),
                Some(&restored_caller.identity)
            );
            assert_eq!(
                app.content_identity.as_ref(),
                Some(&restored_caller.identity)
            );
        }
    }

    #[test]
    fn complete_local_document_symbols_open_immediately_without_runtime_pending() {
        let mut app = navigation_app("mod outer {\n    fn inner() {}\n}\n");
        assert!(app.content_structure.symbols_complete);

        app.open_document_symbols();

        assert!(app.navigation_invocation.is_none());
        let picker = app.navigation_picker.as_ref().unwrap();
        assert_eq!(picker.title, "Document Symbols");
        assert!(
            picker
                .results
                .iter()
                .any(|item| item.label.contains("outer"))
        );
        assert!(
            picker
                .results
                .iter()
                .any(|item| item.label.contains("inner"))
        );
    }

    #[test]
    fn incomplete_document_symbols_without_lsp_report_unavailable_and_stay_put() {
        let mut app = navigation_app("fn caller() {}\n");
        mark_local_symbols_incomplete(&mut app);
        let before = app.content_identity.clone();

        app.open_document_symbols();
        assert!(app.navigation_invocation.is_some());
        for _ in 0..100 {
            app.poll_background();
            if app.navigation_invocation.is_none() {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }

        assert_eq!(app.content_identity, before);
        assert!(app.navigation_picker.is_none());
        assert!(
            app.navigation_status
                .as_ref()
                .is_some_and(|status| status.message.contains("unavailable for Rust"))
        );
    }

    #[test]
    fn lsp_document_symbols_preserve_nested_and_flat_labels_utf16_and_atomic_selection() {
        let mut app = navigation_app("// 😀 target\nfn caller() {}\n");
        mark_local_symbols_incomplete(&mut app);
        app.open_document_symbols();
        let invocation = app.navigation_invocation.clone().unwrap();
        let emoji_name = SourceRange {
            start: SourcePosition { line: 0, byte: 8 },
            end: SourcePosition { line: 0, byte: 14 },
        };
        let caller_name = SourceRange {
            start: SourcePosition { line: 1, byte: 3 },
            end: SourcePosition { line: 1, byte: 9 },
        };
        app.apply_document_symbol_completion(NavigationDocumentSymbolCompletion {
            generation: invocation.generation,
            source_identity: invocation.source_identity.clone(),
            source_version: invocation.source_version,
            symbols: vec![
                protocol_symbol("target", emoji_name, None),
                protocol_symbol("caller", caller_name, Some(0)),
            ],
        });

        let picker = app.navigation_picker.as_ref().unwrap();
        assert_eq!(picker.results.len(), 2);
        assert!(picker.results[0].label.contains("caller.rs:1:7 · target"));
        assert!(
            picker.results[1]
                .label
                .starts_with("  caller.rs:2:4 · caller")
        );
        app.move_navigation_picker(1);
        app.accept_navigation_picker_selection();
        assert!(app.navigation_picker.is_none());
        assert_eq!(app.navigation_caret.point, caller_name.start);
        assert_eq!(app.navigation_target_highlight, Some(caller_name));
        assert_eq!(app.navigation_back.len(), 1);

        mark_local_symbols_incomplete(&mut app);
        app.open_document_symbols();
        let invocation = app.navigation_invocation.clone().unwrap();
        app.apply_document_symbol_completion(NavigationDocumentSymbolCompletion {
            generation: invocation.generation,
            source_identity: invocation.source_identity,
            source_version: invocation.source_version,
            symbols: vec![
                protocol_symbol("flat-a", emoji_name, None),
                protocol_symbol("flat-b", caller_name, None),
            ],
        });
        let picker = app.navigation_picker.as_ref().unwrap();
        assert!(!picker.results[0].label.starts_with(' '));
        assert!(!picker.results[1].label.starts_with(' '));
    }

    #[test]
    fn late_document_symbol_completion_is_rejected_without_clearing_current_pending() {
        let mut app = navigation_app("fn caller() {}\n");
        mark_local_symbols_incomplete(&mut app);
        app.open_document_symbols();
        let invocation = app.navigation_invocation.clone().unwrap();
        let name = SourceRange {
            start: SourcePosition { line: 0, byte: 3 },
            end: SourcePosition { line: 0, byte: 9 },
        };

        app.apply_document_symbol_completion(NavigationDocumentSymbolCompletion {
            generation: invocation.generation.saturating_sub(1),
            source_identity: invocation.source_identity.clone(),
            source_version: invocation.source_version,
            symbols: vec![protocol_symbol("late", name, None)],
        });

        assert!(app.navigation_picker.is_none());
        assert_eq!(
            app.navigation_invocation
                .as_ref()
                .map(|current| current.generation),
            Some(invocation.generation)
        );
        app.cancel_pending_navigation();
    }

    #[test]
    fn lsp_document_symbol_zero_sets_status_and_single_still_uses_picker() {
        let mut app = navigation_app("fn caller() {}\n");
        mark_local_symbols_incomplete(&mut app);
        app.open_document_symbols();
        let first = app.navigation_invocation.clone().unwrap();
        app.apply_document_symbol_completion(NavigationDocumentSymbolCompletion {
            generation: first.generation,
            source_identity: first.source_identity,
            source_version: first.source_version,
            symbols: Vec::new(),
        });
        assert!(app.navigation_picker.is_none());
        assert!(
            app.navigation_status
                .as_ref()
                .is_some_and(|status| status.message == "No document symbols found.")
        );

        app.open_document_symbols();
        let second = app.navigation_invocation.clone().unwrap();
        let name = SourceRange {
            start: SourcePosition { line: 0, byte: 3 },
            end: SourcePosition { line: 0, byte: 9 },
        };
        app.apply_document_symbol_completion(NavigationDocumentSymbolCompletion {
            generation: second.generation,
            source_identity: second.source_identity,
            source_version: second.source_version,
            symbols: vec![protocol_symbol("caller", name, None)],
        });
        assert_eq!(
            app.navigation_picker
                .as_ref()
                .map(|picker| picker.results.len()),
            Some(1)
        );
    }

    #[test]
    fn terminal_navigation_failures_clear_pending_and_surface_status() {
        for message in [
            "language server initialize failed",
            "File revalidation worker became wedged.",
            "language server selected unsupported position encoding utf-8",
        ] {
            let mut app = navigation_app("fn caller() {}\n");
            app.request_semantic_navigation(NavigationOperation::Definition);
            let invocation = app.navigation_invocation.clone().unwrap();
            let before = app.content_identity.clone();
            app.apply_navigation_completion(NavigationRuntimeCompletion {
                generation: invocation.generation,
                operation: invocation.operation,
                source_identity: invocation.source_identity,
                source_version: invocation.source_version,
                result: NavigationProtocolResult::Failed(message.to_owned()),
            });
            assert!(app.navigation_invocation.is_none());
            assert_eq!(app.content_identity, before);
            assert!(
                app.navigation_status
                    .as_ref()
                    .is_some_and(|status| status.message.contains(message))
            );
        }
    }

    #[test]
    fn refresh_cancels_pending_definition_and_document_symbols_and_rejects_late_results() {
        let mut app = navigation_app("fn caller() {}\n");
        let identity = app.content_identity.clone();

        app.request_semantic_navigation(NavigationOperation::Definition);
        let definition = app.navigation_invocation.clone().unwrap();
        app.request_refresh(false);
        assert!(app.is_refreshing());
        assert!(app.navigation_invocation.is_none());
        assert!(app.pending_navigation_stage.is_none());
        assert!(app.navigation_picker.is_none());
        assert!(!app.is_content_loading());
        assert!(app.navigation_status.is_none());

        app.apply_navigation_completion(NavigationRuntimeCompletion {
            generation: definition.generation,
            operation: definition.operation,
            source_identity: definition.source_identity,
            source_version: definition.source_version,
            result: NavigationProtocolResult::Locations(Vec::new()),
        });
        assert!(app.navigation_invocation.is_none());
        assert!(app.navigation_status.is_none());
        assert_eq!(app.content_identity, identity);

        app.wait_for_background();
        assert!(!app.is_refreshing());

        mark_local_symbols_incomplete(&mut app);
        app.open_document_symbols();
        let symbols = app.navigation_invocation.clone().unwrap();
        app.request_refresh(false);
        assert!(app.navigation_invocation.is_none());
        app.apply_document_symbol_completion(NavigationDocumentSymbolCompletion {
            generation: symbols.generation,
            source_identity: symbols.source_identity,
            source_version: symbols.source_version,
            symbols: vec![protocol_symbol(
                "late",
                SourceRange {
                    start: SourcePosition { line: 0, byte: 3 },
                    end: SourcePosition { line: 0, byte: 9 },
                },
                None,
            )],
        });
        assert!(app.navigation_picker.is_none());
        assert!(app.navigation_status.is_none());
        app.wait_for_background();
        assert!(!app.is_refreshing());
        assert!(!app.is_content_loading());
    }

    #[test]
    fn refresh_failure_keeps_the_installed_graph_root_but_cancels_navigation_loading() {
        let mut app = navigation_app("fn caller() {}\n");
        let source = app.navigation_source.clone().unwrap();
        let graph = app.repo_graph.clone();
        app.request_semantic_navigation(NavigationOperation::Definition);
        let pending = app.navigation_invocation.clone().unwrap();

        let refresh = app.request_refresh(false);
        app.apply_refresh_completion(RefreshCompletion {
            generation: refresh,
            result: Err("fixture refresh failure".to_owned()),
        });

        assert!(!app.is_refreshing());
        assert!(!app.is_content_loading());
        assert!(app.navigation_invocation.is_none());
        assert!(app.pending_navigation_stage.is_none());
        assert_eq!(
            app.navigation_source
                .as_ref()
                .map(|source| &source.server_root),
            Some(&source.server_root)
        );
        assert_eq!(
            app.repo_graph.as_ref().map(RepoGraph::workspace_root),
            graph.as_ref().map(RepoGraph::workspace_root)
        );
        assert_eq!(
            app.last_refresh_error.as_deref(),
            Some("refresh failed: fixture refresh failure")
        );

        app.apply_navigation_completion(NavigationRuntimeCompletion {
            generation: pending.generation,
            operation: pending.operation,
            source_identity: pending.source_identity,
            source_version: pending.source_version,
            result: NavigationProtocolResult::Locations(Vec::new()),
        });
        assert!(app.navigation_status.is_none());
    }

    #[test]
    fn refresh_closes_picker_and_cancels_navigation_stage_content_loading() {
        let mut picker_app = navigation_picker_app(4);
        assert!(picker_app.navigation_picker.is_some());
        let refresh = picker_app.request_refresh(false);
        assert!(picker_app.navigation_picker.is_none());
        picker_app.apply_refresh_completion(RefreshCompletion {
            generation: refresh,
            result: Ok(empty_snapshot()),
        });

        let mut stage_app = navigation_app("fn caller() {}\n");
        let origin = stage_app.current_navigation_entry().unwrap();
        let generation = stage_app.next_navigation_generation();
        let invocation = NavigationInvocation {
            generation,
            operation: NavigationOperation::Definition,
            source_identity: origin.target.document.clone(),
            source_version: stage_app.navigation_document_version,
            origin,
            history_intent: NavigationHistoryIntent::Jump,
            destination_viewport: None,
            return_focus: FocusPane::Content,
        };
        let target =
            ContentIdentity::from_absolute(&stage_app.root, &stage_app.root.join("target.rs"))
                .unwrap();
        stage_app.accept_navigation_target(
            invocation,
            NavigationTarget {
                document: target,
                range: NavigationTargetRange::Utf16(lsp_types::Range::new(
                    lsp_types::Position::new(0, 3),
                    lsp_types::Position::new(0, 9),
                )),
            },
        );
        assert!(stage_app.pending_navigation_stage.is_some());
        assert!(stage_app.is_content_loading());

        let refresh = stage_app.request_refresh(false);
        assert!(stage_app.pending_navigation_stage.is_none());
        assert!(!stage_app.is_content_loading());
        stage_app.apply_refresh_completion(RefreshCompletion {
            generation: refresh,
            result: Ok(empty_snapshot()),
        });
        assert!(!stage_app.is_refreshing());
        assert!(!stage_app.is_content_loading());
    }

    #[test]
    fn successful_refresh_rebinds_nested_server_root_without_reloading_visible_content() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let old_root = root.join("a");
        let new_root = old_root.join("b");
        init_git_repository(&old_root);
        let relative = PathBuf::from("a/b/caller.rs");
        let source_text = "fn caller() {\n    caller();\n}\nfn helper() {}\n";
        fs::create_dir_all(&new_root).unwrap();
        fs::write(root.join(&relative), source_text).unwrap();

        let mut app = App::new(root.clone()).unwrap();
        app.wait_for_background();
        app.request_content(
            ContentKind::Preview,
            relative.display().to_string(),
            ContentTarget::Workspace(relative.clone()),
        );
        app.wait_for_background();
        app.focused_pane = FocusPane::Content;
        app.ui_regions.content_inner = Rect::new(0, 0, 30, 3);
        app.ui_regions.content_body = app.ui_regions.content_inner;
        app.collapse_all_folds();
        app.content_scroll = 1;
        app.navigation_caret = NavigationCaret {
            point: SourcePosition { line: 1, byte: 4 },
            preferred_display_column: 4,
        };
        let highlight = SourceRange {
            start: SourcePosition { line: 1, byte: 4 },
            end: SourcePosition { line: 1, byte: 10 },
        };
        app.navigation_target_highlight = Some(highlight);
        app.navigation_back
            .push_back(app.current_navigation_entry().unwrap());

        let old_source = app.navigation_source.clone().unwrap();
        assert_eq!(old_source.server_root, old_root.canonicalize().unwrap());
        let lines = app.content_lines.clone();
        let folds = app.content_collapsed_folds.clone();
        let viewport = app.content_scroll;
        let version = app.navigation_document_version;
        let caret = app.navigation_caret;
        let history = app.navigation_back.clone();
        app.open_search(SearchMode::Files);

        fs::remove_dir_all(old_root.join(".git")).unwrap();
        init_git_repository(&new_root);
        let canonical_new_root = new_root.canonicalize().unwrap();
        app.request_refresh(true);
        app.wait_for_background();

        let rebound = app.navigation_source.as_ref().unwrap();
        assert_eq!(rebound.server_root, canonical_new_root);
        assert_eq!(rebound.identity, old_source.identity);
        assert_eq!(rebound.absolute_path, old_source.absolute_path);
        assert_eq!(rebound.disk_raw_len, old_source.disk_raw_len);
        assert_eq!(rebound.language, old_source.language);
        assert_eq!(rebound.text, old_source.text);
        assert!(Arc::ptr_eq(&rebound.text, &old_source.text));
        assert!(Arc::ptr_eq(&rebound.line_index, &old_source.line_index));
        assert!(Arc::ptr_eq(&rebound.structure, &old_source.structure));
        assert_eq!(app.content_lines, lines);
        assert_eq!(app.content_collapsed_folds, folds);
        assert_eq!(app.content_scroll, viewport);
        assert_eq!(app.navigation_document_version, version);
        assert_eq!(app.navigation_caret, caret);
        assert_eq!(app.navigation_target_highlight, Some(highlight));
        assert_eq!(app.navigation_back.len(), history.len());
        let actual_history = app.navigation_back.back().unwrap();
        let expected_history = history.back().unwrap();
        assert_eq!(actual_history.target, expected_history.target);
        assert_eq!(actual_history.viewport.line, expected_history.viewport.line);
        assert_eq!(
            actual_history.viewport.byte_start,
            expected_history.viewport.byte_start
        );
        assert_eq!(
            actual_history.viewport.effective_scroll,
            expected_history.viewport.effective_scroll
        );
        assert!(!app.is_refreshing());
        assert!(!app.is_content_loading());

        app.close_search(true);
        assert_eq!(
            app.navigation_source
                .as_ref()
                .map(|source| source.server_root.as_path()),
            Some(canonical_new_root.as_path())
        );
        assert_eq!(app.content_lines, lines);
        assert_eq!(app.content_collapsed_folds, folds);
        assert_eq!(app.content_scroll, viewport);
        assert_eq!(app.navigation_back.len(), history.len());
        assert_eq!(
            app.navigation_back.back().unwrap().target,
            history.back().unwrap().target
        );

        app.request_semantic_navigation(NavigationOperation::Definition);
        assert!(app.navigation_invocation.is_some());
        assert_eq!(
            app.navigation_source
                .as_ref()
                .map(|source| source.server_root.as_path()),
            Some(canonical_new_root.as_path())
        );
        app.cancel_pending_navigation();

        let generation = app.content_requests.begin();
        app.apply_content_completion(ContentCompletion {
            generation,
            kind: ContentKind::Preview,
            purpose: ContentPurpose::Display,
            result: Ok(ContentSnapshot {
                provider: Some("builtin:text".to_owned()),
                lines: lines.clone(),
                highlights: Vec::new(),
                show_line_numbers: true,
                identity: Some(old_source.identity.clone()),
                fold_source: FoldSource::BuiltinText,
                fold_regions: old_source.structure.folds.clone(),
                structure: old_source.structure.as_ref().clone(),
                navigation_source: Some(old_source.as_ref().clone()),
            }),
        });
        assert_eq!(
            app.navigation_source
                .as_ref()
                .map(|source| source.server_root.as_path()),
            Some(canonical_new_root.as_path())
        );
    }

    #[test]
    fn refresh_degrades_a_source_whose_workspace_identity_is_no_longer_valid() {
        let mut app = navigation_app("fn caller() {}\n");
        let lines = app.content_lines.clone();
        let mut invalid = app.navigation_source.as_ref().unwrap().as_ref().clone();
        invalid.absolute_path = app.root.join("../outside.rs");
        app.navigation_source = Some(Arc::new(invalid));

        app.rebind_navigation_sources_after_refresh();

        assert!(app.navigation_source.is_none());
        assert_eq!(app.content_lines, lines);
        assert!(app.content_identity.is_some());
    }

    fn navigation_picker_app(count: usize) -> App {
        let source = (0..count)
            .map(|index| format!("fn item_{index}() {{}}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut app = navigation_app(&source);
        let origin = app.current_navigation_entry().unwrap();
        let generation = app.next_navigation_generation();
        let identity = app.navigation_source.as_ref().unwrap().identity.clone();
        let invocation = NavigationInvocation {
            generation,
            operation: NavigationOperation::DocumentSymbols,
            source_identity: identity.clone(),
            source_version: app.navigation_document_version,
            origin,
            history_intent: NavigationHistoryIntent::Jump,
            destination_viewport: None,
            return_focus: FocusPane::Content,
        };
        let results = (0..count)
            .map(|index| {
                let range = SourceRange {
                    start: SourcePosition {
                        line: index,
                        byte: 3,
                    },
                    end: SourcePosition {
                        line: index,
                        byte: 9,
                    },
                };
                NavigationPickerItem {
                    target: NavigationTarget {
                        document: identity.clone(),
                        range: NavigationTargetRange::Source(range),
                    },
                    label: format!("caller.rs:{}:4 · item_{index}", index + 1),
                    detail: None,
                }
            })
            .collect();
        app.open_navigation_picker("Document Symbols", invocation, results);
        app
    }

    #[test]
    fn navigation_picker_uses_preview_and_grouped_results_only_when_wide_enough() {
        let mut app = navigation_picker_app(3);
        let backend = TestBackend::new(140, 28);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| crate::ui::draw(frame, &mut app))
            .unwrap();

        assert!(app.ui_regions.navigation_preview.width >= 48);
        assert!(app.ui_regions.navigation_results.width >= 32);
        let wide = terminal.backend().buffer().content();
        let wide = wide.iter().map(|cell| cell.symbol()).collect::<String>();
        assert!(wide.contains("fn item_0"));
        assert!(wide.contains("caller.rs"));

        let backend = TestBackend::new(70, 18);
        let mut narrow_terminal = Terminal::new(backend).unwrap();
        narrow_terminal
            .draw(|frame| crate::ui::draw(frame, &mut app))
            .unwrap();
        assert_eq!(app.ui_regions.navigation_preview, Rect::default());
        assert!(app.ui_regions.navigation_results.width > 0);
    }

    #[test]
    fn scrolled_navigation_picker_mouse_click_commits_the_visible_location() {
        let mut app = navigation_picker_app(40);
        app.navigation_picker
            .as_mut()
            .unwrap()
            .list_state
            .select(Some(30));
        let backend = TestBackend::new(50, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| crate::ui::draw(frame, &mut app))
            .unwrap();
        let offset = app.navigation_picker.as_ref().unwrap().list_state.offset();
        assert!(offset > 0);
        let visible_row =
            1usize.min(usize::from(app.ui_regions.navigation_results.height).saturating_sub(1));
        let expected = offset + visible_row;
        let expected_result = match app
            .navigation_picker
            .as_ref()
            .and_then(|picker| picker.visible_rows.get(expected))
        {
            Some(NavigationPickerRow::Result(index)) => *index,
            row => panic!("expected a result row, got {row:?}"),
        };
        let mouse = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: app.ui_regions.navigation_results.x,
            row: app
                .ui_regions
                .navigation_results
                .y
                .saturating_add(u16::try_from(visible_row).unwrap()),
            modifiers: KeyModifiers::NONE,
        };
        app.handle_mouse(mouse);
        assert!(app.navigation_picker.is_none());
        assert_eq!(app.navigation_caret.point.line, expected_result);
    }

    #[test]
    fn resized_narrow_navigation_picker_ignores_outside_hit_and_commits_visible_location() {
        let mut app = navigation_picker_app(40);
        app.navigation_picker
            .as_mut()
            .unwrap()
            .list_state
            .select(Some(35));
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| crate::ui::draw(frame, &mut app))
            .unwrap();
        terminal.resize(Rect::new(0, 0, 32, 10)).unwrap();
        terminal
            .draw(|frame| crate::ui::draw(frame, &mut app))
            .unwrap();
        let picker = app.navigation_picker.as_ref().unwrap();
        let offset = picker.list_state.offset();
        let visible = usize::from(app.ui_regions.navigation_results.height);
        assert!(visible > 0);
        let last_visible = visible - 1;
        let expected = offset + last_visible;
        let expected_result = match picker.visible_rows.get(expected) {
            Some(NavigationPickerRow::Result(index)) => *index,
            row => panic!("expected a result row, got {row:?}"),
        };

        // The row immediately outside the results viewport must not select or
        // commit a location.
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: app.ui_regions.navigation_results.x,
            row: app.ui_regions.navigation_results.bottom(),
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(
            app.navigation_picker
                .as_ref()
                .unwrap()
                .list_state
                .selected(),
            Some(35)
        );

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: app.ui_regions.navigation_results.x,
            row: app
                .ui_regions
                .navigation_results
                .y
                .saturating_add(u16::try_from(last_visible).unwrap()),
            modifiers: KeyModifiers::NONE,
        });
        assert!(app.navigation_picker.is_none());
        assert_eq!(app.navigation_caret.point.line, expected_result);
    }

    #[test]
    fn semantic_navigation_without_an_available_engine_keeps_the_preview_in_place() {
        let mut app = navigation_app("fn caller() {}\n");
        let before = app.content_lines.clone();
        let identity = app.content_identity.clone();

        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        for _ in 0..100 {
            app.poll_background();
            if app.navigation_invocation.is_none() {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }

        assert_eq!(app.content_lines, before);
        assert_eq!(app.content_identity, identity);
        assert!(
            app.navigation_status
                .as_ref()
                .is_some_and(|status| status.message.contains("unavailable for Rust"))
        );
        assert!(app.navigation_back.is_empty());
    }

    #[test]
    fn unified_control_shortcuts_dispatch_semantic_navigation() {
        let cases = [
            (
                KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
                NavigationOperation::Definition,
            ),
            (
                KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL),
                NavigationOperation::References,
            ),
            (
                KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL),
                NavigationOperation::Implementations,
            ),
        ];

        for (key, expected) in cases {
            let mut app = navigation_app("fn caller() {}\n");
            app.handle_key(key);
            assert_eq!(
                app.navigation_invocation
                    .as_ref()
                    .map(|invocation| invocation.operation),
                Some(expected)
            );
            app.cancel_pending_navigation();
        }
    }

    #[test]
    fn ctrl_s_opens_local_document_symbols_in_the_results_popup() {
        let mut app = navigation_app("fn caller() {}\n");

        app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));

        let picker = app.navigation_picker.as_ref().unwrap();
        assert_eq!(picker.title, "Document Symbols");
        assert_eq!(picker.results.len(), 1);
        assert!(picker.preview.is_some());
    }

    #[test]
    fn references_and_implementations_always_open_results_even_for_one_location() {
        for operation in [
            NavigationOperation::References,
            NavigationOperation::Implementations,
        ] {
            let mut app = navigation_app("fn caller() {}\n");
            app.request_semantic_navigation(operation);
            let invocation = app.navigation_invocation.clone().unwrap();
            let uri = crate::navigation::path_to_lsp_uri(&app.root.join("caller.rs")).unwrap();
            app.apply_navigation_completion(NavigationRuntimeCompletion {
                generation: invocation.generation,
                operation,
                source_identity: invocation.source_identity,
                source_version: invocation.source_version,
                result: NavigationProtocolResult::Locations(vec![ProtocolLocation::for_app_test(
                    uri,
                    lsp_types::Range::new(
                        lsp_types::Position::new(0, 3),
                        lsp_types::Position::new(0, 9),
                    ),
                )]),
            });

            let picker = app.navigation_picker.as_ref().unwrap();
            assert_eq!(picker.results.len(), 1);
            assert_eq!(picker.groups.len(), 1);
            assert_eq!(
                picker.visible_rows,
                [
                    NavigationPickerRow::Group(0),
                    NavigationPickerRow::Result(0)
                ]
            );
            assert!(picker.preview.is_some());
        }
    }

    #[test]
    fn navigation_opens_a_dependency_source_without_selecting_it_in_the_tree() {
        let mut app = navigation_app("fn caller() {}\n");
        let dependency = app
            .root
            .parent()
            .unwrap()
            .join("dependency-cache/example.com/module@v1.2.3");
        fs::create_dir_all(&dependency).unwrap();
        fs::write(dependency.join("go.mod"), "module example.com/module\n").unwrap();
        fs::write(dependency.join("source.rs"), "pub fn dependency() {}\n").unwrap();
        // Windows temporary-directory paths can carry an extended-length
        // prefix until normalized. Model the URI sent by a language server,
        // which uses its canonical filesystem spelling.
        let dependency = dependency.canonicalize().unwrap();
        let target_path = dependency.join("source.rs");
        let uri = crate::navigation::path_to_lsp_uri(&target_path).unwrap();
        let classified_target = crate::navigation::lsp_uri_to_navigation_target(&uri, &app.root)
            .expect("the external package fixture must classify as a safe dependency target");
        assert!(matches!(
            classified_target,
            NavigationFileTarget::Dependency(_)
        ));
        let caller = app.content_identity.clone().unwrap();

        app.request_semantic_navigation(NavigationOperation::Definition);
        let invocation = app.navigation_invocation.clone().unwrap();
        app.apply_navigation_completion(NavigationRuntimeCompletion {
            generation: invocation.generation,
            operation: invocation.operation,
            source_identity: invocation.source_identity,
            source_version: invocation.source_version,
            result: NavigationProtocolResult::Locations(vec![ProtocolLocation::for_app_test(
                uri,
                lsp_types::Range::new(
                    lsp_types::Position::new(0, 7),
                    lsp_types::Position::new(0, 17),
                ),
            )]),
        });
        app.wait_for_background();

        let Some(ContentIdentity::Dependency { root, relative, .. }) =
            app.content_identity.as_ref()
        else {
            panic!(
                "expected an external navigation target, got {:?}; status: {:?}",
                app.content_identity, app.navigation_status
            );
        };
        assert!(root.ends_with(Path::new("dependency-cache/example.com/module@v1.2.3")));
        assert_eq!(relative, Path::new("source.rs"));
        assert_eq!(app.selected_content_title(), "Dependency Source");
        assert!(app.selected_content_label().starts_with("Dependency ·"));
        assert_eq!(app.tree_scope, TreeScope::AllFiles);
        assert!(
            app.all_entries
                .iter()
                .all(|entry| !entry.relative.starts_with("dependency-cache"))
        );
        assert_eq!(app.navigation_back.len(), 1);

        app.navigate_history(NavigationHistoryIntent::Back);
        app.wait_for_background();
        assert_eq!(app.content_identity.as_ref(), Some(&caller));
    }

    #[test]
    fn results_preview_loads_cross_file_without_replacing_content_until_accept() {
        let mut app = navigation_app("fn caller() {}\n");
        let caller = app.content_identity.clone().unwrap();
        let origin = app.current_navigation_entry().unwrap();
        let generation = app.next_navigation_generation();
        let target =
            ContentIdentity::from_absolute(&app.root, &app.root.join("target.rs")).unwrap();
        let invocation = NavigationInvocation {
            generation,
            operation: NavigationOperation::References,
            source_identity: caller.clone(),
            source_version: app.navigation_document_version,
            origin,
            history_intent: NavigationHistoryIntent::Jump,
            destination_viewport: None,
            return_focus: FocusPane::Content,
        };
        app.open_navigation_picker(
            "References",
            invocation,
            vec![NavigationPickerItem {
                target: NavigationTarget {
                    document: target.clone(),
                    range: NavigationTargetRange::Utf16(lsp_types::Range::new(
                        lsp_types::Position::new(0, 3),
                        lsp_types::Position::new(0, 9),
                    )),
                },
                label: "target.rs:1:4".to_owned(),
                detail: None,
            }],
        );

        assert!(app.is_navigation_preview_loading());
        assert!(!app.is_content_loading());
        assert_eq!(app.content_identity.as_ref(), Some(&caller));
        app.wait_for_background();

        let picker = app.navigation_picker.as_ref().unwrap();
        let preview = picker.preview.as_ref().unwrap();
        assert_eq!(preview.path, Path::new("target.rs"));
        assert_eq!(preview.lines, ["fn target() {}"]);
        assert_eq!(app.content_identity.as_ref(), Some(&caller));

        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        app.wait_for_background();
        assert!(app.navigation_picker.is_none());
        assert_eq!(app.content_identity.as_ref(), Some(&target));
        assert_eq!(app.navigation_back.len(), 1);
    }

    #[test]
    fn navigation_file_group_click_only_collapses_without_committing() {
        let mut app = navigation_picker_app(3);
        assert_eq!(
            app.navigation_picker.as_ref().unwrap().visible_rows.len(),
            4
        );

        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| crate::ui::draw(frame, &mut app))
            .unwrap();
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: app.ui_regions.navigation_results.x,
            row: app.ui_regions.navigation_results.y,
            modifiers: KeyModifiers::NONE,
        });

        let picker = app.navigation_picker.as_ref().unwrap();
        assert_eq!(picker.visible_rows, [NavigationPickerRow::Group(0)]);
        assert!(picker.preview.is_none());
        assert_eq!(
            app.content_identity.as_ref().map(ContentIdentity::path),
            Some(Path::new("caller.rs"))
        );
    }

    #[test]
    fn alt_hover_underlines_the_complete_token_and_alt_click_requests_definition() {
        let mut app = navigation_app("fn caller() {}\n");
        let column = u16::try_from(app.content_gutter_width().saturating_add(3)).unwrap();
        let hover = MouseEvent {
            kind: MouseEventKind::Moved,
            column,
            row: 0,
            modifiers: KeyModifiers::ALT,
        };

        app.handle_mouse(hover);
        let token = SourceRange {
            start: SourcePosition { line: 0, byte: 3 },
            end: SourcePosition { line: 0, byte: 9 },
        };
        assert_eq!(app.navigation_hover_highlight, Some(token));
        assert!(app.navigation_highlights(0).iter().any(|highlight| {
            highlight.kind == HighlightKind::NavigationHover && highlight.range == (3..9)
        }));

        app.handle_mouse(MouseEvent {
            modifiers: KeyModifiers::NONE,
            ..hover
        });
        assert!(app.navigation_hover_highlight.is_none());

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            ..hover
        });
        assert_eq!(
            app.navigation_invocation
                .as_ref()
                .map(|invocation| invocation.operation),
            Some(NavigationOperation::Definition)
        );
    }

    #[test]
    fn retired_navigation_aliases_do_not_dispatch_semantic_navigation() {
        let mut app = navigation_app("fn caller() {}\n");
        for key in [
            KeyEvent::new(KeyCode::F(12), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::F(7), KeyModifiers::ALT),
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
        ] {
            app.handle_key(key);
        }
        assert!(app.navigation_invocation.is_none());
    }

    #[test]
    fn stale_navigation_completion_cannot_replace_the_current_invocation() {
        let mut app = navigation_app("fn caller() {}\n");
        app.request_semantic_navigation(NavigationOperation::Definition);
        let invocation = app.navigation_invocation.clone().unwrap();
        let before = app.content_lines.clone();

        app.apply_navigation_completion(NavigationRuntimeCompletion {
            generation: invocation.generation.saturating_sub(1),
            operation: invocation.operation,
            source_identity: invocation.source_identity.clone(),
            source_version: invocation.source_version,
            result: NavigationProtocolResult::Locations(Vec::new()),
        });

        assert_eq!(app.content_lines, before);
        assert_eq!(
            app.navigation_invocation
                .as_ref()
                .map(|current| current.generation),
            Some(invocation.generation)
        );
        app.cancel_pending_navigation();
    }

    #[test]
    fn failed_cross_file_navigation_stage_is_atomic() {
        let mut app = navigation_app("fn caller() {}\n");
        let before_lines = app.content_lines.clone();
        let before_identity = app.content_identity.clone();
        let before_scope = app.tree_scope;
        let origin = app.current_navigation_entry().unwrap();
        let generation = app.next_navigation_generation();
        let invocation = NavigationInvocation {
            generation,
            operation: NavigationOperation::Definition,
            source_identity: origin.target.document.clone(),
            source_version: app.navigation_document_version,
            origin,
            history_intent: NavigationHistoryIntent::Jump,
            destination_viewport: None,
            return_focus: app.focused_pane,
        };
        let target_path = app.root.join("target.rs");
        let target_identity = ContentIdentity::from_absolute(&app.root, &target_path).unwrap();
        app.accept_navigation_target(
            invocation,
            NavigationTarget {
                document: target_identity,
                range: NavigationTargetRange::Utf16(lsp_types::Range::new(
                    lsp_types::Position::new(0, 3),
                    lsp_types::Position::new(0, 9),
                )),
            },
        );
        let stage = app.pending_navigation_stage.clone().unwrap();

        app.apply_content_completion(ContentCompletion {
            generation: stage.content_generation,
            kind: ContentKind::Preview,
            purpose: ContentPurpose::NavigationStage {
                navigation_generation: stage.invocation.generation,
            },
            result: Err("fixture stage failure".to_owned()),
        });

        assert_eq!(app.content_lines, before_lines);
        assert_eq!(app.content_identity, before_identity);
        assert_eq!(app.tree_scope, before_scope);
        assert!(app.navigation_back.is_empty());
        assert!(app.pending_navigation_stage.is_none());
        assert!(
            app.navigation_status
                .as_ref()
                .is_some_and(|status| status.message.contains("fixture stage failure"))
        );
    }

    #[test]
    fn successful_cross_file_stage_commits_history_then_back_and_forward() {
        let mut app = navigation_app("fn caller() {}\n");
        let caller = app.content_identity.clone().unwrap();
        let origin = app.current_navigation_entry().unwrap();
        let generation = app.next_navigation_generation();
        let invocation = NavigationInvocation {
            generation,
            operation: NavigationOperation::Definition,
            source_identity: caller.clone(),
            source_version: app.navigation_document_version,
            origin,
            history_intent: NavigationHistoryIntent::Jump,
            destination_viewport: None,
            return_focus: app.focused_pane,
        };
        let target_path = app.root.join("target.rs");
        let target = ContentIdentity::from_absolute(&app.root, &target_path).unwrap();
        app.accept_navigation_target(
            invocation,
            NavigationTarget {
                document: target.clone(),
                range: NavigationTargetRange::Utf16(lsp_types::Range::new(
                    lsp_types::Position::new(0, 3),
                    lsp_types::Position::new(0, 9),
                )),
            },
        );
        app.wait_for_background();

        assert_eq!(app.content_identity.as_ref(), Some(&target));
        assert_eq!(app.navigation_back.len(), 1);
        assert!(app.navigation_forward.is_empty());

        app.navigate_history(NavigationHistoryIntent::Back);
        app.wait_for_background();
        assert_eq!(app.content_identity.as_ref(), Some(&caller));
        assert!(app.navigation_back.is_empty());
        assert_eq!(app.navigation_forward.len(), 1);

        app.navigate_history(NavigationHistoryIntent::Forward);
        app.wait_for_background();
        assert_eq!(app.content_identity.as_ref(), Some(&target));
        assert_eq!(app.navigation_back.len(), 1);
        assert!(app.navigation_forward.is_empty());
    }

    #[test]
    fn copy_path_relative_returns_workspace_relative_path() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("helper.rs"), "fn h() {}\n").unwrap();
        let mut app = App::new(directory.path().to_path_buf()).unwrap();
        app.wait_for_background();

        let path = app
            .selected_copy_path(false)
            .expect("selected_copy_path returns Some for a file");
        assert_eq!(path, PathBuf::from("helper.rs"));
    }

    #[test]
    fn copy_path_relative_for_directory_has_no_trailing_slash() {
        let directory = tempfile::tempdir().unwrap();
        let src = directory.path().join("src");
        fs::create_dir(&src).unwrap();
        fs::write(src.join("lib.rs"), "// lib\n").unwrap();
        let mut app = App::new(directory.path().to_path_buf()).unwrap();
        app.wait_for_background();

        // Select the directory entry (first entry should be "src").
        let dir_path = app
            .all_entries
            .iter()
            .find(|e| e.relative == Path::new("src"))
            .map(|e| e.relative.clone())
            .expect("src directory is present");
        assert_eq!(dir_path, PathBuf::from("src"));

        let path = app
            .selected_copy_path(false)
            .expect("selected_copy_path returns Some");
        // The method does not append a slash; the caller adds it.
        assert_eq!(path, PathBuf::from("src"));
    }

    #[cfg(unix)]
    #[test]
    fn copy_path_resolve_returns_real_path_for_all_files_symlink() {
        use std::os::unix::fs::symlink;

        let parent = tempfile::tempdir().unwrap();
        let workspace = parent.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("real-target.txt");
        fs::write(&target, "real content\n").unwrap();
        symlink(&target, workspace.join("a-link.txt")).unwrap();

        let mut app = App::new(workspace.clone()).unwrap();
        app.wait_for_background();
        assert_eq!(app.tree_scope, TreeScope::AllFiles);

        let resolved = app
            .selected_copy_path(true)
            .expect("selected_copy_path(true) returns Some");
        // The resolved real path should point to the target, not the link.
        assert_eq!(resolved, target.canonicalize().unwrap_or(target.clone()));
        assert_ne!(resolved, workspace.join("a-link.txt"));
    }

    #[cfg(unix)]
    #[test]
    fn copy_path_resolve_git_changes_scope_returns_none_without_git_repo() {
        use std::os::unix::fs::symlink;

        let parent = tempfile::tempdir().unwrap();
        let workspace = parent.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("real-target.txt");
        fs::write(&target, "real content\n").unwrap();
        symlink(&target, workspace.join("a-link.txt")).unwrap();

        let mut app = App::new(workspace.clone()).unwrap();
        app.wait_for_background();
        // Switch to Git Changes scope: without a git repo there are no rows,
        // so selected_entry() yields None and selected_copy_path() returns None.
        app.tree_scope = TreeScope::GitChanges;
        assert!(app.selected_copy_path(true).is_none());
    }

    #[test]
    fn copy_path_resolve_regular_file_returns_absolute_path() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("plain.txt"), "hello\n").unwrap();
        let mut app = App::new(directory.path().to_path_buf()).unwrap();
        app.wait_for_background();

        let path = app
            .selected_copy_path(true)
            .expect("selected_copy_path returns Some");
        // App::new canonicalizes the root, so the resolved path uses the
        // canonical root. It does NOT additionally canonicalize the entry.
        let expected = directory
            .path()
            .canonicalize()
            .unwrap_or_else(|_| directory.path().to_path_buf())
            .join("plain.txt");
        assert_eq!(path, expected);
    }

    #[test]
    fn queue_selected_path_copy_sets_status_for_relative_file() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("helper.rs"), "fn h() {}\n").unwrap();
        let mut app = App::new(directory.path().to_path_buf()).unwrap();
        app.wait_for_background();

        app.queue_selected_path_copy(false);

        // Either the clipboard succeeded (status set) or failed (last_error set).
        // The path must appear in the status message on success.
        if let Some(status) = &app.clipboard_status {
            assert!(status.contains("Copied path"), "status: {status}");
            assert!(status.contains("helper.rs"), "status: {status}");
        } else {
            // Clipboard unavailable in test env: last_error should be set.
            assert!(app.last_error.is_some());
        }
    }

    #[test]
    fn queue_selected_path_copy_directory_gets_trailing_slash() {
        let directory = tempfile::tempdir().unwrap();
        let src = directory.path().join("src");
        fs::create_dir(&src).unwrap();
        fs::write(src.join("lib.rs"), "// lib\n").unwrap();
        let mut app = App::new(directory.path().to_path_buf()).unwrap();
        app.wait_for_background();

        // Select the "src" directory.
        let idx = app
            .all_entries
            .iter()
            .position(|e| e.relative == Path::new("src"))
            .expect("src directory exists");
        app.tree_state.select(Some(idx));
        app.queue_selected_path_copy(false);

        if let Some(status) = &app.clipboard_status {
            assert!(
                status.ends_with("src/"),
                "directory path should end with slash: {status}"
            );
        }
    }

    #[test]
    fn queue_selected_path_copy_no_selection_shows_guidance() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("helper.rs"), "fn h() {}\n").unwrap();
        let mut app = App::new(directory.path().to_path_buf()).unwrap();
        app.wait_for_background();

        // Clear the selection to simulate no entry selected.
        app.tree_state.select(None);
        app.queue_selected_path_copy(false);

        assert_eq!(
            app.clipboard_status.as_deref(),
            Some("Select a file or directory to copy its path")
        );
    }

    #[test]
    fn queue_selected_path_copy_sets_status_for_absolute_file() {
        // Force OSC52 so the copy always succeeds (writes to stdout) regardless
        // of whether a native clipboard command is available in the test env.
        // SAFETY: see the env helper in the integration tests; osc52 is benign.
        unsafe { std::env::set_var("LATTELENS_CLIPBOARD", "osc52") };

        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("helper.rs"), "fn h() {}\n").unwrap();
        let mut app = App::new(directory.path().to_path_buf()).unwrap();
        app.wait_for_background();

        app.queue_selected_path_copy(true);

        // App::new canonicalizes the root, so use app.root for the expected path.
        let expected = app.root.join("helper.rs").display().to_string();
        let status = app
            .clipboard_status
            .as_deref()
            .expect("clipboard_status is set on successful copy");
        assert_eq!(status, format!("Copied absolute path: {expected}"));
    }

    #[test]
    fn queue_selected_path_copy_clears_pending_clipboard_text() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("helper.rs"), "fn h() {}\n").unwrap();
        let mut app = App::new(directory.path().to_path_buf()).unwrap();
        app.wait_for_background();

        // Simulate a stale content-selection copy pending flush.
        app.pending_clipboard_text = Some("stale selection text".to_owned());
        app.queue_selected_path_copy(false);

        // The path copy always clears the pending content-selection copy.
        assert!(
            app.pending_clipboard_text.is_none(),
            "pending_clipboard_text must be cleared after a path copy"
        );
    }

    #[test]
    fn queue_selected_path_copy_clears_last_error_on_success() {
        // SAFETY: osc52 makes the copy deterministic (always succeeds).
        unsafe { std::env::set_var("LATTELENS_CLIPBOARD", "osc52") };

        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("helper.rs"), "fn h() {}\n").unwrap();
        let mut app = App::new(directory.path().to_path_buf()).unwrap();
        app.wait_for_background();

        // Simulate a stale copy error from a previous attempt.
        app.last_error = Some("copy failed: stale clipboard error".to_owned());
        app.queue_selected_path_copy(false);

        // A successful copy clears a prior "copy failed:" error.
        assert!(
            app.last_error.is_none(),
            "last_error should be cleared after a successful copy"
        );
        assert!(
            app.clipboard_status.is_some(),
            "clipboard_status should be set on success"
        );
    }

    #[test]
    fn queue_selected_path_copy_preserves_non_copy_error() {
        // SAFETY: osc52 makes the copy deterministic (always succeeds).
        unsafe { std::env::set_var("LATTELENS_CLIPBOARD", "osc52") };

        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("helper.rs"), "fn h() {}\n").unwrap();
        let mut app = App::new(directory.path().to_path_buf()).unwrap();
        app.wait_for_background();

        // A non-copy error (not starting with "copy failed:") must be preserved.
        app.last_error = Some("scan failed: permission denied".to_owned());
        app.queue_selected_path_copy(false);

        assert_eq!(
            app.last_error.as_deref(),
            Some("scan failed: permission denied"),
            "non-copy errors must not be cleared by a successful path copy"
        );
    }

    fn drag_content(
        app: &mut App,
        start_column: u16,
        start_row: u16,
        end_column: u16,
        end_row: u16,
    ) {
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: start_column,
            row: start_row,
            modifiers: KeyModifiers::NONE,
        });
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: end_column,
            row: end_row,
            modifiers: KeyModifiers::NONE,
        });
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: end_column,
            row: end_row,
            modifiers: KeyModifiers::NONE,
        });
    }
}
