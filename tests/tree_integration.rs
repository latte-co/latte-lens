mod support;

use std::{collections::HashMap, path::Path};

use latte_lens::{
    git::FileStatus,
    tree::{FileEntry, changed_only, scan, scan_with_limit},
};
use support::TestRepo;

#[test]
fn scan_honors_gitignore_keeps_hidden_files_and_excludes_git_metadata() {
    let fixture = TestRepo::new();
    fixture.write(".gitignore", "ignored/\n*.log\n");
    fixture.write("src/main.rs", "fn main() {}\n");
    fixture.write("ignored/private.txt", "secret\n");
    fixture.write("debug.log", "noise\n");
    fixture.write(".visible-to-lens", "hidden by shell, not by lens\n");

    let scan = scan(fixture.root(), &HashMap::new()).unwrap();
    assert!(!scan.truncated);
    let entries = scan.entries;
    let paths: Vec<_> = entries
        .iter()
        .map(|entry| entry.relative.as_path())
        .collect();

    assert!(paths.contains(&Path::new("src")));
    assert!(paths.contains(&Path::new("src/main.rs")));
    assert!(paths.contains(&Path::new(".visible-to-lens")));
    assert!(!paths.iter().any(|path| path.starts_with(".git")));
    assert!(!paths.iter().any(|path| path.starts_with("ignored")));
    assert!(!paths.contains(&Path::new("debug.log")));
}

#[test]
fn scan_propagates_change_state_and_keeps_deleted_paths() {
    let fixture = TestRepo::new();
    fixture.write("src/live.rs", "present\n");
    let statuses = HashMap::from([
        (
            Path::new("src/live.rs").to_path_buf(),
            FileStatus {
                index: ' ',
                worktree: 'M',
            },
        ),
        (
            Path::new("src/deleted.rs").to_path_buf(),
            FileStatus {
                index: ' ',
                worktree: 'D',
            },
        ),
    ]);

    let entries = scan(fixture.root(), &statuses).unwrap().entries;
    let directory = entry(&entries, "src");
    let live = entry(&entries, "src/live.rs");
    let deleted = entry(&entries, "src/deleted.rs");

    assert!(directory.is_dir);
    assert!(directory.contains_changes);
    assert_eq!(live.depth, 1);
    assert_eq!(live.status.unwrap().label(), "M");
    assert!(!deleted.exists);
    assert_eq!(deleted.status.unwrap().label(), "D");
}

#[test]
fn limited_scan_reports_partial_results_and_keeps_status_paths_visible() {
    let fixture = TestRepo::new();
    fixture.write("changed.txt", "present\n");
    let statuses = HashMap::from([(
        Path::new("changed.txt").to_path_buf(),
        FileStatus {
            index: ' ',
            worktree: 'M',
        },
    )]);

    let scan = scan_with_limit(fixture.root(), &statuses, 0).unwrap();

    assert!(scan.truncated);
    assert_eq!(scan.entries.len(), 1);
    assert_eq!(scan.entries[0].relative, Path::new("changed.txt"));
    assert!(scan.entries[0].exists);
    assert!(scan.entries[0].status.is_some());
}

#[test]
fn changed_tree_contains_only_changed_files_and_required_ancestors() {
    let fixture = TestRepo::new();
    fixture.write("clean/readme.md", "clean\n");
    fixture.write("src/nested/live.rs", "present\n");
    let statuses = HashMap::from([
        (
            Path::new("src/nested/live.rs").to_path_buf(),
            FileStatus {
                index: ' ',
                worktree: 'M',
            },
        ),
        (
            Path::new("removed/deep/gone.rs").to_path_buf(),
            FileStatus {
                index: ' ',
                worktree: 'D',
            },
        ),
    ]);

    let all = scan(fixture.root(), &statuses).unwrap();
    let changed = changed_only(&all.entries);
    let paths: Vec<_> = changed
        .iter()
        .map(|entry| entry.relative.as_path())
        .collect();

    assert_eq!(
        paths,
        [
            Path::new("removed"),
            Path::new("removed/deep"),
            Path::new("removed/deep/gone.rs"),
            Path::new("src"),
            Path::new("src/nested"),
            Path::new("src/nested/live.rs"),
        ]
    );
    assert!(!paths.iter().any(|path| path.starts_with("clean")));
    assert!(!entry(&changed, "removed").exists);
    assert!(entry(&changed, "removed").is_dir);
    assert_eq!(entry(&changed, "removed/deep/gone.rs").depth, 2);
}

#[test]
fn scan_sorts_directory_siblings_before_files_at_each_level() {
    let fixture = TestRepo::new();
    for path in [
        "z-root.txt",
        "a-root.txt",
        "z-dir/middle.txt",
        "a-dir/z-child.txt",
        "a-dir/a-child.txt",
        "a-dir/b-dir/inner.txt",
    ] {
        fixture.write(path, "fixture\n");
    }

    let paths: Vec<_> = scan(fixture.root(), &HashMap::new())
        .unwrap()
        .entries
        .into_iter()
        .map(|entry| entry.relative)
        .collect();
    assert_eq!(
        paths,
        [
            Path::new("a-dir").to_path_buf(),
            Path::new("a-dir/b-dir").to_path_buf(),
            Path::new("a-dir/b-dir/inner.txt").to_path_buf(),
            Path::new("a-dir/a-child.txt").to_path_buf(),
            Path::new("a-dir/z-child.txt").to_path_buf(),
            Path::new("z-dir").to_path_buf(),
            Path::new("z-dir/middle.txt").to_path_buf(),
            Path::new("a-root.txt").to_path_buf(),
            Path::new("z-root.txt").to_path_buf(),
        ]
    );
}

#[test]
fn scan_reports_a_missing_root() {
    let directory = tempfile::tempdir().unwrap();
    let missing = directory.path().join("missing");
    let error = scan(&missing, &HashMap::new()).unwrap_err();
    assert!(error.to_string().contains("failed to scan"));
}

#[test]
fn file_entry_name_uses_the_final_path_component() {
    let entry = FileEntry {
        relative: "src/latte.rs".into(),
        is_dir: false,
        depth: 1,
        status: None,
        contains_changes: false,
        exists: true,
    };
    assert_eq!(entry.name(), "latte.rs");
}

fn entry<'a>(entries: &'a [FileEntry], path: &str) -> &'a FileEntry {
    entries
        .iter()
        .find(|entry| entry.relative == Path::new(path))
        .unwrap_or_else(|| panic!("missing tree entry {path}"))
}
