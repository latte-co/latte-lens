use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs,
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, bail};

use crate::git::{ChangeDetails, FileStatus, GitRepo, GitStatusEntry, SubmoduleStatus};

/// Maximum number of directories considered while looking for Git worktrees.
///
/// Ordinary files and `.git` markers do not consume this budget. The existing
/// name is retained because it is part of the public discovery API.
pub const DEFAULT_MAX_DISCOVERY_ENTRIES: usize = 50_000;
pub const DEFAULT_MAX_REPOSITORIES: usize = 1_024;
pub const DEFAULT_MAX_DISCOVERY_DEPTH: usize = 128;

/// Stable identity for a repository worktree within a machine.
///
/// Initialized repositories use their canonical worktree path. Placeholders
/// use the normalized path declared by their initialized parent. Consequently
/// the same relative filename in two repositories always has a distinct
/// `(RepoId, path)` identity.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RepoId(PathBuf);

impl RepoId {
    pub fn path(&self) -> &Path {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepoKind {
    WorkspaceRoot,
    /// Existing compatibility for opening a selected directory inside a
    /// repository. The worktree itself can be outside the workspace boundary.
    Containing,
    Nested,
    LinkedWorktree,
    Submodule,
    SubmodulePlaceholder,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GitLayout {
    GitDirectory,
    GitFile,
    Placeholder,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct RepoPath {
    pub repo_id: RepoId,
    pub relative: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoChange {
    pub path: RepoPath,
    pub original_path: Option<PathBuf>,
    pub status: FileStatus,
    pub submodule: SubmoduleStatus,
}

impl RepoChange {
    pub const fn submodule_pointer_changed(&self) -> bool {
        self.status.has_staged_change() || self.submodule.commit_changed
    }

    pub const fn submodule_worktree_dirty(&self) -> bool {
        self.submodule.modified_content || self.submodule.untracked_content
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RepoRelationState {
    OrdinaryNested {
        untracked_in_parent: bool,
        tracked_changes_in_parent: bool,
    },
    Submodule {
        initialized: bool,
        parent_change: Option<RepoChange>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoRelation {
    pub parent: RepoId,
    pub path_in_parent: PathBuf,
    pub state: RepoRelationState,
}

#[derive(Clone, Debug)]
pub struct RepoNode {
    pub id: RepoId,
    pub kind: RepoKind,
    pub layout: GitLayout,
    pub worktree: PathBuf,
    pub git_dir: Option<PathBuf>,
    /// Path below the selected workspace. `None` is used only for a containing
    /// repository whose worktree root is outside that boundary.
    pub workspace_relative: Option<PathBuf>,
    pub relation: Option<RepoRelation>,
    pub repo: Option<GitRepo>,
}

#[derive(Clone, Debug)]
pub struct RepoSnapshot {
    pub node: RepoNode,
    pub branch: Option<String>,
    /// Changes owned by this repository after nested-repository suppression.
    pub changes: Vec<RepoChange>,
    /// Parent entries hidden at ordinary nested-repository boundaries. These
    /// remain available for diagnostics and relationship markers.
    pub suppressed_parent_changes: Vec<RepoChange>,
    pub status_error: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DiscoveryTruncation {
    EntryLimit { limit: usize },
    RepositoryLimit { limit: usize },
    DepthLimit { limit: usize, path: PathBuf },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveryError {
    pub path: PathBuf,
    pub message: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DiscoveryReport {
    /// Number of directories considered for traversal.
    ///
    /// Ordinary files, symlinks, and `.git` markers are excluded.
    pub entries_scanned: usize,
    pub repositories_discovered: usize,
    pub truncations: Vec<DiscoveryTruncation>,
    pub errors: Vec<DiscoveryError>,
}

impl DiscoveryReport {
    pub fn is_truncated(&self) -> bool {
        !self.truncations.is_empty()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DiscoveryOptions {
    /// Maximum number of directories considered for traversal.
    ///
    /// Ordinary files, symlinks, and `.git` markers do not consume this budget.
    pub max_entries: usize,
    pub max_repositories: usize,
    pub max_depth: usize,
}

impl Default for DiscoveryOptions {
    fn default() -> Self {
        Self {
            max_entries: DEFAULT_MAX_DISCOVERY_ENTRIES,
            max_repositories: DEFAULT_MAX_REPOSITORIES,
            max_depth: DEFAULT_MAX_DISCOVERY_DEPTH,
        }
    }
}

#[derive(Clone, Debug)]
pub struct RepoGraph {
    workspace_root: PathBuf,
    repositories: Vec<RepoSnapshot>,
    report: DiscoveryReport,
    change_details: HashMap<RepoPath, ChangeDetails>,
}

impl RepoGraph {
    pub fn discover(workspace_root: &Path) -> Result<Self> {
        Self::discover_with_options(workspace_root, DiscoveryOptions::default())
    }

    pub fn discover_with_options(workspace_root: &Path, options: DiscoveryOptions) -> Result<Self> {
        let workspace_root = workspace_root
            .canonicalize()
            .with_context(|| format!("cannot open workspace {}", workspace_root.display()))?;
        if !workspace_root.is_dir() {
            bail!("{} is not a directory", workspace_root.display());
        }

        let mut discovery = Discovery::new(workspace_root, options);
        discovery.run()
    }

    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    pub fn repositories(&self) -> &[RepoSnapshot] {
        &self.repositories
    }

    pub fn report(&self) -> &DiscoveryReport {
        &self.report
    }

    pub fn repository(&self, id: &RepoId) -> Option<&RepoSnapshot> {
        self.repositories
            .iter()
            .find(|snapshot| &snapshot.node.id == id)
    }

    pub(crate) fn change_details(&self, path: &RepoPath) -> Option<&ChangeDetails> {
        self.change_details.get(path)
    }

    /// Count the semantic change rows projected by the repository-aware UI.
    ///
    /// A submodule status entry in its parent is relationship input, not a
    /// second file row. It becomes one pointer row only when the Gitlink
    /// changed; internal dirt remains represented by the child repository's
    /// own changes.
    pub fn projected_change_count(&self) -> usize {
        let parent_submodule_paths: HashSet<RepoPath> = self
            .repositories
            .iter()
            .filter_map(|snapshot| match &snapshot.node.relation.as_ref()?.state {
                RepoRelationState::Submodule {
                    parent_change: Some(change),
                    ..
                } => Some(change.path.clone()),
                _ => None,
            })
            .collect();
        let changes = self
            .repositories
            .iter()
            .flat_map(|snapshot| &snapshot.changes)
            .filter(|change| !parent_submodule_paths.contains(&change.path))
            .count();
        let pointers: HashSet<RepoPath> = self
            .repositories
            .iter()
            .filter_map(|snapshot| match &snapshot.node.relation.as_ref()?.state {
                RepoRelationState::Submodule {
                    parent_change: Some(change),
                    ..
                } if change.submodule_pointer_changed() => Some(change.path.clone()),
                _ => None,
            })
            .collect();
        changes + pointers.len()
    }

    /// Route a changed path through the Git worktree that owns its `RepoId`.
    pub fn diff_for(&self, path: &RepoPath) -> Result<Vec<String>> {
        let snapshot = self
            .repository(&path.repo_id)
            .ok_or_else(|| anyhow!("unknown repository {}", path.repo_id.path().display()))?;
        let repo = snapshot.node.repo.as_ref().ok_or_else(|| {
            anyhow!(
                "repository {} is an uninitialized submodule",
                snapshot.node.worktree.display()
            )
        })?;
        let change = snapshot
            .changes
            .iter()
            .chain(&snapshot.suppressed_parent_changes)
            .find(|change| change.path == *path);
        repo.diff_for_change(
            &path.relative,
            change.and_then(|change| change.original_path.as_deref()),
            change.map(|change| change.status),
        )
    }
}

#[derive(Clone, Debug)]
struct Candidate {
    id: RepoId,
    repo: GitRepo,
    canonical_git_dir: PathBuf,
    layout: GitLayout,
}

struct Discovery {
    workspace_root: PathBuf,
    options: DiscoveryOptions,
    report: DiscoveryReport,
    candidates: Vec<Candidate>,
    identities: HashSet<(PathBuf, PathBuf)>,
}

impl Discovery {
    fn new(workspace_root: PathBuf, options: DiscoveryOptions) -> Self {
        Self {
            workspace_root,
            options,
            report: DiscoveryReport::default(),
            candidates: Vec::new(),
            identities: HashSet::new(),
        }
    }

    fn run(&mut self) -> Result<RepoGraph> {
        self.discover_containing_repository()?;
        self.walk_workspace();
        self.build_graph()
    }

    fn discover_containing_repository(&mut self) -> Result<()> {
        let Some(repo) = GitRepo::discover(&self.workspace_root)? else {
            return Ok(());
        };
        let canonical_root = repo
            .root()
            .canonicalize()
            .with_context(|| format!("cannot resolve {}", repo.root().display()))?;
        if canonical_root != self.workspace_root {
            let layout = git_layout(&canonical_root).unwrap_or(GitLayout::GitDirectory);
            self.add_candidate(repo, canonical_root, layout);
        }
        Ok(())
    }

    fn walk_workspace(&mut self) {
        let mut queue = VecDeque::from([(self.workspace_root.clone(), 0_usize)]);
        let mut visited = HashSet::from([self.workspace_root.clone()]);
        let mut directory_limit_reached = false;

        while let Some((directory, depth)) = queue.pop_front() {
            if directory == self.workspace_root || has_git_marker(&directory) {
                match self.discover_exact_repository(&directory) {
                    Ok(Some((repo, canonical_root, layout))) => {
                        if !self.add_candidate(repo, canonical_root, layout) {
                            break;
                        }
                    }
                    Ok(None) => {
                        if has_git_marker(&directory) {
                            self.error(&directory, "Git marker is not a valid worktree");
                        }
                    }
                    Err(error) => self.error(&directory, format!("{error:#}")),
                }
            }

            // Directories queued before the cap are still inspected as
            // repository boundaries. Their contents are not enumerated, so
            // the directory cap cannot silently erase an already-observed repo.
            if directory_limit_reached {
                continue;
            }

            let children = match fs::read_dir(&directory) {
                Ok(entries) => entries,
                Err(error) => {
                    self.error(&directory, format!("cannot read directory: {error}"));
                    continue;
                }
            };
            // Membership at the global cap must not depend on filesystem
            // enumeration order. Keep only directories, then sort this one
            // already-opened level before consuming the remaining budget.
            // Ordinary files are never sorted or inspected beyond their type.
            let mut directories = Vec::new();
            for child in children {
                let child = match child {
                    Ok(child) => child,
                    Err(error) => {
                        self.error(&directory, format!("cannot read directory entry: {error}"));
                        continue;
                    }
                };
                if child.file_name() == ".git" {
                    continue;
                }
                let path = child.path();
                let file_type = match child.file_type() {
                    Ok(file_type) => file_type,
                    Err(error) => {
                        self.error(&path, format!("cannot inspect entry: {error}"));
                        continue;
                    }
                };
                if file_type.is_symlink() || !file_type.is_dir() {
                    continue;
                }
                directories.push(child);
            }
            directories.sort_by_key(|child| child.file_name());

            for child in directories {
                let path = child.path();
                if self.report.entries_scanned == self.options.max_entries {
                    self.push_truncation(DiscoveryTruncation::EntryLimit {
                        limit: self.options.max_entries,
                    });
                    directory_limit_reached = true;
                    break;
                }
                self.report.entries_scanned += 1;
                let canonical_path = match path.canonicalize() {
                    Ok(path) => path,
                    Err(error) => {
                        self.error(&path, format!("cannot resolve directory: {error}"));
                        continue;
                    }
                };
                if !canonical_path.starts_with(&self.workspace_root) {
                    self.error(&path, "refusing to traverse outside the selected workspace");
                    continue;
                }
                if !visited.insert(canonical_path.clone()) {
                    continue;
                }
                if depth == self.options.max_depth {
                    self.push_truncation(DiscoveryTruncation::DepthLimit {
                        limit: self.options.max_depth,
                        path: canonical_path,
                    });
                    continue;
                }
                queue.push_back((canonical_path, depth + 1));
            }
        }
    }

    fn discover_exact_repository(
        &self,
        directory: &Path,
    ) -> Result<Option<(GitRepo, PathBuf, GitLayout)>> {
        let Some(layout) = git_layout(directory) else {
            return Ok(None);
        };
        let Some(repo) = GitRepo::discover(directory)? else {
            return Ok(None);
        };
        let canonical_directory = directory
            .canonicalize()
            .with_context(|| format!("cannot resolve {}", directory.display()))?;
        let canonical_root = repo
            .root()
            .canonicalize()
            .with_context(|| format!("cannot resolve {}", repo.root().display()))?;
        if canonical_root != canonical_directory {
            return Ok(None);
        }
        Ok(Some((repo, canonical_root, layout)))
    }

    fn add_candidate(&mut self, repo: GitRepo, canonical_root: PathBuf, layout: GitLayout) -> bool {
        let canonical_git_dir = match repo.git_dir().canonicalize() {
            Ok(git_dir) => git_dir,
            Err(error) => {
                self.error(
                    repo.git_dir(),
                    format!("cannot resolve Git directory: {error}"),
                );
                return true;
            }
        };
        let identity = (canonical_root.clone(), canonical_git_dir.clone());
        if !self.identities.insert(identity) {
            return true;
        }
        if self.candidates.len() == self.options.max_repositories {
            self.push_truncation(DiscoveryTruncation::RepositoryLimit {
                limit: self.options.max_repositories,
            });
            return false;
        }
        self.candidates.push(Candidate {
            id: RepoId(canonical_root),
            repo,
            canonical_git_dir,
            layout,
        });
        true
    }

    fn build_graph(&mut self) -> Result<RepoGraph> {
        self.candidates
            .sort_by(|left, right| left.id.cmp(&right.id));
        let mut nodes: Vec<RepoNode> = self
            .candidates
            .iter()
            .map(|candidate| RepoNode {
                id: candidate.id.clone(),
                kind: if candidate.id.path() == self.workspace_root {
                    RepoKind::WorkspaceRoot
                } else if candidate.id.path().starts_with(&self.workspace_root) {
                    match candidate.layout {
                        GitLayout::GitFile => RepoKind::LinkedWorktree,
                        GitLayout::GitDirectory => RepoKind::Nested,
                        GitLayout::Placeholder => unreachable!(),
                    }
                } else {
                    RepoKind::Containing
                },
                layout: candidate.layout,
                worktree: candidate.id.0.clone(),
                git_dir: Some(candidate.canonical_git_dir.clone()),
                workspace_relative: candidate
                    .id
                    .path()
                    .strip_prefix(&self.workspace_root)
                    .ok()
                    .map(Path::to_path_buf),
                relation: None,
                repo: Some(candidate.repo.clone()),
            })
            .collect();

        let mut submodule_links: HashMap<RepoId, (RepoId, PathBuf)> = HashMap::new();
        for candidate in self.candidates.clone() {
            let paths = match candidate.repo.submodule_paths() {
                Ok(paths) => paths,
                Err(error) => {
                    self.error(
                        candidate.id.path(),
                        format!("cannot read submodules: {error:#}"),
                    );
                    continue;
                }
            };
            for path in paths {
                if !safe_relative_path(&path) {
                    self.error(
                        candidate.id.path().join(&path),
                        "submodule path is not a safe relative path",
                    );
                    continue;
                }
                let declared = candidate.id.path().join(&path);
                let metadata = fs::symlink_metadata(&declared).ok();
                if metadata
                    .as_ref()
                    .is_some_and(|metadata| metadata.file_type().is_symlink())
                {
                    self.error(&declared, "refusing to follow a submodule symlink");
                    continue;
                }
                let identity_path = if metadata.is_some() {
                    match declared.canonicalize() {
                        Ok(path) => path,
                        Err(error) => {
                            self.error(
                                &declared,
                                format!("cannot resolve submodule path: {error}"),
                            );
                            continue;
                        }
                    }
                } else {
                    declared
                };
                if !identity_path.starts_with(&self.workspace_root) {
                    self.error(
                        &identity_path,
                        "submodule path escapes the selected workspace",
                    );
                    continue;
                }

                let existing = nodes
                    .iter()
                    .position(|node| node.worktree == identity_path && node.repo.is_some());
                let child_id = if let Some(index) = existing {
                    nodes[index].kind = RepoKind::Submodule;
                    nodes[index].id.clone()
                } else {
                    if nodes.len() == self.options.max_repositories {
                        self.push_truncation(DiscoveryTruncation::RepositoryLimit {
                            limit: self.options.max_repositories,
                        });
                        continue;
                    }
                    let id = RepoId(identity_path.clone());
                    nodes.push(RepoNode {
                        id: id.clone(),
                        kind: RepoKind::SubmodulePlaceholder,
                        layout: GitLayout::Placeholder,
                        worktree: identity_path.clone(),
                        git_dir: None,
                        workspace_relative: identity_path
                            .strip_prefix(&self.workspace_root)
                            .ok()
                            .map(Path::to_path_buf),
                        relation: None,
                        repo: None,
                    });
                    id
                };
                match submodule_links.entry(child_id.clone()) {
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        entry.insert((candidate.id.clone(), path));
                    }
                    std::collections::hash_map::Entry::Occupied(entry)
                        if entry.get().0 != candidate.id =>
                    {
                        self.error(
                            child_id.path(),
                            "repository is declared as a submodule by multiple parents",
                        );
                    }
                    std::collections::hash_map::Entry::Occupied(_) => {}
                }
            }
        }

        nodes.sort_by(|left, right| left.id.cmp(&right.id));
        let initialized_ids: Vec<(RepoId, PathBuf)> = nodes
            .iter()
            .filter(|node| node.repo.is_some())
            .map(|node| (node.id.clone(), node.worktree.clone()))
            .collect();
        for node in &mut nodes {
            if let Some((parent, path)) = submodule_links.get(&node.id) {
                node.kind = if node.repo.is_some() {
                    RepoKind::Submodule
                } else {
                    RepoKind::SubmodulePlaceholder
                };
                node.relation = Some(RepoRelation {
                    parent: parent.clone(),
                    path_in_parent: path.clone(),
                    state: RepoRelationState::Submodule {
                        initialized: node.repo.is_some(),
                        parent_change: None,
                    },
                });
                continue;
            }
            let parent = initialized_ids
                .iter()
                .filter(|(_, root)| root != &node.worktree && node.worktree.starts_with(root))
                .max_by_key(|(_, root)| root.components().count());
            if let Some((parent_id, parent_root)) = parent {
                let path_in_parent = node
                    .worktree
                    .strip_prefix(parent_root)
                    .expect("ancestor selected above")
                    .to_path_buf();
                node.relation = Some(RepoRelation {
                    parent: parent_id.clone(),
                    path_in_parent,
                    state: RepoRelationState::OrdinaryNested {
                        untracked_in_parent: false,
                        tracked_changes_in_parent: false,
                    },
                });
            }
        }

        let builds: Vec<SnapshotBuild> = nodes.into_iter().map(snapshot).collect();
        let mut change_details = HashMap::new();
        let mut snapshots = Vec::with_capacity(builds.len());
        for build in builds {
            change_details.extend(build.change_details);
            snapshots.push(build.snapshot);
        }
        for snapshot in &mut snapshots {
            if snapshot.node.kind == RepoKind::Containing {
                let prefix = self
                    .workspace_root
                    .strip_prefix(&snapshot.node.worktree)
                    .expect("containing repository is an ancestor of the workspace");
                snapshot
                    .changes
                    .retain(|change| change.path.relative.starts_with(prefix));
            }
        }
        let visible_status_paths: HashSet<RepoPath> = snapshots
            .iter()
            .flat_map(|snapshot| snapshot.changes.iter().map(|change| change.path.clone()))
            .collect();
        change_details.retain(|path, _| visible_status_paths.contains(path));
        apply_relationship_status(&mut snapshots);
        snapshots.sort_by(|left, right| left.node.id.cmp(&right.node.id));
        self.report.repositories_discovered = snapshots.len();

        Ok(RepoGraph {
            workspace_root: self.workspace_root.clone(),
            repositories: snapshots,
            report: self.report.clone(),
            change_details,
        })
    }

    fn error(&mut self, path: impl AsRef<Path>, message: impl Into<String>) {
        self.report.errors.push(DiscoveryError {
            path: path.as_ref().to_path_buf(),
            message: message.into(),
        });
    }

    fn push_truncation(&mut self, truncation: DiscoveryTruncation) {
        if !self.report.truncations.contains(&truncation) {
            self.report.truncations.push(truncation);
        }
    }
}

struct SnapshotBuild {
    snapshot: RepoSnapshot,
    change_details: HashMap<RepoPath, ChangeDetails>,
}

fn snapshot(node: RepoNode) -> SnapshotBuild {
    let Some(repo) = node.repo.as_ref() else {
        return SnapshotBuild {
            snapshot: RepoSnapshot {
                node,
                branch: None,
                changes: Vec::new(),
                suppressed_parent_changes: Vec::new(),
                status_error: None,
            },
            change_details: HashMap::new(),
        };
    };

    let branch = repo.branch().ok();
    match repo.status_snapshots() {
        Ok(entries) => {
            let change_details = repo
                .change_details(&entries)
                .into_iter()
                .map(|(relative, details)| {
                    (
                        RepoPath {
                            repo_id: node.id.clone(),
                            relative,
                        },
                        details,
                    )
                })
                .collect();
            let mut changes: Vec<RepoChange> = entries
                .into_iter()
                .map(|snapshot| repo_change(&node.id, snapshot.entry))
                .collect();
            changes.sort_by(|left, right| left.path.relative.cmp(&right.path.relative));
            SnapshotBuild {
                snapshot: RepoSnapshot {
                    node,
                    branch,
                    changes,
                    suppressed_parent_changes: Vec::new(),
                    status_error: None,
                },
                change_details,
            }
        }
        Err(error) => SnapshotBuild {
            snapshot: RepoSnapshot {
                node,
                branch,
                changes: Vec::new(),
                suppressed_parent_changes: Vec::new(),
                status_error: Some(format!("{error:#}")),
            },
            change_details: HashMap::new(),
        },
    }
}

fn repo_change(id: &RepoId, entry: GitStatusEntry) -> RepoChange {
    RepoChange {
        path: RepoPath {
            repo_id: id.clone(),
            relative: entry.path,
        },
        original_path: entry.original_path,
        status: entry.status,
        submodule: entry.submodule,
    }
}

fn apply_relationship_status(snapshots: &mut [RepoSnapshot]) {
    let positions: HashMap<RepoId, usize> = snapshots
        .iter()
        .enumerate()
        .map(|(index, snapshot)| (snapshot.node.id.clone(), index))
        .collect();

    for child_index in 0..snapshots.len() {
        let Some(relation) = snapshots[child_index].node.relation.clone() else {
            continue;
        };
        let Some(&parent_index) = positions.get(&relation.parent) else {
            continue;
        };
        match relation.state {
            RepoRelationState::OrdinaryNested { .. } => {
                let mut retained = Vec::new();
                let mut suppressed = Vec::new();
                for change in std::mem::take(&mut snapshots[parent_index].changes) {
                    if change.path.relative.starts_with(&relation.path_in_parent) {
                        suppressed.push(change);
                    } else {
                        retained.push(change);
                    }
                }
                let untracked = suppressed.iter().any(|change| change.status.is_untracked());
                let tracked = suppressed
                    .iter()
                    .any(|change| !change.status.is_untracked());
                snapshots[parent_index].changes = retained;
                snapshots[parent_index]
                    .suppressed_parent_changes
                    .extend(suppressed);
                snapshots[child_index].node.relation = Some(RepoRelation {
                    state: RepoRelationState::OrdinaryNested {
                        untracked_in_parent: untracked,
                        tracked_changes_in_parent: tracked,
                    },
                    ..relation
                });
            }
            RepoRelationState::Submodule { initialized, .. } => {
                let parent_change = snapshots[parent_index]
                    .changes
                    .iter()
                    .find(|change| change.path.relative == relation.path_in_parent)
                    .cloned();
                snapshots[child_index].node.relation = Some(RepoRelation {
                    state: RepoRelationState::Submodule {
                        initialized,
                        parent_change,
                    },
                    ..relation
                });
            }
        }
    }
}

fn has_git_marker(directory: &Path) -> bool {
    fs::symlink_metadata(directory.join(".git"))
        .map(|metadata| !metadata.file_type().is_symlink())
        .unwrap_or(false)
}

fn git_layout(directory: &Path) -> Option<GitLayout> {
    let metadata = fs::symlink_metadata(directory.join(".git")).ok()?;
    if metadata.file_type().is_symlink() {
        None
    } else if metadata.is_dir() {
        Some(GitLayout::GitDirectory)
    } else if metadata.is_file() {
        Some(GitLayout::GitFile)
    } else {
        None
    }
}

fn safe_relative_path(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsafe_submodule_paths_are_rejected() {
        assert!(safe_relative_path(Path::new("modules/child")));
        assert!(!safe_relative_path(Path::new("../escape")));
        assert!(!safe_relative_path(Path::new("./child")));
        assert!(!safe_relative_path(Path::new("")));
    }

    #[test]
    fn report_exposes_every_truncation() {
        let report = DiscoveryReport {
            truncations: vec![DiscoveryTruncation::EntryLimit { limit: 2 }],
            ..DiscoveryReport::default()
        };
        assert!(report.is_truncated());
    }
}
