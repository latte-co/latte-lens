mod support;

use std::{fs, path::Path, thread, time::Duration};

use lattelens::git::{FileStatus, GitRepo};
use support::TestRepo;

#[test]
fn discovers_repository_from_nested_directory_and_reads_branch() {
    let fixture = TestRepo::new();
    fixture.write("src/lib.rs", "pub fn latte() {}\n");
    fixture.commit_all("initial");

    let nested = fixture.root().join("src/nested");
    fs::create_dir_all(&nested).unwrap();
    let repo = GitRepo::discover(&nested).unwrap().expect("repository");

    assert_eq!(
        repo.root().canonicalize().unwrap(),
        fixture.root().canonicalize().unwrap()
    );
    assert_eq!(repo.branch().unwrap(), "main");
}

#[test]
fn discovery_preserves_root_boundary_whitespace_for_status_and_diff() {
    let fixture = TestRepo::with_root_name("  repository root  ");
    fixture.write("tracked.txt", "original\n");
    fixture.commit_all("initial");
    fixture.write("tracked.txt", "changed\n");

    let repo = GitRepo::discover(fixture.root()).unwrap().unwrap();

    assert_eq!(
        repo.root().file_name(),
        Some(std::ffi::OsStr::new("  repository root  "))
    );
    assert_eq!(
        repo.root().canonicalize().unwrap(),
        fixture.root().canonicalize().unwrap()
    );
    let statuses = repo.statuses().unwrap();
    let status = statuses[Path::new("tracked.txt")];
    let diff = repo
        .diff_for(Path::new("tracked.txt"), Some(status))
        .unwrap()
        .join("\n");
    assert!(diff.contains("-original"));
    assert!(diff.contains("+changed"));
}

#[test]
fn discover_returns_none_outside_a_repository() {
    let directory = tempfile::tempdir().unwrap();
    assert!(GitRepo::discover(directory.path()).unwrap().is_none());
}

#[test]
fn status_and_diff_cover_staged_worktree_and_untracked_changes_without_mutation() {
    let fixture = TestRepo::new();
    fixture.write("tracked.txt", "original\n");
    fixture.commit_all("initial");

    fixture.write("tracked.txt", "staged value\n");
    fixture.git(&["add", "tracked.txt"]);
    fixture.write("tracked.txt", "worktree value\n");
    fixture.write("new file.txt", "first\nsecond\n");

    let before = fixture.status_bytes();
    let repo = GitRepo::discover(fixture.root()).unwrap().unwrap();
    let statuses = repo.statuses().unwrap();

    let tracked_status = statuses[Path::new("tracked.txt")];
    assert_eq!(
        tracked_status,
        FileStatus {
            index: 'M',
            worktree: 'M'
        }
    );
    assert_eq!(statuses[Path::new("new file.txt")].label(), "??");

    let tracked_diff = repo
        .diff_for(Path::new("tracked.txt"), Some(tracked_status))
        .unwrap()
        .join("\n");
    assert!(tracked_diff.contains("── STAGED ──"));
    assert!(tracked_diff.contains("+staged value"));
    assert!(tracked_diff.contains("── WORKTREE ──"));
    assert!(tracked_diff.contains("+worktree value"));

    let untracked_diff = repo
        .diff_for(
            Path::new("new file.txt"),
            statuses.get(Path::new("new file.txt")).copied(),
        )
        .unwrap()
        .join("\n");
    assert!(untracked_diff.contains("── UNTRACKED ──"));
    assert!(untracked_diff.contains("+++ b/new file.txt"));
    assert!(untracked_diff.contains("+first"));
    assert_eq!(
        fixture.status_bytes(),
        before,
        "viewer must remain read-only"
    );
}

#[test]
fn viewer_git_operations_do_not_refresh_stale_index_stat_data() {
    let fixture = TestRepo::new();
    fixture.write("tracked.txt", "unchanged\n");
    fixture.commit_all("initial");

    let tracked_path = fixture.root().join("tracked.txt");
    let committed_modified = fs::metadata(&tracked_path).unwrap().modified().unwrap();
    thread::sleep(Duration::from_millis(1_100));
    fixture.write("tracked.txt", "unchanged\n");
    let stale_modified = fs::metadata(&tracked_path).unwrap().modified().unwrap();
    assert_ne!(
        stale_modified, committed_modified,
        "test setup must make the index stat data stale"
    );

    let index_path = fixture.root().join(".git/index");
    let index_before = fs::read(&index_path).unwrap();
    let assert_index_unchanged = || {
        assert_eq!(
            fs::read(&index_path).unwrap(),
            index_before,
            "read-only Git inspection must not refresh .git/index"
        );
    };

    let repo = GitRepo::discover(fixture.root()).unwrap().unwrap();
    assert_index_unchanged();
    assert_eq!(repo.branch().unwrap(), "main");
    assert_index_unchanged();
    assert!(repo.statuses().unwrap().is_empty());
    assert_index_unchanged();

    fixture.write("tracked.txt", "changed contents\n");
    let statuses = repo.statuses().unwrap();
    let status = statuses[Path::new("tracked.txt")];
    assert_eq!(
        status,
        FileStatus {
            index: ' ',
            worktree: 'M'
        }
    );
    let diff = repo
        .diff_for(Path::new("tracked.txt"), Some(status))
        .unwrap()
        .join("\n");
    assert!(diff.contains("+changed contents"));
    assert_index_unchanged();
}

// Apple filesystems reject non-UTF-8 names before Git can observe them. Other
// Unix targets preserve arbitrary filename bytes and exercise the full path.
#[cfg(all(unix, not(target_vendor = "apple")))]
#[test]
fn non_utf8_path_round_trips_through_status_existence_and_diff() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt, path::PathBuf};

    let fixture = TestRepo::new();
    let relative_path = PathBuf::from(OsString::from_vec(b"invalid-\xff-name.txt".to_vec()));
    fs::write(fixture.root().join(&relative_path), "original\n").unwrap();
    fixture.commit_all("initial");
    fs::write(fixture.root().join(&relative_path), "changed\n").unwrap();

    let repo = GitRepo::discover(fixture.root()).unwrap().unwrap();
    let statuses = repo.statuses().unwrap();
    let (reported_path, status) = statuses
        .get_key_value(&relative_path)
        .expect("status must retain the filename's original bytes");

    assert_eq!(
        *status,
        FileStatus {
            index: ' ',
            worktree: 'M'
        }
    );
    assert!(
        fixture.root().join(reported_path).exists(),
        "reported status path must identify the real filesystem object"
    );

    let diff = repo
        .diff_for(reported_path, Some(*status))
        .unwrap()
        .join("\n");
    assert!(diff.contains("-original"));
    assert!(diff.contains("+changed"));
}

#[cfg(all(unix, not(target_vendor = "apple")))]
#[test]
fn non_utf8_staged_rename_routes_both_raw_pathspecs() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt, path::PathBuf};

    let fixture = TestRepo::new();
    let old_path = PathBuf::from(OsString::from_vec(b"old-\xff-name.txt".to_vec()));
    let new_path = PathBuf::from(OsString::from_vec(b"new-\xfe-name.txt".to_vec()));
    fs::write(fixture.root().join(&old_path), "unchanged\n").unwrap();
    fixture.commit_all("initial");
    fs::rename(
        fixture.root().join(&old_path),
        fixture.root().join(&new_path),
    )
    .unwrap();
    fixture.git(&["add", "--all"]);

    let repo = GitRepo::discover(fixture.root()).unwrap().unwrap();
    let entry = repo
        .status_entries()
        .unwrap()
        .into_iter()
        .find(|entry| entry.status.index == 'R')
        .expect("raw-byte rename");
    assert_eq!(entry.path, new_path);
    assert_eq!(entry.original_path.as_deref(), Some(old_path.as_path()));

    let diff = repo
        .diff_for_change(
            &entry.path,
            entry.original_path.as_deref(),
            Some(entry.status),
        )
        .unwrap()
        .join("\n");
    assert!(diff.contains("── STAGED ──"));
    assert!(diff.contains("similarity index 100%"));
    assert!(!diff.contains("new file mode"));
}

#[cfg(all(unix, not(target_vendor = "apple")))]
#[test]
fn non_utf8_root_round_trips_through_discovery_status_and_diff() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};

    let root_name = OsString::from_vec(b" root-\xff ".to_vec());
    let fixture = TestRepo::with_root_name(&root_name);
    fixture.write("tracked.txt", "original\n");
    fixture.commit_all("initial");
    fixture.write("tracked.txt", "changed\n");

    let repo = GitRepo::discover(fixture.root()).unwrap().unwrap();

    assert_eq!(repo.root().file_name(), Some(root_name.as_os_str()));
    let statuses = repo.statuses().unwrap();
    let status = statuses[Path::new("tracked.txt")];
    let diff = repo
        .diff_for(Path::new("tracked.txt"), Some(status))
        .unwrap()
        .join("\n");
    assert!(diff.contains("-original"));
    assert!(diff.contains("+changed"));
}

#[test]
fn clean_file_diff_explains_that_there_are_no_changes() {
    let fixture = TestRepo::new();
    fixture.write("clean.txt", "unchanged\n");
    fixture.commit_all("initial");
    let repo = GitRepo::discover(fixture.root()).unwrap().unwrap();

    let lines = repo.diff_for(Path::new("clean.txt"), None).unwrap();

    assert!(lines[0].contains("has no uncommitted changes"));
}

#[test]
fn untracked_preview_rejects_binary_and_oversized_files() {
    let fixture = TestRepo::new();
    fixture.write("binary.dat", [b'a', 0, b'b']);
    fixture.write("large.txt", vec![b'x'; 512 * 1024 + 1]);
    let repo = GitRepo::discover(fixture.root()).unwrap().unwrap();
    let untracked = Some(FileStatus {
        index: '?',
        worktree: '?',
    });

    assert_eq!(
        repo.diff_for(Path::new("binary.dat"), untracked).unwrap(),
        ["Untracked binary file."]
    );
    assert!(
        repo.diff_for(Path::new("large.txt"), untracked).unwrap()[0]
            .contains("too large to preview")
    );
}

#[cfg(unix)]
#[test]
fn untracked_symlink_diff_renders_only_the_link_target_text() {
    use std::os::unix::fs::symlink;

    let fixture = TestRepo::new();
    let outside = tempfile::tempdir().unwrap();
    let outside_file = outside.path().join("external.txt");
    let secret = "external-file-content-c4ad";
    fs::write(&outside_file, secret).unwrap();
    symlink(&outside_file, fixture.root().join("external-link")).unwrap();
    let repo = GitRepo::discover(fixture.root()).unwrap().unwrap();
    let untracked = Some(FileStatus {
        index: '?',
        worktree: '?',
    });

    let diff = repo
        .diff_for(Path::new("external-link"), untracked)
        .unwrap()
        .join("\n");

    assert!(diff.contains("new file mode 120000"));
    assert!(diff.contains(&format!("+{}", outside_file.display())));
    assert!(!diff.contains(secret));
}

#[test]
fn branch_falls_back_to_detached_head_revision() {
    let fixture = TestRepo::new();
    fixture.write("README.md", "fixture\n");
    fixture.commit_all("initial");
    fixture.git(&["checkout", "--quiet", "--detach"]);
    let repo = GitRepo::discover(fixture.root()).unwrap().unwrap();

    assert!(repo.branch().unwrap().starts_with("detached@"));
}

#[test]
fn empty_repository_reports_its_unborn_branch() {
    let fixture = TestRepo::new();
    let repo = GitRepo::discover(fixture.root()).unwrap().unwrap();

    assert_eq!(repo.branch().unwrap(), "main");
}

#[test]
fn untracked_preview_is_bounded_by_a_line_limit() {
    let fixture = TestRepo::new();
    let contents: String = (0..2_001).map(|index| format!("line {index}\n")).collect();
    fixture.write("many-lines.txt", contents);
    let repo = GitRepo::discover(fixture.root()).unwrap().unwrap();

    let lines = repo
        .diff_for(
            Path::new("many-lines.txt"),
            Some(FileStatus {
                index: '?',
                worktree: '?',
            }),
        )
        .unwrap();

    assert_eq!(lines.len(), 2_007);
    assert_eq!(
        lines.last().unwrap(),
        "… preview truncated after 2000 lines"
    );
}

#[test]
fn missing_untracked_file_returns_contextual_error() {
    let fixture = TestRepo::new();
    let repo = GitRepo::discover(fixture.root()).unwrap().unwrap();

    let error = repo
        .diff_for(
            Path::new("gone.txt"),
            Some(FileStatus {
                index: '?',
                worktree: '?',
            }),
        )
        .unwrap_err()
        .to_string();

    assert!(error.contains("cannot inspect"));
    assert!(error.contains("gone.txt"));
}
