use std::{
    collections::{HashMap, HashSet},
    io,
    ops::Range,
    path::{Path, PathBuf},
    time::Duration,
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
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::{
    clipboard,
    git::{FileStatus, GitRepo},
    preview::{HighlightSpan, PreviewProvider, PreviewRegistry},
    repo_graph::{
        DiscoveryError, DiscoveryTruncation, RepoChange, RepoGraph, RepoId, RepoKind, RepoPath,
        RepoRelationState, RepoSnapshot,
    },
    runtime::{
        ContentCompletion, ContentKind, ContentRequest, ContentTarget, RefreshCompletion,
        RefreshRequest, RefreshSnapshot, RequestGeneration, WorkerRuntime,
    },
    tree::{self, FileEntry},
    ui,
};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TreeScope {
    #[default]
    AllFiles,
    GitChanges,
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
            Self::GitChanges => Self::AllFiles,
        }
    }

    pub const fn previous(self) -> Self {
        self.next()
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
    pub refresh_button: Rect,
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
        } else {
            None
        }
    }

    fn refresh_at(self, column: u16, row: u16) -> bool {
        contains(self.refresh_button, column, row)
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
    all_files_expansion: HashMap<PathBuf, bool>,
    git_changes_expansion: HashMap<GitRowIdentity, bool>,
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
    last_refresh_error: Option<String>,
    should_quit: bool,
}

impl App {
    pub fn new(path: PathBuf) -> Result<Self> {
        Self::with_preview_registry(path, PreviewRegistry::with_builtins())
    }

    pub fn with_preview_registry(path: PathBuf, preview_registry: PreviewRegistry) -> Result<Self> {
        Self::with_preview_registry_and_scan_limit(
            path,
            preview_registry,
            tree::DEFAULT_MAX_ENTRIES,
        )
    }

    fn with_preview_registry_and_scan_limit(
        path: PathBuf,
        preview_registry: PreviewRegistry,
        scan_entry_limit: usize,
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

        let runtime = WorkerRuntime::start(root.clone(), preview_registry.clone())?;
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
            content_lines: Vec::new(),
            content_highlights: Vec::new(),
            content_scroll: 0,
            content_horizontal_scroll: 0,
            content_selection: None,
            pending_clipboard_text: None,
            clipboard_status: None,
            content_mode: ContentMode::Info,
            content_provider: None,
            content_show_line_numbers: false,
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
            all_files_expansion: HashMap::new(),
            git_changes_expansion: HashMap::new(),
            visible_rows: Vec::new(),
            visible_changed_entries: Vec::new(),
            git_rows: Vec::new(),
            visible_git_rows: Vec::new(),
            scan_entry_limit,
            runtime,
            refresh_requests: RequestGeneration::default(),
            content_requests: RequestGeneration::default(),
            last_refresh_error: None,
            should_quit: false,
        };
        app.request_refresh();
        app.wait_for_refresh();
        if let Some(error) = app.last_refresh_error.take() {
            anyhow::bail!(error);
        }
        app.wait_for_background();
        Ok(app)
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

            if event::poll(Duration::from_millis(250))? {
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
        Ok(())
    }

    pub fn visible_entries(&self) -> &[FileEntry] {
        match self.tree_scope {
            TreeScope::AllFiles => &self.visible_rows,
            TreeScope::GitChanges => &self.visible_changed_entries,
        }
    }

    pub fn visible_git_rows(&self) -> &[GitTreeRow] {
        &self.visible_git_rows
    }

    pub fn tree_row_count(&self) -> usize {
        match self.tree_scope {
            TreeScope::AllFiles => self.visible_rows.len(),
            TreeScope::GitChanges => self.visible_git_rows.len(),
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
        }
    }

    pub const fn scope_is_truncated(&self) -> bool {
        match self.tree_scope {
            TreeScope::AllFiles => self.all_files_truncated,
            TreeScope::GitChanges => self.git_changes_truncated,
        }
    }

    pub fn selected_entry(&self) -> Option<&FileEntry> {
        let index = self.tree_state.selected()?;
        match self.tree_scope {
            TreeScope::AllFiles => self.visible_rows.get(index),
            TreeScope::GitChanges => self.visible_git_rows.get(index)?.file_entry(),
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
        }
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

    pub fn selected_content_label(&self) -> String {
        match self.tree_scope {
            TreeScope::AllFiles => self
                .selected_entry()
                .map(|entry| entry.relative.display().to_string())
                .unwrap_or_default(),
            TreeScope::GitChanges => self
                .selected_git_row()
                .map(|row| row.label.clone())
                .unwrap_or_default(),
        }
    }

    pub fn selected_content_title(&self) -> &'static str {
        let Some(row) = self.selected_git_row() else {
            return if self.selected_entry().is_some_and(|entry| entry.is_dir) {
                "Directory"
            } else {
                self.content_mode.label()
            };
        };
        match row.kind {
            GitRowKind::Repository { .. } => "Repository",
            GitRowKind::Directory => "Directory",
            GitRowKind::Pointer(_) => "Submodule pointer",
            GitRowKind::Issue(_) => "Repository error",
            GitRowKind::Change(_) => match self.content_mode {
                ContentMode::Info => "Info",
                ContentMode::Diff => "Diff",
                ContentMode::Preview => "Preview",
            },
        }
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
        let wrap_preview = self.content_mode == ContentMode::Preview;
        let gutter_width = if self.content_show_line_numbers {
            self.content_lines.len().max(1).to_string().len() + 3
        } else {
            0
        };
        let text_width = usize::from(width).saturating_sub(gutter_width).max(1);
        self.content_lines
            .iter()
            .enumerate()
            .flat_map(|(line_index, line)| {
                if wrap_preview {
                    wrap_line_ranges(line, text_width)
                        .into_iter()
                        .enumerate()
                        .map(move |(index, byte_range)| ContentVisualRow {
                            line_index,
                            byte_range,
                            continuation: index > 0,
                        })
                        .collect::<Vec<_>>()
                } else {
                    vec![ContentVisualRow {
                        line_index,
                        byte_range: 0..line.len(),
                        continuation: false,
                    }]
                }
            })
            .collect()
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
        match (key.code, key.modifiers) {
            (KeyCode::Char('c' | 'C'), modifiers)
                if modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER) =>
            {
                self.queue_selected_preview_copy();
            }
            (KeyCode::Char('q') | KeyCode::Esc, _) => self.should_quit = true,
            (KeyCode::Tab, _) => self.set_tree_scope(self.tree_scope.next()),
            (KeyCode::BackTab, _) => self.set_tree_scope(self.tree_scope.previous()),
            (KeyCode::Char('1'), KeyModifiers::NONE) => {
                self.set_tree_scope(TreeScope::AllFiles);
            }
            (KeyCode::Char('2'), KeyModifiers::NONE) => {
                self.set_tree_scope(TreeScope::GitChanges);
            }
            (KeyCode::Char('h'), KeyModifiers::NONE) => self.focused_pane = FocusPane::Tree,
            (KeyCode::Char('l'), KeyModifiers::NONE) => self.focused_pane = FocusPane::Content,
            (KeyCode::Char('r'), _) => self.request_refresh(),
            (KeyCode::Char('p'), KeyModifiers::NONE) => self.load_selected_preview(),
            (KeyCode::Char('d'), KeyModifiers::NONE) => self.load_selected_diff(),
            (KeyCode::Char('n'), KeyModifiers::NONE) if self.content_mode == ContentMode::Diff => {
                self.select_changed(1);
            }
            (KeyCode::Char('N'), _) if self.content_mode == ContentMode::Diff => {
                self.select_changed(-1);
            }
            _ => match self.focused_pane {
                FocusPane::ScopeTabs => self.handle_scope_tabs_key(key),
                FocusPane::Tree => self.handle_tree_key(key),
                FocusPane::Content => self.handle_content_key(key),
            },
        }
    }

    /// Apply a mouse event using hit boxes captured during the latest draw.
    pub fn handle_mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.clipboard_status = None;
                self.tree_resize_dragging = false;
                if self.ui_regions.refresh_at(mouse.column, mouse.row) {
                    self.clear_content_selection();
                    self.focused_pane = FocusPane::Tree;
                    self.request_refresh();
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
        self.content_selection = Some(ContentSelection {
            anchor_before: before,
            anchor_after: after,
            head: before,
            dragging: true,
            dragged: false,
        });
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
        let visual_row = self
            .content_scroll
            .saturating_add(visible_row)
            .min(visual_rows.len().saturating_sub(1));
        let visual_row = visual_rows.get(visual_row)?;
        let visible_column = usize::from(mouse.column.saturating_sub(rows.x));
        let rendered_column = if self.content_mode == ContentMode::Preview {
            visible_column
        } else {
            self.content_horizontal_scroll
                .saturating_add(visible_column)
        };
        let gutter_width = if self.content_show_line_numbers {
            self.content_lines.len().max(1).to_string().len() + 3
        } else {
            0
        };
        let text_column = rendered_column.saturating_sub(gutter_width);
        let line = self.content_lines.get(visual_row.line_index)?;
        let segment = line.get(visual_row.byte_range.clone())?;
        let (before, after) = grapheme_bounds_at_column(segment, text_column);
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
        let first_git_entry = scope == TreeScope::GitChanges
            && self.tree_scope != TreeScope::GitChanges
            && self.git_changes_selection.is_none();
        if scope == TreeScope::GitChanges {
            self.request_refresh();
        }

        self.apply_tree_scope(scope);
        // A clean repository row is a valid fallback, but on the first entry
        // it must not become a saved preference while the requested refresh
        // is in flight. Leaving the selection open lets the refreshed view
        // keep the established changed-file-first default. Explicit and
        // previously saved Git selections remain stable across refreshes.
        if first_git_entry {
            self.tree_state.select(None);
        }
    }

    fn apply_tree_scope(&mut self, scope: TreeScope) {
        if self.tree_scope == scope {
            return;
        }

        self.remember_current_selection();
        self.tree_scope = scope;
        self.tree_state = ListState::default();
        self.rebuild_visible_rows();
        match scope {
            TreeScope::AllFiles => {
                self.restore_visible_selection(self.all_files_selection.clone());
            }
            TreeScope::GitChanges => {
                self.restore_git_selection(self.git_changes_selection.clone());
            }
        }
    }

    pub fn should_quit(&self) -> bool {
        self.should_quit
    }

    pub const fn is_refreshing(&self) -> bool {
        self.refresh_requests.is_loading()
    }

    pub const fn is_content_loading(&self) -> bool {
        self.content_requests.is_loading()
    }

    fn handle_scope_tabs_key(&mut self, key: KeyEvent) {
        match (key.code, key.modifiers) {
            (KeyCode::Left, KeyModifiers::NONE) => self.set_tree_scope(TreeScope::AllFiles),
            (KeyCode::Right, KeyModifiers::NONE) => self.set_tree_scope(TreeScope::GitChanges),
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
            (KeyCode::Down | KeyCode::Char('j'), _) => self.scroll_content(1, 0),
            (KeyCode::Up | KeyCode::Char('k'), _) => self.scroll_content(-1, 0),
            (KeyCode::PageDown, _) | (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
                self.scroll_content(12, 0);
            }
            (KeyCode::PageUp, _) | (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
                self.scroll_content(-12, 0);
            }
            (KeyCode::Left, KeyModifiers::SHIFT) => self.scroll_content(0, -4),
            (KeyCode::Right, KeyModifiers::SHIFT) => self.scroll_content(0, 4),
            (KeyCode::Left, KeyModifiers::NONE) => self.focused_pane = FocusPane::Tree,
            (KeyCode::Right, KeyModifiers::NONE) => self.focused_pane = FocusPane::Content,
            (KeyCode::Home | KeyCode::Char('g'), _) => self.content_scroll = 0,
            (KeyCode::End | KeyCode::Char('G'), _) => {
                self.content_scroll = self
                    .content_visual_rows(self.ui_regions.content_inner.width)
                    .len()
                    .saturating_sub(1);
            }
            _ => {}
        }
    }

    fn handle_mouse_scroll(&mut self, mouse: MouseEvent, delta: isize) {
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
        self.rebuild_visible_rows();
        self.restore_visible_selection(Some(relative));
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
            }
        });
        if let Some(index) = target {
            self.select(index);
            self.load_selected_diff();
        }
    }

    fn scroll_content(&mut self, vertical: isize, horizontal: isize) {
        self.content_scroll = self.content_scroll.saturating_add_signed(vertical);
        if self.content_mode != ContentMode::Preview {
            self.content_horizontal_scroll = self
                .content_horizontal_scroll
                .saturating_add_signed(horizontal);
        }
    }

    fn request_refresh(&mut self) {
        self.remember_current_selection();
        let generation = self.refresh_requests.begin();
        self.last_refresh_error = None;
        self.runtime.request_refresh(RefreshRequest {
            generation,
            scan_entry_limit: self.scan_entry_limit,
        });
    }

    pub fn poll_background(&mut self) {
        let (refresh, content) = self.runtime.take_completions();
        if let Some(completion) = refresh {
            self.apply_refresh_completion(completion);
        }
        if let Some(completion) = content {
            self.apply_content_completion(completion);
        }
    }

    /// Wait until all currently requested work is reduced into application
    /// state. The interactive loop never calls this; it is useful to startup,
    /// embedders, and deterministic tests that do not own an event loop.
    #[doc(hidden)]
    pub fn wait_for_background(&mut self) {
        while self.is_refreshing() || self.is_content_loading() {
            if !self.runtime.wait_for_completion() {
                self.last_error =
                    Some("background worker stopped before completing work".to_owned());
                break;
            }
            self.poll_background();
        }
    }

    fn wait_for_refresh(&mut self) {
        while self.is_refreshing() {
            if !self.runtime.wait_for_completion() {
                let error = "background worker stopped before refresh completed".to_owned();
                self.last_refresh_error = Some(error.clone());
                self.last_error = Some(error);
                break;
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
        self.remember_current_selection();
        let changed_entries = tree::changed_only(&snapshot.scan.entries);

        self.branch = snapshot.branch;
        self.all_files_truncated = snapshot.scan.truncated;
        // Git status paths are synthesized into the filtered tree, but that
        // tree still comes from the bounded filesystem traversal. Keep the
        // conservative partial marker in both views instead of claiming the
        // filesystem-derived Git projection is complete.
        self.git_changes_truncated = snapshot.scan.truncated;
        self.all_entries = snapshot.scan.entries;
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
            self.repository_graph_truncated = graph.report().is_truncated();
            self.git_changes_truncated |= self.repository_graph_truncated;
            self.git_rows = build_git_rows(&self.root, &graph, &snapshot.existing_changes);
            self.changed_count = self
                .git_rows
                .iter()
                .filter(|row| matches!(row.kind, GitRowKind::Change(_) | GitRowKind::Pointer(_)))
                .count();
            debug_assert_eq!(self.changed_count, snapshot.projected_change_count);
            self.repo_graph = Some(graph);
        } else {
            self.changed_count = 0;
            self.git_rows.clear();
        }
        self.reconcile_expansion_state();
        self.tree_state = ListState::default();
        self.rebuild_visible_rows();

        match self.tree_scope {
            TreeScope::AllFiles => {
                self.restore_visible_selection(self.all_files_selection.clone());
            }
            TreeScope::GitChanges => {
                self.restore_git_selection(self.git_changes_selection.clone());
            }
        }

        self.last_refresh_error = None;
        self.last_error = None;
    }

    fn default_selection_index(&self) -> Option<usize> {
        match self.tree_scope {
            TreeScope::AllFiles => (!self.visible_rows.is_empty()).then_some(0),
            TreeScope::GitChanges => self
                .visible_git_rows
                .iter()
                .position(GitTreeRow::is_change)
                .or_else(|| (!self.visible_git_rows.is_empty()).then_some(0)),
        }
    }

    fn remember_current_selection(&mut self) {
        match self.tree_scope {
            TreeScope::AllFiles => self.all_files_selection = self.selected_relative_path(),
            TreeScope::GitChanges => {
                self.git_changes_selection =
                    self.selected_git_row().map(|row| row.identity.clone());
            }
        }
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
            };
            self.set_info(vec![message.to_owned()]);
            return;
        }

        match self.tree_scope {
            TreeScope::AllFiles => self.load_selected_preview(),
            TreeScope::GitChanges => self.load_selected_diff(),
        }
    }

    fn load_selected_info(&mut self) {
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
            "Expanded · Enter or click to collapse."
        } else {
            "Collapsed · Enter or click to expand."
        };
        self.set_info(vec![summary, String::new(), action.to_owned()]);
    }

    fn load_selected_diff(&mut self) {
        if self.tree_scope == TreeScope::GitChanges {
            let Some(row) = self.selected_git_row().cloned() else {
                self.set_info(vec!["No repository row selected.".to_owned()]);
                return;
            };
            match row.kind {
                GitRowKind::Change(change) | GitRowKind::Pointer(change) => {
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

        self.request_content(
            ContentKind::Diff,
            relative.display().to_string(),
            ContentTarget::Repository(change),
        );
    }

    fn load_selected_preview(&mut self) {
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

        self.request_content(
            ContentKind::Preview,
            relative.display().to_string(),
            ContentTarget::Workspace(relative),
        );
    }

    fn request_content(&mut self, kind: ContentKind, label: String, target: ContentTarget) {
        let generation = self.content_requests.begin();
        self.reset_content(match kind {
            ContentKind::Diff => ContentMode::Diff,
            ContentKind::Preview => ContentMode::Preview,
        });
        self.content_lines = vec![format!("Loading {label}…")];
        self.runtime.request_content(ContentRequest {
            generation,
            kind,
            target,
        });
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

    fn apply_content_completion(&mut self, completion: ContentCompletion) {
        if !self.content_requests.accept(completion.generation) {
            return;
        }
        let mode = match completion.kind {
            ContentKind::Diff => ContentMode::Diff,
            ContentKind::Preview => ContentMode::Preview,
        };
        self.reset_content(mode);
        match completion.result {
            Ok(snapshot) => {
                self.content_provider = snapshot.provider;
                self.content_lines = snapshot.lines;
                self.content_highlights = snapshot.highlights;
                self.content_show_line_numbers = snapshot.show_line_numbers;
                if self
                    .last_error
                    .as_deref()
                    .is_some_and(|error| error.starts_with("content failed:"))
                {
                    self.last_error = None;
                }
            }
            Err(error) => {
                self.content_lines = vec![match completion.kind {
                    ContentKind::Diff => format!("Unable to load diff: {error}"),
                    ContentKind::Preview => format!("Unable to preview file: {error}"),
                }];
                self.last_error = Some(format!("content failed: {error}"));
            }
        }
    }

    fn reset_content(&mut self, mode: ContentMode) {
        self.content_scroll = 0;
        self.content_horizontal_scroll = 0;
        self.clear_content_selection();
        self.clipboard_status = None;
        self.content_mode = mode;
        self.content_provider = None;
        self.content_highlights.clear();
        self.content_show_line_numbers = false;
    }

    fn set_info(&mut self, lines: Vec<String>) {
        self.content_requests.invalidate();
        self.runtime.cancel_pending_content();
        self.reset_content(ContentMode::Info);
        self.content_lines = lines;
    }
}

fn grapheme_bounds_at_column(line: &str, column: usize) -> (usize, usize) {
    let mut display_column: usize = 0;
    for (byte, grapheme) in line.grapheme_indices(true) {
        let width = UnicodeWidthStr::width(grapheme);
        let end = byte + grapheme.len();
        if column < display_column.saturating_add(width.max(1)) {
            return (byte, end);
        }
        display_column = display_column.saturating_add(width);
    }
    (line.len(), line.len())
}

fn wrap_line_ranges(line: &str, width: usize) -> Vec<Range<usize>> {
    if line.is_empty() {
        return std::iter::once(0..0).collect();
    }

    let width = width.max(1);
    let mut ranges = Vec::new();
    let mut start = 0;
    let mut used_width: usize = 0;
    for (byte, grapheme) in line.grapheme_indices(true) {
        let grapheme_width = UnicodeWidthStr::width(grapheme).max(1);
        if used_width > 0 && used_width.saturating_add(grapheme_width) > width {
            ranges.push(start..byte);
            start = byte;
            used_width = 0;
        }
        used_width = used_width.saturating_add(grapheme_width);
    }
    ranges.push(start..line.len());
    ranges
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
    for truncation in &graph.report().truncations {
        let (path, message) = match truncation {
            DiscoveryTruncation::EntryLimit { limit } => (
                root.to_path_buf(),
                format!("Repository discovery stopped at the {limit}-entry limit."),
            ),
            DiscoveryTruncation::RepositoryLimit { limit } => (
                root.to_path_buf(),
                format!("Repository discovery stopped at the {limit}-repository limit."),
            ),
            DiscoveryTruncation::DepthLimit { limit, path } => (
                path.clone(),
                format!("Repository discovery stopped at depth {limit}."),
            ),
        };
        rows.push(GitTreeRow {
            identity: GitRowIdentity::Issue(path.clone()),
            kind: GitRowKind::Issue(message.clone()),
            depth: 0,
            label: format!("[partial] {}", compact_workspace_path(root, &path)),
            detail: message,
            status: None,
            exists: true,
            ancestors: Vec::new(),
            file_entry: None,
        });
    }
    rows
}

struct GitChangeProjection<'a> {
    pointer_paths: &'a HashSet<RepoPath>,
    existing_changes: &'a HashSet<RepoPath>,
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
    let detail = repository_detail(snapshot, direct_count + pointer_count);
    rows.push(GitTreeRow {
        identity: repo_identity.clone(),
        kind: GitRowKind::Repository {
            repo_id: id.clone(),
            kind: snapshot.node.kind,
            change_count: direct_count + pointer_count,
            status_error: snapshot.status_error.clone(),
        },
        depth: repo_ancestors.len(),
        label: repository_label(root, snapshot),
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

fn repository_label(_root: &Path, snapshot: &RepoSnapshot) -> String {
    snapshot
        .node
        .workspace_relative
        .as_deref()
        .filter(|path| !path.as_os_str().is_empty())
        .map(display_workspace_path)
        .unwrap_or_else(|| ".".to_owned())
}

fn display_workspace_path(path: &Path) -> String {
    path.iter()
        .map(|component| component.to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn repository_detail(snapshot: &RepoSnapshot, change_count: usize) -> String {
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
    parts.push(format!(
        "{change_count} file{}",
        if change_count == 1 { "" } else { "s" }
    ));
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

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::repo_graph::DiscoveryOptions;
    use crate::runtime::{ContentSnapshot, RefreshSnapshot};
    use crate::tree::ScanResult;

    #[test]
    fn grapheme_columns_keep_wide_and_combining_characters_atomic() {
        let line = "a拿铁e\u{301}";

        assert_eq!(grapheme_bounds_at_column(line, 0), (0, 1));
        assert_eq!(grapheme_bounds_at_column(line, 1), (1, 4));
        assert_eq!(grapheme_bounds_at_column(line, 2), (1, 4));
        assert_eq!(grapheme_bounds_at_column(line, 3), (4, 7));
        assert_eq!(grapheme_bounds_at_column(line, 5), (7, 10));
        assert_eq!(grapheme_bounds_at_column(line, 6), (10, 10));
    }

    #[test]
    fn preview_wrap_ranges_preserve_grapheme_boundaries_and_empty_lines() {
        assert_eq!(wrap_line_ranges("ab拿c", 3), [0..2, 2..6]);
        assert_eq!(wrap_line_ranges("e\u{301}xy", 2), [0..4, 4..5]);
        assert_eq!(
            wrap_line_ranges("", 8),
            std::iter::once(0..0).collect::<Vec<_>>()
        );
    }

    #[test]
    fn workspace_paths_use_portable_ui_separators() {
        let path = Path::new("modules").join("child").join("nested");

        assert_eq!(display_workspace_path(&path), "modules/child/nested");
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
            result: Ok(content_snapshot("obsolete preview")),
        });

        assert!(app.is_content_loading());
        assert_eq!(app.content_mode, ContentMode::Diff);
        assert_eq!(app.content_lines, ["Loading current diff…"]);

        app.apply_content_completion(ContentCompletion {
            generation: current_diff,
            kind: ContentKind::Diff,
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
    fn repository_discovery_truncation_becomes_a_partial_selectable_row() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("entry.txt"), "fixture").unwrap();
        let graph = RepoGraph::discover_with_options(
            directory.path(),
            DiscoveryOptions {
                max_entries: 0,
                max_repositories: 8,
                max_depth: 8,
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
            },
            graph: Some(graph),
            existing_changes: HashSet::new(),
        });
        app.apply_tree_scope(TreeScope::GitChanges);

        assert!(app.repository_graph_truncated);
        assert!(app.git_changes_truncated);
        assert!(app.visible_git_rows().iter().any(|row| {
            matches!(row.kind, GitRowKind::Issue(_)) && row.label.starts_with("[partial]")
        }));
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
            },
        }
    }

    fn content_snapshot(line: &str) -> ContentSnapshot {
        ContentSnapshot {
            provider: None,
            lines: vec![line.to_owned()],
            highlights: Vec::new(),
            show_line_numbers: false,
        }
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
