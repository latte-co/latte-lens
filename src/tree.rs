use std::{
    cmp::Ordering,
    collections::{HashSet, VecDeque},
    fs,
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result};

use crate::{
    content_safety::{path_exists_without_following, resolves_to_directory, symlink_target},
    git::{FileStatus, StatusMap},
};

pub const DEFAULT_MAX_ENTRIES: usize = 50_000;
/// Number of path components loaded below the selected workspace at startup.
///
/// Deeper directories remain visible as boundaries and are enumerated only
/// when the user expands them.
pub const DEFAULT_INITIAL_SCAN_DEPTH: usize = 2;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileEntry {
    pub relative: PathBuf,
    pub is_dir: bool,
    pub depth: usize,
    pub status: Option<FileStatus>,
    pub contains_changes: bool,
    pub exists: bool,
    /// Raw target text when this entry is a symbolic link, otherwise `None`.
    ///
    /// The target is read without following the link, so it mirrors exactly
    /// what `ln -s` recorded. `is_dir` still reflects the resolved kind, so a
    /// directory symlink stays expandable while surfacing its link nature here.
    pub symlink_target: Option<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScanResult {
    pub entries: Vec<FileEntry>,
    /// True when traversal discovered another entry after reaching its cap.
    pub truncated: bool,
    /// Directories whose own entries have not been enumerated yet.
    pub unloaded_directories: HashSet<PathBuf>,
}

impl FileEntry {
    pub fn name(&self) -> String {
        self.relative
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.relative.display().to_string())
    }
}

pub fn scan(root: &Path, statuses: &StatusMap) -> Result<ScanResult> {
    scan_with_limit(root, statuses, DEFAULT_MAX_ENTRIES)
}

/// Scan at most `max_entries` filesystem entries below `root`.
///
/// Git status paths are synthesized after traversal so changed and deleted
/// paths remain visible. As a result, `entries` can be longer than the limit;
/// `truncated` specifically describes the bounded filesystem traversal.
pub fn scan_with_limit(
    root: &Path,
    statuses: &StatusMap,
    max_entries: usize,
) -> Result<ScanResult> {
    scan_with_depth(root, statuses, max_entries, usize::MAX)
}

/// Scan at most `max_depth` path components below `root`.
///
/// A depth of two includes `one/two`, but does not enumerate entries below a
/// directory at that depth. Such directories are returned in
/// `unloaded_directories` for on-demand loading.
pub fn scan_with_depth(
    root: &Path,
    statuses: &StatusMap,
    max_entries: usize,
    max_depth: usize,
) -> Result<ScanResult> {
    let mut seen = HashSet::new();
    let mut entries = Vec::new();
    let mut truncated = false;
    let mut unloaded_directories = HashSet::new();
    let mut directories = VecDeque::from([(root.to_path_buf(), 0_usize)]);

    'scan: while let Some((directory, directory_depth)) = directories.pop_front() {
        if directory_depth == max_depth {
            if let Ok(relative) = directory.strip_prefix(root)
                && !relative.as_os_str().is_empty()
            {
                unloaded_directories.insert(relative.to_path_buf());
            }
            continue;
        }
        let mut children = fs::read_dir(&directory)
            .with_context(|| format!("failed to scan {}", directory.display()))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .with_context(|| format!("failed to scan {}", directory.display()))?;
        children.sort_by_key(fs::DirEntry::file_name);

        for child in children {
            // All Files is a filesystem view, so dotfiles and ignored paths
            // stay visible. Only Git's internal metadata is outside this scope.
            if child.file_name() == ".git" {
                continue;
            }

            // Checking before insertion uses the next discovered path as proof
            // that an exactly-full result is partial. A tree with exactly the
            // cap and no more descendants remains complete.
            if entries.len() == max_entries {
                truncated = true;
                if let Ok(relative) = directory.strip_prefix(root)
                    && !relative.as_os_str().is_empty()
                {
                    unloaded_directories.insert(relative.to_path_buf());
                }
                break 'scan;
            }

            let file_type = child.file_type().with_context(|| {
                format!(
                    "failed to inspect filesystem entry {}",
                    child.path().display()
                )
            })?;
            let path = child.path();
            // A directory symlink is expandable like a real directory, but it
            // is never auto-recursed during the breadth-first scan: following
            // it eagerly could walk a cycle. It is enumerated lazily on expand.
            let is_symlinked_dir = file_type.is_symlink() && resolves_to_directory(&path);
            let is_dir = file_type.is_dir() || is_symlinked_dir;
            let link_target = file_type
                .is_symlink()
                .then(|| fs::read_link(&path).ok())
                .flatten();
            let relative = path
                .strip_prefix(root)
                .expect("scanned paths stay below the root")
                .to_path_buf();
            if is_dir {
                let child_depth = directory_depth.saturating_add(1);
                if is_symlinked_dir || child_depth == max_depth {
                    unloaded_directories.insert(relative.clone());
                } else {
                    directories.push_back((path, child_depth));
                }
            }
            seen.insert(relative.clone());
            entries.push(make_entry(relative, is_dir, true, link_target, statuses));
        }
    }

    if truncated {
        unloaded_directories.extend(directories.into_iter().filter_map(|(directory, _)| {
            directory
                .strip_prefix(root)
                .ok()
                .filter(|relative| !relative.as_os_str().is_empty())
                .map(Path::to_path_buf)
        }));
    }

    // Deleted and renamed paths may no longer exist, but still belong in the tree.
    // Synthesize missing parents as well so the changed-files tree keeps its
    // directory hierarchy even when the whole path was deleted or ignored.
    for path in statuses.keys() {
        let mut parent = path.parent().map(Path::to_path_buf);
        while let Some(directory) = parent
            .take()
            .filter(|directory| !directory.as_os_str().is_empty())
        {
            let directory_depth = directory.components().count();
            if directory_depth <= max_depth && seen.insert(directory.clone()) {
                let exists = fs::symlink_metadata(root.join(&directory))
                    .is_ok_and(|metadata| metadata.is_dir());
                if exists && directory_depth == max_depth {
                    unloaded_directories.insert(directory.clone());
                }
                entries.push(make_entry(directory.clone(), true, exists, None, statuses));
            }
            parent = directory.parent().map(Path::to_path_buf);
        }
        if path.components().count() <= max_depth && !seen.contains(path) {
            let absolute = root.join(path);
            entries.push(make_entry(
                path.clone(),
                false,
                path_exists_without_following(&absolute),
                symlink_target(&absolute),
                statuses,
            ));
        }
    }

    entries.sort_by(|left, right| {
        compare_tree_paths(&left.relative, left.is_dir, &right.relative, right.is_dir)
    });
    Ok(ScanResult {
        entries,
        truncated,
        unloaded_directories,
    })
}

/// Enumerate one directory for lazy tree expansion without walking into its
/// child directories.
pub fn scan_directory(
    root: &Path,
    relative: &Path,
    statuses: &StatusMap,
    max_entries: usize,
) -> Result<ScanResult> {
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        anyhow::bail!(
            "directory path {} escapes the scan root",
            relative.display()
        );
    }
    let directory = root.join(relative);
    let mut children = fs::read_dir(&directory)
        .with_context(|| format!("failed to scan {}", directory.display()))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("failed to scan {}", directory.display()))?;
    children.sort_by_key(fs::DirEntry::file_name);

    let mut entries = Vec::new();
    let mut unloaded_directories = HashSet::new();
    let mut truncated = false;
    for child in children {
        if child.file_name() == ".git" {
            continue;
        }
        if entries.len() == max_entries {
            truncated = true;
            break;
        }
        let file_type = child.file_type().with_context(|| {
            format!(
                "failed to inspect filesystem entry {}",
                child.path().display()
            )
        })?;
        let path = child.path();
        let is_dir = file_type.is_dir() || (file_type.is_symlink() && resolves_to_directory(&path));
        let link_target = file_type
            .is_symlink()
            .then(|| fs::read_link(&path).ok())
            .flatten();
        let child_relative = path
            .strip_prefix(root)
            .expect("lazily scanned paths stay below the root")
            .to_path_buf();
        if is_dir {
            unloaded_directories.insert(child_relative.clone());
        }
        entries.push(make_entry(
            child_relative,
            is_dir,
            true,
            link_target,
            statuses,
        ));
    }
    entries.sort_by(|left, right| {
        compare_tree_paths(&left.relative, left.is_dir, &right.relative, right.is_dir)
    });
    Ok(ScanResult {
        entries,
        truncated,
        unloaded_directories,
    })
}

/// Compare flattened tree paths in display order.
///
/// At every hierarchy level, directory siblings sort before file siblings;
/// names within each group retain the platform's deterministic path ordering.
/// Descendants stay directly after their parent instead of being grouped with
/// unrelated directories elsewhere in the tree.
pub(crate) fn compare_tree_paths(
    left: &Path,
    left_is_dir: bool,
    right: &Path,
    right_is_dir: bool,
) -> Ordering {
    let mut left_components = left.components().peekable();
    let mut right_components = right.components().peekable();

    loop {
        match (left_components.next(), right_components.next()) {
            (Some(left_component), Some(right_component)) if left_component == right_component => {}
            (Some(left_component), Some(right_component)) => {
                let left_component_is_dir = left_components.peek().is_some() || left_is_dir;
                let right_component_is_dir = right_components.peek().is_some() || right_is_dir;
                return right_component_is_dir
                    .cmp(&left_component_is_dir)
                    .then_with(|| left_component.as_os_str().cmp(right_component.as_os_str()));
            }
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (None, None) => return right_is_dir.cmp(&left_is_dir),
        }
    }
}

/// Return only changed files and the ancestor directories needed to place them.
pub fn changed_only(entries: &[FileEntry]) -> Vec<FileEntry> {
    entries
        .iter()
        .filter(|entry| entry.status.is_some() || (entry.is_dir && entry.contains_changes))
        .cloned()
        .collect()
}

fn make_entry(
    relative: PathBuf,
    is_dir: bool,
    exists: bool,
    symlink_target: Option<PathBuf>,
    statuses: &StatusMap,
) -> FileEntry {
    let status = statuses.get(&relative).copied();
    let contains_changes = status.is_some()
        || (is_dir
            && statuses
                .keys()
                .any(|changed_path| changed_path.starts_with(&relative)));
    let depth = relative.components().count().saturating_sub(1);

    FileEntry {
        relative,
        is_dir,
        depth,
        status,
        contains_changes,
        exists,
        symlink_target,
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, fs};

    use super::*;

    fn relative_paths(scan: &ScanResult) -> Vec<PathBuf> {
        scan.entries
            .iter()
            .map(|entry| entry.relative.clone())
            .collect()
    }

    fn fixture_with_files_in_order(names: &[&str]) -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        for name in names {
            fs::write(directory.path().join(name), name).unwrap();
        }
        directory
    }

    #[test]
    fn limited_scan_retains_deterministic_subset_across_creation_orders() {
        let first = fixture_with_files_in_order(&[
            "zeta.txt",
            "gamma.txt",
            "alpha.txt",
            "epsilon.txt",
            "beta.txt",
        ]);
        let second = fixture_with_files_in_order(&[
            "beta.txt",
            "epsilon.txt",
            "alpha.txt",
            "gamma.txt",
            "zeta.txt",
        ]);

        let first_scan = scan_with_limit(first.path(), &HashMap::new(), 3).unwrap();
        let second_scan = scan_with_limit(second.path(), &HashMap::new(), 3).unwrap();
        let expected = vec![
            PathBuf::from("alpha.txt"),
            PathBuf::from("beta.txt"),
            PathBuf::from("epsilon.txt"),
        ];

        assert_eq!(relative_paths(&first_scan), expected);
        assert_eq!(relative_paths(&second_scan), expected);
        assert!(first_scan.truncated);
        assert!(second_scan.truncated);
    }

    #[test]
    fn limited_scan_keeps_the_root_directory_shape_before_deep_descendants() {
        let directory = tempfile::tempdir().unwrap();
        for path in [
            "a-heavy/one.txt",
            "a-heavy/two.txt",
            "z-late/child.txt",
            "root-last.txt",
        ] {
            let path = directory.path().join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "fixture").unwrap();
        }

        let scan = scan_with_limit(directory.path(), &HashMap::new(), 3).unwrap();

        assert_eq!(
            relative_paths(&scan),
            [
                PathBuf::from("a-heavy"),
                PathBuf::from("z-late"),
                PathBuf::from("root-last.txt"),
            ]
        );
        assert!(scan.truncated);
        assert_eq!(
            scan.unloaded_directories,
            [PathBuf::from("a-heavy"), PathBuf::from("z-late")]
                .into_iter()
                .collect()
        );
    }

    #[test]
    fn tree_order_places_directories_before_files_at_every_level() {
        let directory = tempfile::tempdir().unwrap();
        for path in [
            "z-root.txt",
            "a-root.txt",
            "z-dir/middle.txt",
            "a-dir/z-child.txt",
            "a-dir/a-child.txt",
            "a-dir/b-dir/inner.txt",
        ] {
            let path = directory.path().join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "fixture").unwrap();
        }

        let scan = scan(directory.path(), &HashMap::new()).unwrap();
        assert_eq!(
            relative_paths(&scan),
            [
                PathBuf::from("a-dir"),
                PathBuf::from("a-dir/b-dir"),
                PathBuf::from("a-dir/b-dir/inner.txt"),
                PathBuf::from("a-dir/a-child.txt"),
                PathBuf::from("a-dir/z-child.txt"),
                PathBuf::from("z-dir"),
                PathBuf::from("z-dir/middle.txt"),
                PathBuf::from("a-root.txt"),
                PathBuf::from("z-root.txt"),
            ]
        );
    }

    #[test]
    fn truncation_requires_an_entry_beyond_the_limit() {
        let directory = tempfile::tempdir().unwrap();
        for name in ["a.txt", "b.txt", "c.txt"] {
            fs::write(directory.path().join(name), name).unwrap();
        }

        let partial = scan_with_limit(directory.path(), &HashMap::new(), 2).unwrap();
        assert_eq!(
            relative_paths(&partial),
            [PathBuf::from("a.txt"), PathBuf::from("b.txt")]
        );
        assert!(partial.truncated);

        let exact = scan_with_limit(directory.path(), &HashMap::new(), 3).unwrap();
        assert_eq!(
            relative_paths(&exact),
            [
                PathBuf::from("a.txt"),
                PathBuf::from("b.txt"),
                PathBuf::from("c.txt")
            ]
        );
        assert!(!exact.truncated);
    }

    #[cfg(unix)]
    #[test]
    fn directory_symlink_is_expandable_but_not_auto_recursed() {
        use std::os::unix::fs::symlink;

        let outer = tempfile::tempdir().unwrap();
        let real_dir = outer.path().join("real-dir");
        fs::create_dir(&real_dir).unwrap();
        fs::write(real_dir.join("inner.txt"), "inner").unwrap();
        let workspace = outer.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        symlink(&real_dir, workspace.join("linked-dir")).unwrap();

        let scan = scan(&workspace, &HashMap::new()).unwrap();

        // The link is an expandable directory row, but its children are not
        // enumerated during the initial breadth scan (cycle safety); it is
        // returned as an unloaded directory for lazy expansion.
        let link = scan
            .entries
            .iter()
            .find(|entry| entry.relative == Path::new("linked-dir"))
            .expect("directory symlink is present");
        assert!(link.is_dir);
        assert_eq!(link.symlink_target.as_deref(), Some(real_dir.as_path()));
        assert!(
            !relative_paths(&scan).contains(&PathBuf::from("linked-dir/inner.txt")),
            "a directory symlink is not auto-recursed"
        );
        assert!(
            scan.unloaded_directories
                .contains(&PathBuf::from("linked-dir"))
        );

        // Lazy expansion enumerates the link target's entries on demand.
        let expanded =
            scan_directory(&workspace, Path::new("linked-dir"), &HashMap::new(), 1_000).unwrap();
        assert!(relative_paths(&expanded).contains(&PathBuf::from("linked-dir/inner.txt")));
    }

    #[cfg(unix)]
    #[test]
    fn scan_records_raw_symlink_targets_for_files_and_directories() {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("real.txt"), "content").unwrap();
        // Relative link text is recorded exactly as written, not resolved.
        symlink("real.txt", workspace.path().join("a-file-link.txt")).unwrap();
        symlink(
            "/tmp/somewhere-external",
            workspace.path().join("b-dir-link"),
        )
        .unwrap();

        let scan = scan(workspace.path(), &HashMap::new()).unwrap();

        let file_link = scan
            .entries
            .iter()
            .find(|entry| entry.relative == Path::new("a-file-link.txt"))
            .expect("file symlink present");
        assert_eq!(
            file_link.symlink_target.as_deref(),
            Some(Path::new("real.txt"))
        );
        assert!(!file_link.is_dir);

        // A plain regular file has no target recorded.
        let plain = scan
            .entries
            .iter()
            .find(|entry| entry.relative == Path::new("real.txt"))
            .expect("regular file present");
        assert_eq!(plain.symlink_target, None);
    }

    #[cfg(unix)]
    #[test]
    fn self_referential_directory_symlink_does_not_loop_the_scan() {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().unwrap();
        // A link that points at its own parent would loop if auto-recursed.
        symlink(workspace.path(), workspace.path().join("loop")).unwrap();
        fs::write(workspace.path().join("file.txt"), "ok").unwrap();

        let scan = scan(workspace.path(), &HashMap::new()).unwrap();

        let link = scan
            .entries
            .iter()
            .find(|entry| entry.relative == Path::new("loop"))
            .expect("self-referential link is present");
        assert!(link.is_dir);
        assert!(scan.unloaded_directories.contains(&PathBuf::from("loop")));
        assert!(relative_paths(&scan).contains(&PathBuf::from("file.txt")));
    }

    #[cfg(unix)]
    #[test]
    fn broken_directory_symlink_is_a_leaf_not_a_directory() {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().unwrap();
        symlink(
            workspace.path().join("does-not-exist"),
            workspace.path().join("dangling"),
        )
        .unwrap();

        let scan = scan(workspace.path(), &HashMap::new()).unwrap();

        let link = scan
            .entries
            .iter()
            .find(|entry| entry.relative == Path::new("dangling"))
            .expect("broken link is present");
        assert!(
            !link.is_dir,
            "a link that resolves to nothing is not expandable"
        );
    }
}
