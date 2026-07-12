use std::{
    cmp::Ordering,
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use ignore::WalkBuilder;

use crate::{
    content_safety::path_exists_without_following,
    git::{FileStatus, StatusMap},
};

pub const DEFAULT_MAX_ENTRIES: usize = 50_000;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileEntry {
    pub relative: PathBuf,
    pub is_dir: bool,
    pub depth: usize,
    pub status: Option<FileStatus>,
    pub contains_changes: bool,
    pub exists: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScanResult {
    pub entries: Vec<FileEntry>,
    /// True when traversal discovered another entry after reaching its cap.
    pub truncated: bool,
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
    let mut builder = WalkBuilder::new(root);
    builder
        // All Files is a filesystem view, so dotfiles and ignored paths stay
        // visible. Only Git's internal metadata is outside this scope.
        .standard_filters(false)
        .sort_by_file_path(|left, right| left.cmp(right))
        .filter_entry(|entry| entry.file_name() != ".git");

    let mut seen = HashSet::new();
    let mut entries = Vec::new();
    let mut truncated = false;
    for result in builder.build() {
        let entry = result.with_context(|| format!("failed to scan {}", root.display()))?;
        if entry.path() == root {
            continue;
        }

        // Checking before insertion uses the next discovered path as proof
        // that an exactly-full result is partial. A tree with exactly the cap
        // remains complete.
        if entries.len() == max_entries {
            truncated = true;
            break;
        }

        let relative = entry
            .path()
            .strip_prefix(root)
            .expect("walked paths stay below the root")
            .to_path_buf();
        let is_dir = entry.file_type().is_some_and(|kind| kind.is_dir());
        seen.insert(relative.clone());
        entries.push(make_entry(relative, is_dir, true, statuses));
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
            if seen.insert(directory.clone()) {
                entries.push(make_entry(
                    directory.clone(),
                    true,
                    fs::symlink_metadata(root.join(&directory))
                        .is_ok_and(|metadata| metadata.is_dir()),
                    statuses,
                ));
            }
            parent = directory.parent().map(Path::to_path_buf);
        }
        if !seen.contains(path) {
            entries.push(make_entry(
                path.clone(),
                false,
                path_exists_without_following(&root.join(path)),
                statuses,
            ));
        }
    }

    entries.sort_by(|left, right| {
        compare_tree_paths(&left.relative, left.is_dir, &right.relative, right.is_dir)
    });
    Ok(ScanResult { entries, truncated })
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

fn make_entry(relative: PathBuf, is_dir: bool, exists: bool, statuses: &StatusMap) -> FileEntry {
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

    #[test]
    fn zero_limit_distinguishes_empty_and_non_empty_roots() {
        let empty = tempfile::tempdir().unwrap();
        let complete = scan_with_limit(empty.path(), &HashMap::new(), 0).unwrap();
        assert!(complete.entries.is_empty());
        assert!(!complete.truncated);

        fs::write(empty.path().join("file.txt"), "fixture").unwrap();
        let partial = scan_with_limit(empty.path(), &HashMap::new(), 0).unwrap();
        assert!(partial.entries.is_empty());
        assert!(partial.truncated);
    }
}
