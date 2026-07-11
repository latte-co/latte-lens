use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use latte_lens::repo_graph::{
    DiscoveryOptions, DiscoveryTruncation, GitLayout, RepoGraph, RepoKind, RepoRelationState,
};

fn run(root: &Path, args: &[&str]) -> Output {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .unwrap_or_else(|error| panic!("run git {}: {error}", args.join(" ")));
    assert!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn run_with_path(root: &Path, before: &[&str], path: &Path, after: &[&str]) -> Output {
    let output = Command::new("git")
        .args(before)
        .arg(path)
        .args(after)
        .current_dir(root)
        .output()
        .unwrap_or_else(|error| panic!("run git with path: {error}"));
    assert!(
        output.status.success(),
        "git command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn init(root: &Path) {
    fs::create_dir_all(root).unwrap();
    run(root, &["-c", "init.defaultBranch=main", "init", "--quiet"]);
    run(root, &["config", "user.name", "Latte Lens Tests"]);
    run(
        root,
        &["config", "user.email", "latte-lens@example.invalid"],
    );
}

fn commit_all(root: &Path, message: &str) {
    run(root, &["add", "--all"]);
    run(root, &["commit", "--quiet", "-m", message]);
}

fn write(root: &Path, relative: &str, contents: &str) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, contents).unwrap();
}

fn snapshot_at<'a>(graph: &'a RepoGraph, root: &Path) -> &'a latte_lens::repo_graph::RepoSnapshot {
    let canonical = root.canonicalize().unwrap();
    graph
        .repositories()
        .iter()
        .find(|snapshot| snapshot.node.worktree == canonical)
        .unwrap_or_else(|| panic!("missing repository {}", root.display()))
}

#[test]
fn non_git_workspace_discovers_clean_and_dirty_repositories_and_routes_by_owner() {
    let workspace = tempfile::tempdir().unwrap();
    let alpha = workspace.path().join("alpha");
    let beta = workspace.path().join("beta");
    let clean = workspace.path().join("clean");
    for root in [&alpha, &beta, &clean] {
        init(root);
        write(root, "same.txt", "original\n");
        commit_all(root, "initial");
    }
    write(&alpha, "same.txt", "alpha change\n");
    write(&beta, "same.txt", "beta change\n");

    let graph = RepoGraph::discover(workspace.path()).unwrap();

    assert_eq!(
        graph.workspace_root(),
        workspace.path().canonicalize().unwrap()
    );
    assert_eq!(graph.repositories().len(), 3);
    assert!(snapshot_at(&graph, &clean).changes.is_empty());
    let alpha_change = snapshot_at(&graph, &alpha).changes[0].path.clone();
    let beta_change = snapshot_at(&graph, &beta).changes[0].path.clone();
    assert_eq!(alpha_change.relative, beta_change.relative);
    assert_ne!(alpha_change.repo_id, beta_change.repo_id);

    let alpha_diff = graph.diff_for(&alpha_change).unwrap().join("\n");
    let beta_diff = graph.diff_for(&beta_change).unwrap().join("\n");
    assert!(alpha_diff.contains("+alpha change"));
    assert!(!alpha_diff.contains("+beta change"));
    assert!(beta_diff.contains("+beta change"));
    assert!(!beta_diff.contains("+alpha change"));
    assert_eq!(graph.projected_change_count(), 2);
}

#[test]
fn clean_single_repository_remains_in_the_repository_inventory() {
    let workspace = tempfile::tempdir().unwrap();
    init(workspace.path());
    write(workspace.path(), "tracked.txt", "clean\n");
    commit_all(workspace.path(), "initial");

    let graph = RepoGraph::discover(workspace.path()).unwrap();

    assert_eq!(graph.repositories().len(), 1);
    let root = snapshot_at(&graph, workspace.path());
    assert_eq!(root.node.kind, RepoKind::WorkspaceRoot);
    assert!(root.changes.is_empty());
    assert_eq!(graph.projected_change_count(), 0);
}

#[test]
fn capped_repository_membership_is_deterministic_across_creation_orders() {
    for creation_order in [
        ["gamma", "delta", "beta", "alpha"],
        ["alpha", "beta", "delta", "gamma"],
    ] {
        let workspace = tempfile::tempdir().unwrap();
        for name in creation_order {
            init(&workspace.path().join(name));
        }

        let graph = RepoGraph::discover_with_options(
            workspace.path(),
            DiscoveryOptions {
                max_entries: 3,
                max_repositories: 10,
                max_depth: 8,
            },
        )
        .unwrap();
        let membership: Vec<&Path> = graph
            .repositories()
            .iter()
            .map(|snapshot| {
                snapshot
                    .node
                    .workspace_relative
                    .as_deref()
                    .expect("nested repository path")
            })
            .collect();

        assert_eq!(
            membership,
            [Path::new("alpha"), Path::new("beta"), Path::new("delta")]
        );
        assert!(
            graph
                .report()
                .truncations
                .contains(&DiscoveryTruncation::EntryLimit { limit: 3 })
        );
    }
}

#[test]
fn staged_rename_diff_routes_original_and_destination_paths() {
    let workspace = tempfile::tempdir().unwrap();
    init(workspace.path());
    write(workspace.path(), "old name.txt", "unchanged contents\n");
    commit_all(workspace.path(), "initial");
    fs::rename(
        workspace.path().join("old name.txt"),
        workspace.path().join("new name.txt"),
    )
    .unwrap();
    run(workspace.path(), &["add", "--all"]);

    let graph = RepoGraph::discover(workspace.path()).unwrap();
    let snapshot = snapshot_at(&graph, workspace.path());
    let change = snapshot
        .changes
        .iter()
        .find(|change| change.status.index == 'R')
        .expect("staged rename");
    assert_eq!(change.path.relative, Path::new("new name.txt"));
    assert_eq!(
        change.original_path.as_deref(),
        Some(Path::new("old name.txt"))
    );

    let diff = graph.diff_for(&change.path).unwrap().join("\n");
    assert!(diff.contains("diff --git a/old name.txt b/new name.txt"));
    assert!(diff.contains("rename from old name.txt"));
    assert!(diff.contains("rename to new name.txt"));
    assert!(!diff.contains("new file mode"));
}

#[test]
fn staged_copy_diff_routes_original_and_destination_paths() {
    let workspace = tempfile::tempdir().unwrap();
    init(workspace.path());
    let original: String = (1..=20).map(|line| format!("line {line}\n")).collect();
    write(workspace.path(), "source name.txt", &original);
    commit_all(workspace.path(), "initial");
    fs::copy(
        workspace.path().join("source name.txt"),
        workspace.path().join("copy name.txt"),
    )
    .unwrap();
    write(
        workspace.path(),
        "source name.txt",
        &format!("{original}source changed\n"),
    );
    run(workspace.path(), &["config", "status.renames", "copies"]);
    run(workspace.path(), &["add", "--all"]);

    let graph = RepoGraph::discover(workspace.path()).unwrap();
    let snapshot = snapshot_at(&graph, workspace.path());
    let change = snapshot
        .changes
        .iter()
        .find(|change| change.status.index == 'C')
        .expect("staged copy");
    assert_eq!(change.path.relative, Path::new("copy name.txt"));
    assert_eq!(
        change.original_path.as_deref(),
        Some(Path::new("source name.txt"))
    );

    let diff = graph.diff_for(&change.path).unwrap().join("\n");
    assert!(diff.contains("diff --git a/source name.txt b/copy name.txt"));
    assert!(diff.contains("copy from source name.txt"));
    assert!(diff.contains("copy to copy name.txt"));
    assert!(!diff.contains("new file mode"));
}

#[test]
fn root_and_ordinary_nested_repo_are_stable_boundaries_with_parent_suppression() {
    let workspace = tempfile::tempdir().unwrap();
    init(workspace.path());
    write(workspace.path(), "root.txt", "root\n");
    commit_all(workspace.path(), "root initial");

    let nested = workspace.path().join("vendor/nested");
    init(&nested);
    write(&nested, "inside.txt", "nested\n");
    commit_all(&nested, "nested initial");
    write(&nested, "inside.txt", "nested dirty\n");

    let first = RepoGraph::discover(workspace.path()).unwrap();
    let root = snapshot_at(&first, workspace.path());
    let child = snapshot_at(&first, &nested);
    let first_child_id = child.node.id.clone();
    assert_eq!(root.node.kind, RepoKind::WorkspaceRoot);
    assert_eq!(child.node.kind, RepoKind::Nested);
    let relation = child.node.relation.as_ref().unwrap();
    assert_eq!(relation.parent, root.node.id);
    assert_eq!(relation.path_in_parent, Path::new("vendor/nested"));
    assert!(matches!(
        relation.state,
        RepoRelationState::OrdinaryNested {
            untracked_in_parent: true,
            ..
        }
    ));
    assert!(
        root.changes
            .iter()
            .all(|change| !change.path.relative.starts_with("vendor/nested"))
    );
    assert!(
        root.suppressed_parent_changes
            .iter()
            .any(|change| change.path.relative.starts_with("vendor/nested"))
    );

    let second = RepoGraph::discover(workspace.path()).unwrap();
    assert_eq!(snapshot_at(&second, &nested).node.id, first_child_id);
    assert_eq!(first.projected_change_count(), 1);
}

#[test]
fn selected_directory_inside_repo_remains_the_boundary() {
    let repository = tempfile::tempdir().unwrap();
    init(repository.path());
    write(
        repository.path(),
        "selected/inside.txt",
        "inside original\n",
    );
    write(repository.path(), "outside.txt", "outside original\n");
    commit_all(repository.path(), "initial");
    write(repository.path(), "selected/inside.txt", "inside changed\n");
    write(repository.path(), "outside.txt", "outside changed\n");

    let selected = repository.path().join("selected");
    let graph = RepoGraph::discover(&selected).unwrap();
    let containing = snapshot_at(&graph, repository.path());

    assert_eq!(graph.workspace_root(), selected.canonicalize().unwrap());
    assert_eq!(containing.node.kind, RepoKind::Containing);
    assert_eq!(containing.node.workspace_relative, None);
    assert_eq!(containing.changes.len(), 1);
    assert_eq!(
        containing.changes[0].path.relative,
        Path::new("selected/inside.txt")
    );
    let diff = graph
        .diff_for(&containing.changes[0].path)
        .unwrap()
        .join("\n");
    assert!(diff.contains("+inside changed"));
    assert!(!diff.contains("+outside changed"));
}

#[test]
fn linked_worktree_git_file_is_a_separate_repository() {
    let workspace = tempfile::tempdir().unwrap();
    let main = workspace.path().join("main");
    let linked = workspace.path().join("linked");
    init(&main);
    write(&main, "tracked.txt", "initial\n");
    commit_all(&main, "initial");
    run_with_path(
        &main,
        &["worktree", "add", "--quiet", "--detach"],
        &linked,
        &[],
    );

    let graph = RepoGraph::discover(workspace.path()).unwrap();
    let linked_snapshot = snapshot_at(&graph, &linked);

    assert_eq!(linked_snapshot.node.kind, RepoKind::LinkedWorktree);
    assert_eq!(linked_snapshot.node.layout, GitLayout::GitFile);
    assert!(linked.join(".git").is_file());
    assert_ne!(
        linked_snapshot.node.git_dir,
        snapshot_at(&graph, &main).node.git_dir
    );
}

#[test]
fn recursive_initialized_submodules_and_uninitialized_placeholders_keep_parents() {
    let sources = tempfile::tempdir().unwrap();
    let grand_source = sources.path().join("grand-source");
    init(&grand_source);
    write(&grand_source, "grand.txt", "grand\n");
    commit_all(&grand_source, "grand initial");

    let child_source = sources.path().join("child-source");
    init(&child_source);
    write(&child_source, "child.txt", "child\n");
    commit_all(&child_source, "child initial");
    run_with_path(
        &child_source,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "--quiet",
        ],
        &grand_source,
        &["nested/grand"],
    );
    commit_all(&child_source, "add nested submodule");

    let workspace = tempfile::tempdir().unwrap();
    init(workspace.path());
    write(workspace.path(), "root.txt", "root\n");
    commit_all(workspace.path(), "root initial");
    run_with_path(
        workspace.path(),
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "--quiet",
        ],
        &child_source,
        &["modules/child"],
    );
    write(
        workspace.path(),
        ".gitmodules",
        &format!(
            "{}\n[submodule \"missing\"]\n\tpath = modules/missing\n\turl = ../missing\n",
            fs::read_to_string(workspace.path().join(".gitmodules"))
                .unwrap()
                .trim_end()
        ),
    );
    commit_all(workspace.path(), "add child and placeholder metadata");
    run(
        workspace.path(),
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "update",
            "--init",
            "--recursive",
            "--quiet",
        ],
    );

    let child = workspace.path().join("modules/child");
    let grand = child.join("nested/grand");
    let graph = RepoGraph::discover(workspace.path()).unwrap();
    let root_snapshot = snapshot_at(&graph, workspace.path());
    let child_snapshot = snapshot_at(&graph, &child);
    let grand_snapshot = snapshot_at(&graph, &grand);
    assert!(root_snapshot.changes.is_empty());
    assert!(child_snapshot.changes.is_empty());
    assert!(grand_snapshot.changes.is_empty());
    assert_eq!(child_snapshot.node.kind, RepoKind::Submodule);
    assert_eq!(grand_snapshot.node.kind, RepoKind::Submodule);
    assert_eq!(child_snapshot.node.layout, GitLayout::GitFile);
    assert_eq!(grand_snapshot.node.layout, GitLayout::GitFile);
    assert_eq!(
        child_snapshot.node.relation.as_ref().unwrap().parent,
        root_snapshot.node.id
    );
    assert_eq!(
        grand_snapshot.node.relation.as_ref().unwrap().parent,
        child_snapshot.node.id
    );

    let placeholder = graph
        .repositories()
        .iter()
        .find(|snapshot| snapshot.node.kind == RepoKind::SubmodulePlaceholder)
        .expect("uninitialized placeholder");
    assert_eq!(
        placeholder.node.workspace_relative.as_deref(),
        Some(Path::new("modules/missing"))
    );
    assert_eq!(
        placeholder.node.relation.as_ref().unwrap().parent,
        root_snapshot.node.id
    );
    assert!(matches!(
        placeholder.node.relation.as_ref().unwrap().state,
        RepoRelationState::Submodule {
            initialized: false,
            ..
        }
    ));
    assert_eq!(graph.projected_change_count(), 0);
}

#[test]
fn submodule_parent_pointer_and_internal_child_changes_are_distinct_and_read_only() {
    let sources = tempfile::tempdir().unwrap();
    let source = sources.path().join("child-source");
    init(&source);
    write(&source, "tracked.txt", "initial\n");
    commit_all(&source, "initial");

    let workspace = tempfile::tempdir().unwrap();
    init(workspace.path());
    run_with_path(
        workspace.path(),
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "--quiet",
        ],
        &source,
        &["child"],
    );
    commit_all(workspace.path(), "add child");
    let child = workspace.path().join("child");
    write(&child, "tracked.txt", "committed in child\n");
    commit_all(&child, "advance child pointer");
    write(&child, "tracked.txt", "dirty inside child\n");

    let root_index = fs::read(workspace.path().join(".git/index")).unwrap();
    let child_git_dir = run(&child, &["rev-parse", "--absolute-git-dir"]);
    let child_index_path = PathBuf::from(
        String::from_utf8(child_git_dir.stdout)
            .unwrap()
            .trim_end()
            .to_owned(),
    )
    .join("index");
    let child_index = fs::read(&child_index_path).unwrap();

    let graph = RepoGraph::discover(workspace.path()).unwrap();
    let child_snapshot = snapshot_at(&graph, &child);
    let RepoRelationState::Submodule {
        parent_change: Some(parent_change),
        ..
    } = &child_snapshot.node.relation.as_ref().unwrap().state
    else {
        panic!("expected parent submodule status");
    };
    assert!(parent_change.submodule_pointer_changed());
    assert!(parent_change.submodule_worktree_dirty());
    assert!(
        child_snapshot
            .changes
            .iter()
            .any(|change| change.path.relative == Path::new("tracked.txt"))
    );
    assert_eq!(graph.projected_change_count(), 2);
    assert_eq!(
        fs::read(workspace.path().join(".git/index")).unwrap(),
        root_index
    );
    assert_eq!(fs::read(child_index_path).unwrap(), child_index);
}

#[test]
fn discovery_limits_and_invalid_git_markers_are_explicit() {
    let workspace = tempfile::tempdir().unwrap();
    let valid = workspace.path().join("a-valid");
    init(&valid);
    let invalid = workspace.path().join("b-invalid");
    fs::create_dir_all(invalid.join(".git")).unwrap();
    fs::write(workspace.path().join("z-file"), "fixture").unwrap();

    let graph = RepoGraph::discover_with_options(
        workspace.path(),
        DiscoveryOptions {
            max_entries: 2,
            max_repositories: 1,
            max_depth: 8,
        },
    )
    .unwrap();

    assert!(graph.report().is_truncated());
    assert!(
        graph
            .report()
            .truncations
            .contains(&DiscoveryTruncation::EntryLimit { limit: 2 })
    );
    assert_eq!(graph.report().repositories_discovered, 1);

    let invalid_graph = RepoGraph::discover(workspace.path()).unwrap();
    assert!(
        invalid_graph
            .report()
            .errors
            .iter()
            .any(|error| error.path == invalid.canonicalize().unwrap()),
        "errors: {:?}",
        invalid_graph.report().errors
    );
}

#[test]
fn repository_cap_is_explicit_and_git_internals_do_not_consume_the_entry_budget() {
    let workspace = tempfile::tempdir().unwrap();
    init(&workspace.path().join("a"));
    init(&workspace.path().join("b"));

    let capped = RepoGraph::discover_with_options(
        workspace.path(),
        DiscoveryOptions {
            max_entries: 10,
            max_repositories: 1,
            max_depth: 8,
        },
    )
    .unwrap();
    assert!(
        capped
            .report()
            .truncations
            .contains(&DiscoveryTruncation::RepositoryLimit { limit: 1 })
    );
    assert_eq!(capped.repositories().len(), 1);

    let empty_repo = tempfile::tempdir().unwrap();
    init(empty_repo.path());
    let complete = RepoGraph::discover_with_options(
        empty_repo.path(),
        DiscoveryOptions {
            max_entries: 1,
            max_repositories: 1,
            max_depth: 0,
        },
    )
    .unwrap();
    assert_eq!(complete.report().entries_scanned, 1);
    assert!(!complete.report().is_truncated());
    assert_eq!(complete.repositories().len(), 1);
}

#[cfg(unix)]
#[test]
fn symlink_cycles_and_workspace_escapes_are_not_followed() {
    use std::os::unix::fs::symlink;

    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let outside_repo = outside.path().join("repo");
    init(&outside_repo);
    symlink(&outside_repo, workspace.path().join("outside-link")).unwrap();
    symlink(workspace.path(), workspace.path().join("cycle")).unwrap();

    let graph = RepoGraph::discover(workspace.path()).unwrap();

    assert!(graph.repositories().is_empty());
    assert!(!graph.report().is_truncated());
}

#[cfg(unix)]
#[test]
fn carriage_return_suffixed_root_round_trips_through_graph_status_and_diff() {
    use std::ffi::OsStr;

    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join(OsStr::new("repository\r"));
    init(&root);
    write(&root, "tracked.txt", "initial\n");
    commit_all(&root, "initial");
    write(&root, "tracked.txt", "changed\n");
    let index_before = fs::read(root.join(".git/index")).unwrap();

    let graph = RepoGraph::discover(&root).unwrap();
    let snapshot = snapshot_at(&graph, &root);
    assert_eq!(
        snapshot.node.worktree.file_name(),
        Some(OsStr::new("repository\r"))
    );
    let diff = graph
        .diff_for(&snapshot.changes[0].path)
        .unwrap()
        .join("\n");
    assert!(diff.contains("+changed"));
    assert_eq!(fs::read(root.join(".git/index")).unwrap(), index_before);
}
